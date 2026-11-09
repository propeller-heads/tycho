//! What a package declares about its components, read from its `integration_test.tycho.yaml`:
//! which checks it turns off and why, what kind of limit each direction has, and how far
//! executions may differ from simulation.
//!
//! protocol-testing reads these with the rest of a test configuration. [`PackageExpectations`]
//! reads them alone, for a runner that checks a protocol's live pools rather than a package's
//! test cases.

use std::{
    collections::HashMap,
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
};

use miette::{miette, IntoDiagnostic, WrapErr};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};
use tycho_simulation::tycho_common::{models::Chain, Bytes};

/// Which checks a package's tests run.
///
/// `Legacy` runs the checks every package on main was written against. `Strict` is for new
/// packages: it rejects keys the runner does not read, and runs every check of a test and reports
/// all failures instead of stopping at the first. Later checks are added to `Strict` only, so a
/// legacy package keeps passing or failing exactly as before. The one exception applies to both:
/// the package's `substreams_yaml_path` must name its chain ([`package_chain`]), since
/// [`load_packages`] cannot place a package without it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    #[default]
    Legacy,
    Strict,
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

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LimitKind::Hard => "hard",
            LimitKind::Soft => "soft",
            LimitKind::UnderCap => "under_cap",
        })
    }
}

impl Limits {
    /// What is wrong with the declaration's fields: each one its kind requires that is missing or
    /// empty, and each one its kind does not read that is written.
    fn field_problems(&self) -> Vec<String> {
        let reason = ("reason", &self.reason);
        let lost_capacity = ("lost_capacity", &self.lost_capacity);
        let (required, unread) = match self.kind {
            LimitKind::Hard => (vec![], vec![reason, lost_capacity]),
            LimitKind::Soft => (vec![reason], vec![lost_capacity]),
            LimitKind::UnderCap => (vec![reason, lost_capacity], vec![]),
        };
        let mut problems = Vec::new();
        for (name, value) in required {
            if value
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                problems.push(format!("missing {name}"));
            }
        }
        for (name, value) in unread {
            if value.is_some() {
                problems.push(format!("{name} is not read for a {} limit", self.kind));
            }
        }
        problems
    }

    /// Why `limits.past_limit` does not apply to this limit, if it does not: a soft or under-cap
    /// limit is not the venue's bound. The reason names what the declaration gives.
    pub fn past_limit_skip_reason(&self) -> Option<String> {
        let reason = self
            .reason
            .as_deref()
            .unwrap_or_default();
        match self.kind {
            LimitKind::Hard => None,
            LimitKind::Soft => Some(format!("the limit is declared soft: {reason}")),
            LimitKind::UnderCap => Some(format!(
                "the limit is declared under_cap: {reason}; it gives up {}",
                self.lost_capacity
                    .as_deref()
                    .unwrap_or_default()
            )),
        }
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

/// A component whose indexed balances feed only TVL, not simulation, so they are checked within a
/// tolerance instead of exactly:
///
/// ```yaml
/// tvl_only:
///   bps: 10
///   cause: "the indexed reserves exclude the protocol fee bucket the contract also holds"
///   measured: "6.2 bps at most over 40 components, blocks 21000000-21000100 (range --report)"
/// ```
///
/// `bps` is relative to the on-chain amount; `wei` is absolute, in base units of the token. A
/// difference within either passes. A plain skip would also hide a dropped or doubled balance
/// delta, which puts TVL off by 100%; the tolerance sits well below that.
///
/// The declaration is checked, not trusted: the runner decodes the snapshot again with the
/// component's balances scaled down, and every limit, spot price and quote must stay the same
/// (`balances.tvl_only`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TvlOnly {
    /// The largest difference allowed, in basis points of the on-chain amount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bps: Option<u32>,
    /// The largest difference allowed, in base units of the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wei: Option<u64>,
    /// Why the indexed balances drift from the chain.
    pub cause: String,
    /// The largest drift a run measured, and which run.
    pub measured: String,
}

impl TvlOnly {
    pub fn tolerance(&self) -> Tolerance {
        Tolerance { bps: self.bps.unwrap_or(0), wei: self.wei.unwrap_or(0) }
    }
}

