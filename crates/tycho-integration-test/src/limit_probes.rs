//! Limit probes: for a protocol whose package is in strict mode, each sampled direction is also
//! swapped at one size relative to its limit, in rotation, and judged with the strict checks.
//!
//! The realistic swap `process_state` makes for every protocol is left as it is, with its own
//! metrics; a probe only feeds `tycho_integration_check_total` and
//! `tycho_integration_probe_drift_bps`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use num_bigint::BigUint;
use num_traits::Zero;
use tracing::{error, info};
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
};
use tycho_simulation::protocol::models::ProtocolComponent;
use tycho_test::{
    checks::{
        execution::{check_execution_slippage, drift, missing_execution, missing_revert},
        ladder::{at_execution_cap, executable_max_input, ProbeSize},
        limits::{check_direction, Limit, Quote},
        outcome::CheckId,
    },
    execution::{
        encoding::encode_swap,
        models::{TychoExecutionInput, TychoExecutionResult},
    },
    expectations::{Direction, Limits, PackageExpectations, Tolerance},
};

use crate::{fee_fetcher::RouterFeeOnOutput, metrics, packages::StrictPackages};

/// The largest amount the probes swap: Uniswap V3 and V4 reject an amount above 96 bits.
fn max_probe_amount() -> BigUint {
    BigUint::from(2u128.pow(96) - 1)
}

/// The strict packages, and the size each direction is probed at next.
pub struct StrictChecks {
    packages: StrictPackages,
    rotation: Mutex<HashMap<(String, Direction), usize>>,
}

impl StrictChecks {
    pub fn new(packages: StrictPackages) -> Self {
        Self { packages, rotation: Mutex::new(HashMap::new()) }
    }

    /// The declarations of `protocol_system`, when its package is strict.
    pub fn package(&self, protocol_system: &str) -> Option<&PackageExpectations> {
        self.packages.get(protocol_system)
    }

    /// The protocol systems with a strict package.
    pub fn protocols(&self) -> impl Iterator<Item = &str> {
        self.packages.protocols()
    }

    /// The size to probe `direction` of `component` at this time: each of [`ProbeSize::ALL`] in
    /// turn, so every size comes up within four samples of a direction.
    fn next_size(&self, component: &str, direction: &Direction) -> ProbeSize {
        let mut rotation = self
            .rotation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let turn = rotation
            .entry((component.to_lowercase(), direction.clone()))
            .or_default();
        let size = ProbeSize::ALL[*turn % ProbeSize::ALL.len()];
        *turn += 1;
        size
    }
}

/// Which strict check a probe's execution answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeRole {
    /// A fraction of the limit: the execution matches the simulation within the tolerance.
    Fraction,
    /// The limit itself: the execution matches the quote at the limit.
    AtLimit,
    /// Past the limit: past a hard limit's bound the execution reverts, and one unit past an
    /// under-cap limit it executes. `at_limit` is the simulation id of the swap at the limit in
    /// the same batch, without which what happens past it proves nothing.
    PastLimit { at_limit: String, must_revert: bool },
}

/// A probe swap, queued with the block's other executions.
#[derive(Debug, Clone)]
pub struct Probe {
    pub protocol: String,
    pub component: String,
    /// `IN -> OUT`, by token symbol, for the logs.
    pub direction: String,
    pub size: ProbeSize,
    pub role: ProbeRole,
    pub tolerance: Tolerance,
}

impl Probe {
    fn check(&self) -> CheckId {
        match self.role {
            ProbeRole::Fraction => CheckId::ExecLadder,
            ProbeRole::AtLimit => CheckId::LimitsAtLimit,
            ProbeRole::PastLimit { .. } => CheckId::LimitsPastLimit,
        }
    }
}

/// What a probe of one direction needs from `process_state`.
pub struct Target<'a> {
    pub chain: Chain,
    pub component: &'a ProtocolComponent,
    pub state: &'a dyn ProtocolSim,
    pub token_in: &'a Token,
    pub token_out: &'a Token,
    pub max_input: &'a BigUint,
    pub max_output: &'a BigUint,
    /// The prefix of the probe's simulation ids, unique to the component and direction.
    pub simulation_id: String,
}

