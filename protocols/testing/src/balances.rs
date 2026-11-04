//! Strict mode's balance check: each expected component's indexed balances against the chain, read
//! from the source the component declares.
//!
//! The indexer stores, per component and token, the balance the component holds. Where the tokens
//! sit in the component's own contract, that is `balanceOf(component id)`, today's check. Where
//! they sit elsewhere, such as a shared vault, or the indexed amount is bounded by several
//! contracts, the component declares where to read it:
//!
//! ```yaml
//! balance_check: default                              # balanceOf(component id)
//! balance_check: {holder: "attribute:vault"}          # the address in an attribute
//! balance_check: {holder: "0x…"}                      # a fixed address
//! balance_check: {min_of: [self, "attribute:vault"]}  # the smallest of several
//! balance_check:                                      # per token; unlisted tokens: self
//!   tokens:
//!     "0x<USDC>": {holder: "0x<pocket>"}
//!     "0x<USDS>": {holder: "0x<PSM>", token: "0x<DAI>"}   # another token's balance
//! ```

use std::{collections::HashMap, fmt, str::FromStr};

use alloy::{
    dyn_abi::{DynSolType, DynSolValue},
    json_abi::Function,
};
use num_bigint::BigUint;
use num_traits::Zero;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use tycho_simulation::tycho_common::Bytes;

use crate::config::Tolerance;

/// Where a token balance is read on chain: `balanceOf(holder)`, or the native balance for the
/// zero address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holder {
    /// The component's id, which must be an address.
    Component,
    /// The address in the named attribute: the component's static attributes first, then its
    /// state attributes at the snapshot block.
    Attribute(String),
    /// A fixed address.
    Address(Bytes),
}

impl FromStr for Holder {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "self" {
            return Ok(Holder::Component);
        }
        if let Some(name) = s.strip_prefix("attribute:") {
            if name.trim().is_empty() {
                return Err("`attribute:` needs an attribute name".to_string());
            }
            return Ok(Holder::Attribute(name.to_string()));
        }
        if s.starts_with("0x") {
            let address = Bytes::from_str(s).map_err(|e| format!("`{s}` is not hex: {e}"))?;
            if address.len() != 20 {
                return Err(format!("`{s}` is not a 20-byte address"));
            }
            return Ok(Holder::Address(address));
        }
        Err(format!("`{s}` is not a holder: write `self`, `attribute:NAME` or a 0x address"))
    }
}

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Holder::Component => f.write_str("self"),
            Holder::Attribute(name) => write!(f, "attribute:{name}"),
            Holder::Address(address) => write!(f, "{address}"),
        }
    }
}

impl<'de> Deserialize<'de> for Holder {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl Serialize for Holder {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Where a component's indexed balances are checked against.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BalanceCheck {
    /// `balanceOf(component id)`.
    #[default]
    Default,
    /// `balanceOf(holder)`.
    Holder(Holder),
    /// The smallest `balanceOf` of the holders: the amount is bounded by each of them.
    MinOf(Vec<Holder>),
    /// A source per token; a token not listed is read from the component itself.
    Tokens(HashMap<Bytes, TokenSource>),
    /// One view call for every token, typically taking the token as an argument: a singleton
    /// vault's per-pool, per-token view.
    Call(Call),
}

/// Where one token of a [`BalanceCheck::Tokens`] declaration is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// A balance held by an address.
    Held(Source),
    /// A view call returning the amount, for a balance that is protocol accounting rather than a
    /// token balance any address holds.
    Call(Call),
    /// The indexer reports no balance for this token, for the reason given; the check is that it
    /// stays so. A component's balances need not cover every token it trades.
    Unindexed(String),
}

/// A view call whose `uint` result is a token's balance:
///
/// ```yaml
/// call: {to: "0x<vat>", fn: "dai(address)", args: ["0x<join>"], divide_by: "1e27"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub to: Holder,
    /// The function's signature, `name(type,...)`.
    pub function: String,
    /// One per parameter: `self` (the component id), `token` (the token being checked),
    /// `attribute:NAME`, or a literal the parameter's type parses.
    pub args: Vec<String>,
    /// Divides the result, rounding down, for accounting kept at a higher precision than the
    /// token's.
    pub divide_by: Option<BigUint>,
}

/// What the balance check reads from the chain, at the snapshot block.
pub trait Chain {
    /// `token.balanceOf(holder)`, or the native balance for the zero address.
    fn balance_of(&self, token: &Bytes, holder: &Bytes) -> Result<BigUint, String>;
    /// The return data of an `eth_call` to `to` with `calldata`.
    fn call(&self, to: &Bytes, calldata: &Bytes) -> Result<Bytes, String>;
}

impl Call {
    fn parse(entry: CallEntry) -> Result<Self, String> {
        let CallEntry { to, function, args, divide_by } = entry;
        let parsed = parse_function(&function)?;
        let args: Vec<String> = args
            .into_iter()
            .map(|arg| match arg {
                serde_yaml::Value::String(arg) => Ok(arg),
                serde_yaml::Value::Number(arg) => Ok(arg.to_string()),
                serde_yaml::Value::Null |
                serde_yaml::Value::Bool(_) |
                serde_yaml::Value::Sequence(_) |
                serde_yaml::Value::Mapping(_) |
                serde_yaml::Value::Tagged(_) => {
                    Err(format!("`{function}`: an argument must be a string or a number"))
                }
            })
            .collect::<Result<_, _>>()?;
        if args.len() != parsed.inputs.len() {
            return Err(format!(
                "`{function}` takes {} arguments, but `args` has {}",
                parsed.inputs.len(),
                args.len()
            ));
        }
        // Literals are checked now; `self` and attributes only exist at the snapshot.
        for (arg, input) in args.iter().zip(&parsed.inputs) {
            if arg != "self" && arg != "token" && !arg.starts_with("attribute:") {
                coerce(&input.ty, arg).map_err(|e| format!("`{function}`: {e}"))?;
            }
        }
        let divide_by = divide_by
            .map(|divisor| parse_divisor(&divisor))
            .transpose()?;
        Ok(Self { to, function, args, divide_by })
    }