/// What is wrong with a declared tolerance, if anything: it needs a nonzero `bps` below 100%, a
/// nonzero `wei`, or both, and a cause and a measurement.
pub fn tolerance_problems(
    bps: Option<u32>,
    wei: Option<u64>,
    cause: &str,
    measured: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    if bps.is_none() && wei.is_none() {
        problems.push(
            "it declares neither `bps` nor `wei`: declare one, or remove the declaration (in \
             strict mode, exact is the default)"
                .to_string(),
        );
    }
    for (field, value) in [("bps", bps.map(u64::from)), ("wei", wei)] {
        if value == Some(0) {
            problems.push(format!("`{field}: 0` allows nothing: leave it out"));
        }
    }
    if let Some(bps) = bps.filter(|bps| *bps >= 10_000) {
        problems.push(format!("`bps: {bps}` is out of range: `bps` must be 1 to 9999"));
    }
    for (field, value) in [("cause", cause), ("measured", measured)] {
        if value.trim().is_empty() {
            problems.push(format!("`{field}` is empty"));
        }
    }
    problems
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
                // Keys that differ only in case or spacing are one direction.
                if declaration
                    .directions
                    .insert(direction.clone(), parse(key, limits.clone())?)
                    .is_some()
                {
                    return Err(de::Error::custom(format!("limits: {direction} is declared twice")));
                }
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

/// Records each entry of `declaration` that lacks a field its kind requires, or has one its kind
/// does not read.
pub fn check_declaration(
    declaration: &LimitsDeclaration,
    location: &str,
    problems: &mut Vec<String>,
) {
    for (key, limits) in declaration.entries() {
        for problem in limits.field_problems() {
            problems.push(format!("{location}, {key}: {problem}"));
        }
    }
}

/// The tolerance executions are checked with: in strict mode, the declared one, or exact without
/// a declaration; in legacy mode, [`Tolerance::LEGACY`] whatever is declared.
pub fn execution_tolerance(harness: Harness, declared: Option<&ExecutionTolerance>) -> Tolerance {
    match (harness, declared) {
        (Harness::Legacy, _) => Tolerance::LEGACY,
        (Harness::Strict, None) => Tolerance::EXACT,
        (Harness::Strict, Some(declared)) => {
            Tolerance { bps: declared.bps.unwrap_or(0), wei: declared.wei.unwrap_or(0) }
        }
    }
}

/// The limit declared for `direction`, from the most specific declaration: the component's for
/// that direction, the component's default, the package's for that direction, then the package's
/// default.
pub fn limits_for<'a>(
    component: Option<&'a LimitsDeclaration>,
    package: Option<&'a LimitsDeclaration>,
    direction: &Direction,
) -> Option<&'a Limits> {
    component
        .and_then(|declaration| declaration.get(direction))
        .or_else(|| package.and_then(|declaration| declaration.get(direction)))
}

/// What a package declares about one of its expected components that holds whatever the block:
/// its limits and its `tvl_only`. Skips are left out, since a test skips a check for its own
/// range, and another test of the same component may run it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComponentExpectations {
    pub limits: Option<LimitsDeclaration>,
    pub tvl_only: Option<TvlOnly>,
}

/// What a package declares, without its test cases: the declarations that apply to every pool
/// of its protocol, and those of the components its tests list, by lowercase id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageExpectations {
    pub protocol_system: String,
    pub harness: Harness,
    pub limits: Option<LimitsDeclaration>,
    pub execution_tolerance: Option<ExecutionTolerance>,
    pub components: HashMap<String, ComponentExpectations>,
}

/// The keys of a test yaml that [`PackageExpectations`] reads. Every other key is ignored: the
/// test runner validates the whole file, and [`PackageExpectations::from_yaml`] validates these.
#[derive(Deserialize)]
struct PackageEntry {
    protocol_system: String,
    #[serde(default)]
    harness: Harness,
    #[serde(default)]
    limits: Option<LimitsDeclaration>,
    #[serde(default)]
    execution_tolerance: Option<ExecutionTolerance>,
    #[serde(default)]
    tests: Vec<TestEntry>,
}

#[derive(Deserialize)]
struct TestEntry {
    #[serde(default)]
    expected_components: Vec<ComponentEntry>,
}

