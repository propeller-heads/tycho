//! Owner calls that may lift a gate or change a fee, discovered from the bytecode.
//!
//! A candidate is a state-changing function taking no argument or one `bool`, `address` or
//! `uint`, that writes a storage field the transfer path reads. Each is called before the legs
//! run, by the caller `Callers` finds for it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use alloy::{
    primitives::{keccak256, Address, Bytes, B256, U256},
    rpc::types::{state::StateOverride, BlockNumberOrTag, TransactionRequest},
};

use super::{
    code::{CodeSummary, FunctionSummary},
    scenario::{call, derived_address, BlockGap, LegTiming, Scenario, MAX_BLOCK_SKIP},
};
use crate::rpc::EthereumRpcClient;

/// `transfer(address,uint256)` and `transferFrom(address,address,uint256)`.
pub(super) const TRANSFER_SELECTORS: [[u8; 4]; 2] =
    [[0xa9, 0x05, 0x9c, 0xbb], [0x23, 0xb8, 0x72, 0xdd]];
/// ERC-20 functions every holder may call; their writes are not configuration.
pub(super) const HOLDER_FUNCTIONS: [[u8; 4]; 5] = [
    TRANSFER_SELECTORS[0],
    TRANSFER_SELECTORS[1],
    // approve(address,uint256)
    [0x09, 0x5e, 0xa7, 0xb3],
    // increaseAllowance(address,uint256)
    [0x39, 0x50, 0x93, 0x51],
    // decreaseAllowance(address,uint256)
    [0xa4, 0x57, 0xc2, 0xd7],
];
/// `owner()` and `getOwner()`.
const OWNER_GETTERS: [[u8; 4]; 2] = [[0x8d, 0xa5, 0xcb, 0x5b], [0x89, 0x3d, 0x20, 0xe8]];
/// `renounceOwnership()` and `transferOwnership(address)` write the owner slot most transfer
/// paths read, but never unlock, and would take the owner's power away from later calls.
pub(super) const NEVER_CALLED: [&[u8]; 2] = [&[0x71, 0x50, 0x18, 0xa6], &[0xf2, 0xfd, 0xe3, 0x8b]];
/// Upper bound on privileged calls of one kind tried per token, each costing one simulation.
pub(super) const MAX_PRIVILEGED_CALLS: usize = 24;
/// Privileged-call simulations of one token run at the same time; most tokens have fewer, so
/// they run in one round trip.
pub(super) const PRIVILEGED_CALL_CONCURRENCY: usize = 24;
/// Smallest `uintN` that holds `large_number()`.
const LARGE_NUMBER_MIN_BITS: usize = 104;
/// How long after the owner's calls the legs run in `@later` scenarios: past anti-bot windows
/// that start at opening, a few blocks to 15 minutes in practice.
const AFTER_OPENING: BlockGap = BlockGap { blocks: MAX_BLOCK_SKIP, secs: 3600 };

/// The argument an owner call passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Argument {
    None,
    Bool(bool),
    Uint(U256),
    Address(Address),
}

impl Argument {
    /// Values tried for a function taking one argument of `abi_type`, most useful first.
    /// `None` when the type is not one a switch or setter takes.
    fn candidates(abi_type: &str, pool: Address) -> Option<Vec<Argument>> {
        match abi_type {
            "" => Some(vec![Argument::None]),
            "bool" => Some(vec![Argument::Bool(true), Argument::Bool(false)]),
            "address" => Some(vec![Argument::Address(pool)]),
            _ => {
                let bits: usize = abi_type
                    .strip_prefix("uint")?
                    .parse()
                    .ok()?;
                // 0 disables delays and launch windows (`enableTrading(deadBlocks)`) and fees;
                // 100 is a full percentage (`setMaxTxPercent(100)`); the large value lifts
                // absolute limits above any supply.
                let mut values = vec![Argument::Uint(U256::ZERO), Argument::Uint(U256::from(100))];
                if bits >= LARGE_NUMBER_MIN_BITS {
                    values.push(Argument::Uint(large_number()));
                }
                Some(values)
            }
        }
    }