/// Records one check outcome, and logs a failure with the component and the detail, which the
/// metric leaves out.
fn record(probe: &ProbeContext<'_>, check: CheckId, outcome: Outcome<'_>) {
    let (label, reason) = match outcome {
        Outcome::Pass => ("pass", ""),
        Outcome::Fail { reason, detail } => {
            error!(
                event_type = "check_failure",
                protocol = probe.protocol,
                component = probe.component,
                direction = probe.direction,
                size = %probe.size,
                check = check.as_str(),
                reason,
                "{detail}"
            );
            ("fail", reason)
        }
        Outcome::NotRun { reason } => ("not_run", reason),
    };
    metrics::record_check(probe.protocol, check.as_str(), label, reason, &probe.size.to_string());
}

/// The outcome of one check, with a reason from a fixed set for the metric.
enum Outcome<'a> {
    Pass,
    Fail { reason: &'a str, detail: String },
    NotRun { reason: &'a str },
}

/// Who a recorded outcome is about.
struct ProbeContext<'a> {
    protocol: &'a str,
    component: &'a str,
    direction: &'a str,
    size: ProbeSize,
}

/// Probes one direction of a strict package's component at the size its turn comes to.
///
/// Records the outcomes the simulation alone decides (a quote that errors, the max output, a
/// direction that cannot be probed), and returns the swaps to execute, by simulation id, for
/// [`judge`] once they ran.
pub fn probe_direction(
    strict: &StrictChecks,
    package: &PackageExpectations,
    target: &Target<'_>,
) -> Vec<(String, TychoExecutionInput, Probe)> {
    let Target { component, state, token_in, token_out, max_input, max_output, .. } = target;
    let component_id = component.id.to_string();
    let direction =
        Direction { token_in: token_in.address.clone(), token_out: token_out.address.clone() };
    let label = format!("{} -> {}", token_in.symbol, token_out.symbol);
    let size = strict.next_size(&component_id, &direction);
    let context = ProbeContext {
        protocol: &component.protocol_system,
        component: &component_id,
        direction: &label,
        size,
    };
    let checks: &[CheckId] = match size {
        ProbeSize::Fraction(_) => &[CheckId::ExecLadder],
        ProbeSize::Limit => &CheckId::LIMITS,
    };
    let not_run = |reason| {
        for check in checks {
            record(&context, *check, Outcome::NotRun { reason });
        }
    };

    let executable = executable_max_input(max_input);
    if executable.is_zero() {
        not_run("zero_limit");
        return Vec::new();
    }
    let tolerance = package.tolerance();
    let probe = |role| Probe {
        protocol: component.protocol_system.clone(),
        component: component_id.clone(),
        direction: label.clone(),
        size,
        role,
        tolerance,
    };
    let quote = |amount: BigUint| {
        state
            .get_amount_out(amount, token_in, token_out)
            .map(|quote| Quote { amount: quote.amount, gas: quote.gas })
            .map_err(|e| e.to_string())
    };

    match size {
        ProbeSize::Fraction(_) => {
            let amount = size.amount(&executable);
            if amount.is_zero() {
                not_run("zero_amount");
                return Vec::new();
            }
            if amount > max_probe_amount() {
                not_run("execution_cap");
                return Vec::new();
            }
            let quoted = match quote(amount.clone()) {
                Ok(quoted) => quoted,
                Err(e) => {
                    record(
                        &context,
                        CheckId::SimLadder,
                        Outcome::Fail {
                            reason: "quote_error",
                            detail: format!("quoting {amount} {} failed: {e}", token_in.symbol),
                        },
                    );
                    return Vec::new();
                }
            };
            encoded(target, &context, "fraction", amount, &quoted, probe(ProbeRole::Fraction))
                .into_iter()
                .collect()
        }
        ProbeSize::Limit => {
            let Some(limits) = package.limits_for(&component_id, &direction) else {
                not_run("no_declaration");
                return Vec::new();
            };
            if executable > max_probe_amount() {
                not_run("execution_cap");
                return Vec::new();
            }
            let capped = at_execution_cap(max_input);
            let checked = check_direction(
                quote,
                &label,
                &Limit { max_input: &executable, max_output, capped },
                limits,
            );
            for (check, detail) in &checked.failures {
                record(&context, *check, Outcome::Fail { reason: "quote", detail: detail.clone() });
            }
            let failed = |check| {
                checked
                    .failures
                    .iter()
                    .any(|(failed, _)| *failed == check)
            };
            match &checked.at_limit_quote {
                Some(_) if !failed(CheckId::LimitsMaxOutput) => {
                    record(&context, CheckId::LimitsMaxOutput, Outcome::Pass)
                }
                Some(_) => {}
                None => record(
                    &context,
                    CheckId::LimitsMaxOutput,
                    Outcome::NotRun { reason: "at_limit_failed" },
                ),
            }
            // A soft limit's claim, that twice the limit quotes, is checked in simulation alone; a
            // hard or under-cap limit's needs the swap past it executed.
            let past_limit_decided = if capped {
                record(
                    &context,
                    CheckId::LimitsPastLimit,
                    Outcome::NotRun { reason: "execution_cap" },
                );
                true
            } else if let Limits::Soft { .. } = limits {
                if !failed(CheckId::LimitsPastLimit) {
                    record(&context, CheckId::LimitsPastLimit, Outcome::Pass);
                }
                true
            } else {
                false
            };

            let Some(at_quote) = &checked.at_limit_quote else {
                if !past_limit_decided && !failed(CheckId::LimitsPastLimit) {
                    record(
                        &context,
                        CheckId::LimitsPastLimit,
                        Outcome::NotRun { reason: "at_limit_failed" },
                    );
                }
                return Vec::new();
            };
            let Some(at_limit) = encoded(
                target,
                &context,
                "at_limit",
                executable,
                at_quote,
                probe(ProbeRole::AtLimit),
            ) else {
                return Vec::new();
            };
            let mut swaps = vec![at_limit];
            if let Some(past) = checked.past_limit_swap {
                let role = ProbeRole::PastLimit {
                    at_limit: swaps[0].0.clone(),
                    must_revert: past.must_revert,
                };
                if let Some(past_limit) = encoded(
                    target,
                    &context,
                    "past_limit",
                    past.amount_in,
                    // The minimum output is the quote at the limit, so past a hard limit a venue
                    // that accepts the input executes the swap, and only a revert passes.
                    at_quote,
                    probe(role),
                ) {
                    swaps.push(past_limit);
                }
            }
            swaps
        }
    }
}

