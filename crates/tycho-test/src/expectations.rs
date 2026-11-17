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
    hash::Hash,
    io::Read,
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
    str::FromStr,
};

use itertools::Itertools;
use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use serde::{
    de::{self, DeserializeOwned, Visitor},
    Deserialize, Deserializer, Serialize,
};
use tycho_simulation::tycho_common::{models::Chain, Bytes};

/// Which checks a package's tests run.
///
/// `Legacy` runs the checks the existing packages are written against. `Strict` is for new
/// packages: it rejects keys the runner does not read, and runs every check of a test and reports
/// all failures instead of stopping at the first. The checks added with it (limits, balance
/// lenses, exact execution) run in `Strict` only, so they do not change a legacy package's
/// result. The one exception applies to both:
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

/// What one direction's limit is. Every kind must be quotable and executable at the limit itself.
///
/// ```yaml
/// {kind: hard}
/// {kind: soft, reason: "constant product: any input trades, at a worse price"}
/// {kind: under_cap, reason: "a keeper-set rate we do not index; gives up to 2% of the cap"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Limits {
    /// `max_input` is the largest input the venue accepts (a vault cap, a redemption limit, the
    /// end of the liquidity range), whichever side of the swap the venue bounds. One unit more,
    /// `get_amount_out` must error and execution must revert.
    Hard {},
    /// The venue has no bound on the input, so the limit is a recommended size, as the
    /// `ProtocolSim::get_limits` documentation allows. The reason names the code that shows it.
    /// Twice the limit must still quote.
    Soft { reason: String },
    /// The venue has a bound, and `max_input` stays below it because `get_limits` cannot report
    /// the largest input the venue accepts, such as a bound on the output without an exact
    /// inverse. The reason shows why, and what capacity the limit gives up. One unit past the
    /// limit, the quote and the execution must still succeed.
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
            Limits::Soft { reason } | Limits::UnderCap { reason, .. } => Some(reason),
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredTolerance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bps: Option<Bps>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

impl fmt::Display for Bps {
    /// As the yaml writes it: `3`, `0.5` or `0.02`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        CentiBps::from(self.centi()).fmt(f)
    }
}

/// An amount in hundredths of a basis point, printed in basis points with no trailing zero: `3`,
/// `0.5` or `0.02`. The one formatter of a bps amount: [`Bps`], [`Tolerance`] and the execution
/// mismatch messages print through it.
pub(crate) struct CentiBps(pub BigUint);

impl From<u32> for CentiBps {
    fn from(centi: u32) -> Self {
        CentiBps(BigUint::from(centi))
    }
}

impl fmt::Display for CentiBps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = &self.0 / 100u32;
        let hundredths = (&self.0 % 100u32)
            .to_u32_digits()
            .first()
            .copied()
            .unwrap_or(0);
        match hundredths {
            0 => write!(f, "{whole}"),
            h if h % 10 == 0 => write!(f, "{whole}.{}", h / 10),
            h => write!(f, "{whole}.{h:02}"),
        }
    }
}

impl Serialize for Bps {
    /// As a string, so no decimal is lost to a float.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
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
    /// From the scalar as the yaml writes it, quoted or not, so `0.5` is read as written rather
    /// than through a float.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Written;

        impl Visitor<'_> for Written {
            type Value = Bps;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("`bps`, a number of basis points from 0.01 to 9999.99")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Bps, E> {
                v.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Written)
    }
}

/// A declaration that is one value for every key, or a map of an optional `default` and values
/// keyed by [`KeyedField::Key`], with the default applying to every key without an entry:
///
/// ```yaml
/// wei: 2
/// wei: {default: 1000, "0x<usdc>": 1}
/// limits: {kind: hard}
/// limits: {default: {kind: soft, reason: "..."}, "0x<usdc>->0x<susdc>": {kind: hard}}
/// ```
///
/// Built only by deserializing, which rejects an empty map and a key declared twice, so its fields
/// stay private.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyed<F: KeyedField> {
    /// The value of every key without an entry of its own; the one value, in that form.
    default: Option<F::Value>,
    entries: HashMap<F::Key, F::Value>,
}

