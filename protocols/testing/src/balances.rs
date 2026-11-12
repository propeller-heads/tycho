//! Strict mode's balance check: each expected component's indexed balances against the chain, as
//! the package's balance lens reads them.
//!
//! The indexer stores, per component and token, the amount the component holds. Where that amount
//! sits on chain differs by protocol: in the component's own contract, at an address an attribute
//! names, in a vault's per-pool accounting, or in a number the protocol keeps. A package says how
//! to read it with a lens, a Solidity contract implementing `IBalanceLens` under
//! `protocols/adapter-integration/evm/src/`, named in the yaml:
//!
//! ```yaml
//! harness: strict
//! balance_lens: ComponentIdBalances   # each token's balanceOf(component id)
//! ```
//!
//! The runner builds the lens with forge and calls it once per expected component, with an
//! `eth_call` at the snapshot block that places the lens's code by a state override. The lens
//! takes the component's id, tokens and attributes and returns one amount per token, which the
//! indexed balance must equal, or come within the component's `tvl_only` tolerance of.

use std::collections::HashMap;

use alloy::{
    primitives::{Address, U256},
    sol,
    sol_types::SolCall,
};
use itertools::Itertools;
use num_bigint::BigUint;
use tycho_simulation::tycho_common::Bytes;

use crate::config::{DeclaredTolerance, Tolerance};

sol! {
    interface IBalanceLens {
        struct Attribute {
            string name;
            bytes value;
        }

        function balances(bytes id, address[] tokens, Attribute[] attributes)
            external
            view
            returns (uint256[] amounts);
    }
}

/// A package's balance lens: its contract name and its runtime bytecode.
pub struct Lens {
    pub name: String,
    pub code: Bytes,
}

