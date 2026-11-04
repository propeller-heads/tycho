use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::Read,
    str::FromStr,
};

use colored::Colorize;
use miette::{miette, IntoDiagnostic, WrapErr};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};
use similar::{ChangeTag, TextDiff};
use tycho_simulation::tycho_common::{models::protocol::ProtocolComponent, Bytes};

use crate::balances::BalanceCheck;

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
    /// What the component's limits are, overriding the package's
    /// [`IntegrationTestsConfig::limits`].
    pub limits: Option<LimitsDeclaration>,
    /// Where strict mode reads the component's balances on chain; `balanceOf(component id)`
    /// without it.
    pub balance_check: Option<BalanceCheck>,
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
    #[serde(default)]
    limits: Option<LimitsDeclaration>,
    #[serde(default)]
    balance_check: Option<BalanceCheck>,
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
            limits,
            balance_check,
        } = entry;
        Self {
            base: ProtocolComponentExpectation { id, tokens, static_attributes, creation_tx },
            skip_simulation,
            skip_execution,
            limits,
            balance_check,
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
/// and `false` is the default, so the key is left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Skip {
    /// Not written: the check runs.
    #[default]
    No,
    /// `true` or `false`, legacy mode's form.
    Bool(bool),
    /// Skipped, for this reason.
    Reason(String),
}

impl Skip {
    pub fn is_skipped(&self) -> bool {
        matches!(self, Skip::Bool(true) | Skip::Reason(_))
    }

    /// Why the check is off, for logs and the report. `None` when it is not skipped.
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

/// What kind of bound a component's `get_limits` reports, which decides how strict mode tests it.
///
/// Every kind must be quotable and executable at the limit itself. Only a `hard` limit is also
/// tested one unit past.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    /// The venue rejects any input above the limit (a vault cap, a redemption limit, the end of
    /// the liquidity range), and the limit is exactly that bound. Past it, `get_amount_out`
    /// must error and execution must revert.
    Hard,
    /// The venue has no bound on the input, so the limit is a recommended size, as the
    /// `ProtocolSim::get_limits` documentation allows. Requires a `reason`.
    Soft,
    /// The venue has a bound, and the limit stays below it on purpose because the exact bound
    /// cannot be derived. Requires a `reason`, which shows why no exact bound can be derived, and
    /// the `lost_capacity` it gives up.
    UnderCap,
}

/// What one direction's limit is:
///
/// ```yaml
/// {kind: hard}
/// {kind: soft, reason: "constant product: any input trades, at a worse price"}
/// {kind: under_cap, reason: "...", lost_capacity: "~2% of the vault cap"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub kind: LimitKind,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub lost_capacity: Option<String>,
}

impl Limits {
    /// The fields this kind requires that are missing or empty.
    fn missing_fields(&self) -> Vec<&'static str> {
        let required: &[&'static str] = match self.kind {
            LimitKind::Hard => &[],
            LimitKind::Soft => &["reason"],
            LimitKind::UnderCap => &["reason", "lost_capacity"],
        };
        required
            .iter()
            .copied()
            .filter(|field| {
                let value = match *field {
                    "reason" => &self.reason,
                    _ => &self.lost_capacity,
                };
                value
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
            })
            .collect()
    }
}

/// A package's declared tolerance between simulated and executed amounts:
///
/// ```yaml
/// execution_tolerance:
///   bps: 3
///   cause: "the vault rounds shares down on deposit; the simulation rounds at the end"
///   measured: "2.1 bps at most over blocks 21000000-21000100 (range --report)"
/// ```
///
/// `bps` is relative to the executed amount; `wei` is absolute, in base units of the token out,
/// for a fixed rounding loss such as stETH's 1-2 wei per transfer. Either or both can be declared,
/// and a difference within either passes.
///
/// A tolerance can hide a fee or rounding bug as well as a known artifact, so it states why
/// exact is out of reach and the drift a run measured.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTolerance {
    /// The largest difference allowed, in basis points of the executed amount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bps: Option<u32>,
    /// The largest difference allowed, in base units of the token out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wei: Option<u64>,
    /// Why simulation cannot match execution exactly.
    pub cause: String,
    /// The largest drift a run measured, and which run.
    pub measured: String,
}