/// What a [`Keyed`] declaration holds, and how the yaml writes it.
pub trait KeyedField {
    type Key: fmt::Debug + fmt::Display + Clone + Eq + Hash + Ord;
    type Value: fmt::Debug + Clone + Eq + Serialize + DeserializeOwned;
    /// The declaration's key in the yaml, for messages.
    const NAME: &'static str;
    /// The forms the declaration takes, for a message when it is neither.
    const SHAPE: &'static str;
    /// A key of the value itself, which makes a map one value rather than a map of keys, as
    /// `kind` does for limits; `None` when a value is never a map.
    const VALUE_KEY: Option<&'static str>;

    /// Reads a key other than `default`.
    fn parse_key(key: &str) -> Result<Self::Key, String>;
}

impl<F: KeyedField> Keyed<F> {
    /// The value of every key without an entry of its own, if declared.
    fn default_value(&self) -> Option<&F::Value> {
        self.default.as_ref()
    }

    /// The value of `key`: its own entry, else the default.
    fn value_for(&self, key: &F::Key) -> Option<&F::Value> {
        self.entries
            .get(key)
            .or(self.default.as_ref())
    }

    /// Every declaration with its key as written, `default` first, then the keys in order.
    fn as_written(&self) -> Vec<(String, &F::Value)> {
        let mut entries: Vec<(String, &F::Value)> = self
            .default
            .iter()
            .map(|value| ("default".to_string(), value))
            .collect();
        entries.extend(
            self.entries
                .iter()
                .sorted_by(|a, b| a.0.cmp(b.0))
                .map(|(key, value)| (key.to_string(), value)),
        );
        entries
    }
}

impl<F: KeyedField> Serialize for Keyed<F> {
    /// As the yaml writes it: the one value, or a map of `default` and the keys in order.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match (&self.default, self.entries.is_empty()) {
            (Some(value), true) => value.serialize(serializer),
            _ => {
                let mut map = serializer.serialize_map(None)?;
                for (key, value) in self.as_written() {
                    map.serialize_entry(&key, value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de, F: KeyedField> Deserialize<'de> for Keyed<F> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        let one = |value: serde_yaml::Value| {
            // The shape is restated only when it is what is wrong: a value of the right shape
            // with a bad field names the field.
            let right_shape = match &value {
                serde_yaml::Value::Mapping(_) => true,
                serde_yaml::Value::Bool(_) |
                serde_yaml::Value::Number(_) |
                serde_yaml::Value::String(_) => F::VALUE_KEY.is_none(),
                serde_yaml::Value::Null |
                serde_yaml::Value::Sequence(_) |
                serde_yaml::Value::Tagged(_) => false,
            };
            serde_yaml::from_value(value)
                .map(|value| Self { default: Some(value), entries: HashMap::new() })
                .map_err(|e| {
                    de::Error::custom(if right_shape {
                        format!("`{}`: {e}", F::NAME)
                    } else {
                        format!("`{}` is {}: {e}", F::NAME, F::SHAPE)
                    })
                })
        };
        // A map is the keyed form, unless it holds the key that marks a single value.
        let serde_yaml::Value::Mapping(map) = value else {
            return one(value);
        };
        if F::VALUE_KEY.is_some_and(|key| map.contains_key(key)) {
            return one(serde_yaml::Value::Mapping(map));
        }
        if map.is_empty() {
            return Err(de::Error::custom(format!("`{}` is an empty map", F::NAME)));
        }
        let mut declaration = Self { default: None, entries: HashMap::new() };
        for (key, value) in map {
            let serde_yaml::Value::String(key) = key else {
                return Err(de::Error::custom(format!(
                    "a `{}` key is not a string: {}",
                    F::NAME,
                    yaml_text(&key)
                )));
            };
            let parsed = if key == "default" {
                None
            } else {
                Some(F::parse_key(&key).map_err(de::Error::custom)?)
            };
            let value = serde_yaml::from_value::<F::Value>(value)
                .map_err(|e| de::Error::custom(format!("{} `{key}`: {e}", F::NAME)))?;
            let Some(parsed) = parsed else {
                declaration.default = Some(value);
                continue;
            };
            // Keys written differently, in case or spacing, can be one key.
            if declaration
                .entries
                .insert(parsed.clone(), value)
                .is_some()
            {
                return Err(de::Error::custom(format!("{}: {parsed} is declared twice", F::NAME)));
            }
        }
        Ok(declaration)
    }
}

/// An absolute tolerance in base units of the token: one amount for every token, or an amount per
/// token address with an optional default for the others.
pub type Wei = Keyed<WeiField>;

/// The [`KeyedField`] of [`Wei`]: amounts keyed by token address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeiField;

impl KeyedField for WeiField {
    type Key = Bytes;
    type Value = NonZeroU64;
    const NAME: &'static str = "wei";
    const SHAPE: &'static str = "an amount, or a map of `default` and token addresses to amounts";
    const VALUE_KEY: Option<&'static str> = None;

