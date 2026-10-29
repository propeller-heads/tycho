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

#[cfg(test)]
mod tests {
    use alloy::primitives::keccak256;

    use super::*;
    use crate::services::token_analyzer::simulation::code::{FunctionSummary, StorageField};

    const TOKEN: Address = Address::repeat_byte(7);
    const OWNER: Address = Address::repeat_byte(1);
    const POOL: Address = Address::repeat_byte(2);
    const GATE_SLOT: B256 = B256::repeat_byte(0x0a);
    const OTHER_SLOT: B256 = B256::repeat_byte(0x0b);
    const OWNER_SLOT: B256 = B256::repeat_byte(0x0c);

    fn selector(signature: &str) -> Bytes {
        Bytes::copy_from_slice(&keccak256(signature)[..4])
    }

    fn function(signature: &str, arguments: &str, mutability: &str) -> FunctionSummary {
        FunctionSummary {
            selector: selector(signature),
            arguments: Some(arguments.to_string()),
            state_mutability: Some(mutability.to_string()),
        }
    }

    fn storage(slot: B256, reads: &[&str], writes: &[&str]) -> StorageField {
        StorageField {
            slot,
            offset: 0,
            r#type: "bool".to_string(),
            reads: reads
                .iter()
                .map(|s| selector(s))
                .collect(),
            writes: writes
                .iter()
                .map(|s| selector(s))
                .collect(),
        }
    }

    fn owner_field(slot: B256, offset: u8, writes: &[&str]) -> StorageField {
        StorageField {
            offset,
            r#type: "address".to_string(),
            ..storage(slot, &["owner()"], writes)
        }
    }

    /// A token whose `transfer` reads a gate that `allowtrading()` and `setGate(bool)` write,
    /// plus distractors: a view function, `renounceOwnership()`, and a switch of an unrelated
    /// slot.
    fn gated_token() -> CodeSummary {
        CodeSummary {
            functions: vec![
                function("transfer(address,uint256)", "address,uint256", "nonpayable"),
                function("allowtrading()", "", "nonpayable"),
                function("setGate(bool)", "bool", "nonpayable"),
                function("gate()", "", "view"),
                function("renounceOwnership()", "", "nonpayable"),
                function("setUnrelated()", "", "nonpayable"),
            ],
            storage: vec![
                storage(
                    GATE_SLOT,
                    &["transfer(address,uint256)", "gate()"],
                    &["allowtrading()", "setGate(bool)", "renounceOwnership()"],
                ),
                storage(OTHER_SLOT, &[], &["setUnrelated()"]),
            ],
        }
    }

    /// `gated_token` plus setters of the gate slot: a pool setter, an absolute limit, a small
    /// percentage, and `transferOwnership`, which must never be called.
    fn token_with_setters() -> CodeSummary {
        let mut code = gated_token();
        for (signature, arguments) in [
            ("setPair(address)", "address"),
            ("setMaxTx(uint256)", "uint256"),
            ("setMaxTxPercent(uint8)", "uint8"),
            ("transferOwnership(address)", "address"),
        ] {
            code.functions
                .push(function(signature, arguments, "nonpayable"));
            code.storage[0]
                .writes
                .push(selector(signature));
        }
        code
    }

    fn names(code: &CodeSummary, traced: &BTreeSet<B256>) -> Vec<String> {
        candidate_unlocks(code, TOKEN, POOL, OWNER, traced)
            .iter()
            .map(Unlock::name)
            .collect()
    }

    fn calldata(tx: &TransactionRequest) -> Bytes {
        tx.input
            .input()
            .expect("calldata set")
            .clone()
    }

    #[test]
    fn switches_of_transfer_path_state_become_candidates() {
        let (allow, gate) = (selector("allowtrading()"), selector("setGate(bool)"));
        assert_eq!(
            names(&gated_token(), &BTreeSet::new()),
            [format!("{allow}()"), format!("{gate}(true)"), format!("{gate}(false)")]
        );
    }

    #[test]
    fn traced_reads_extend_the_transfer_path() {
        let traced = BTreeSet::from([OTHER_SLOT]);
        let unrelated = format!("{}()", selector("setUnrelated()"));
        assert!(names(&gated_token(), &traced).contains(&unrelated));
    }

