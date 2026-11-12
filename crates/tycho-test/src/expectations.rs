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
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
    str::FromStr,
};

use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize,
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
                    return Err(E::custom(
                        "the reason for a skip is empty: leave the key out to run the check, or \
                         write why it is skipped",
                    ));
                }
                // serde_yaml reads only `true` and `false` as booleans, so `no` or a quoted
                // `"false"` arrives as a string, and would otherwise turn the check off.
                const YES_OR_NO: [&str; 8] = ["true", "false", "yes", "no", "on", "off", "y", "n"];
                if YES_OR_NO
                    .iter()
                    .any(|word| v.trim().eq_ignore_ascii_case(word))
                {
                    return Err(E::custom(format!(
                        "`{v}` reads as yes or no, not as a reason: leave the key out to run the \
                         check, or write why it is skipped"
                    )));
                }
                Ok(Skip::Reason(v.to_string()))
            }
        }

        deserializer.deserialize_any(SkipVisitor)
    }
}

/// What kind of bound a direction's `get_limits` reports, which decides how strict mode tests it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKind {
    Hard,
    Soft,
    UnderCap,
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

/// What one direction's limit is. Every kind must be quotable and executable at the limit itself.
///
/// ```yaml
/// {kind: hard}
/// {kind: hard, rounding: 1}
/// {kind: soft, reason: "constant product: any input trades, at a worse price"}
/// {kind: under_cap, reason: "a keeper-set rate we do not index; gives up to 2% of the cap"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Limits {
    /// The venue rejects any input above its bound (a vault cap, a redemption limit, the end of
    /// the liquidity range), and the limit is that bound, or below it by at most `rounding` units
    /// where `get_limits` derives it with a rounding division, such as an input from an
    /// output-side bound. Past the bound, `get_amount_out` must error and execution must revert.
    Hard {
        #[serde(default)]
        rounding: u64,
    },
    /// The venue has no bound on the input, so the limit is a recommended size, as the
    /// `ProtocolSim::get_limits` documentation allows. The reason names the code that shows it.
    /// Twice the limit must still quote.
    Soft { reason: String },
    /// The venue has a bound, and the limit stays below it on purpose because the exact bound
    /// cannot be derived. The reason shows why, and what capacity the limit gives up. One unit
    /// past the limit must still quote and execute.
    UnderCap { reason: String },
}

impl Limits {
    pub fn kind(&self) -> LimitKind {
        match self {
            Limits::Hard { .. } => LimitKind::Hard,
            Limits::Soft { .. } => LimitKind::Soft,
            Limits::UnderCap { .. } => LimitKind::UnderCap,
        }
    }

    /// The declared reason, for the kinds that require one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Limits::Hard { .. } => None,
            Limits::Soft { reason } | Limits::UnderCap { reason } => Some(reason),
        }
    }
}

/// A tolerance a package declares, with why exact is out of reach:
///
/// ```yaml
/// execution_tolerance:
///   bps: 0.02
///   reason: "the vault prices from a share price rounded to 6 digits; 0.011 bps at most over
///     blocks 21000000-21000100 (range --report)"
/// execution_tolerance:
///   wei: {default: 2, "0x<USDC>": 1}
///   reason: "stETH transfers move shares, rounding the amount down by up to 2 wei"
/// ```
///
/// `bps` is relative to the reference amount (the executed amount for `execution_tolerance`, the
/// on-chain amount for `tvl_only`), with up to two decimals, from 0.01 to 9999.99. `wei` is
/// absolute, in base units of the token: one amount for every token, or a map from token address
/// to amount with an optional `default` for the tokens it does not list. At least one of the two
/// is declared, and a difference within either passes.
///
/// A tolerance can hide a fee or rounding bug as well as a known artifact, so its reason says
/// why exact is out of reach, and how far the run that measured it saw the amounts drift.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredTolerance {
    #[serde(default)]
    pub bps: Option<Bps>,
    #[serde(default)]
    pub wei: Option<Wei>,
    pub reason: String,
}

