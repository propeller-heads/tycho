//! Turns simulated outcomes, traces and the bytecode summary into a `TokenAnalysis`.

use std::collections::{BTreeSet, HashMap, HashSet};

use alloy::primitives::{Address, Bytes, B256};
use tycho_common::models::token::{
    ControlSlot, ExternalCall, LegFees, SlotRole, TokenAnalysis, TokenFlag, TokenVerdict,
};

use super::{
    code::CodeSummary,
    owner_calls::{transfer_path_slots, Argument, Probe, TRANSFER_SELECTORS},
    scenario::{swap_topics, Outcome},
    trace::TracedLegs,
    ANALYZER_NAME,
};
use crate::BytesCodec;

/// Fee differences below this many basis points come from rounding, not from the token.
const FEE_TOLERANCE_BPS: i64 = 10;

/// Functions that write a balance in the course of moving the caller's own tokens.
const ORDINARY_BALANCE_WRITERS: [[u8; 4]; 4] = [
    TRANSFER_SELECTORS[0],
    TRANSFER_SELECTORS[1],
    // burn(uint256)
    [0x42, 0x96, 0x6c, 0x68],
    // deliver(uint256)
    [0x3b, 0xd5, 0xd1, 0x73],
];

/// ERC-20 functions every holder may call; their writes are not configuration.
const HOLDER_FUNCTIONS: [[u8; 4]; 5] = [
    TRANSFER_SELECTORS[0],
    TRANSFER_SELECTORS[1],
    // approve(address,uint256)
    [0x09, 0x5e, 0xa7, 0xb3],
    // increaseAllowance(address,uint256)
    [0x39, 0x50, 0x93, 0x51],
    // decreaseAllowance(address,uint256)
    [0xa4, 0x57, 0xc2, 0xd7],
];

/// Router functions a token calls to sell its accumulated fees. Calls to them are routine and
/// do not set `TokenFlag::CallsExternalContract`.
const SWAP_BACK_SELECTORS: [[u8; 4]; 9] = [
    // WETH()
    [0xad, 0x5c, 0x46, 0x48],
    // getAmountsOut(uint256,address[])
    [0xd0, 0x6c, 0xa6, 0x1f],
    // factory()
    [0xc4, 0x5a, 0x01, 0x55],
    // swapExactTokensForETHSupportingFeeOnTransferTokens
    [0x79, 0x1a, 0xc9, 0x47],
    // swapExactTokensForTokensSupportingFeeOnTransferTokens
    [0x5c, 0x11, 0xd7, 0x95],
    // swapExactTokensForETH
    [0x18, 0xcb, 0xaf, 0xe5],
    // swapExactTokensForTokens
    [0x38, 0xed, 0x17, 0x39],
    // addLiquidityETH
    [0xf3, 0x05, 0xd7, 0x19],
    // addLiquidity
    [0xe8, 0xe3, 0x37, 0x00],
];

/// Everything observed about one token at one block.
pub(super) struct Evidence<'a> {
    pub block: u64,
    pub pool: Address,
    /// Whether the base scenarios traded through the pool as real swaps.
    pub swapped: bool,
    pub implementation: Option<Address>,
    pub owner: Option<Address>,
    pub code: &'a CodeSummary,
    pub traces: &'a TracedLegs,
    /// The `base` scenario, which decides the fees and whether the token trades.
    pub base: &'a Outcome,
    /// Every base scenario by name, `base` included: `base_next_block`, `base_large`,
    /// `base_bare` (V2 pairs only), `time+1d`, `time+30d` (when the time shift ran).
    pub scenarios: &'a HashMap<String, Outcome>,
    /// Owner-call scenarios with the probe each ran.
    pub probes: &'a [(Probe, Outcome)],
    pub watchlist: &'a HashSet<Address>,
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
    if let Some(call) = watch_listed(&evidence.traces.calls, evidence.watchlist) {
        let reason = format!("the {} leg calls watch-listed contract {}", call.leg, call.target);
        return (TokenVerdict::NoQuote, Some(reason));
    }
    let base = evidence.base;
    if let Some(bare) = evidence.outcome("base_bare") {
        if environment_sensitive(base, bare) {
            let reason = format!(
                "behaves differently when the transaction looks simulated (zero gas price and \
                 base fee): {} in a realistic environment, {} in a bare one",
                describe(base),
                describe(bare)
            );
            return (TokenVerdict::NoQuote, Some(reason));
        }
    }
    // Before the ok check: a token may let a sell in the buy's block through and refuse it from
    // the next block on.
    let honeypot = if base.only_sell_failed() {
        Some(base)
    } else {
        evidence
            .outcome("base_next_block")
            .filter(|o| o.only_sell_failed())
    };
    if let Some(honeypot) = honeypot {
        let reason = honeypot
            .failure_reason()
            .unwrap_or_else(|| "sells fail while buys and transfers work".to_string());
        return (TokenVerdict::NoQuote, Some(format!("honeypot: {reason}")));
    }
    if base.all_legs_ok() {
        let verdict =
            if base.delivered_exact_amounts() { TokenVerdict::Ok } else { TokenVerdict::Fee };
        return (verdict, None);
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

/// The same legs succeed in one environment and fail in the other, or charge different fees.
fn environment_sensitive(realistic: &Outcome, bare: &Outcome) -> bool {
    realistic.all_legs_ok() != bare.all_legs_ok() ||
        (realistic.all_legs_ok() && fees_differ(&realistic.fees(), &bare.fees()))
}

fn describe(outcome: &Outcome) -> String {
    match outcome.failure_reason() {
        Some(failure) => failure,
        None => format!("all legs succeed, fees {:?}", outcome.fees()),
    }
}

fn watch_listed<'a>(
    calls: &'a [ExternalCall],
    watchlist: &HashSet<Address>,
) -> Option<&'a ExternalCall> {
    calls
        .iter()
        .find(|call| watchlist.contains(&Address::from_bytes(&call.target)))
}