    /// The calldata for this component.
    fn calldata(&self, component: &Component<'_>, token: &Bytes) -> Result<Bytes, String> {
        let function = parse_function(&self.function)?;
        let values = self
            .args
            .iter()
            .zip(&function.inputs)
            .map(|(arg, input)| {
                let literal = match arg.as_str() {
                    "self" => component.id.to_string(),
                    "token" => token.to_string(),
                    arg => match arg.strip_prefix("attribute:") {
                        Some(name) => {
                            let value = attribute(name, component)?;
                            match (input.ty.as_str(), address_from_attribute(value)) {
                                ("address", Some(address)) => address.to_string(),
                                _ => value.to_string(),
                            }
                        }
                        None => arg.to_string(),
                    },
                };
                coerce(&input.ty, &literal).map_err(|e| format!("`{}`: {e}", self.function))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut calldata = function.selector().to_vec();
        calldata.extend(DynSolValue::Tuple(values).abi_encode_params());
        Ok(Bytes::from(calldata))
    }

    /// Reads the amount: the call's first return word, divided if declared.
    fn read(
        &self,
        component: &Component<'_>,
        token: &Bytes,
        chain: &impl Chain,
    ) -> Result<BigUint, String> {
        let to = resolve(&self.to, component)?;
        let returned = chain.call(&to, &self.calldata(component, token)?)?;
        if returned.len() < 32 {
            return Err(format!("{self} returned {} bytes, not a uint", returned.len()));
        }
        let amount = BigUint::from_bytes_be(&returned[..32]);
        Ok(match &self.divide_by {
            Some(divisor) => amount / divisor,
            None => amount,
        })
    }
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self
            .function
            .split('(')
            .next()
            .unwrap_or_default();
        write!(f, "{}.{name}({})", self.to, self.args.join(", "))?;
        if let Some(divisor) = &self.divide_by {
            write!(f, " / {divisor}")?;
        }
        Ok(())
    }
}

/// Parses `name(type,...)` as a function returning a `uint256`.
fn parse_function(signature: &str) -> Result<Function, String> {
    let function = Function::parse(signature)
        .map_err(|e| format!("`{signature}` is not a function signature: {e}"))?;
    Ok(function)
}

fn coerce(ty: &str, literal: &str) -> Result<DynSolValue, String> {
    let ty = DynSolType::parse(ty).map_err(|e| format!("type `{ty}`: {e}"))?;
    ty.coerce_str(literal)
        .map_err(|e| format!("`{literal}` does not parse as `{ty}`: {e}"))
}

/// A divisor written as an integer or as `1eN`.
fn parse_divisor(divisor: &str) -> Result<BigUint, String> {
    let parsed = match divisor.strip_prefix("1e") {
        Some(exponent) => exponent
            .parse::<u32>()
            .ok()
            .map(|exponent| BigUint::from(10u32).pow(exponent)),
        None => divisor.parse::<BigUint>().ok(),
    };
    match parsed {
        Some(divisor) if divisor > BigUint::from(1u32) => Ok(divisor),
        Some(_) => Err(format!("`divide_by: {divisor}` must be more than 1; leave it out instead")),
        None => Err(format!("`divide_by: {divisor}` is not an integer or `1eN`")),
    }
}

/// A `call`, as the yaml writes it.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CallEntry {
    to: Holder,
    #[serde(rename = "fn")]
    function: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<serde_yaml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    divide_by: Option<String>,
}

/// Where one token's indexed balance is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The smallest balance of these holders.
    pub holders: Vec<Holder>,
    /// The token whose balance is read, when it is not the indexed token itself: a wrapper
    /// whose inventory is another contract's balance of the underlying token.
    pub token: Option<Bytes>,
}

impl Source {
    fn of(holders: Vec<Holder>) -> Self {
        Self { holders, token: None }
    }
}

impl BalanceCheck {
    /// Where `token` is read.
    fn source(&self, token: &Bytes) -> Source {
        match self {
            BalanceCheck::Default => Source::of(vec![Holder::Component]),
            BalanceCheck::Holder(holder) => Source::of(vec![holder.clone()]),
            BalanceCheck::MinOf(holders) => Source::of(holders.clone()),
            BalanceCheck::Tokens(sources) => match sources.get(token) {
                Some(TokenSource::Held(source)) => source.clone(),
                // `check_component` reads calls before asking for a held source.
                Some(TokenSource::Call(_) | TokenSource::Unindexed(_)) | None => {
                    Source::of(vec![Holder::Component])
                }
            },
            // `check_component` reads calls before asking for a held source.
            BalanceCheck::Call(_) => Source::of(vec![Holder::Component]),
        }
    }

    fn call(&self, token: &Bytes) -> Option<&Call> {
        match self {
            BalanceCheck::Tokens(sources) => match sources.get(token) {
                Some(TokenSource::Call(call)) => Some(call),
                Some(TokenSource::Held(_) | TokenSource::Unindexed(_)) | None => None,
            },
            BalanceCheck::Call(call) => Some(call),
            BalanceCheck::Default | BalanceCheck::Holder(_) | BalanceCheck::MinOf(_) => None,
        }
    }
}

const SHAPE: &str = "`default`, `{holder: …}`, `{min_of: [...]}`, `{call: …}` or `{tokens: \
                     {0x<token>: {holder: …} | {call: …}, ...}}`";

/// The keys of a yaml mapping, in order.
fn keys(map: &serde_yaml::Mapping) -> Vec<String> {
    map.keys()
        .map(|key| key.as_str().unwrap_or("?").to_string())
        .collect()
}

fn from_value<T: serde::de::DeserializeOwned, E: de::Error>(
    map: &serde_yaml::Mapping,
    key: &str,
) -> Result<T, E> {
    serde_yaml::from_value(
        map.get(key)
            .cloned()
            .unwrap_or_default(),
    )
    .map_err(E::custom)
}