/// How far an executed amount out may differ from the simulated one: up to `bps` basis points of
/// the executed amount, or up to `wei` base units of the token out, whichever allows more. Both
/// zero is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tolerance {
    pub bps: u32,
    pub wei: u64,
}

impl Tolerance {
    pub const EXACT: Tolerance = Tolerance { bps: 0, wei: 0 };
    /// Legacy mode's tolerance for every package: 0.5%.
    pub const LEGACY: Tolerance = Tolerance { bps: 50, wei: 0 };
}

impl fmt::Display for Tolerance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.bps, self.wei) {
            (0, 0) => f.write_str("exact"),
            (bps, 0) => write!(f, "{bps} bps"),
            (0, wei) => write!(f, "{wei} wei"),
            (bps, wei) => write!(f, "{bps} bps or {wei} wei"),
        }
    }
}

/// A swap direction, as a `limits` declaration keys it: `"0x<token in>->0x<token out>"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Direction {
    pub token_in: Bytes,
    pub token_out: Bytes,
}

impl FromStr for Direction {
    type Err = String;

    fn from_str(key: &str) -> Result<Self, Self::Err> {
        let (token_in, token_out) = key.split_once("->").ok_or_else(|| {
            format!(
                "`{key}` is not a direction: expected `default` or `0x<token in>->0x<token out>`"
            )
        })?;
        let parse = |address: &str| {
            Bytes::from_str(address.trim())
                .map_err(|e| format!("`{key}`: `{}` is not an address: {e}", address.trim()))
        };
        let direction = Direction { token_in: parse(token_in)?, token_out: parse(token_out)? };
        if direction.token_in == direction.token_out {
            return Err(format!("`{key}` sells and buys the same token"));
        }
        Ok(direction)
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}->{}", self.token_in, self.token_out)
    }
}

/// The `limits` of a package or a component: one [`Limits`] for every direction, or one per
/// direction keyed by token address, with an optional `default` for the rest.
///
/// ```yaml
/// limits: {kind: hard}
/// limits:
///   default: {kind: soft, reason: "..."}
///   "0x<usdc>->0x<susdc>": {kind: hard}
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LimitsDeclaration {
    /// The limit of every direction without its own entry.
    pub default: Option<Limits>,
    pub directions: HashMap<Direction, Limits>,
}

impl LimitsDeclaration {
    /// The declaration of `direction`: its own, or else the default.
    pub fn get(&self, direction: &Direction) -> Option<&Limits> {
        self.directions
            .get(direction)
            .or(self.default.as_ref())
    }

    /// Every declaration with its key, `default` first, then the directions in order.
    fn entries(&self) -> Vec<(String, &Limits)> {
        let mut entries: Vec<(String, &Limits)> = self
            .default
            .iter()
            .map(|limits| ("default".to_string(), limits))
            .collect();
        let mut directions: Vec<_> = self.directions.iter().collect();
        directions.sort_by(|a, b| a.0.cmp(b.0));
        entries.extend(
            directions
                .into_iter()
                .map(|(direction, limits)| (direction.to_string(), limits)),
        );
        entries
    }
}

impl<'de> Deserialize<'de> for LimitsDeclaration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        let parse = |key: &str, value: serde_yaml::Value| {
            serde_yaml::from_value::<Limits>(value).map_err(|e| match key {
                "" => de::Error::custom(format!("limits: {e}")),
                key => de::Error::custom(format!("limits `{key}`: {e}")),
            })
        };
        let Some(map) = value.as_mapping() else {
            return Err(de::Error::custom(
                "`limits` is `{kind: ...}` or a map of `default` and directions to `{kind: ...}`",
            ));
        };
        if map.contains_key("kind") {
            return Ok(Self { default: Some(parse("", value)?), directions: HashMap::new() });
        }
        let mut declaration = Self::default();
        for (key, limits) in map {
            let key = key
                .as_str()
                .ok_or_else(|| de::Error::custom("a `limits` key is not a string"))?;
            if key == "default" {
                declaration.default = Some(parse(key, limits.clone())?);
            } else {
                let direction = Direction::from_str(key).map_err(de::Error::custom)?;
                declaration
                    .directions
                    .insert(direction, parse(key, limits.clone())?);
            }
        }
        Ok(declaration)
    }
}