impl DeclaredTolerance {
    /// The tolerance for amounts of `token`.
    pub fn tolerance(&self, token: &Bytes) -> Tolerance {
        Tolerance {
            centi_bps: self.bps.map_or(0, |bps| bps.0.get()),
            wei: self
                .wei
                .as_ref()
                .map_or(0, |wei| wei.of(token)),
        }
    }

    /// What is wrong with the declaration, if anything: it declares neither `bps` nor `wei`, or
    /// its reason is blank. serde already rejects a zero, an out-of-range `bps` and an unknown
    /// field.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.bps.is_none() && self.wei.is_none() {
            problems.push(
                "it declares neither `bps` nor `wei`: declare one, or remove the declaration (in \
                 strict mode, exact is the default)"
                    .to_string(),
            );
        }
        if self.reason.trim().is_empty() {
            problems.push("the reason is empty".to_string());
        }
        problems
    }
}

/// A relative tolerance in hundredths of a basis point, from 1 (0.01 bps) to 999_999 (9999.99
/// bps). Written in the yaml as a number or a string with up to two decimals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bps(NonZeroU32);

impl Bps {
    /// The tolerance in hundredths of a basis point.
    pub fn centi(&self) -> u32 {
        self.0.get()
    }
}

impl FromStr for Bps {
    type Err = String;

    fn from_str(written: &str) -> Result<Self, String> {
        let invalid = || {
            format!("`bps: {written}` is not a number of basis points from 0.01 to 9999.99, with up to two decimals")
        };
        let (whole, fraction) = written
            .trim()
            .split_once('.')
            .unwrap_or_else(|| (written.trim(), ""));
        if whole.is_empty() ||
            fraction.len() > 2 ||
            !whole
                .chars()
                .chain(fraction.chars())
                .all(|c| c.is_ascii_digit())
        {
            return Err(invalid());
        }
        let whole: u32 = whole
            .parse()
            .map_err(|_not_a_number| invalid())?;
        let fraction: u32 = format!("{fraction:0<2}")
            .parse()
            .map_err(|_not_a_number| invalid())?;
        whole
            .checked_mul(100)
            .and_then(|centi| centi.checked_add(fraction))
            .filter(|centi| *centi < 1_000_000)
            .and_then(NonZeroU32::new)
            .map(Bps)
            .ok_or_else(invalid)
    }
}

impl<'de> Deserialize<'de> for Bps {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A yaml number reads as an integer or a float; its decimal form is parsed like a string.
        let written = match serde_yaml::Value::deserialize(deserializer)? {
            serde_yaml::Value::Number(number) => number.to_string(),
            serde_yaml::Value::String(written) => written,
            other @ (serde_yaml::Value::Null |
            serde_yaml::Value::Bool(_) |
            serde_yaml::Value::Sequence(_) |
            serde_yaml::Value::Mapping(_) |
            serde_yaml::Value::Tagged(_)) => {
                return Err(de::Error::custom(format!(
                    "`bps` is a number of basis points, not {}",
                    yaml_text(&other)
                )))
            }
        };
        written
            .parse()
            .map_err(de::Error::custom)
    }
}

/// An absolute tolerance in base units of the token: one amount for every token, or an amount per
/// token address with an optional default for the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wei {
    All(NonZeroU64),
    PerToken { default: Option<NonZeroU64>, tokens: HashMap<Bytes, NonZeroU64> },
}

impl Wei {
    /// The amount for `token`, 0 when the declaration names no amount for it.
    pub fn of(&self, token: &Bytes) -> u64 {
        match self {
            Wei::All(wei) => wei.get(),
            Wei::PerToken { default, tokens } => tokens
                .get(token)
                .or(default.as_ref())
                .map_or(0, |wei| wei.get()),
        }
    }
}