fn flags(evidence: &Evidence, fees: &LegFees) -> Vec<TokenFlag> {
    let mut flags = Vec::new();
    let base = evidence.base;
    let base_ok = base.all_legs_ok();
    let ok = |name: &str| {
        evidence
            .outcome(name)
            .is_some_and(Outcome::all_legs_ok)
    };

    let routine = |call: &ExternalCall| {
        call.selector
            .as_ref()
            .is_none_or(|selector| {
                SWAP_BACK_SELECTORS
                    .iter()
                    .any(|s| selector.as_ref() == s)
            })
    };
    if evidence
        .traces
        .calls
        .iter()
        .any(|call| !routine(call))
    {
        flags.push(TokenFlag::CallsExternalContract);
    }
    if has_balance_writer(evidence.code) {
        flags.push(TokenFlag::BalanceWriter);
    }
    let time_dependent = ["time+1d", "time+30d"]
        .iter()
        .any(|name| {
            let later = evidence
                .outcome(name)
                .filter(|o| o.all_legs_ok());
            base_ok && later.is_some_and(|o| fees_differ(fees, &o.fees()))
        });
    if time_dependent {
        flags.push(TokenFlag::TimeDependentFee);
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
    let drift = base
        .legs
        .as_ref()
        .is_some_and(|legs| !legs.walletDrift.is_zero());
    if drift {
        flags.push(TokenFlag::BalanceDrift);
    }
    let large_failed = evidence
        .outcome("base_large")
        .is_some_and(|large| !large.all_legs_ok());
    if base_ok && large_failed {
        flags.push(TokenFlag::SizeLimited);
    }
    if !base_ok && ok("base_next_block") {
        flags.push(TokenFlag::PerBlockLimit);
    }
    if evidence.implementation.is_some() {
        flags.push(TokenFlag::Proxy);
    }
    if evidence.owner == Some(Address::ZERO) {
        flags.push(TokenFlag::OwnershipRenounced);
    }
    if let Some(bare) = evidence.outcome("base_bare") {
        if environment_sensitive(base, bare) {
            flags.push(TokenFlag::EnvironmentSensitive);
        }
    }
    if !evidence.swapped {
        flags.push(TokenFlag::DirectLegs);
    }
    let sold = base
        .legs
        .as_ref()
        .is_some_and(|legs| legs.sellOk);
    if sold && !base.sell_logged {
        flags.push(TokenFlag::UnloggedTransfer);
    }
    flags
}

fn fees_differ(a: &LegFees, b: &LegFees) -> bool {
    let differ = |x: Option<i64>, y: Option<i64>| match (x, y) {
        (Some(x), Some(y)) => (x - y).abs() > FEE_TOLERANCE_BPS,
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
    };
    differ(a.buy, b.buy) || differ(a.transfer, b.transfer) || differ(a.sell, b.sell)
}

/// A function other than the ordinary ones writes the `address => uint256` mapping `transfer`
/// writes, i.e. the balances.
fn has_balance_writer(code: &CodeSummary) -> bool {
    code.storage.iter().any(|record| {
        let is_balances = record.r#type.replace(' ', "") == "mapping(address=>uint256)";
        let written_by_transfer = record.writes.iter().any(|w| {
            TRANSFER_SELECTORS
                .iter()
                .any(|t| w.as_ref() == t)
        });
        is_balances &&
            written_by_transfer &&
            record.writes.iter().any(|w| {
                !ORDINARY_BALANCE_WRITERS
                    .iter()
                    .any(|o| w.as_ref() == o)
            })
    })
}

/// Fields the transfer path reads, `transfer` does not write, and another state-changing
/// function does: configuration that can change how the token transfers.
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
        let role = writers
            .iter()
            .filter_map(|w| roles.get(w))
            .copied()
            .max_by_key(|role| role_rank(*role))
            .unwrap_or(SlotRole::Unknown);
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
    slots
}

/// The role each single owner call proved for the fields it writes: opening a blocked token
/// proves a gate (a limit when the call passed a number), lowering the fee proves a fee.
fn proven_roles(evidence: &Evidence) -> HashMap<Bytes, SlotRole> {
    let base_ok = evidence.base.all_legs_ok();
    // Probes run in the bare environment; compare their fees with the base run there.
    let reference = evidence
        .outcome("base_bare")
        .unwrap_or(evidence.base);
    let base_tax = i64::try_from(reference.fees().max_tax()).unwrap_or(i64::MAX);
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
        SlotRole::Unknown => 0,
        SlotRole::Limit => 1,
        SlotRole::Gate => 2,
        SlotRole::Fee => 3,
    }
}
