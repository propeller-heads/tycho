//! Owner calls that may lift a gate or change a fee, discovered from the bytecode.
//!
//! A candidate is a state-changing function taking no argument or one `bool`, `address` or
//! `uint`, that writes a storage field the transfer path reads. Each is called by the current
//! owner before the legs run.

use std::collections::BTreeSet;

use alloy::{
    primitives::{Address, Bytes, B256, U256},
    rpc::types::TransactionRequest,
};

use super::{
    code::CodeSummary,
    scenario::{call, BlockGap, LegTiming, Scenario, SECONDS_PER_BLOCK},
};

/// `transfer(address,uint256)` and `transferFrom(address,address,uint256)`.
pub(super) const TRANSFER_SELECTORS: [[u8; 4]; 2] =
    [[0xa9, 0x05, 0x9c, 0xbb], [0x23, 0xb8, 0x72, 0xdd]];
/// `owner()` and `getOwner()`.
const OWNER_GETTERS: [[u8; 4]; 2] = [[0x8d, 0xa5, 0xcb, 0x5b], [0x89, 0x3d, 0x20, 0xe8]];
/// `renounceOwnership()` and `transferOwnership(address)` write the owner slot most transfer
/// paths read, but never unlock, and would take the owner's power away from later calls.
const NEVER_CALLED: [&[u8]; 2] = [&[0x71, 0x50, 0x18, 0xa6], &[0xf2, 0xfd, 0xe3, 0x8b]];
/// Upper bound on owner calls tried per token, each costing one scenario.
const MAX_UNLOCKS: usize = 24;
/// Smallest `uintN` that holds `large_number()`.
const LARGE_NUMBER_MIN_BITS: usize = 104;
/// How long after the owner's calls the legs run in `@later` scenarios: past anti-bot windows
/// that start at opening (a few blocks to 15 minutes in practice), within the 256-block gap
/// `eth_simulateV1` allows.
const AFTER_OPENING: BlockGap = BlockGap { blocks: 200, secs: 200 * SECONDS_PER_BLOCK };

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

    /// Ordering for `MAX_UNLOCKS`: switches before setters.
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

/// One owner call to try.
#[derive(Debug, Clone)]
pub(super) struct Unlock {
    pub selector: Bytes,
    pub argument: Argument,
    tx: TransactionRequest,
}

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

/// Scenarios that may lift a gate: each candidate call alone, single switches again with the
/// legs `AFTER_OPENING` later, and the candidates combined.
pub(super) fn unlock_probes(unlocks: &[Unlock], pool: Address) -> Vec<Probe> {
    let mut probes = combined(unlocks, pool);
    for unlock in unlocks {
        let name = format!("unlock:{}", unlock.name());
        let single = Scenario::new(&name, vec![unlock.tx.clone()], LegTiming::SameBlock);
        probes.push(Probe { scenario: single, unlock: Some(unlock.clone()) });
        if unlock.argument.is_switch() {
            let later = LegTiming::AfterPrefix(AFTER_OPENING);
            let delayed = Scenario::new(&format!("{name}@later"), vec![unlock.tx.clone()], later);
            probes.push(Probe { scenario: delayed, unlock: Some(unlock.clone()) });
        }
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
            LegTiming::SameBlock,
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
                Scenario::new(&format!("unlock:{name}"), prefix.clone(), LegTiming::SameBlock);
            probes.push(Probe { scenario, unlock: None });
        }
        let later = LegTiming::AfterPrefix(AFTER_OPENING);
        let scenario = Scenario::new(&format!("unlock:{name}@later"), prefix, later);
        probes.push(Probe { scenario, unlock: None });
    }
    probes
}

/// Owner calls to every state-changing function that takes at most one `bool`, `address` or
/// `uint` argument and writes a field the transfer path reads. Each is tried with every value
/// from `Argument::candidates`; switches come first when the list is cut to `MAX_UNLOCKS`.
///
/// Functions writing the owner field are left out: making the pool the owner exempts it from
/// the token's checks, which unlocks nothing an owner would do.
pub(super) fn candidate_unlocks(
    code: &CodeSummary,
    token: Address,
    pool: Address,
    owner: Address,
    traced_reads: &BTreeSet<B256>,
) -> Vec<Unlock> {
    let transfer_path = transfer_path_slots(code, traced_reads);
    let owner_fields = owner_fields(code);
    let mut unlocks = Vec::new();
    for function in &code.functions {
        let selector = function.selector.as_ref();
        let Some(values) = function
            .arguments
            .as_deref()
            .and_then(|args| Argument::candidates(args, pool))
        else {
            continue;
        };
        if !function.is_state_changing() || NEVER_CALLED.contains(&selector) {
            continue;
        }
        if !writes_slot(code, &function.selector, &transfer_path) ||
            writes_field(code, &function.selector, &owner_fields)
        {
            continue;
        }
        for argument in values {
            let mut calldata = selector.to_vec();
            if let Some(word) = argument.encode() {
                calldata.extend_from_slice(&word);
            }
            let tx = call(owner, token, calldata);
            unlocks.push(Unlock { selector: function.selector.clone(), argument, tx });
        }
    }
    unlocks.sort_by_key(|u| u.argument.priority());
    unlocks.truncate(MAX_UNLOCKS);
    unlocks
}

/// Slots the transfer path depends on: those evmole attributes to `transfer` or
/// `transferFrom`, plus those a traced leg read without writing.
pub(super) fn transfer_path_slots(
    code: &CodeSummary,
    traced_reads: &BTreeSet<B256>,
) -> BTreeSet<B256> {
    let mut slots = traced_reads.clone();
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
fn owner_fields(code: &CodeSummary) -> BTreeSet<(B256, u8)> {
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
