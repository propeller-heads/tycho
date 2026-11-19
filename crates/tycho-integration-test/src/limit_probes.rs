//! Limit probes: for a protocol whose package is in strict mode, each sampled direction is also
//! swapped at one size relative to its limit, in rotation, and judged with the strict checks.
//!
//! Every protocol gets the realistic swap `process_state` makes, with its own metrics; a probe
//! only feeds `tycho_integration_check_total` and `tycho_integration_probe_drift_bps`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use num_bigint::BigUint;
use num_traits::Zero;
use strum_macros::IntoStaticStr;
use tracing::{error, info};
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
};
use tycho_simulation::protocol::models::ProtocolComponent;
use tycho_test::{
    checks::{
        execution::{check_execution_slippage, drift, past_limit_mismatch},
        ladder::{executable_max_input, ExecutableLimit, ProbeSize},
        limits::{
            check_direction, past_limit_in_simulation, LimitSizes, PastLimitClaim,
            PastLimitInSimulation, Quote,
        },
        outcome::CheckId,
    },
    execution::{
        encoding::encode_swap_without_surplus_capture,
        models::{TychoExecutionInput, TychoExecutionResult},
    },
    expectations::{Direction, PackageExpectations, Tolerance},
};

use crate::{fee_fetcher::RouterFeeOnOutput, metrics, packages::StrictPackages, ErrorCategory};

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

    /// Every strict package, with its protocol system.
    pub fn packages(&self) -> impl Iterator<Item = (&str, &PackageExpectations)> {
        self.packages.iter()
    }

    /// The size to probe `direction` of `component` at this time: each of [`ProbeSize::ALL`] in
    /// turn, so every size comes up within four samples of a direction.
    fn next_size(&self, component: &str, direction: &Direction) -> ProbeSize {
        let mut rotation = lock(&self.rotation);
        let turn = rotation
            .entry((component.to_lowercase(), direction.clone()))
            .or_default();
        let size = ProbeSize::ALL[*turn % ProbeSize::ALL.len()];
        *turn += 1;
        size
    }

    /// The next turn of the direction `token_in -> token_out` of `component`, planning the checks
    /// of the size it comes to.
    fn turn(
        &self,
        component: &ProtocolComponent,
        token_in: &Token,
        token_out: &Token,
    ) -> Arc<Turn> {
        let component_id = component.id.to_string();
        let direction =
            Direction { token_in: token_in.address.clone(), token_out: token_out.address.clone() };
        let size = self.next_size(&component_id, &direction);
        let planned = match size {
            ProbeSize::Fraction(_) => vec![CheckId::SimLadder, CheckId::ExecLadder],
            ProbeSize::Limit => CheckId::LIMITS.to_vec(),
        };
        Arc::new(Turn {
            protocol: component.protocol_system.clone(),
            component: component_id,
            direction,
            label: format!("{} -> {}", token_in.symbol, token_out.symbol),
            size,
            pending: Mutex::new(planned),
            unsettled: Mutex::new(NotRun::Unrecorded),
        })
    }
}

/// One turn of one direction: the checks its size is for, each recorded exactly once.
///
/// The probe's swaps hold the turn until they are judged. A check still unsettled when the turn
/// drops, the swaps judged or dropped, is recorded as not run, with the reason [`Turn::abandon`]
/// gave, or `unrecorded`: an exit that forgot a check shows on the dashboard instead of going
/// missing.
#[derive(Debug)]
pub struct Turn {
    protocol: String,
    component: String,
    direction: Direction,
    /// `IN -> OUT`, by token symbol, for the logs.
    label: String,
    size: ProbeSize,
    pending: Mutex<Vec<CheckId>>,
    unsettled: Mutex<NotRun>,
}

impl Turn {
    /// Records `outcome` for `check`, which must be one of the turn's unsettled checks.
    fn settle(&self, check: CheckId, outcome: Outcome) {
        let mut pending = lock(&self.pending);
        let Some(position) = pending
            .iter()
            .position(|planned| *planned == check)
        else {
            error!(
                protocol = self.protocol,
                component = self.component,
                direction = self.label,
                size = %self.size,
                check = check.as_str(),
                discarded = ?outcome,
                "A probe's check is recorded twice, or was not planned: this outcome is discarded"
            );
            return;
        };
        pending.remove(position);
        drop(pending);
        self.record(check, outcome);
    }

    /// Settles every unsettled check as not run, for `reason`.
    fn not_run(&self, reason: NotRun) {
        let pending: Vec<CheckId> = std::mem::take(&mut *lock(&self.pending));
        if reason == NotRun::Unrecorded && !pending.is_empty() {
            // The metric has no component or check to point at the bug, so the log names them.
            error!(
                protocol = self.protocol,
                component = self.component,
                direction = self.label,
                size = %self.size,
                checks = ?pending,
                "A probe's checks were dropped with no outcome recorded: a bug in the probes, or \
                 a panic while they were pending"
            );
        }
        for check in pending {
            self.record(check, Outcome::NotRun(reason));
        }
    }

    /// The reason the checks unsettled when the turn drops did not run.
    pub fn abandon(&self, reason: NotRun) {
        *lock(&self.unsettled) = reason;
    }

    /// Records one check outcome, and logs a failure with the component and the detail, which
    /// the metric leaves out.
    fn record(&self, check: CheckId, outcome: Outcome) {
        let (label, reason) = match outcome {
            Outcome::Pass => ("pass", ""),
            Outcome::Fail(reason, detail) => {
                error!(
                    event_type = "check_failure",
                    protocol = self.protocol,
                    component = self.component,
                    direction = self.label,
                    size = %self.size,
                    check = check.as_str(),
                    reason = reason.as_str(),
                    "{detail}"
                );
                ("fail", reason.as_str())
            }
            Outcome::NotRun(reason) => ("not_run", reason.as_str()),
        };
        metrics::record_check(
            &self.protocol,
            check.as_str(),
            label,
            reason,
            &self.size.to_string(),
        );
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let reason = *lock(&self.unsettled);
        self.not_run(reason);
    }
}

