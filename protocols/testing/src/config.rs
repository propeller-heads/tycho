use std::{
    collections::{HashMap, HashSet},
    io::Read,
};

use colored::Colorize;
use miette::{miette, IntoDiagnostic, WrapErr};
use serde::{Deserialize, Serialize};
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
    pub skip_simulation: bool,
    pub skip_execution: bool,
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
    #[serde(default = "default_false")]
    skip_simulation: bool,
    #[serde(default = "default_false")]
    skip_execution: bool,
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

fn default_false() -> bool {
    false
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
    pub skip_balance_check: bool,
    pub protocol_type_names: Vec<String>,
    pub protocol_system: String,
    pub module_name: Option<String>,
    pub tests: Vec<IntegrationTest>,
}

impl IntegrationTestsConfig {
    /// Parses a test configuration from yaml.
    ///
    /// Keys the runner does not read are an error in strict mode, naming every such key, and a
    /// warning in legacy mode, where the package keeps running as before.
    pub fn from_yaml(reader: impl Read, source: &str) -> miette::Result<Self> {
        let mut unknown_keys = Vec::new();
        let config: Self =
            serde_ignored::deserialize(yaml_serde::Deserializer::from_reader(reader), |path| {
                unknown_keys.push(path.to_string())
            })
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to load test configuration {source}"))?;

        if unknown_keys.is_empty() {
            return Ok(config);
        }
        match config.harness {
            Harness::Strict => Err(miette!(
                "{source} declares `harness: strict` but has keys the runner does not read: {}. \
                 Fix the spelling or remove them.",
                unknown_keys.join(", ")
            )),
            Harness::Legacy => {
                tracing::warn!(
                    "{source} has keys the runner does not read: {}",
                    unknown_keys.join(", ")
                );
                Ok(config)
            }
        }
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
skip_balance_check: false
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
        assert!(!component.skip_simulation);
        assert!(!component.skip_execution);
        assert!(config.tests[0]
            .excluded_components
            .is_empty());
    }

    #[test]
    fn component_keys_reach_the_component() {
        let config = parse(&yaml(
            "harness: strict",
            "        static_attributes: {fee: \"0x0bb8\"}\n        skip_simulation: true\n        skip_execution: true",
        ))
        .unwrap();
        assert_eq!(config.harness, Harness::Strict);
        let component = &config.tests[0].expected_components[0];
        assert_eq!(component.base.static_attributes["fee"], Bytes::from("0x0bb8"));
        assert!(component.skip_simulation);
        assert!(component.skip_execution);
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
        assert!(!config.tests[0].expected_components[0].skip_simulation);
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
}
