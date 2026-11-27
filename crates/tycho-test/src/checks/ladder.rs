//! The quote ladder: for every direction of a component, its limits, spot price and quotes at
//! 0.1%, 1% and 10% of its limit, and, where the package declares the limit, the checks at and
//! past it. [`run_ladder`] encodes each quote as a router swap, for the fixture executor
//! protocol-testing overrides; the caller executes them, and [`judge_limits`] judges the limit
//! checks from the results.

use std::collections::{HashMap, HashSet};

use itertools::Itertools;
use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use num_traits::Zero;
use serde_json::json;
use tracing::{info, warn};
use tycho_simulation::{
    protocol::models::{ProtocolComponent, Update},
    tycho_common::{
        models::{token::Token, Chain},
        simulation::protocol_sim::ProtocolSim,
    },
};

use crate::{
    checks::{
        execution::{execution_mismatch, past_limit_mismatch},
        limits::{self, LimitSizes, PastLimitClaim, PastLimitInSimulation, Quote},
        outcome::{CheckId, LimitReport, Outcome},
    },
    execution::{
        encoding::{encode_swap, EXECUTOR_ADDRESS},
        models::{TychoExecutionInput, TychoExecutionResult},
    },
    expectations::{DeclaredBy, Direction, ExecutionTolerances, Limits},
};

/// The limit declared for each direction of each component a runner checks, keyed by lowercase
/// component id and direction, with where the declaration comes from.
pub type DeclaredLimits = HashMap<(String, Direction), (Limits, DeclaredBy)>;

/// What the ladder hands to execution: the ladder's quotes and, in strict mode, the limit checks
/// of each declared direction with their swaps, which [`judge_limits`] judges once executed.
#[derive(Default)]
pub struct LadderOutput {
    /// The ladder's quotes: each execution must match its simulation.
    executions: HashMap<String, TychoExecutionInput>,
    /// Strict: the swaps at and past each direction's limit, keyed by the ids in
    /// [`LimitCheckRecord`]. The swap at the limit is executed like the ladder's; the swap past
    /// it must do what the declared kind claims.
    limit_executions: HashMap<String, TychoExecutionInput>,
    /// Strict: every direction whose limit was checked.
    limit_checks: Vec<LimitCheckRecord>,
    /// Strict: every declared direction whose limit was not checked, and why.
    unchecked_limits: Vec<UncheckedLimit>,
    /// Why each component whose ladder failed failed, in id order.
    failed_components: Vec<String>,
    /// Each component that trades no direction at this block, every one with a zero limit, in id
    /// order. Its declared directions are recorded as skipped for the zero limit.
    untraded_components: Vec<String>,
}

impl LadderOutput {
    /// Why the ladder failed for some components, all of them in one error; `None` when it
    /// succeeded for every component it ran on. A component that trades no direction is a
    /// failure: a test lists the components it expects to trade.
    pub fn failure(&self) -> Option<miette::Report> {
        Self::failure_of(
            self.failed_components
                .iter()
                .chain(&self.untraded_components),
        )
    }

    /// [`LadderOutput::failure`], with a component that trades no direction at this block passed
    /// over: strict `full` ladders every pool of an update, and a pool with no capacity at a
    /// block says nothing about the package.
    pub fn failure_of_traded(&self) -> Option<miette::Report> {
        Self::failure_of(self.failed_components.iter())
    }

    fn failure_of<'a>(failures: impl Iterator<Item = &'a String>) -> Option<miette::Report> {
        let failures: Vec<&String> = failures.collect();
        if failures.is_empty() {
            return None;
        }
        Some(miette!(
            "the ladder failed for {} component(s): {}",
            failures.len(),
            failures.iter().join("; ")
        ))
    }

    /// Adds `other`'s swaps and limit records.
    fn extend(&mut self, other: LadderOutput) {
        self.executions.extend(other.executions);
        self.limit_executions
            .extend(other.limit_executions);
        self.limit_checks
            .extend(other.limit_checks);
        self.unchecked_limits
            .extend(other.unchecked_limits);
        self.failed_components
            .extend(other.failed_components);
        self.untraded_components
            .extend(other.untraded_components);
    }

    /// The ladder's quotes, by simulation id: each execution must match its simulation.
    pub fn executions(&self) -> &HashMap<String, TychoExecutionInput> {
        &self.executions
    }

    /// [`Self::executions`], taken out.
    pub fn into_executions(self) -> HashMap<String, TychoExecutionInput> {
        self.executions
    }

    /// Whether there is any swap to execute, without copying them as [`Self::all_executions`]
    /// does.
    pub fn has_executions(&self) -> bool {
        !self.executions.is_empty() || !self.limit_executions.is_empty()
    }

    /// Every swap to execute: the ladder's quotes and the swaps at and past each limit.
    pub fn all_executions(&self) -> HashMap<String, TychoExecutionInput> {
        let mut all = self.executions.clone();
        all.extend(self.limit_executions.clone());
        all
    }

    /// The swaps at each limit. They measure drift like the ladder's quotes; a swap past a limit
    /// pays against the quote at the limit, so it does not.
    pub fn at_limit_executions(&self) -> HashMap<String, TychoExecutionInput> {
        self.limit_checks
            .iter()
            .filter_map(|record| record.at_limit_execution())
            .filter_map(|id| self.limit_executions.get_key_value(id))
            .map(|(id, input)| (id.clone(), input.clone()))
            .collect()
    }
}

/// Strict: a declared direction of a simulated component whose limit was not checked.
struct UncheckedLimit {
    /// The component's id, as indexed.
    component: String,
    direction: Direction,
    limits: Limits,
    declared_by: DeclaredBy,
    /// Why not. A zero limit is a skip, a direction the ladder never reached is a failure, and a
    /// component whose ladder failed leaves its directions not run.
    why: Unchecked,
}

impl UncheckedLimit {
    fn new(
        component: &str,
        direction: &Direction,
        (limits, declared_by): &(Limits, DeclaredBy),
        why: Unchecked,
    ) -> Self {
        Self {
            component: component.to_string(),
            direction: direction.clone(),
            limits: limits.clone(),
            declared_by: *declared_by,
            why,
        }
    }
}

/// Why a declared direction's limit was not checked.
enum Unchecked {
    /// `get_limits` reports a zero max input, with this max output: the venue does not trade this
    /// direction at this block.
    ZeroLimit { max_output: BigUint },
    /// The ladder did not quote the direction, for example because the indexed tokens differ
    /// from the declared ones.
    NotQuoted,
    /// The component's ladder failed, so `sim.ladder` fails and its limits are not checked.
    LadderFailed,
}

/// Strict: what was found about one direction's limit before execution, which [`judge_limits`]
/// completes with the executions.
struct LimitCheckRecord {
    /// The component's id, as indexed.
    component: String,
    direction: Direction,
    /// The declaration the direction is checked against, and where it comes from.
    limits: Limits,
    declared_by: DeclaredBy,
    /// `get_limits`' max input, after the `int128` cap, and its max output.
    max_input: BigUint,
    max_output: BigUint,
    /// Whether the limit is at or above `int128::MAX`, the cap `run_ladder` applies to every
    /// protocol's executions.
    capped: bool,
    /// The checks that failed in simulation, with the reason.
    failures: Vec<(CheckId, String)>,
    /// How far the swap at the limit got.
    at_limit: AtLimit,
}