impl Serialize for LimitsDeclaration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let entries = self.entries();
        let mut map = serializer.serialize_map(Some(entries.len()))?;
        for (key, limits) in entries {
            map.serialize_entry(&key, limits)?;
        }
        map.end()
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
    /// Off by default. Strict mode rejects it: each component declares its `balance_check`.
    #[serde(default)]
    pub skip_balance_check: Skip,
    /// What the components' limits are, unless a component declares its own. Strict mode requires
    /// a declaration for every direction of every expected component, here or on the component.
    #[serde(default)]
    pub limits: Option<LimitsDeclaration>,
    /// How far executions may differ from simulation, in strict mode. Without it, strict mode
    /// requires an exact match.
    #[serde(default)]
    pub execution_tolerance: Option<ExecutionTolerance>,
    pub protocol_type_names: Vec<String>,
    pub protocol_system: String,
    pub module_name: Option<String>,
    pub tests: Vec<IntegrationTest>,
}

impl IntegrationTestsConfig {
    /// Parses a test configuration from yaml.
    ///
    /// Keys the runner does not read are an error in strict mode, naming every such key, and a
    /// warning in legacy mode, where the package keeps running as before. A `limits` declaration
    /// must have the fields its kind requires, and in strict mode every expected component needs
    /// one.
    pub fn from_yaml(reader: impl Read, source: &str) -> miette::Result<Self> {
        let mut unknown_keys = Vec::new();
        let config: Self =
            serde_ignored::deserialize(serde_yaml::Deserializer::from_reader(reader), |path| {
                unknown_keys.push(path.to_string())
            })
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to load test configuration {source}"))?;

        if !unknown_keys.is_empty() {
            match config.harness {
                Harness::Strict => {
                    return Err(miette!(
                        "{source} declares `harness: strict` but has keys the runner does not \
                         read: {}. Fix the spelling or remove them.",
                        unknown_keys.join(", ")
                    ))
                }
                Harness::Legacy => tracing::warn!(
                    "{source} has keys the runner does not read: {}",
                    unknown_keys.join(", ")
                ),
            }
        }
        if config.harness == Harness::Strict {
            config.check_skips_have_reasons(source)?;
            config.check_balances_are_not_skipped(source)?;
        }
        config.check_limits(source)?;
        config.check_tolerance(source)?;
        Ok(config)
    }

    /// The tolerance executions are checked with: in strict mode, the declared one, or exact
    /// without a declaration; in legacy mode, [`Tolerance::LEGACY`] whatever is declared.
    pub fn tolerance(&self) -> Tolerance {
        match (self.harness, &self.execution_tolerance) {
            (Harness::Legacy, _) => Tolerance::LEGACY,
            (Harness::Strict, None) => Tolerance::EXACT,
            (Harness::Strict, Some(declared)) => {
                Tolerance { bps: declared.bps.unwrap_or(0), wei: declared.wei.unwrap_or(0) }
            }
        }
    }

    /// A declared tolerance has a nonzero `bps` below 100%, a nonzero `wei`, or both, and states
    /// its cause and measurement, in both modes.
    fn check_tolerance(&self, source: &str) -> miette::Result<()> {
        let Some(tolerance) = &self.execution_tolerance else {
            return Ok(());
        };
        let mut problems = Vec::new();
        match (tolerance.bps, tolerance.wei) {
            (None, None) => problems.push(
                "it declares neither `bps` nor `wei`; exact is the default: remove the declaration"
                    .to_string(),
            ),
            (Some(0), _) | (_, Some(0)) => problems.push(
                "a zero `bps` or `wei` allows nothing; exact is the default: leave it out"
                    .to_string(),
            ),
            (Some(bps), _) if bps >= 10_000 => {
                problems.push(format!("`bps: {bps}` allows any output"))
            }
            _ => {}
        }
        for (field, value) in [("cause", &tolerance.cause), ("measured", &tolerance.measured)] {
            if value.trim().is_empty() {
                problems.push(format!("`{field}` is empty"));
            }
        }
        if problems.is_empty() {
            return Ok(());
        }
        Err(miette!("{source} has an invalid `execution_tolerance`: {}", problems.join("; ")))
    }

