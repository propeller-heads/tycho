//! What the token's bytecode tells without running it: selectors, argument types, mutability,
//! and the storage fields each function reads and writes, recovered with evmole.

use alloy::{
    primitives::{b256, keccak256, Address, Bytes, B256},
    rpc::types::BlockNumberOrTag,
};
use evmole::{contract_info, ContractInfoArgs};

use super::scenario::{call, first_word, return_data_unless_reverted};
use crate::rpc::EthereumRpcClient;

/// `bytes32(uint256(keccak256("eip1967.proxy.implementation")) - 1)`
const EIP1967_IMPLEMENTATION_SLOT: B256 =
    b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");

/// Runtime code of an EIP-1167 minimal proxy before and after the 20-byte implementation address.
const MINIMAL_PROXY_PREFIX: [u8; 10] = [0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];
const MINIMAL_PROXY_SUFFIX: [u8; 15] =
    [0x5a, 0xf4, 0x3d, 0x82, 0x80, 0x3e, 0x90, 0x3d, 0x91, 0x60, 0x2b, 0x57, 0xfd, 0x5b, 0xf3];

/// The code to analyse: the token's own, or its implementation's when it is a proxy.
pub(super) struct Code {
    pub bytecode: Bytes,
    /// EIP-1967 or EIP-1167 implementation, when the token is a proxy.
    pub implementation: Option<Address>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct CodeSummary {
    pub functions: Vec<FunctionSummary>,
    pub storage: Vec<StorageField>,
}

#[derive(Debug, Clone)]
pub(super) struct FunctionSummary {
    pub selector: Bytes,
    /// Comma-separated argument types; `None` when evmole could not recover them.
    pub arguments: Option<String>,
    pub state_mutability: Option<String>,
}

impl FunctionSummary {
    pub fn is_state_changing(&self) -> bool {
        matches!(self.state_mutability.as_deref(), Some("nonpayable") | Some("payable"))
    }
}

/// A storage field inferred by evmole, with the selectors that read and write it.
#[derive(Debug, Clone)]
pub(super) struct StorageField {
    pub slot: B256,
    pub offset: u8,
    pub r#type: String,
    pub reads: Vec<Bytes>,
    pub writes: Vec<Bytes>,
}

/// Reads the token's code and, for EIP-1967 or EIP-1167 proxies, its implementation's.
pub(super) async fn fetch_code(
    rpc: &EthereumRpcClient,
    token: Address,
    block: BlockNumberOrTag,
) -> Result<Code, String> {
    let (code, eip1967) =
        tokio::join!(rpc.eth_get_code(block, token), eip1967_implementation(rpc, token, block));
    let code = code.map_err(|e| format!("code of token {token}: {e}"))?;
    if code.is_empty() {
        return Err(format!("token {token} has no code at block {block}"));
    }
    let implementation = match minimal_proxy_target(code.as_ref()) {
        Some(target) => Some(target),
        None => eip1967?,
    };
    let Some(implementation) = implementation else {
        return Ok(Code { bytecode: Bytes::copy_from_slice(code.as_ref()), implementation: None });
    };
    let bytecode = rpc
        .eth_get_code(block, implementation)
        .await
        .map_err(|e| format!("code of implementation {implementation}: {e}"))?;
    Ok(Code {
        bytecode: Bytes::copy_from_slice(bytecode.as_ref()),
        implementation: Some(implementation),
    })
}

/// Recovers functions and storage layout from `bytecode`. CPU-bound: run it off the async
/// runtime.
pub(super) fn summarize_bytecode(bytecode: &[u8]) -> CodeSummary {
    let info = contract_info(
        ContractInfoArgs::new(bytecode)
            .with_selectors()
            .with_arguments()
            .with_state_mutability()
            .with_storage(),
    );
    let mut functions = Vec::new();
    for f in info.functions.unwrap_or_default() {
        functions.push(FunctionSummary {
            selector: Bytes::copy_from_slice(&f.selector),
            arguments: f.arguments.map(|args| {
                args.iter()
                    .map(|a| a.sol_type_name().into_owned())
                    .collect::<Vec<_>>()
                    .join(",")
            }),
            state_mutability: f
                .state_mutability
                .map(|m| format!("{m:?}").to_lowercase()),
        });
    }
    let mut storage = Vec::new();
    for s in info.storage.unwrap_or_default() {
        storage.push(StorageField {
            slot: B256::from(s.slot),
            offset: s.offset,
            r#type: s.r#type,
            reads: s
                .reads
                .iter()
                .map(|sel| Bytes::copy_from_slice(sel))
                .collect(),
            writes: s
                .writes
                .iter()
                .map(|sel| Bytes::copy_from_slice(sel))
                .collect(),
        });
    }
    CodeSummary { functions, storage }
}

/// Returns `owner()`, else `getOwner()`. `Some(Address::ZERO)` means ownership was renounced;
/// `None` means the token exposes neither function. Fails when the node could not answer, so
/// that a transient error does not pass for a token without an owner.
pub(super) async fn read_owner(
    rpc: &EthereumRpcClient,
    token: Address,
    block: BlockNumberOrTag,
) -> Result<Option<Address>, String> {
    for signature in ["owner()", "getOwner()"] {
        let calldata = keccak256(signature)[..4].to_vec();
        let result = rpc
            .eth_call(call(Address::ZERO, token, calldata), block)
            .await;
        let out = return_data_unless_reverted(result)
            .map_err(|e| format!("{signature} of {token:#x}: {e}"))?;
        if let Some(out) = out.filter(|out| out.len() == 32) {
            return Ok(Some(Address::from_word(first_word(out.as_ref()).into())));
        }
    }
    Ok(None)
}

/// The implementation an EIP-1167 minimal proxy delegates to, if `code` is one.
fn minimal_proxy_target(code: &[u8]) -> Option<Address> {
    let rest = code.strip_prefix(MINIMAL_PROXY_PREFIX.as_slice())?;
    let (target, suffix) = rest.split_at_checked(20)?;
    (suffix == MINIMAL_PROXY_SUFFIX).then(|| Address::from_slice(target))
}

async fn eip1967_implementation(
    rpc: &EthereumRpcClient,
    token: Address,
    block: BlockNumberOrTag,
) -> Result<Option<Address>, String> {
    let slots = rpc
        .get_selected_storage(block, token, &[EIP1967_IMPLEMENTATION_SLOT])
        .await
        .map_err(|e| format!("EIP-1967 slot of token {token}: {e}"))?;
    let value = slots
        .get(&EIP1967_IMPLEMENTATION_SLOT)
        .copied()
        .flatten()
        .unwrap_or_default();
    let implementation = Address::from_slice(&value[12..]);
    Ok((!implementation.is_zero()).then_some(implementation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_minimal_proxies() {
        let target = Address::repeat_byte(0x42);
        let code =
            [MINIMAL_PROXY_PREFIX.as_slice(), target.as_slice(), &MINIMAL_PROXY_SUFFIX].concat();
        assert_eq!(minimal_proxy_target(&code), Some(target));
        assert_eq!(minimal_proxy_target(&code[..code.len() - 1]), None);
        assert_eq!(minimal_proxy_target(&[0x60, 0x80]), None);
    }
}