/// Reads the chain at the snapshot block.
pub trait Chain {
    /// The return data of an `eth_call` of `calldata` to an address whose code is `code`.
    fn call_code(&self, code: &Bytes, calldata: &Bytes) -> Result<Bytes, String>;
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

impl Component<'_> {
    /// The lens's arguments: the id's bytes when it is hex, else its UTF-8 bytes; the tokens; and
    /// the static attributes, then the state ones, each sorted by name, so a state attribute
    /// comes after a static one of the same name.
    fn lens_calldata(&self) -> Result<Bytes, String> {
        let id = match self.id.strip_prefix("0x") {
            Some(hex) => {
                alloy::hex::decode(hex).unwrap_or_else(|_not_hex| self.id.as_bytes().to_vec())
            }
            None => self.id.as_bytes().to_vec(),
        };
        let tokens = self
            .tokens
            .iter()
            .map(|(address, symbol)| {
                <[u8; 20]>::try_from(address.as_ref())
                    .map(Address::from)
                    .map_err(|_wrong_length| {
                        format!("{symbol}'s address {address} is not 20 bytes")
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let attributes = [self.static_attributes, self.state_attributes]
            .into_iter()
            .flat_map(|attributes| {
                attributes
                    .iter()
                    .sorted_by(|a, b| a.0.cmp(b.0))
            })
            .map(|(name, value)| IBalanceLens::Attribute {
                name: name.clone(),
                value: value.to_vec().into(),
            })
            .collect();
        Ok(Bytes::from(
            IBalanceLens::balancesCall { id: id.into(), tokens, attributes }.abi_encode(),
        ))
    }
}

/// Checks a component's indexed balance of every token against the amount `lens` reads on
/// chain: exactly, or within the component's `tvl_only` tolerance for that token. A token
/// without an indexed balance counts as zero. Returns every mismatch, or why there are no amounts
/// to compare: the lens failed, returned data that does not decode, or returned a different
/// number of amounts than the component has tokens.
pub fn check_component(
    component: &Component<'_>,
    lens: &Lens,
    tvl_only: Option<&DeclaredTolerance>,
    chain: &impl Chain,
) -> Vec<String> {
    let lens_name = &lens.name;
    let amounts = component
        .lens_calldata()
        .and_then(|calldata| chain.call_code(&lens.code, &calldata))
        .map_err(|e| format!("the balance lens {lens_name} failed: {e}"))
        .and_then(|returned| {
            IBalanceLens::balancesCall::abi_decode_returns(&returned).map_err(|e| {
                format!("the balance lens {lens_name} returned data that does not decode: {e}")
            })
        });
    let amounts = match amounts {
        Ok(amounts) if amounts.len() == component.tokens.len() => amounts,
        Ok(amounts) => {
            return vec![format!(
                "the balance lens {lens_name} returned {} amounts for {} tokens",
                amounts.len(),
                component.tokens.len()
            )]
        }
        Err(e) => return vec![e],
    };

    let mut failures = Vec::new();
    for ((token, symbol), on_chain) in component.tokens.iter().zip(amounts) {
        let on_chain = to_biguint(on_chain);
        let indexed = component
            .balances
            .get(token)
            .map_or_else(BigUint::default, |balance| BigUint::from_bytes_be(balance));
        let tolerance = tvl_only.map_or(Tolerance::EXACT, |declared| declared.tolerance(token));
        let gap = if indexed > on_chain { &indexed - &on_chain } else { &on_chain - &indexed };
        if tolerance.allows(&gap, &on_chain) {
            continue;
        }
        let outside = match tolerance {
            Tolerance::EXACT => String::new(),
            tolerance => format!(", outside the `tvl_only` tolerance of {tolerance}"),
        };
        failures.push(format!(
            "{symbol}: indexed {indexed}, but {lens_name} reads {on_chain} on chain{outside}"
        ));
    }
    failures
}

fn to_biguint(amount: U256) -> BigUint {
    BigUint::from_bytes_be(&amount.to_be_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, str::FromStr};

    use alloy::sol_types::SolValue;

    use super::*;

    const POOL: &str = "0x00000000000000000000000000000000000000aa";
    const TOKEN_A: &str = "0x00000000000000000000000000000000000000a1";
    const TOKEN_B: &str = "0x00000000000000000000000000000000000000b1";

    fn bytes(hex: &str) -> Bytes {
        Bytes::from_str(hex).unwrap()
    }

    fn amount(n: u64) -> Bytes {
        Bytes::from(BigUint::from(n).to_bytes_be())
    }

    /// A chain whose lens returns `returned`, recording the code and calldata it was called with.
    struct Scripted {
        returned: Result<Bytes, String>,
        called_with: RefCell<Option<(Bytes, Bytes)>>,
    }

    impl Scripted {
        fn amounts(amounts: &[u64]) -> Self {
            let words: Vec<U256> = amounts
                .iter()
                .map(|n| U256::from(*n))
                .collect();
            Self::returning(Ok(Bytes::from(words.abi_encode())))
        }

        fn returning(returned: Result<Bytes, String>) -> Self {
            Self { returned, called_with: RefCell::new(None) }
        }
    }

    impl Chain for Scripted {
        fn call_code(&self, code: &Bytes, calldata: &Bytes) -> Result<Bytes, String> {
            *self.called_with.borrow_mut() = Some((code.clone(), calldata.clone()));
            self.returned.clone()
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

        fn check(&self, chain: &Scripted, tvl_only: Option<&DeclaredTolerance>) -> Vec<String> {
            let component = Component {
                id: &self.id,
                tokens: &self.tokens,
                balances: &self.balances,
                static_attributes: &self.static_attributes,
                state_attributes: &self.state_attributes,
            };
            check_component(&component, &lens(), tvl_only, chain)
        }
    }

    fn lens() -> Lens {
        Lens { name: "PoolLens".to_string(), code: bytes("0x60006000") }
    }

    fn tvl_only(declared: &str) -> DeclaredTolerance {
        serde_yaml::from_str(declared).unwrap()
    }

    #[test]
    fn indexed_balances_must_equal_what_the_lens_reads() {
        let fixture = Fixture::new();
        assert!(fixture
            .check(&Scripted::amounts(&[100, 200]), None)
            .is_empty());
        assert_eq!(
            fixture.check(&Scripted::amounts(&[101, 200]), None),
            vec!["A: indexed 100, but PoolLens reads 101 on chain".to_string()]
        );
    }

    #[test]
    fn a_token_without_an_indexed_balance_must_read_zero() {
        let mut fixture = Fixture::new();
        fixture.balances.remove(&bytes(TOKEN_B));
        assert!(fixture
            .check(&Scripted::amounts(&[100, 0]), None)
            .is_empty());
        assert_eq!(
            fixture.check(&Scripted::amounts(&[100, 5]), None),
            vec!["B: indexed 0, but PoolLens reads 5 on chain".to_string()]
        );
    }

    #[test]
    fn the_lens_gets_the_id_tokens_and_attributes_with_state_after_static() {
        let mut fixture = Fixture::new();
        fixture.static_attributes = HashMap::from([
            ("balance_owner".to_string(), bytes("0x01")),
            ("fee".to_string(), bytes("0x02")),
        ]);
        fixture.state_attributes = HashMap::from([("balance_owner".to_string(), bytes("0x03"))]);
        let chain = Scripted::amounts(&[100, 200]);
        assert!(fixture.check(&chain, None).is_empty());

        let (code, calldata) = chain.called_with.take().unwrap();
        assert_eq!(code, lens().code);
        let call = IBalanceLens::balancesCall::abi_decode(&calldata).unwrap();
        assert_eq!(call.id.to_vec(), bytes(POOL).to_vec());
        assert_eq!(
            call.tokens,
            vec![Address::from_str(TOKEN_A).unwrap(), Address::from_str(TOKEN_B).unwrap()]
        );
        let attributes: Vec<(String, Vec<u8>)> = call
            .attributes
            .into_iter()
            .map(|attribute| (attribute.name, attribute.value.to_vec()))
            .collect();
        assert_eq!(
            attributes,
            vec![
                ("balance_owner".to_string(), vec![1]),
                ("fee".to_string(), vec![2]),
                ("balance_owner".to_string(), vec![3]),
            ]
        );
    }

    #[test]
    fn an_id_that_is_not_hex_is_passed_as_its_text() {
        let mut fixture = Fixture::new();
        fixture.id = "ethereum-sky-converter".to_string();
        let chain = Scripted::amounts(&[100, 200]);
        assert!(fixture.check(&chain, None).is_empty());
        let (_, calldata) = chain.called_with.take().unwrap();
        let call = IBalanceLens::balancesCall::abi_decode(&calldata).unwrap();
        assert_eq!(call.id.to_vec(), b"ethereum-sky-converter".to_vec());
    }

    #[test]
    fn a_lens_that_fails_or_answers_wrongly_fails_the_component() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.check(&Scripted::returning(Err("execution reverted".to_string())), None),
            vec!["the balance lens PoolLens failed: execution reverted".to_string()]
        );
        assert_eq!(
            fixture.check(&Scripted::amounts(&[100]), None),
            vec!["the balance lens PoolLens returned 1 amounts for 2 tokens".to_string()]
        );
        let garbage = fixture.check(&Scripted::returning(Ok(bytes("0x01"))), None);
        assert!(garbage[0].contains("does not decode"), "{garbage:?}");
    }

    #[test]
    fn a_tvl_only_tolerance_allows_its_bps_or_wei_of_the_on_chain_amount() {
        // Indexed 100 A and 200 B, against 101 A and 199 B on chain: 99 and 50 bps off.
        let fixture = Fixture::new();
        let on_chain = || Scripted::amounts(&[101, 199]);
        let check = |declared: &str| fixture.check(&on_chain(), Some(&tvl_only(declared)));
        assert!(check("{bps: 100, reason: r}").is_empty());
        assert!(check("{wei: 1, reason: r}").is_empty());
        assert_eq!(
            check("{bps: 60, reason: r}"),
            vec!["A: indexed 100, but PoolLens reads 101 on chain, outside the `tvl_only` \
                  tolerance of 60 bps"
                .to_string()]
        );
        assert_eq!(fixture.check(&on_chain(), None).len(), 2);
        // `wei` per token: 1 for A, nothing for B.
        let failures = check(&format!("{{wei: {{\"{TOKEN_A}\": 1}}, reason: r}}"));
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].starts_with("B: "), "{failures:?}");

        // The bps are of the on-chain amount: 1 off 199 is 50.3 bps, 1 off 200 would be 50.
        let failures =
            fixture.check(&Scripted::amounts(&[100, 199]), Some(&tvl_only("{bps: 50, reason: r}")));
        assert_eq!(failures.len(), 1, "{failures:?}");
    }
}
