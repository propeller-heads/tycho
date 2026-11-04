//! What each test checked, and with what outcome.
//!
//! A [`Checks`] collector runs the checks of one test. In legacy mode it stops at the first
//! failure, as the runner always has. In strict mode it runs every check it can and fails the test
//! at the end, listing every failure. The results of every test go into a [`Report`], which the
//! runner can write as JSON.

use std::{fmt, path::Path};

use miette::{miette, IntoDiagnostic, WrapErr};
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

/// Runs the checks of one test and records how each ended.
pub struct Checks {
    harness: Harness,
    test: String,
    results: Vec<CheckResult>,
}

impl Checks {
    pub fn new(harness: Harness, test: &str) -> Self {
        Self { harness, test: test.to_string(), results: Vec::new() }
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
                self.record(id, None, None, Outcome::Pass);
                Ok(Some(value))
            }
            Err(e) => {
                self.record(id, None, None, Outcome::Fail { detail: format!("{e:#}") });
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
        self.record(id, None, None, Outcome::Skipped { reason: reason.to_string() });
    }

    /// Records a check the package's configuration turned off for one component, by its lowercase
    /// id, as the runner matches components.
    pub fn skip_component(&mut self, id: CheckId, component: &str, reason: &str) {
        self.record(id, Some(component), None, Outcome::Skipped { reason: reason.to_string() });
    }

    /// Records a check that could not run because `prerequisite` failed.
    pub fn not_run(&mut self, id: CheckId, prerequisite: CheckId) {
        self.record(id, None, None, Outcome::NotRun { reason: format!("{prerequisite} failed") });
    }

    /// Records a failure that happened outside [`Checks::run`], such as an indexing error.
    pub fn fail(&mut self, id: CheckId, error: &miette::Report) {
        self.record(id, None, None, Outcome::Fail { detail: format!("{error:#}") });
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
    /// `failures` if there are any, else `unfailed`, which says whether the check passed or why it
    /// did not run.
    pub fn record_direction(
        &mut self,
        id: CheckId,
        component: &str,
        direction: &str,
        unfailed: Outcome,
        failures: &[String],
    ) {
        let outcome = if failures.is_empty() {
            unfailed
        } else {
            let detail = failures.join("; ");
            error!("[{}] {id} failed for {component} {direction}: {detail}", self.test);
            Outcome::Fail { detail }
        };
        self.record(id, Some(component), Some(direction), outcome);
    }

    fn record(
        &mut self,
        check: CheckId,
        component: Option<&str>,
        direction: Option<&str>,
        outcome: Outcome,
    ) {
        self.results.push(CheckResult {
            test: self.test.clone(),
            check,
            component: component.map(str::to_lowercase),
            direction: direction.map(str::to_string),
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
    fn a_skipped_component_is_reported_by_its_lowercase_id() {
        let mut checks = Checks::new(Harness::Strict, "t");
        checks.skip_component(CheckId::ExecLadder, "0xAbC", "no fork state");
        let (outcome, results) = checks.finish();
        assert!(outcome.is_ok());
        assert_eq!(results[0].component.as_deref(), Some("0xabc"));
        assert_eq!(results[0].outcome, Outcome::Skipped { reason: "no fork state".to_string() });
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
    fn a_check_recorded_per_direction_passes_fails_skips_or_is_not_run_per_direction() {
        let mut checks = Checks::new(Harness::Strict, "t");
        let failure = ["quotes past the limit".to_string()];
        let soft = || Outcome::Skipped { reason: "soft".to_string() };
        let not_run = || Outcome::NotRun { reason: "limits.at_limit failed".to_string() };
        checks.record_direction(CheckId::LimitsPastLimit, "0x01", "0xa->0xb", Outcome::Pass, &[]);
        checks.record_direction(
            CheckId::LimitsPastLimit,
            "0x01",
            "0xb->0xa",
            Outcome::Pass,
            &failure,
        );
        checks.record_direction(CheckId::LimitsPastLimit, "0x02", "0xa->0xb", soft(), &[]);
        checks.record_direction(CheckId::LimitsPastLimit, "0x02", "0xb->0xa", soft(), &failure);
        checks.record_direction(CheckId::LimitsPastLimit, "0x03", "0xa->0xb", not_run(), &[]);

        let (outcome, results) = checks.finish();
        // The check failed twice and is named once.
        assert_eq!(outcome.unwrap_err().to_string(), "Failed checks: limits.past_limit");
        let outcomes: Vec<_> = results
            .iter()
            .map(|r| (r.component.as_deref(), r.direction.as_deref(), &r.outcome))
            .collect();
        assert_eq!(outcomes[0], (Some("0x01"), Some("0xa->0xb"), &Outcome::Pass));
        assert!(matches!(outcomes[1].2, Outcome::Fail { .. }));
        assert_eq!(outcomes[2], (Some("0x02"), Some("0xa->0xb"), &soft()));
        // A failure outranks a skip.
        assert!(matches!(outcomes[3].2, Outcome::Fail { .. }));
        assert_eq!(outcomes[4], (Some("0x03"), Some("0xa->0xb"), &not_run()));
    }
}
