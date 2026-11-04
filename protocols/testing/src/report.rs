//! What each test checked, and with what outcome.
//!
//! A [`Checks`] collector runs the checks of one test. In legacy mode it stops at the first
//! failure, as the runner always has. In strict mode it runs every check it can and fails the test
//! at the end, listing every failure. The results of every test go into a [`Report`], which the
//! runner can write as JSON.

use std::{fmt, path::Path};

use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::{BigInt, BigUint};
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive, Zero};
use serde::Serialize;
use tracing::error;

use crate::config::Harness;

/// The checks a test runs, named the way the report and the yaml refer to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckId {
    /// Indexing the range and finding the block to read the snapshot at.
    Index,
    /// Every expected component is indexed, with its tokens and static attributes.
    StateComponents,
    /// No excluded component is indexed.
    StateExcluded,
    /// Indexed component balances match the chain.
    Balances,
    /// Strict: a component declared `tvl_only` quotes the same with its balances halved.
    BalancesTvlOnly,
    /// The snapshot decodes, and every expected component decodes from it.
    Decode,
    /// Limits, spot prices and quotes along the ladder, in every direction.
    SimLadder,
    /// Executing the ladder's quotes on chain matches the simulation.
    ExecLadder,
    /// Strict: every direction quotes and executes at its limit.
    LimitsAtLimit,
    /// Strict: past a hard limit, the quote errors and the execution reverts.
    LimitsPastLimit,
    /// Strict: the quote at the limit pays no more than the limit's max output.
    LimitsMaxOutput,
}

impl CheckId {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckId::Index => "index",
            CheckId::StateComponents => "state.components",
            CheckId::StateExcluded => "state.excluded",
            CheckId::Balances => "balances",
            CheckId::BalancesTvlOnly => "balances.tvl_only",
            CheckId::Decode => "decode",
            CheckId::SimLadder => "sim.ladder",
            CheckId::ExecLadder => "exec.ladder",
            CheckId::LimitsAtLimit => "limits.at_limit",
            CheckId::LimitsPastLimit => "limits.past_limit",
            CheckId::LimitsMaxOutput => "limits.max_output",
        }
    }
}

impl fmt::Display for CheckId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for CheckId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// How a check ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Fail {
        detail: String,
    },
    /// The package's configuration turned the check off.
    Skipped {
        reason: String,
    },
    /// An earlier check failed and this one needs its result.
    NotRun {
        reason: String,
    },
}

/// One check of one test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub test: String,
    pub check: CheckId,
    /// The component the result is about, when it is about one only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// The swap direction the result is about, `0x<token in>->0x<token out>`, when it is about one
    /// only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// The largest difference between a simulated and an executed amount out in one test, whether or
/// not it was within tolerance. A declared tolerance's `measured` is checked against it.
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
    /// its token out, which may not be the one above. A `wei` tolerance is checked against it.
    pub largest_difference: String,
}

