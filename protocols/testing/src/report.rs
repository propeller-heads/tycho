//! What each test checked, and with what outcome.
//!
//! A [`Checks`] collector runs the checks of one test. In legacy mode it stops at the first
//! failure, as the runner always has. In strict mode it runs every check it can and fails the test
//! at the end, listing every failure. The results of every test go into a [`Report`], which the
//! runner can write as JSON.

use std::path::Path;

use itertools::Itertools;
use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::{BigInt, BigUint};
use num_traits::Signed;
use serde::Serialize;
use tracing::error;
pub use tycho_test::checks::{
    execution::drift,
    outcome::{CheckId, CheckResult, LimitReport, Outcome},
};

use crate::config::Harness;

/// The largest difference between a simulated and an executed amount out in one test, whether or
/// not it was within tolerance. A declared tolerance's `measured` can be checked against it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Drift {
    pub test: String,
    pub simulation: String,
    pub component: String,
    pub simulated: String,
    pub executed: String,
    /// `(executed - simulated) / executed`, in basis points: negative when the simulation
    /// overquotes, positive when it underquotes.
    pub bps: f64,
    /// `executed - simulated` of the execution furthest off in absolute terms, in base units of
    /// its token out, which may not be the one above. A `wei` tolerance can be checked against it.
    pub largest_difference: String,
    /// The token out `largest_difference` is in.
    pub largest_difference_token: String,
}

/// The execution furthest off in relative terms so far, before it becomes a [`Drift`].
struct Observed {
    simulation: String,
    component: String,
    simulated: String,
    executed: String,
    bps: f64,
}

/// How a test ended, from [`Checks::finish`].
pub struct Finished {
    /// An error naming every failed check if any failed.
    pub outcome: miette::Result<()>,
    pub results: Vec<CheckResult>,
    /// The largest drift, if any execution was noted.
    pub drift: Option<Drift>,
}

/// Runs the checks of one test and records how each ended.
pub struct Checks {
    harness: Harness,
    test: String,
    results: Vec<CheckResult>,
    drift: Option<Observed>,
    /// The largest `executed - simulated` in magnitude, with its token out.
    largest_difference: Option<(BigInt, String)>,
}

impl Checks {
    pub fn new(harness: Harness, test: &str) -> Self {
        Self {
            harness,
            test: test.to_string(),
            results: Vec::new(),
            drift: None,
            largest_difference: None,
        }
    }

    /// Notes an execution's output against its simulation, keeping the largest difference.
    /// A zero output has no relative difference and is not noted; it fails its check anyway.
    pub fn observe_execution(
        &mut self,
        simulation: &str,
        component: &str,
        token_out: &str,
        simulated: &BigUint,
        executed: &BigUint,
    ) {
        let Some((difference, bps)) = drift(simulated, executed) else {
            return;
        };
        if self
            .largest_difference
            .as_ref()
            .is_none_or(|(largest, _)| difference.abs() > largest.abs())
        {
            self.largest_difference = Some((difference, token_out.to_string()));
        }
        if self
            .drift
            .as_ref()
            .is_some_and(|largest| largest.bps.abs() >= bps.abs())
        {
            return;
        }
        self.drift = Some(Observed {
            simulation: simulation.to_string(),
            component: component.to_string(),
            simulated: simulated.to_string(),
            executed: executed.to_string(),
            bps,
        });
    }

