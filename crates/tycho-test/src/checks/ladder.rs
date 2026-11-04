//! The quote ladder: for every direction of a component, its limits, spot price and quotes at
//! 0.1%, 1% and 10% of its limit, and, where the package declares the limit, the checks at and
//! past it. The ladder encodes each quote as a router swap; the runner executes them and judges
//! the results.

use std::collections::HashMap;

use itertools::Itertools;
use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use num_traits::{FromPrimitive, Zero};
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
    },
    execution::{
        encoding::{encode_swap, EXECUTOR_ADDRESS},
        models::{TychoExecutionInput, TychoExecutionResult},
    },
    expectations::{Direction, LimitKind, Tolerance},
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
}

/// Strict: the limit checks of one direction of one component, before execution.
pub struct LimitDirection {
    pub component: String,
    pub direction: Direction,
    pub kind: LimitKind,
    /// Whether the limit is at or above `int128::MAX`, the execution cap.
    pub capped: bool,
    /// Whether `max_input` quoted. The past-limit swap and the max-output comparison need it.
    pub quoted_at_limit: bool,
    /// Why the component skips execution, if it does: the limit checks then run in simulation
    /// only.
    pub execution_skipped: Option<String>,
    /// The checks that failed in simulation, with the reason.
    pub failures: Vec<(CheckId, String)>,
    /// The ids of the direction's executions in [`LadderOutput::at_limit`] and
    /// [`LadderOutput::past_limit`].
    pub at_limit: Option<String>,
    pub past_limit: Option<String>,
}

impl LimitDirection {
    pub fn failures_of(&self, check: CheckId) -> Vec<String> {
        self.failures
            .iter()
            .filter(|(id, _)| *id == check)
            .map(|(_, reason)| reason.clone())
            .collect()
    }

    /// What `limits.at_limit` records when nothing failed: a pass once the quote at the limit
    /// was executed, a skip when the component skips execution.
    pub fn at_limit_unfailed(&self) -> Outcome {
        match (&self.at_limit, &self.execution_skipped) {
            (None, Some(reason)) => Outcome::Skipped {
                reason: format!("the quote at the limit is not executed: {reason}"),
            },
            (Some(_), _) | (None, None) => Outcome::Pass,
        }
    }

    /// What `limits.past_limit` records when nothing failed: a skip when the check does not apply
    /// to the limit or the component skips execution, not run when the limit did not quote, since
    /// the past-limit swap takes its minimum output from that quote, and a pass once the swap
    /// was executed.
    pub fn past_limit_unfailed(&self) -> Outcome {
        let skipped = |reason: &str| Outcome::Skipped { reason: reason.to_string() };
        match self.kind {
            LimitKind::Soft => return skipped("the limit is declared soft"),
            LimitKind::UnderCap => return skipped("the limit is declared under_cap"),
            LimitKind::Hard if self.capped => {
                return skipped(
                    "the limit is at the int128::MAX cap, which is not the venue's bound",
                )
            }
            LimitKind::Hard => {}
        }
        match (&self.past_limit, self.quoted_at_limit, &self.execution_skipped) {
            (Some(_), _, _) => Outcome::Pass,
            (None, false, _) => {
                Outcome::NotRun { reason: format!("{} failed", CheckId::LimitsAtLimit) }
            }
            (None, true, Some(reason)) => Outcome::Skipped {
                reason: format!("the swap past the limit is not executed: {reason}"),
            },
            (None, true, None) => Outcome::Pass,
        }
    }