/// How far a direction's swap at the limit got before execution.
enum AtLimit {
    /// `max_input` did not quote, which is recorded as a failure of `limits.at_limit`.
    QuoteFailed,
    /// It quoted, and the component skips execution, for this reason: the limit checks run in
    /// simulation only.
    NotExecuted { reason: String },
    /// It quoted, and its swap is queued, by its id in [`LadderOutput::limit_executions`].
    Queued {
        id: String,
        /// The id of the swap past the limit, and what its execution must do: queued for a hard
        /// or under-cap limit whose quote past it behaved as declared. `None` for a soft limit,
        /// whose claim is checked in simulation alone, and for one whose quote past it
        /// contradicted the declaration, which is recorded as a failure of `limits.past_limit`.
        past: Option<(String, PastLimitClaim)>,
    },
}

impl LimitCheckRecord {
    /// The id of the swap at the limit in [`LadderOutput::limit_executions`], when it was queued.
    fn at_limit_execution(&self) -> Option<&String> {
        match &self.at_limit {
            AtLimit::Queued { id, .. } => Some(id),
            AtLimit::QuoteFailed | AtLimit::NotExecuted { .. } => None,
        }
    }

    /// The swap past the limit in [`LadderOutput::limit_executions`], by its id, and what its
    /// execution must do, when it was queued.
    fn past_limit_execution(&self) -> Option<&(String, PastLimitClaim)> {
        match &self.at_limit {
            AtLimit::Queued { past, .. } => past.as_ref(),
            AtLimit::QuoteFailed | AtLimit::NotExecuted { .. } => None,
        }
    }

    /// The limit and its declaration, as every `limits.*` result of the direction reports them.
    fn report(&self) -> LimitReport {
        LimitReport::new(
            &self.limits,
            self.declared_by,
            Some(&LimitSizes {
                max_input: &self.max_input,
                max_output: &self.max_output,
                capped: self.capped,
            }),
        )
    }

    fn failures_of(&self, check: CheckId) -> Vec<String> {
        self.failures
            .iter()
            .filter(|(id, _)| *id == check)
            .map(|(_, reason)| reason.clone())
            .collect()
    }

    /// What `limits.at_limit` records when nothing failed: a pass once the quote at the limit
    /// was executed, a skip when the component skips execution. A failed quote is recorded as a
    /// failure, which outranks this.
    fn at_limit_unfailed(&self) -> Outcome {
        match &self.at_limit {
            AtLimit::Queued { .. } => Outcome::Pass,
            AtLimit::NotExecuted { reason } => Outcome::Skipped {
                reason: format!("the quote at the limit is not executed: {reason}"),
            },
            AtLimit::QuoteFailed => {
                Outcome::NotRun { reason: "the limit did not quote".to_string() }
            }
        }
    }

    /// What `limits.past_limit` records when nothing failed, from what the simulation alone
    /// decides ([`limits::past_limit_in_simulation`]), then the execution:
    ///
    /// - Not run when the limit did not quote, whatever the kind: nothing was probed past it.
    /// - Not run for a limit at the `int128` cap, which is not the venue's bound, as in the cluster
    ///   test (`execution_cap`).
    /// - A pass for a soft limit, whose claim is checked in simulation alone: twice the limit
    ///   quoted.
    /// - Skipped when the component skips execution.
    /// - Not run when the swap at the limit did not execute: what happens past a limit that does
    ///   not execute proves nothing.
    /// - A pass once the swap past the limit was executed, and did what the declaration claims.
    fn past_limit_unfailed(&self, at_limit_executed: bool) -> Outcome {
        let not_run = || Outcome::NotRun { reason: format!("{} failed", CheckId::LimitsAtLimit) };
        let quoted = !matches!(self.at_limit, AtLimit::QuoteFailed);
        match limits::past_limit_in_simulation(quoted, self.capped, &self.limits) {
            PastLimitInSimulation::AtLimitFailed => not_run(),
            PastLimitInSimulation::Capped => Outcome::NotRun {
                reason: "the limit is at the int128::MAX execution cap, which is not the venue's \
                         bound"
                    .to_string(),
            },
            PastLimitInSimulation::Passed => Outcome::Pass,
            PastLimitInSimulation::NeedsExecution => match &self.at_limit {
                AtLimit::QuoteFailed => not_run(),
                AtLimit::NotExecuted { reason } => Outcome::Skipped {
                    reason: format!("the swap past the limit is not executed: {reason}"),
                },
                AtLimit::Queued { past: Some(_), .. } if at_limit_executed => Outcome::Pass,
                AtLimit::Queued { past: Some(_), .. } => not_run(),
                // The quote past the limit contradicted the declaration, which is recorded as a
                // failure, and that outranks this.
                AtLimit::Queued { past: None, .. } => Outcome::NotRun {
                    reason: "the quote past the limit contradicted the declaration".to_string(),
                },
            },
        }
    }

    /// What `limits.max_output` records when nothing failed: not run when the limit did not
    /// quote, since there is nothing to compare, and a pass otherwise.
    fn max_output_unfailed(&self) -> Outcome {
        match self.at_limit {
            AtLimit::QuoteFailed => {
                Outcome::NotRun { reason: format!("{} failed", CheckId::LimitsAtLimit) }
            }
            AtLimit::NotExecuted { .. } | AtLimit::Queued { .. } => Outcome::Pass,
        }
    }
}

/// `limit`, capped at `int128::MAX`: Uniswap V4's bound, which `run_ladder` and the cluster
/// probes apply to every protocol's executions, including those whose executor takes more.
///
/// The bound comes from Uniswap V4, which settles a swap through a `BalanceDelta` of two `int128`s,
/// and the executor's `swapExactInputSingle` takes the amount as a `uint128` that v4-core casts to
/// `int128`, so no amount above `int128::MAX` can be executed however deep the pool is. A limit
/// above that ceiling is not executable, and the percentages the harness trades at must stay
/// distinct sizes, so the cap belongs on the limit itself rather than on each amount derived from
/// it.
///
/// The amount is `limit` unchanged when it is below the ceiling.
pub fn executable_max_input(limit: &BigUint) -> ExecutableLimit {
    let ceiling = int128_ceiling();
    // Even at exactly `int128::MAX`, one unit past it reverts in the executor's cast, whatever the
    // venue allows, so a limit at the ceiling is the cap too.
    let capped = *limit >= ceiling;
    ExecutableLimit { amount: if *limit > ceiling { ceiling } else { limit.clone() }, capped }
}

/// A limit after the `int128` cap ([`executable_max_input`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableLimit {
    /// The largest amount a swap can be executed with.
    pub amount: BigUint,
    /// Whether the cap is the bound: the limit is at or above `int128::MAX`, so what is past
    /// `amount` reverts in the executor, not at the venue's bound.
    pub capped: bool,
}

/// A size a direction is swapped at, relative to its limit after the `int128` cap
/// ([`executable_max_input`]): one of the ladder's fractions, or the limit itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbeSize {
    /// The limit times this many thousandths.
    Fraction(u32),
    /// The limit itself, where the limit checks run.
    Limit,
}

