//! Whether executions match their simulations.

use std::collections::HashMap;

use itertools::Itertools;
use miette::ensure;
use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive, Zero};
use tracing::{error, info};

use crate::{
    execution::models::{TychoExecutionInput, TychoExecutionResult},
    expectations::Tolerance,
};

/// Reports whether an executed amount out matches the simulated one within `tolerance`: in basis
/// points of the executed amount, or in base units of the token out.
///
/// Returns the reason for the mismatch otherwise, saying whether the simulation over- or
/// underquotes. A zero executed amount never matches, since no relative difference can be
/// expressed against it.
pub fn check_execution_slippage(
    expected_amount_out: &BigUint,
    amount_out: &BigUint,
    tolerance: Tolerance,
) -> Result<(), String> {
    if amount_out.is_zero() {
        return Err("execution returned no output".to_string());
    }

    let executed = BigInt::from(amount_out.clone());
    let difference = executed.clone() - BigInt::from(expected_amount_out.clone());
    let within = difference.abs() <= BigInt::from(tolerance.wei) ||
        difference.abs() * 10_000u32 <= executed.clone() * tolerance.bps;
    if within {
        return Ok(());
    }

    let bps = BigRational::new(difference.abs() * 10_000u32, executed)
        .to_f64()
        .map_or_else(|| "?".to_string(), format_bps);
    let (paid, quotes) =
        if difference.is_negative() { ("less", "overquotes") } else { ("more", "underquotes") };
    Err(format!(
        "execution paid {} {paid} than simulated: the simulation {quotes} by {bps} bps, beyond \
         the tolerance ({tolerance})",
        difference.abs()
    ))
}

/// Formats a basis-point difference with two decimals, or with two significant digits below
/// 0.01 bps, where a wei of a large trade would otherwise print as `0.00`.
fn format_bps(bps: f64) -> String {
    if bps == 0.0 || bps.abs() >= 0.01 {
        format!("{bps:.2}")
    } else {
        format!("{bps:.1e}")
    }
}

/// Why the execution of `simulation_id` does not match its simulation, if it does not.
pub fn execution_mismatch(
    simulation_id: &str,
    input: &TychoExecutionInput,
    results: &HashMap<String, TychoExecutionResult>,
    tolerance: Tolerance,
) -> Option<String> {
    let component = &input.component_id;
    let mismatch = match results.get(simulation_id) {
        Some(TychoExecutionResult::Success { amount_out, .. }) => {
            info!(
                "[{component}] Execution returned: {} {} -> {amount_out} {}",
                input.solution.amount_in(),
                input.token_in,
                input.token_out
            );
            check_execution_slippage(&input.expected_amount_out, amount_out, tolerance)
                .err()
                .map(|reason| {
                    format!(
                        "[{component}] Execution does not match simulation for {simulation_id}: \
                         {reason} (simulation={}, execution={amount_out})",
                        input.expected_amount_out
                    )
                })
        }
        Some(TychoExecutionResult::Revert { reason, .. }) => {
            Some(format!("[{component}] Execution reverted for {simulation_id}: {reason}"))
        }
        Some(TychoExecutionResult::Failed { error_msg }) => {
            Some(format!("[{component}] Execution failed for {simulation_id}: {error_msg}"))
        }
        None => Some(format!("[{component}] No result found for simulation {simulation_id}")),
    };
    if let Some(mismatch) = &mismatch {
        error!("{mismatch}");
    }
    mismatch
}

/// Every execution in `inputs` matches its simulation.
pub fn check_executions_match(
    inputs: &HashMap<String, TychoExecutionInput>,
    results: &HashMap<String, TychoExecutionResult>,
    tolerance: Tolerance,
) -> miette::Result<()> {
    let failure_count = inputs
        .iter()
        .sorted_by(|a, b| a.0.cmp(b.0))
        .filter_map(|(simulation_id, input)| {
            execution_mismatch(simulation_id, input, results, tolerance)
        })
        .count();
    let success_count = inputs.len() - failure_count;
    info!("Batch execution complete: {} successes, {} failures", success_count, failure_count);
    ensure!(
        failure_count.is_zero(),
        "Execution validation failed: {success_count} successes, {failure_count} failures"
    );
    Ok(())
}