/// Reads the holders of a `{holder: …}` or `{min_of: [...]}` mapping, ignoring `other` keys.
fn holders<E: de::Error>(
    map: &serde_yaml::Mapping,
    other: &[&str],
    location: &str,
) -> Result<Vec<Holder>, E> {
    let keys = keys(map);
    let own: Vec<&str> = keys
        .iter()
        .map(String::as_str)
        .filter(|key| !other.contains(key))
        .collect();
    match own[..] {
        ["holder"] => Ok(vec![from_value(map, "holder")?]),
        ["min_of"] => {
            let holders: Vec<Holder> = from_value(map, "min_of")?;
            if holders.len() < 2 {
                return Err(E::custom(format!(
                    "{location}: `min_of` needs at least two holders; for one, write `{{holder: …}}`"
                )));
            }
            Ok(holders)
        }
        _ => Err(E::custom(format!("{location} has keys {}; it takes {SHAPE}", keys.join(", ")))),
    }
}

impl<'de> Deserialize<'de> for BalanceCheck {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Through a yaml value first, so an unknown key or a bad holder gets its own message
        // rather than serde's "did not match any variant".
        let value = serde_yaml::Value::deserialize(deserializer)?;
        let map = match &value {
            serde_yaml::Value::String(name) if name == "default" => {
                return Ok(BalanceCheck::Default)
            }
            serde_yaml::Value::Mapping(map) => map,
            serde_yaml::Value::String(_) |
            serde_yaml::Value::Null |
            serde_yaml::Value::Bool(_) |
            serde_yaml::Value::Number(_) |
            serde_yaml::Value::Sequence(_) |
            serde_yaml::Value::Tagged(_) => {
                return Err(de::Error::custom(format!("`balance_check` takes {SHAPE}")))
            }
        };
        if map.contains_key("call") {
            if keys(map) != ["call"] {
                return Err(de::Error::custom(format!(
                    "`balance_check` has keys {}; `call` stands alone",
                    keys(map).join(", ")
                )));
            }
            let entry: CallEntry = from_value(map, "call")
                .map_err(|e: D::Error| de::Error::custom(format!("`balance_check`: {e}")))?;
            return Call::parse(entry)
                .map(BalanceCheck::Call)
                .map_err(|e| de::Error::custom(format!("`balance_check`: {e}")));
        }
        if keys(map) != ["tokens"] {
            let mut holders = holders(map, &[], "`balance_check`")?;
            return Ok(if holders.len() == 1 {
                BalanceCheck::Holder(holders.remove(0))
            } else {
                BalanceCheck::MinOf(holders)
            });
        }
        let Some(serde_yaml::Value::Mapping(tokens)) = map.get("tokens") else {
            return Err(de::Error::custom(
                "`balance_check: {tokens: …}` maps token addresses to sources",
            ));
        };
        let mut sources = HashMap::new();
        for (token, source) in tokens {
            let token_key = token.as_str().unwrap_or("?");
            let location = format!("`balance_check` of token {token_key}");
            let address = parse_address(token_key).map_err(de::Error::custom)?;
            let serde_yaml::Value::Mapping(source) = source else {
                return Err(de::Error::custom(format!(
                    "{location} takes `{{holder: …}}` or `{{call: …}}`"
                )));
            };
            if source.contains_key("unindexed") {
                let reason = match (keys(source).as_slice(), source.get("unindexed")) {
                    ([only], Some(serde_yaml::Value::Mapping(entry)))
                        if only == "unindexed" && keys(entry) == ["reason"] =>
                    {
                        entry
                            .get("reason")
                            .and_then(serde_yaml::Value::as_str)
                            .filter(|reason| !reason.trim().is_empty())
                    }
                    _ => None,
                };
                let Some(reason) = reason else {
                    return Err(de::Error::custom(format!(
                        "{location}: write `{{unindexed: {{reason: \"...\"}}}}`, alone"
                    )));
                };
                sources.insert(address, TokenSource::Unindexed(reason.to_string()));
                continue;
            }
            if source.contains_key("call") {
                if keys(source) != ["call"] {
                    return Err(de::Error::custom(format!(
                        "{location} has keys {}; `call` stands alone",
                        keys(source).join(", ")
                    )));
                }
                let entry: CallEntry = from_value(source, "call")
                    .map_err(|e: D::Error| de::Error::custom(format!("{location}: {e}")))?;
                let call = Call::parse(entry)
                    .map_err(|e| de::Error::custom(format!("{location}: {e}")))?;
                sources.insert(address, TokenSource::Call(call));
                continue;
            }
            let token = match source.get("token") {
                Some(read) => {
                    Some(parse_address(read.as_str().unwrap_or("?")).map_err(de::Error::custom)?)
                }
                None => None,
            };
            let holders = holders(source, &["token"], &location)?;
            sources.insert(address, TokenSource::Held(Source { holders, token }));
        }
        Ok(BalanceCheck::Tokens(sources))
    }
}

fn parse_address(s: &str) -> Result<Bytes, String> {
    match s.parse::<Holder>()? {
        Holder::Address(address) => Ok(address),
        Holder::Component | Holder::Attribute(_) => Err(format!("`{s}` is not a token address")),
    }
}

fn source_value(source: &Source) -> serde_yaml::Value {
    let mut map = serde_yaml::Mapping::new();
    let holders: Vec<serde_yaml::Value> = source
        .holders
        .iter()
        .map(|holder| holder.to_string().into())
        .collect();
    match &holders[..] {
        [holder] => map.insert("holder".into(), holder.clone()),
        _ => map.insert("min_of".into(), holders.into()),
    };
    if let Some(token) = &source.token {
        map.insert("token".into(), token.to_string().into());
    }
    map.into()
}

fn call_value(call: &Call) -> serde_yaml::Value {
    let entry = CallEntry {
        to: call.to.clone(),
        function: call.function.clone(),
        args: call
            .args
            .iter()
            .map(|arg| arg.as_str().into())
            .collect(),
        divide_by: call
            .divide_by
            .as_ref()
            .map(BigUint::to_string),
    };
    let mut map = serde_yaml::Mapping::new();
    map.insert("call".into(), serde_yaml::to_value(entry).expect("a call entry serializes"));
    map.into()
}