    /// Ordering for `MAX_PRIVILEGED_CALLS`: switches before setters.
    fn priority(self) -> u8 {
        match self {
            Argument::None | Argument::Bool(_) => 0,
            Argument::Address(_) => 1,
            Argument::Uint(_) => 2,
        }
    }

    fn label(self) -> String {
        match self {
            Argument::None => "()".to_string(),
            Argument::Bool(v) => format!("({v})"),
            Argument::Uint(v) if v == large_number() => "(1e30)".to_string(),
            Argument::Uint(v) => format!("({v})"),
            Argument::Address(_) => "(pool)".to_string(),
        }
    }

    fn encode(self) -> Option<[u8; 32]> {
        match self {
            Argument::None => None,
            Argument::Bool(v) => Some(U256::from(u8::from(v)).to_be_bytes()),
            Argument::Uint(v) => Some(v.to_be_bytes()),
            Argument::Address(a) => Some(a.into_word().0),
        }
    }

    fn is_switch(self) -> bool {
        matches!(self, Argument::None | Argument::Bool(true))
    }
}

/// 10^30: above the raw supply of any token in practice, small enough that limit arithmetic
/// on it does not overflow.
pub(super) fn large_number() -> U256 {
    U256::from(10u64).pow(U256::from(30))
}

/// Who calls each privileged function, and the token storage that lets the role holder pass
/// role and allowlist checks.
#[derive(Debug, Clone, Default)]
pub(super) struct Callers {
    /// Possible callers of each function, most likely first; `settle_callers` keeps one.
    pub by_function: HashMap<Bytes, Vec<Address>>,
    /// Entries of the address mappings role-gated functions read, set to 1 for `role_holder()`.
    pub grants: BTreeMap<B256, B256>,
}

impl Callers {
    pub fn of(&self, selector: &Bytes) -> Option<Address> {
        self.by_function
            .get(selector)
            .and_then(|candidates| candidates.first())
            .copied()
    }
}

/// The address the probes give roles to: a fresh address, so that the grants change nothing
/// else.
pub(super) fn role_holder() -> Address {
    derived_address(b"tycho-token-analysis-role-holder")
}

/// `(slot, offset)` of every `address` field a state-changing function reads: the fields an
/// access check compares the caller with.
pub(super) fn address_fields_read_by_setters(code: &CodeSummary) -> BTreeSet<(B256, u8)> {
    let state_changing: BTreeSet<&Bytes> = code
        .functions
        .iter()
        .filter(|f| f.is_state_changing())
        .map(|f| &f.selector)
        .collect();
    let mut fields = BTreeSet::new();
    for record in &code.storage {
        let read_by_setter = record
            .reads
            .iter()
            .any(|sel| state_changing.contains(sel));
        if record.r#type == "address" && read_by_setter {
            fields.insert((record.slot, record.offset));
        }
    }
    fields
}

