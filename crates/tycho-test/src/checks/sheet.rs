//! A decoded state's answers to the questions the stream and `tvl_only` checks ask: each
//! direction's limits, spot price and quotes along the ladder.
//!
//! A sheet holds the answers as printed values, so it outlives the state, which a later decode may
//! invalidate. Two sheets of the same component compare question by question.

use std::collections::BTreeMap;

use itertools::Itertools;
use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim};

/// What a check asks a decoded state.
pub trait Quoter {
    /// `(max input, max output)` of selling `token_in` for `token_out`, or the error, printed.
    fn limits(&self, token_in: &Token, token_out: &Token) -> Result<(BigUint, BigUint), String>;
    /// The price of `base` in `quote`, or the error, printed.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, String>;
    /// What selling `amount_in` of `token_in` pays in `token_out`, or the error, printed.
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

/// The fractions of a direction's max input that [`quote_sheet`] and the `tvl_only` comparison
/// quote, in thousandths (0.1%, 1% and 10%), as the ladder does.
pub(crate) const LADDER_THOUSANDTHS: [u32; 3] = [1, 10, 100];

/// Everything a state answers, by question: each direction's limits and spot price, and quotes at
/// 0.1%, 1% and 10% of its max input after `sizing`, which caps it as the ladder does. Values are
/// printed, so a sheet outlives the state, which a later decode may invalidate. A token is named
/// by its symbol, followed by its address when another of `tokens` has the same symbol.
pub fn quote_sheet<Q: Quoter + ?Sized>(
    state: &Q,
    tokens: &[Token],
    sizing: impl Fn(&BigUint) -> BigUint,
) -> BTreeMap<String, String> {
    let show =
        |result: Result<String, String>| result.unwrap_or_else(|e| format!("an error ({e})"));
    let name = |token: &Token| {
        let shared = tokens
            .iter()
            .filter(|other| other.symbol == token.symbol)
            .count() >
            1;
        if shared {
            format!("{} ({})", token.symbol, token.address)
        } else {
            token.symbol.clone()
        }
    };
    let mut sheet = BTreeMap::new();
    for pair in tokens.iter().permutations(2) {
        let (token_in, token_out) = (pair[0], pair[1]);
        let direction = format!("{} -> {}", name(token_in), name(token_out));
        let limits = state.limits(token_in, token_out);
        sheet.insert(
            format!("{direction}: limits"),
            show(
                limits
                    .clone()
                    .map(|(input, output)| format!("max input {input}, max output {output}")),
            ),
        );
        sheet.insert(
            format!("{direction}: spot price"),
            show(
                state
                    .spot_price(token_in, token_out)
                    .map(|price| price.to_string()),
            ),
        );
        let Ok((max_input, _)) = limits else {
            continue;
        };
        let max_input = sizing(&max_input);
        for thousandths in LADDER_THOUSANDTHS {
            let amount_in = &max_input * thousandths / 1000u32;
            if amount_in.is_zero() {
                continue;
            }
            sheet.insert(
                format!(
                    "{direction}: the quote at {:.1}% of the limit",
                    f64::from(thousandths) / 10.0
                ),
                show(
                    state
                        .amount_out(amount_in.clone(), token_in, token_out)
                        .map(|out| format!("{out} for {amount_in} {}", token_in.symbol)),
                ),
            );
        }
    }
    sheet
}

/// Every question `streamed` and `snapshot` answer differently, or only one of them answers.
pub fn sheet_differences(
    streamed: &BTreeMap<String, String>,
    snapshot: &BTreeMap<String, String>,
) -> Vec<String> {
    streamed
        .keys()
        .chain(snapshot.keys())
        .unique()
        .sorted()
        .filter_map(|question| match (streamed.get(question), snapshot.get(question)) {
            (Some(a), Some(b)) if a == b => None,
            (a, b) => Some(format!(
                "{question}: streamed {}, snapshot {}",
                a.map_or("nothing", String::as_str),
                b.map_or("nothing", String::as_str)
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_simulation::tycho_common::{models::Chain, Bytes};

    use super::*;

    fn token(address: &str, symbol: &str) -> Token {
        Token::new(&Bytes::from_str(address).unwrap(), symbol, 18, 0, &[], Chain::Ethereum, 100)
    }

    /// A constant-product pool over fixed reserves; a zero reserve stops a direction.
    struct Pool(u64, u64);

    impl Quoter for Pool {
        fn limits(&self, token_in: &Token, _: &Token) -> Result<(BigUint, BigUint), String> {
            Ok(match token_in.symbol.as_str() {
                "A" => (self.0.into(), self.1.into()),
                _ => (self.1.into(), self.0.into()),
            })
        }
        fn spot_price(&self, base: &Token, _: &Token) -> Result<f64, String> {
            match base.symbol.as_str() {
                "A" => Ok(self.1 as f64 / self.0 as f64),
                _ => Err("no price".to_string()),
            }
        }
        fn amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            _: &Token,
        ) -> Result<BigUint, String> {
            let (reserve_in, reserve_out) = match token_in.symbol.as_str() {
                "A" => (BigUint::from(self.0), BigUint::from(self.1)),
                _ => (BigUint::from(self.1), BigUint::from(self.0)),
            };
            Ok(&reserve_out * &amount_in / (reserve_in + amount_in))
        }
    }

    fn tokens() -> Vec<Token> {
        vec![
            token("0x00000000000000000000000000000000000000a1", "A"),
            token("0x00000000000000000000000000000000000000b1", "B"),
        ]
    }

    #[test]
    fn a_sheet_answers_limits_spot_price_and_the_ladder_in_every_direction() {
        let sheet = quote_sheet(&Pool(100_000, 200_000), &tokens(), BigUint::clone);
        assert_eq!(sheet.len(), 10);
        assert_eq!(sheet["A -> B: limits"], "max input 100000, max output 200000");
        assert_eq!(sheet["A -> B: the quote at 1.0% of the limit"], "1980 for 1000 A");
        assert_eq!(sheet["B -> A: spot price"], "an error (no price)");
        // A direction with a zero limit is not quoted.
        let one_way = quote_sheet(&Pool(100_000, 0), &tokens(), BigUint::clone);
        assert_eq!(one_way["B -> A: limits"], "max input 0, max output 100000");
        assert!(!one_way.contains_key("B -> A: the quote at 1.0% of the limit"));
    }

    #[test]
    fn a_sheet_tells_apart_tokens_that_share_a_symbol() {
        let tokens = [
            token("0x00000000000000000000000000000000000000a1", "A"),
            token("0x00000000000000000000000000000000000000a2", "A"),
        ];
        let sheet = quote_sheet(&Pool(100_000, 200_000), &tokens, BigUint::clone);
        // Both directions are asked, rather than one overwriting the other.
        assert_eq!(sheet.len(), 10);
        assert!(sheet.contains_key(
            "A (0x00000000000000000000000000000000000000a1) -> A \
             (0x00000000000000000000000000000000000000a2): limits"
        ));
    }

    #[test]
    fn sheets_of_the_same_state_match_and_a_drifted_state_differs_by_question() {
        let streamed = quote_sheet(&Pool(100_000, 200_000), &tokens(), BigUint::clone);
        assert!(sheet_differences(&streamed, &streamed).is_empty());
        // A delta applied twice: one reserve off.
        let snapshot = quote_sheet(&Pool(100_000, 201_000), &tokens(), BigUint::clone);
        let differences = sheet_differences(&streamed, &snapshot);
        assert!(
            differences.contains(
                &"A -> B: limits: streamed max input 100000, max output 200000, snapshot max \
                  input 100000, max output 201000"
                    .to_string()
            ),
            "{differences:#?}"
        );
        let mut partial = snapshot.clone();
        partial.remove("A -> B: spot price");
        assert!(sheet_differences(&snapshot, &partial)
            .contains(&"A -> B: spot price: streamed 2.01, snapshot nothing".to_string()));
    }
}