    /// Runs `check` and records its outcome.
    ///
    /// Returns the check's value when it passed. When it failed, legacy mode returns the error, so
    /// the test stops there as before; strict mode records the failure and returns `Ok(None)`, so
    /// the caller can go on with the checks that do not depend on this one.
    pub fn run<T>(
        &mut self,
        id: CheckId,
        check: impl FnOnce() -> miette::Result<T>,
    ) -> miette::Result<Option<T>> {
        match check() {
            Ok(value) => {
                self.record(id, None, None, Outcome::Pass, None);
                Ok(Some(value))
            }
            Err(e) => {
                self.record(id, None, None, Outcome::Fail { detail: format!("{e:#}") }, None);
                match self.harness {
                    Harness::Legacy => Err(e),
                    Harness::Strict => {
                        error!("[{}] {id} failed: {e:#}", self.test);
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Records a check the package's configuration turned off.
    pub fn skip(&mut self, id: CheckId, reason: &str) {
        self.record(id, None, None, Outcome::Skipped { reason: reason.to_string() }, None);
    }

    /// Records a check the package's configuration turned off for one component, by its lowercase
    /// id, as the runner matches components.
    pub fn skip_component(&mut self, id: CheckId, component: &str, reason: &str) {
        self.record(
            id,
            Some(component),
            None,
            Outcome::Skipped { reason: reason.to_string() },
            None,
        );
    }

    /// Records a check that could not run because `prerequisite` failed.
    pub fn not_run(&mut self, id: CheckId, prerequisite: CheckId) {
        self.record(
            id,
            None,
            None,
            Outcome::NotRun { reason: format!("{prerequisite} failed") },
            None,
        );
    }

    /// Records a failure that happened outside [`Checks::run`], such as an indexing error.
    pub fn fail(&mut self, id: CheckId, error: &miette::Report) {
        self.record(id, None, None, Outcome::Fail { detail: format!("{error:#}") }, None);
    }

    /// Ends the test: an error naming every failed check if any failed, `Ok` otherwise, with the
    /// results and the largest drift either way.
    pub fn finish(self) -> Finished {
        // A check recorded per direction can fail several times; name it once.
        let failed: Vec<&str> = self
            .results
            .iter()
            .filter(|result| matches!(result.outcome, Outcome::Fail { .. }))
            .map(|result| result.check.as_str())
            .unique()
            .collect();
        let outcome = if failed.is_empty() {
            Ok(())
        } else {
            Err(miette!("Failed checks: {}", failed.join(", ")))
        };
        let drift = self
            .drift
            .zip(self.largest_difference)
            .map(|(observed, (largest_difference, token))| {
                let Observed { simulation, component, simulated, executed, bps } = observed;
                Drift {
                    test: self.test,
                    simulation,
                    component,
                    simulated,
                    executed,
                    bps,
                    largest_difference: largest_difference.to_string(),
                    largest_difference_token: token,
                }
            });
        Finished { outcome, results: self.results, drift }
    }

    /// Records the outcome of a limit check on one direction of one component, with the limit and
    /// its declaration: a failure listing `failures` if there are any, else `unfailed`, which says
    /// whether the check passed or why it did not run.
    pub fn record_direction(
        &mut self,
        id: CheckId,
        component: &str,
        direction: &str,
        limit: &LimitReport,
        unfailed: Outcome,
        failures: &[String],
    ) {
        self.record_scoped(id, component, Some(direction), Some(limit), unfailed, failures);
    }

    /// Records the outcome of a check on one component: a failure listing `failures` if there
    /// are any, else a pass.
    pub fn record_component(&mut self, id: CheckId, component: &str, failures: &[String]) {
        self.record_scoped(id, component, None, None, Outcome::Pass, failures);
    }

    fn record_scoped(
        &mut self,
        id: CheckId,
        component: &str,
        direction: Option<&str>,
        limit: Option<&LimitReport>,
        unfailed: Outcome,
        failures: &[String],
    ) {
        let outcome = if failures.is_empty() {
            unfailed
        } else {
            let detail = failures.join("; ");
            let scope =
                direction.map_or_else(|| component.to_string(), |d| format!("{component} {d}"));
            error!("[{}] {id} failed for {scope}: {detail}", self.test);
            Outcome::Fail { detail }
        };
        self.record(id, Some(component), direction, outcome, limit);
    }

    fn record(
        &mut self,
        check: CheckId,
        component: Option<&str>,
        direction: Option<&str>,
        outcome: Outcome,
        limit: Option<&LimitReport>,
    ) {
        self.results.push(CheckResult {
            test: self.test.clone(),
            check,
            component: component.map(str::to_lowercase),
            direction: direction.map(str::to_string),
            outcome,
            limit: limit.cloned(),
        });
    }
}

/// The results of every test of one run of a package.
#[derive(Debug, Serialize)]
pub struct Report {
    pub package: String,
    pub chain: String,
    pub harness: Harness,
    pub results: Vec<CheckResult>,
    /// The largest drift of each test that executed anything.
    pub drift: Vec<Drift>,
}

impl Report {
    /// Adds a finished test's results and drift. Returns `run`, the test's own result, if it
    /// failed, and else the checks' outcome.
    pub fn add_test(&mut self, finished: Finished, run: miette::Result<()>) -> miette::Result<()> {
        self.results.extend(finished.results);
        self.drift.extend(finished.drift);
        run.and(finished.outcome)
    }
}

impl Report {
    pub fn write(&self, path: &Path) -> miette::Result<()> {
        let json = serde_json::to_string_pretty(self).into_diagnostic()?;
        std::fs::write(path, json)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to write the test report to {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DeclaredBy, Limits};

    fn limit() -> LimitReport {
        LimitReport::new(&Limits::Hard { rounding: 0 }, DeclaredBy::Component, None, false)
    }

    fn failing() -> miette::Result<()> {
        Err(miette!("boom"))
    }

    #[test]
    fn legacy_mode_stops_at_the_first_failure() {
        let mut checks = Checks::new(Harness::Legacy, "t");
        assert!(checks
            .run(CheckId::StateComponents, failing)
            .is_err());
        let Finished { outcome, results, .. } = checks.finish();
        assert!(outcome.is_err());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].check, CheckId::StateComponents);
    }

    #[test]
    fn strict_mode_runs_on_and_reports_every_failure() {
        let mut checks = Checks::new(Harness::Strict, "t");
        assert_eq!(
            checks
                .run(CheckId::StateComponents, failing)
                .unwrap(),
            None
        );
        assert_eq!(
            checks
                .run(CheckId::Balances, || Ok(7))
                .unwrap(),
            Some(7)
        );
        assert_eq!(
            checks
                .run(CheckId::Decode, failing)
                .unwrap(),
            None
        );
        checks.not_run(CheckId::SimLadder, CheckId::Decode);

        let Finished { outcome, results, .. } = checks.finish();
        let message = outcome.unwrap_err().to_string();
        assert!(message.contains("state.components") && message.contains("decode"), "{message}");
        assert!(!message.contains("balances"), "{message}");
        let outcomes: Vec<_> = results
            .iter()
            .map(|r| (r.check, &r.outcome))
            .collect();
        assert_eq!(outcomes.len(), 4);
        assert_eq!(outcomes[1], (CheckId::Balances, &Outcome::Pass));
        assert_eq!(
            outcomes[3],
            (CheckId::SimLadder, &Outcome::NotRun { reason: "decode failed".to_string() })
        );
    }

    #[test]
    fn a_skipped_component_is_reported_by_its_lowercase_id() {
        let mut checks = Checks::new(Harness::Strict, "t");
        checks.skip_component(CheckId::ExecLadder, "0xAbC", "no fork state");
        let Finished { outcome, results, .. } = checks.finish();
        assert!(outcome.is_ok());
        assert_eq!(
            results,
            vec![CheckResult {
                test: "t".to_string(),
                check: CheckId::ExecLadder,
                component: Some("0xabc".to_string()),
                direction: None,
                outcome: Outcome::Skipped { reason: "no fork state".to_string() },
                limit: None,
            }]
        );
    }

    #[test]
    fn a_test_whose_checks_all_pass_passes() {
        let mut checks = Checks::new(Harness::Strict, "t");
        checks
            .run(CheckId::StateComponents, || Ok(()))
            .unwrap();
        checks.skip(CheckId::Balances, "no address holds the pooled ether");
        assert!(checks.finish().outcome.is_ok());
    }

    #[test]
    fn the_report_names_checks_as_the_yaml_does() {
        let report = Report {
            package: "ethereum-foo".to_string(),
            chain: "ethereum".to_string(),
            harness: Harness::Strict,
            drift: vec![],
            results: vec![
                CheckResult {
                    test: "t".to_string(),
                    check: CheckId::SimLadder,
                    component: None,
                    direction: None,
                    outcome: Outcome::Fail { detail: "boom".to_string() },
                    limit: None,
                },
                CheckResult {
                    test: "t".to_string(),
                    check: CheckId::LimitsPastLimit,
                    component: Some("0x01".to_string()),
                    direction: Some("0x02->0x03".to_string()),
                    outcome: Outcome::Skipped { reason: "off".to_string() },
                    limit: Some(LimitReport::new(
                        &Limits::Soft { reason: "constant product".to_string() },
                        DeclaredBy::PackageDefault,
                        Some((&BigUint::from(1000u32), &BigUint::from(2000u32))),
                        false,
                    )),
                },
            ],
        };
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(json["harness"], "strict");
        assert_eq!(json["results"][0]["check"], "sim.ladder");
        assert_eq!(json["results"][0]["outcome"], "fail");
        assert_eq!(json["results"][0]["detail"], "boom");
        assert!(json["results"][0]
            .get("component")
            .is_none());
        assert!(json["results"][0]
            .get("direction")
            .is_none());
        assert_eq!(json["results"][1]["check"], "limits.past_limit");
        assert_eq!(json["results"][1]["component"], "0x01");
        assert_eq!(json["results"][1]["direction"], "0x02->0x03");
        assert_eq!(json["results"][1]["outcome"], "skipped");
        assert_eq!(json["results"][1]["reason"], "off");
        assert!(json["results"][0]
            .get("limit")
            .is_none());
        assert_eq!(
            json["results"][1]["limit"],
            serde_json::json!({
                "kind": "soft",
                "reason": "constant product",
                "declared_by": "package_default",
                "max_input": "1000",
                "max_output": "2000",
                "capped": false,
            })
        );
    }

    /// Records `unfailed` for `limits.past_limit` on `component` and `direction`, failing with
    /// `failures` if any, and returns the outcome it recorded.
    fn record_one(unfailed: Outcome, failures: &[String]) -> Outcome {
        let mut checks = Checks::new(Harness::Strict, "t");
        checks.record_direction(
            CheckId::LimitsPastLimit,
            "0x01",
            "0xa->0xb",
            &limit(),
            unfailed,
            failures,
        );
        let results = checks.finish().results;
        results[0].outcome.clone()
    }

    #[test]
    fn a_failure_outranks_what_a_direction_would_otherwise_record() {
        let failure = ["quotes past the limit".to_string()];
        let skipped = Outcome::Skipped { reason: "capped".to_string() };
        for unfailed in [Outcome::Pass, skipped.clone()] {
            assert_eq!(
                record_one(unfailed, &failure),
                Outcome::Fail { detail: "quotes past the limit".to_string() }
            );
        }
        assert_eq!(record_one(Outcome::Pass, &[]), Outcome::Pass);
        assert_eq!(record_one(skipped.clone(), &[]), skipped);
    }

    #[test]
    fn a_check_failing_in_several_directions_is_named_once() {
        let mut checks = Checks::new(Harness::Strict, "t");
        let failure = ["quotes past the limit".to_string()];
        for direction in ["0xa->0xb", "0xb->0xa"] {
            checks.record_direction(
                CheckId::LimitsPastLimit,
                "0x01",
                direction,
                &limit(),
                Outcome::Pass,
                &failure,
            );
        }
        let Finished { outcome, results, .. } = checks.finish();
        assert_eq!(outcome.unwrap_err().to_string(), "Failed checks: limits.past_limit");
        let directions: Vec<_> = results
            .iter()
            .map(|r| (r.component.as_deref(), r.direction.as_deref()))
            .collect();
        assert_eq!(
            directions,
            vec![(Some("0x01"), Some("0xa->0xb")), (Some("0x01"), Some("0xb->0xa"))]
        );
    }

    #[test]
    fn a_check_recorded_per_component_has_no_direction() {
        let mut checks = Checks::new(Harness::Strict, "t");
        checks.record_component(CheckId::Balances, "0x01", &[]);
        checks.record_component(CheckId::Balances, "0x02", &["A: indexed 1".to_string()]);

        let Finished { outcome, results, .. } = checks.finish();
        assert_eq!(outcome.unwrap_err().to_string(), "Failed checks: balances");
        assert_eq!(
            (results[0].component.as_deref(), results[0].direction.as_deref(), &results[0].outcome),
            (Some("0x01"), None, &Outcome::Pass)
        );
        assert_eq!(results[1].outcome, Outcome::Fail { detail: "A: indexed 1".to_string() });
    }

    #[test]
    fn the_largest_drift_of_a_test_is_kept_with_its_sign() {
        assert!(Checks::new(Harness::Strict, "t")
            .finish()
            .drift
            .is_none());

        let amount = |n: u32| BigUint::from(n);
        let mut checks = Checks::new(Harness::Strict, "t");
        checks.observe_execution("a", "0x01", "USDC", &amount(1000), &amount(1000));
        // An overquote of 30 bps.
        checks.observe_execution("b", "0x01", "USDC", &amount(1003), &amount(1000));
        // An underquote of 10 bps.
        checks.observe_execution("c", "0x02", "WETH", &amount(999), &amount(1000));
        // No output: not noted.
        checks.observe_execution("d", "0x02", "WETH", &amount(1000), &amount(0));
        // Overquotes by 20 wei, more than "b" in wei but only 2 bps.
        checks.observe_execution("e", "0x02", "WETH", &amount(100_020), &amount(100_000));

        let drift = checks.finish().drift.unwrap();
        assert_eq!(drift.test, "t");
        assert_eq!(drift.simulation, "b");
        assert_eq!(drift.component, "0x01");
        assert_eq!((drift.simulated.as_str(), drift.executed.as_str()), ("1003", "1000"));
        assert!((drift.bps + 30.0).abs() < 1e-9, "{}", drift.bps);
        assert_eq!(
            (drift.largest_difference.as_str(), drift.largest_difference_token.as_str()),
            ("-20", "WETH")
        );

        let mut checks = Checks::new(Harness::Strict, "t");
        checks.observe_execution("f", "0x01", "USDC", &amount(1000), &amount(1001));
        let drift = checks.finish().drift.unwrap();
        assert!((drift.bps - 10_000.0 / 1001.0).abs() < 1e-9, "{}", drift.bps);
        assert_eq!(drift.largest_difference, "1");
    }
}