impl Serialize for BalanceCheck {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value: serde_yaml::Value = match self {
            BalanceCheck::Default => "default".into(),
            BalanceCheck::Holder(holder) => source_value(&Source::of(vec![holder.clone()])),
            BalanceCheck::MinOf(holders) => source_value(&Source::of(holders.clone())),
            BalanceCheck::Call(call) => call_value(call),
            BalanceCheck::Tokens(sources) => {
                let mut tokens: Vec<(&Bytes, &TokenSource)> = sources.iter().collect();
                tokens.sort_by_key(|(token, _)| *token);
                let tokens: serde_yaml::Mapping = tokens
                    .into_iter()
                    .map(|(token, source)| {
                        let value = match source {
                            TokenSource::Held(source) => source_value(source),
                            TokenSource::Call(call) => call_value(call),
                            TokenSource::Unindexed(reason) => {
                                let mut entry = serde_yaml::Mapping::new();
                                entry.insert("reason".into(), reason.as_str().into());
                                let mut map = serde_yaml::Mapping::new();
                                map.insert("unindexed".into(), entry.into());
                                map.into()
                            }
                        };
                        (token.to_string().into(), value)
                    })
                    .collect();
                let mut map = serde_yaml::Mapping::new();
                map.insert("tokens".into(), tokens.into());
                map.into()
            }
        };
        value.serialize(serializer)
    }
}

/// One expected component, as the balance check sees it.
pub struct Component<'a> {
    pub id: &'a str,
    /// `(address, symbol)` of each of its tokens.
    pub tokens: &'a [(Bytes, String)],
    /// Its indexed balances, by token address.
    pub balances: &'a HashMap<Bytes, Bytes>,
    pub static_attributes: &'a HashMap<String, Bytes>,
    pub state_attributes: &'a HashMap<String, Bytes>,
}

/// Checks a component's indexed balance of every token against the declared source, within
/// `tolerance`: exact, unless the component is declared `tvl_only`. Returns every mismatch, or the
/// reason a balance could not be read. A token declared unindexed must have no balance, whatever
/// the tolerance.
///
/// `chain` reads the chain at the snapshot block.
pub fn check_component(
    component: &Component<'_>,
    check: &BalanceCheck,
    tolerance: Tolerance,
    chain: &impl Chain,
) -> Vec<String> {
    let outside = match tolerance {
        Tolerance::EXACT => String::new(),
        tolerance => format!(", outside the `tvl_only` tolerance of {tolerance}"),
    };
    let mut failures = Vec::new();
    if let BalanceCheck::Tokens(sources) = check {
        for token in sources.keys() {
            if !component
                .tokens
                .iter()
                .any(|(address, _)| address == token)
            {
                failures.push(format!(
                    "`balance_check` declares a source for {token}, which is not one of the \
                     component's tokens"
                ));
            }
        }
    }
    for (token, symbol) in component.tokens {
        let indexed = component
            .balances
            .get(token)
            .map_or_else(BigUint::default, |balance| BigUint::from_bytes_be(balance));
        if let BalanceCheck::Tokens(sources) = check {
            if let Some(TokenSource::Unindexed(_)) = sources.get(token) {
                let indexed = component
                    .balances
                    .get(token)
                    .map(|balance| BigUint::from_bytes_be(balance))
                    .filter(|balance| !balance.is_zero());
                if let Some(indexed) = indexed {
                    failures.push(format!(
                        "{symbol}: declared unindexed, but the indexer reports {indexed}"
                    ));
                }
                continue;
            }
        }
        if let Some(call) = check.call(token) {
            match call.read(component, token, chain) {
                Ok(on_chain) if within(&indexed, &on_chain, tolerance) => {}
                Ok(on_chain) => failures.push(format!(
                    "{symbol}: indexed {indexed}, but {call} is {on_chain} on chain{outside}"
                )),
                Err(e) => failures.push(format!("{symbol}: calling {call} failed: {e}")),
            }
            continue;
        }
        let source = check.source(token);
        let read = source.token.as_ref().unwrap_or(token);
        let of_token = source
            .token
            .as_ref()
            .map_or_else(String::new, |read| format!(" of {read}"));
        let mut smallest: Option<(BigUint, &Holder)> = None;
        let mut failed = false;
        for holder in &source.holders {
            let balance = resolve(holder, component).and_then(|address| {
                chain
                    .balance_of(read, &address)
                    .map_err(|e| format!("reading the balance{of_token} of {holder} failed: {e}"))
            });
            match balance {
                Ok(balance) => {
                    if smallest
                        .as_ref()
                        .is_none_or(|(least, _)| balance < *least)
                    {
                        smallest = Some((balance, holder));
                    }
                }
                Err(e) => {
                    failures.push(format!("{symbol}: {e}"));
                    failed = true;
                    break;
                }
            }
        }
        if let (false, Some((on_chain, holder))) = (failed, smallest) {
            if !within(&indexed, &on_chain, tolerance) {
                failures.push(format!(
                    "{symbol}: indexed {indexed}, but {holder} holds {on_chain}{of_token} on \
                     chain{}{outside}",
                    if source.holders.len() > 1 { " (the smallest of the holders)" } else { "" }
                ));
            }
        }
    }
    failures
}

/// Whether `indexed` is within `tolerance` of `on_chain`: up to `bps` basis points of the
/// on-chain amount, or up to `wei` base units.
fn within(indexed: &BigUint, on_chain: &BigUint, tolerance: Tolerance) -> bool {
    let difference = if indexed > on_chain { indexed - on_chain } else { on_chain - indexed };
    difference <= BigUint::from(tolerance.wei) ||
        &difference * 10_000u32 <= on_chain * tolerance.bps
}

/// The address a holder names, for this component.
fn resolve(holder: &Holder, component: &Component<'_>) -> Result<Bytes, String> {
    match holder {
        Holder::Component => {
            let id = Bytes::from_str(component.id)
                .ok()
                .filter(|id| id.len() == 20);
            id.ok_or_else(|| {
                format!(
                    "the component id {} is not an address, so it holds no balance: declare \
                     `balance_check: {{holder: …}}`",
                    component.id
                )
            })
        }
        Holder::Attribute(name) => {
            let value = attribute(name, component)?;
            address_from_attribute(value)
                .ok_or_else(|| format!("the holder attribute `{name}` is {value}, not an address"))
        }
        Holder::Address(address) => Ok(address.clone()),
    }
}

