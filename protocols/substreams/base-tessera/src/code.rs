//! Contract bytecode read through `eth_call`, the only state read Substreams offers besides
//! balances.
//!
//! One Multicall3 `aggregate3` call first CREATE2-deploys a 15-byte reader through the
//! deterministic deployment proxy, allowed to fail when the reader already exists, then calls the
//! reader, which returns `EXTCODECOPY(target)`. The deployment only exists inside the `eth_call`.
//!
//! An `eth_call` on a Flashblocks partial block stalls the stream for about three minutes, so a
//! package using this module must run without partial blocks.

use anyhow::{anyhow, Result};
use keccak_hash::keccak;
use std::collections::HashMap;
use substreams::hex;
use substreams_ethereum::pb::eth::rpc::{RpcCall, RpcCalls};

use crate::common::id;

const MULTICALL3: [u8; 20] = hex!("cA11bde05977b3631167028862bE2a173976CA11");
const DEPLOYER: [u8; 20] = hex!("4e59b44847b379578588920cA78FbF26c0B4956C");
/// Init code whose 15-byte runtime returns `EXTCODECOPY(calldataload(0))`.
const READER_INIT: [u8; 27] = hex!("600f600c600039600f6000f3600035803b8060006000843c6000f3");
const AGGREGATE3: [u8; 4] = hex!("82ad56cb");

/// Bytecode of contracts at the end of the current block.
pub trait CodeSource {
    /// Code of every address in `addresses`, keyed by address. A contract without code maps to
    /// empty bytes.
    fn codes(&self, addresses: &[Vec<u8>]) -> Result<HashMap<Vec<u8>, Vec<u8>>>;
}

/// Reads code with one batched `eth_call`.
pub struct RpcCode;

impl CodeSource for RpcCode {
    fn codes(&self, addresses: &[Vec<u8>]) -> Result<HashMap<Vec<u8>, Vec<u8>>> {
        if addresses.is_empty() {
            return Ok(HashMap::new());
        }
        let calls = RpcCalls {
            calls: addresses
                .iter()
                .map(|a| code_call(a))
                .collect(),
        };
        let responses = substreams_ethereum::rpc::eth_call(&calls).responses;
        let mut codes = HashMap::new();
        for (address, response) in addresses.iter().zip(responses) {
            if response.failed {
                return Err(anyhow!("code eth_call failed for {}", id(address)));
            }
            let code = last_result(&response.raw)
                .ok_or_else(|| anyhow!("undecodable code eth_call result for {}", id(address)))?;
            codes.insert(address.clone(), code);
        }
        Ok(codes)
    }
}

fn word(value: usize) -> [u8; 32] {
    let mut w = [0; 32];
    w[24..].copy_from_slice(&(value as u64).to_be_bytes());
    w
}

fn address_word(address: &[u8]) -> [u8; 32] {
    let mut w = [0; 32];
    w[12..].copy_from_slice(address);
    w
}

fn reader_address() -> Vec<u8> {
    let mut preimage = vec![0xff];
    preimage.extend_from_slice(&DEPLOYER);
    preimage.extend_from_slice(&[0; 32]);
    preimage.extend_from_slice(keccak(READER_INIT).as_bytes());
    keccak(preimage).as_bytes()[12..].to_vec()
}

/// ABI-encodes one `(address target, bool allowFailure, bytes callData)` tuple.
fn call3(target: &[u8], allow_failure: bool, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&address_word(target));
    out.extend_from_slice(&word(usize::from(allow_failure)));
    out.extend_from_slice(&word(0x60));
    out.extend_from_slice(&word(data.len()));
    out.extend_from_slice(data);
    out.resize(out.len() + (32 - data.len() % 32) % 32, 0);
    out
}

fn aggregate3(calls: &[Vec<u8>]) -> Vec<u8> {
    let mut out = AGGREGATE3.to_vec();
    out.extend_from_slice(&word(0x20));
    out.extend_from_slice(&word(calls.len()));
    let mut offset = 32 * calls.len();
    for call in calls {
        out.extend_from_slice(&word(offset));
        offset += call.len();
    }
    for call in calls {
        out.extend_from_slice(call);
    }
    out
}