/// The possible callers of each function that may change state, most likely first, from
/// evmole's view of the storage it reads:
/// - only the owner, when it reads the field `owner()` returns;
/// - else the stored addresses it reads, other than `excluded`: an `admin` or `operator` by any
///   name;
/// - then `role_holder()`, when it reads an `address => bool` or `address => uint8` mapping, as
///   OpenZeppelin's `AccessControl` and allowlists do. The holder's entry in each such mapping is
///   set to 1;
/// - then the owner from `owner()` or `getOwner()`: evmole misses fields, so the owner stays a
///   candidate of every function.
pub(super) fn find_callers(
    code: &CodeSummary,
    owner: Option<Address>,
    stored: &HashMap<(B256, u8), Address>,
    excluded: &[Address],
) -> Callers {
    let owner = owner.filter(|o| !o.is_zero());
    let owner_fields = owner_fields(code);
    let mut callers = Callers::default();
    for function in code
        .functions
        .iter()
        .filter(|f| f.may_change_state())
    {
        let reads: Vec<_> = code
            .storage
            .iter()
            .filter(|record| {
                record
                    .reads
                    .contains(&function.selector)
            })
            .collect();
        let reads_owner = reads
            .iter()
            .any(|record| owner_fields.contains(&(record.slot, record.offset)));
        let mut candidates = Vec::new();
        if !reads_owner {
            for record in &reads {
                let Some(address) = stored.get(&(record.slot, record.offset)) else { continue };
                if !address.is_zero() && !excluded.contains(address) {
                    candidates.push(*address);
                }
            }
            let role_mappings: Vec<B256> = reads
                .iter()
                .filter(|record| is_role_mapping(&record.r#type))
                .map(|record| record.slot)
                .collect();
            for slot in &role_mappings {
                callers
                    .grants
                    .insert(mapping_entry(role_holder(), *slot), B256::with_last_byte(1));
            }
            if !role_mappings.is_empty() {
                candidates.push(role_holder());
            }
        }
        candidates.extend(owner);
        let mut seen = BTreeSet::new();
        candidates.retain(|candidate| seen.insert(*candidate));
        if !candidates.is_empty() {
            callers
                .by_function
                .insert(function.selector.clone(), candidates);
        }
    }
    callers
}

/// Keeps one caller for each function with several candidates: the first whose call with zero
/// arguments does not revert. When every candidate reverts, the function may refuse zero
/// arguments, so the owner, the last candidate, stays. Fails when the node could not answer.
pub(super) async fn settle_callers(
    rpc: &EthereumRpcClient,
    token: Address,
    block: BlockNumberOrTag,
    code: &CodeSummary,
    callers: &mut Callers,
) -> Result<(), String> {
    let mut state = StateOverride::default();
    state
        .entry(token)
        .or_default()
        .state_diff = Some(
        callers
            .grants
            .clone()
            .into_iter()
            .collect(),
    );
    let mut checks = Vec::new();
    for function in &code.functions {
        let Some(candidates) = callers
            .by_function
            .get(&function.selector)
            .filter(|c| c.len() > 1)
        else {
            continue;
        };
        let arguments = function
            .arguments
            .as_deref()
            .map_or(0, |args| {
                args.split(',')
                    .filter(|a| !a.is_empty())
                    .count()
            });
        let mut calldata = function.selector.to_vec();
        calldata.resize(4 + 32 * arguments, 0);
        for candidate in candidates {
            let tx = call(*candidate, token, calldata.clone());
            let state = state.clone();
            let selector = function.selector.clone();
            let candidate = *candidate;
            checks.push(async move {
                let result = rpc
                    .eth_call_with_state_overrides(tx, block, state)
                    .await;
                let passes = match result {
                    Ok(_) => true,
                    Err(error) if error.is_execution_failure() => false,
                    Err(error) => return Err(format!("calling {selector} as {candidate}: {error}")),
                };
                Ok((selector, candidate, passes))
            });
        }
    }
    let results = futures::future::try_join_all(checks).await?;
    let mut passing: HashMap<Bytes, Vec<Address>> = HashMap::new();
    for (selector, candidate, passes) in results {
        if passes {
            passing
                .entry(selector)
                .or_default()
                .push(candidate);
        }
    }
    for (selector, candidates) in &mut callers.by_function {
        if candidates.len() < 2 {
            continue;
        }
        let kept = passing
            .get(selector)
            .and_then(|passed| {
                candidates
                    .iter()
                    .find(|c| passed.contains(c))
            })
            .or_else(|| candidates.last())
            .copied();
        *candidates = kept.into_iter().collect();
    }
    Ok(())
}

fn is_role_mapping(abi_type: &str) -> bool {
    matches!(
        abi_type.replace(' ', "").as_str(),
        "mapping(address=>bool)" | "mapping(address=>uint8)"
    )
}

/// Storage slot of `key`'s entry in the Solidity mapping at `slot`.
pub(super) fn mapping_entry(key: Address, slot: B256) -> B256 {
    keccak256([key.into_word().as_slice(), slot.as_slice()].concat())
}

/// One call to a function of the token, from the caller `Callers` finds for it, with the
/// argument `A` it passes.
#[derive(Debug, Clone)]
pub(super) struct PrivilegedCall<A> {
    pub selector: Bytes,
    pub argument: A,
    pub tx: TransactionRequest,
}

/// Calls to every function that may change state, except those in `NEVER_CALLED`: one for each
/// argument `arguments` gives the function, encoded as the words after the selector. Functions
/// without a caller are left out.
pub(super) fn privileged_calls<A>(
    code: &CodeSummary,
    token: Address,
    callers: &Callers,
    arguments: impl Fn(&FunctionSummary) -> Vec<(A, Vec<[u8; 32]>)>,
) -> Vec<PrivilegedCall<A>> {
    let mut calls = Vec::new();
    for function in &code.functions {
        if !function.may_change_state() || NEVER_CALLED.contains(&function.selector.as_ref()) {
            continue;
        }
        let Some(caller) = callers.of(&function.selector) else { continue };
        for (argument, words) in arguments(function) {
            let mut calldata = function.selector.to_vec();
            for word in words {
                calldata.extend_from_slice(&word);
            }
            let tx = call(caller, token, calldata);
            calls.push(PrivilegedCall { selector: function.selector.clone(), argument, tx });
        }
    }
    calls
}

/// One owner call to try.
pub(super) type Unlock = PrivilegedCall<Argument>;

impl Unlock {
    pub fn name(&self) -> String {
        format!("{}{}", self.selector, self.argument.label())
    }
}

/// A scenario and the single owner call it tries, if it tries exactly one.
pub(super) struct Probe {
    pub scenario: Scenario,
    pub unlock: Option<Unlock>,
}

/// Scenarios that may lift a gate: each candidate call alone, and the candidates combined, also
/// with the legs `AFTER_OPENING` later.
pub(super) fn unlock_probes(unlocks: &[Unlock], pool: Address) -> Vec<Probe> {
    let mut probes = combined(unlocks, pool);
    for unlock in unlocks {
        let name = format!("unlock:{}", unlock.name());
        let single = Scenario::new(&name, vec![unlock.tx.clone()], LegTiming::BuyWithPrefix);
        probes.push(Probe { scenario: single, unlock: Some(unlock.clone()) });
    }
    probes
}

/// Scenarios that may change the fee: each number setter called with zero.
pub(super) fn fee_probes(unlocks: &[Unlock]) -> Vec<Probe> {
    let mut probes = Vec::new();
    for unlock in unlocks {
        if unlock.argument != Argument::Uint(U256::ZERO) {
            continue;
        }
        let scenario = Scenario::new(
            &format!("fee:{}", unlock.name()),
            vec![unlock.tx.clone()],
            LegTiming::BuyWithPrefix,
        );
        probes.push(Probe { scenario, unlock: Some(unlock.clone()) });
    }
    probes
}

/// Owner calls made together, for gates that need several switches or a setter before the
/// switch (`setPair(pool)` then `openTrading()`): `all` is every no-argument and `true` call,
/// `all+pool` puts address setters called with the pool first, `all+pool+max` adds number
/// setters called with a large value. Each also runs with the legs `AFTER_OPENING` the calls.
/// Number setters called with 0 or 100 and `false` calls are left out: they close as many gates
/// as they open.
fn combined(unlocks: &[Unlock], pool: Address) -> Vec<Probe> {
    let txs = |keep: &dyn Fn(Argument) -> bool| -> Vec<TransactionRequest> {
        unlocks
            .iter()
            .filter(|u| keep(u.argument))
            .map(|u| u.tx.clone())
            .collect()
    };
    let switches = txs(&|a| a.is_switch());
    let pool_setters = txs(&|a| a == Argument::Address(pool));
    let large_numbers = txs(&|a| a == Argument::Uint(large_number()));

    let mut combos = vec![("all", switches.clone())];
    let with_pool = [pool_setters.as_slice(), &switches].concat();
    let with_max = [pool_setters.as_slice(), &large_numbers, &switches].concat();
    for (name, prefix) in [("all+pool", with_pool), ("all+pool+max", with_max)] {
        if prefix.len() >
            combos
                .last()
                .map_or(0, |(_, p)| p.len())
        {
            combos.push((name, prefix));
        }
    }

    let mut probes = Vec::new();
    for (name, prefix) in combos {
        if prefix.is_empty() {
            continue;
        }
        if prefix.len() > 1 {
            let scenario =
                Scenario::new(&format!("unlock:{name}"), prefix.clone(), LegTiming::BuyWithPrefix);
            probes.push(Probe { scenario, unlock: None });
        }
        let later = LegTiming::BuyAfterPrefix(AFTER_OPENING);
        let scenario = Scenario::new(&format!("unlock:{name}@later"), prefix, later);
        probes.push(Probe { scenario, unlock: None });
    }
    probes
}

/// Calls to every state-changing function that takes at most one `bool`, `address` or
/// `uint` argument and writes a field the transfer path reads. Each is tried with every value
/// from `Argument::candidates`; switches come first when the list is cut to
/// `MAX_PRIVILEGED_CALLS`.
///
/// Functions writing the owner field are left out: making the pool the owner exempts it from
/// the token's checks, which unlocks nothing an owner would do.
pub(super) fn candidate_unlocks(
    code: &CodeSummary,
    token: Address,
    pool: Address,
    callers: &Callers,
    traced_reads: &BTreeMap<B256, B256>,
) -> Vec<Unlock> {
    let transfer_path = transfer_path_slots(code, traced_reads);
    let owner_fields = owner_fields(code);
    let mut unlocks = privileged_calls(code, token, callers, |function| {
        let Some(values) = function
            .arguments
            .as_deref()
            .and_then(|args| Argument::candidates(args, pool))
        else {
            return Vec::new();
        };
        if !writes_slot(code, &function.selector, &transfer_path) ||
            writes_field(code, &function.selector, &owner_fields)
        {
            return Vec::new();
        }
        values
            .into_iter()
            .map(|argument| (argument, argument.encode().into_iter().collect()))
            .collect()
    });
    unlocks.sort_by_key(|u| u.argument.priority());
    unlocks.truncate(MAX_PRIVILEGED_CALLS);
    unlocks
}

/// Slots the transfer path depends on: those evmole attributes to `transfer` or
/// `transferFrom`, plus those a traced leg read without writing.
pub(super) fn transfer_path_slots(
    code: &CodeSummary,
    traced_reads: &BTreeMap<B256, B256>,
) -> BTreeSet<B256> {
    let mut slots: BTreeSet<B256> = traced_reads.keys().copied().collect();
    for record in &code.storage {
        let read = record.reads.iter().any(|sel| {
            TRANSFER_SELECTORS
                .iter()
                .any(|t| sel.as_ref() == t)
        });
        if read {
            slots.insert(record.slot);
        }
    }
    slots
}

/// `(slot, offset)` of the `address` fields `owner()` or `getOwner()` read. Matched by offset
/// and type because the owner often shares its slot with a trading flag.
pub(super) fn owner_fields(code: &CodeSummary) -> BTreeSet<(B256, u8)> {
    let mut fields = BTreeSet::new();
    for record in &code.storage {
        let read_by_getter = record.reads.iter().any(|sel| {
            OWNER_GETTERS
                .iter()
                .any(|g| sel.as_ref() == g)
        });
        if read_by_getter && record.r#type == "address" {
            fields.insert((record.slot, record.offset));
        }
    }
    fields
}

fn writes_field(code: &CodeSummary, selector: &Bytes, fields: &BTreeSet<(B256, u8)>) -> bool {
    code.storage.iter().any(|record| {
        fields.contains(&(record.slot, record.offset)) && record.writes.contains(selector)
    })
}

fn writes_slot(code: &CodeSummary, selector: &Bytes, slots: &BTreeSet<B256>) -> bool {
    code.storage
        .iter()
        .any(|record| slots.contains(&record.slot) && record.writes.contains(selector))
}