impl ProbeSize {
    /// The ladder's rungs: 0.1%, 1% and 10% of the limit. `run_ladder` and the `tvl_only`
    /// comparison quote at these.
    pub const LADDER: [ProbeSize; 3] =
        [ProbeSize::Fraction(1), ProbeSize::Fraction(10), ProbeSize::Fraction(100)];

    /// The ladder's rungs, then the limit: the sizes a direction is swapped at within its limit,
    /// smallest first. The swap past the limit is sized by the limit checks, not here.
    pub const ALL: [ProbeSize; 4] =
        [ProbeSize::LADDER[0], ProbeSize::LADDER[1], ProbeSize::LADDER[2], ProbeSize::Limit];

    /// The amount to swap for an `executable_limit`, the limit after the `int128` cap. Zero when
    /// a fraction of a small limit rounds down to nothing.
    pub fn amount(self, executable_limit: &BigUint) -> BigUint {
        match self {
            ProbeSize::Fraction(thousandths) => executable_limit * thousandths / 1000u32,
            ProbeSize::Limit => executable_limit.clone(),
        }
    }
}

// `ALL` lists every rung of `LADDER`, then the limit: a rung added to the ladder fails the build
// until `ALL` lists it too.
const _: () = assert!(ProbeSize::ALL.len() == ProbeSize::LADDER.len() + 1);

impl std::fmt::Display for ProbeSize {
    /// `0.1%`, `1%`, `10%` or `limit`: the label reports and metrics use.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeSize::Fraction(thousandths) if thousandths % 10 == 0 => {
                write!(f, "{}%", thousandths / 10)
            }
            ProbeSize::Fraction(thousandths) => {
                write!(f, "{}.{}%", thousandths / 10, thousandths % 10)
            }
            ProbeSize::Limit => f.write_str("limit"),
        }
    }
}

/// `int128::MAX`, the cap `run_ladder` applies to every protocol's executions (see
/// [`executable_max_input`]).
fn int128_ceiling() -> BigUint {
    BigUint::from(i128::MAX as u128)
}

/// Which components skip the ladder, by lowercase id, each with its reason.
#[derive(Clone, Copy)]
pub struct Skips<'a> {
    /// Components not simulated, nor executed.
    pub simulation: &'a HashMap<String, &'a str>,
    /// Components simulated but not executed.
    pub execution: &'a HashMap<String, &'a str>,
}

/// Runs the ladder on every component of `update`, in id order, and encodes each quote as a
/// router swap on `chain` for the fixture executor at [`EXECUTOR_ADDRESS`].
///
/// For each direction between a component's tokens it gets the limits, the spot price, and
/// quotes at 0.1%, 1% and 10% of the limit (after the `int128` cap). A direction with a zero
/// limit is passed over. With `declared_limits`, keyed by `(lowercase component id, direction)`,
/// it also checks each declared direction's limit and queues the swap at the limit and, for a
/// hard or under-cap limit, the swap past it. A declared direction it passes over or never reaches
/// is kept too, and [`judge_limits`] records its checks as skipped, not run or failed. `skips` and
/// `declared_limits` are looked up by the lowercase id, whatever case the update carries.
///
/// A component fails, and is listed in [`LadderOutput::failure`] with its swaps dropped and its
/// declared limits not checked, when:
/// - it is missing from `protocol_components`;
/// - `get_limits`, `spot_price` or `get_amount_out` errors;
/// - a ladder amount rounds to zero;
/// - it has fewer than two tokens;
/// - a swap cannot be encoded.
///
/// A component whose every direction has a zero limit is laddered, with nothing to quote, and
/// listed apart: [`LadderOutput::failure`] counts it and [`LadderOutput::failure_of_traded`]
/// does not.
///
/// The other components of the update are still laddered.
pub fn run_ladder(
    chain: Chain,
    update: &Update,
    protocol_components: &HashMap<String, ProtocolComponent>,
    skips: Skips<'_>,
    protocol_system: &str,
    declared_limits: Option<&DeclaredLimits>,
) -> LadderOutput {
    let mut output = LadderOutput::default();
    let run = LadderRun { chain, skips, protocol_system };
    // Each component's declared directions, grouped and sorted once for the whole update.
    let mut declared_by_component: HashMap<&str, Vec<DeclaredDirection<'_>>> = HashMap::new();
    for ((declared_id, direction), declared) in declared_limits
        .into_iter()
        .flatten()
        .sorted_by(|a, b| a.0.cmp(b.0))
    {
        declared_by_component
            .entry(declared_id.as_str())
            .or_default()
            .push((direction, declared));
    }

    // Sorted, so a report lists components in the same order on every run.
    for (id, state) in update
        .states
        .iter()
        .sorted_by(|a, b| a.0.cmp(b.0))
    {
        // The skips and limits are keyed by lowercase id; the update keeps the id as indexed.
        let key = id.to_lowercase();
        // The caller logs a skip's reason once per component.
        if skips.simulation.contains_key(&key) {
            continue;
        }
        let declared = declared_by_component
            .get(key.as_str())
            .map_or(&[][..], Vec::as_slice);
        let mut part = LadderOutput::default();
        let component = protocol_components.get(id);
        let laddered = match component {
            Some(component) => {
                ladder_component(&run, (id, &key), state.as_ref(), component, declared, &mut part)
            }
            None => Err(miette!("Couldn't find protocol component {id}")),
        };
        match laddered {
            Ok(Traded::Some) => output.extend(part),
            Ok(Traded::Nothing) => {
                warn!("[{id}] Every swap direction reported a zero limit at this block");
                output.extend(part);
                output.untraded_components.push(format!(
                    "No tradable direction for pool {id}: every swap direction reported a \
                         zero limit."
                ));
            }
            Err(e) => {
                warn!("[{id}] The ladder failed, so the component's swaps are dropped: {e:#}");
                output
                    .failed_components
                    .push(format!("{e:#}"));
                // Its declared limits are not checked, since the ladder they need failed. The id is
                // the one every other row reports: as indexed, or the lowercase key when the
                // component is missing.
                let component_id =
                    component.map_or_else(|| key.clone(), |component| component.id.to_string());
                for (direction, declared) in declared {
                    output
                        .unchecked_limits
                        .push(UncheckedLimit::new(
                            &component_id,
                            direction,
                            declared,
                            Unchecked::LadderFailed,
                        ));
                }
            }
        }
    }

    output
}

/// What [`run_ladder`] runs every component's ladder with.
#[derive(Clone, Copy)]
struct LadderRun<'a> {
    chain: Chain,
    skips: Skips<'a>,
    protocol_system: &'a str,
}

/// One declared direction of a component, with its declaration.
type DeclaredDirection<'a> = (&'a Direction, &'a (Limits, DeclaredBy));

/// Whether a component quoted any direction.
enum Traded {
    Some,
    /// Every direction has a zero limit.
    Nothing,
}