/// Why the execution of `simulation_id`, one unit past a hard limit, does not show the limit holds:
/// it settled, or it could not be simulated. `None` when it reverted.
pub fn missing_revert(
    simulation_id: &str,
    input: &TychoExecutionInput,
    results: &HashMap<String, TychoExecutionResult>,
) -> Option<String> {
    let component = &input.component_id;
    let amount_in = input.solution.amount_in();
    let failure = match results.get(simulation_id) {
        Some(TychoExecutionResult::Revert { reason, .. }) => {
            info!(
                "[{component}] {amount_in} {}, one unit past the hard limit, reverts: {reason}",
                input.token_in
            );
            None
        }
        Some(TychoExecutionResult::Success { amount_out, .. }) => Some(format!(
            "[{component}] {amount_in} {}, one unit past the declared hard limit, executes and \
             pays {amount_out} {}",
            input.token_in, input.token_out
        )),
        Some(TychoExecutionResult::Failed { error_msg }) => Some(format!(
            "[{component}] {simulation_id} could not be simulated, so the revert past the limit \
             is unproven: {error_msg}"
        )),
        None => Some(format!("[{component}] No result found for simulation {simulation_id}")),
    };
    if let Some(failure) = &failure {
        error!("{failure}");
    }
    failure
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_tolerance_is_half_a_percent_either_way() {
        let executed = BigUint::from(1000u32);
        let check = |simulated: u32| {
            check_execution_slippage(&BigUint::from(simulated), &executed, Tolerance::LEGACY)
        };

        assert!(check(1000).is_ok());
        // 5/1000 is exactly 0.5%, in either direction.
        assert!(check(1005).is_ok());
        assert!(check(995).is_ok());
        assert!(check(1006).is_err());
        assert!(check(994).is_err());
    }

    #[test]
    fn exact_tolerance_fails_on_one_wei() {
        let executed = BigUint::from(1_000_000u32);
        assert!(check_execution_slippage(&executed, &executed, Tolerance::EXACT).is_ok());
        let reason =
            check_execution_slippage(&BigUint::from(1_000_001u32), &executed, Tolerance::EXACT)
                .expect_err("one wei off is not exact");
        assert!(reason.contains("paid 1 less"), "{reason}");
        assert!(reason.contains("tolerance (exact)"), "{reason}");
    }

    #[test]
    fn a_declared_tolerance_allows_up_to_its_bps() {
        let executed = BigUint::from(10_000u32);
        let check = |simulated: u32| {
            check_execution_slippage(
                &BigUint::from(simulated),
                &executed,
                Tolerance { bps: 3, wei: 0 },
            )
        };
        assert!(check(10_003).is_ok());
        assert!(check(9_997).is_ok());
        assert!(check(10_004).is_err());
        assert!(check(9_996).is_err());
    }

    #[test]
    fn a_wei_tolerance_allows_a_fixed_difference_at_any_size() {
        // stETH: a 150k ETH trade paying 1 wei less than simulated, which no bps can allow
        // without allowing ether.
        let executed = BigUint::from(150_000u32) * BigUint::from(10u32).pow(18);
        let simulated = &executed + 1u32;
        let one_wei = Tolerance { bps: 0, wei: 1 };
        assert!(check_execution_slippage(&simulated, &executed, one_wei).is_ok());
        assert!(check_execution_slippage(&(&executed - 1u32), &executed, one_wei).is_ok());
        let reason = check_execution_slippage(&(&executed + 2u32), &executed, one_wei)
            .expect_err("2 wei is past a 1 wei tolerance");
        assert!(reason.contains("tolerance (1 wei)"), "{reason}");

        // With both declared, a difference within either passes.
        let both = Tolerance { bps: 1, wei: 5 };
        let small = BigUint::from(10_000u32);
        assert!(check_execution_slippage(&(&small + 5u32), &small, both).is_ok());
        let large = BigUint::from(1_000_000u32);
        assert!(check_execution_slippage(&(&large + 100u32), &large, both).is_ok());
        assert!(check_execution_slippage(&(&large + 101u32), &large, both).is_err());
    }

    #[test]
    fn a_tiny_mismatch_prints_its_bps_with_significant_digits() {
        let executed = BigUint::from(150_000u32) * BigUint::from(10u32).pow(18);
        let reason =
            check_execution_slippage(&(&executed + 1u32), &executed, Tolerance::EXACT).unwrap_err();
        assert!(reason.contains("overquotes by 6.7e-20 bps"), "{reason}");
        assert_eq!(format_bps(0.0), "0.00");
        assert_eq!(format_bps(0.01), "0.01");
        assert_eq!(format_bps(0.0042), "4.2e-3");
    }

    #[test]
    fn a_mismatch_says_whether_the_simulation_over_or_underquotes() {
        let executed = BigUint::from(10_000u32);
        let over = check_execution_slippage(&BigUint::from(10_010u32), &executed, Tolerance::EXACT)
            .unwrap_err();
        assert!(
            over.contains("paid 10 less") && over.contains("overquotes by 10.00 bps"),
            "{over}"
        );
        let under = check_execution_slippage(&BigUint::from(9_990u32), &executed, Tolerance::EXACT)
            .unwrap_err();
        assert!(
            under.contains("paid 10 more") && under.contains("underquotes by 10.00 bps"),
            "{under}"
        );
    }

    #[test]
    fn zero_output_execution_does_not_match() {
        for tolerance in [Tolerance::EXACT, Tolerance { bps: 0, wei: 5 }, Tolerance::LEGACY] {
            let reason =
                check_execution_slippage(&BigUint::from(1000u32), &BigUint::zero(), tolerance)
                    .expect_err("a zero output must never match");
            assert!(reason.contains("no output"), "unexpected reason: {reason}");

            // Simulating zero as well does not turn a swap that returned nothing into a match.
            assert!(
                check_execution_slippage(&BigUint::zero(), &BigUint::zero(), tolerance).is_err()
            );
        }
    }
}
