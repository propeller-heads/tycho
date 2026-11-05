//! Strict mode's verification of a `tvl_only` declaration: that simulation ignores the component's
//! balances.
//!
//! A `tvl_only` component's balances are checked within a tolerance, which is only sound if they
//! feed TVL and nothing else. The runner checks that, rather than taking it on trust: it decodes
//! the snapshot a second time with the component's balances scaled down, each token's by a
//! different factor so their ratios change too, and asks both decoded states the same questions.
//! Every limit, spot price and ladder quote must come out the same. If one changes, simulation
//! reads the balances, and they must match the chain exactly.

use std::collections::HashMap;

use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::{
    protocol::models::Update,
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

/// Scales down every indexed balance of the components `ids` in `snapshot`, keeping each value's
/// byte length: in token address order, the first token's balance to 1/2, the second's to 1/3,
/// and so on, so a simulation that prices from the ratio of the balances quotes differently too.
/// Returns how many balances changed, per declared id: only a zero balance stays the same, and a
/// component with nothing to scale verifies nothing.
pub fn scale_balances(snapshot: &mut Snapshot, ids: &[String]) -> HashMap<String, usize> {
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
        let balances = component
            .state
            .balances
            .iter_mut()
            .sorted_by(|a, b| a.0.cmp(b.0));
        for (divisor, (_, balance)) in (2u32..).zip(balances) {
            let value = BigUint::from_bytes_be(balance);
            let scaled = &value / divisor;
            if scaled == value {
                continue;
            }
            let scaled = scaled.to_bytes_be();
            let mut bytes = vec![
                0u8;
                balance
                    .len()
                    .saturating_sub(scaled.len())
            ];
            bytes.extend(scaled);
            *balance = Bytes::from(bytes);
            *count += 1;
        }
    }
    changed
}

/// Verifies that component `id`, declared `tvl_only`, quotes the same in `scaled`, the snapshot
/// decoded with its balances scaled down, as in `original`, with [`compare`]. `changed` counts the
/// balances the scaling changed, per declared id. Returns every difference, or why the check could
/// not be made: no balance to scale, or the component missing from either decode.
pub fn check(
    id: &str,
    original: &Update,
    scaled: &Update,
    changed: &HashMap<String, usize>,
    sizing: impl Fn(&BigUint) -> BigUint,
) -> Vec<String> {
    if changed
        .get(id)
        .is_none_or(|count| *count == 0)
    {
        return vec!["the component has no nonzero indexed balance, so scaling its balances \
                     verifies nothing; it needs no `tvl_only`"
            .to_string()];
    }
    fn find<'a>(update: &'a Update, id: &str) -> Option<(&'a String, &'a dyn ProtocolSim)> {
        update
            .states
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(id))
            .map(|(key, state)| (key, state.as_ref()))
    }
    let Some((key, original_state)) = find(original, id) else {
        return vec!["the component did not decode".to_string()];
    };
    let Some((_, scaled_state)) = find(scaled, id) else {
        return vec!["the component did not decode with its balances scaled down".to_string()];
    };
    let Some(component) = original.new_pairs.get(key) else {
        return vec![format!("the update has no component {key}")];
    };
    compare(original_state, scaled_state, &component.tokens, sizing)
}

/// The fractions of a direction's max input the ladder quotes, in thousandths.
const LADDER: [u32; 3] = [1, 10, 100];

/// Compares `original` with `scaled`, the same component decoded with its balances scaled, in
/// every direction between `tokens`: limits, spot price, and a quote at 0.1%, 1% and 10% of the
/// original max input after `sizing`, which caps it as the ladder does. Returns every difference.
///
/// A question both states fail to answer with the same error is no difference. When the original
/// state answers no question at all, nothing was compared, and that is returned as a difference
/// too: the halving would otherwise verify nothing.
pub fn compare<Q: Quoter + ?Sized>(
    original: &Q,
    scaled: &Q,
    tokens: &[Token],
    sizing: impl Fn(&BigUint) -> BigUint,
) -> Vec<String> {
    let mut changes = Vec::new();
    let mut answered = 0usize;
    for pair in tokens.iter().permutations(2) {
        let (token_in, token_out) = (pair[0], pair[1]);
        let direction = format!("{} -> {}", token_in.symbol, token_out.symbol);
        let limits = original.limits(token_in, token_out);
        answered += usize::from(limits.is_ok());
        if let Some(change) =
            changed(&limits, &scaled.limits(token_in, token_out), |(input, output)| {
                format!("max input {input}, max output {output}")
            })
        {
            changes.push(format!("{direction}: limits {change}"));
        }
        let spot_price = original.spot_price(token_in, token_out);
        answered += usize::from(spot_price.is_ok());
        if let Some(change) =
            changed(&spot_price, &scaled.spot_price(token_in, token_out), f64::to_string)
        {
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
            let quote = original.amount_out(amount_in.clone(), token_in, token_out);
            answered += usize::from(quote.is_ok());
            if let Some(change) = changed(
                &quote,
                &scaled.amount_out(amount_in.clone(), token_in, token_out),
                BigUint::to_string,
            ) {
                changes.push(format!(
                    "{direction}: the quote for {amount_in} {} {change}",
                    token_in.symbol
                ));
            }
        }
    }
    if answered == 0 {
        changes.push(
            "every limit, spot price and quote errors, so halving the balances verifies nothing"
                .to_string(),
        );
    }
    changes
}