#[derive(Deserialize)]
struct ComponentEntry {
    id: String,
    #[serde(default)]
    limits: Option<LimitsDeclaration>,
    #[serde(default)]
    tvl_only: Option<TvlOnly>,
}

impl PackageExpectations {
    /// Reads the declarations of a package's test yaml; `source` names it in messages.
    ///
    /// Errors when the yaml does not parse, when a declaration is invalid as the test runner
    /// would reject it (a limit missing a field its kind requires, a tolerance without a cause,
    /// ...), or when a component listed by several tests declares different limits or `tvl_only`
    /// in them, naming which.
    pub fn from_yaml(reader: impl Read, source: &str) -> miette::Result<Self> {
        let entry: PackageEntry = serde_yaml::from_reader(reader)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read the declarations of {source}"))?;
        let mut problems = Vec::new();
        if let Some(declaration) = &entry.limits {
            check_declaration(declaration, "package", &mut problems);
        }
        if let Some(tolerance) = &entry.execution_tolerance {
            for problem in tolerance_problems(
                tolerance.bps,
                tolerance.wei,
                &tolerance.cause,
                &tolerance.measured,
            ) {
                problems.push(format!("execution_tolerance: {problem}"));
            }
        }
        let mut package = Self {
            protocol_system: entry.protocol_system,
            harness: entry.harness,
            limits: entry.limits,
            execution_tolerance: entry.execution_tolerance,
            components: HashMap::new(),
        };
        for component in entry
            .tests
            .into_iter()
            .flat_map(|test| test.expected_components)
        {
            let location = format!("component {}", component.id);
            if let Some(declaration) = &component.limits {
                check_declaration(declaration, &location, &mut problems);
            }
            if let Some(tvl_only) = &component.tvl_only {
                for problem in tolerance_problems(
                    tvl_only.bps,
                    tvl_only.wei,
                    &tvl_only.cause,
                    &tvl_only.measured,
                ) {
                    problems.push(format!("{location}, tvl_only: {problem}"));
                }
            }
            let expectations =
                ComponentExpectations { limits: component.limits, tvl_only: component.tvl_only };
            if let Err(e) = package.add_component(&component.id, expectations) {
                problems.push(format!("{e}, in several tests"));
            }
        }
        if !problems.is_empty() {
            return Err(miette!("{source} has invalid declarations: {}", problems.join("; ")));
        }
        Ok(package)
    }

    /// Adds what a test declares about component `id`. Errors, naming the field, when the
    /// component is already there with a different declaration.
    fn add_component(
        &mut self,
        id: &str,
        expectations: ComponentExpectations,
    ) -> Result<(), String> {
        let Some(seen) = self.components.get(&id.to_lowercase()) else {
            self.components
                .insert(id.to_lowercase(), expectations);
            return Ok(());
        };
        let mut differ = Vec::new();
        if seen.limits != expectations.limits {
            differ.push("limits");
        }
        if seen.tvl_only != expectations.tvl_only {
            differ.push("tvl_only");
        }
        match differ.as_slice() {
            [] => Ok(()),
            fields => Err(format!("component {id} declares different {}", fields.join(" and "))),
        }
    }

    /// Merges `other`, another yaml of the same protocol system: the package-level declarations
    /// must be the same, and so must those of a component both list; components only one lists
    /// are added. Errors name the declaration that differs.
    fn merge(&mut self, other: PackageExpectations) -> Result<(), String> {
        let mut differ = Vec::new();
        if self.harness != other.harness {
            differ.push("harness");
        }
        if self.limits != other.limits {
            differ.push("limits");
        }
        if self.execution_tolerance != other.execution_tolerance {
            differ.push("execution_tolerance");
        }
        if !differ.is_empty() {
            return Err(format!("different package {}", differ.join(" and ")));
        }
        for (id, expectations) in other.components {
            self.add_component(&id, expectations)?;
        }
        Ok(())
    }

    /// The tolerance executions of this package's pools are checked with.
    pub fn tolerance(&self) -> Tolerance {
        execution_tolerance(self.harness, self.execution_tolerance.as_ref())
    }