/// The swap of `amount` at `quote`, encoded for the production router, with its simulation id.
/// An encoding error is recorded as the probe's check not run, since it is not the venue's.
fn encoded(
    target: &Target<'_>,
    context: &ProbeContext<'_>,
    kind: &str,
    amount: BigUint,
    quote: &Quote,
    probe: Probe,
) -> Option<(String, TychoExecutionInput, Probe)> {
    let check = probe.check();
    match encode_swap(
        target.component,
        Some(Arc::from(target.state.clone_box())),
        target.token_in,
        target.token_out,
        amount,
        target.chain,
        None,
        quote.gas.clone(),
        quote.amount.clone(),
    ) {
        Ok((solution, transaction)) => Some((
            format!("{}-probe-{kind}", target.simulation_id),
            TychoExecutionInput {
                solution,
                estimated_gas: transaction.estimated_gas().clone(),
                transaction,
                expected_amount_out: quote.amount.clone(),
                protocol_system: target.component.protocol_system.clone(),
                component_id: target.component.id.to_string(),
                token_in: target.token_in.address.to_string(),
                token_out: target.token_out.address.to_string(),
                // Probes skip price level venues, the only ones with account overwrites.
                account_overwrites: None,
            },
            probe,
        )),
        Err(e) => {
            info!(
                "[{}] {} probe at {}: encoding failed: {e}",
                context.component, context.direction, context.size
            );
            record(context, check, Outcome::NotRun { reason: "encoding" });
            None
        }
    }
}