/// Runs the checks of one test and records how each ended.
pub struct Checks {
    harness: Harness,
    test: String,
    results: Vec<CheckResult>,
    drift: Option<Drift>,
    largest_difference: Option<BigInt>,
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
        simulated: &BigUint,
        executed: &BigUint,
    ) {
        if executed.is_zero() {
            return;
        }
        let difference = BigInt::from(executed.clone()) - BigInt::from(simulated.clone());
        if self
            .largest_difference
            .as_ref()
            .is_none_or(|largest| difference.abs() > largest.abs())
        {
            self.largest_difference = Some(difference.clone());
        }
        let Some(bps) =
            (BigRational::new(difference * 10_000, BigInt::from(executed.clone()))).to_f64()
        else {
            return;
        };
        if self
            .drift
            .as_ref()
            .is_some_and(|largest| largest.bps.abs() >= bps.abs())
        {
            return;
        }
        self.drift = Some(Drift {
            test: self.test.clone(),
            simulation: simulation.to_string(),
            component: component.to_string(),
            simulated: simulated.to_string(),
            executed: executed.to_string(),
            bps,
            largest_difference: String::new(),
        });
    }

    /// The largest drift noted so far, if any execution was noted.
    pub fn take_drift(&mut self) -> Option<Drift> {
        let largest_difference = self.largest_difference.take();
        self.drift.take().map(|drift| Drift {
            largest_difference: largest_difference.map_or_else(String::new, |d| d.to_string()),
            ..drift
        })
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
                self.record(id, Outcome::Pass);
                Ok(Some(value))
            }
            Err(e) => {
                self.record(id, Outcome::Fail { detail: format!("{e:#}") });
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
        self.record(id, Outcome::Skipped { reason: reason.to_string() });
    }

    /// Records a check the package's configuration turned off for one component.
    pub fn skip_component(&mut self, id: CheckId, component: &str, reason: &str) {
        self.results.push(CheckResult {
            test: self.test.clone(),
            check: id,
            component: Some(component.to_string()),
            direction: None,
            outcome: Outcome::Skipped { reason: reason.to_string() },
        });
    }

    /// Records a check that could not run because `prerequisite` failed.
    pub fn not_run(&mut self, id: CheckId, prerequisite: CheckId) {
        self.record(id, Outcome::NotRun { reason: format!("{prerequisite} failed") });
    }

    /// Records a failure that happened outside [`Checks::run`], such as an indexing error.
    pub fn fail(&mut self, id: CheckId, error: &miette::Report) {
        self.record(id, Outcome::Fail { detail: format!("{error:#}") });
    }

    /// Ends the test: an error naming every failed check if any failed, `Ok` otherwise, and the
    /// results either way.
    pub fn finish(self) -> (miette::Result<()>, Vec<CheckResult>) {
        // A check recorded per direction can fail several times; name it once.
        let mut failed: Vec<String> = Vec::new();
        for result in &self.results {
            let name = result.check.to_string();
            if matches!(result.outcome, Outcome::Fail { .. }) && !failed.contains(&name) {
                failed.push(name);
            }
        }
        let outcome = if failed.is_empty() {
            Ok(())
        } else {
            Err(miette!("Failed checks: {}", failed.join(", ")))
        };
        (outcome, self.results)
    }

    /// Records the outcome of a check on one direction of one component: a failure listing
    /// `failures` if there are any, else a skip when `skipped` gives a reason, else a pass.
    pub fn record_direction(
        &mut self,
        id: CheckId,
        component: &str,
        direction: &str,
        skipped: Option<&str>,
        failures: &[String],
    ) {
        self.record_scoped(id, component, Some(direction), skipped, failures);
    }

    /// Records the outcome of a check on one component: a failure listing `failures` if there
    /// are any, else a pass.
    pub fn record_component(&mut self, id: CheckId, component: &str, failures: &[String]) {
        self.record_scoped(id, component, None, None, failures);
    }

    fn record_scoped(
        &mut self,
        id: CheckId,
        component: &str,
        direction: Option<&str>,
        skipped: Option<&str>,
        failures: &[String],
    ) {
        let outcome = match (skipped, failures.is_empty()) {
            (_, false) => {
                let detail = failures.join("; ");
                let scope =
                    direction.map_or_else(|| component.to_string(), |d| format!("{component} {d}"));
                error!("[{}] {id} failed for {scope}: {detail}", self.test);
                Outcome::Fail { detail }
            }
            (Some(reason), true) => Outcome::Skipped { reason: reason.to_string() },
            (None, true) => Outcome::Pass,
        };
        self.results.push(CheckResult {
            test: self.test.clone(),
            check: id,
            component: Some(component.to_string()),
            direction: direction.map(str::to_string),
            outcome,
        });
    }

    fn record(&mut self, check: CheckId, outcome: Outcome) {
        self.results.push(CheckResult {
            test: self.test.clone(),
            check,
            component: None,
            direction: None,
            outcome,
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

    fn failing() -> miette::Result<()> {
        Err(miette!("boom"))
    }

    #[test]
    fn legacy_mode_stops_at_the_first_failure() {
        let mut checks = Checks::new(Harness::Legacy, "t");
        assert!(checks
            .run(CheckId::StateComponents, failing)
            .is_err());
        let (outcome, results) = checks.finish();
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

        let (outcome, results) = checks.finish();
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
    fn a_test_whose_checks_all_pass_passes() {
        let mut checks = Checks::new(Harness::Strict, "t");
        checks
            .run(CheckId::StateComponents, || Ok(()))
            .unwrap();
        checks.skip(CheckId::Balances, "skip_balance_check: true");
        assert!(checks.finish().0.is_ok());
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
                },
                CheckResult {
                    test: "t".to_string(),
                    check: CheckId::LimitsPastLimit,
                    component: Some("0x01".to_string()),
                    direction: Some("0x02->0x03".to_string()),
                    outcome: Outcome::Skipped { reason: "off".to_string() },
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
    }

    #[test]
    fn a_check_recorded_per_direction_passes_fails_or_skips_per_direction() {
        let mut checks = Checks::new(Harness::Strict, "t");
        let failure = ["quotes past the limit".to_string()];
        checks.record_direction(CheckId::LimitsPastLimit, "0x01", "0xa->0xb", None, &[]);
        checks.record_direction(CheckId::LimitsPastLimit, "0x01", "0xb->0xa", None, &failure);
        checks.record_direction(CheckId::LimitsPastLimit, "0x02", "0xa->0xb", Some("soft"), &[]);
        checks.record_direction(
            CheckId::LimitsPastLimit,
            "0x02",
            "0xb->0xa",
            Some("soft"),
            &failure,
        );

        let (outcome, results) = checks.finish();
        // The check failed twice and is named once.
        assert_eq!(outcome.unwrap_err().to_string(), "Failed checks: limits.past_limit");
        let outcomes: Vec<_> = results
            .iter()
            .map(|r| (r.component.as_deref(), r.direction.as_deref(), &r.outcome))
            .collect();
        assert_eq!(outcomes[0], (Some("0x01"), Some("0xa->0xb"), &Outcome::Pass));
        assert!(matches!(outcomes[1].2, Outcome::Fail { .. }));
        assert_eq!(
            outcomes[2],
            (Some("0x02"), Some("0xa->0xb"), &Outcome::Skipped { reason: "soft".to_string() })
        );
        // A failure outranks a skip.
        assert!(matches!(outcomes[3].2, Outcome::Fail { .. }));
    }

    #[test]
    fn a_check_recorded_per_component_has_no_direction() {
        let mut checks = Checks::new(Harness::Strict, "t");
        checks.record_component(CheckId::Balances, "0x01", &[]);
        checks.record_component(CheckId::Balances, "0x02", &["A: indexed 1".to_string()]);

        let (outcome, results) = checks.finish();
        assert_eq!(outcome.unwrap_err().to_string(), "Failed checks: balances");
        assert_eq!(
            (results[0].component.as_deref(), results[0].direction.as_deref(), &results[0].outcome),
            (Some("0x01"), None, &Outcome::Pass)
        );
        assert_eq!(results[1].outcome, Outcome::Fail { detail: "A: indexed 1".to_string() });
    }

    #[test]
    fn the_largest_drift_of_a_test_is_kept_with_its_sign() {
        let mut checks = Checks::new(Harness::Strict, "t");
        assert!(checks.take_drift().is_none());

        let amount = |n: u32| BigUint::from(n);
        checks.observe_execution("a", "0x01", &amount(1000), &amount(1000));
        checks.observe_execution("b", "0x01", &amount(1003), &amount(1000)); // overquote, 30 bps
        checks.observe_execution("c", "0x02", &amount(999), &amount(1000)); // underquote, 10 bps
        checks.observe_execution("d", "0x02", &amount(1000), &amount(0)); // no output: not noted
                                                                          // Overquotes by 20 wei, more than "b" in wei but only 2 bps.
        checks.observe_execution("e", "0x02", &amount(100_020), &amount(100_000));

        let drift = checks.take_drift().unwrap();
        assert_eq!(drift.simulation, "b");
        assert_eq!(drift.component, "0x01");
        assert_eq!((drift.simulated.as_str(), drift.executed.as_str()), ("1003", "1000"));
        assert!((drift.bps + 30.0).abs() < 1e-9, "{}", drift.bps);
        assert_eq!(drift.largest_difference, "-20");

        checks.observe_execution("f", "0x01", &amount(1000), &amount(1001));
        let drift = checks.take_drift().unwrap();
        assert!(drift.bps > 0.0);
        assert_eq!(drift.largest_difference, "1");
    }
}