/// `mutex`'s value, whether or not a thread panicked holding it.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Which strict check a probe's execution answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeRole {
    /// A fraction of the limit: the execution matches the simulation within the tolerance.
    Fraction,
    /// The limit itself: the execution matches the quote at the limit.
    AtLimit,
    /// One unit past the limit: the execution does what `claim` says, reverting past a hard limit
    /// and executing past an under-cap one. `at_limit` is the simulation id of the swap at
    /// the limit in the same batch, without which what happens past it proves nothing.
    PastLimit { at_limit: String, claim: PastLimitClaim },
}

/// A probe swap, queued with the block's other executions.
#[derive(Debug, Clone)]
pub struct Probe {
    pub turn: Arc<Turn>,
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

/// Every probe of `probes` that was not judged records its unsettled checks as not run for
/// `reason`, once dropped.
pub fn abandon(probes: &HashMap<String, Probe>, reason: NotRun) {
    for probe in probes.values() {
        probe.turn.abandon(reason);
    }
}

/// A batch of the update failed, after the batches before it returned `results`: their probes
/// are judged, and every other probe's unsettled checks are recorded as not run
/// (`batch_failed`). A probe's swaps at and past the limit are in one batch, since the batch
/// depends only on the protocol.
pub fn judge_after_failed_batch(
    probes: &HashMap<String, Probe>,
    inputs: &HashMap<String, TychoExecutionInput>,
    results: &HashMap<String, TychoExecutionResult>,
    router_fee: RouterFeeOnOutput,
) {
    let ran: HashMap<String, Probe> = probes
        .iter()
        .filter(|(simulation_id, _)| results.contains_key(*simulation_id))
        .map(|(simulation_id, probe)| (simulation_id.clone(), probe.clone()))
        .collect();
    judge(&ran, inputs, results, router_fee);
    abandon(probes, NotRun::BatchFailed);
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

/// The outcome of one check.
#[derive(Debug)]
enum Outcome {
    Pass,
    /// Why it failed, for the metric, and the detail, for the log.
    Fail(Fail, String),
    NotRun(NotRun),
}

/// Why a probe's check failed: the metric's `reason` label for `outcome="fail"`, the variant's
/// name in snake case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Fail {
    /// `sim.ladder`: the quote at a fraction of the limit errors.
    QuoteError,
    /// A limit check the simulation alone fails, before any execution.
    LimitQuote,
    /// An execution pays other than its quote, less the router fee, beyond the tolerance.
    Mismatch,
    /// A fraction or the limit reverts.
    Revert,
    /// One unit past a hard limit, the swap executes.
    PastLimitExecuted,
    /// Past an under-cap limit, the swap reverts.
    PastLimitReverted,
}

impl Fail {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Why a probe's check did not run: the metric's `reason` label for `outcome="not_run"`, the
/// variant's name in snake case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum NotRun {
    /// A price level venue: its quote prices a block not built yet.
    BookVenue,
    /// A next-block protocol: its quote moves with the block it executes in.
    NextBlock,
    /// CoWAMM, which is not executed.
    NotExecutable,
    /// The package declares no limit for the direction.
    NoDeclaration,
    /// `get_limits` reports a zero max input.
    ZeroLimit,
    /// The fraction of the limit rounds to zero.
    ZeroAmount,
    /// The limit is at or past `int128::MAX`, the execution cap, so what is past it reverts in
    /// the executor, not at the venue's bound.
    ExecutionCap,
    /// The quote at a fraction of the limit failed, so the swap was not made.
    QuoteFailed,
    /// The quote at the limit failed, so nothing at or past it is checked.
    AtLimitFailed,
    /// The swap at the limit did not execute, so what happens past it proves nothing.
    AtLimitNotExecuted,
    /// The swap could not be encoded.
    Encoding,
    /// The token's balance slot was not found, so the swap could not be simulated.
    StorageSlot,
    /// The node failed the call, such as for a block it has not reached.
    Rpc,
    /// The router reverted before the swap reached the venue, as when it is paused
    /// (`EnforcedPause()`): the revert is the router's, not the venue's.
    Router,
    /// The harness could not build or read the swap's simulation.
    Harness,
    /// The batch of simulations the swap was in failed, or an earlier batch of the update failed,
    /// so the swap's batch did not run.
    BatchFailed,
    /// The run executes nothing (`--disable-execution`).
    ExecutionDisabled,
    /// The swap has no result in its batch.
    NoResult,
    /// No outcome was recorded: a bug in the probes.
    Unrecorded,
}

impl NotRun {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Probes one direction of a strict package's component at the size its turn comes to.
///
/// Records the outcomes the simulation alone decides (a quote that errors, the max output, a
/// direction that cannot be probed), and returns the swaps to execute, by simulation id, for
/// [`judge`] once they ran. The swaps carry the turn, which records whatever is left unsettled
/// when they drop.
pub fn probe_direction(
    strict: &StrictChecks,
    package: &PackageExpectations,
    target: &Target<'_>,
) -> Vec<(String, TychoExecutionInput, Probe)> {
    let Target { component, state, token_in, token_out, max_input, max_output, .. } = target;
    let turn = strict.turn(component, token_in, token_out);

    let ExecutableLimit { amount: executable, capped } = executable_max_input(max_input);
    if executable.is_zero() {
        turn.not_run(NotRun::ZeroLimit);
        return Vec::new();
    }
    let tolerance = package.tolerance(&turn.component, &token_out.address);
    let probe = |role| Probe { turn: turn.clone(), role, tolerance };
    let quote = |amount: BigUint| {
        state
            .get_amount_out(amount, token_in, token_out)
            .map(|quote| Quote { amount: quote.amount, gas: quote.gas })
            .map_err(|e| e.to_string())
    };

    match turn.size {
        ProbeSize::Fraction(_) => {
            let amount = turn.size.amount(&executable);
            if amount.is_zero() {
                turn.not_run(NotRun::ZeroAmount);
                return Vec::new();
            }
            let quoted = match quote(amount.clone()) {
                Ok(quoted) => quoted,
                Err(e) => {
                    let detail = format!("quoting {amount} {} failed: {e}", token_in.symbol);
                    turn.settle(CheckId::SimLadder, Outcome::Fail(Fail::QuoteError, detail));
                    turn.not_run(NotRun::QuoteFailed);
                    return Vec::new();
                }
            };
            turn.settle(CheckId::SimLadder, Outcome::Pass);
            encoded(target, "fraction", amount, &quoted, probe(ProbeRole::Fraction))
                .into_iter()
                .collect()
        }
        ProbeSize::Limit => {
            let Some(limits) = package.limits_for(&turn.component, &turn.direction) else {
                turn.not_run(NotRun::NoDeclaration);
                return Vec::new();
            };
            let checked = check_direction(
                quote,
                &turn.label,
                &LimitSizes { max_input: &executable, max_output, capped },
                limits,
            );
            for (check, detail) in &checked.failures {
                turn.settle(*check, Outcome::Fail(Fail::LimitQuote, detail.clone()));
            }
            let failed = |check| {
                checked
                    .failures
                    .iter()
                    .any(|(failed, _)| *failed == check)
            };
            let in_simulation =
                past_limit_in_simulation(checked.at_limit_quote.is_some(), capped, limits);
            let Some(at_quote) = &checked.at_limit_quote else {
                // Nothing at or past the limit can be checked without its quote.
                turn.not_run(NotRun::AtLimitFailed);
                return Vec::new();
            };
            if !failed(CheckId::LimitsMaxOutput) {
                turn.settle(CheckId::LimitsMaxOutput, Outcome::Pass);
            }
            if !failed(CheckId::LimitsPastLimit) {
                match in_simulation {
                    PastLimitInSimulation::Capped => {
                        turn.settle(CheckId::LimitsPastLimit, Outcome::NotRun(NotRun::ExecutionCap))
                    }
                    PastLimitInSimulation::Passed => {
                        turn.settle(CheckId::LimitsPastLimit, Outcome::Pass)
                    }
                    // Judged once the swap past the limit executed; the quote at the limit
                    // succeeded, so it was not `AtLimitFailed`.
                    PastLimitInSimulation::NeedsExecution |
                    PastLimitInSimulation::AtLimitFailed => {}
                }
            }

            let Some(at_limit) =
                encoded(target, "at_limit", executable, at_quote, probe(ProbeRole::AtLimit))
            else {
                // The swap past the limit proves nothing without the swap at it.
                turn.not_run(NotRun::Encoding);
                return Vec::new();
            };
            let mut swaps = vec![at_limit];
            if let Some(past) = checked.past_limit_swap {
                let role = ProbeRole::PastLimit { at_limit: swaps[0].0.clone(), claim: past.claim };
                // Its minimum output is 80% of the quote at the limit, which a larger input easily
                // meets: past a hard limit a venue that accepts the input executes the swap, and
                // only a revert passes.
                swaps.extend(encoded(target, "past_limit", past.amount_in, at_quote, probe(role)));
            }
            swaps
        }
    }
}

/// The swap of `amount` at `quote`, encoded for the production router, with its simulation id.
/// An encoding error settles the probe's check as not run, since it is not the venue's.
fn encoded(
    target: &Target<'_>,
    kind: &str,
    amount: BigUint,
    quote: &Quote,
    probe: Probe,
) -> Option<(String, TychoExecutionInput, Probe)> {
    // Without surplus capture, so a venue that pays more than the quote is seen to.
    match encode_swap_without_surplus_capture(
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
            let turn = &probe.turn;
            info!(
                component = turn.component,
                direction = turn.label,
                size = %turn.size,
                "A probe could not be encoded: {e}"
            );
            turn.settle(probe.check(), Outcome::NotRun(NotRun::Encoding));
            None
        }
    }
}

/// Records the checks of the size `direction` of `component` comes to as not run, for a strict
/// protocol whose swaps cannot be probed.
pub fn not_probed(
    strict: &StrictChecks,
    component: &ProtocolComponent,
    token_in: &Token,
    token_out: &Token,
    reason: NotRun,
) {
    strict
        .turn(component, token_in, token_out)
        .not_run(reason);
}

/// Judges every executed probe: a fraction or the limit must execute within its tolerance of its
/// quote, after the router fee; past a hard limit the swap must revert, and past an under-cap one
/// execute, given that the limit itself executed.
pub fn judge(
    probes: &HashMap<String, Probe>,
    inputs: &HashMap<String, TychoExecutionInput>,
    results: &HashMap<String, TychoExecutionResult>,
    router_fee: RouterFeeOnOutput,
) {
    for (simulation_id, probe) in probes {
        let outcome = match inputs.get(simulation_id) {
            Some(input) => judged(simulation_id, probe, input, results, router_fee),
            None => Outcome::NotRun(NotRun::NoResult),
        };
        probe
            .turn
            .settle(probe.check(), outcome);
    }
}

/// The outcome of one executed probe.
fn judged(
    simulation_id: &str,
    probe: &Probe,
    input: &TychoExecutionInput,
    results: &HashMap<String, TychoExecutionResult>,
    router_fee: RouterFeeOnOutput,
) -> Outcome {
    let turn = &probe.turn;
    let result = results.get(simulation_id);
    match &probe.role {
        ProbeRole::Fraction | ProbeRole::AtLimit => {
            execution_outcome(turn, input, result, probe.tolerance, router_fee)
        }
        ProbeRole::PastLimit { at_limit, claim } => {
            if !matches!(results.get(at_limit), Some(TychoExecutionResult::Success { .. })) {
                return Outcome::NotRun(NotRun::AtLimitNotExecuted);
            }
            match result {
                None => Outcome::NotRun(NotRun::NoResult),
                Some(TychoExecutionResult::Failed { error_msg }) => not_executed(turn, error_msg),
                Some(TychoExecutionResult::Revert { reason, .. }) if router_reverted(reason) => {
                    Outcome::NotRun(NotRun::Router)
                }
                Some(
                    TychoExecutionResult::Success { .. } | TychoExecutionResult::Revert { .. },
                ) => {
                    match past_limit_mismatch(simulation_id, input, results, *claim) {
                        None => Outcome::Pass,
                        // The claim says which way it went wrong: past a hard limit the swap
                        // executed, past an under-cap one it reverted.
                        Some(detail) => Outcome::Fail(
                            match claim {
                                PastLimitClaim::Reverts => Fail::PastLimitExecuted,
                                PastLimitClaim::Executes => Fail::PastLimitReverted,
                            },
                            detail,
                        ),
                    }
                }
            }
        }
    }
}

/// Whether an execution paid its quote, less the router fee, within `tolerance`.
fn execution_outcome(
    turn: &Turn,
    input: &TychoExecutionInput,
    result: Option<&TychoExecutionResult>,
    tolerance: Tolerance,
    router_fee: RouterFeeOnOutput,
) -> Outcome {
    let amount_in = input.solution.amount_in();
    match result {
        Some(TychoExecutionResult::Success { amount_out, .. }) => {
            let expected = router_fee.deduct(&input.expected_amount_out);
            if let Some((_, bps)) = drift(&expected, amount_out) {
                metrics::record_probe_drift(&turn.protocol, &turn.size.to_string(), bps);
            }
            match check_execution_slippage(&expected, amount_out, tolerance) {
                Ok(()) => Outcome::Pass,
                Err(reason) => Outcome::Fail(
                    Fail::Mismatch,
                    format!(
                        "{amount_in} {} executed for {amount_out} {}, simulated {expected}: {reason}",
                        input.token_in, input.token_out
                    ),
                ),
            }
        }
        Some(TychoExecutionResult::Revert { reason, .. }) if router_reverted(reason) => {
            Outcome::NotRun(NotRun::Router)
        }
        Some(TychoExecutionResult::Revert { reason, .. }) => Outcome::Fail(
            Fail::Revert,
            format!("{amount_in} {} reverted: {reason}", input.token_in),
        ),
        Some(TychoExecutionResult::Failed { error_msg }) => not_executed(turn, error_msg),
        None => Outcome::NotRun(NotRun::NoResult),
    }
}

/// Whether a revert is the router's own, raised before the swap reached the venue, so it says
/// nothing of the venue.
fn router_reverted(reason: &str) -> bool {
    crate::categorize_error(reason) == ErrorCategory::RouterPaused
}

/// A probe that was not executed: the error is the harness's or the node's, so it says nothing of
/// the venue.
fn not_executed(turn: &Turn, error_msg: &str) -> Outcome {
    info!(
        component = turn.component,
        direction = turn.label,
        size = %turn.size,
        "A probe was not executed: {error_msg}"
    );
    Outcome::NotRun(match crate::categorize_error(error_msg) {
        ErrorCategory::StorageSlot => NotRun::StorageSlot,
        ErrorCategory::Node => NotRun::Rpc,
        ErrorCategory::RouterPaused => NotRun::Router,
        ErrorCategory::NegativeSlippage | ErrorCategory::FeeToken | ErrorCategory::Other => {
            NotRun::Harness
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, str::FromStr};

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

    /// A strict package for `uniswap_v2` whose package-level `limits` are `limits`, or none, with
    /// `extra` yaml after them.
    fn strict_with(limits: Option<&str>, extra: &str) -> StrictChecks {
        let limits = limits
            .map(|limits| format!("limits: {limits}\n"))
            .unwrap_or_default();
        let yaml = format!(
            "protocol_system: uniswap_v2\nharness: strict\n{limits}\
             substreams_yaml_path: ./substreams.yaml\n{extra}"
        );
        install_check_log();
        StrictChecks::new(StrictPackages::from_yamls_for_tests(yaml))
    }

    fn strict(limits: Option<&str>) -> StrictChecks {
        strict_with(limits, "tests: []\n")
    }

    /// Probes USDC -> WETH of `state` `turns` times, returning the last probe's swaps.
    fn probe_state(
        strict: &StrictChecks,
        pool: &UniswapV2State,
        max_input: &BigUint,
        turns: usize,
    ) -> Vec<(String, TychoExecutionInput, Probe)> {
        probe_component(strict, &component(), pool, max_input, turns)
    }

    /// Probes USDC -> WETH of `component`, priced by `pool`, `turns` times, returning the last
    /// probe's swaps.
    fn probe_component(
        strict: &StrictChecks,
        component: &ProtocolComponent,
        pool: &UniswapV2State,
        max_input: &BigUint,
        turns: usize,
    ) -> Vec<(String, TychoExecutionInput, Probe)> {
        let (usdc, weth) = (&component.tokens[0], &component.tokens[1]);
        let (_, max_output) = state()
            .get_limits(usdc.address.clone(), weth.address.clone())
            .unwrap();
        let package = strict.package("uniswap_v2").unwrap();
        let mut swaps = Vec::new();
        for _ in 0..turns {
            // The swaps of the turn before are dropped here, which settles what they left.
            swaps = probe_direction(
                strict,
                package,
                &Target {
                    chain: Chain::Ethereum,
                    component,
                    state: pool,
                    token_in: usdc,
                    token_out: weth,
                    max_input,
                    max_output: &max_output,
                    simulation_id: "sim".to_string(),
                },
            );
        }
        swaps
    }

    /// Probes USDC -> WETH `turns` times, returning the last probe's swaps.
    fn probe(strict: &StrictChecks, turns: usize) -> Vec<(String, TychoExecutionInput, Probe)> {
        let (max_input, _) = state()
            .get_limits(Bytes::from_str(USDC).unwrap(), Bytes::from_str(WETH).unwrap())
            .unwrap();
        probe_state(strict, &state(), &max_input, turns)
    }

    /// A check outcome as the metric records it: the check, `pass`, `fail` or `not_run`, and the
    /// reason.
    type Recorded = (String, String, String);

    thread_local! {
        /// The check outcomes this thread recorded, in order.
        static RECORDED: RefCell<Vec<Recorded>> = const { RefCell::new(Vec::new()) };
        /// Keeps [`CheckLog`] this thread's recorder.
        static INSTALLED: RefCell<Option<::metrics::LocalRecorderGuard<'static>>> =
            const { RefCell::new(None) };
    }

    /// A recorder that keeps each increment of `tycho_integration_check_total` in this thread's
    /// [`RECORDED`], and drops every other metric.
    struct CheckLog;

    static CHECK_LOG: CheckLog = CheckLog;

    /// The counter of one `(check, outcome, reason)`: each increment is one recorded outcome.
    struct CheckCounter(Recorded);

    impl ::metrics::CounterFn for CheckCounter {
        fn increment(&self, value: u64) {
            for _ in 0..value {
                RECORDED.with(|recorded| {
                    recorded
                        .borrow_mut()
                        .push(self.0.clone())
                });
            }
        }

        fn absolute(&self, _value: u64) {}
    }

    impl ::metrics::Recorder for CheckLog {
        fn describe_counter(
            &self,
            _: ::metrics::KeyName,
            _: Option<::metrics::Unit>,
            _: ::metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: ::metrics::KeyName,
            _: Option<::metrics::Unit>,
            _: ::metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: ::metrics::KeyName,
            _: Option<::metrics::Unit>,
            _: ::metrics::SharedString,
        ) {
        }

        fn register_counter(
            &self,
            key: &::metrics::Key,
            _: &::metrics::Metadata<'_>,
        ) -> ::metrics::Counter {
            if key.name() != "tycho_integration_check_total" {
                return ::metrics::Counter::noop();
            }
            let label = |name: &str| {
                key.labels()
                    .find(|label| label.key() == name)
                    .map(|label| label.value().to_string())
                    .unwrap_or_default()
            };
            ::metrics::Counter::from_arc(Arc::new(CheckCounter((
                label("check"),
                label("outcome"),
                label("reason"),
            ))))
        }

        fn register_gauge(
            &self,
            _: &::metrics::Key,
            _: &::metrics::Metadata<'_>,
        ) -> ::metrics::Gauge {
            ::metrics::Gauge::noop()
        }

        fn register_histogram(
            &self,
            _: &::metrics::Key,
            _: &::metrics::Metadata<'_>,
        ) -> ::metrics::Histogram {
            ::metrics::Histogram::noop()
        }
    }

    /// Makes [`CheckLog`] this thread's recorder, once.
    fn install_check_log() {
        INSTALLED.with(|installed| {
            installed
                .borrow_mut()
                .get_or_insert_with(|| ::metrics::set_default_local_recorder(&CHECK_LOG));
        });
    }

    /// The check outcomes recorded since the last call.
    fn recorded() -> Vec<Recorded> {
        RECORDED.with(|recorded| std::mem::take(&mut *recorded.borrow_mut()))
    }

    /// The outcomes of the last of `turns` probes, its swaps judged against `results` by kind
    /// (`fraction`, `at_limit`, `past_limit`), in the order recorded.
    fn turn_outcomes(
        strict: &StrictChecks,
        turns: usize,
        results: impl Fn(&str, &TychoExecutionInput) -> Option<TychoExecutionResult>,
    ) -> Vec<Recorded> {
        let swaps = probe(strict, turns - 1);
        drop(swaps);
        recorded();
        let swaps = probe(strict, 1);
        let mut inputs = HashMap::new();
        let mut probes = HashMap::new();
        let mut executed = HashMap::new();
        for (id, input, probe) in swaps {
            let kind = id
                .rsplit("-probe-")
                .next()
                .unwrap()
                .to_string();
            if let Some(result) = results(&kind, &input) {
                executed.insert(id.clone(), result);
            }
            inputs.insert(id.clone(), input);
            probes.insert(id, probe);
        }
        judge(&probes, &inputs, &executed, fee());
        drop(probes);
        recorded()
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
        assert_eq!(probe.turn.size, ProbeSize::Fraction(1));
        assert!(id.ends_with("-probe-fraction"), "{id}");
        let (max_input, _) = state()
            .get_limits(Bytes::from_str(USDC).unwrap(), Bytes::from_str(WETH).unwrap())
            .unwrap();
        assert_eq!(input.solution.amount_in(), &(max_input / 1000u32));
        // The router takes no output above its `expectedAmountOut` as surplus, so the probe pays
        // the receiver all the venue pays; the comparison is still against the quote.
        let no_capture = BigUint::from_bytes_be(&alloy::primitives::U256::MAX.to_be_bytes::<32>());
        assert_eq!(input.solution.expected_amount_out(), &no_capture);
        assert_eq!(
            input.solution.min_amount_out(),
            &(&input.expected_amount_out * 8000u32 / 10_000u32)
        );
        assert!(input.expected_amount_out < no_capture);
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

    /// The deployed router's revert while it is paused.
    fn paused() -> TychoExecutionResult {
        TychoExecutionResult::Revert {
            reason: "EnforcedPause()".to_string(),
            state_overwrites: None,
            overwrite_metadata: None,
        }
    }

    fn failed(error_msg: &str) -> TychoExecutionResult {
        TychoExecutionResult::Failed { error_msg: error_msg.to_string() }
    }

    /// What the router pays for `input` when the venue pays its quote.
    fn paid(input: &TychoExecutionInput) -> TychoExecutionResult {
        success(fee().deduct(&input.expected_amount_out))
    }

    const PASS: (&str, &str) = ("pass", "");

    fn not_run(reason: NotRun) -> (&'static str, &'static str) {
        ("not_run", reason.as_str())
    }

    fn fail(reason: Fail) -> (&'static str, &'static str) {
        ("fail", reason.as_str())
    }

    /// `(check, outcome, reason)` for each of `checks`.
    fn one(check: CheckId, (outcome, reason): (&str, &str)) -> Recorded {
        (check.as_str().to_string(), outcome.to_string(), reason.to_string())
    }

    fn each(checks: &[(CheckId, (&'static str, &'static str))]) -> Vec<Recorded> {
        checks
            .iter()
            .map(|(check, outcome)| one(*check, *outcome))
            .collect()
    }

    #[test]
    fn a_fraction_records_its_quote_and_its_execution() {
        let strict = strict(None);
        assert_eq!(
            turn_outcomes(&strict, 1, |_, input| Some(paid(input))),
            each(&[(CheckId::SimLadder, PASS), (CheckId::ExecLadder, PASS)])
        );
        // The second size is 1%.
        let one_short = |_: &str, input: &TychoExecutionInput| {
            Some(success(fee().deduct(&input.expected_amount_out) - 1u32))
        };
        assert_eq!(
            turn_outcomes(&strict, 1, one_short)[1],
            one(CheckId::ExecLadder, fail(Fail::Mismatch))
        );
    }

    #[test]
    fn an_execution_matches_its_quote_after_the_router_fee() {
        for (result, outcome) in [
            (Some(revert()), fail(Fail::Revert)),
            // A paused router reverts before the venue is reached.
            (Some(paused()), not_run(NotRun::Router)),
            (None, not_run(NotRun::NoResult)),
            (
                Some(failed(
                    "Couldn't find storage slots for token 0xc5fb36dd2fb59d3b98deff88425a3f425ee469ed",
                )),
                not_run(NotRun::StorageSlot),
            ),
            (
                Some(failed("Transaction failed: server returned an error response: error code -32000: block 0x1929fcc not found")),
                not_run(NotRun::Rpc),
            ),
            (Some(failed("Failed to create swap request: no route")), not_run(NotRun::Harness)),
        ] {
            let strict = strict(None);
            assert_eq!(
                turn_outcomes(&strict, 1, |_, _| result.clone()),
                each(&[(CheckId::SimLadder, PASS), (CheckId::ExecLadder, outcome)])
            );
        }
    }

    #[test]
    fn a_component_tolerance_from_the_yaml_judges_its_probes() {
        let declared = format!(
            "tests:\n  - name: t\n    expected_components:\n      - id: \"{POOL}\"\n        \
             tokens: [\"{USDC}\", \"{WETH}\"]\n        execution_tolerance: {{bps: 1, reason: \
             \"r\"}}\n"
        );
        // Half a bps short of the quote, less the fee.
        let short = |_: &str, input: &TychoExecutionInput| {
            let expected = fee().deduct(&input.expected_amount_out);
            Some(success(&expected - &expected / 20_000u32))
        };
        assert_eq!(
            turn_outcomes(&strict_with(None, &declared), 1, short)[1],
            one(CheckId::ExecLadder, PASS)
        );
        assert_eq!(
            turn_outcomes(&strict(None), 1, short)[1],
            one(CheckId::ExecLadder, fail(Fail::Mismatch))
        );
    }

    #[test]
    fn a_direction_without_a_limit_records_its_checks_not_run() {
        let strict = strict(None);
        let zero = BigUint::zero();
        drop(probe_state(&strict, &state(), &zero, 1));
        assert_eq!(
            recorded(),
            each(&[
                (CheckId::SimLadder, not_run(NotRun::ZeroLimit)),
                (CheckId::ExecLadder, not_run(NotRun::ZeroLimit)),
            ])
        );
        // The next size, 1% of 99, rounds to zero.
        drop(probe_state(&strict, &state(), &BigUint::from(99u32), 1));
        assert_eq!(
            recorded(),
            each(&[
                (CheckId::SimLadder, not_run(NotRun::ZeroAmount)),
                (CheckId::ExecLadder, not_run(NotRun::ZeroAmount)),
            ])
        );
    }

    #[test]
    fn a_fraction_that_does_not_quote_fails_the_ladder() {
        let strict = strict(None);
        let empty = UniswapV2State::new(U256::ZERO, U256::ZERO);
        drop(probe_state(&strict, &empty, &BigUint::from(1_000_000u32), 1));
        assert_eq!(
            recorded(),
            each(&[
                (CheckId::SimLadder, fail(Fail::QuoteError)),
                (CheckId::ExecLadder, not_run(NotRun::QuoteFailed)),
            ])
        );
    }

    #[test]
    fn the_limit_is_not_probed_without_a_declaration() {
        let strict = strict(None);
        assert_eq!(
            turn_outcomes(&strict, 4, |_, _| None),
            each(&[
                (CheckId::LimitsAtLimit, not_run(NotRun::NoDeclaration)),
                (CheckId::LimitsPastLimit, not_run(NotRun::NoDeclaration)),
                (CheckId::LimitsMaxOutput, not_run(NotRun::NoDeclaration)),
            ])
        );
    }

    #[test]
    fn a_soft_limit_is_swapped_at_the_limit_only() {
        let strict = strict(Some("{kind: soft, reason: \"constant product\"}"));
        let swaps = probe(&strict, 4);
        let roles: Vec<&ProbeRole> = swaps
            .iter()
            .map(|(_, _, probe)| &probe.role)
            .collect();
        assert_eq!(roles, vec![&ProbeRole::AtLimit]);
        drop(swaps);
        recorded();
        assert_eq!(
            turn_outcomes(&strict, 4, |_, input| Some(paid(input))),
            each(&[
                (CheckId::LimitsMaxOutput, PASS),
                (CheckId::LimitsPastLimit, PASS),
                (CheckId::LimitsAtLimit, PASS),
            ])
        );
    }

    #[test]
    fn a_hard_limit_the_venue_quotes_past_is_not_swapped_past() {
        // Uniswap V2 quotes any input, so the quote past a "hard" limit already fails the check,
        // and no swap past the limit is queued.
        let strict = strict(Some("{kind: hard}"));
        let swaps = probe(&strict, 4);
        let roles: Vec<&ProbeRole> = swaps
            .iter()
            .map(|(_, _, probe)| &probe.role)
            .collect();
        assert_eq!(roles, vec![&ProbeRole::AtLimit]);
        drop(swaps);
        recorded();
        assert_eq!(
            turn_outcomes(&strict, 4, |_, _| Some(revert())),
            each(&[
                (CheckId::LimitsPastLimit, fail(Fail::LimitQuote)),
                (CheckId::LimitsMaxOutput, PASS),
                (CheckId::LimitsAtLimit, fail(Fail::Revert)),
            ])
        );
    }

    #[test]
    fn an_under_cap_limit_is_swapped_at_and_one_unit_past_the_limit() {
        let strict = strict(Some("{kind: under_cap, reason: \"r; gives up 1%\"}"));
        let swaps = probe(&strict, 4);
        let roles: Vec<&ProbeRole> = swaps
            .iter()
            .map(|(_, _, probe)| &probe.role)
            .collect();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0], &ProbeRole::AtLimit);
        assert!(matches!(roles[1], ProbeRole::PastLimit { claim: PastLimitClaim::Executes, .. }));
        drop(swaps);
        recorded();

        let mut outcomes = turn_outcomes(&strict, 4, |_, input| Some(paid(input)));
        // `judge` goes through the swaps in any order.
        outcomes[1..].sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected = each(&[
            (CheckId::LimitsMaxOutput, PASS),
            (CheckId::LimitsAtLimit, PASS),
            (CheckId::LimitsPastLimit, PASS),
        ]);
        expected[1..].sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(outcomes, expected);

        // Without the swap at the limit, the swap past it proves nothing.
        let mut outcomes =
            turn_outcomes(&strict, 4, |kind, input| (kind == "past_limit").then(|| paid(input)));
        outcomes[1..].sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected = each(&[
            (CheckId::LimitsMaxOutput, PASS),
            (CheckId::LimitsAtLimit, not_run(NotRun::NoResult)),
            (CheckId::LimitsPastLimit, not_run(NotRun::AtLimitNotExecuted)),
        ]);
        expected[1..].sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(outcomes, expected);
    }

    /// The outcome of judging a past-limit swap with `claim`, the swap at the limit having
    /// `at`'s result and the past one `past`'s.
    fn past_limit_outcome(
        claim: PastLimitClaim,
        at: Option<TychoExecutionResult>,
        past: Option<TychoExecutionResult>,
    ) -> (&'static str, &'static str) {
        let strict = strict(None);
        let swaps = probe(&strict, 1);
        let (_, input, fraction) = &swaps[0];
        let probe = Probe {
            role: ProbeRole::PastLimit { at_limit: "at".to_string(), claim },
            ..fraction.clone()
        };
        let mut results: HashMap<String, TychoExecutionResult> = past
            .map(|past| ("past".to_string(), past))
            .into_iter()
            .collect();
        results.extend(at.map(|at| ("at".to_string(), at)));
        match judged("past", &probe, input, &results, fee()) {
            Outcome::Pass => PASS,
            Outcome::Fail(reason, _) => fail(reason),
            Outcome::NotRun(reason) => not_run(reason),
        }
    }

    #[test]
    fn a_swap_past_the_limit_must_revert_once_the_limit_executed() {
        let executed = || Some(success(BigUint::from(1u32)));
        let judge = |at, past| past_limit_outcome(PastLimitClaim::Reverts, at, Some(past));
        assert_eq!(judge(executed(), revert()), PASS);
        assert_eq!(judge(executed(), success(BigUint::from(1u32))), fail(Fail::PastLimitExecuted));
        assert_eq!(judge(Some(revert()), revert()), not_run(NotRun::AtLimitNotExecuted));
        assert_eq!(
            judge(executed(), failed("Transaction failed: block 0x1929fcc not found")),
            not_run(NotRun::Rpc)
        );
        // The router's own revert is not the venue rejecting the input.
        assert_eq!(judge(executed(), paused()), not_run(NotRun::Router));
        assert_eq!(
            past_limit_outcome(PastLimitClaim::Reverts, executed(), None),
            not_run(NotRun::NoResult)
        );
    }

    #[test]
    fn a_swap_past_an_under_cap_limit_must_execute_once_the_limit_executed() {
        let executed = || Some(success(BigUint::from(1u32)));
        let judge = |past| past_limit_outcome(PastLimitClaim::Executes, executed(), Some(past));
        assert_eq!(judge(success(BigUint::from(1u32))), PASS);
        assert_eq!(judge(revert()), fail(Fail::PastLimitReverted));
    }

    #[test]
    fn a_probe_without_its_input_has_no_result() {
        let strict = strict(None);
        let swaps = probe(&strict, 1);
        recorded();
        let probes: HashMap<String, Probe> = swaps
            .into_iter()
            .map(|(id, _, probe)| (id, probe))
            .collect();
        judge(&probes, &HashMap::new(), &HashMap::new(), fee());
        drop(probes);
        assert_eq!(recorded(), each(&[(CheckId::ExecLadder, not_run(NotRun::NoResult))]));
    }

    #[test]
    fn after_a_failed_batch_the_probes_of_the_batches_that_ran_are_judged() {
        let strict = strict(None);
        for ran in [true, false] {
            let swaps = probe(&strict, 1);
            recorded();
            let mut inputs = HashMap::new();
            let mut results = HashMap::new();
            let mut probes = HashMap::new();
            for (id, input, probe) in swaps {
                if ran {
                    results.insert(id.clone(), paid(&input));
                }
                inputs.insert(id.clone(), input);
                probes.insert(id, probe);
            }
            judge_after_failed_batch(&probes, &inputs, &results, fee());
            drop(probes);
            let outcome = if ran { PASS } else { not_run(NotRun::BatchFailed) };
            assert_eq!(recorded(), each(&[(CheckId::ExecLadder, outcome)]), "ran: {ran}");
        }
    }

    #[test]
    fn a_probe_dropped_unjudged_records_why() {
        let strict = strict(None);
        let swaps = probe(&strict, 1);
        recorded();
        let probes: HashMap<String, Probe> = swaps
            .into_iter()
            .map(|(id, _, probe)| (id, probe))
            .collect();
        abandon(&probes, NotRun::BatchFailed);
        drop(probes);
        assert_eq!(recorded(), each(&[(CheckId::ExecLadder, not_run(NotRun::BatchFailed))]));

        // `--disable-execution` gives its own reason.
        let probes: HashMap<String, Probe> = probe(&strict, 1)
            .into_iter()
            .map(|(id, _, probe)| (id, probe))
            .collect();
        recorded();
        abandon(&probes, NotRun::ExecutionDisabled);
        drop(probes);
        assert_eq!(recorded(), each(&[(CheckId::ExecLadder, not_run(NotRun::ExecutionDisabled))]));

        // Dropped without a reason, it is a bug, and shows as one.
        drop(probe(&strict, 1));
        assert_eq!(
            recorded(),
            each(&[(CheckId::SimLadder, PASS), (CheckId::ExecLadder, not_run(NotRun::Unrecorded))])
        );
    }

    /// The real limit of USDC -> WETH on [`state`].
    fn usdc_limit() -> BigUint {
        state()
            .get_limits(Bytes::from_str(USDC).unwrap(), Bytes::from_str(WETH).unwrap())
            .unwrap()
            .0
    }

    #[test]
    fn a_limit_that_does_not_quote_leaves_the_rest_not_run() {
        let strict = strict(Some("{kind: hard}"));
        let empty = UniswapV2State::new(U256::ZERO, U256::ZERO);
        // The fourth turn is the limit.
        drop(probe_state(&strict, &empty, &BigUint::from(1_000_000u32), 4));
        let outcomes = recorded();
        assert_eq!(
            outcomes[outcomes.len() - 3..],
            each(&[
                (CheckId::LimitsAtLimit, fail(Fail::LimitQuote)),
                (CheckId::LimitsPastLimit, not_run(NotRun::AtLimitFailed)),
                (CheckId::LimitsMaxOutput, not_run(NotRun::AtLimitFailed)),
            ])
        );
    }

    #[test]
    fn a_limit_at_the_execution_cap_is_not_probed_past() {
        let strict = strict(Some("{kind: soft, reason: \"constant product\"}"));
        let cap = BigUint::from(i128::MAX as u128);
        drop(probe_state(&strict, &state(), &(&cap + 1u32), 4));
        let outcomes = recorded();
        assert!(
            outcomes.contains(&one(CheckId::LimitsPastLimit, not_run(NotRun::ExecutionCap))),
            "{outcomes:?}"
        );
    }

    #[test]
    fn a_quote_at_the_limit_past_the_max_output_fails_it() {
        // get_limits reports a max output far below what the limit quotes.
        let strict = strict(Some("{kind: soft, reason: \"constant product\"}"));
        let component = component();
        let pool = state();
        let (usdc, weth) = (&component.tokens[0], &component.tokens[1]);
        let target = Target {
            chain: Chain::Ethereum,
            component: &component,
            state: &pool,
            token_in: usdc,
            token_out: weth,
            max_input: &usdc_limit(),
            max_output: &BigUint::from(1u32),
            simulation_id: "sim".to_string(),
        };
        let package = strict.package("uniswap_v2").unwrap();
        for _ in 0..4 {
            drop(probe_direction(&strict, package, &target));
        }
        let outcomes = recorded();
        assert!(
            outcomes.contains(&one(CheckId::LimitsMaxOutput, fail(Fail::LimitQuote))),
            "{outcomes:?}"
        );
    }

    #[test]
    fn a_probe_that_cannot_be_encoded_is_not_run() {
        // Under cap, so the simulation leaves the swap past the limit to execute: one unit past
        // a Uniswap V2 pool's limit quotes.
        let strict = strict(Some("{kind: under_cap, reason: r}"));
        let mut unencodable = component();
        unencodable.protocol_system = "no_such_protocol".to_string();
        let limit = usdc_limit();

        // A fraction quotes, and its swap cannot be encoded.
        drop(probe_component(&strict, &unencodable, &state(), &limit, 1));
        assert_eq!(
            recorded(),
            each(&[(CheckId::SimLadder, PASS), (CheckId::ExecLadder, not_run(NotRun::Encoding))])
        );

        // Without the swap at the limit, the swap past it is not run either. Turns 2 and 3 are
        // fractions; the fourth is the limit.
        drop(probe_component(&strict, &unencodable, &state(), &limit, 2));
        recorded();
        drop(probe_component(&strict, &unencodable, &state(), &limit, 1));
        let outcomes = recorded();
        for check in [CheckId::LimitsAtLimit, CheckId::LimitsPastLimit] {
            assert!(outcomes.contains(&one(check, not_run(NotRun::Encoding))), "{outcomes:?}");
        }
    }

    #[test]
    fn an_unprobeable_direction_records_the_checks_of_its_size_not_run() {
        let strict = strict(None);
        let component = component();
        not_probed(
            &strict,
            &component,
            &component.tokens[0],
            &component.tokens[1],
            NotRun::BookVenue,
        );
        assert_eq!(
            recorded(),
            each(&[
                (CheckId::SimLadder, not_run(NotRun::BookVenue)),
                (CheckId::ExecLadder, not_run(NotRun::BookVenue)),
            ])
        );
    }

    #[test]
    fn a_check_is_recorded_once() {
        let strict = strict(None);
        let swaps = probe(&strict, 1);
        recorded();
        let turn = &swaps[0].2.turn;
        turn.settle(CheckId::ExecLadder, Outcome::Pass);
        turn.settle(CheckId::ExecLadder, Outcome::Pass);
        assert_eq!(recorded(), each(&[(CheckId::ExecLadder, PASS)]));
    }
}