/// Records the checks of the size `direction` of `component` comes to as not run, for a strict
/// protocol whose swaps cannot be probed: a venue whose quote is priced for another block, or one
/// that cannot be executed at all.
pub fn not_probed(
    strict: &StrictChecks,
    component: &ProtocolComponent,
    token_in: &Token,
    token_out: &Token,
    reason: &str,
) {
    let component_id = component.id.to_string();
    let direction =
        Direction { token_in: token_in.address.clone(), token_out: token_out.address.clone() };
    let label = format!("{} -> {}", token_in.symbol, token_out.symbol);
    let size = strict.next_size(&component_id, &direction);
    let context = ProbeContext {
        protocol: &component.protocol_system,
        component: &component_id,
        direction: &label,
        size,
    };
    let checks: &[CheckId] = match size {
        ProbeSize::Fraction(_) => &[CheckId::ExecLadder],
        ProbeSize::Limit => &CheckId::LIMITS,
    };
    for check in checks {
        record(&context, *check, Outcome::NotRun { reason });
    }
}

/// Judges every executed probe: a fraction or the limit must execute within the package's
/// tolerance of its quote, after the router fee; one unit past a hard limit must revert, given
/// that the limit itself executed.
pub fn judge(
    probes: &HashMap<String, Probe>,
    inputs: &HashMap<String, TychoExecutionInput>,
    results: &HashMap<String, TychoExecutionResult>,
    router_fee: RouterFeeOnOutput,
) {
    for (simulation_id, probe) in probes {
        let Some(input) = inputs.get(simulation_id) else {
            continue;
        };
        let context = ProbeContext {
            protocol: &probe.protocol,
            component: &probe.component,
            direction: &probe.direction,
            size: probe.size,
        };
        let outcome = judged(&context, simulation_id, probe, input, results, router_fee);
        record(&context, probe.check(), outcome);
    }
}

/// The outcome of one executed probe.
fn judged<'a>(
    context: &ProbeContext<'_>,
    simulation_id: &str,
    probe: &Probe,
    input: &TychoExecutionInput,
    results: &HashMap<String, TychoExecutionResult>,
    router_fee: RouterFeeOnOutput,
) -> Outcome<'a> {
    match &probe.role {
        ProbeRole::Fraction | ProbeRole::AtLimit => execution_outcome(
            context,
            input,
            results.get(simulation_id),
            probe.tolerance,
            router_fee,
        ),
        ProbeRole::PastLimit { at_limit, must_revert } => match results.get(at_limit) {
            Some(TychoExecutionResult::Success { .. }) => {
                let missing = if *must_revert {
                    missing_revert(simulation_id, input, results)
                } else {
                    missing_execution(simulation_id, input, results)
                };
                match missing {
                    None => Outcome::Pass,
                    Some(detail) => match results.get(simulation_id) {
                        // Past a hard limit the swap executed; past an under-cap one it reverted.
                        Some(TychoExecutionResult::Success { .. }) => {
                            Outcome::Fail { reason: "executed", detail }
                        }
                        Some(TychoExecutionResult::Revert { .. }) => {
                            Outcome::Fail { reason: "reverted", detail }
                        }
                        Some(TychoExecutionResult::Failed { error_msg }) => {
                            not_executed(context, error_msg)
                        }
                        None => Outcome::NotRun { reason: "no_result" },
                    },
                }
            }
            _ => Outcome::NotRun { reason: "at_limit_not_executed" },
        },
    }
}