impl<'de> Deserialize<'de> for Wei {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let amount = |key: &str, value: serde_yaml::Value| {
            serde_yaml::from_value::<NonZeroU64>(value)
                .map_err(|e| de::Error::custom(format!("`wei` {key}: {e}")))
        };
        match serde_yaml::Value::deserialize(deserializer)? {
            serde_yaml::Value::Mapping(map) => {
                let mut default = None;
                let mut tokens = HashMap::new();
                for (key, value) in map {
                    let serde_yaml::Value::String(key) = key else {
                        return Err(de::Error::custom(format!(
                            "a `wei` key is `default` or a token address, not {}",
                            yaml_text(&key)
                        )));
                    };
                    if key == "default" {
                        default = Some(amount(&key, value)?);
                        continue;
                    }
                    let token = Bytes::from_str(&key)
                        .ok()
                        .filter(|token| token.len() == 20)
                        .ok_or_else(|| {
                            de::Error::custom(format!(
                                "a `wei` key is `default` or a token address, not `{key}`"
                            ))
                        })?;
                    if tokens
                        .insert(token, amount(&key, value)?)
                        .is_some()
                    {
                        return Err(de::Error::custom(format!("`wei` lists {key} twice")));
                    }
                }
                if default.is_none() && tokens.is_empty() {
                    return Err(de::Error::custom("`wei` is an empty map"));
                }
                Ok(Wei::PerToken { default, tokens })
            }
            value @ (serde_yaml::Value::Null |
            serde_yaml::Value::Bool(_) |
            serde_yaml::Value::Number(_) |
            serde_yaml::Value::String(_) |
            serde_yaml::Value::Sequence(_) |
            serde_yaml::Value::Tagged(_)) => amount("", value).map(Wei::All),
        }
    }
}

/// A yaml value as it was written, for messages.
fn yaml_text(value: &serde_yaml::Value) -> String {
    serde_yaml::to_string(value)
        .map(|text| format!("`{}`", text.trim_end()))
        .unwrap_or_else(|_| "a value that cannot be shown".to_string())
}

/// How far an amount may differ from the one it is checked against: up to `centi_bps`
/// hundredths of a basis point of the reference amount, or up to `wei` base units of the token,
/// whichever allows more. Both zero is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tolerance {
    pub centi_bps: u32,
    pub wei: u64,
}

impl Tolerance {
    pub const EXACT: Tolerance = Tolerance { centi_bps: 0, wei: 0 };
    /// Legacy mode's tolerance for every package: 0.5%.
    pub const LEGACY: Tolerance = Tolerance { centi_bps: 5_000, wei: 0 };

    /// Whether `gap`, the magnitude of a difference, is within the tolerance of `reference`.
    pub fn allows(&self, gap: &BigUint, reference: &BigUint) -> bool {
        gap <= &BigUint::from(self.wei) || gap * 1_000_000u32 <= reference * self.centi_bps
    }
}

impl fmt::Display for Tolerance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bps = match self.centi_bps % 100 {
            0 => format!("{} bps", self.centi_bps / 100),
            hundredths => format!("{}.{hundredths:02} bps", self.centi_bps / 100),
        };
        match (self.centi_bps, self.wei) {
            (0, 0) => f.write_str("exact"),
            (_, 0) => f.write_str(&bps),
            (0, wei) => write!(f, "{wei} wei"),
            (_, wei) => write!(f, "{bps} or {wei} wei"),
        }
    }
}

/// The tolerance every execution of a package is checked with: in legacy mode
/// [`Tolerance::LEGACY`], whatever is declared; in strict mode the component's
/// `execution_tolerance`, else the package's, else exact, for the execution's token out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionTolerances {
    legacy: bool,
    package: Option<DeclaredTolerance>,
    /// By lowercase component id.
    components: HashMap<String, DeclaredTolerance>,
}

impl ExecutionTolerances {
    /// Exact for every execution: strict mode without declarations.
    pub fn exact() -> Self {
        Self::default()
    }

    /// [`Tolerance::LEGACY`] for every execution.
    pub fn legacy() -> Self {
        Self { legacy: true, ..Self::default() }
    }