    fn parse_key(key: &str) -> Result<Bytes, String> {
        Bytes::from_str(key)
            .ok()
            .filter(|token| token.len() == 20)
            .ok_or_else(|| format!("a `wei` key is `default` or a token address, not `{key}`"))
    }
}

impl Wei {
    /// The amount for `token`, 0 when the declaration names no amount for it.
    pub fn of(&self, token: &Bytes) -> u64 {
        self.value_for(token)
            .map_or(0, |wei| wei.get())
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
    /// `exact`, `0.5 bps`, `2 wei` or `0.5 bps or 2 wei`, the bps as [`Bps`] writes them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.centi_bps == 0 && self.wei == 0 {
            return f.write_str("exact");
        }
        if self.centi_bps != 0 {
            write!(f, "{} bps", CentiBps::from(self.centi_bps))?;
            if self.wei != 0 {
                f.write_str(" or ")?;
            }
        }
        if self.wei != 0 {
            write!(f, "{} wei", self.wei)?;
        }
        Ok(())
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
        resolve_tolerance(
            self.legacy,
            self.components
                .get(&component_id.to_lowercase()),
            self.package.as_ref(),
            token_out,
        )
    }
}

/// The tolerance an execution paying `token_out` is checked with: [`Tolerance::LEGACY`] in legacy
/// mode; in strict mode the component's declaration, else the package's, else exact.
fn resolve_tolerance(
    legacy: bool,
    component: Option<&DeclaredTolerance>,
    package: Option<&DeclaredTolerance>,
    token_out: &Bytes,
) -> Tolerance {
    if legacy {
        return Tolerance::LEGACY;
    }
    component
        .or(package)
        .map_or(Tolerance::EXACT, |declared| declared.tolerance(token_out))
}

/// Which `limits` declaration a direction's limit comes from, most specific first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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
///
/// Deserializing rejects a direction declared twice and one between a token and itself.
pub type LimitsDeclaration = Keyed<LimitsField>;

/// The [`KeyedField`] of [`LimitsDeclaration`]: limits keyed by direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitsField;

impl KeyedField for LimitsField {
    type Key = Direction;
    type Value = Limits;
    const NAME: &'static str = "limits";
    const SHAPE: &'static str =
        "`{kind: ...}` or a map of `default` and directions to `{kind: ...}`";
    const VALUE_KEY: Option<&'static str> = Some("kind");

    fn parse_key(key: &str) -> Result<Direction, String> {
        Direction::from_str(key)
    }
}

impl LimitsDeclaration {
    /// The limit of every direction without its own entry, if declared.
    pub fn default_limits(&self) -> Option<&Limits> {
        self.default_value()
    }

    /// The directions with an entry of their own.
    pub fn directions(&self) -> impl Iterator<Item = &Direction> {
        self.entries.keys()
    }

    /// The declaration of `direction`, with `own` when it is the direction's entry and `default`
    /// when it is the declaration's default.
    fn get(
        &self,
        direction: &Direction,
        own: DeclaredBy,
        default: DeclaredBy,
    ) -> Option<(&Limits, DeclaredBy)> {
        match self.entries.get(direction) {
            Some(limits) => Some((limits, own)),
            None => self
                .default_value()
                .map(|limits| (limits, default)),
        }
    }
}