/// The ladder of one component, `id` as the update carries it and `key` lowercase, into `part`,
/// with the component's `declared` directions; see [`run_ladder`].
fn ladder_component(
    run: &LadderRun<'_>,
    (id, key): (&str, &str),
    state: &dyn ProtocolSim,
    component: &ProtocolComponent,
    declared: &[DeclaredDirection<'_>],
    part: &mut LadderOutput,
) -> miette::Result<Traded> {
    let LadderRun { chain, skips, protocol_system } = *run;
    let declaration_of = |direction: &Direction| {
        declared
            .iter()
            .find(|(declared, _)| *declared == direction)
            .map(|(_, declaration)| *declaration)
    };
    let component_id = component.id.to_string();
    let mut reached = HashSet::new();

    let tokens = component.tokens.clone();

    // Test all permutations of swap directions
    let swap_directions: Vec<_> = tokens
        .iter()
        .permutations(2)
        .map(|perm| (perm[0], perm[1]))
        .collect();

    let mut quoted_any_direction = false;

    for &(token_in, token_out) in &swap_directions {
        let swap = Swap { chain, component, token_in, token_out, protocol_system };
        let (max_input, max_output) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .into_diagnostic()
            .wrap_err(format!(
                "Error getting limits for Pool {id:?} for in token: {}, and out token: {}",
                token_in.address, token_out.address
            ))?;

        info!(
            "[{}] Retrieved limits. | Max input: {max_input} {} | Max output: {max_output} {}",
            id, token_in.symbol, token_out.symbol
        );

        let ExecutableLimit { amount: executable_input, capped } = executable_max_input(&max_input);
        if executable_input != max_input {
            warn!(
                "[{}] Limit of {max_input} {} exceeds what a swap can be executed with; \
                 sizing trades from {executable_input} instead",
                id, token_in.symbol
            );
        }
        let max_input = executable_input;

        // A zero limit means the venue does not quote this direction at all - a
        // one-directional component such as ETH -> stETH staking, or a redemption
        // rate limit with no capacity at this block. Skip the direction instead of
        // failing the component; a component whose every direction is zero is listed as
        // untraded (`Traded::Nothing`).
        let direction =
            Direction { token_in: token_in.address.clone(), token_out: token_out.address.clone() };
        reached.insert(direction.clone());
        if max_input.is_zero() {
            warn!(
                "[{}] Zero limit for {} -> {}, skipping direction",
                id, token_in.symbol, token_out.symbol
            );
            if let Some(declaration) = declaration_of(&direction) {
                part.unchecked_limits
                    .push(UncheckedLimit::new(
                        &component_id,
                        &direction,
                        declaration,
                        Unchecked::ZeroLimit { max_output: max_output.clone() },
                    ));
            }
            continue;
        }

        // Priced per direction rather than once per component: consumers key their
        // price data by swap direction, so a venue that quotes only one ordering
        // leaves them without a price for a direction that does trade. Asked after
        // the zero-limit skip, so a direction the venue does not trade is not
        // required to have a price either.
        let spot_price = state
            .spot_price(token_in, token_out)
            .into_diagnostic()
            .wrap_err(format!(
                "Error calculating spot price for Pool {id:?} for in token: {}, and out token: {}",
                token_in.address, token_out.address
            ))?;
        info!("[{}] Spot price {}/{}: {:?}", id, token_in.symbol, token_out.symbol, spot_price);

        // The limit sizes the ladder: a venue may revert at 10 or at 10 million USDC, and a
        // fraction of its own limit is safe for it.
        for size in ProbeSize::LADDER {
            let amount_in = size.amount(&max_input);
            if amount_in.is_zero() {
                return Err(miette!("Amount in at {size} of the limit is zero for pool {id}."));
            }

            let amount_out_result = state
                    .get_amount_out(amount_in.clone(), token_in, token_out)
                    .into_diagnostic()
                    .wrap_err(format!(
                        "Error calculating amount out for Pool {id:?} at {size} with input of {amount_in} {}.",
                        token_in.symbol,
                    ))?;

            info!(
                "[{}] Simulated amount out for trading {} of max: ({} {} -> {} {}) (gas: {})",
                id,
                size,
                amount_in,
                token_in.symbol,
                amount_out_result.amount,
                token_out.symbol,
                amount_out_result.gas
            );

            quoted_any_direction = true;

            if skips.execution.contains_key(key) {
                continue;
            }

            let simulation_id = format!(
                "test_{}_{}_{}_{}",
                component.id, token_in.symbol, token_out.symbol, amount_in
            );
            let input = execution_input(
                &swap,
                amount_in,
                Quote { amount: amount_out_result.amount, gas: amount_out_result.gas },
            )?;
            part.executions
                .insert(simulation_id, input);
        }

        if let Some(declaration) = declaration_of(&direction) {
            let record = check_declared_limit(
                &swap,
                direction,
                state,
                declaration,
                &LimitSizes { max_input: &max_input, max_output: &max_output, capped },
                skips
                    .execution
                    .get(key)
                    .map(|reason| reason.to_string()),
                &mut part.limit_executions,
            )?;
            part.limit_checks.push(record);
        }
    }

    // A declared direction the loop above never reached has no limit result. Fail it rather
    // than let it drop out of the report.
    for (direction, declaration) in declared {
        if !reached.contains(*direction) {
            part.unchecked_limits
                .push(UncheckedLimit::new(
                    &component_id,
                    direction,
                    declaration,
                    Unchecked::NotQuoted,
                ));
        }
    }

    if swap_directions.is_empty() {
        return Err(miette!("No swap direction for pool {id}: it has fewer than two tokens."));
    }
    Ok(if quoted_any_direction { Traded::Some } else { Traded::Nothing })
}

/// One direction of one component, as the router swaps it.
struct Swap<'a> {
    chain: Chain,
    component: &'a ProtocolComponent,
    token_in: &'a Token,
    token_out: &'a Token,
    protocol_system: &'a str,
}

/// Checks the declared limit of `swap`'s direction in simulation, and queues its swaps at and
/// past the limit into `limit_executions`, unless the component skips execution.
fn check_declared_limit(
    swap: &Swap<'_>,
    direction: Direction,
    state: &dyn ProtocolSim,
    (declared, declared_by): &(Limits, DeclaredBy),
    limit: &LimitSizes<'_>,
    execution_skipped: Option<String>,
    limit_executions: &mut HashMap<String, TychoExecutionInput>,
) -> miette::Result<LimitCheckRecord> {
    let Swap { component, token_in, token_out, .. } = swap;
    let id = &component.id;
    let checked = limits::check_direction(
        |amount| {
            state
                .get_amount_out(amount, token_in, token_out)
                .map(|quote| Quote { amount: quote.amount, gas: quote.gas })
                .map_err(|e| e.to_string())
        },
        &format!("{} -> {}", token_in.symbol, token_out.symbol),
        limit,
        declared,
    );
    if let Some(rejection) = &checked.past_limit_rejection {
        info!(
            "[{id}] {} -> {}: the quote past the limit errors: {rejection}",
            token_in.symbol, token_out.symbol
        );
    }
    let at_limit = match (checked.at_limit_quote, execution_skipped) {
        (None, _) => AtLimit::QuoteFailed,
        (Some(_), Some(reason)) => AtLimit::NotExecuted { reason },
        (Some(quote), None) => {
            // The swap past the limit is encoded with the quote at the limit, so its minimum
            // output, 80% of that quote, is one a larger input easily meets: past a hard limit a
            // venue that accepts the input executes the swap, and only a revert passes.
            let past = match checked.past_limit_swap {
                Some(past) => {
                    let past_id = format!("past_limit_{id}_{direction}");
                    limit_executions.insert(
                        past_id.clone(),
                        execution_input(swap, past.amount_in, quote.clone())?,
                    );
                    Some((past_id, past.claim))
                }
                None => None,
            };
            let at_id = format!("at_limit_{id}_{direction}");
            limit_executions
                .insert(at_id.clone(), execution_input(swap, limit.max_input.clone(), quote)?);
            AtLimit::Queued { id: at_id, past }
        }
    };
    Ok(LimitCheckRecord {
        component: id.to_string(),
        direction,
        limits: declared.clone(),
        declared_by: *declared_by,
        max_input: limit.max_input.clone(),
        max_output: limit.max_output.clone(),
        capped: limit.capped,
        failures: checked.failures,
        at_limit,
    })
}