    /// The tolerances `harness` checks with, from the package's declaration and its components',
    /// keyed by component id in any case.
    pub fn new<'a>(
        harness: Harness,
        package: Option<&DeclaredTolerance>,
        components: impl IntoIterator<Item = (&'a str, &'a DeclaredTolerance)>,
    ) -> Self {
        match harness {
            Harness::Legacy => Self::legacy(),
            Harness::Strict => Self {
                legacy: false,
                package: package.cloned(),
                components: components
                    .into_iter()
                    .map(|(id, declared)| (id.to_lowercase(), declared.clone()))
                    .collect(),
            },
        }
    }

    /// The tolerance of an execution of `component_id` paying `token_out`.
    pub fn get(&self, component_id: &str, token_out: &Bytes) -> Tolerance {
        if self.legacy {
            return Tolerance::LEGACY;
        }
        self.components
            .get(&component_id.to_lowercase())
            .or(self.package.as_ref())
            .map_or(Tolerance::EXACT, |declared| declared.tolerance(token_out))
    }

    /// The package's own declaration, which applies to every component that declares none.
    pub fn package(&self) -> Option<&DeclaredTolerance> {
        self.package.as_ref()
    }
}

/// Which `limits` declaration a direction's limit comes from, most specific first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredBy {
    /// The component's entry for the direction.
    Component,
    /// The component's `default`, or its one declaration for every direction.
    ComponentDefault,
    /// The package's entry for the direction.
    Package,
    /// The package's `default`, or its one declaration for every direction.
    PackageDefault,
}

impl DeclaredBy {
    /// The name a report gives it.
    pub fn as_str(&self) -> &'static str {
        match self {
            DeclaredBy::Component => "component",
            DeclaredBy::ComponentDefault => "component_default",
            DeclaredBy::Package => "package",
            DeclaredBy::PackageDefault => "package_default",
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
    /// The declaration of `direction`, with `own` when it is the direction's entry and `default`
    /// when it is the declaration's default.
    fn get(
        &self,
        direction: &Direction,
        own: DeclaredBy,
        default: DeclaredBy,
    ) -> Option<(&Limits, DeclaredBy)> {
        match self.directions.get(direction) {
            Some(limits) => Some((limits, own)),
            None => self
                .default
                .as_ref()
                .map(|limits| (limits, default)),
        }
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
        let parse = |key: &str, value| {
            serde_yaml::from_value::<Limits>(value)
                .map_err(|e| de::Error::custom(format!("limits `{key}`: {e}")))
        };
        let serde_yaml::Value::Mapping(map) = serde_yaml::Value::deserialize(deserializer)? else {
            return Err(de::Error::custom(
                "`limits` is `{kind: ...}` or a map of `default` and directions to `{kind: ...}`",
            ));
        };
        if map.contains_key("kind") {
            return serde_yaml::from_value(serde_yaml::Value::Mapping(map))
                .map(|limits| Self { default: Some(limits), directions: HashMap::new() })
                .map_err(|e| de::Error::custom(format!("limits: {e}")));
        }
        let mut declaration = Self::default();
        for (key, limits) in map {
            let serde_yaml::Value::String(key) = key else {
                return Err(de::Error::custom("a `limits` key is not a string"));
            };
            if key == "default" {
                declaration.default = Some(parse(&key, limits)?);
            } else {
                let direction = Direction::from_str(&key).map_err(de::Error::custom)?;
                // Keys that differ only in case or spacing are one direction.
                if declaration
                    .directions
                    .insert(direction.clone(), parse(&key, limits)?)
                    .is_some()
                {
                    return Err(de::Error::custom(format!("limits: {direction} is declared twice")));
                }
            }
        }
        Ok(declaration)
    }
}

/// Records each entry of `declaration` whose reason is blank. serde already rejects a missing
/// reason and a field the kind does not have.
pub fn check_declaration(
    declaration: &LimitsDeclaration,
    location: &str,
    problems: &mut Vec<String>,
) {
    for (key, limits) in declaration.entries() {
        if limits
            .reason()
            .is_some_and(|reason| reason.trim().is_empty())
        {
            problems.push(format!("{location}, {key}: the reason is empty"));
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
    declaration_for(component, package, direction).map(|(limits, _)| limits)
}

/// [`limits_for`], with which declaration it comes from.
pub fn declaration_for<'a>(
    component: Option<&'a LimitsDeclaration>,
    package: Option<&'a LimitsDeclaration>,
    direction: &Direction,
) -> Option<(&'a Limits, DeclaredBy)> {
    component
        .and_then(|declaration| {
            declaration.get(direction, DeclaredBy::Component, DeclaredBy::ComponentDefault)
        })
        .or_else(|| {
            package.and_then(|declaration| {
                declaration.get(direction, DeclaredBy::Package, DeclaredBy::PackageDefault)
            })
        })
}

/// What a package declares about one of its expected components that holds whatever the block:
/// its limits, its execution tolerance and its `tvl_only`. Skips are left out, since a test skips
/// a check for its own range, and another test of the same component may run it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComponentExpectations {
    pub limits: Option<LimitsDeclaration>,
    pub execution_tolerance: Option<DeclaredTolerance>,
    pub tvl_only: Option<DeclaredTolerance>,
}

/// What a package declares, without its test cases: the declarations that apply to every pool
/// of its protocol, and those of the components its tests list, by lowercase id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageExpectations {
    pub protocol_system: String,
    pub harness: Harness,
    pub limits: Option<LimitsDeclaration>,
    pub execution_tolerance: Option<DeclaredTolerance>,
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
    execution_tolerance: Option<DeclaredTolerance>,
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
    execution_tolerance: Option<DeclaredTolerance>,
    #[serde(default)]
    tvl_only: Option<DeclaredTolerance>,
}

