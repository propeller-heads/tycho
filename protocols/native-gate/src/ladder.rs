use num_bigint::BigUint;
use num_traits::{One, Zero};
use tycho_common::models::token::Token;

/// Fractions of the sell limit the ladder visits, as powers of ten below it.
const LIMIT_DECADES: u32 = 6;
/// A limit above this many whole tokens is a placeholder, not liquidity: the ladder then spans
/// whole-token decades and visits the limit itself only once.
const PLAUSIBLE_LIMIT_DECADES: u32 = 12;

/// Returns the input amounts to check for a sell token, each with a label naming its rung.
///
/// The ladder covers dust (where rounding dominates), one whole token, decades below `limit`,
/// and the limit itself with its two neighbours. Without a plausible limit it spans whole-token
/// decades instead. Amounts are deduplicated and never zero.
pub fn amounts(sell: &Token, limit: Option<&BigUint>) -> Vec<(String, BigUint)> {
    let unit = BigUint::from(10u8).pow(sell.decimals);
    let mut rungs: Vec<(String, BigUint)> = [1u32, 2, 10, 1000]
        .into_iter()
        .map(|wei| (format!("dust:{wei}"), BigUint::from(wei)))
        .collect();
    rungs.push(("unit".to_string(), unit.clone()));
    let plausible = BigUint::from(10u8).pow(sell.decimals + PLAUSIBLE_LIMIT_DECADES);
    match limit {
        Some(limit) if !limit.is_zero() && *limit <= plausible => {
            for decade in (1..=LIMIT_DECADES).rev() {
                rungs.push((format!("limit/1e{decade}"), limit / BigUint::from(10u8).pow(decade)));
            }
            rungs.push(("limit-1".to_string(), limit - BigUint::one()));
            rungs.push(("limit".to_string(), limit.clone()));
            rungs.push(("limit+1".to_string(), limit + BigUint::one()));
        }
        _ => {
            for decade in 1..=3u32 {
                rungs.push((format!("unit/1e{decade}"), &unit / BigUint::from(10u8).pow(decade)));
            }
            for decade in 1..=6u32 {
                rungs.push((format!("unit*1e{decade}"), &unit * BigUint::from(10u8).pow(decade)));
            }
            if let Some(limit) = limit.filter(|limit| !limit.is_zero()) {
                rungs.push(("limit".to_string(), limit.clone()));
            }
        }
    }
    let mut seen = Vec::new();
    rungs.retain(|(_, amount)| {
        if amount.is_zero() || seen.contains(amount) {
            return false;
        }
        seen.push(amount.clone());
        true
    });
    rungs
}

#[cfg(test)]
mod tests {
    use tycho_common::{models::Chain, Bytes};

    use super::*;

    fn token(decimals: u32) -> Token {
        Token::new(&Bytes::zero(20), "T", decimals, 0, &[], Chain::Base, 100)
    }

    #[test]
    fn test_limit_ladder_ends_on_both_sides_of_the_limit() {
        let rungs = amounts(&token(6), Some(&BigUint::from(5_000_000_000u64)));

        let tail: Vec<_> = rungs[rungs.len() - 3..]
            .iter()
            .map(|(_, amount)| amount.clone())
            .collect();
        assert_eq!(
            tail,
            vec![
                BigUint::from(4_999_999_999u64),
                BigUint::from(5_000_000_000u64),
                BigUint::from(5_000_000_001u64)
            ]
        );
    }

    #[test]
    fn test_implausible_limit_falls_back_to_token_decades() {
        let rungs = amounts(&token(0), Some(&BigUint::from(10u64).pow(20)));

        let labels: Vec<_> = rungs
            .iter()
            .map(|(label, _)| label.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "dust:1",
                "dust:2",
                "dust:10",
                "dust:1000",
                "unit*1e2",
                "unit*1e4",
                "unit*1e5",
                "unit*1e6",
                "limit"
            ]
        );
    }

    #[test]
    fn test_ladder_skips_zero_and_duplicate_amounts() {
        let rungs = amounts(&token(0), Some(&BigUint::from(10u8)));

        let values: Vec<_> = rungs
            .iter()
            .map(|(_, amount)| amount.clone())
            .collect();
        assert_eq!(
            values,
            [1u32, 2, 10, 1000, 9, 11]
                .map(BigUint::from)
                .to_vec()
        );
    }
}
