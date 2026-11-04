//! Strict mode's verification of a `tvl_only` declaration: that simulation ignores the component's
//! balances.
//!
//! A `tvl_only` component's balances are checked within a tolerance, which is only sound if they
//! feed TVL and nothing else. The runner checks that, rather than taking it on trust: it decodes
//! the snapshot a second time with the component's balances halved, and asks both decoded states
//! the same questions. Every limit, spot price and ladder quote must come out the same. If one
//! changes, simulation reads the balances, and they must match the chain exactly.

use std::collections::HashMap;

use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::{
    tycho_client::feed::synchronizer::Snapshot,
    tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim, Bytes},
};

/// What the verification asks a decoded state.
pub trait Quoter {
    /// `(max input, max output)` of selling `token_in` for `token_out`.
    fn limits(&self, token_in: &Token, token_out: &Token) -> Result<(BigUint, BigUint), String>;
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, String>;
    fn amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<BigUint, String>;
}

impl Quoter for dyn ProtocolSim {
    fn limits(&self, token_in: &Token, token_out: &Token) -> Result<(BigUint, BigUint), String> {
        self.get_limits(token_in.address.clone(), token_out.address.clone())
            .map_err(|e| e.to_string())
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, String> {
        ProtocolSim::spot_price(self, base, quote).map_err(|e| e.to_string())
    }

    fn amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<BigUint, String> {
        self.get_amount_out(amount_in, token_in, token_out)
            .map(|quote| quote.amount)
            .map_err(|e| e.to_string())
    }
}

/// Halves every indexed balance of the components `ids` in `snapshot`, keeping each value's byte
/// length. Returns how many balances changed, per declared id: only a zero balance stays the
/// same, and a component with nothing to halve verifies nothing.
pub fn halve_balances(snapshot: &mut Snapshot, ids: &[String]) -> HashMap<String, usize> {
    let mut changed = HashMap::new();
    for (id, component) in &mut snapshot.states {
        let Some(declared) = ids
            .iter()
            .find(|declared| declared.eq_ignore_ascii_case(id))
        else {
            continue;
        };
        let count = changed
            .entry(declared.clone())
            .or_insert(0);
        for balance in component.state.balances.values_mut() {
            let value = BigUint::from_bytes_be(balance);
            let half = &value / 2u32;
            if half == value {
                continue;
            }
            let half = half.to_bytes_be();
            let mut bytes = vec![0u8; balance.len().saturating_sub(half.len())];
            bytes.extend(half);
            *balance = Bytes::from(bytes);
            *count += 1;
        }
    }
    changed
}

/// The fractions of a direction's max input the ladder quotes, in thousandths.
const LADDER: [u32; 3] = [1, 10, 100];

/// Compares `original` with `halved`, the same component decoded with its balances halved, in
/// every direction between `tokens`: limits, spot price, and a quote at 0.1%, 1% and 10% of the
/// original max input after `sizing`, which caps it as the ladder does. Returns every difference.
/// A question both states fail to answer is no difference.
pub fn compare<Q: Quoter + ?Sized>(
    original: &Q,
    halved: &Q,
    tokens: &[Token],
    sizing: impl Fn(&BigUint) -> BigUint,
) -> Vec<String> {
    let mut changes = Vec::new();
    for pair in tokens.iter().permutations(2) {
        let (token_in, token_out) = (pair[0], pair[1]);
        let direction = format!("{} -> {}", token_in.symbol, token_out.symbol);
        let limits = original.limits(token_in, token_out);
        if let Some(change) =
            changed(&limits, &halved.limits(token_in, token_out), |(input, output)| {
                format!("max input {input}, max output {output}")
            })
        {
            changes.push(format!("{direction}: limits {change}"));
        }
        if let Some(change) = changed(
            &original.spot_price(token_in, token_out),
            &halved.spot_price(token_in, token_out),
            f64::to_string,
        ) {
            changes.push(format!("{direction}: spot price {change}"));
        }
        let Ok((max_input, _)) = limits else {
            continue;
        };
        let max_input = sizing(&max_input);
        for thousandths in LADDER {
            let amount_in = &max_input * thousandths / 1000u32;
            if amount_in.is_zero() {
                continue;
            }
            if let Some(change) = changed(
                &original.amount_out(amount_in.clone(), token_in, token_out),
                &halved.amount_out(amount_in.clone(), token_in, token_out),
                BigUint::to_string,
            ) {
                changes.push(format!(
                    "{direction}: the quote for {amount_in} {} {change}",
                    token_in.symbol
                ));
            }
        }
    }
    changes
}

/// How `halved` differs from `original`, if it does: `is X, but Y with the balances halved`.
fn changed<T: PartialEq>(
    original: &Result<T, String>,
    halved: &Result<T, String>,
    show: impl Fn(&T) -> String,
) -> Option<String> {
    let show = |result: &Result<T, String>| match result {
        Ok(value) => show(value),
        Err(e) => format!("an error ({e})"),
    };
    let same = match (original, halved) {
        (Ok(original), Ok(halved)) => original == halved,
        (Err(_), Err(_)) => true,
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => false,
    };
    (!same).then(|| format!("is {}, but {} with the balances halved", show(original), show(halved)))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use num_traits::ToPrimitive;
    use tycho_simulation::{
        tycho_client::feed::synchronizer::ComponentWithState,
        tycho_common::models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            Chain,
        },
    };

