//! Turns simulated outcomes, traces and the bytecode summary into a `TokenAnalysis`.

use std::collections::{BTreeSet, HashMap, HashSet};

use alloy::primitives::{Address, Bytes, B256};
use tycho_common::models::token::{
    ControlSlot, LegFees, SlotRole, TokenAnalysis, TokenFlag, TokenVerdict,
};

use super::{
    code::CodeSummary,
    fee_fields::FeeStart,
    owner_calls::{transfer_path_slots, Argument, Probe, HOLDER_FUNCTIONS, TRANSFER_SELECTORS},
    scenario::{swap_topics, Outcome},
    trace::TracedLegs,
    ANALYZER_NAME,
};
use crate::BytesCodec;

/// Fee differences below this many basis points come from rounding, not from the token.
const FEE_TOLERANCE_BPS: i64 = 10;

/// Everything observed about one token at one block.
pub(super) struct Evidence<'a> {
    pub block: u64,
    pub pool: Address,
    /// Whether the base scenarios traded through the pool as real swaps.
    pub swapped: bool,
    /// Whether the pool held too little of the token and got a balance for the analysis.
    pub funded: bool,
    pub implementation: Option<Address>,
    pub owner: Option<Address>,
    pub code: &'a CodeSummary,
    pub traces: &'a TracedLegs,
    /// The `base` scenario, which decides the fees and whether the token trades.
    pub base: &'a Outcome,
    /// Every base scenario by name, `base` included: `base_large` and `time+30d`, when they
    /// ran.
    pub scenarios: &'a HashMap<String, Outcome>,
    /// Owner-call scenarios with the probe each ran.
    pub probes: &'a [(Probe, Outcome)],
    /// Functions whose privileged caller lowered the pool's balance of the token.
    pub seizures: &'a [Bytes],
    /// Field writes that started a fee in a token charging none.
    pub fee_starts: &'a [FeeStart],
}

impl Evidence<'_> {
    fn outcome(&self, name: &str) -> Option<&Outcome> {
        self.scenarios.get(name)
    }
}

pub(super) fn classify(evidence: &Evidence) -> TokenAnalysis {
    let fees = evidence.base.fees();
    let (verdict, reason) = verdict(evidence);
    TokenAnalysis {
        verdict,
        reason,
        fees,
        gas: evidence.base.transfer_gas(),
        block: Some(evidence.block),
        pool: Some(evidence.pool.to_bytes()),
        analyzer: ANALYZER_NAME.to_string(),
        flags: flags(evidence, &fees),
        external_calls: evidence.traces.calls.clone(),
        control_slots: control_slots(evidence),
    }
}

fn verdict(evidence: &Evidence) -> (TokenVerdict, Option<String>) {
    let base = evidence.base;
    if base.only_sell_failed() {
        let reason = base
            .failure_reason()
            .unwrap_or_else(|| "sells fail while buys and transfers work".to_string());
        // A pool without real liquidity can fail a sell on its own, e.g. a token selling its
        // fees through an empty pair: analyse again once the liquidity arrives.
        if evidence.funded {
            let reason = format!("sells fail before the pool has liquidity: {reason}");
            return (TokenVerdict::Blocked, Some(reason));
        }
        return (TokenVerdict::NoQuote, Some(format!("honeypot: {reason}")));
    }
    if let Some(selector) = evidence.seizures.first() {
        let reason = format!(
            "the caller of function {selector} can take tokens out of the pool, which leaves the \
             pool's reserves wrong"
        );
        return (TokenVerdict::NoQuote, Some(reason));
    }
    if base.all_legs_ok() {
        if let Some(reason) = fee_outside_model(evidence) {
            return (TokenVerdict::VariableFee, Some(reason));
        }
        if !base.delivered_exact_amounts() {
            return (TokenVerdict::Fee, None);
        }
        // The called contract can start charging or refusing transfers at any time, so exact
        // amounts today promise nothing.
        if let Some(call) = evidence.traces.calls.first() {
            let reason = format!(
                "the {} leg calls contract {}, which can change how the token transfers",
                call.leg, call.target
            );
            return (TokenVerdict::Fee, Some(reason));
        }
        // A privileged caller can start a fee at any time, so exact amounts today promise
        // nothing either.
        if let Some(start) = evidence.fee_starts.first() {
            return (TokenVerdict::Fee, Some(start.describe()));
        }
        return (TokenVerdict::Ok, None);
    }
    let reason = base.failure_reason();
    let opened_by_time = evidence
        .scenarios
        .iter()
        .any(|(name, outcome)| name.starts_with("time+") && outcome.all_legs_ok());
    let opened_by_owner = evidence
        .probes
        .iter()
        .any(|(probe, outcome)| {
            probe
                .scenario
                .name
                .starts_with("unlock:") &&
                outcome.unlocked()
        });
    if opened_by_time || opened_by_owner {
        return (TokenVerdict::TemporarilyBlocked, reason);
    }
    (TokenVerdict::Blocked, reason)
}

