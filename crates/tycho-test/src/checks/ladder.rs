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
    tycho_common::models::{token::Token, Chain},
};

use crate::{
    checks::{
        execution::{execution_mismatch, missing_revert},
        limits::{self, Quote},
        outcome::{CheckId, Outcome},
        sheet::LADDER_THOUSANDTHS,
    },
    execution::{
        encoding::{encode_swap, EXECUTOR_ADDRESS},
        models::{TychoExecutionInput, TychoExecutionResult},
    },
    expectations::{Direction, Limits, Tolerance},
};

/// What the ladder hands to execution.
#[derive(Default)]
pub struct LadderOutput {
    /// The ladder's quotes: each execution must match its simulation.
    pub executions: HashMap<String, TychoExecutionInput>,
    /// Strict: the quote at each direction's limit, executed like the ladder's.
    pub at_limit: HashMap<String, TychoExecutionInput>,
    /// Strict: one unit past each hard limit; each execution must revert.
    pub past_limit: HashMap<String, TychoExecutionInput>,
    /// Strict: every direction whose limit was checked.
    pub limit_directions: Vec<LimitDirection>,
    /// Strict: every declared direction whose limit was not checked, and why.
    pub unchecked_limits: Vec<UncheckedLimit>,
}

/// Strict: a declared direction of a simulated component whose limit was not checked.
pub struct UncheckedLimit {
    /// The component's id, as indexed.
    component: String,
    direction: Direction,
    /// Why not. A zero limit is a skip; a direction the ladder never reached is a failure.
    why: Unchecked,
}

/// Why a declared direction's limit was not checked.
enum Unchecked {
    /// `get_limits` reports zero: the venue does not trade this direction at this block.
    ZeroLimit,
    /// The ladder did not quote the direction, for example because the indexed tokens differ
    /// from the declared ones.
    NotQuoted,
}

/// Strict: the limit checks of one direction of one component, before execution.
///
/// Built by [`run_ladder`] and judged by [`judge_limits`], which looks its executions up in the
/// same [`LadderOutput`], so its fields stay private.
pub struct LimitDirection {
    /// The component's id, as indexed.
    component: String,
    direction: Direction,
    /// The declaration the direction is checked against.
    limits: Limits,
    /// Whether the limit is at or above `int128::MAX`, the cap the runner applies to every
    /// protocol's executions.
    capped: bool,
    /// Whether `max_input` quoted. The past-limit swap and the max-output comparison need it.
    quoted_at_limit: bool,
    /// Why the component skips execution, if it does: the limit checks then run in simulation
    /// only.
    execution_skipped: Option<String>,
    /// The checks that failed in simulation, with the reason.
    failures: Vec<(CheckId, String)>,
    /// The id of the direction's execution in [`LadderOutput::at_limit`], when it was queued.
    at_limit: Option<String>,
    /// The id of the direction's execution in [`LadderOutput::past_limit`], when it was queued.
    past_limit: Option<String>,
}

impl LimitDirection {
    /// The component's id, as indexed.
    pub fn component(&self) -> &str {
        &self.component
    }

    /// The direction the limit is of.
    pub fn direction(&self) -> &Direction {
        &self.direction
    }

    fn failures_of(&self, check: CheckId) -> Vec<String> {
        self.failures
            .iter()
            .filter(|(id, _)| *id == check)
            .map(|(_, reason)| reason.clone())
            .collect()
    }

    /// What `limits.at_limit` records when nothing failed: a pass once the quote at the limit
    /// was executed, a skip when the component skips execution. Not run otherwise, which only a
    /// failed quote leads to, and that is recorded as a failure first.
    fn at_limit_unfailed(&self) -> Outcome {
        match (&self.at_limit, &self.execution_skipped) {
            (Some(_), _) => Outcome::Pass,
            (None, Some(reason)) => Outcome::Skipped {
                reason: format!("the quote at the limit is not executed: {reason}"),
            },
            (None, None) => Outcome::NotRun { reason: "the limit did not quote".to_string() },
        }
    }

