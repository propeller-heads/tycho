use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::Read,
};

use colored::Colorize;
use miette::{miette, IntoDiagnostic, WrapErr};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};
use similar::{ChangeTag, TextDiff};
use tycho_simulation::tycho_common::{models::protocol::ProtocolComponent, Bytes};

/// Represents a ProtocolComponent with its main attributes
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProtocolComponentExpectation {
    pub id: String,
    pub tokens: Vec<Bytes>,
    #[serde(default)]
    pub static_attributes: HashMap<String, Bytes>,
    pub creation_tx: Bytes,
}

/// Represents a ProtocolComponent with test configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(from = "ExpectedComponentEntry")]
pub struct ProtocolComponentWithTestConfig {
    #[serde(flatten)]
    pub base: ProtocolComponentExpectation,
    pub skip_simulation: Skip,
    pub skip_execution: Skip,
}

/// An `expected_components` entry as written in the yaml, with every key at one level.
///
/// Deserializing through this flat shape, rather than `#[serde(flatten)]`, lets the parser see
/// every key, so a key nobody reads is reported instead of silently dropped.
#[derive(Deserialize)]
struct ExpectedComponentEntry {
    id: String,
    tokens: Vec<Bytes>,
    #[serde(default)]
    static_attributes: HashMap<String, Bytes>,
    creation_tx: Bytes,
    #[serde(default)]
    skip_simulation: Skip,
    #[serde(default)]
    skip_execution: Skip,
}

impl ProtocolComponentWithTestConfig {
    /// Why the component does not execute, if it does not: its `skip_simulation`, since execution
    /// needs a simulation, else its `skip_execution`.
    pub fn execution_skip_reason(&self) -> Option<&str> {
        self.skip_simulation
            .reason()
            .or_else(|| self.skip_execution.reason())
    }
}

impl From<ExpectedComponentEntry> for ProtocolComponentWithTestConfig {
    fn from(entry: ExpectedComponentEntry) -> Self {
        let ExpectedComponentEntry {
            id,
            tokens,
            static_attributes,
            creation_tx,
            skip_simulation,
            skip_execution,
        } = entry;
        Self {
            base: ProtocolComponentExpectation { id, tokens, static_attributes, creation_tx },
            skip_simulation,
            skip_execution,
        }
    }
}

/// A check a package turns off, and why.
///
/// Written as the reason, a non-empty string:
///
/// ```yaml
/// skip_execution: "range predates Shanghai; the router needs PUSH0"
/// ```
///
/// Legacy mode also accepts `true` and `false`. Strict mode rejects both: `true` gives no reason,
/// and `false` is the default, so the key is left out. In both modes, a string that reads as yes or
/// no, such as `no` or `"false"`, is an error rather than a reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Skip {
    /// Not written: the check runs.
    #[default]
    No,
    /// `true` or `false`, legacy mode's form. `false` runs the check, like [`Skip::No`].
    Bool(bool),
    /// Skipped, for this reason.
    Reason(String),
}

impl Skip {
    pub fn is_skipped(&self) -> bool {
        self.reason().is_some()
    }

    /// Why the check is off, for logs and the report. `None` when it is not skipped. A legacy
    /// `true` has no reason, and reads "no reason given".
    pub fn reason(&self) -> Option<&str> {
        match self {
            Skip::No | Skip::Bool(false) => None,
            Skip::Bool(true) => Some("no reason given"),
            Skip::Reason(reason) => Some(reason),
        }
    }
}

impl<'de> Deserialize<'de> for Skip {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SkipVisitor;

        impl Visitor<'_> for SkipVisitor {
            type Value = Skip;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("the reason for the skip, as a string, or `true` or `false`")
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Skip, E> {
                Ok(Skip::Bool(v))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Skip, E> {
                if v.trim().is_empty() {
                    return Err(E::custom("the reason for a skip is empty"));
                }
                // serde_yaml reads only `true` and `false` as booleans, so `no` or a quoted
                // `"false"` arrives as a string, and would otherwise turn the check off.
                const YES_OR_NO: [&str; 8] = ["true", "false", "yes", "no", "on", "off", "y", "n"];
                if YES_OR_NO
                    .iter()
                    .any(|word| v.trim().eq_ignore_ascii_case(word))
                {
                    return Err(E::custom(format!(
                        "`{v}` reads as yes or no, not as a reason: write why the check is \
                         skipped, or leave the key out to run it"
                    )));
                }
                Ok(Skip::Reason(v.to_string()))
            }
        }