/// The named attribute: the static one, else the state one at the snapshot block.
fn attribute<'a>(name: &str, component: &'a Component<'_>) -> Result<&'a Bytes, String> {
    component
        .static_attributes
        .get(name)
        .or_else(|| component.state_attributes.get(name))
        .ok_or_else(|| {
            format!("the attribute `{name}` is in neither the static nor the state attributes")
        })
}

/// An address stored in an attribute: 20 bytes, or 32 with the address in the low 20 and zeros
/// above, as an ABI-encoded address is.
fn address_from_attribute(value: &Bytes) -> Option<Bytes> {
    match value.len() {
        20 => Some(value.clone()),
        32 if value[..12]
            .iter()
            .all(|byte| *byte == 0) =>
        {
            Some(Bytes::from(value[12..].to_vec()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: &str = "0x00000000000000000000000000000000000000aa";
    const VAULT: &str = "0x00000000000000000000000000000000000000bb";
    const OTHER: &str = "0x00000000000000000000000000000000000000cc";
    const TOKEN_A: &str = "0x00000000000000000000000000000000000000a1";
    const TOKEN_B: &str = "0x00000000000000000000000000000000000000b1";

    fn bytes(hex: &str) -> Bytes {
        Bytes::from_str(hex).unwrap()
    }

    fn amount(n: u64) -> Bytes {
        Bytes::from(BigUint::from(n).to_bytes_be())
    }

    /// A chain where `holdings` gives each `(token, holder)` balance; any other read fails.
    /// A scripted chain: balances by `(token, holder)`, and return data by `(to, calldata)`.
    /// Any other read fails.
    #[derive(Default)]
    struct Scripted {
        holdings: HashMap<(Bytes, Bytes), u64>,
        calls: HashMap<(Bytes, Bytes), Bytes>,
    }

    impl Scripted {
        fn returning(mut self, to: &str, calldata: Bytes, word: &BigUint) -> Self {
            let mut returned = vec![0u8; 32];
            let word = word.to_bytes_be();
            returned[32 - word.len()..].copy_from_slice(&word);
            self.calls
                .insert((bytes(to), calldata), Bytes::from(returned));
            self
        }
    }

    impl Chain for Scripted {
        fn balance_of(&self, token: &Bytes, holder: &Bytes) -> Result<BigUint, String> {
            self.holdings
                .get(&(token.clone(), holder.clone()))
                .map(|n| BigUint::from(*n))
                .ok_or_else(|| format!("no balance of {token} for {holder}"))
        }

        fn call(&self, to: &Bytes, calldata: &Bytes) -> Result<Bytes, String> {
            self.calls
                .get(&(to.clone(), calldata.clone()))
                .cloned()
                .ok_or_else(|| format!("no call to {to} with {calldata}"))
        }
    }

    fn chain(holdings: &[(&str, &str, u64)]) -> Scripted {
        Scripted {
            holdings: holdings
                .iter()
                .map(|(token, holder, n)| ((bytes(token), bytes(holder)), *n))
                .collect(),
            calls: HashMap::new(),
        }
    }

    struct Fixture {
        id: String,
        tokens: Vec<(Bytes, String)>,
        balances: HashMap<Bytes, Bytes>,
        static_attributes: HashMap<String, Bytes>,
        state_attributes: HashMap<String, Bytes>,
    }

    impl Fixture {
        /// A pool of tokens A and B, indexed with 100 A and 200 B.
        fn new() -> Self {
            Self {
                id: POOL.to_string(),
                tokens: vec![(bytes(TOKEN_A), "A".to_string()), (bytes(TOKEN_B), "B".to_string())],
                balances: HashMap::from([
                    (bytes(TOKEN_A), amount(100)),
                    (bytes(TOKEN_B), amount(200)),
                ]),
                static_attributes: HashMap::new(),
                state_attributes: HashMap::new(),
            }
        }

        fn check(&self, check: &BalanceCheck, chain: &Scripted) -> Vec<String> {
            self.check_within(check, Tolerance::EXACT, chain)
        }

        fn check_within(
            &self,
            check: &BalanceCheck,
            tolerance: Tolerance,
            chain: &Scripted,
        ) -> Vec<String> {
            let component = Component {
                id: &self.id,
                tokens: &self.tokens,
                balances: &self.balances,
                static_attributes: &self.static_attributes,
                state_attributes: &self.state_attributes,
            };
            check_component(&component, check, tolerance, chain)
        }
    }

    fn parse(yaml: &str) -> Result<BalanceCheck, String> {
        serde_yaml::from_str(yaml).map_err(|e| e.to_string())
    }

    #[test]
    fn default_reads_the_component_itself() {
        let fixture = Fixture::new();
        let failures = fixture
            .check(&BalanceCheck::Default, &chain(&[(TOKEN_A, POOL, 100), (TOKEN_B, POOL, 200)]));
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn a_mismatch_names_the_token_and_both_amounts_and_the_other_tokens_are_still_checked() {
        let fixture = Fixture::new();
        let failures = fixture
            .check(&BalanceCheck::Default, &chain(&[(TOKEN_A, POOL, 99), (TOKEN_B, POOL, 201)]));
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures[0].contains("A: indexed 100, but self holds 99"), "{}", failures[0]);
        assert!(failures[1].contains("B: indexed 200, but self holds 201"), "{}", failures[1]);
    }

    #[test]
    fn an_unindexed_balance_is_zero() {
        let mut fixture = Fixture::new();
        fixture.balances.remove(&bytes(TOKEN_B));
        let failures = fixture
            .check(&BalanceCheck::Default, &chain(&[(TOKEN_A, POOL, 100), (TOKEN_B, POOL, 0)]));
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn a_holder_can_come_from_a_static_or_a_state_attribute() {
        let on_chain = [(TOKEN_A, VAULT, 100), (TOKEN_B, VAULT, 200)];
        let check = BalanceCheck::Holder(Holder::Attribute("vault".to_string()));

        let mut from_static = Fixture::new();
        from_static
            .static_attributes
            .insert("vault".to_string(), bytes(VAULT));
        assert!(from_static
            .check(&check, &chain(&on_chain))
            .is_empty());

        // ABI-encoded, as state attributes often are.
        let mut from_state = Fixture::new();
        let mut encoded = vec![0u8; 12];
        encoded.extend_from_slice(&bytes(VAULT));
        from_state
            .state_attributes
            .insert("vault".to_string(), Bytes::from(encoded));
        assert!(from_state
            .check(&check, &chain(&on_chain))
            .is_empty());

        // The static attribute wins over the state one.
        from_state
            .static_attributes
            .insert("vault".to_string(), bytes(OTHER));
        let failures = from_state.check(&check, &chain(&on_chain));
        assert!(
            failures[0].contains("reading the balance of attribute:vault failed"),
            "{failures:?}"
        );
    }

    #[test]
    fn a_missing_or_malformed_holder_attribute_fails_the_component() {
        let check = BalanceCheck::Holder(Holder::Attribute("vault".to_string()));
        let missing = Fixture::new().check(&check, &chain(&[]));
        assert!(missing[0].contains("`vault` is in neither"), "{missing:?}");

        let mut malformed = Fixture::new();
        malformed
            .state_attributes
            .insert("vault".to_string(), amount(7));
        let failures = malformed.check(&check, &chain(&[]));
        assert!(failures[0].contains("not an address"), "{failures:?}");
    }

    #[test]
    fn a_literal_holder_is_read_directly() {
        let check = BalanceCheck::Holder(Holder::Address(bytes(VAULT)));
        let failures =
            Fixture::new().check(&check, &chain(&[(TOKEN_A, VAULT, 100), (TOKEN_B, VAULT, 200)]));
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn min_of_compares_against_the_binding_holder() {
        let check = BalanceCheck::MinOf(vec![Holder::Component, Holder::Address(bytes(VAULT))]);
        // A is bound by the vault, B by the pool.
        let on_chain = [
            (TOKEN_A, POOL, 500),
            (TOKEN_A, VAULT, 100),
            (TOKEN_B, POOL, 200),
            (TOKEN_B, VAULT, 900),
        ];
        assert!(Fixture::new()
            .check(&check, &chain(&on_chain))
            .is_empty());

        let wrong = [
            (TOKEN_A, POOL, 500),
            (TOKEN_A, VAULT, 90),
            (TOKEN_B, POOL, 200),
            (TOKEN_B, VAULT, 900),
        ];
        let failures = Fixture::new().check(&check, &chain(&wrong));
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0]
                .contains(&format!("A: indexed 100, but {VAULT} holds 90 on chain (the smallest")),
            "{}",
            failures[0]
        );
    }

    #[test]
    fn a_component_id_that_is_not_an_address_needs_a_holder() {
        let mut fixture = Fixture::new();
        fixture.id = format!("0x{}", "ab".repeat(32));
        let failures = fixture.check(&BalanceCheck::Default, &chain(&[]));
        assert_eq!(failures.len(), 2, "one per token: {failures:?}");
        assert!(failures
            .iter()
            .all(|failure| failure.contains("is not an address")));
    }

    /// The Sky wrapper's shape: it holds nothing; its USDC sits in the PSM's pocket, and the
    /// USDS it can pay out is the PSM's DAI.
    #[test]
    fn a_source_per_token_can_read_another_holder_and_another_token() {
        const POCKET: &str = VAULT;
        const PSM: &str = OTHER;
        const DAI: &str = "0x00000000000000000000000000000000000000d1";
        let check = parse(&format!(
            "{{tokens: {{\"{TOKEN_A}\": {{holder: \"{POCKET}\"}}, \"{TOKEN_B}\": {{holder: \"{PSM}\", token: \"{DAI}\"}}}}}}"
        ))
        .unwrap();
        let on_chain = [(TOKEN_A, POCKET, 100), (DAI, PSM, 200), (TOKEN_B, PSM, 0)];
        assert!(Fixture::new()
            .check(&check, &chain(&on_chain))
            .is_empty());

        let wrong = [(TOKEN_A, POCKET, 100), (DAI, PSM, 199)];
        let failures = Fixture::new().check(&check, &chain(&wrong));
        assert_eq!(
            failures,
            vec![format!("B: indexed 200, but {PSM} holds 199 of {DAI} on chain")]
        );
        assert_eq!(parse(&serde_yaml::to_string(&check).unwrap()), Ok(check));
    }

    #[test]
    fn a_token_without_a_source_is_read_from_the_component() {
        let check =
            parse(&format!("{{tokens: {{\"{TOKEN_A}\": {{holder: \"{VAULT}\"}}}}}}")).unwrap();
        let on_chain = [(TOKEN_A, VAULT, 100), (TOKEN_B, POOL, 200)];
        assert!(Fixture::new()
            .check(&check, &chain(&on_chain))
            .is_empty());
    }

    #[test]
    fn a_source_for_a_token_the_component_lacks_fails() {
        let check =
            parse(&format!("{{tokens: {{\"{OTHER}\": {{holder: \"{VAULT}\"}}}}}}")).unwrap();
        let on_chain = [(TOKEN_A, POOL, 100), (TOKEN_B, POOL, 200)];
        let failures = Fixture::new().check(&check, &chain(&on_chain));
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("not one of the component's tokens"), "{}", failures[0]);
    }

    /// Lido's shape: the ETH balance is `getTotalPooledEther()` of the component itself.
    #[test]
    fn a_call_on_the_component_reads_its_accounting() {
        let check = parse(&format!(
            "{{tokens: {{\"{TOKEN_A}\": {{call: {{to: self, fn: \"getTotalPooledEther()\"}}}}}}}}"
        ))
        .unwrap();
        let mut fixture = Fixture::new();
        fixture.balances.remove(&bytes(TOKEN_B));
        let scripted = || {
            chain(&[(TOKEN_B, POOL, 0)]).returning(
                POOL,
                bytes("0x37cfdaca"),
                &BigUint::from(100u32),
            )
        };
        assert!(fixture
            .check(&check, &scripted())
            .is_empty());

        fixture
            .balances
            .insert(bytes(TOKEN_A), amount(101));
        assert_eq!(
            fixture.check(&check, &scripted()),
            vec!["A: indexed 101, but self.getTotalPooledEther() is 100 on chain".to_string()]
        );
        assert_eq!(parse(&serde_yaml::to_string(&check).unwrap()), Ok(check));
    }

    /// Sky's converter: DAI is the DAI join's escrow in the vat, kept in rad (1e45) for a wad
    /// (1e18) token.
    #[test]
    fn a_call_takes_typed_arguments_and_a_divisor() {
        const JOIN: &str = "0x9759a6ac90977b93b58547b4a71c78317f391a28";
        let check = parse(&format!(
            "{{tokens: {{\"{TOKEN_A}\": {{call: {{to: \"{VAULT}\", fn: \"dai(address)\", args: [\"{JOIN}\"], divide_by: \"1e27\"}}}}}}}}"
        ))
        .unwrap();
        let calldata =
            bytes("0x6c25b3460000000000000000000000009759a6ac90977b93b58547b4a71c78317f391a28");
        let rad = BigUint::from(100u32) * BigUint::from(10u32).pow(27) + 999u32;
        let failures = Fixture::new()
            .check(&check, &chain(&[(TOKEN_B, POOL, 200)]).returning(VAULT, calldata, &rad));
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(parse(&serde_yaml::to_string(&check).unwrap()), Ok(check));
    }

    #[test]
    fn call_arguments_can_be_the_component_id_an_attribute_or_a_number() {
        let mut fixture = Fixture::new();
        fixture.id = format!("0x{}aa", "00".repeat(31));
        fixture
            .state_attributes
            .insert("shares".to_string(), amount(1000));
        fixture.tokens.truncate(1);
        let on_chain = |to: &str, calldata: &str| {
            Scripted::default().returning(to, bytes(calldata), &BigUint::from(100u32))
        };

        // A bytes32 pool id, as a singleton vault takes it.
        let by_id = parse(&format!(
            "{{tokens: {{\"{TOKEN_A}\": {{call: {{to: \"{VAULT}\", fn: \"getPoolTokens(bytes32)\", args: [self]}}}}}}}}"
        ))
        .unwrap();
        let get_pool_tokens =
            "0xf94d466800000000000000000000000000000000000000000000000000000000000000aa";
        assert!(fixture
            .check(&by_id, &on_chain(VAULT, get_pool_tokens))
            .is_empty());

        let convert = "0x07a2d13a00000000000000000000000000000000000000000000000000000000000003e8";
        for arg in ["attribute:shares", "1000"] {
            let check = parse(&format!(
                "{{tokens: {{\"{TOKEN_A}\": {{call: {{to: \"{VAULT}\", fn: \"convertToAssets(uint256)\", args: [{arg}]}}}}}}}}"
            ))
            .unwrap();
            let failures = fixture.check(&check, &on_chain(VAULT, convert));
            assert!(failures.is_empty(), "{arg}: {failures:?}");
        }
    }

    /// Balancer V2's shape: the vault's per-pool, per-token view, for every token at once.
    #[test]
    fn one_call_can_serve_every_token() {
        let mut fixture = Fixture::new();
        fixture.id = format!("0x{}aa", "00".repeat(31));
        let check = parse(&format!(
            "{{call: {{to: \"{VAULT}\", fn: \"getPoolTokenInfo(bytes32,address)\", args: [self, token]}}}}"
        ))
        .unwrap();
        let pool_id = format!("{}aa", "00".repeat(31));
        let info = |token: &str| bytes(&format!("0xb05f8e48{pool_id}{:0>64}", &token[2..]));
        let scripted = Scripted::default()
            .returning(VAULT, info(TOKEN_A), &BigUint::from(100u32))
            .returning(VAULT, info(TOKEN_B), &BigUint::from(200u32));
        let failures = fixture.check(&check, &scripted);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(parse(&serde_yaml::to_string(&check).unwrap()), Ok(check));
    }

    /// Lido's stETH and wstETH: the component trades them, the indexer tracks only its ETH.
    #[test]
    fn an_unindexed_token_must_stay_unindexed() {
        let check = parse(&format!(
            "{{tokens: {{\"{TOKEN_B}\": {{unindexed: {{reason: \"only the pooled ether is tracked\"}}}}}}}}"
        ))
        .unwrap();
        let mut fixture = Fixture::new();
        // B is held on chain, but not indexed.
        let on_chain = chain(&[(TOKEN_A, POOL, 100), (TOKEN_B, POOL, 7)]);
        fixture.balances.remove(&bytes(TOKEN_B));
        assert!(fixture
            .check(&check, &on_chain)
            .is_empty());

        fixture
            .balances
            .insert(bytes(TOKEN_B), amount(7));
        assert_eq!(
            fixture.check(&check, &on_chain),
            vec!["B: declared unindexed, but the indexer reports 7".to_string()]
        );
        assert_eq!(parse(&serde_yaml::to_string(&check).unwrap()), Ok(check));
    }

    #[test]
    fn a_failed_or_short_call_fails_the_token() {
        let check = parse(&format!(
            "{{tokens: {{\"{TOKEN_A}\": {{call: {{to: self, fn: \"getTotalPooledEther()\"}}}}}}}}"
        ))
        .unwrap();
        let mut fixture = Fixture::new();
        fixture.tokens.truncate(1);
        let failures = fixture.check(&check, &Scripted::default());
        assert!(
            failures[0].starts_with("A: calling self.getTotalPooledEther() failed"),
            "{failures:?}"
        );

        let mut short = Scripted::default();
        short
            .calls
            .insert((bytes(POOL), bytes("0x37cfdaca")), bytes("0x01"));
        let failures = fixture.check(&check, &short);
        assert!(failures[0].contains("returned 1 bytes, not a uint"), "{failures:?}");
    }

    #[test]
    fn a_tvl_only_tolerance_allows_its_bps_or_wei_of_the_on_chain_amount() {
        // Indexed 100 A and 200 B, against 101 A and 199 B on chain: 99 and 50 bps off.
        let fixture = Fixture::new();
        let on_chain = chain(&[(TOKEN_A, POOL, 101), (TOKEN_B, POOL, 199)]);
        let check = |tolerance| fixture.check_within(&BalanceCheck::Default, tolerance, &on_chain);
        assert!(check(Tolerance { bps: 100, wei: 0 }).is_empty());
        assert!(check(Tolerance { bps: 0, wei: 1 }).is_empty());
        let failures = check(Tolerance { bps: 60, wei: 0 });
        assert_eq!(
            failures,
            vec!["A: indexed 100, but self holds 101 on chain, outside the `tvl_only` tolerance \
                  of 60 bps"
                .to_string()]
        );
        assert_eq!(check(Tolerance::EXACT).len(), 2);
    }

    #[test]
    fn a_tvl_only_tolerance_applies_to_calls_but_not_to_unindexed_tokens() {
        let mut fixture = Fixture::new();
        fixture.balances.remove(&bytes(TOKEN_B));
        fixture
            .balances
            .insert(bytes(TOKEN_B), amount(3));
        let check = parse(&format!(
            "tokens: {{\"{TOKEN_A}\": {{call: {{to: self, fn: \"reserve()\"}}}}, \"{TOKEN_B}\": \
             {{unindexed: {{reason: \"not tracked\"}}}}}}"
        ))
        .unwrap();
        let on_chain = chain(&[]).returning(
            POOL,
            Bytes::from(alloy::primitives::keccak256("reserve()")[..4].to_vec()),
            &BigUint::from(101u32),
        );
        let failures = fixture.check_within(&check, Tolerance { bps: 100, wei: 0 }, &on_chain);
        assert_eq!(failures, vec!["B: declared unindexed, but the indexer reports 3".to_string()]);
    }

    #[test]
    fn every_yaml_shape_parses() {
        assert_eq!(parse("default"), Ok(BalanceCheck::Default));
        assert_eq!(
            parse("{holder: \"attribute:vault\"}"),
            Ok(BalanceCheck::Holder(Holder::Attribute("vault".to_string())))
        );
        assert_eq!(
            parse(&format!("{{holder: \"{VAULT}\"}}")),
            Ok(BalanceCheck::Holder(Holder::Address(bytes(VAULT))))
        );
        assert_eq!(
            parse(&format!("{{min_of: [self, \"attribute:vault\", \"{VAULT}\"]}}")),
            Ok(BalanceCheck::MinOf(vec![
                Holder::Component,
                Holder::Attribute("vault".to_string()),
                Holder::Address(bytes(VAULT))
            ]))
        );
        let check = parse("{min_of: [self, \"attribute:vault\"]}").unwrap();
        assert_eq!(parse(&serde_yaml::to_string(&check).unwrap()), Ok(check));
    }

    /// A `tokens` declaration with `source` for token A.
    fn call(source: &str) -> String {
        if source.contains("holder") {
            let (call, rest) = source.split_once("}, ").unwrap();
            return format!("{{tokens: {{\"{TOKEN_A}\": {{call: {call}}}, {rest}}}}}}}");
        }
        format!("{{tokens: {{\"{TOKEN_A}\": {{call: {source}}}}}}}")
    }

    #[test]
    fn malformed_declarations_say_what_is_wrong() {
        for (yaml, problem) in [
            ("skip", "takes `default`"),
            ("{tokens: {\"0x00000000000000000000000000000000000000a1\": {unindexed: true}}}", "write `{unindexed: {reason"),
            ("{tokens: {\"0x00000000000000000000000000000000000000a1\": {unindexed: {reason: \" \"}}}}", "write `{unindexed: {reason"),
            ("{tokens: {\"0x00000000000000000000000000000000000000a1\": {unindexed: {reason: r}, holder: self}}}", "write `{unindexed: {reason"),
            ("{call: {to: self, fn: \"f()\"}, tokens: {}}", "`call` stands alone"),
            (&call("{to: self, fn: \"f()\"}, holder: self"), "`call` stands alone"),
            (&call("{to: self, fn: \"f(\"}"), "is not a function signature"),
            (&call("{to: self, fn: \"f(address)\"}"), "takes 1 arguments, but `args` has 0"),
            (&call("{to: self, fn: \"f(address)\", args: [\"0x12\"]}"), "`0x12` does not parse as `address`"),
            (&call("{to: self, fn: \"f()\", divide_by: \"1\"}"), "must be more than 1"),
            (&call("{to: self, fn: \"f()\", divide_by: \"ten\"}"), "is not an integer or `1eN`"),
            (&call("{to: self, fn: \"f()\", returns: \"uint\"}"), "unknown field `returns`"),
            (&call("{fn: \"f()\"}"), "missing field `to`"),
            ("{tokens: {self: {holder: self}}}", "not a token address"),
            ("{tokens: {\"0x00000000000000000000000000000000000000a1\": {token: \"0x00000000000000000000000000000000000000a1\"}}}", "has keys token"),
            ("{tokens: {\"0x00000000000000000000000000000000000000a1\": self}}", "takes `{holder: …}`"),
            ("{holder: self, token: \"0x00000000000000000000000000000000000000a1\"}", "has keys holder, token"),
            ("{tokens: {\"0x00000000000000000000000000000000000000a1\": {min_of: [self]}}}", "at least two holders"),
            ("{holder: self, min_of: [self]}", "has keys holder, min_of"),
            ("{holders: self}", "has keys holders"),
            ("{min_of: [self]}", "at least two holders"),
            ("{holder: \"attribute:\"}", "needs an attribute name"),
            ("{holder: \"0x1234\"}", "not a 20-byte address"),
            ("{holder: vault}", "write `self`, `attribute:NAME` or a 0x address"),
            ("[self]", "takes `default`"),
        ] {
            let err = parse(yaml).unwrap_err();
            assert!(err.contains(problem), "{yaml}: {err}");
        }
    }
}