/// How `scaled` differs from `original`, if it does: `is X, but Y with the balances scaled down`.
fn changed<T: PartialEq>(
    original: &Result<T, String>,
    scaled: &Result<T, String>,
    show: impl Fn(&T) -> String,
) -> Option<String> {
    let show = |result: &Result<T, String>| match result {
        Ok(value) => show(value),
        Err(e) => format!("an error ({e})"),
    };
    let same = match (original, scaled) {
        (Ok(original), Ok(scaled)) => original == scaled,
        (Err(original), Err(scaled)) => original == scaled,
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => false,
    };
    (!same).then(|| {
        format!("is {}, but {} with the balances scaled down", show(original), show(scaled))
    })
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
        // As `scale_balances` scales them: A to 1/2, B to 1/3.
        let scaled = Pool { balances: (50_000, 66_666), ..original };
        (original, scaled)
    }

    #[test]
    fn a_component_that_ignores_its_balances_quotes_the_same_with_them_scaled_down() {
        let (original, scaled) = pair(false);
        let changes = compare(&original, &scaled, &tokens(), BigUint::clone);
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn a_component_that_reads_its_balances_fails_with_each_change() {
        let (original, scaled) = pair(true);
        let changes = compare(&original, &scaled, &tokens(), BigUint::clone);
        // Per direction: the limits, the spot price and every quote.
        assert_eq!(changes.len(), 10, "{changes:#?}");
        assert_eq!(
            changes[0],
            "A -> B: limits is max input 100000, max output 200000, but max input 50000, max \
             output 66666 with the balances scaled down"
        );
        assert_eq!(
            changes[3],
            "A -> B: the quote for 1000 A is 1980, but 1307 with the balances scaled down"
        );
    }

    #[test]
    fn the_quotes_are_sized_from_the_original_limits_after_the_ladder_cap() {
        let (original, scaled) = pair(true);
        let changes = compare(&original, &scaled, &tokens(), |_| BigUint::from(20_000u32));
        assert!(
            changes.contains(
                &"A -> B: the quote for 2000 A is 3921, but 2564 with the balances scaled down"
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
    fn a_component_that_answers_nothing_verifies_nothing() {
        let changes = compare(&Failing, &Failing, &tokens(), BigUint::clone);
        assert_eq!(
            changes,
            vec!["every limit, spot price and quote errors, so halving the balances verifies \
                  nothing"
                .to_string()]
        );
    }

    /// A pool that quotes, but has no spot price: the error is `price_error`.
    struct Unpriced {
        pool: Pool,
        price_error: &'static str,
    }

    impl Quoter for Unpriced {
        fn limits(
            &self,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<(BigUint, BigUint), String> {
            self.pool.limits(token_in, token_out)
        }
        fn spot_price(&self, _: &Token, _: &Token) -> Result<f64, String> {
            Err(self.price_error.to_string())
        }
        fn amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<BigUint, String> {
            self.pool
                .amount_out(amount_in, token_in, token_out)
        }
    }

    #[test]
    fn the_same_error_on_both_sides_is_no_change_and_a_different_one_is() {
        let (original, scaled) = pair(false);
        let unpriced = |pool, price_error| Unpriced { pool, price_error };
        let (original, scaled) = (unpriced(original, "no price"), unpriced(scaled, "no price"));
        assert!(compare(&original, &scaled, &tokens(), BigUint::clone).is_empty());

        let scaled = Unpriced { price_error: "no price for 50000", ..scaled };
        let changes = compare(&original, &scaled, &tokens(), BigUint::clone);
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert!(
            changes[0].ends_with(
                "spot price is an error (no price), but an error (no price for 50000) with the \
                 balances scaled down"
            ),
            "{changes:?}"
        );
    }

    #[test]
    fn an_error_on_one_side_only_is_a_change() {
        let (original, _) = pair(false);
        let mixed: [&dyn Quoter; 2] = [&original, &Failing];
        let changes = compare(mixed[0], mixed[1], &tokens(), BigUint::clone);
        assert!(
            changes[0].ends_with("but an error (no limits) with the balances scaled down"),
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

    /// An update holding a uniswap v2 pool `id` with `reserves`, whose tokens are A and B.
    fn update(id: &str, reserves: Option<u64>) -> Update {
        use tycho_simulation::{
            evm::protocol::uniswap_v2::state::UniswapV2State,
            protocol::models::ProtocolComponent as Component,
        };
        let component = Component::new(
            Bytes::from_str(id).unwrap(),
            "uniswap_v2".to_string(),
            "uniswap_v2_pool".to_string(),
            Chain::Ethereum,
            tokens(),
            Vec::new(),
            HashMap::new(),
            Bytes::default(),
            Default::default(),
        );
        let states: HashMap<String, Box<dyn ProtocolSim>> = reserves
            .map(|reserve| {
                let reserve = alloy::primitives::U256::from(reserve);
                let state: Box<dyn ProtocolSim> = Box::new(UniswapV2State::new(reserve, reserve));
                (id.to_string(), state)
            })
            .into_iter()
            .collect();
        Update::new(1, states, HashMap::from([(id.to_string(), component)]))
    }

    const POOL: &str = "0x00000000000000000000000000000000000000Aa";

    #[test]
    fn check_compares_the_two_decodes_of_a_declared_component() {
        let changed = HashMap::from([(POOL.to_string(), 2)]);
        let same = update(POOL, Some(1_000_000));
        assert!(
            check(POOL, &same, &update(POOL, Some(1_000_000)), &changed, BigUint::clone).is_empty()
        );
        let moved = check(POOL, &same, &update(POOL, Some(400_000)), &changed, BigUint::clone);
        assert!(moved[0].starts_with("A -> B: limits is"), "{moved:?}");
    }

    #[test]
    fn check_fails_when_it_cannot_compare() {
        let decoded = update(POOL, Some(1_000_000));
        let changed = HashMap::from([(POOL.to_string(), 2)]);
        let nothing_scaled = check(POOL, &decoded, &decoded, &HashMap::new(), BigUint::clone);
        assert!(nothing_scaled[0].contains("no nonzero indexed balance"), "{nothing_scaled:?}");
        let undecoded = check(POOL, &decoded, &update(POOL, None), &changed, BigUint::clone);
        assert_eq!(
            undecoded,
            vec!["the component did not decode with its balances scaled down".to_string()]
        );
        let missing = check(POOL, &update(POOL, None), &decoded, &changed, BigUint::clone);
        assert_eq!(missing, vec!["the component did not decode".to_string()]);
    }

    /// A pool that prices from the ratio of its balances, with limits of its own: the shape a
    /// uniform scaling of the balances would not catch.
    struct RatioPool {
        limits: (u64, u64),
        balances: (u64, u64),
    }

    impl Quoter for RatioPool {
        fn limits(&self, token_in: &Token, _: &Token) -> Result<(BigUint, BigUint), String> {
            let (a, b) = self.limits;
            Ok(if token_in.symbol == "A" { (a.into(), b.into()) } else { (b.into(), a.into()) })
        }
        fn spot_price(&self, base: &Token, _: &Token) -> Result<f64, String> {
            let (a, b) = self.balances;
            Ok(if base.symbol == "A" { b as f64 / a as f64 } else { a as f64 / b as f64 })
        }
        fn amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            _: &Token,
        ) -> Result<BigUint, String> {
            let (a, b) = self.balances;
            let (r_in, r_out) = if token_in.symbol == "A" { (a, b) } else { (b, a) };
            Ok(amount_in * r_out / r_in)
        }
    }

    #[test]
    fn a_component_that_prices_from_the_ratio_of_its_balances_fails() {
        let original = RatioPool { limits: (1_000, 2_000), balances: (100_000, 200_000) };
        // Halving both would keep the ratio, and every answer.
        let halved = RatioPool { balances: (50_000, 100_000), ..original };
        assert!(compare(&original, &halved, &tokens(), BigUint::clone).is_empty());
        let scaled = RatioPool { balances: (50_000, 66_666), ..original };
        assert!(!compare(&original, &scaled, &tokens(), BigUint::clone).is_empty());
    }

    #[test]
    fn scaling_touches_only_the_declared_components_and_keeps_the_byte_length() {
        let mut snapshot = snapshot(&[
            ("0xaa", &[(1, vec![0, 0, 0x03, 0xe8]), (2, vec![0x09])]),
            ("0xbb", &[(1, vec![0x64])]),
        ]);
        let changed = scale_balances(&mut snapshot, &["0xAA".to_string()]);
        assert_eq!(changed, HashMap::from([("0xAA".to_string(), 2)]));
        let balances = &snapshot.states["0xaa"].state.balances;
        // The first token by address to 1/2, the second to 1/3.
        assert_eq!(balances[&Bytes::from(vec![1])], Bytes::from(vec![0, 0, 0x01, 0xf4]));
        assert_eq!(balances[&Bytes::from(vec![2])], Bytes::from(vec![0x03]));
        assert_eq!(
            snapshot.states["0xbb"].state.balances[&Bytes::from(vec![1])],
            Bytes::from(vec![0x64])
        );
    }
}