        deserializer.deserialize_any(SkipVisitor)
    }
}

impl Serialize for Skip {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Skip::No => serializer.serialize_bool(false),
            Skip::Bool(skipped) => serializer.serialize_bool(*skipped),
            Skip::Reason(reason) => serializer.serialize_str(reason),
        }
    }
}

impl ProtocolComponentExpectation {
    pub fn compare(&self, other: &ProtocolComponent, colorize_output: bool) -> Option<String> {
        let mut diffs = Vec::new();

        // Compare id (case-insensitive)
        if self.id.to_lowercase() != other.id.to_string().to_lowercase() {
            let diff = Self::format_diff("id", &self.id, &other.id.to_string(), colorize_output);
            diffs.push(format!("Field 'id' mismatch for {}:\n{}", self.id, diff));
        }

        // Compare tokens (order-independent)
        let self_tokens_set: HashSet<_> = self.tokens.iter().collect();
        let other_tokens_set: HashSet<_> = other.tokens.iter().collect();
        if self_tokens_set != other_tokens_set {
            let self_tokens = format!("{:?}", self.tokens);
            let other_tokens = format!("{:?}", other.tokens);
            let diff = Self::format_diff("tokens", &self_tokens, &other_tokens, colorize_output);
            diffs.push(format!("Field 'tokens' mismatch for {}:\n{}", self.id, diff));
        }

        // Compare static_attributes
        for (key, value) in &self.static_attributes {
            let other_value = other.static_attributes.get(key);
            match other_value {
                Some(other_value) => {
                    if value != other_value {
                        let self_value = format!("{value:?}");
                        let other_value = format!("{other_value:?}");
                        let diff = Self::format_diff(
                            "static_attributes",
                            &self_value,
                            &other_value,
                            colorize_output,
                        );
                        diffs.push(format!(
                            "Field 'static_attributes' mismatch for {}:\n{}",
                            self.id, diff
                        ));
                    }
                }
                None => {
                    diffs.push(format!(
                        "Field 'static_attributes' mismatch for {}: Key '{}' not found. Available attributes: {:?}",
                        self.id, key, other.static_attributes,
                    ));
                }
            }
        }
        // Compare creation_tx
        if self.creation_tx != other.creation_tx {
            let self_tx = format!("{}", self.creation_tx.clone());
            let other_tx = format!("{}", other.creation_tx.clone());
            let diff = Self::format_diff("creation_tx", &self_tx, &other_tx, colorize_output);
            diffs.push(format!("Field 'creation_tx' mismatch for {}:\n{}", self.id, diff));
        }

        if diffs.is_empty() {
            None
        } else {
            Some(diffs.join("\n"))
        }
    }
    fn format_diff(_field_name: &str, left: &str, right: &str, colorize: bool) -> String {
        let diff = TextDiff::from_lines(left, right);

        let mut result = String::new();
        for change in diff.iter_all_changes() {
            let formatted = match change.tag() {
                ChangeTag::Delete => {
                    if colorize {
                        format!("{}", format!("-{}", change.value().trim_end()).red())
                    } else {
                        format!("-{}", change.value().trim_end())
                    }
                }
                ChangeTag::Insert => {
                    if colorize {
                        format!("{}", format!("+{}", change.value().trim_end()).green())
                    } else {
                        format!("+{}", change.value().trim_end())
                    }
                }
                ChangeTag::Equal => {
                    format!(" {}", change.value().trim_end())
                }
            };
            result.push_str(&formatted);
            result.push('\n');
        }

        result
    }
}

/// Configuration for an individual test
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IntegrationTest {
    pub name: String,
    pub start_block: u64,
    pub stop_block: u64,
    pub initialized_accounts: Option<Vec<String>>,
    pub expected_components: Vec<ProtocolComponentWithTestConfig>,
    #[serde(default)]
    pub excluded_components: Vec<String>,
}

