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

use num_bigint::BigUint;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use tycho_simulation::tycho_common::Bytes;

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
    Tokens(HashMap<Bytes, Source>),
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
            BalanceCheck::Tokens(sources) => sources
                .get(token)
                .cloned()
                .unwrap_or_else(|| Source::of(vec![Holder::Component])),
        }
    }
}

const SHAPE: &str =
    "`default`, `{holder: …}`, `{min_of: [...]}` or `{tokens: {0x<token>: {holder: …}, ...}}`";

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
                return Err(de::Error::custom(format!("{location} takes `{{holder: …}}`")));
            };
            let token = match source.get("token") {
                Some(read) => {
                    Some(parse_address(read.as_str().unwrap_or("?")).map_err(de::Error::custom)?)
                }
                None => None,
            };
            let holders = holders(source, &["token"], &location)?;
            sources.insert(address, Source { holders, token });
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

impl Serialize for BalanceCheck {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value: serde_yaml::Value = match self {
            BalanceCheck::Default => "default".into(),
            BalanceCheck::Holder(holder) => source_value(&Source::of(vec![holder.clone()])),
            BalanceCheck::MinOf(holders) => source_value(&Source::of(holders.clone())),
            BalanceCheck::Tokens(sources) => {
                let mut tokens: Vec<(&Bytes, &Source)> = sources.iter().collect();
                tokens.sort_by_key(|(token, _)| *token);
                let tokens: serde_yaml::Mapping = tokens
                    .into_iter()
                    .map(|(token, source)| (token.to_string().into(), source_value(source)))
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

/// Checks a component's indexed balance of every token against the declared source. Returns every
/// mismatch, or the reason a balance could not be read.
///
/// `balance_of(token, holder)` reads the chain at the snapshot block.
pub fn check_component(
    component: &Component<'_>,
    check: &BalanceCheck,
    balance_of: impl Fn(&Bytes, &Bytes) -> Result<BigUint, String>,
) -> Vec<String> {
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
                balance_of(read, &address)
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
            if indexed != on_chain {
                failures.push(format!(
                    "{symbol}: indexed {indexed}, but {holder} holds {on_chain}{of_token} on chain{}",
                    if source.holders.len() > 1 { " (the smallest of the holders)" } else { "" }
                ));
            }
        }
    }
    failures
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
            let value = component
                .static_attributes
                .get(name)
                .or_else(|| component.state_attributes.get(name))
                .ok_or_else(|| {
                    format!("the holder attribute `{name}` is in neither the static nor the state attributes")
                })?;
            address_from_attribute(value)
                .ok_or_else(|| format!("the holder attribute `{name}` is {value}, not an address"))
        }
        Holder::Address(address) => Ok(address.clone()),
    }
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
    fn chain(holdings: &[(&str, &str, u64)]) -> impl Fn(&Bytes, &Bytes) -> Result<BigUint, String> {
        let holdings: HashMap<(Bytes, Bytes), u64> = holdings
            .iter()
            .map(|(token, holder, n)| ((bytes(token), bytes(holder)), *n))
            .collect();
        move |token, holder| {
            holdings
                .get(&(token.clone(), holder.clone()))
                .map(|n| BigUint::from(*n))
                .ok_or_else(|| format!("no balance of {token} for {holder}"))
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

        fn check(
            &self,
            check: &BalanceCheck,
            balance_of: impl Fn(&Bytes, &Bytes) -> Result<BigUint, String>,
        ) -> Vec<String> {
            let component = Component {
                id: &self.id,
                tokens: &self.tokens,
                balances: &self.balances,
                static_attributes: &self.static_attributes,
                state_attributes: &self.state_attributes,
            };
            check_component(&component, check, balance_of)
        }
    }

    fn parse(yaml: &str) -> Result<BalanceCheck, String> {
        serde_yaml::from_str(yaml).map_err(|e| e.to_string())
    }

    #[test]
    fn default_reads_the_component_itself() {
        let fixture = Fixture::new();
        let failures = fixture
            .check(&BalanceCheck::Default, chain(&[(TOKEN_A, POOL, 100), (TOKEN_B, POOL, 200)]));
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn a_mismatch_names_the_token_and_both_amounts_and_the_other_tokens_are_still_checked() {
        let fixture = Fixture::new();
        let failures = fixture
            .check(&BalanceCheck::Default, chain(&[(TOKEN_A, POOL, 99), (TOKEN_B, POOL, 201)]));
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures[0].contains("A: indexed 100, but self holds 99"), "{}", failures[0]);
        assert!(failures[1].contains("B: indexed 200, but self holds 201"), "{}", failures[1]);
    }

    #[test]
    fn an_unindexed_balance_is_zero() {
        let mut fixture = Fixture::new();
        fixture.balances.remove(&bytes(TOKEN_B));
        let failures = fixture
            .check(&BalanceCheck::Default, chain(&[(TOKEN_A, POOL, 100), (TOKEN_B, POOL, 0)]));
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
            .check(&check, chain(&on_chain))
            .is_empty());

        // ABI-encoded, as state attributes often are.
        let mut from_state = Fixture::new();
        let mut encoded = vec![0u8; 12];
        encoded.extend_from_slice(&bytes(VAULT));
        from_state
            .state_attributes
            .insert("vault".to_string(), Bytes::from(encoded));
        assert!(from_state
            .check(&check, chain(&on_chain))
            .is_empty());

        // The static attribute wins over the state one.
        from_state
            .static_attributes
            .insert("vault".to_string(), bytes(OTHER));
        let failures = from_state.check(&check, chain(&on_chain));
        assert!(
            failures[0].contains("reading the balance of attribute:vault failed"),
            "{failures:?}"
        );
    }

    #[test]
    fn a_missing_or_malformed_holder_attribute_fails_the_component() {
        let check = BalanceCheck::Holder(Holder::Attribute("vault".to_string()));
        let missing = Fixture::new().check(&check, chain(&[]));
        assert!(missing[0].contains("`vault` is in neither"), "{missing:?}");

        let mut malformed = Fixture::new();
        malformed
            .state_attributes
            .insert("vault".to_string(), amount(7));
        let failures = malformed.check(&check, chain(&[]));
        assert!(failures[0].contains("not an address"), "{failures:?}");
    }

    #[test]
    fn a_literal_holder_is_read_directly() {
        let check = BalanceCheck::Holder(Holder::Address(bytes(VAULT)));
        let failures =
            Fixture::new().check(&check, chain(&[(TOKEN_A, VAULT, 100), (TOKEN_B, VAULT, 200)]));
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
            .check(&check, chain(&on_chain))
            .is_empty());

        let wrong = [
            (TOKEN_A, POOL, 500),
            (TOKEN_A, VAULT, 90),
            (TOKEN_B, POOL, 200),
            (TOKEN_B, VAULT, 900),
        ];
        let failures = Fixture::new().check(&check, chain(&wrong));
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
        let failures = fixture.check(&BalanceCheck::Default, chain(&[]));
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
            .check(&check, chain(&on_chain))
            .is_empty());

        let wrong = [(TOKEN_A, POCKET, 100), (DAI, PSM, 199)];
        let failures = Fixture::new().check(&check, chain(&wrong));
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
            .check(&check, chain(&on_chain))
            .is_empty());
    }

    #[test]
    fn a_source_for_a_token_the_component_lacks_fails() {
        let check =
            parse(&format!("{{tokens: {{\"{OTHER}\": {{holder: \"{VAULT}\"}}}}}}")).unwrap();
        let on_chain = [(TOKEN_A, POOL, 100), (TOKEN_B, POOL, 200)];
        let failures = Fixture::new().check(&check, chain(&on_chain));
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("not one of the component's tokens"), "{}", failures[0]);
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

    #[test]
    fn malformed_declarations_say_what_is_wrong() {
        for (yaml, problem) in [
            ("skip", "takes `default`"),
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