/// Whether an execution paid its quote, less the router fee, within `tolerance`.
fn execution_outcome<'a>(
    context: &ProbeContext<'_>,
    input: &TychoExecutionInput,
    result: Option<&TychoExecutionResult>,
    tolerance: Tolerance,
    router_fee: RouterFeeOnOutput,
) -> Outcome<'a> {
    let amount_in = input.solution.amount_in();
    match result {
        Some(TychoExecutionResult::Success { amount_out, .. }) => {
            let expected = router_fee.deduct(&input.expected_amount_out);
            if let Some((_, bps)) = drift(&expected, amount_out) {
                metrics::record_probe_drift(context.protocol, &context.size.to_string(), bps);
            }
            match check_execution_slippage(&expected, amount_out, tolerance) {
                Ok(()) => Outcome::Pass,
                Err(reason) => Outcome::Fail {
                    reason: "mismatch",
                    detail: format!(
                        "{amount_in} {} executed for {amount_out} {}, simulated {expected}: {reason}",
                        input.token_in, input.token_out
                    ),
                },
            }
        }
        Some(TychoExecutionResult::Revert { reason, .. }) => Outcome::Fail {
            reason: "revert",
            detail: format!("{amount_in} {} reverted: {reason}", input.token_in),
        },
        Some(TychoExecutionResult::Failed { error_msg }) => not_executed(context, error_msg),
        None => Outcome::NotRun { reason: "no_result" },
    }
}