    /// What `limits.past_limit` records when nothing failed.
    ///
    /// - Skipped when the check does not apply: a soft or under-cap limit, with the declared
    ///   reason, or a limit at the `int128` cap.
    /// - Not run when the swap at the limit did not quote or did not execute: the past-limit swap
    ///   takes its minimum output from that quote, and a revert past a limit that does not execute
    ///   either proves nothing.
    /// - Skipped when the component skips execution.
    /// - A pass once the swap past the limit was executed, and reverted.
    fn past_limit_unfailed(&self, at_limit_executed: bool) -> Outcome {
        if let Some(reason) = self.limits.past_limit_skip_reason() {
            return Outcome::Skipped { reason };
        }
        if self.capped {
            return Outcome::Skipped {
                reason: "the limit is at the int128::MAX cap, which is not the venue's bound"
                    .to_string(),
            };
        }
        let not_run = || Outcome::NotRun { reason: format!("{} failed", CheckId::LimitsAtLimit) };
        match (&self.past_limit, self.quoted_at_limit, &self.execution_skipped) {
            (Some(_), _, _) if at_limit_executed => Outcome::Pass,
            (Some(_), _, _) | (None, false, _) => not_run(),
            (None, true, Some(reason)) => Outcome::Skipped {
                reason: format!("the swap past the limit is not executed: {reason}"),
            },
            // Only a quote past the limit leads here, and that is recorded as a failure first.
            (None, true, None) => {
                Outcome::NotRun { reason: "the swap past the limit was not queued".to_string() }
            }
        }
    }

    /// What `limits.max_output` records when nothing failed: not run when the limit did not
    /// quote, since there is nothing to compare, and a pass otherwise.
    fn max_output_unfailed(&self) -> Outcome {
        if self.quoted_at_limit {
            Outcome::Pass
        } else {
            Outcome::NotRun { reason: format!("{} failed", CheckId::LimitsAtLimit) }
        }
    }
}

/// The largest input amount a swap can actually be executed with, given `limit`.
///
/// Uniswap V4 settles a swap through a `BalanceDelta` of two `int128`s, and the executor's
/// `swapExactInputSingle` takes the amount as a `uint128` that v4-core casts to `int128`, so no
/// amount above `int128::MAX` can be executed however deep the pool is. A limit above that
/// ceiling is not executable, and the percentages the harness trades at must stay distinct sizes,
/// so the cap belongs on the limit itself rather than on each amount derived from it.
///
/// Returns `limit` unchanged when it is at or below the ceiling.
pub fn executable_max_input(limit: &BigUint) -> BigUint {
    let ceiling = int128_ceiling();
    if *limit > ceiling {
        ceiling
    } else {
        limit.clone()
    }
}

/// `int128::MAX`, the cap the runner applies to every protocol's executions (see
/// [`executable_max_input`]).
fn int128_ceiling() -> BigUint {
    BigUint::from(i128::MAX as u128)
}