impl PackageExpectations {
    /// Reads the declarations of a package's test yaml; `source` names it in messages.
    ///
    /// Errors when the yaml does not parse, when a declaration is invalid as the test runner
    /// would reject it (a limit missing a field its kind requires, a tolerance without a reason,
    /// ...), or when a component listed by several tests declares different limits, execution
    /// tolerances or `tvl_only` in them, naming which.
    pub fn from_yaml(reader: impl Read, source: &str) -> miette::Result<Self> {
        let entry: PackageEntry = serde_yaml::from_reader(reader)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read the declarations of {source}"))?;
        let mut problems = Vec::new();
        if let Some(declaration) = &entry.limits {
            check_declaration(declaration, "package", &mut problems);
        }
        if let Some(tolerance) = &entry.execution_tolerance {
            for problem in tolerance.problems() {
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
            for (key, declared) in [
                ("execution_tolerance", &component.execution_tolerance),
                ("tvl_only", &component.tvl_only),
            ] {
                for problem in declared
                    .iter()
                    .flat_map(DeclaredTolerance::problems)
                {
                    problems.push(format!("{location}, {key}: {problem}"));
                }
            }
            let expectations = ComponentExpectations {
                limits: component.limits,
                execution_tolerance: component.execution_tolerance,
                tvl_only: component.tvl_only,
            };
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
        if seen.execution_tolerance != expectations.execution_tolerance {
            differ.push("execution_tolerance");
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

    /// The tolerances executions of this package's pools are checked with.
    pub fn tolerances(&self) -> ExecutionTolerances {
        ExecutionTolerances::new(
            self.harness,
            self.execution_tolerance.as_ref(),
            self.components
                .iter()
                .filter_map(|(id, component)| {
                    component
                        .execution_tolerance
                        .as_ref()
                        .map(|declared| (id.as_str(), declared))
                }),
        )
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
        self.declaration_for(component_id, direction)
            .map(|(limits, _)| limits)
    }

    /// [`Self::limits_for`], with which declaration it comes from.
    pub fn declaration_for(
        &self,
        component_id: &str,
        direction: &Direction,
    ) -> Option<(&Limits, DeclaredBy)> {
        declaration_for(
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
    let mut yamls = Vec::new();
    for path in test_yamls(root, false)? {
        let yaml = fs::read_to_string(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read {}", path.display()))?;
        yamls.push((path.display().to_string(), yaml));
    }
    load_packages_from(yamls, chain)
}

/// [`load_packages`] over yamls already read, each with the name its errors give it: for a
/// runner that carries the yamls instead of a checkout. Merges and errors as `load_packages` does.
pub fn load_packages_from(
    yamls: impl IntoIterator<Item = (String, String)>,
    chain: Chain,
) -> miette::Result<HashMap<String, PackageExpectations>> {
    let mut packages: HashMap<String, (PackageExpectations, String)> = HashMap::new();
    for (source, yaml) in yamls {
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
execution_tolerance: {bps: 3, reason: "vault rounding; 2.1 bps over 100 blocks"}
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
        let usdc = Bytes::from("0x01");
        assert_eq!(package.tolerances().get("0xAA", &usdc), Tolerance { centi_bps: 300, wei: 0 });
        let kind = |component: &str, key: &str| {
            package
                .limits_for(component, &direction(key))
                .map(|limits| limits.kind())
        };
        assert_eq!(kind("0xAA", "0x01->0x02"), Some(LimitKind::Hard));
        assert_eq!(kind("0xAA", "0x02->0x01"), Some(LimitKind::Soft));
        assert_eq!(kind("0xbb", "0x01->0x02"), Some(LimitKind::Soft), "an unlisted pool");
    }

    #[test]
    fn a_package_names_where_each_declaration_comes_from() {
        let package = PackageExpectations::from_yaml(PACKAGE.as_bytes(), "vault").unwrap();
        let by = |component: &str, key: &str| {
            package
                .declaration_for(component, &direction(key))
                .map(|(_, by)| by)
        };
        assert_eq!(by("0xAA", "0x01->0x02"), Some(DeclaredBy::Component));
        assert_eq!(by("0xAA", "0x02->0x01"), Some(DeclaredBy::PackageDefault));
        assert_eq!(by("0xbb", "0x01->0x02"), Some(DeclaredBy::PackageDefault));
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
        let tvl_only = "        tvl_only: {bps: 5, reason: \"fee bucket; 4 bps\"}\n";
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
                .map(|tvl_only| tvl_only.tolerance(&Bytes::from("0x01"))),
            Some(Tolerance { centi_bps: 500, wei: 0 })
        );
    }

    #[test]
    fn declarations_the_runner_rejects_are_rejected() {
        for (from, to, problem) in [
            (
                "{kind: soft, reason: \"any input trades\"}",
                "{kind: soft, reason: \" \"}",
                "package, default: the reason is empty",
            ),
            (
                "{kind: soft, reason: \"any input trades\"}",
                "{kind: soft}",
                "missing field `reason`",
            ),
            ("bps: 3, reason", "bps: 20000, reason", "from 0.01 to 9999.99"),
            (
                "reason: \"vault rounding; 2.1 bps over 100 blocks\"",
                "reason: \" \"",
                "execution_tolerance: the reason is empty",
            ),
            (
                "{kind: hard}",
                "{kind: under_cap, reason: \"r\", lost_capacity: \"2%\"}",
                "unknown field `lost_capacity`",
            ),
        ] {
            let invalid = PACKAGE.replace(from, to);
            let err = PackageExpectations::from_yaml(invalid.as_bytes(), "vault").unwrap_err();
            let err = err
                .chain()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(": ");
            assert!(err.contains(problem), "{to}: {err}");
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
    fn packages_load_the_same_from_memory_as_from_disk() {
        let dir = yaml_dir("memory");
        fs::write(dir.join("integration_test.tycho.yaml"), PACKAGE).unwrap();
        let from_disk = load_packages(&dir, Chain::Ethereum).unwrap();
        let from_memory =
            load_packages_from([("vault".to_string(), PACKAGE.to_string())], Chain::Ethereum)
                .unwrap();
        assert_eq!(from_memory, from_disk);
        assert!(load_packages_from([("vault".to_string(), PACKAGE.to_string())], Chain::Base)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_invalid_yaml_for_another_chain_fails_every_chain() {
        let dir = yaml_dir("other-chain");
        fs::write(dir.join("integration_test.tycho.yaml"), PACKAGE).unwrap();
        let invalid_on_base = PACKAGE
            .replace("./substreams.yaml", "./base-vault.yaml")
            .replace("{kind: soft, reason: \"any input trades\"}", "{kind: soft, reason: \" \"}");
        fs::write(dir.join("integration_test_base.tycho.yaml"), invalid_on_base).unwrap();
        let err = load_packages(&dir, Chain::Ethereum).unwrap_err();
        assert!(
            err.to_string()
                .contains("the reason is empty"),
            "{err}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_mode_keeps_half_a_percent() {
        let legacy = PACKAGE.replace("harness: strict\n", "");
        let package = PackageExpectations::from_yaml(legacy.as_bytes(), "vault").unwrap();
        assert_eq!(package.tolerances(), ExecutionTolerances::legacy());
    }

    fn declared(yaml: &str) -> Result<DeclaredTolerance, String> {
        serde_yaml::from_str(yaml).map_err(|e| e.to_string())
    }

    #[test]
    fn bps_takes_up_to_two_decimals_from_a_hundredth_to_under_100_percent() {
        for (written, centi_bps) in
            [("3", 300), ("0.02", 2), ("\"0.02\"", 2), ("1.5", 150), ("9999.99", 999_999)]
        {
            let tolerance = declared(&format!("{{bps: {written}, reason: r}}")).unwrap();
            assert_eq!(
                tolerance
                    .tolerance(&Bytes::from("0x01"))
                    .centi_bps,
                centi_bps,
                "{written}"
            );
        }
        for written in ["0", "0.00", "0.001", "10000", "-1", "\"1e2\"", "\".5\"", "[1]"] {
            let err = declared(&format!("{{bps: {written}, reason: r}}")).unwrap_err();
            assert!(err.contains("bps"), "{written}: {err}");
        }
    }

    #[test]
    fn wei_is_one_amount_or_one_per_token_with_a_default() {
        let usdc = Bytes::from("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let weth = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let all = declared("{wei: 2, reason: r}").unwrap();
        assert_eq!(all.tolerance(&usdc).wei, 2);
        let per_token = declared(
            "{wei: {default: 1000, \"0xA0b86991c6218b36c1d19d4a2e9eB0cE3606eB48\": 1}, reason: r}",
        )
        .unwrap();
        assert_eq!(per_token.tolerance(&usdc).wei, 1);
        assert_eq!(per_token.tolerance(&weth).wei, 1000);
        let without_default =
            declared("{wei: {\"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48\": 1}, reason: r}")
                .unwrap();
        assert_eq!(without_default.tolerance(&weth), Tolerance::EXACT);

        for (written, problem) in [
            ("0", "nonzero"),
            ("{default: 0}", "nonzero"),
            ("{}", "empty map"),
            ("{usdc: 1}", "not `usdc`"),
            ("{\"0x01\": 1}", "not `0x01`"),
        ] {
            let err = declared(&format!("{{wei: {written}, reason: r}}")).unwrap_err();
            assert!(err.contains(problem), "{written}: {err}");
        }
        assert!(declared("{bps: 1, reason: r, cause: c}")
            .unwrap_err()
            .contains("unknown field `cause`"));
        assert_eq!(
            declared("{reason: r}")
                .unwrap()
                .problems()
                .len(),
            1
        );
    }

    #[test]
    fn a_component_tolerance_overrides_the_package_one_in_strict_mode_only() {
        let token = Bytes::from("0x01");
        let package = declared("{bps: 3, reason: r}").unwrap();
        let component = declared("{wei: 2, reason: r}").unwrap();
        let strict =
            ExecutionTolerances::new(Harness::Strict, Some(&package), [("0xAA", &component)]);
        assert_eq!(strict.get("0xaa", &token), Tolerance { centi_bps: 0, wei: 2 });
        assert_eq!(strict.get("0xbb", &token), Tolerance { centi_bps: 300, wei: 0 });
        assert_eq!(
            ExecutionTolerances::new(Harness::Strict, None, []).get("0xbb", &token),
            Tolerance::EXACT
        );
        assert_eq!(
            ExecutionTolerances::new(Harness::Legacy, Some(&package), [("0xAA", &component)])
                .get("0xaa", &token),
            Tolerance::LEGACY
        );
    }

    #[test]
    fn a_tolerance_prints_its_hundredths_of_a_bps() {
        assert_eq!(Tolerance { centi_bps: 2, wei: 0 }.to_string(), "0.02 bps");
        assert_eq!(Tolerance { centi_bps: 300, wei: 2 }.to_string(), "3 bps or 2 wei");
        assert_eq!(Tolerance::EXACT.to_string(), "exact");
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