/// Which checks a package's tests run.
///
/// `Legacy` runs the checks every package on main was written against. `Strict` is for new
/// packages: it rejects keys the runner does not read, and runs every check of a test and reports
/// all failures instead of stopping at the first. Later checks are added to `Strict` only, so a
/// legacy package keeps passing or failing exactly as before.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    #[default]
    Legacy,
    Strict,
}

/// Main integration test configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IntegrationTestsConfig {
    #[serde(default)]
    pub harness: Harness,
    pub substreams_yaml_path: String,
    pub adapter_contract: Option<String>,
    pub adapter_build_signature: Option<String>,
    pub adapter_build_args: Option<String>,
    pub initialized_accounts: Option<Vec<String>>,
    #[serde(default)]
    pub skip_balance_check: Skip,
    pub protocol_type_names: Vec<String>,
    pub protocol_system: String,
    pub module_name: Option<String>,
    pub tests: Vec<IntegrationTest>,
}

impl IntegrationTestsConfig {
    /// Parses a test configuration from yaml.
    ///
    /// Errors when the yaml does not parse, or a skip is an empty string or reads as yes or no. In
    /// strict mode it also errors, listing every problem at once, on keys the runner does not read
    /// and on skips written as `true` or `false` or that can have no effect. In legacy mode
    /// unknown keys are a warning, and the package keeps running as before.
    pub fn from_yaml(reader: impl Read, source: &str) -> miette::Result<Self> {
        let mut unknown_keys = Vec::new();
        let config: Self =
            serde_ignored::deserialize(serde_yaml::Deserializer::from_reader(reader), |path| {
                unknown_keys.push(path.to_string())
            })
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to load test configuration {source}"))?;

        match config.harness {
            Harness::Strict => {
                let mut problems = config.skip_problems();
                if !unknown_keys.is_empty() {
                    problems.push(format!(
                        "keys the runner does not read: {}. Fix the spelling or remove them",
                        unknown_keys.join(", ")
                    ));
                }
                if problems.is_empty() {
                    return Ok(config);
                }
                Err(miette!(
                    "{source} declares `harness: strict`, but has {}.",
                    problems.join("; ")
                ))
            }
            Harness::Legacy => {
                if !unknown_keys.is_empty() {
                    tracing::warn!(
                        "{source} has keys the runner does not read: {}",
                        unknown_keys.join(", ")
                    );
                }
                Ok(config)
            }
        }
    }