    /// The limit declared for `direction` of `component`, from the most specific declaration:
    /// the component's for that direction, the component's default, the package's for that
    /// direction, then the package's default.
    pub fn limits_for<'a>(
        &'a self,
        component: &'a ProtocolComponentWithTestConfig,
        direction: &Direction,
    ) -> Option<&'a Limits> {
        component
            .limits
            .as_ref()
            .and_then(|declaration| declaration.get(direction))
            .or_else(|| {
                self.limits
                    .as_ref()
                    .and_then(|declaration| declaration.get(direction))
            })
    }

    /// Every check on the `limits` declarations, in both modes:
    ///
    /// - each has the fields its kind requires;
    /// - a component's direction is one of its token pairs, and a package's direction is one of
    ///   some expected component's;
    /// - in strict mode, every direction of every expected component has a declaration.
    fn check_limits(&self, source: &str) -> miette::Result<()> {
        let mut problems = Vec::new();
        let mut undeclared = Vec::new();
        let mut all_directions = HashSet::new();

        for test in &self.tests {
            for component in &test.expected_components {
                let location = format!("test `{}`, component {}", test.name, component.base.id);
                let directions = component_directions(component);
                if let Some(declaration) = &component.limits {
                    check_declaration(declaration, &location, &mut problems);
                    for direction in declaration.directions.keys() {
                        if !directions.contains(direction) {
                            problems.push(format!(
                                "{location}: {direction} is not one of its directions"
                            ));
                        }
                    }
                }
                for direction in &directions {
                    if self
                        .limits_for(component, direction)
                        .is_none()
                    {
                        undeclared.push(format!("{location}, {direction}"));
                    }
                }
                all_directions.extend(directions);
            }
        }
        if let Some(declaration) = &self.limits {
            check_declaration(declaration, "package", &mut problems);
            for direction in declaration.directions.keys() {
                if !all_directions.contains(direction) {
                    problems.push(format!(
                        "package: {direction} is not a direction of any expected component"
                    ));
                }
            }
        }

        if !problems.is_empty() {
            return Err(miette!(
                "{source} has invalid `limits` declarations: {}",
                problems.join("; ")
            ));
        }
        if self.harness == Harness::Strict && !undeclared.is_empty() {
            return Err(miette!(
                "{source} declares `harness: strict`, where every direction of every component \
                 declares its limit (`limits: {{kind: hard | soft | under_cap, ...}}`, on the \
                 package or the component, for every direction or per direction), but these \
                 have none: {}",
                undeclared.join("; ")
            ));
        }
        Ok(())
    }

    /// Every skip in the configuration, with where it is written: `(location, flag, skip)`.
    fn skips(&self) -> Vec<(String, &'static str, &Skip)> {
        let mut skips =
            vec![("package".to_string(), "skip_balance_check", &self.skip_balance_check)];
        for test in &self.tests {
            for component in &test.expected_components {
                let location = format!("test `{}`, component {}", test.name, component.base.id);
                skips.push((location.clone(), "skip_simulation", &component.skip_simulation));
                skips.push((location, "skip_execution", &component.skip_execution));
            }
        }
        skips
    }

    /// Strict mode: a skip is its reason, a string. `true` gives none, and `false` is the default.
    fn check_skips_have_reasons(&self, source: &str) -> miette::Result<()> {
        let written = |value: bool| -> Vec<String> {
            self.skips()
                .into_iter()
                .filter(|(_, _, skip)| **skip == Skip::Bool(value))
                .map(|(location, flag, _)| format!("{flag} ({location})"))
                .collect()
        };
        let mut problems = Vec::new();
        let unexplained = written(true);
        if !unexplained.is_empty() {
            problems.push(format!(
                "these are `true`, which gives no reason: {}. Write each skip as its reason, \
                 `skip_execution: \"...\"`",
                unexplained.join(", ")
            ));
        }
        let redundant = written(false);
        if !redundant.is_empty() {
            problems.push(format!(
                "these are `false`, which is the default: {}. Remove them",
                redundant.join(", ")
            ));
        }
        if problems.is_empty() {
            return Ok(());
        }
        Err(miette!(
            "{source} declares `harness: strict`, where a skip is written as its reason: {}.",
            problems.join("; ")
        ))
    }

    /// Strict mode: balances are checked for every expected component, against the source each
    /// declares, so the package-level skip is not accepted, whatever its reason.
    fn check_balances_are_not_skipped(&self, source: &str) -> miette::Result<()> {
        let Some(reason) = self.skip_balance_check.reason() else {
            return Ok(());
        };
        Err(miette!(
            "{source} declares `harness: strict`, where every expected component's balances are \
             checked, but it sets `skip_balance_check` ({reason}). Remove it, and declare where a \
             component's balances are held with `balance_check: {{holder: \"attribute:NAME\" | \
             \"0x...\"}}` or `{{min_of: [self, ...]}}` where it is not the component's own address."
        ))
    }
}