    #[test]
    fn setters_are_tried_with_the_pool_and_numbers_after_switches() {
        let (pair, max, percent) = (
            selector("setPair(address)"),
            selector("setMaxTx(uint256)"),
            selector("setMaxTxPercent(uint8)"),
        );
        assert_eq!(
            names(&token_with_setters(), &BTreeSet::new())[3..],
            [
                format!("{pair}(pool)"),
                format!("{max}(0)"),
                format!("{max}(100)"),
                format!("{max}(1e30)"),
                format!("{percent}(0)"),
                format!("{percent}(100)"),
            ]
        );
    }

    #[test]
    fn calls_writing_the_owner_field_are_never_tried() {
        let mut code = gated_token();
        let guardian = "setGuardian(address)";
        code.functions
            .push(function(guardian, "address", "nonpayable"));
        code.storage[0]
            .writes
            .push(selector(guardian));
        code.storage
            .push(owner_field(OWNER_SLOT, 0, &[guardian]));
        assert!(!names(&code, &BTreeSet::new())
            .iter()
            .any(|n| n.starts_with(&selector(guardian).to_string())));
    }

    #[test]
    fn a_switch_packed_next_to_the_owner_is_still_tried() {
        let mut code = gated_token();
        // The gate bool sits at offset 0 of the slot whose offset 1 holds the owner; evmole
        // attributes the `owner()` read to both fields.
        code.storage[0]
            .reads
            .push(selector("owner()"));
        code.storage
            .push(owner_field(GATE_SLOT, 1, &["transferOwnership(address)"]));
        assert!(
            names(&code, &BTreeSet::new()).contains(&format!("{}()", selector("allowtrading()")))
        );
    }

    #[test]
    fn combined_probes_put_setters_before_switches_and_also_run_later() {
        let unlocks =
            candidate_unlocks(&token_with_setters(), TOKEN, POOL, OWNER, &BTreeSet::new());
        let probes = unlock_probes(&unlocks, POOL);
        let find = |name: &str| {
            probes
                .iter()
                .find(|p| p.scenario.name == name)
                .unwrap_or_else(|| panic!("probe {name} present"))
        };
        let with_max = find("unlock:all+pool+max");
        let selectors: Vec<Bytes> = with_max
            .scenario
            .prefix
            .iter()
            .map(|tx| calldata(tx).slice(..4))
            .collect();
        assert_eq!(
            selectors,
            [
                selector("setPair(address)"),
                selector("setMaxTx(uint256)"),
                selector("allowtrading()"),
                selector("setGate(bool)"),
            ]
        );
        assert_eq!(
            U256::from_be_slice(&calldata(&with_max.scenario.prefix[1])[4..]),
            large_number()
        );
        assert!(with_max.unlock.is_none());
        assert_eq!(find("unlock:all@later").scenario.timing, LegTiming::AfterPrefix(AFTER_OPENING));
        let single = find(&format!("unlock:{}()@later", selector("allowtrading()")));
        assert_eq!(
            single
                .unlock
                .as_ref()
                .map(|u| u.argument),
            Some(Argument::None)
        );
        for tx in &find("unlock:all").scenario.prefix {
            assert_eq!(tx.from, Some(OWNER));
            assert_eq!(tx.to, Some(TOKEN.into()));
        }
    }

    #[test]
    fn fee_probes_call_number_setters_with_zero() {
        let unlocks =
            candidate_unlocks(&token_with_setters(), TOKEN, POOL, OWNER, &BTreeSet::new());
        let probes = fee_probes(&unlocks);
        assert_eq!(probes.len(), 2);
        for probe in probes {
            assert_eq!(probe.unlock.map(|u| u.argument), Some(Argument::Uint(U256::ZERO)));
        }
    }

    #[test]
    fn small_uints_skip_the_large_value() {
        assert_eq!(
            Argument::candidates("uint8", POOL),
            Some(vec![Argument::Uint(U256::ZERO), Argument::Uint(U256::from(100))])
        );
        assert_eq!(Argument::candidates("uint256", POOL).map(|v| v.len()), Some(3));
        assert_eq!(Argument::candidates("address,uint256", POOL), None);
        assert_eq!(Argument::candidates("uintx", POOL), None);
    }
}