    /// Strict mode's problems with the skips, one entry per kind of problem: a skip is written
    /// as its reason, so `true` (no reason) and `false` (the default) are errors, and so is a
    /// `skip_execution` next to a `skip_simulation`, since execution never runs without
    /// simulation and the reason would be read nowhere.
    fn skip_problems(&self) -> Vec<String> {
        let mut unexplained = Vec::new();
        let mut redundant = Vec::new();
        let mut unread = Vec::new();
        let mut note = |flag: &str, location: &str, skip: &Skip| match skip {
            Skip::Bool(true) => unexplained.push(format!("{flag} ({location})")),
            Skip::Bool(false) => redundant.push(format!("{flag} ({location})")),
            Skip::No | Skip::Reason(_) => {}
        };
        note("skip_balance_check", "package", &self.skip_balance_check);
        for test in &self.tests {
            for component in &test.expected_components {
                let location = format!("test `{}`, component {}", test.name, component.base.id);
                note("skip_simulation", &location, &component.skip_simulation);
                note("skip_execution", &location, &component.skip_execution);
                if component.skip_simulation.is_skipped() && component.skip_execution.is_skipped() {
                    unread.push(location);
                }
            }
        }

        let mut problems = Vec::new();
        if !unexplained.is_empty() {
            problems.push(format!(
                "skips written as `true`, which gives no reason: {}. Write the reason instead, \
                 as `<flag>: \"...\"`",
                unexplained.join(", ")
            ));
        }
        if !redundant.is_empty() {
            problems.push(format!(
                "skips written as `false`, which is the default: {}. Remove them",
                redundant.join(", ")
            ));
        }
        if !unread.is_empty() {
            problems.push(format!(
                "`skip_execution` next to `skip_simulation`, where execution never runs: {}. \
                 Remove the `skip_execution`",
                unread.join(", ")
            ));
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal package config, with `extra` inserted as top-level keys and `component_extra` as
    /// keys of the one expected component.
    fn yaml(extra: &str, component_extra: &str) -> String {
        format!(
            r#"{extra}
substreams_yaml_path: ./substreams.yaml
protocol_type_names: ["foo_pool"]
protocol_system: vm:foo
tests:
  - name: test_foo
    start_block: 100
    stop_block: 200
    expected_components:
      - id: "0xAbC"
        tokens: ["0x01", "0x02"]
        creation_tx: "0x03"
{component_extra}
"#
        )
    }

    fn parse(yaml: &str) -> miette::Result<IntegrationTestsConfig> {
        IntegrationTestsConfig::from_yaml(yaml.as_bytes(), "test.yaml")
    }

    #[test]
    fn a_config_without_harness_is_legacy_with_defaults() {
        let config = parse(&yaml("", "")).unwrap();
        assert_eq!(config.harness, Harness::Legacy);
        assert_eq!(config.protocol_system, "vm:foo");
        let component = &config.tests[0].expected_components[0];
        assert_eq!(component.base.id, "0xAbC");
        assert_eq!(component.base.tokens.len(), 2);
        assert!(component
            .base
            .static_attributes
            .is_empty());
        assert_eq!(component.skip_simulation, Skip::No);
        assert_eq!(component.skip_execution, Skip::No);
        assert!(config.tests[0]
            .excluded_components
            .is_empty());
    }

    #[test]
    fn component_keys_reach_the_component() {
        let config = parse(&yaml(
            "harness: strict",
            "        static_attributes: {fee: \"0x0bb8\"}\n        skip_execution: \"r\"",
        ))
        .unwrap();
        assert_eq!(config.harness, Harness::Strict);
        let component = &config.tests[0].expected_components[0];
        assert_eq!(component.base.static_attributes["fee"], Bytes::from("0x0bb8"));
        assert!(!component.skip_simulation.is_skipped());
        assert!(component.skip_execution.is_skipped());
    }

    #[test]
    fn strict_mode_rejects_unknown_keys_and_names_each() {
        let err = parse(&yaml(
            "harness: strict\nskip_balance_checks: true",
            "        skip_simulaton: true",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("test.yaml"), "{err}");
        assert!(err.contains("skip_balance_checks"), "{err}");
        assert!(err.contains("skip_simulaton"), "{err}");
    }

    #[test]
    fn strict_mode_rejects_unknown_keys_in_a_test() {
        let config = yaml("harness: strict", "")
            .replace("    stop_block: 200\n", "    stop_block: 200\n    expected_state: {}\n");
        let err = parse(&config).unwrap_err().to_string();
        assert!(err.contains("expected_state"), "{err}");
    }

    #[test]
    fn legacy_mode_accepts_unknown_keys() {
        let config =
            parse(&yaml("skip_balance_checks: true", "        skip_simulaton: true")).unwrap();
        assert_eq!(config.harness, Harness::Legacy);
        assert_eq!(config.tests[0].expected_components[0].skip_simulation, Skip::No);
    }

    #[test]
    fn an_unknown_harness_is_an_error() {
        let err = parse(&yaml("harness: strikt", "")).unwrap_err();
        assert!(format!("{err:?}").contains("strikt"), "{err:?}");
    }

    #[test]
    fn a_missing_required_key_names_the_source() {
        let config = yaml("", "").replace("protocol_system: vm:foo\n", "");
        let err = parse(&config).unwrap_err();
        let message = format!("{err:?}");
        assert!(message.contains("test.yaml"), "{message}");
        assert!(message.contains("protocol_system"), "{message}");
    }

    #[test]
    fn a_skip_is_its_reason() {
        for harness in ["", "harness: strict"] {
            let config =
                parse(&yaml(harness, "        skip_execution: \"no fork state\"")).unwrap();
            let skip = &config.tests[0].expected_components[0].skip_execution;
            assert_eq!(skip, &Skip::Reason("no fork state".to_string()));
            assert!(skip.is_skipped());
            assert_eq!(skip.reason(), Some("no fork state"));
        }
    }

    #[test]
    fn legacy_mode_accepts_true_and_false() {
        let config = parse(&yaml(
            "skip_balance_check: false",
            "        skip_simulation: true\n        skip_execution: false",
        ))
        .unwrap();
        let component = &config.tests[0].expected_components[0];
        assert!(component.skip_simulation.is_skipped());
        assert_eq!(component.skip_simulation.reason(), Some("no reason given"));
        assert!(!component.skip_execution.is_skipped());
        assert_eq!(component.skip_execution.reason(), None);
        assert_eq!(config.skip_balance_check.reason(), None);
    }

    #[test]
    fn strict_mode_rejects_true_and_names_each_flag_and_component() {
        let err = parse(&yaml(
            "harness: strict\nskip_balance_check: true",
            "        skip_simulation: true",
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(
                "skips written as `true`, which gives no reason: skip_balance_check (package), \
                 skip_simulation (test `test_foo`, component 0xAbC)"
            ),
            "{err}"
        );
    }

    #[test]
    fn strict_mode_rejects_false_and_names_each_flag_and_component() {
        let err = parse(&yaml("harness: strict", "        skip_execution: false"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "skips written as `false`, which is the default: skip_execution (test \
                 `test_foo`, component 0xAbC). Remove them"
            ),
            "{err}"
        );
    }

    #[test]
    fn strict_mode_rejects_a_skip_execution_next_to_a_skip_simulation() {
        let both = "        skip_simulation: \"no adapter\"\n        skip_execution: \"no fork\"";
        let err = parse(&yaml("harness: strict", both))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "`skip_execution` next to `skip_simulation`, where execution never runs: test \
                 `test_foo`, component 0xAbC"
            ),
            "{err}"
        );
        assert!(parse(&yaml("", both)).is_ok());
    }

    #[test]
    fn strict_mode_reports_skip_problems_and_unknown_keys_together() {
        let err = parse(&yaml(
            "harness: strict\nskip_balance_checks: x",
            "        skip_execution: false",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("skips written as `false`"), "{err}");
        assert!(err.contains("keys the runner does not read: skip_balance_checks"), "{err}");
    }

    #[test]
    fn a_skip_that_reads_as_yes_or_no_is_an_error_in_both_modes() {
        for word in ["no", "\"false\"", "off", "Yes", "\"TRUE\"", "n"] {
            for harness in ["", "harness: strict"] {
                let err = parse(&yaml(harness, &format!("        skip_execution: {word}")))
                    .unwrap_err()
                    .chain()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(": ");
                assert!(err.contains("reads as yes or no, not as a reason"), "{word}: {err}");
            }
        }
        // A reason that merely contains such a word is a reason.
        let config = parse(&yaml("", "        skip_execution: \"no fork state\"")).unwrap();
        assert!(config.tests[0].expected_components[0]
            .skip_execution
            .is_skipped());
    }

    #[test]
    fn a_component_that_skips_simulation_does_not_execute_either() {
        let config = parse(&yaml("", "        skip_simulation: \"no adapter\"")).unwrap();
        let component = &config.tests[0].expected_components[0];
        assert_eq!(component.execution_skip_reason(), Some("no adapter"));
        let config = parse(&yaml("", "        skip_execution: \"no fork\"")).unwrap();
        assert_eq!(config.tests[0].expected_components[0].execution_skip_reason(), Some("no fork"));
        let config = parse(&yaml("", "")).unwrap();
        assert_eq!(config.tests[0].expected_components[0].execution_skip_reason(), None);
    }

    #[test]
    fn a_skip_is_a_non_empty_string_or_a_bool() {
        for (component_extra, expected) in [
            ("        skip_execution: \"  \"", "empty"),
            ("        skip_execution: {reason: \"r\"}", "the reason for the skip, as a string"),
            ("        skip_execution: 3", "the reason for the skip, as a string"),
        ] {
            for harness in ["", "harness: strict"] {
                let err = format!("{:?}", parse(&yaml(harness, component_extra)).unwrap_err());
                assert!(err.contains(expected), "{component_extra} ({harness}): {err}");
            }
        }
    }
}