/// Records each entry of `declaration` whose reason is blank. serde already rejects a missing
/// reason and a field the kind does not have.
pub fn check_declaration(
    declaration: &LimitsDeclaration,
    location: &str,
    problems: &mut Vec<String>,
) {
    for (key, limits) in declaration.as_written() {
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
    /// The chain the package indexes, from its `substreams_yaml_path` ([`package_chain`]).
    pub chain: Chain,
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
    substreams_yaml_path: String,
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
    /// Errors when the yaml does not parse, when the file name of its `substreams_yaml_path`
    /// names no chain ([`package_chain`]), when a declaration is invalid on its own (a limit
    /// missing a field its kind requires, a tolerance without a reason, ...), or when a component
    /// listed by several tests declares different limits, execution tolerances or `tvl_only` in
    /// them, naming which. The test runner's own parser checks more: that a strict package has a
    /// package-level `limits` default, and that each declared direction is one of a component's;
    /// a repository test runs it on every package yaml.
    pub fn from_yaml(reader: impl Read, source: &str) -> miette::Result<Self> {
        let entry: PackageEntry = serde_yaml::from_reader(reader)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read the declarations of {source}"))?;
        let chain =
            package_chain(&entry.substreams_yaml_path).map_err(|e| miette!("{source}: {e}"))?;
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
            chain,
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

    /// The tolerance of an execution of `component_id` paying `token_out`, as
    /// [`Self::tolerances`] gives it, without building them all: in legacy mode
    /// [`Tolerance::LEGACY`]; in strict mode the component's `execution_tolerance`, else the
    /// package's, else exact.
    pub fn tolerance(&self, component_id: &str, token_out: &Bytes) -> Tolerance {
        resolve_tolerance(
            self.harness == Harness::Legacy,
            self.component(component_id)
                .and_then(|component| component.execution_tolerance.as_ref()),
            self.execution_tolerance.as_ref(),
            token_out,
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
pub fn load_packages_from<S: AsRef<str>, Y: AsRef<str>>(
    yamls: impl IntoIterator<Item = (S, Y)>,
    chain: Chain,
) -> miette::Result<HashMap<String, PackageExpectations>> {
    let mut packages: HashMap<String, (PackageExpectations, String)> = HashMap::new();
    for (source, yaml) in yamls {
        let source = source.as_ref().to_string();
        // Read before the chain is filtered, so an invalid yaml fails every chain, as documented.
        let package = PackageExpectations::from_yaml(yaml.as_ref().as_bytes(), &source)?;
        if package.chain != chain {
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
            // A component's own declarations are checked as the package's are.
            (
                "{kind: hard}",
                "{kind: soft, reason: \" \"}",
                "component 0xAA, 0x01->0x02: the reason is empty",
            ),
            (
                "        limits:\n          \"0x01",
                "        tvl_only: {bps: 5, reason: \" \"}\n        limits:\n          \"0x01",
                "component 0xAA, tvl_only: the reason is empty",
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

        // A component listed by two tests declares the same in both.
        let (first, second) = PACKAGE.split_at(
            PACKAGE
                .find("  - name: second")
                .unwrap(),
        );
        let differs =
            format!("{first}{}", second.replace("{kind: hard}", "{kind: soft, reason: \"r\"}"));
        let err = PackageExpectations::from_yaml(differs.as_bytes(), "vault")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("component 0xaa declares different limits, in several tests"),
            "{err}"
        );
    }

    /// The packages of `yamls` on Ethereum, each yaml named by its index.
    fn ethereum_packages(yamls: &[&str]) -> miette::Result<HashMap<String, PackageExpectations>> {
        load_packages_from(
            yamls
                .iter()
                .enumerate()
                .map(|(i, yaml)| (format!("yaml {i}"), *yaml)),
            Chain::Ethereum,
        )
    }

    #[test]
    fn two_yamls_of_one_system_merge_their_components() {
        let other_pool = PACKAGE
            .replace("0xAA", "0xBB")
            .replace("0xaa", "0xbb");
        let packages = ethereum_packages(&[PACKAGE, &other_pool]).unwrap();
        let vault = &packages["vault"];
        assert!(vault.component("0xaa").is_some() && vault.component("0xbb").is_some());

        for (from, to, differs) in [
            (
                "limits: {kind: soft, reason: \"any input trades\"}",
                "limits: {kind: hard}",
                "different package limits",
            ),
            ("harness: strict", "harness: legacy", "different package harness"),
            ("bps: 3, reason", "bps: 4, reason", "different package execution_tolerance"),
        ] {
            let err = ethereum_packages(&[PACKAGE, &other_pool.replace(from, to)]).unwrap_err();
            assert!(err.to_string().contains(differs), "{err}");
        }

        // A pool both yamls list must be declared the same in both.
        let shared = PACKAGE.replace("{kind: hard}", "{kind: soft, reason: \"r\"}");
        let err = ethereum_packages(&[PACKAGE, &shared]).unwrap_err();
        assert!(
            err.to_string()
                .contains("declares different limits"),
            "{err}"
        );
        assert!(ethereum_packages(&[PACKAGE, PACKAGE]).is_ok());
    }

    #[test]
    fn packages_are_kept_for_their_own_chain() {
        let packages = ethereum_packages(&[PACKAGE]).unwrap();
        assert_eq!(packages.keys().collect::<Vec<_>>(), ["vault"]);
        let vault = &packages["vault"];
        assert_eq!(vault.chain, Chain::Ethereum);
        assert_eq!(vault.limits_for("0xAA", &direction("0x01->0x02")), Some(&Limits::Hard {}));
        assert!(load_packages_from([("vault", PACKAGE)], Chain::Base)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_invalid_yaml_for_another_chain_fails_every_chain() {
        let invalid_on_base = PACKAGE
            .replace("./substreams.yaml", "./base-vault.yaml")
            .replace("{kind: soft, reason: \"any input trades\"}", "{kind: soft, reason: \" \"}");
        let err = ethereum_packages(&[PACKAGE, &invalid_on_base]).unwrap_err();
        assert!(
            err.to_string()
                .contains("the reason is empty"),
            "{err}"
        );
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
        // Decimals a float does not hold exactly are read as written, quoted or not.
        for (written, centi_bps) in [
            ("3", 300),
            ("0.02", 2),
            ("\"0.02\"", 2),
            ("1.5", 150),
            ("9999.99", 999_999),
            ("0.07", 7),
            ("0.29", 29),
            ("1.10", 110),
            ("\"0.5\"", 50),
            ("'2.01'", 201),
            ("4097.73", 409_773),
        ] {
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
    fn a_component_tolerance_read_from_yaml_overrides_the_package_one_in_strict_mode_only() {
        // PACKAGE declares 3 bps for the package; 0xAA adds its own, in both tests.
        let own = "        execution_tolerance: {wei: 2, reason: \"rounding\"}\n        limits:\n";
        let yaml = PACKAGE.replace("        limits:\n", own);
        let token = Bytes::from("0x02");
        let strict = PackageExpectations::from_yaml(yaml.as_bytes(), "vault").unwrap();
        let legacy = PackageExpectations::from_yaml(
            yaml.replace("harness: strict\n", "")
                .as_bytes(),
            "vault",
        )
        .unwrap();
        let exact = PackageExpectations::from_yaml(
            yaml.replace(
                "execution_tolerance: {bps: 3, reason: \"vault rounding; 2.1 bps over 100 blocks\"}\n",
                "",
            )
            .as_bytes(),
            "vault",
        )
        .unwrap();
        for (package, component, expected) in [
            (&strict, "0xaa", Tolerance { centi_bps: 0, wei: 2 }),
            (&strict, "0xAA", Tolerance { centi_bps: 0, wei: 2 }),
            (&strict, "0xbb", Tolerance { centi_bps: 300, wei: 0 }),
            (&legacy, "0xaa", Tolerance::LEGACY),
            (&legacy, "0xbb", Tolerance::LEGACY),
            (&exact, "0xaa", Tolerance { centi_bps: 0, wei: 2 }),
            (&exact, "0xbb", Tolerance::EXACT),
        ] {
            assert_eq!(
                package
                    .tolerances()
                    .get(component, &token),
                expected,
                "{component}"
            );
            assert_eq!(package.tolerance(component, &token), expected, "{component}");
        }
    }

    #[test]
    fn a_declaration_of_the_right_shape_names_its_bad_field_not_the_shape() {
        let err = serde_yaml::from_str::<LimitsDeclaration>("{kind: soft}")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("`limits`: missing field `reason`"), "{err}");
        let err = serde_yaml::from_str::<LimitsDeclaration>("5")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("`limits` is `{kind: ...}`"), "{err}");
        let err = declared("{wei: 0, reason: r}").unwrap_err();
        assert!(err.contains("`wei`: invalid value"), "{err}");
    }

    #[test]
    fn a_limits_or_wei_map_is_a_default_and_keys() {
        let limits: LimitsDeclaration = serde_yaml::from_str(
            "{default: {kind: hard}, \"0x01->0x02\": {kind: soft, reason: r}}",
        )
        .unwrap();
        assert_eq!(limits.default_limits(), Some(&Limits::Hard {}));
        assert_eq!(limits.directions().collect::<Vec<_>>(), [&direction("0x01->0x02")]);
        // One value and a map of only `default` are the same declaration.
        let one: Wei = serde_yaml::from_str("2").unwrap();
        let only_default: Wei = serde_yaml::from_str("{default: 2}").unwrap();
        assert_eq!(one, only_default);
        assert_eq!(serde_json::to_value(&only_default).unwrap(), serde_json::json!(2));
        for (yaml, problem) in [
            ("{\"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48\": 1, \"0xA0B86991C6218B36C1D19D4A2E9EB0CE3606EB48\": 2}", "is declared twice"),
            ("{1: 2}", "a `wei` key is not a string: `1`"),
            ("{default: x}", "wei `default`: invalid type"),
            ("[1]", "`wei` is an amount, or a map"),
        ] {
            let err = serde_yaml::from_str::<Wei>(yaml).unwrap_err().to_string();
            assert!(err.contains(problem), "{yaml}: {err}");
        }
        assert!(serde_yaml::from_str::<LimitsDeclaration>("{}")
            .unwrap_err()
            .to_string()
            .contains("`limits` is an empty map"));
    }

    #[test]
    fn a_declaration_serializes_as_the_yaml_writes_it() {
        let tolerance = declared(
            "{bps: 0.5, wei: {default: 2, \"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48\": 1}, \
             reason: r}",
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&tolerance).unwrap(),
            serde_json::json!({
                "bps": "0.5",
                "wei": {"default": 2, "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48": 1},
                "reason": "r",
            })
        );
        assert_eq!(
            serde_json::to_value(declared("{bps: 0.02, reason: r}").unwrap()).unwrap(),
            serde_json::json!({"bps": "0.02", "reason": "r"})
        );
        assert_eq!(
            serde_json::to_value(declared("{wei: 3, reason: r}").unwrap()).unwrap(),
            serde_json::json!({"wei": 3, "reason": "r"})
        );
    }

    #[test]
    fn a_tolerance_prints_its_hundredths_of_a_bps() {
        assert_eq!(Tolerance { centi_bps: 2, wei: 0 }.to_string(), "0.02 bps");
        assert_eq!(Tolerance { centi_bps: 300, wei: 2 }.to_string(), "3 bps or 2 wei");
        assert_eq!(Tolerance::EXACT.to_string(), "exact");
        assert_eq!(Tolerance { centi_bps: 50, wei: 0 }.to_string(), "0.5 bps");
        assert_eq!(Tolerance { centi_bps: 0, wei: 2 }.to_string(), "2 wei");
        // The same text as the declaration it comes from.
        let declared = declared("{bps: 0.5, reason: r}").unwrap();
        assert_eq!(
            declared
                .tolerance(&Bytes::from("0x01"))
                .to_string(),
            format!("{} bps", declared.bps.unwrap())
        );
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
            let yaml = fs::read_to_string(&path).unwrap();
            if let Err(e) =
                PackageExpectations::from_yaml(yaml.as_bytes(), &path.display().to_string())
            {
                panic!("{e:?}");
            }
        }
    }
}