fn code_call(target: &[u8]) -> RpcCall {
    let mut deploy = vec![0; 32];
    deploy.extend_from_slice(&READER_INIT);
    RpcCall {
        to_addr: MULTICALL3.to_vec(),
        data: aggregate3(&[
            call3(&DEPLOYER, true, &deploy),
            call3(&reader_address(), false, &address_word(target)),
        ]),
    }
}

fn read_word(raw: &[u8], at: usize) -> Option<usize> {
    let w = raw.get(at..at.checked_add(32)?)?;
    if w[..24].iter().any(|b| *b != 0) {
        return None;
    }
    usize::try_from(u64::from_be_bytes(w[24..].try_into().ok()?)).ok()
}

/// Return data of the last `(bool success, bytes returnData)` in an `aggregate3` result, if that
/// call succeeded.
fn last_result(raw: &[u8]) -> Option<Vec<u8>> {
    let array = read_word(raw, 0)?;
    let count = read_word(raw, array)?;
    let items = array.checked_add(32)?;
    let element = items.checked_add(read_word(raw, items + 32 * count.checked_sub(1)?)?)?;
    if read_word(raw, element)? != 1 {
        return None;
    }
    let bytes = element.checked_add(read_word(raw, element + 32)?)?;
    let len = read_word(raw, bytes)?;
    raw.get(
        bytes + 32..
            bytes
                .checked_add(32)?
                .checked_add(len)?,
    )
    .map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calldata_matches_reference_encoding() {
        // cast calldata "aggregate3((address,bool,bytes)[])" with the deploy and read calls.
        let expected = concat!(
            "82ad56cb",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000002",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000100",
            "0000000000000000000000004e59b44847b379578588920ca78fbf26c0b4956c",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "000000000000000000000000000000000000000000000000000000000000003b",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "600f600c600039600f6000f3600035803b8060006000843c6000f30000000000",
            "000000000000000000000000b0f118ccad22e3391dba60d0bd9b27b4af4eb593",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000006d9dd143e42b6338f4f6a7c0c26d124658f641cb",
        );
        let call = code_call(&hex!("6d9dd143e42b6338f4f6a7c0c26d124658f641cb"));
        assert_eq!(call.to_addr, MULTICALL3.to_vec());
        assert_eq!(hex::encode(call.data), expected);
    }

    /// `aggregate3` result encoding for the given `(success, returnData)` pairs.
    fn result(entries: &[(bool, &[u8])]) -> Vec<u8> {
        let tuples: Vec<Vec<u8>> = entries
            .iter()
            .map(|(success, data)| {
                let mut t = word(usize::from(*success)).to_vec();
                t.extend_from_slice(&word(0x40));
                t.extend_from_slice(&word(data.len()));
                t.extend_from_slice(data);
                t.resize(t.len() + (32 - data.len() % 32) % 32, 0);
                t
            })
            .collect();
        let mut out = word(0x20).to_vec();
        out.extend_from_slice(&word(tuples.len()));
        let mut offset = 32 * tuples.len();
        for t in &tuples {
            out.extend_from_slice(&word(offset));
            offset += t.len();
        }
        for t in tuples {
            out.extend(t);
        }
        out
    }

    #[test]
    fn decodes_the_reader_return_data() {
        let code = vec![0x60; 37];
        // The deployment fails once the reader exists; only the read matters.
        assert_eq!(last_result(&result(&[(false, &[]), (true, &code)])), Some(code));
        assert_eq!(last_result(&result(&[(true, &[0; 32]), (true, &[])])), Some(vec![]));
    }

    #[test]
    fn rejects_failed_or_truncated_results() {
        assert_eq!(last_result(&result(&[(true, &[]), (false, &[1, 2])])), None);
        let truncated = result(&[(true, &[]), (true, &[0x60; 40])]);
        assert_eq!(last_result(&truncated[..truncated.len() - 32]), None);
        assert_eq!(last_result(&[]), None);
    }
}