fn flags(evidence: &Evidence, fees: &LegFees) -> Vec<TokenFlag> {
    let mut flags = Vec::new();
    let base = evidence.base;
    let base_ok = base.all_legs_ok();
    if !evidence.traces.calls.is_empty() {
        flags.push(TokenFlag::CallsExternalContract);
    }
    if !evidence.seizures.is_empty() {
        flags.push(TokenFlag::OwnerCanReducePoolBalance);
    }
    if fee_outside_model(evidence).is_some() {
        flags.push(TokenFlag::FeeOutsideModel);
    }
    if !evidence.fee_starts.is_empty() {
        flags.push(TokenFlag::FeeCanStart);
    }
    let legs = [fees.buy, fees.transfer, fees.sell];
    let measured: Vec<i64> = legs.into_iter().flatten().collect();
    let spread = measured
        .iter()
        .max()
        .zip(measured.iter().min())
        .map(|(max, min)| max - min);
    if spread.is_some_and(|s| s > FEE_TOLERANCE_BPS) {
        flags.push(TokenFlag::DirectionDependentFee);
    }
    let swapped = evidence
        .scenarios
        .values()
        .any(|outcome| {
            outcome.leg_logs.iter().any(|log| {
                log.topics()
                    .first()
                    .is_some_and(|t| swap_topics().contains(t))
            })
        });
    if swapped {
        flags.push(TokenFlag::SwapsOnTransfer);
    }
    let large_failed = evidence
        .outcome("base_large")
        .is_some_and(|large| !large.all_legs_ok());
    if base_ok && large_failed {
        flags.push(TokenFlag::SizeLimited);
    }
    if evidence.implementation.is_some() {
        flags.push(TokenFlag::Proxy);
    }
    if evidence.owner == Some(Address::ZERO) {
        flags.push(TokenFlag::OwnershipRenounced);
    }
    if !evidence.swapped {
        flags.push(TokenFlag::DirectLegs);
    }
    if evidence.funded {
        flags.push(TokenFlag::FundedPool);
    }
    flags
}

/// Scenarios that repeat `base` with one thing changed that a constant fee per leg ignores.
const FEE_MODEL_CHECKS: [(&str, &str); 2] =
    [("base_large", "a 4 times larger trade"), ("time+30d", "30 days later")];

/// Why one constant fee per leg does not describe the token: a scenario of `FEE_MODEL_CHECKS`
/// whose legs all succeed with a fee different from `base`'s. `None` when `base` failed or every
/// such scenario charges the same fees.
fn fee_outside_model(evidence: &Evidence) -> Option<String> {
    let base = evidence.base;
    if !base.all_legs_ok() {
        return None;
    }
    let fees = base.fees();
    for (name, change) in FEE_MODEL_CHECKS {
        let Some(other) = evidence
            .outcome(name)
            .filter(|o| o.all_legs_ok())
        else {
            continue;
        };
        if fees_differ(&fees, &other.fees()) {
            return Some(format!(
                "the fees change with {change}: {} at first, {} then",
                describe_fees(&fees),
                describe_fees(&other.fees())
            ));
        }
    }
    None
}

/// For example "buy 1999 bp, transfer 0 bp, sell -", where "-" is a leg without a fee.
fn describe_fees(fees: &LegFees) -> String {
    let leg = |fee: Option<i64>| match fee {
        Some(bps) => format!("{bps} bp"),
        None => "-".to_string(),
    };
    format!("buy {}, transfer {}, sell {}", leg(fees.buy), leg(fees.transfer), leg(fees.sell))
}

fn fees_differ(a: &LegFees, b: &LegFees) -> bool {
    let differ = |x: Option<i64>, y: Option<i64>| match (x, y) {
        (Some(x), Some(y)) => (x - y).abs() > FEE_TOLERANCE_BPS,
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
    };
    differ(a.buy, b.buy) || differ(a.transfer, b.transfer) || differ(a.sell, b.sell)
}