/// Every swap direction between the component's expected tokens.
pub fn component_directions(component: &ProtocolComponentWithTestConfig) -> Vec<Direction> {
    let tokens = &component.base.tokens;
    let mut directions = Vec::new();
    for token_in in tokens {
        for token_out in tokens {
            if token_in != token_out {
                directions
                    .push(Direction { token_in: token_in.clone(), token_out: token_out.clone() });
            }
        }
    }
    directions
}

/// Records each entry of `declaration` that lacks a field its kind requires.
fn check_declaration(declaration: &LimitsDeclaration, location: &str, problems: &mut Vec<String>) {
    for (key, limits) in declaration.entries() {
        let missing = limits.missing_fields();
        if !missing.is_empty() {
            problems.push(format!("{location}, {key}: missing {}", missing.join(", ")));
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

    /// Strict mode with every direction's limit declared, for tests about something else.
    const STRICT_WITH_LIMITS: &str = "harness: strict\nlimits: {kind: hard}";

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
            "        static_attributes: {fee: \"0x0bb8\"}\n        skip_simulation: \"r\"\n        skip_execution: \"r\"\n        limits: {kind: hard}",
        ))
        .unwrap();
        assert_eq!(config.harness, Harness::Strict);
        let component = &config.tests[0].expected_components[0];
        assert_eq!(component.base.static_attributes["fee"], Bytes::from("0x0bb8"));
        assert!(component.skip_simulation.is_skipped());
        assert!(component.skip_execution.is_skipped());
        assert_eq!(
            component.limits,
            Some(LimitsDeclaration { default: Some(hard()), directions: HashMap::new() })
        );
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
        for harness in ["", STRICT_WITH_LIMITS] {
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
    fn strict_mode_rejects_true_and_false_and_names_each_flag_and_component() {
        let err = parse(&yaml(
            "harness: strict\nskip_balance_check: true",
            "        skip_simulation: true\n        skip_execution: false",
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`true`, which gives no reason: skip_balance_check (package)"),
            "{err}"
        );
        assert!(err.contains("skip_simulation (test `test_foo`, component 0xAbC)"), "{err}");
        assert!(
            err.contains(
                "`false`, which is the default: skip_execution (test `test_foo`, component 0xAbC)"
            ),
            "{err}"
        );
    }

    #[test]
    fn strict_mode_rejects_a_balance_skip_even_with_a_reason() {
        let skipped = |harness: &str| {
            yaml(&format!("{harness}\nskip_balance_check: \"held by the vault\""), "")
        };
        let err = parse(&skipped(STRICT_WITH_LIMITS))
            .unwrap_err()
            .to_string();
        assert!(err.contains("sets `skip_balance_check` (held by the vault)"), "{err}");
        assert!(err.contains("balance_check: {holder:"), "{err}");

        assert!(parse(&skipped(""))
            .unwrap()
            .skip_balance_check
            .is_skipped());

        // Without the key, the check runs, so a strict package need not write `false`.
        assert!(!parse(&yaml(STRICT_WITH_LIMITS, ""))
            .unwrap()
            .skip_balance_check
            .is_skipped());
    }

    #[test]
    fn a_component_declares_its_balance_source_in_either_mode() {
        let declared = "        balance_check: {min_of: [self, \"attribute:vault\"]}";
        for harness in ["", STRICT_WITH_LIMITS] {
            let config = parse(&yaml(harness, declared)).unwrap();
            assert_eq!(
                config.tests[0].expected_components[0].balance_check,
                Some(BalanceCheck::MinOf(vec![
                    crate::balances::Holder::Component,
                    crate::balances::Holder::Attribute("vault".to_string()),
                ]))
            );
            assert_eq!(
                parse(&yaml(harness, "")).unwrap().tests[0].expected_components[0].balance_check,
                None
            );
            let err = parse(&yaml(harness, "        balance_check: {holder: vault}")).unwrap_err();
            assert!(format!("{err:?}").contains("not a holder"), "{err:?}");
        }
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

    fn hard() -> Limits {
        Limits { kind: LimitKind::Hard, reason: None, lost_capacity: None }
    }

    fn direction(key: &str) -> Direction {
        Direction::from_str(key).unwrap()
    }

    /// The kind declared for each direction of the one expected component.
    fn kinds(config: &IntegrationTestsConfig) -> Vec<(String, Option<LimitKind>)> {
        let component = &config.tests[0].expected_components[0];
        component_directions(component)
            .into_iter()
            .map(|direction| {
                let kind = config
                    .limits_for(component, &direction)
                    .map(|limits| limits.kind);
                (direction.to_string(), kind)
            })
            .collect()
    }

    const UNDER_CAP: &str = "{kind: under_cap, reason: \"r\", lost_capacity: \"2%\"}";

    #[test]
    fn strict_mode_requires_a_declaration_for_every_direction() {
        let err = parse(&yaml("harness: strict", ""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("component 0xAbC, 0x01->0x02"), "{err}");
        assert!(err.contains("component 0xAbC, 0x02->0x01"), "{err}");

        let one_direction = "        limits: {\"0x01->0x02\": {kind: hard}}";
        let err = parse(&yaml("harness: strict", one_direction))
            .unwrap_err()
            .to_string();
        assert!(!err.contains("0x01->0x02"), "{err}");
        assert!(err.contains("component 0xAbC, 0x02->0x01"), "{err}");

        // Legacy mode tests no limits, so it needs no declaration.
        assert!(parse(&yaml("", "")).is_ok());
    }

    #[test]
    fn the_most_specific_declaration_applies() {
        let soft = "{kind: soft, reason: \"constant product\"}";
        // The package's default, and its entry for one direction.
        let package =
            format!("harness: strict\nlimits: {{default: {soft}, \"0x02->0x01\": {UNDER_CAP}}}");
        assert_eq!(
            kinds(&parse(&yaml(&package, "")).unwrap()),
            vec![
                ("0x01->0x02".to_string(), Some(LimitKind::Soft)),
                ("0x02->0x01".to_string(), Some(LimitKind::UnderCap)),
            ]
        );

        // A component's entry for a direction overrides the package.
        let component = "        limits: {\"0x01->0x02\": {kind: hard}}";
        assert_eq!(
            kinds(&parse(&yaml(&package, component)).unwrap()),
            vec![
                ("0x01->0x02".to_string(), Some(LimitKind::Hard)),
                ("0x02->0x01".to_string(), Some(LimitKind::UnderCap)),
            ]
        );

        // So does a component's default, even over the package's entry for that direction.
        let component = "        limits: {kind: hard}";
        assert_eq!(
            kinds(&parse(&yaml(&package, component)).unwrap()),
            vec![
                ("0x01->0x02".to_string(), Some(LimitKind::Hard)),
                ("0x02->0x01".to_string(), Some(LimitKind::Hard)),
            ]
        );
    }

    #[test]
    fn each_kind_requires_its_fields_in_both_modes() {
        for (declaration, problem) in [
            ("{kind: soft}", "default: missing reason"),
            ("{kind: soft, reason: \" \"}", "default: missing reason"),
            ("{kind: under_cap, reason: \"r\"}", "default: missing lost_capacity"),
            ("{\"0x01->0x02\": {kind: soft}}", "0x01->0x02: missing reason"),
        ] {
            for harness in ["", "harness: strict"] {
                let err = parse(&yaml(harness, &format!("        limits: {declaration}")))
                    .unwrap_err()
                    .to_string();
                assert!(
                    err.contains(&format!("component 0xAbC, {problem}")),
                    "{declaration}: {err}"
                );
            }
        }
        assert!(parse(&yaml("harness: strict", &format!("        limits: {UNDER_CAP}"))).is_ok());
    }

    #[test]
    fn a_direction_must_be_one_the_components_trade() {
        let err = parse(&yaml("", "        limits: {\"0x01->0x03\": {kind: hard}}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("component 0xAbC: 0x01->0x03 is not one of its directions"), "{err}");

        let err = parse(&yaml("limits: {\"0x03->0x01\": {kind: hard}}", ""))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("package: 0x03->0x01 is not a direction of any expected component"),
            "{err}"
        );
    }

    #[test]
    fn a_malformed_declaration_is_an_error() {
        for declaration in [
            "{kind: exact}",
            "{kind: hard, because: \"x\"}",
            "{kind: under_cap, reason: \"r\", evidence: \"e\", lost_capacity: \"2%\"}",
            "{\"0x01-0x02\": {kind: hard}}",
            "{\"0x01->0x01\": {kind: hard}}",
            "{\"0x01->usdc\": {kind: hard}}",
            "hard",
        ] {
            let err = parse(&yaml("", &format!("        limits: {declaration}"))).unwrap_err();
            assert!(format!("{err:?}").contains("test.yaml"), "{declaration}: {err:?}");
        }
    }

    #[test]
    fn directions_parse_and_print_as_addresses() {
        let parsed = direction("0xAB->0x0c");
        assert_eq!(parsed.token_in, Bytes::from("0xab"));
        assert_eq!(parsed.to_string(), "0xab->0x0c");
    }

    const TOLERANCE: &str =
        "execution_tolerance: {bps: 3, cause: \"vault rounding\", measured: \"2.1 bps over 100 blocks\"}";

    #[test]
    fn strict_mode_is_exact_unless_a_tolerance_is_declared() {
        assert_eq!(
            parse(&yaml(STRICT_WITH_LIMITS, ""))
                .unwrap()
                .tolerance(),
            Tolerance::EXACT
        );
        let declared = parse(&yaml(&format!("{STRICT_WITH_LIMITS}\n{TOLERANCE}"), "")).unwrap();
        assert_eq!(declared.tolerance(), Tolerance { bps: 3, wei: 0 });
        for (declaration, expected) in [
            ("{wei: 2, cause: \"c\", measured: \"m\"}", Tolerance { bps: 0, wei: 2 }),
            ("{bps: 1, wei: 2, cause: \"c\", measured: \"m\"}", Tolerance { bps: 1, wei: 2 }),
        ] {
            let config = parse(&yaml(
                &format!("{STRICT_WITH_LIMITS}\nexecution_tolerance: {declaration}"),
                "",
            ))
            .unwrap();
            assert_eq!(config.tolerance(), expected, "{declaration}");
        }
    }

    #[test]
    fn legacy_mode_keeps_half_a_percent_whatever_is_declared() {
        assert_eq!(
            parse(&yaml("", ""))
                .unwrap()
                .tolerance(),
            Tolerance::LEGACY
        );
        assert_eq!(
            parse(&yaml(TOLERANCE, ""))
                .unwrap()
                .tolerance(),
            Tolerance::LEGACY
        );
    }

    #[test]
    fn a_tolerance_needs_a_cause_a_measurement_and_a_sensible_size() {
        for (declaration, problem) in [
            ("{bps: 0, cause: \"c\", measured: \"m\"}", "allows nothing"),
            ("{wei: 0, cause: \"c\", measured: \"m\"}", "allows nothing"),
            ("{bps: 2, wei: 0, cause: \"c\", measured: \"m\"}", "allows nothing"),
            ("{cause: \"c\", measured: \"m\"}", "neither `bps` nor `wei`"),
            ("{bps: 10000, cause: \"c\", measured: \"m\"}", "allows any output"),
            ("{bps: 3, cause: \" \", measured: \"m\"}", "`cause` is empty"),
            ("{bps: 3, cause: \"c\", measured: \"\"}", "`measured` is empty"),
        ] {
            for harness in ["", STRICT_WITH_LIMITS] {
                let err =
                    parse(&yaml(&format!("{harness}\nexecution_tolerance: {declaration}"), ""))
                        .unwrap_err()
                        .to_string();
                assert!(err.contains(problem), "{declaration}: {err}");
            }
        }
        for declaration in
            ["{bps: 3, cause: \"c\"}", "{bps: 3, cause: \"c\", measured: \"m\", why: \"x\"}"]
        {
            let err = parse(&yaml(&format!("execution_tolerance: {declaration}"), "")).unwrap_err();
            assert!(format!("{err:?}").contains("test.yaml"), "{declaration}: {err:?}");
        }
    }
}