    /// What `limits.max_output` records when nothing failed: not run when the limit did not
    /// quote, since there is nothing to compare, and a pass otherwise.
    pub fn max_output_unfailed(&self) -> Outcome {
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

/// `int128::MAX`, the largest amount a swap can be executed with.
fn int128_ceiling() -> BigUint {
    BigUint::from(i128::MAX as u128)
}

/// Whether `limit` is at or above `int128::MAX`. Such a limit is the execution cap, not the venue's
/// bound: even at exactly `int128::MAX`, one unit past it reverts in the executor's cast, whatever
/// the venue allows.
pub fn at_execution_cap(limit: &BigUint) -> bool {
    *limit >= int128_ceiling()
}

/// Runs simulations for all protocol components and swap directions.
///
/// This method performs comprehensive simulation testing on protocol components by:
/// 1. Computing spot prices for all token pairs
/// 2. Simulating swaps with different input amounts (0.1%, 1%, 10% of limits)
/// 3. Testing all possible swap directions between tokens
/// 4. Preparing execution data for each simulation
///
/// The simulation uses the Tycho SDK to calculate expected outputs and prepare
/// transaction data for subsequent execution testing.
///
/// # Arguments
/// * `chain` - The chain the swaps are encoded for
/// * `update` - The decoded protocol state containing all component data
/// * `protocol_components` - Mapping of component IDs to their ProtocolComponent models
/// * `skip_simulation`, `skip_execution` - The lowercase ids of the components that skip simulation
///   or execution, each with its reason
/// * `protocol_system` - The protocol system identifier
/// * `limit_kinds` - Strict mode: the kind of limit each component declares
///
/// # Returns
/// The ladder's quotes, keyed by simulation id, for execution. With `limit_kinds` (strict
/// mode, one kind per component id) also each direction's limit checks, the quote at the limit,
/// and for a hard limit the input one unit past it, all for execution as well.
///
/// # Errors
/// Returns an error if:
/// - Spot price calculation fails for any component
/// - Simulation fails to calculate amount out
/// - Transaction encoding fails
pub fn run_ladder(
    chain: Chain,
    update: &Update,
    protocol_components: &HashMap<String, ProtocolComponent>,
    skip_simulation: &HashMap<String, &str>,
    skip_execution: &HashMap<String, &str>,
    protocol_system: &str,
    limit_kinds: Option<&HashMap<(String, Direction), LimitKind>>,
) -> miette::Result<LadderOutput> {
    let mut output = LadderOutput::default();

    for (id, state) in update.states.iter() {
        if let Some(reason) = skip_simulation.get(id) {
            info!("Skipping simulation for component {id}: {reason}");
            continue;
        }
        let component = protocol_components
            .get(id)
            .ok_or_else(|| miette!("Couldn't find protocol component {id}"))?;

        let tokens = component.tokens.clone();

        // Test get_amount_out with different percentages of limits. The reserves or limits
        // are relevant because we need to know how much to test with. We
        // don't know if a pool is going to revert with 10 or 10 million
        // USDC, for example, so by using the limits we can use "safe
        // values" where the sim shouldn't break. We then retrieve the
        // amount out for 0.1%, 1% and 10%.
        let percentages = [0.001, 0.01, 0.1];

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
            if capped {
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
            if max_input.is_zero() {
                warn!(
                    "[{}] Zero limit for {} -> {}, skipping direction",
                    id, token_in.symbol, token_out.symbol
                );
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

            for percentage in percentages.iter() {
                // For precision, multiply by 1000 then divide by 1000
                let percentage_biguint =
                    BigUint::from_f64(percentage * 1000.0).ok_or_else(|| {
                        miette!("[{id}] Percentage {percentage} is not a non-negative number")
                    })?;
                let thousand = BigUint::from(1000u32);
                let amount_in = (&max_input * &percentage_biguint) / &thousand;

                // Skip if amount is zero
                if amount_in.is_zero() {
                    return Err(miette!(
                        "Amount in multiplied by percentage {percentage} is zero for pool {id}."
                    ));
                }

                let amount_out_result = state
                        .get_amount_out(amount_in.clone(), token_in, token_out)
                        .into_diagnostic()
                        .wrap_err(format!(
                            "Error calculating amount out for Pool {id:?} at {:.1}% with input of {amount_in} {}.",
                            percentage * 100.0,
                            token_in.symbol,
                        ))?;

                info!(
                        "[{}] Simulated amount out for trading {:.1}% of max: ({} {} -> {} {}) (gas: {})",
                        id,
                        percentage * 100.0,
                        amount_in,
                        token_in.symbol,
                        amount_out_result.amount,
                        token_out.symbol,
                        amount_out_result.gas
                    );

                quoted_any_direction = true;

                if let Some(reason) = skip_execution.get(id) {
                    info!("Skipping execution for component {id}: {reason}");
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

            let direction = Direction {
                token_in: token_in.address.clone(),
                token_out: token_out.address.clone(),
            };
            let kind = limit_kinds.and_then(|kinds| kinds.get(&(id.clone(), direction.clone())));
            if let Some(kind) = kind {
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
                    *kind,
                );
                let mut limits = LimitDirection {
                    component: id.clone(),
                    direction,
                    kind: *kind,
                    capped,
                    quoted_at_limit: checked.at_limit.is_some(),
                    execution_skipped: skip_execution
                        .get(id)
                        .map(|reason| reason.to_string()),
                    failures: checked.failures,
                    at_limit: None,
                    past_limit: None,
                };
                if let (Some(quote), false) = (&checked.at_limit, skip_execution.contains_key(id)) {
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

        if !quoted_any_direction {
            return Err(miette!(
                "No tradable direction for pool {id}: every swap direction reported a zero limit."
            ));
        }
    }

    Ok(output)
}

/// Encodes the router swap that executes `quote` for `amount_in`.
pub fn execution_input(
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

/// Why one execution fails a limit check, if it does.
type Judge<'a> = dyn Fn(&str, &TychoExecutionInput, &HashMap<String, TychoExecutionResult>) -> Option<String>
    + 'a;

/// The outcome of one limit check of one direction, as a report records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitJudgement {
    pub check: CheckId,
    pub component: String,
    /// `0x<token in>->0x<token out>`.
    pub direction: String,
    /// What the check records when nothing failed: a pass, or why it did not run.
    pub unfailed: Outcome,
    /// Every failure, from simulation and from execution.
    pub failures: Vec<String>,
}

/// Judges the limit checks of every direction in `output`: `limits.at_limit`,
/// `limits.past_limit` and `limits.max_output`, in that order for each direction.
///
/// `executed` holds the results of executing `output`'s inputs, or why the batch could not run,
/// which fails every execution a check needs. With `None`, nothing was executed, and the checks
/// are judged from the simulation alone.
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

        let mut at_limit = limits.failures_of(CheckId::LimitsAtLimit);
        at_limit.extend(execution_failure(&limits.at_limit, &|id, input, results| {
            execution_mismatch(id, input, results, tolerance)
        }));
        judgements.push(judgement(CheckId::LimitsAtLimit, limits.at_limit_unfailed(), at_limit));

        let mut past_limit = limits.failures_of(CheckId::LimitsPastLimit);
        past_limit.extend(execution_failure(&limits.past_limit, &|id, input, results| {
            missing_revert(id, input, results)
        }));
        judgements.push(judgement(
            CheckId::LimitsPastLimit,
            limits.past_limit_unfailed(),
            past_limit,
        ));

        judgements.push(judgement(
            CheckId::LimitsMaxOutput,
            limits.max_output_unfailed(),
            limits.failures_of(CheckId::LimitsMaxOutput),
        ));
    }
    judgements
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_execution::encoding::models::Solution;
    use tycho_simulation::tycho_common::Bytes;

    use super::*;
    use crate::execution::models::Transaction;

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
                kind: LimitKind::Hard,
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
    fn a_failed_batch_fails_every_execution_and_no_execution_judges_the_simulation() {
        let judged = judge_limits(&output(), Some(Err("the node is down")), Tolerance::EXACT);
        assert_eq!(judged[0].failures, vec!["the node is down".to_string()]);
        assert_eq!(judged[1].failures, vec!["the node is down".to_string()]);

        let simulated_only = judge_limits(&output(), None, Tolerance::EXACT);
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
    fn past_limit_does_not_apply_to_a_soft_or_capped_limit() {
        let mut soft = output();
        soft.limit_directions[0].kind = LimitKind::Soft;
        assert_eq!(
            judge_limits(&soft, None, Tolerance::EXACT)[1].unfailed,
            Outcome::Skipped { reason: "the limit is declared soft".to_string() }
        );
        let mut capped = output();
        capped.limit_directions[0].capped = true;
        assert!(matches!(
            &judge_limits(&capped, None, Tolerance::EXACT)[1].unfailed,
            Outcome::Skipped { reason } if reason.contains("int128::MAX")
        ));
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
    /// `quoted_at_limit` is false, and executed unless `execution_skipped` gives a reason.
    fn unexecuted(quoted_at_limit: bool, execution_skipped: Option<&str>) -> LadderOutput {
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
        let judged = judge_limits(&unexecuted(true, None), Some(Ok(&results)), Tolerance::EXACT);
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
        let output = unexecuted(true, Some("no fork state"));
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
        let judged = judge_limits(&unexecuted(false, None), None, Tolerance::EXACT);
        let not_run = Outcome::NotRun { reason: "limits.at_limit failed".to_string() };
        assert_eq!(judged[1].unfailed, not_run);
        assert_eq!(judged[2].unfailed, not_run);
    }
}