/// Whether `limit` is at or above `int128::MAX`. Such a limit is the execution cap, not the venue's
/// bound: even at exactly `int128::MAX`, one unit past it reverts in the executor's cast, whatever
/// the venue allows.
fn at_execution_cap(limit: &BigUint) -> bool {
    *limit >= int128_ceiling()
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
/// hard limit, one unit past it; a declared direction it passes over or never reaches is listed
/// in [`LadderOutput::unchecked_limits`]. `skips` and `declared_limits` are looked up by the
/// lowercase id, whatever case the update carries.
///
/// # Errors
/// On the first component that fails, which drops the rest of the update:
/// - a component of the update is missing from `protocol_components`;
/// - `get_limits`, `spot_price` or `get_amount_out` errors;
/// - a ladder amount rounds to zero;
/// - every direction of a component has a zero limit;
/// - a swap cannot be encoded.
pub fn run_ladder(
    chain: Chain,
    update: &Update,
    protocol_components: &HashMap<String, ProtocolComponent>,
    skips: Skips<'_>,
    protocol_system: &str,
    declared_limits: Option<&HashMap<(String, Direction), Limits>>,
) -> miette::Result<LadderOutput> {
    let mut output = LadderOutput::default();

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
        let mut reached = HashSet::new();
        let component = protocol_components
            .get(id)
            .ok_or_else(|| miette!("Couldn't find protocol component {id}"))?;

        let tokens = component.tokens.clone();

        // Test all permutations of swap directions
        let swap_directions: Vec<_> = tokens
            .iter()
            .permutations(2)
            .map(|perm| (perm[0], perm[1]))
            .collect();

        let mut quoted_any_direction = false;

        for (token_in, token_out) in &swap_directions {
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

            let executable_input = executable_max_input(&max_input);
            let capped = at_execution_cap(&max_input);
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
            // failing the component; the guard below still requires that at least one
            // direction was exercised.
            let direction = Direction {
                token_in: token_in.address.clone(),
                token_out: token_out.address.clone(),
            };
            reached.insert(direction.clone());
            if max_input.is_zero() {
                warn!(
                    "[{}] Zero limit for {} -> {}, skipping direction",
                    id, token_in.symbol, token_out.symbol
                );
                if declared_limits.is_some_and(|declared| {
                    declared.contains_key(&(key.clone(), direction.clone()))
                }) {
                    output
                        .unchecked_limits
                        .push(UncheckedLimit {
                            component: id.clone(),
                            direction,
                            why: Unchecked::ZeroLimit,
                        });
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
            for thousandths in LADDER_THOUSANDTHS {
                let percentage = f64::from(thousandths) / 10.0;
                let amount_in = &max_input * thousandths / 1000u32;
                if amount_in.is_zero() {
                    return Err(miette!(
                        "Amount in at {percentage}% of the limit is zero for pool {id}."
                    ));
                }

                let amount_out_result = state
                        .get_amount_out(amount_in.clone(), token_in, token_out)
                        .into_diagnostic()
                        .wrap_err(format!(
                            "Error calculating amount out for Pool {id:?} at {percentage:.1}% with input of {amount_in} {}.",
                            token_in.symbol,
                        ))?;

                info!(
                        "[{}] Simulated amount out for trading {:.1}% of max: ({} {} -> {} {}) (gas: {})",
                        id,
                        percentage,
                        amount_in,
                        token_in.symbol,
                        amount_out_result.amount,
                        token_out.symbol,
                        amount_out_result.gas
                    );

                quoted_any_direction = true;

                if skips.execution.contains_key(&key) {
                    continue;
                }

                let simulation_id = format!(
                    "test_{}_{}_{}_{}",
                    component.id, token_in.symbol, token_out.symbol, amount_in
                );
                let input = execution_input(
                    chain,
                    component,
                    token_in,
                    token_out,
                    amount_in.clone(),
                    &Quote {
                        amount: amount_out_result.amount.clone(),
                        gas: amount_out_result.gas.clone(),
                    },
                    protocol_system,
                )?;
                output
                    .executions
                    .insert(simulation_id, input);
            }

            let declared = declared_limits
                .and_then(|declared| declared.get(&(key.clone(), direction.clone())));
            if let Some(declared) = declared {
                let checked = limits::check_direction(
                    |amount| {
                        state
                            .get_amount_out(amount, token_in, token_out)
                            .map(|quote| Quote { amount: quote.amount, gas: quote.gas })
                            .map_err(|e| e.to_string())
                    },
                    &format!("{} -> {}", token_in.symbol, token_out.symbol),
                    &max_input,
                    &max_output,
                    capped,
                    declared.kind,
                );
                if let Some(rejection) = &checked.past_limit_rejection {
                    info!(
                        "[{id}] {} -> {}: the quote one unit past the limit errors: {rejection}",
                        token_in.symbol, token_out.symbol
                    );
                }
                let mut limits = LimitDirection {
                    component: id.clone(),
                    direction,
                    limits: declared.clone(),
                    capped,
                    quoted_at_limit: checked.at_limit.is_some(),
                    execution_skipped: skips
                        .execution
                        .get(&key)
                        .map(|reason| reason.to_string()),
                    failures: checked.failures,
                    at_limit: None,
                    past_limit: None,
                };
                if let (Some(quote), false) =
                    (&checked.at_limit, skips.execution.contains_key(&key))
                {
                    let simulation_id = format!("at_limit_{id}_{}", limits.direction);
                    output.at_limit.insert(
                        simulation_id.clone(),
                        execution_input(
                            chain,
                            component,
                            token_in,
                            token_out,
                            max_input.clone(),
                            quote,
                            protocol_system,
                        )?,
                    );
                    limits.at_limit = Some(simulation_id);
                    // The swap's minimum output is set from the quote at the limit, so a venue
                    // that accepts the extra unit settles, and only a revert passes.
                    if let Some(past) = checked.past_limit {
                        let simulation_id = format!("past_limit_{id}_{}", limits.direction);
                        output.past_limit.insert(
                            simulation_id.clone(),
                            execution_input(
                                chain,
                                component,
                                token_in,
                                token_out,
                                past,
                                quote,
                                protocol_system,
                            )?,
                        );
                        limits.past_limit = Some(simulation_id);
                    }
                }
                output.limit_directions.push(limits);
            }
        }

        // A declared direction the loop above never reached has no limit result. Fail it rather
        // than let it drop out of the report.
        if let Some(declared) = declared_limits {
            for (declared_id, direction) in declared.keys().sorted() {
                if *declared_id == key && !reached.contains(direction) {
                    output
                        .unchecked_limits
                        .push(UncheckedLimit {
                            component: id.clone(),
                            direction: direction.clone(),
                            why: Unchecked::NotQuoted,
                        });
                }
            }
        }

        if !quoted_any_direction {
            return Err(miette!(
                "No tradable direction for pool {id}: every swap direction reported a zero limit."
            ));
        }
    }

    Ok(output)
}

/// Encodes the router swap that executes `quote` for `amount_in`.
fn execution_input(
    chain: Chain,
    component: &ProtocolComponent,
    token_in: &Token,
    token_out: &Token,
    amount_in: BigUint,
    quote: &Quote,
    protocol_system: &str,
) -> miette::Result<TychoExecutionInput> {
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
        expected_amount_out: quote.amount.clone(),
        protocol_system: protocol_system.to_string(),
        component_id: component.id.to_string(),
        token_in: token_in.symbol.clone(),
        token_out: token_out.symbol.clone(),
        estimated_gas: quote.gas.clone(),
        account_overwrites: None,
    })
}