    use super::*;

    fn token(address: &str, symbol: &str) -> Token {
        Token::new(&Bytes::from_str(address).unwrap(), symbol, 18, 0, &[], Chain::Ethereum, 100)
    }

    fn tokens() -> Vec<Token> {
        vec![
            token("0x00000000000000000000000000000000000000a1", "A"),
            token("0x00000000000000000000000000000000000000b1", "B"),
        ]
    }

    /// A constant-product pool over `reserves`, which either quotes from its balances, as a pool
    /// whose decoder reads them does, or from reserves of its own, ignoring the balances.
    struct Pool {
        reserves: (u64, u64),
        balances: (u64, u64),
        reads_balances: bool,
    }

    impl Pool {
        fn reserves(&self, token_in: &Token) -> (BigUint, BigUint) {
            let (a, b) = if self.reads_balances { self.balances } else { self.reserves };
            match token_in.symbol.as_str() {
                "A" => (a.into(), b.into()),
                _ => (b.into(), a.into()),
            }
        }
    }

    impl Quoter for Pool {
        fn limits(
            &self,
            token_in: &Token,
            _token_out: &Token,
        ) -> Result<(BigUint, BigUint), String> {
            let (reserve_in, reserve_out) = self.reserves(token_in);
            Ok((reserve_in, reserve_out))
        }

        fn spot_price(&self, base: &Token, _quote: &Token) -> Result<f64, String> {
            let (reserve_in, reserve_out) = self.reserves(base);
            Ok(reserve_out.to_f64().unwrap() / reserve_in.to_f64().unwrap())
        }