/// A probe the node could not execute: the error is the harness's or the node's, such as a token
/// whose balance slot was not found or a block the node has not reached, so it says nothing of the
/// venue.
fn not_executed<'a>(context: &ProbeContext<'_>, error_msg: &str) -> Outcome<'a> {
    info!(
        "[{}] {} probe at {}: not executed: {error_msg}",
        context.component, context.direction, context.size
    );
    let reason = match crate::categorize_error(error_msg) {
        "Storage slot not found" => "storage_slot",
        _ => "rpc",
    };
    Outcome::NotRun { reason }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::U256;
    use tycho_common::Bytes;
    use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

    use super::*;

    const POOL: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[],
            Chain::Ethereum,
            100,
        )
    }

    fn component() -> ProtocolComponent {
        ProtocolComponent::new(
            Bytes::from_str(POOL).unwrap(),
            "uniswap_v2".to_string(),
            "uniswap_v2_pool".to_string(),
            Chain::Ethereum,
            vec![token(USDC, "USDC", 6), token(WETH, "WETH", 18)],
            Vec::new(),
            HashMap::new(),
            Bytes::default(),
            Default::default(),
        )
    }

    fn state() -> UniswapV2State {
        UniswapV2State::new(
            U256::from(30_000_000u64) * U256::from(10u64).pow(U256::from(6)),
            U256::from(10_000u64) * U256::from(10u64).pow(U256::from(18)),
        )
    }

    /// A strict package for `uniswap_v2` whose package-level `limits` are `limits`, or none.
    fn strict(limits: Option<&str>) -> StrictChecks {
        let limits = limits
            .map(|limits| format!("limits: {limits}\n"))
            .unwrap_or_default();
        let yaml = format!(
            "protocol_system: uniswap_v2\nharness: strict\n{limits}\
             substreams_yaml_path: ./substreams.yaml\ntests: []\n"
        );
        let packages = StrictPackages::from_yamls_for_tests(yaml);
        StrictChecks::new(packages)
    }

    /// Probes USDC -> WETH `turns` times, returning the last probe's swaps.
    fn probe(strict: &StrictChecks, turns: usize) -> Vec<(String, TychoExecutionInput, Probe)> {
        let component = component();
        let state = state();
        let (usdc, weth) = (&component.tokens[0], &component.tokens[1]);
        let (max_input, max_output) = state
            .get_limits(usdc.address.clone(), weth.address.clone())
            .unwrap();
        let package = strict.package("uniswap_v2").unwrap();
        let mut swaps = Vec::new();
        for _ in 0..turns {
            swaps = probe_direction(
                strict,
                package,
                &Target {
                    chain: Chain::Ethereum,
                    component: &component,
                    state: &state,
                    token_in: usdc,
                    token_out: weth,
                    max_input: &max_input,
                    max_output: &max_output,
                    simulation_id: "sim".to_string(),
                },
            );
        }
        swaps
    }

    #[test]
    fn each_direction_rotates_through_every_size() {
        let strict = strict(None);
        let direction = Direction {
            token_in: Bytes::from_str(USDC).unwrap(),
            token_out: Bytes::from_str(WETH).unwrap(),
        };
        let sizes: Vec<ProbeSize> = (0..5)
            .map(|turn| {
                // The component's case does not start a new rotation.
                let id = if turn % 2 == 0 { POOL.to_string() } else { POOL.to_lowercase() };
                strict.next_size(&id, &direction)
            })
            .collect();
        assert_eq!(sizes[..4], ProbeSize::ALL);
        assert_eq!(sizes[4], ProbeSize::ALL[0]);
        // Another direction has its own turn.
        let reverse = Direction { token_in: direction.token_out, token_out: direction.token_in };
        assert_eq!(strict.next_size(POOL, &reverse), ProbeSize::ALL[0]);
    }

    #[test]
    fn a_fraction_is_swapped_at_its_share_of_the_limit() {
        let strict = strict(None);
        let swaps = probe(&strict, 1);
        let [(id, input, probe)] = swaps.as_slice() else { panic!("one swap: {swaps:?}") };
        assert_eq!(probe.role, ProbeRole::Fraction);
        assert_eq!(probe.size, ProbeSize::Fraction(1));
        assert!(id.ends_with("-probe-fraction"), "{id}");
        let (max_input, _) = state()
            .get_limits(Bytes::from_str(USDC).unwrap(), Bytes::from_str(WETH).unwrap())
            .unwrap();
        assert_eq!(input.solution.amount_in(), &(max_input / 1000u32));
    }

    #[test]
    fn the_limit_is_not_probed_without_a_declaration() {
        // The fourth turn is the limit.
        assert!(probe(&strict(None), 4).is_empty());
    }

    #[test]
    fn a_soft_limit_is_swapped_at_the_limit_only() {
        let swaps = probe(&strict(Some("{kind: soft, reason: \"constant product\"}")), 4);
        let roles: Vec<&ProbeRole> = swaps
            .iter()
            .map(|(_, _, probe)| &probe.role)
            .collect();
        assert_eq!(roles, vec![&ProbeRole::AtLimit]);
    }

    #[test]
    fn a_hard_limit_the_venue_quotes_past_is_not_swapped_past() {
        // Uniswap V2 quotes any input, so the quote past a "hard" limit already fails the check,
        // and no swap past the limit is queued.
        let swaps = probe(&strict(Some("{kind: hard}")), 4);
        let roles: Vec<&ProbeRole> = swaps
            .iter()
            .map(|(_, _, probe)| &probe.role)
            .collect();
        assert_eq!(roles, vec![&ProbeRole::AtLimit]);
    }

    fn fee() -> RouterFeeOnOutput {
        RouterFeeOnOutput::new_for_tests(10, 10_000)
    }

    fn success(amount_out: BigUint) -> TychoExecutionResult {
        TychoExecutionResult::Success {
            amount_out,
            gas_used: 0,
            state_overwrites: None,
            overwrite_metadata: None,
        }
    }

    fn revert() -> TychoExecutionResult {
        TychoExecutionResult::Revert {
            reason: "past the bound".to_string(),
            state_overwrites: None,
            overwrite_metadata: None,
        }
    }

    /// The outcome label and reason of judging `probe` against `results`.
    fn judge_one(
        probe: &Probe,
        input: &TychoExecutionInput,
        id: &str,
        results: &HashMap<String, TychoExecutionResult>,
    ) -> (&'static str, String) {
        let context = ProbeContext {
            protocol: &probe.protocol,
            component: &probe.component,
            direction: &probe.direction,
            size: probe.size,
        };
        match judged(&context, id, probe, input, results, fee()) {
            Outcome::Pass => ("pass", String::new()),
            Outcome::Fail { reason, .. } => ("fail", reason.to_string()),
            Outcome::NotRun { reason } => ("not_run", reason.to_string()),
        }
    }

    #[test]
    fn an_execution_matches_its_quote_after_the_router_fee() {
        let swaps = probe(&strict(None), 1);
        let (id, input, probe) = &swaps[0];
        let paid = fee().deduct(&input.expected_amount_out);
        let results = HashMap::from([(id.clone(), success(paid.clone()))]);
        assert_eq!(judge_one(probe, input, id, &results), ("pass", String::new()));
        // Strict mode without a declared tolerance is exact: one wei short fails.
        let results = HashMap::from([(id.clone(), success(paid - 1u32))]);
        assert_eq!(judge_one(probe, input, id, &results), ("fail", "mismatch".to_string()));
        let results = HashMap::from([(id.clone(), revert())]);
        assert_eq!(judge_one(probe, input, id, &results), ("fail", "revert".to_string()));
        assert_eq!(
            judge_one(probe, input, id, &HashMap::new()),
            ("not_run", "no_result".to_string())
        );
    }

    fn failed(error_msg: &str) -> TychoExecutionResult {
        TychoExecutionResult::Failed { error_msg: error_msg.to_string() }
    }

    #[test]
    fn an_execution_the_node_could_not_run_is_not_run() {
        let swaps = probe(&strict(None), 1);
        let (id, input, probe) = &swaps[0];
        let judge = |result| judge_one(probe, input, id, &HashMap::from([(id.clone(), result)]));
        assert_eq!(
            judge(failed(
                "Couldn't find storage slots for token 0xc5fb36dd2fb59d3b98deff88425a3f425ee469ed"
            )),
            ("not_run", "storage_slot".to_string())
        );
        assert_eq!(
            judge(failed("Transaction failed: server returned an error response: error code -32000: block 0x1929fcc not found")),
            ("not_run", "rpc".to_string())
        );
    }

    #[test]
    fn a_swap_past_the_limit_must_revert_once_the_limit_executed() {
        let swaps = probe(&strict(None), 1);
        let (_, input, fraction) = &swaps[0];
        let past = Probe {
            role: ProbeRole::PastLimit { at_limit: "at".to_string(), must_revert: true },
            ..fraction.clone()
        };
        let judge = |at: Option<TychoExecutionResult>, past_result: TychoExecutionResult| {
            let mut results = HashMap::from([("past".to_string(), past_result)]);
            results.extend(at.map(|at| ("at".to_string(), at)));
            judge_one(&past, input, "past", &results)
        };
        let executed = || Some(success(BigUint::from(1u32)));
        assert_eq!(judge(executed(), revert()), ("pass", String::new()));
        assert_eq!(
            judge(executed(), success(BigUint::from(1u32))),
            ("fail", "executed".to_string())
        );
        assert_eq!(
            judge(Some(revert()), revert()),
            ("not_run", "at_limit_not_executed".to_string())
        );
        assert_eq!(
            judge(executed(), failed("block 0x1929fcc not found")),
            ("not_run", "rpc".to_string())
        );
    }

    #[test]
    fn a_swap_past_an_under_cap_limit_must_execute_once_the_limit_executed() {
        let swaps = probe(&strict(None), 1);
        let (_, input, fraction) = &swaps[0];
        let past = Probe {
            role: ProbeRole::PastLimit { at_limit: "at".to_string(), must_revert: false },
            ..fraction.clone()
        };
        let judge = |past_result: TychoExecutionResult| {
            let results = HashMap::from([
                ("past".to_string(), past_result),
                ("at".to_string(), success(BigUint::from(1u32))),
            ]);
            judge_one(&past, input, "past", &results)
        };
        assert_eq!(judge(success(BigUint::from(1u32))), ("pass", String::new()));
        assert_eq!(judge(revert()), ("fail", "reverted".to_string()));
    }

    #[test]
    fn an_under_cap_limit_is_swapped_at_and_one_unit_past_the_limit() {
        let swaps = probe(&strict(Some("{kind: under_cap, reason: \"r; gives up 1%\"}")), 4);
        let roles: Vec<&ProbeRole> = swaps
            .iter()
            .map(|(_, _, probe)| &probe.role)
            .collect();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0], &ProbeRole::AtLimit);
        assert!(matches!(roles[1], ProbeRole::PastLimit { must_revert: false, .. }));
    }
}