/// The results of executing a ladder's swaps, by simulation id.
type Results = HashMap<String, TychoExecutionResult>;

/// Why one execution fails a limit check, if it does.
type Judge<'a> = dyn Fn(&str, &TychoExecutionInput, &Results) -> Option<String> + 'a;

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
}

/// Judges the limit checks of every direction in `output`: `limits.at_limit`,
/// `limits.past_limit` and `limits.max_output`, in that order for each direction, then those of
/// the declared directions that were not checked: skipped for a zero limit, failed for one the
/// ladder never quoted.
///
/// `executed` holds the results of executing `output`'s inputs, or why the batch could not run,
/// which fails every execution a check needs. With `None`, nothing was executed, as at a block
/// that only simulates: the checks are judged from the simulation alone, and those whose swap
/// was queued are skipped rather than passed.
pub fn judge_limits(
    output: &LadderOutput,
    executed: Option<Result<&HashMap<String, TychoExecutionResult>, &str>>,
    tolerance: Tolerance,
) -> Vec<LimitJudgement> {
    let execution_failure = |id: &Option<String>, judge: &Judge<'_>| {
        let executed = executed?;
        let id = id.as_ref()?;
        let input = output
            .at_limit
            .get(id)
            .or_else(|| output.past_limit.get(id))?;
        match executed {
            Err(batch_error) => Some(batch_error.to_string()),
            Ok(results) => judge(id, input, results),
        }
    };

    let mut judgements = Vec::new();
    for limits in &output.limit_directions {
        let direction = limits.direction.to_string();
        let judgement = |check: CheckId, unfailed: Outcome, failures: Vec<String>| LimitJudgement {
            check,
            component: limits.component.clone(),
            direction: direction.clone(),
            unfailed,
            failures,
        };

        let not_executed_here =
            || Outcome::Skipped { reason: "the swap is not executed at this block".to_string() };

        let mut at_limit_failures = limits.failures_of(CheckId::LimitsAtLimit);
        at_limit_failures.extend(execution_failure(&limits.at_limit, &|id, input, results| {
            execution_mismatch(id, input, results, tolerance)
        }));
        let at_limit_unfailed = match (executed, &limits.at_limit) {
            (None, Some(_)) => not_executed_here(),
            (Some(_), _) | (None, None) => limits.at_limit_unfailed(),
        };
        judgements.push(judgement(CheckId::LimitsAtLimit, at_limit_unfailed, at_limit_failures));

        // A revert past the limit proves the bound only if the swap at the limit executes.
        let at_limit_executed = matches!(
            (executed, &limits.at_limit),
            (Some(Ok(results)), Some(id))
                if matches!(results.get(id), Some(TychoExecutionResult::Success { .. }))
        );
        let mut past_limit_failures = limits.failures_of(CheckId::LimitsPastLimit);
        let batch_failed = matches!(executed, Some(Err(_)));
        if at_limit_executed || batch_failed {
            past_limit_failures.extend(execution_failure(&limits.past_limit, &missing_revert));
        }
        let past_limit_unfailed = match (executed, &limits.past_limit) {
            (None, Some(_)) => not_executed_here(),
            (Some(_), _) | (None, None) => limits.past_limit_unfailed(at_limit_executed),
        };
        judgements.push(judgement(
            CheckId::LimitsPastLimit,
            past_limit_unfailed,
            past_limit_failures,
        ));

        judgements.push(judgement(
            CheckId::LimitsMaxOutput,
            limits.max_output_unfailed(),
            limits.failures_of(CheckId::LimitsMaxOutput),
        ));
    }
    for UncheckedLimit { component, direction, why } in &output.unchecked_limits {
        let direction = direction.to_string();
        for check in CheckId::LIMITS {
            let (unfailed, failures) = match why {
                Unchecked::ZeroLimit => (
                    Outcome::Skipped { reason: "the limit is zero at this block".to_string() },
                    Vec::new(),
                ),
                Unchecked::NotQuoted => (
                    Outcome::Pass,
                    vec![format!(
                        "{direction} declares a limit, but the ladder did not quote it: the \
                         indexed tokens differ from the expected ones"
                    )],
                ),
            };
            judgements.push(LimitJudgement {
                check,
                component: component.clone(),
                direction: direction.clone(),
                unfailed,
                failures,
            });
        }
    }
    judgements
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_execution::encoding::models::Solution;
    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_common::{simulation::protocol_sim::ProtocolSim, Bytes},
    };

    use super::*;
    use crate::{execution::models::Transaction, expectations::LimitKind};

    #[test]
    fn execution_trades_are_sized_within_uniswap_v4s_int128_range() {
        let ceiling = BigUint::from(i128::MAX as u128);

        assert_eq!(executable_max_input(&BigUint::from(1_000u32)), BigUint::from(1_000u32));
        assert_eq!(executable_max_input(&ceiling), ceiling);
        assert_eq!(executable_max_input(&(&ceiling + 1u32)), ceiling);
        // The limit the deep Pons pool reports, some 12 orders of magnitude past the ceiling.
        let pons_limit = BigUint::from_str("1693513416259416009682992155640660564186233").unwrap();
        assert_eq!(executable_max_input(&pons_limit), ceiling);
    }

    fn input(amount_in: u32, expected_amount_out: u32) -> TychoExecutionInput {
        TychoExecutionInput {
            solution: Solution::new(
                Bytes::default(),
                Bytes::default(),
                Bytes::from("0x01"),
                Bytes::from("0x02"),
                BigUint::from(amount_in),
                BigUint::from(expected_amount_out),
                BigUint::from(expected_amount_out),
                Vec::new(),
            ),
            transaction: Transaction::new(
                Bytes::default(),
                BigUint::zero(),
                Vec::new(),
                BigUint::zero(),
            ),
            expected_amount_out: BigUint::from(expected_amount_out),
            protocol_system: "vault".to_string(),
            component_id: "0xaa".to_string(),
            token_in: "A".to_string(),
            token_out: "B".to_string(),
            estimated_gas: BigUint::zero(),
            account_overwrites: None,
        }
    }

    /// One hard-limit direction whose at-limit and past-limit inputs are ready to execute.
    fn output() -> LadderOutput {
        let mut output = LadderOutput::default();
        output
            .at_limit
            .insert("at".to_string(), input(100, 50));
        output
            .past_limit
            .insert("past".to_string(), input(101, 50));
        output
            .limit_directions
            .push(LimitDirection {
                component: "0xaa".to_string(),
                direction: Direction::from_str("0x01->0x02").unwrap(),
                limits: Limits { kind: LimitKind::Hard, reason: None, lost_capacity: None },
                capped: false,
                quoted_at_limit: true,
                execution_skipped: None,
                failures: vec![(CheckId::LimitsMaxOutput, "pays past max output".to_string())],
                at_limit: Some("at".to_string()),
                past_limit: Some("past".to_string()),
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
        let judged = judge_limits(&output(), Some(Ok(&results)), Tolerance::EXACT);
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
        let judged = judge_limits(&output(), Some(Ok(&results)), Tolerance::EXACT);
        assert_eq!(
            failures(&judged)[..2],
            [(CheckId::LimitsAtLimit, 1), (CheckId::LimitsPastLimit, 1)]
        );
    }

    #[test]
    fn a_failed_batch_fails_every_execution_a_check_needs() {
        let judged = judge_limits(&output(), Some(Err("the node is down")), Tolerance::EXACT);
        assert_eq!(judged[0].failures, vec!["the node is down".to_string()]);
        assert_eq!(judged[1].failures, vec!["the node is down".to_string()]);
        // The max output needs no execution, and keeps only its simulation failure.
        assert_eq!(judged[2].failures, vec!["pays past max output".to_string()]);
    }

    #[test]
    fn at_a_block_that_only_simulates_queued_swaps_are_skipped() {
        let simulated_only = judge_limits(&output(), None, Tolerance::EXACT);
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
        let judged = judge_limits(&output(), Some(Ok(&results)), Tolerance::EXACT);
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
        let judged = judge_limits(&output(), Some(Ok(&results)), Tolerance::EXACT);
        assert!(judged
            .iter()
            .all(|j| j.component == "0xaa" && j.direction == "0x01->0x02"));
        assert!(judged[0].failures.is_empty() && judged[1].failures.is_empty());
    }

    #[test]
    fn declared_directions_without_a_check_are_skipped_or_failed() {
        let mut output = output();
        output.limit_directions.clear();
        output.unchecked_limits = vec![
            UncheckedLimit {
                component: "0xaa".to_string(),
                direction: Direction::from_str("0x01->0x02").unwrap(),
                why: Unchecked::ZeroLimit,
            },
            UncheckedLimit {
                component: "0xaa".to_string(),
                direction: Direction::from_str("0x02->0x01").unwrap(),
                why: Unchecked::NotQuoted,
            },
        ];
        let judged = judge_limits(&output, None, Tolerance::EXACT);
        assert_eq!(judged.len(), 6);
        assert!(judged[..3]
            .iter()
            .all(|j| j.failures.is_empty() &&
                j.unfailed ==
                    Outcome::Skipped {
                        reason: "the limit is zero at this block".to_string()
                    }));
        assert!(judged[3..]
            .iter()
            .all(|j| j.failures[0].contains("the ladder did not quote it")));
    }

    #[test]
    fn past_limit_does_not_apply_to_a_soft_or_capped_limit() {
        let results =
            HashMap::from([("at".to_string(), success(50)), ("past".to_string(), revert())]);
        let mut soft = output();
        soft.limit_directions[0].limits = Limits {
            kind: LimitKind::Soft,
            reason: Some("constant product".to_string()),
            lost_capacity: None,
        };
        assert_eq!(
            judge_limits(&soft, Some(Ok(&results)), Tolerance::EXACT)[1].unfailed,
            Outcome::Skipped { reason: "the limit is declared soft: constant product".to_string() }
        );
        let mut under_cap = output();
        under_cap.limit_directions[0].limits = Limits {
            kind: LimitKind::UnderCap,
            reason: Some("keeper-set rate".to_string()),
            lost_capacity: Some("2%".to_string()),
        };
        assert_eq!(
            judge_limits(&under_cap, Some(Ok(&results)), Tolerance::EXACT)[1].unfailed,
            Outcome::Skipped {
                reason: "the limit is declared under_cap: keeper-set rate; it gives up 2%"
                    .to_string()
            }
        );
        let mut capped = output();
        capped.limit_directions[0].capped = true;
        assert!(matches!(
            &judge_limits(&capped, Some(Ok(&results)), Tolerance::EXACT)[1].unfailed,
            Outcome::Skipped { reason } if reason.contains("int128::MAX")
        ));
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
        let reserve = alloy::primitives::U256::from(10u64).pow(alloy::primitives::U256::from(24));
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
        )
        .unwrap();
        assert!(skipped.executions.is_empty());

        // Skipping execution, with limits declared: the limits are checked in simulation, and
        // nothing is queued for execution.
        let soft = Limits {
            kind: LimitKind::Soft,
            reason: Some("constant product".to_string()),
            lost_capacity: None,
        };
        let kinds: HashMap<(String, Direction), Limits> = [
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
        )
        .unwrap();
        assert!(simulated_only.executions.is_empty());
        assert!(simulated_only.at_limit.is_empty());
        assert_eq!(simulated_only.limit_directions.len(), 2);
        assert!(simulated_only
            .limit_directions
            .iter()
            .all(|limits| limits.execution_skipped.as_deref() == Some("no adapter")));
    }

    #[test]
    fn a_limit_at_or_above_int128_max_is_the_execution_cap() {
        let ceiling = BigUint::from(i128::MAX as u128);

        assert!(!at_execution_cap(&(&ceiling - 1u32)));
        // Exactly the ceiling: one unit past it reverts in the cast, not at the venue's bound.
        assert!(at_execution_cap(&ceiling));
        assert!(at_execution_cap(&(&ceiling + 1u32)));
    }

    /// The direction of [`output`], with no failure in simulation, quoted at the limit unless
    /// `quoted_at_limit` is false, and its swaps queued unless that or `execution_skipped`
    /// stops them.
    fn with_queued_swaps(quoted_at_limit: bool, execution_skipped: Option<&str>) -> LadderOutput {
        let mut output = output();
        let executed = quoted_at_limit && execution_skipped.is_none();
        let limits = &mut output.limit_directions[0];
        limits.failures.clear();
        limits.quoted_at_limit = quoted_at_limit;
        limits.execution_skipped = execution_skipped.map(str::to_string);
        if !executed {
            limits.at_limit = None;
            limits.past_limit = None;
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
        let judged =
            judge_limits(&with_queued_swaps(true, None), Some(Ok(&results)), Tolerance::EXACT);
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
        let judged = judge_limits(&output, Some(Ok(&HashMap::new())), Tolerance::EXACT);
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
    }

    #[test]
    fn checks_that_need_the_quote_at_the_limit_are_not_run_without_it() {
        let judged = judge_limits(&with_queued_swaps(false, None), None, Tolerance::EXACT);
        let not_run = Outcome::NotRun { reason: "limits.at_limit failed".to_string() };
        assert_eq!(judged[1].unfailed, not_run);
        assert_eq!(judged[2].unfailed, not_run);
    }
}