        fn amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            _token_out: &Token,
        ) -> Result<BigUint, String> {
            let (reserve_in, reserve_out) = self.reserves(token_in);
            Ok(&reserve_out * &amount_in / (reserve_in + amount_in))
        }
    }

    fn pair(reads_balances: bool) -> (Pool, Pool) {
        let original =
            Pool { reserves: (100_000, 200_000), balances: (100_000, 200_000), reads_balances };
        let halved = Pool { balances: (50_000, 100_000), ..original };
        (original, halved)
    }

    #[test]
    fn a_component_that_ignores_its_balances_quotes_the_same_with_them_halved() {
        let (original, halved) = pair(false);
        let changes = compare(&original, &halved, &tokens(), BigUint::clone);
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn a_component_that_reads_its_balances_fails_with_each_change() {
        let (original, halved) = pair(true);
        let changes = compare(&original, &halved, &tokens(), BigUint::clone);
        // Per direction: the limits, and the quotes at 1% and 10%. The spot price is a ratio of
        // the balances, so it holds, and so does the 0.1% quote, which rounds to the same amount.
        assert_eq!(changes.len(), 6, "{changes:#?}");
        assert_eq!(
            changes[0],
            "A -> B: limits is max input 100000, max output 200000, but max input 50000, max \
             output 100000 with the balances halved"
        );
        assert_eq!(
            changes[1],
            "A -> B: the quote for 1000 A is 1980, but 1960 with the balances halved"
        );
    }

    #[test]
    fn the_quotes_are_sized_from_the_original_limits_after_the_ladder_cap() {
        let (original, halved) = pair(true);
        let changes = compare(&original, &halved, &tokens(), |_| BigUint::from(20_000u32));
        assert!(
            changes.contains(
                &"A -> B: the quote for 2000 A is 3921, but 3846 with the balances halved"
                    .to_string()
            ),
            "{changes:#?}"
        );
    }

    struct Failing;

    impl Quoter for Failing {
        fn limits(&self, _: &Token, _: &Token) -> Result<(BigUint, BigUint), String> {
            Err("no limits".to_string())
        }
        fn spot_price(&self, _: &Token, _: &Token) -> Result<f64, String> {
            Err("no price".to_string())
        }
        fn amount_out(&self, _: BigUint, _: &Token, _: &Token) -> Result<BigUint, String> {
            Err("no quote".to_string())
        }
    }

    #[test]
    fn an_error_on_one_side_only_is_a_change_and_on_both_is_not() {
        assert!(compare(&Failing, &Failing, &tokens(), BigUint::clone).is_empty());
        let (original, _) = pair(false);
        let mixed: [&dyn Quoter; 2] = [&original, &Failing];
        let changes = compare(mixed[0], mixed[1], &tokens(), BigUint::clone);
        assert!(
            changes[0].ends_with("but an error (no limits) with the balances halved"),
            "{changes:?}"
        );
    }

    /// A component id, and its balances as `(one-byte token address, value)`.
    type Balances<'a> = (&'a str, &'a [(u8, Vec<u8>)]);

    fn snapshot(balances: &[Balances<'_>]) -> Snapshot {
        Snapshot {
            states: balances
                .iter()
                .map(|(id, balances)| {
                    let balances = balances
                        .iter()
                        .map(|(token, value)| {
                            (Bytes::from(vec![*token]), Bytes::from(value.clone()))
                        })
                        .collect();
                    let component = ComponentWithState {
                        state: ProtocolComponentState::new(id, HashMap::new(), balances),
                        component: ProtocolComponent::default(),
                        component_tvl: None,
                        entrypoints: Vec::new(),
                    };
                    (id.to_string(), component)
                })
                .collect(),
            vm_storage: HashMap::new(),
        }
    }

    #[test]
    fn halving_touches_only_the_declared_components_and_keeps_the_byte_length() {
        let mut snapshot = snapshot(&[
            ("0xaa", &[(1, vec![0, 0, 0x03, 0xe8]), (2, vec![1])]),
            ("0xbb", &[(1, vec![0x64])]),
        ]);
        let changed = halve_balances(&mut snapshot, &["0xAA".to_string()]);
        assert_eq!(changed, HashMap::from([("0xAA".to_string(), 2)]));
        let balances = &snapshot.states["0xaa"].state.balances;
        assert_eq!(balances[&Bytes::from(vec![1])], Bytes::from(vec![0, 0, 0x01, 0xf4]));
        assert_eq!(balances[&Bytes::from(vec![2])], Bytes::from(vec![0]));
        assert_eq!(
            snapshot.states["0xbb"].state.balances[&Bytes::from(vec![1])],
            Bytes::from(vec![0x64])
        );
    }
}