/// Encodes the router swap that executes `quote` for `amount_in`.
fn execution_input(
    swap: &Swap<'_>,
    amount_in: BigUint,
    quote: Quote,
) -> miette::Result<TychoExecutionInput> {
    let Swap { chain, component, token_in, token_out, protocol_system } = *swap;
    // Swaps are grouped before their encoder is looked up, and grouping folds
    // `uniswap_v4_hooks` into `uniswap_v4` because both swap through the same PoolManager.
    // Registering the executor under both names lets the lookup find it whichever of the two
    // the group ends up carrying.
    let mut executors = serde_json::Map::new();
    executors.insert(protocol_system.to_string(), json!(EXECUTOR_ADDRESS));
    if protocol_system == "uniswap_v4_hooks" {
        executors.insert("uniswap_v4".to_string(), json!(EXECUTOR_ADDRESS));
    }
    let executors_json = json!({ (chain.to_string()): executors });
    let (solution, calldata) = encode_swap(
        component,
        None,
        token_in,
        token_out,
        amount_in,
        chain,
        Some(executors_json.to_string()),
        quote.gas.clone(),
        quote.amount.clone(),
    )?;
    Ok(TychoExecutionInput {
        solution,
        transaction: calldata,
        expected_amount_out: quote.amount,
        protocol_system: protocol_system.to_string(),
        component_id: component.id.to_string(),
        token_in: token_in.symbol.clone(),
        token_out: token_out.symbol.clone(),
        estimated_gas: quote.gas,
        account_overwrites: None,
    })
}

/// The outcome of one limit check of one direction, as a report records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitJudgement {
    pub check: CheckId,
    /// The component's id, as indexed.
    pub component: String,
    /// The direction, as [`Direction`] prints it.
    pub direction: String,
    /// What the check records when nothing failed: a pass, or why it did not run.
    pub unfailed: Outcome,
    /// Every failure, from simulation and from execution.
    pub failures: Vec<String>,
    /// The limit and its declaration.
    pub limit: LimitReport,
}