    /// What the package declares about `component_id`, if its tests list it.
    pub fn component(&self, component_id: &str) -> Option<&ComponentExpectations> {
        self.components
            .get(&component_id.to_lowercase())
    }

    /// The limit declared for `direction` of `component_id`, from the most specific declaration:
    /// the component's for that direction, the component's default, the package's for that
    /// direction, then the package's default.
    pub fn limits_for(&self, component_id: &str, direction: &Direction) -> Option<&Limits> {
        limits_for(
            self.component(component_id)
                .and_then(|component| component.limits.as_ref()),
            self.limits.as_ref(),
            direction,
        )
    }
}

/// Reads the declarations of every package for `chain` under `root` (`protocols/substreams`), by
/// protocol system.
///
/// A package's chain is the one its `substreams_yaml_path` names: the first word of that file's
/// name (`base-aero-cl.yaml`, `test-base-lunarbase.yaml`), and Ethereum for a package's own
/// `substreams.yaml`. Templates are skipped. Several yamls of one chain and protocol system are
/// merged: their package-level declarations must match, and so must those of a component more
/// than one lists, since a pool could not tell which applies.
///
/// Errors on the first yaml that cannot be read, whose chain cannot be told, or whose
/// declarations are invalid, whatever chain it is for, so one bad yaml fails every chain: the
/// test runner rejects the same yaml where it is tested.
pub fn load_packages(
    root: &Path,
    chain: Chain,
) -> miette::Result<HashMap<String, PackageExpectations>> {
    let mut packages: HashMap<String, (PackageExpectations, String)> = HashMap::new();
    for path in test_yamls(root, false)? {
        let source = path.display().to_string();
        let yaml = fs::read_to_string(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read {source}"))?;
        let manifest: ManifestEntry = serde_yaml::from_str(&yaml)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read the substreams_yaml_path of {source}"))?;
        let package_chain =
            package_chain(&manifest.substreams_yaml_path).map_err(|e| miette!("{source}: {e}"))?;
        // Read before the chain is filtered, so an invalid yaml fails every chain, as documented.
        let package = PackageExpectations::from_yaml(yaml.as_bytes(), &source)?;
        if package_chain != chain {
            continue;
        }
        match packages.get_mut(&package.protocol_system) {
            Some((seen, first)) => {
                let system = package.protocol_system.clone();
                seen.merge(package).map_err(|e| {
                    miette!("{first} and {source}, both for `{system}` on {chain}: {e}")
                })?;
            }
            None => {
                packages.insert(package.protocol_system.clone(), (package, source));
            }
        }
    }
    Ok(packages
        .into_iter()
        .map(|(system, (package, _))| (system, package))
        .collect())
}

/// The part of a test yaml that names the package's substreams manifest.
#[derive(Deserialize)]
struct ManifestEntry {
    substreams_yaml_path: String,
}

/// Every `integration_test*.tycho.yaml` under `root`, outside build output, sorted; with
/// `templates`, the package templates' yamls too.
pub fn test_yamls(root: &Path, templates: bool) -> miette::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to list {}", dir.display()))?
        {
            let path = entry.into_diagnostic()?.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if path.is_dir() {
                if name != "target" && (templates || !name.contains("-template-")) {
                    dirs.push(path);
                }
            } else if name.starts_with("integration_test") && name.ends_with(".tycho.yaml") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The chain a package's substreams manifest is built for: the first word of the manifest's file
/// name, after a `test-` prefix, and Ethereum for `substreams.yaml`, the name of a package's
/// Ethereum manifest.
pub fn package_chain(substreams_yaml_path: &str) -> Result<Chain, String> {
    let name = Path::new(substreams_yaml_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("`{substreams_yaml_path}` names no manifest"))?;
    let name = name
        .strip_prefix("test-")
        .unwrap_or(name);
    if name == "substreams" {
        return Ok(Chain::Ethereum);
    }
    let word = name
        .split_once('-')
        .map_or(name, |(word, _)| word);
    Chain::from_str(word)
        .map_err(|e| format!("cannot tell the chain of `{substreams_yaml_path}`: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direction(key: &str) -> Direction {
        Direction::from_str(key).unwrap()
    }

    #[test]
    fn directions_parse_and_print_as_addresses() {
        let parsed = direction("0xAB->0x0c");
        assert_eq!(parsed.token_in, Bytes::from("0xab"));
        assert_eq!(parsed.to_string(), "0xab->0x0c");
    }

    const PACKAGE: &str = r#"
protocol_system: "vault"
harness: strict
limits: {kind: soft, reason: "any input trades"}
execution_tolerance: {bps: 3, cause: "vault rounding", measured: "2.1 bps over 100 blocks"}
substreams_yaml_path: ./substreams.yaml
tests:
  - name: first
    expected_components:
      - id: "0xAA"
        tokens: ["0x01", "0x02"]
        skip_execution: "the router needs PUSH0"
        limits:
          "0x01->0x02": {kind: hard}
  - name: second
    expected_components:
      - id: "0xaa"
        tokens: ["0x01", "0x02"]
        skip_execution: "the router needs PUSH0"
        limits:
          "0x01->0x02": {kind: hard}
"#;

    #[test]
    fn a_package_declares_for_its_listed_components_and_for_every_other_pool() {
        let package = PackageExpectations::from_yaml(PACKAGE.as_bytes(), "vault").unwrap();
        assert_eq!(package.protocol_system, "vault");
        assert_eq!(package.tolerance(), Tolerance { bps: 3, wei: 0 });
        let kind = |component: &str, key: &str| {
            package
                .limits_for(component, &direction(key))
                .map(|limits| limits.kind)
        };
        assert_eq!(kind("0xAA", "0x01->0x02"), Some(LimitKind::Hard));
        assert_eq!(kind("0xAA", "0x02->0x01"), Some(LimitKind::Soft));
        assert_eq!(kind("0xbb", "0x01->0x02"), Some(LimitKind::Soft), "an unlisted pool");
    }

    #[test]
    fn a_component_may_skip_differently_in_each_test() {
        let skips_once = PACKAGE.replacen("skip_execution: \"the router needs PUSH0\"", "", 1);
        assert!(PackageExpectations::from_yaml(skips_once.as_bytes(), "vault").is_ok());
    }

    #[test]
    fn a_component_must_declare_the_same_limits_in_every_test() {
        let conflicting = PACKAGE.replacen("{kind: hard}", "{kind: soft, reason: \"r\"}", 1);
        let err = PackageExpectations::from_yaml(conflicting.as_bytes(), "vault").unwrap_err();
        assert!(
            err.to_string()
                .contains("component 0xaa declares different limits, in several tests"),
            "{err}"
        );
    }

    #[test]
    fn a_component_must_declare_the_same_tvl_only_in_every_test() {
        let tvl_only = "        tvl_only: {bps: 5, cause: \"fee bucket\", measured: \"4 bps\"}\n";
        let once =
            PACKAGE.replacen("        limits:\n", &format!("{tvl_only}        limits:\n"), 1);
        let err = PackageExpectations::from_yaml(once.as_bytes(), "vault").unwrap_err();
        assert!(
            err.to_string()
                .contains("declares different tvl_only"),
            "{err}"
        );

        let both = PACKAGE.replace("        limits:\n", &format!("{tvl_only}        limits:\n"));
        let package = PackageExpectations::from_yaml(both.as_bytes(), "vault").unwrap();
        assert_eq!(
            package
                .component("0xAA")
                .and_then(|component| component.tvl_only.as_ref())
                .map(TvlOnly::tolerance),
            Some(Tolerance { bps: 5, wei: 0 })
        );
    }

    #[test]
    fn declarations_the_runner_rejects_are_rejected() {
        for (from, to, problem) in [
            (
                "{kind: soft, reason: \"any input trades\"}",
                "{kind: soft}",
                "package, default: missing reason",
            ),
            ("bps: 3, cause", "bps: 20000, cause", "`bps` must be 1 to 9999"),
            ("{kind: hard}", "{kind: under_cap, reason: \"r\"}", "missing lost_capacity"),
        ] {
            let invalid = PACKAGE.replace(from, to);
            let err = PackageExpectations::from_yaml(invalid.as_bytes(), "vault").unwrap_err();
            assert!(err.to_string().contains(problem), "{to}: {err}");
        }
    }

    /// A directory under the system's temporary one, empty, for yamls.
    fn yaml_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tycho-test-{name}-{}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn two_yamls_of_one_system_merge_their_components() {
        let dir = yaml_dir("merge");
        let other_pool = PACKAGE
            .replace("0xAA", "0xBB")
            .replace("0xaa", "0xbb");
        fs::write(dir.join("integration_test.tycho.yaml"), PACKAGE).unwrap();
        fs::write(dir.join("integration_test_second.tycho.yaml"), &other_pool).unwrap();
        let packages = load_packages(&dir, Chain::Ethereum).unwrap();
        let vault = &packages["vault"];
        assert!(vault.component("0xaa").is_some() && vault.component("0xbb").is_some());

        let other_limits = other_pool
            .replace("limits: {kind: soft, reason: \"any input trades\"}", "limits: {kind: hard}");
        fs::write(dir.join("integration_test_second.tycho.yaml"), other_limits).unwrap();
        let err = load_packages(&dir, Chain::Ethereum).unwrap_err();
        assert!(
            err.to_string()
                .contains("different package limits"),
            "{err}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_invalid_yaml_for_another_chain_fails_every_chain() {
        let dir = yaml_dir("other-chain");
        fs::write(dir.join("integration_test.tycho.yaml"), PACKAGE).unwrap();
        let invalid_on_base = PACKAGE
            .replace("./substreams.yaml", "./base-vault.yaml")
            .replace("{kind: soft, reason: \"any input trades\"}", "{kind: soft}");
        fs::write(dir.join("integration_test_base.tycho.yaml"), invalid_on_base).unwrap();
        let err = load_packages(&dir, Chain::Ethereum).unwrap_err();
        assert!(
            err.to_string()
                .contains("missing reason"),
            "{err}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_mode_keeps_half_a_percent() {
        let legacy = PACKAGE.replace("harness: strict\n", "");
        let package = PackageExpectations::from_yaml(legacy.as_bytes(), "vault").unwrap();
        assert_eq!(package.tolerance(), Tolerance::LEGACY);
    }

    #[test]
    fn every_package_in_the_repository_loads_by_chain_and_protocol_system() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocols/substreams");
        let ethereum = load_packages(&root, Chain::Ethereum).unwrap();
        assert!(ethereum.len() > 20, "{} packages", ethereum.len());
        for system in ["uniswap_v2", "uniswap_v4_hooks", "ekubo_v3", "vm:curve"] {
            assert!(ethereum.contains_key(system), "{system}");
        }
        for (chain, system) in [
            (Chain::Bsc, "ekubo_v3"),
            (Chain::Base, "ekubo_v3"),
            // In `evm-aero-*`: the directory names no chain, the manifest does.
            (Chain::Base, "aero_cl"),
            (Chain::Base, "aero_basic"),
            (Chain::Robinhood, "uniswap_v4_hooks"),
            (Chain::Unichain, "vm:curve"),
            (Chain::Arc, "uniswap_v2"),
            (Chain::Polygon, "ramses_v3"),
        ] {
            let packages = load_packages(&root, chain).unwrap();
            assert!(packages.contains_key(system), "{system} on {chain}");
        }
    }

    #[test]
    fn a_package_is_on_the_chain_its_manifest_names() {
        assert_eq!(package_chain("./substreams.yaml"), Ok(Chain::Ethereum));
        assert_eq!(package_chain("./base-aero-cl.yaml"), Ok(Chain::Base));
        assert_eq!(package_chain("./bsc-ekubo-v3.yaml"), Ok(Chain::Bsc));
        assert_eq!(package_chain("./test-base-lunarbase.yaml"), Ok(Chain::Base));
        let unknown = package_chain("./evm-aero-cl.yaml").unwrap_err();
        assert!(
            unknown.starts_with("cannot tell the chain of `./evm-aero-cl.yaml`:") &&
                unknown.contains("evm"),
            "{unknown}"
        );
    }

    #[test]
    fn every_package_in_the_repository_names_a_known_chain() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocols/substreams");
        for path in test_yamls(&root, false).unwrap() {
            let manifest: ManifestEntry =
                serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            if let Err(e) = package_chain(&manifest.substreams_yaml_path) {
                panic!("{}: {e}", path.display());
            }
        }
    }
}