/// Fields the transfer path reads, `transfer` does not write, and another state-changing
/// function does: configuration that can change how the token transfers. Only fields whose role
/// an owner call proved: evmole finds many fields it cannot tell apart, and a field without a
/// role tells a reader nothing it can act on.
fn control_slots(evidence: &Evidence) -> Vec<ControlSlot> {
    let code = evidence.code;
    let transfer_path: BTreeSet<B256> = transfer_path_slots(code, &evidence.traces.read_only_slots);
    let state_changing: HashSet<&Bytes> = code
        .functions
        .iter()
        .filter(|f| f.is_state_changing())
        .map(|f| &f.selector)
        .collect();
    let roles = proven_roles(evidence);

    let mut slots = Vec::new();
    for record in &code.storage {
        if !transfer_path.contains(&record.slot) {
            continue;
        }
        let written_by_holders = record.writes.iter().any(|w| {
            TRANSFER_SELECTORS
                .iter()
                .any(|t| w.as_ref() == t)
        });
        if written_by_holders {
            continue;
        }
        let writers: Vec<Bytes> = record
            .writes
            .iter()
            .filter(|w| state_changing.contains(w))
            .filter(|w| {
                !HOLDER_FUNCTIONS
                    .iter()
                    .any(|h| w.as_ref() == h)
            })
            .cloned()
            .collect();
        if writers.is_empty() {
            continue;
        }
        let Some(role) = writers
            .iter()
            .filter_map(|w| roles.get(w))
            .copied()
            .max_by_key(|role| role_rank(*role))
        else {
            continue;
        };
        slots.push(ControlSlot {
            slot: record.slot.to_bytes(),
            offset: record.offset,
            writers: writers
                .into_iter()
                .map(|w| tycho_common::Bytes::from(w.to_vec()))
                .collect(),
            role,
        });
    }
    slots.extend(fee_start_slots(evidence, &slots));
    slots
}

/// The fields whose write alone started a fee, other than those in `known`: each proves a fee
/// field. A mapping entry is the slot of the key the write used.
fn fee_start_slots(evidence: &Evidence, known: &[ControlSlot]) -> Vec<ControlSlot> {
    let mut slots: Vec<ControlSlot> = Vec::new();
    for start in evidence.fee_starts {
        let [write] = start.probe.writes.as_slice() else { continue };
        let field = &write.field;
        let slot = field.slot.to_bytes();
        let listed = known
            .iter()
            .chain(&slots)
            .any(|s| s.slot == slot && s.offset == field.offset);
        if listed {
            continue;
        }
        slots.push(ControlSlot {
            slot,
            offset: field.offset,
            writers: field
                .writers
                .iter()
                .map(|w| tycho_common::Bytes::from(w.to_vec()))
                .collect(),
            role: SlotRole::Fee,
        });
    }
    slots
}

/// The role each single owner call proved for the fields it writes: opening a blocked token
/// proves a gate (a limit when the call passed a number), lowering the fee proves a fee.
fn proven_roles(evidence: &Evidence) -> HashMap<Bytes, SlotRole> {
    let base_ok = evidence.base.all_legs_ok();
    let base_tax = i64::try_from(evidence.base.fees().max_tax()).unwrap_or(i64::MAX);
    let mut roles = HashMap::new();
    for (probe, outcome) in evidence.probes {
        let Some(unlock) = &probe.unlock else { continue };
        let role = if probe.scenario.name.starts_with("fee:") {
            let tax = i64::try_from(outcome.fees().max_tax()).unwrap_or(i64::MAX);
            let lowered = outcome.all_legs_ok() && tax + FEE_TOLERANCE_BPS < base_tax;
            lowered.then_some(SlotRole::Fee)
        } else if !base_ok && outcome.unlocked() {
            match unlock.argument {
                Argument::Uint(_) => Some(SlotRole::Limit),
                Argument::None | Argument::Bool(_) | Argument::Address(_) => Some(SlotRole::Gate),
            }
        } else {
            None
        };
        if let Some(role) = role {
            let entry = roles
                .entry(unlock.selector.clone())
                .or_insert(role);
            if role_rank(role) > role_rank(*entry) {
                *entry = role;
            }
        }
    }
    roles
}

fn role_rank(role: SlotRole) -> u8 {
    match role {
        SlotRole::Limit => 1,
        SlotRole::Gate => 2,
        SlotRole::Fee => 3,
    }
}