/// Judges the limit checks of every direction in `output`: `limits.at_limit`,
/// `limits.past_limit` and `limits.max_output`, in that order for each direction, then those of
/// the declared directions that were not checked: skipped for a zero limit, failed for one the
/// ladder never quoted, and not run for a component whose ladder failed.
///
/// `executed` holds the results of executing `output`'s inputs, or why the batch could not run.
/// A batch error fails `limits.at_limit` with that error; the swap past a hard or under-cap limit
/// then proves nothing, so its `limits.past_limit` is not run, while a soft limit's still passes
/// on its simulation and a capped one is still not run. With `None`, nothing was executed: the
/// checks are judged from the simulation alone, and those whose swap was queued are skipped
/// rather than passed.
pub fn judge_limits(
    output: &LadderOutput,
    executed: Option<Result<&HashMap<String, TychoExecutionResult>, &str>>,
    tolerances: &ExecutionTolerances,
) -> Vec<LimitJudgement> {
    let mut judgements = Vec::new();
    for record in &output.limit_checks {
        let direction = record.direction.to_string();
        let report = record.report();
        let judgement = |check: CheckId, unfailed: Outcome, failures: Vec<String>| LimitJudgement {
            check,
            component: record.component.clone(),
            direction: direction.clone(),
            unfailed,
            failures,
            limit: report.clone(),
        };
        let not_executed_here =
            || Outcome::Skipped { reason: "the swap is not executed at this block".to_string() };

        let mut at_limit_failures = record.failures_of(CheckId::LimitsAtLimit);
        if let (Some(executed), Some(id)) = (executed, record.at_limit_execution()) {
            match (executed, output.limit_executions.get(id)) {
                (Err(batch_error), _) => at_limit_failures.push(batch_error.to_string()),
                (Ok(results), Some(input)) => {
                    at_limit_failures.extend(execution_mismatch(id, input, results, tolerances))
                }
                (Ok(_), None) => {}
            }
        }
        let at_limit_unfailed = match (executed, record.at_limit_execution()) {
            (None, Some(_)) => not_executed_here(),
            (Some(_), _) | (None, None) => record.at_limit_unfailed(),
        };
        judgements.push(judgement(CheckId::LimitsAtLimit, at_limit_unfailed, at_limit_failures));

        // What happens past the limit proves the claim only if the swap at the limit executes.
        let at_limit_executed = matches!(
            (executed, record.at_limit_execution()),
            (Some(Ok(results)), Some(id))
                if matches!(results.get(id), Some(TychoExecutionResult::Success { .. }))
        );
        let mut past_limit_failures = record.failures_of(CheckId::LimitsPastLimit);
        if let (true, Some(Ok(results)), Some((id, claim))) =
            (at_limit_executed, executed, record.past_limit_execution())
        {
            if let Some(input) = output.limit_executions.get(id) {
                past_limit_failures.extend(past_limit_mismatch(id, input, results, *claim));
            }
        }
        let past_limit_unfailed = match (executed, record.past_limit_execution()) {
            (None, Some(_)) => not_executed_here(),
            (Some(_), _) | (None, None) => record.past_limit_unfailed(at_limit_executed),
        };
        judgements.push(judgement(
            CheckId::LimitsPastLimit,
            past_limit_unfailed,
            past_limit_failures,
        ));

        judgements.push(judgement(
            CheckId::LimitsMaxOutput,
            record.max_output_unfailed(),
            record.failures_of(CheckId::LimitsMaxOutput),
        ));
    }
    for UncheckedLimit { component, direction, limits, declared_by, why } in
        &output.unchecked_limits
    {
        let direction = direction.to_string();
        let (report, unfailed, failures) = match why {
            Unchecked::ZeroLimit { max_output } => (
                LimitReport::new(
                    limits,
                    *declared_by,
                    Some(&LimitSizes { max_input: &BigUint::zero(), max_output, capped: false }),
                ),
                Outcome::Skipped { reason: "the limit is zero at this block".to_string() },
                Vec::new(),
            ),
            Unchecked::NotQuoted => (
                LimitReport::new(limits, *declared_by, None),
                Outcome::Pass,
                vec![format!(
                    "{direction} declares a limit, but the ladder did not quote it: it is not one \
                     of the indexed component's directions, for example because the indexed tokens \
                     differ from the expected ones"
                )],
            ),
            Unchecked::LadderFailed => (
                LimitReport::new(limits, *declared_by, None),
                Outcome::NotRun { reason: format!("{} failed", CheckId::SimLadder) },
                Vec::new(),
            ),
        };
        for check in CheckId::LIMITS {
            judgements.push(LimitJudgement {
                check,
                component: component.clone(),
                direction: direction.clone(),
                unfailed: unfailed.clone(),
                failures: failures.clone(),
                limit: report.clone(),
            });
        }
    }
    judgements
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes},
    };

    use super::*;
    use crate::checks::execution::test_input as input;

    #[test]
    fn probe_sizes_are_the_ladder_then_the_limit() {
        let limit = BigUint::from(1_000_000u32);
        let amounts: Vec<(String, BigUint)> = ProbeSize::ALL
            .iter()
            .map(|size| (size.to_string(), size.amount(&limit)))
            .collect();
        assert_eq!(
            amounts,
            vec![
                ("0.1%".to_string(), BigUint::from(1_000u32)),
                ("1%".to_string(), BigUint::from(10_000u32)),
                ("10%".to_string(), BigUint::from(100_000u32)),
                ("limit".to_string(), limit),
            ]
        );
        // A fraction of a tiny limit rounds down to nothing.
        assert!(ProbeSize::Fraction(1)
            .amount(&BigUint::from(999u32))
            .is_zero());
    }

    #[test]
    fn execution_trades_are_sized_within_uniswap_v4s_int128_range() {
        let ceiling = int128_ceiling();

        assert_eq!(executable_max_input(&BigUint::from(1_000u32)).amount, BigUint::from(1_000u32));
        assert_eq!(executable_max_input(&ceiling).amount, ceiling);
        assert_eq!(executable_max_input(&(&ceiling + 1u32)).amount, ceiling);
        // The limit the deep Pons pool reports, some 12 orders of magnitude past the ceiling.
        let pons_limit = BigUint::from_str("1693513416259416009682992155640660564186233").unwrap();
        assert_eq!(executable_max_input(&pons_limit).amount, ceiling);
    }

    /// One hard-limit direction whose at-limit and past-limit inputs are ready to execute.
    fn output() -> LadderOutput {
        let mut output = LadderOutput::default();
        output
            .limit_executions
            .insert("at".to_string(), input(100, 50));
        output
            .limit_executions
            .insert("past".to_string(), input(101, 50));
        output
            .limit_checks
            .push(LimitCheckRecord {
                component: "0xaa".to_string(),
                direction: Direction::from_str("0x01->0x02").unwrap(),
                limits: Limits::Hard {},
                declared_by: DeclaredBy::Component,
                max_input: BigUint::from(100u32),
                max_output: BigUint::from(60u32),
                capped: false,
                failures: vec![(CheckId::LimitsMaxOutput, "pays past max output".to_string())],
                at_limit: AtLimit::Queued {
                    id: "at".to_string(),
                    past: Some(("past".to_string(), PastLimitClaim::Reverts)),
                },
            });
        output
    }

    fn success(amount_out: u32) -> TychoExecutionResult {
        TychoExecutionResult::Success {
            amount_out: BigUint::from(amount_out),
            gas_used: 0,
            state_overwrites: None,
            overwrite_metadata: None,
        }
    }

    fn revert() -> TychoExecutionResult {
        TychoExecutionResult::Revert {
            reason: "past the cap".to_string(),
            state_overwrites: None,
            overwrite_metadata: None,
        }
    }

    fn failures(judgements: &[LimitJudgement]) -> Vec<(CheckId, usize)> {
        judgements
            .iter()
            .map(|j| (j.check, j.failures.len()))
            .collect()
    }

    #[test]
    fn a_limit_that_executes_at_and_reverts_past_passes_both() {
        let results =
            HashMap::from([("at".to_string(), success(50)), ("past".to_string(), revert())]);
        let judged = judge_limits(&output(), Some(Ok(&results)), &ExecutionTolerances::exact());
        assert_eq!(
            failures(&judged),
            vec![
                (CheckId::LimitsAtLimit, 0),
                (CheckId::LimitsPastLimit, 0),
                (CheckId::LimitsMaxOutput, 1)
            ]
        );
        assert_eq!(judged[0].direction, "0x01->0x02");
    }

    #[test]
    fn a_settled_swap_past_a_hard_limit_and_a_mismatch_at_it_fail() {
        let results =
            HashMap::from([("at".to_string(), success(49)), ("past".to_string(), success(50))]);
        let judged = judge_limits(&output(), Some(Ok(&results)), &ExecutionTolerances::exact());
        assert_eq!(
            failures(&judged)[..2],
            [(CheckId::LimitsAtLimit, 1), (CheckId::LimitsPastLimit, 1)]
        );
    }

    #[test]
    fn a_failed_batch_fails_the_limit_and_leaves_past_it_not_run() {
        let judged =
            judge_limits(&output(), Some(Err("the node is down")), &ExecutionTolerances::exact());
        assert_eq!(judged[0].failures, vec!["the node is down".to_string()]);
        // Nothing executed at the limit, so nothing past it proves anything.
        assert!(judged[1].failures.is_empty(), "{:?}", judged[1].failures);
        assert_eq!(
            judged[1].unfailed,
            Outcome::NotRun { reason: "limits.at_limit failed".to_string() }
        );
        // The max output needs no execution, and keeps only its simulation failure.
        assert_eq!(judged[2].failures, vec!["pays past max output".to_string()]);
    }

    #[test]
    fn at_a_block_that_only_simulates_queued_swaps_are_skipped() {
        let simulated_only = judge_limits(&output(), None, &ExecutionTolerances::exact());
        let not_executed =
            Outcome::Skipped { reason: "the swap is not executed at this block".to_string() };
        assert_eq!(
            unfailed(&simulated_only)[..2],
            [
                (CheckId::LimitsAtLimit, not_executed.clone()),
                (CheckId::LimitsPastLimit, not_executed)
            ]
        );
        assert_eq!(
            failures(&simulated_only),
            vec![
                (CheckId::LimitsAtLimit, 0),
                (CheckId::LimitsPastLimit, 0),
                (CheckId::LimitsMaxOutput, 1)
            ]
        );
    }

    #[test]
    fn a_revert_past_the_limit_proves_nothing_when_the_limit_reverts_too() {
        let results = HashMap::from([("at".to_string(), revert()), ("past".to_string(), revert())]);
        let judged = judge_limits(&output(), Some(Ok(&results)), &ExecutionTolerances::exact());
        assert_eq!(judged[0].failures.len(), 1);
        assert!(judged[1].failures.is_empty(), "{:?}", judged[1].failures);
        assert_eq!(
            judged[1].unfailed,
            Outcome::NotRun { reason: "limits.at_limit failed".to_string() }
        );
    }

    #[test]
    fn a_judgement_names_the_component_and_direction() {
        let results =
            HashMap::from([("at".to_string(), success(50)), ("past".to_string(), revert())]);
        let judged = judge_limits(&output(), Some(Ok(&results)), &ExecutionTolerances::exact());
        assert!(judged
            .iter()
            .all(|j| j.component == "0xaa" && j.direction == "0x01->0x02"));
        assert!(judged[0].failures.is_empty() && judged[1].failures.is_empty());
    }

    #[test]
    fn declared_directions_without_a_check_are_skipped_or_failed() {
        let mut output = output();
        output.limit_checks.clear();
        let hard = Limits::Hard {};
        output.unchecked_limits = vec![
            UncheckedLimit {
                component: "0xAa".to_string(),
                direction: Direction::from_str("0x01->0x02").unwrap(),
                limits: hard.clone(),
                declared_by: DeclaredBy::PackageDefault,
                why: Unchecked::ZeroLimit { max_output: BigUint::from(7u32) },
            },
            UncheckedLimit {
                component: "0xAa".to_string(),
                direction: Direction::from_str("0x02->0x01").unwrap(),
                limits: hard.clone(),
                declared_by: DeclaredBy::PackageDefault,
                why: Unchecked::NotQuoted,
            },
        ];
        let judged = judge_limits(&output, None, &ExecutionTolerances::exact());
        let checks: Vec<CheckId> = judged.iter().map(|j| j.check).collect();
        assert_eq!(checks, [CheckId::LIMITS, CheckId::LIMITS].concat());
        assert!(judged
            .iter()
            .all(|j| j.component == "0xAa"));
        let zero_report = LimitReport::new(
            &hard,
            DeclaredBy::PackageDefault,
            Some(&LimitSizes {
                max_input: &BigUint::zero(),
                max_output: &BigUint::from(7u32),
                capped: false,
            }),
        );
        for j in &judged[..3] {
            assert_eq!(j.direction, "0x01->0x02");
            assert!(j.failures.is_empty());
            assert_eq!(
                j.unfailed,
                Outcome::Skipped { reason: "the limit is zero at this block".to_string() }
            );
            assert_eq!(j.limit, zero_report);
        }
        let unquoted_report = LimitReport::new(&hard, DeclaredBy::PackageDefault, None);
        for j in &judged[3..] {
            assert_eq!(j.direction, "0x02->0x01");
            assert!(j.failures[0].contains("the ladder did not quote it"), "{:?}", j.failures);
            assert_eq!(j.limit, unquoted_report);
        }
    }

    #[test]
    fn past_limit_passes_a_soft_limit_on_its_simulation_and_does_not_run_a_capped_one() {
        let results =
            HashMap::from([("at".to_string(), success(50)), ("past".to_string(), revert())]);
        let mut soft = with_queued_swaps(true, None);
        soft.limit_checks[0].limits = Limits::Soft { reason: "constant product".to_string() };
        soft.limit_checks[0].at_limit = AtLimit::Queued { id: "at".to_string(), past: None };
        assert_eq!(
            judge_limits(&soft, Some(Ok(&results)), &ExecutionTolerances::exact())[1].unfailed,
            Outcome::Pass
        );
        let mut capped = output();
        capped.limit_checks[0].capped = true;
        assert!(matches!(
            &judge_limits(&capped, Some(Ok(&results)), &ExecutionTolerances::exact())[1].unfailed,
            Outcome::NotRun { reason } if reason.contains("int128::MAX")
        ));
    }

    #[test]
    fn past_an_under_cap_limit_the_swap_must_execute() {
        let mut under_cap = output();
        under_cap.limit_checks[0].limits =
            Limits::UnderCap { reason: "keeper-set rate; gives up 2%".to_string() };
        under_cap.limit_checks[0].at_limit = AtLimit::Queued {
            id: "at".to_string(),
            past: Some(("past".to_string(), PastLimitClaim::Executes)),
        };
        let executed =
            HashMap::from([("at".to_string(), success(50)), ("past".to_string(), success(50))]);
        assert!(judge_limits(&under_cap, Some(Ok(&executed)), &ExecutionTolerances::exact())[1]
            .failures
            .is_empty());
        let reverted =
            HashMap::from([("at".to_string(), success(50)), ("past".to_string(), revert())]);
        let judged = judge_limits(&under_cap, Some(Ok(&reverted)), &ExecutionTolerances::exact());
        assert!(judged[1].failures[0].contains("declare it hard"), "{:?}", judged[1].failures);
        // Without the swap at the limit executed, what happens past it proves nothing.
        let at_reverted =
            HashMap::from([("at".to_string(), revert()), ("past".to_string(), success(50))]);
        let judged =
            judge_limits(&under_cap, Some(Ok(&at_reverted)), &ExecutionTolerances::exact());
        assert_eq!(judged[1].check, CheckId::LimitsPastLimit);
        assert!(judged[1].failures.is_empty(), "{:?}", judged[1].failures);
        assert_eq!(
            judged[1].unfailed,
            Outcome::NotRun { reason: "limits.at_limit failed".to_string() }
        );
    }

    #[test]
    fn a_judgement_reports_the_limit_and_its_declaration() {
        let results =
            HashMap::from([("at".to_string(), success(50)), ("past".to_string(), revert())]);
        let judged = judge_limits(&output(), Some(Ok(&results)), &ExecutionTolerances::exact());
        let expected = LimitReport::new(
            &Limits::Hard {},
            DeclaredBy::Component,
            Some(&LimitSizes {
                max_input: &BigUint::from(100u32),
                max_output: &BigUint::from(60u32),
                capped: false,
            }),
        );
        assert!(judged
            .iter()
            .all(|j| j.limit == expected));
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::json!({
                "kind": "hard",
                "declared_by": "component",
                "max_input": "100",
                "max_output": "60",
                "capped": false,
            })
        );
    }

    /// A uniswap v2 pool with a mixed-case id, as an update can carry it, and its component.
    fn mixed_case_pool() -> (Update, HashMap<String, ProtocolComponent>) {
        let id = "0xAbCd000000000000000000000000000000000001";
        let token = |address: &str, symbol: &str| {
            Token::new(&Bytes::from_str(address).unwrap(), symbol, 18, 0, &[], Chain::Ethereum, 100)
        };
        let tokens = vec![
            token("0x00000000000000000000000000000000000000a1", "A"),
            token("0x00000000000000000000000000000000000000b1", "B"),
        ];
        let component = ProtocolComponent::new(
            Bytes::from_str(id).unwrap(),
            "uniswap_v2".to_string(),
            "uniswap_v2_pool".to_string(),
            Chain::Ethereum,
            tokens,
            Vec::new(),
            HashMap::new(),
            Bytes::default(),
            Default::default(),
        );
        let reserve = alloy::primitives::aliases::U112::from(10u64)
            .pow(alloy::primitives::aliases::U112::from(24));
        let state: Box<dyn ProtocolSim> = Box::new(UniswapV2State::new(reserve, reserve));
        let components = HashMap::from([(id.to_string(), component)]);
        let update = Update::new(1, HashMap::from([(id.to_string(), state)]), components.clone());
        (update, components)
    }

    #[test]
    fn a_skip_applies_to_a_component_whatever_the_case_of_its_id() {
        let (update, components) = mixed_case_pool();
        let lowercase = "0xabcd000000000000000000000000000000000001".to_string();

        let skip = HashMap::from([(lowercase.clone(), "no adapter")]);
        let skipped = run_ladder(
            Chain::Ethereum,
            &update,
            &components,
            Skips { simulation: &skip, execution: &HashMap::new() },
            "uniswap_v2",
            None,
        );
        assert!(skipped.failure().is_none());
        assert!(skipped.executions.is_empty());

        // Skipping execution, with limits declared: the limits are checked in simulation, and
        // nothing is queued for execution.
        let soft =
            (Limits::Soft { reason: "constant product".to_string() }, DeclaredBy::PackageDefault);
        let kinds: DeclaredLimits = [
            "0x00000000000000000000000000000000000000a1->0x00000000000000000000000000000000000000b1",
            "0x00000000000000000000000000000000000000b1->0x00000000000000000000000000000000000000a1",
        ]
        .into_iter()
        .map(|direction| ((lowercase.clone(), Direction::from_str(direction).unwrap()), soft.clone()))
        .collect();
        let simulated_only = run_ladder(
            Chain::Ethereum,
            &update,
            &components,
            Skips { simulation: &HashMap::new(), execution: &skip },
            "uniswap_v2",
            Some(&kinds),
        );
        assert!(simulated_only.failure().is_none());
        assert!(simulated_only.executions.is_empty());
        assert!(simulated_only
            .limit_executions
            .is_empty());
        assert_eq!(simulated_only.limit_checks.len(), 2);
        assert!(simulated_only
            .limit_checks
            .iter()
            .all(|limits| matches!(
                &limits.at_limit,
                AtLimit::NotExecuted { reason } if reason == "no adapter"
            )));
    }

    #[test]
    fn a_limit_at_or_above_int128_max_is_the_execution_cap() {
        let ceiling = int128_ceiling();

        assert!(!executable_max_input(&(&ceiling - 1u32)).capped);
        // Exactly the ceiling: one unit past it reverts in the cast, not at the venue's bound.
        assert!(executable_max_input(&ceiling).capped);
        assert!(executable_max_input(&(&ceiling + 1u32)).capped);
    }

    /// The direction of [`output`], with no failure in simulation, quoted at the limit unless
    /// `quoted_at_limit` is false, and its swaps queued unless that or `execution_skipped`
    /// stops them.
    fn with_queued_swaps(quoted_at_limit: bool, execution_skipped: Option<&str>) -> LadderOutput {
        let mut output = output();
        let limits = &mut output.limit_checks[0];
        limits.failures.clear();
        match (quoted_at_limit, execution_skipped) {
            (false, _) => limits.at_limit = AtLimit::QuoteFailed,
            (true, Some(reason)) => {
                limits.at_limit = AtLimit::NotExecuted { reason: reason.to_string() }
            }
            (true, None) => {}
        }
        output
    }

    fn unfailed(judgements: &[LimitJudgement]) -> Vec<(CheckId, Outcome)> {
        judgements
            .iter()
            .map(|j| (j.check, j.unfailed.clone()))
            .collect()
    }

    #[test]
    fn limit_checks_pass_only_once_executed() {
        let results =
            HashMap::from([("at".to_string(), success(50)), ("past".to_string(), revert())]);
        let judged = judge_limits(
            &with_queued_swaps(true, None),
            Some(Ok(&results)),
            &ExecutionTolerances::exact(),
        );
        assert_eq!(
            unfailed(&judged),
            vec![
                (CheckId::LimitsAtLimit, Outcome::Pass),
                (CheckId::LimitsPastLimit, Outcome::Pass),
                (CheckId::LimitsMaxOutput, Outcome::Pass)
            ]
        );
    }

    #[test]
    fn limit_checks_of_a_component_that_skips_execution_are_skipped() {
        let output = with_queued_swaps(true, Some("no fork state"));
        let judged =
            judge_limits(&output, Some(Ok(&HashMap::new())), &ExecutionTolerances::exact());
        let skipped = |reason: &str| Outcome::Skipped { reason: reason.to_string() };
        assert_eq!(
            unfailed(&judged),
            vec![
                (
                    CheckId::LimitsAtLimit,
                    skipped("the quote at the limit is not executed: no fork state")
                ),
                (
                    CheckId::LimitsPastLimit,
                    skipped("the swap past the limit is not executed: no fork state")
                ),
                // The comparison runs in simulation alone.
                (CheckId::LimitsMaxOutput, Outcome::Pass)
            ]
        );
        assert!(judged
            .iter()
            .all(|j| j.failures.is_empty()));
    }

    #[test]
    fn checks_that_need_the_quote_at_the_limit_are_not_run_without_it() {
        let mut output = with_queued_swaps(false, None);
        // `check_direction` fails the check whose quote at the limit errors.
        output.limit_checks[0]
            .failures
            .push((CheckId::LimitsAtLimit, "does not quote".to_string()));
        let judged = judge_limits(&output, None, &ExecutionTolerances::exact());
        let not_run = Outcome::NotRun { reason: "limits.at_limit failed".to_string() };
        assert_eq!(judged[0].check, CheckId::LimitsAtLimit);
        assert_eq!(judged[0].failures, ["does not quote"]);
        assert_eq!(judged[1].unfailed, not_run);
        assert_eq!(judged[2].unfailed, not_run);

        // A soft limit too: twice the limit is never quoted when the limit itself does not quote.
        output.limit_checks[0].limits = Limits::Soft { reason: "r".to_string() };
        for executed in [None, Some(Ok(&HashMap::new()))] {
            let judged = judge_limits(&output, executed, &ExecutionTolerances::exact());
            assert_eq!(judged[1].check, CheckId::LimitsPastLimit);
            assert_eq!(judged[1].unfailed, not_run);
            assert_eq!(judged[2].unfailed, not_run);
        }
    }

    #[test]
    fn a_failed_ladder_reports_its_limits_under_the_id_as_indexed() {
        // The component is missing from `protocol_components`, so its ladder fails; its declared
        // limits are reported under the lowercase id every other row of it uses.
        let (update, _) = mixed_case_pool();
        let id = "0xabcd000000000000000000000000000000000001";
        let soft =
            (Limits::Soft { reason: "constant product".to_string() }, DeclaredBy::PackageDefault);
        let declared: DeclaredLimits = HashMap::from([(
            (
                id.to_string(),
                Direction::from_str(
                    "0x00000000000000000000000000000000000000a1->0x00000000000000000000000000000000000000b1",
                )
                .unwrap(),
            ),
            soft,
        )]);
        let output = run_ladder(
            Chain::Ethereum,
            &update,
            &HashMap::new(),
            Skips { simulation: &HashMap::new(), execution: &HashMap::new() },
            "uniswap_v2",
            Some(&declared),
        );
        assert!(output.failure().is_some());
        let judged = judge_limits(&output, None, &ExecutionTolerances::exact());
        assert_eq!(judged.len(), 3);
        for judgement in &judged {
            assert_eq!(judgement.component, id);
            assert_eq!(
                judgement.unfailed,
                Outcome::NotRun { reason: format!("{} failed", CheckId::SimLadder) }
            );
        }
    }
}
