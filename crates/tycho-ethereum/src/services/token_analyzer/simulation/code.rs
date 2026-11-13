//! What the token's bytecode tells without running it: selectors, argument types, mutability,
//! and the storage fields each function reads and writes, recovered with evmole.

use std::collections::{BTreeSet, HashMap};

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
/// `bytes32(uint256(keccak256("eip1967.proxy.beacon")) - 1)`
const EIP1967_BEACON_SLOT: B256 =
    b256!("a3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50");
/// `implementation()`, which a beacon answers with the code its proxies run.
const IMPLEMENTATION_SELECTOR: [u8; 4] = [0x5c, 0x60, 0xda, 0x1b];

/// Runtime code of an EIP-1167 minimal proxy before and after the 20-byte implementation address.
const MINIMAL_PROXY_PREFIX: [u8; 10] = [0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];
const MINIMAL_PROXY_SUFFIX: [u8; 15] =
    [0x5a, 0xf4, 0x3d, 0x82, 0x80, 0x3e, 0x90, 0x3d, 0x91, 0x60, 0x2b, 0x57, 0xfd, 0x5b, 0xf3];

/// The code to analyse: the token's own, or its implementation's when it is a proxy.
pub(super) struct Code {
    pub bytecode: Bytes,
    /// EIP-1967 implementation, read from the proxy or from its beacon, or EIP-1167
    /// implementation, when the token is a proxy.
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

    /// Not known to be `view` or `pure`: state-changing, or of unknown mutability.
    pub fn may_change_state(&self) -> bool {
        !matches!(self.state_mutability.as_deref(), Some("view") | Some("pure"))
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

/// Reads the token's code and, for EIP-1967 proxies (beacon proxies included) and EIP-1167
/// proxies, its implementation's.
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

/// Stored `address` values by `(slot, offset)`.
pub(super) type StoredAddresses = HashMap<(B256, u8), Address>;

/// The values of the `address` fields `fields` (slot and byte offset) in the token's storage.
/// Fails when the node could not answer.
pub(super) async fn read_address_fields(
    rpc: &EthereumRpcClient,
    token: Address,
    block: BlockNumberOrTag,
    fields: &BTreeSet<(B256, u8)>,
) -> Result<StoredAddresses, String> {
    if fields.is_empty() {
        return Ok(StoredAddresses::new());
    }
    let slots: BTreeSet<B256> = fields
        .iter()
        .map(|(slot, _)| *slot)
        .collect();
    let slots: Vec<B256> = slots.into_iter().collect();
    let values = rpc
        .get_selected_storage(block, token, &slots)
        .await
        .map_err(|e| format!("address fields of token {token}: {e}"))?;
    let mut addresses = HashMap::new();
    for (slot, offset) in fields {
        let word = values
            .get(slot)
            .copied()
            .flatten()
            .unwrap_or_default();
        if let Some(address) = address_at(&word, *offset) {
            addresses.insert((*slot, *offset), address);
        }
    }
    Ok(addresses)
}

/// The `address` packed at byte `offset` from the low-order end of `word`, as Solidity packs
/// storage; `None` when it would not fit.
fn address_at(word: &B256, offset: u8) -> Option<Address> {
    let end = 32usize.checked_sub(usize::from(offset))?;
    let start = end.checked_sub(20)?;
    Some(Address::from_slice(&word[start..end]))
}

/// The implementation an EIP-1167 minimal proxy delegates to, if `code` is one.
fn minimal_proxy_target(code: &[u8]) -> Option<Address> {
    let rest = code.strip_prefix(MINIMAL_PROXY_PREFIX.as_slice())?;
    let (target, suffix) = rest.split_at_checked(20)?;
    (suffix == MINIMAL_PROXY_SUFFIX).then(|| Address::from_slice(target))
}

/// Where an EIP-1967 proxy finds its implementation.
#[derive(Debug, PartialEq, Eq)]
enum Eip1967Target {
    Implementation(Address),
    /// A beacon proxy asks this contract's `implementation()` on each call.
    Beacon(Address),
}

/// The target the EIP-1967 slots hold: the implementation slot first, as a proxy with both set
/// delegates to it; `None` when neither holds an address.
fn eip1967_target(implementation: B256, beacon: B256) -> Option<Eip1967Target> {
    let address_in = |word: B256| Some(Address::from_word(word)).filter(|a| !a.is_zero());
    if let Some(implementation) = address_in(implementation) {
        return Some(Eip1967Target::Implementation(implementation));
    }
    address_in(beacon).map(Eip1967Target::Beacon)
}

/// The implementation of an EIP-1967 proxy, asking the beacon of a beacon proxy. `None` when the
/// token is not one, or the beacon gives no address. Fails when the node could not answer.
async fn eip1967_implementation(
    rpc: &EthereumRpcClient,
    token: Address,
    block: BlockNumberOrTag,
) -> Result<Option<Address>, String> {
    let slots = rpc
        .get_selected_storage(block, token, &[EIP1967_IMPLEMENTATION_SLOT, EIP1967_BEACON_SLOT])
        .await
        .map_err(|e| format!("EIP-1967 slots of token {token}: {e}"))?;
    let word = |slot: B256| {
        slots
            .get(&slot)
            .copied()
            .flatten()
            .unwrap_or_default()
    };
    let target = eip1967_target(word(EIP1967_IMPLEMENTATION_SLOT), word(EIP1967_BEACON_SLOT));
    let beacon = match target {
        None => return Ok(None),
        Some(Eip1967Target::Implementation(implementation)) => return Ok(Some(implementation)),
        Some(Eip1967Target::Beacon(beacon)) => beacon,
    };
    let result = rpc
        .eth_call(call(Address::ZERO, beacon, IMPLEMENTATION_SELECTOR.to_vec()), block)
        .await;
    let out = return_data_unless_reverted(result)
        .map_err(|e| format!("implementation() of beacon {beacon:#x}: {e}"))?;
    let implementation = out
        .filter(|out| out.len() == 32)
        .map(|out| Address::from_word(first_word(out.as_ref()).into()))
        .filter(|address| !address.is_zero());
    Ok(implementation)
}
