use std::collections::HashMap;

use alloy::{
    primitives::{b256, Address, B256},
    providers::Provider,
    rpc::types::Filter,
};
use miette::{IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use rand::{rngs::StdRng, seq::SliceRandom, Rng};
use tycho_common::{models::token::Token, Bytes};

use crate::fuzz::Source;

const TRANSFER: B256 = b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
/// Blocks per `eth_getLogs` request, to keep each response small.
const LOG_CHUNK: u64 = 500;

/// Token amounts that recently moved into or out of pools, used as realistic swap sizes.
#[derive(Default)]
pub struct History {
    /// Per pool: every transfer of one of its tokens into or out of it.
    by_pool: HashMap<Address, Vec<(Address, BigUint)>>,
    /// Value in ETH of every recorded transfer whose token has a price, across all pools.
    eth_values: Vec<f64>,
}

impl History {
    /// Collects the transfers into and out of `pools` over the `window` blocks up to `block`.
    ///
    /// # Errors
    /// Fails if the node cannot serve the logs.
    pub async fn load(
        provider: &impl Provider,
        pools: &[Address],
        block: u64,
        window: u64,
        prices: &HashMap<Bytes, f64>,
    ) -> miette::Result<Self> {
        let mut history = History::default();
        if pools.is_empty() {
            return Ok(history);
        }
        let as_topics: Vec<B256> = pools
            .iter()
            .map(|pool| pool.into_word())
            .collect();
        let start = block.saturating_sub(window);
        for from in (start..=block).step_by(LOG_CHUNK as usize) {
            let to = (from + LOG_CHUNK - 1).min(block);
            for into_pool in [false, true] {
                let filter = Filter::new()
                    .from_block(from)
                    .to_block(to)
                    .event_signature(TRANSFER);
                let filter = if into_pool {
                    filter.topic2(as_topics.clone())
                } else {
                    filter.topic1(as_topics.clone())
                };
                let logs = provider
                    .get_logs(&filter)
                    .await
                    .into_diagnostic()
                    .wrap_err_with(|| format!("Failed to fetch transfers {from}..={to}"))?;
                for log in logs {
                    let topics = log.topics();
                    // ERC-721 transfers index the token id as a fourth topic.
                    if topics.len() != 3 || log.data().data.len() != 32 {
                        continue;
                    }
                    let pool = Address::from_word(topics[if into_pool { 2 } else { 1 }]);
                    let amount = BigUint::from_bytes_be(&log.data().data);
                    if amount.is_zero() {
                        continue;
                    }
                    let token = log.address();
                    if let Some(price) = prices.get(&Bytes::from(token.as_slice())) {
                        if let Some(value) = amount.to_f64() {
                            history.eth_values.push(value / price);
                        }
                    }
                    history
                        .by_pool
                        .entry(pool)
                        .or_default()
                        .push((token, amount));
                }
            }
        }
        Ok(history)
    }

    /// Draws up to `n` sell amounts from history for one pool direction.
    ///
    /// Takes the pool's own past amounts of the sell token as they are, its past amounts of the
    /// buy token converted at Tycho's token prices, and past trade values from other pools
    /// converted into the sell token. Each draw is also tried one wei to either side.
    pub fn samples(
        &self,
        pool: Option<Address>,
        sell: &Token,
        buy: &Token,
        prices: &HashMap<Bytes, f64>,
        rng: &mut StdRng,
        n: usize,
    ) -> Vec<(Source, BigUint)> {
        let sell_address = Address::from_slice(&sell.address);
        let buy_address = Address::from_slice(&buy.address);
        let sell_price = prices.get(&sell.address).copied();
        let buy_price = prices.get(&buy.address).copied();
        let own: &[(Address, BigUint)] = pool
            .and_then(|pool| self.by_pool.get(&pool))
            .map(Vec::as_slice)
            .unwrap_or_default();

        let mut same_token = Vec::new();
        let mut converted = Vec::new();
        for (token, amount) in own {
            if *token == sell_address {
                same_token.push(amount.clone());
            } else if *token == buy_address {
                if let (Some(sell_price), Some(buy_price), Some(value)) =
                    (sell_price, buy_price, amount.to_f64())
                {
                    converted.push(from_f64(value / buy_price * sell_price));
                }
            }
        }
        let other_pools: Vec<BigUint> = match sell_price {
            Some(sell_price) => self
                .eth_values
                .iter()
                .map(|eth| from_f64(eth * sell_price))
                .collect(),
            None => Vec::new(),
        };

        let mut drawn = Vec::new();
        for (label, values) in [
            ("pool_sell_token", &same_token),
            ("pool_buy_token", &converted),
            ("other_pools", &other_pools),
        ] {
            for base in values.choose_multiple(rng, n.div_ceil(3)) {
                if base.is_zero() {
                    continue;
                }
                drawn.push((Source::History(label, 0), base.clone()));
                let offset = if rng.gen_bool(0.5) { 1 } else { -1 };
                if let Some(neighbour) = crate::fuzz::offset_amount(base, offset) {
                    drawn.push((Source::History(label, offset), neighbour));
                }
            }
        }
        drawn.truncate(n);
        drawn
    }
}

fn from_f64(value: f64) -> BigUint {
    if !value.is_finite() || value < 1.0 {
        return BigUint::zero();
    }
    BigUint::parse_bytes(format!("{value:.0}").as_bytes(), 10).unwrap_or_default()
}

/// The pool address a component's transfers go to, if its id is one: singleton venues such as
/// Uniswap V4 identify pools by a hash and have no per-pool transfers.
pub fn pool_address(component_id: &Bytes) -> Option<Address> {
    (component_id.len() == 20).then(|| Address::from_slice(component_id))
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use tycho_common::models::Chain;

    use super::*;

    fn token(address: Address, decimals: u32) -> Token {
        Token::new(&Bytes::from(address.as_slice()), "T", decimals, 0, &[], Chain::Base, 100)
    }

    #[test]
    fn test_samples_convert_buy_and_other_pool_sizes_into_the_sell_token() {
        let (pool, other_pool) = (Address::repeat_byte(9), Address::repeat_byte(8));
        let (weth, usdc) = (Address::repeat_byte(1), Address::repeat_byte(2));
        // Raw units per ETH: 1 WETH, and 4000 USDC at 6 decimals.
        let prices = HashMap::from([
            (Bytes::from(weth.as_slice()), 1e18),
            (Bytes::from(usdc.as_slice()), 4000e6),
        ]);
        let history = History {
            by_pool: HashMap::from([
                (pool, vec![(weth, BigUint::from(5u64) * BigUint::from(10u64).pow(17))]),
                (other_pool, vec![(usdc, BigUint::from(8_000_000_000u64))]),
            ]),
            // 0.5 WETH into the pool and 8000 USDC elsewhere, i.e. 2 ETH.
            eth_values: vec![0.5, 2.0],
        };
        let mut history = history;
        // 2000 USDC out of the pool, worth 0.5 WETH.
        history
            .by_pool
            .get_mut(&pool)
            .expect("pool was inserted")
            .push((usdc, BigUint::from(2_000_000_000u64)));
        let mut rng = StdRng::seed_from_u64(1);

        let samples =
            history.samples(Some(pool), &token(weth, 18), &token(usdc, 6), &prices, &mut rng, 30);

        let exact: Vec<_> = samples
            .iter()
            .filter(|(source, _)| matches!(source, Source::History(_, 0)))
            .map(|(source, amount)| (source.to_string(), amount.to_string()))
            .collect();
        for expected in [
            ("history:pool_sell_token+0", "500000000000000000"),
            ("history:pool_buy_token+0", "500000000000000000"),
            ("history:other_pools+0", "2000000000000000000"),
        ] {
            assert!(
                exact.contains(&(expected.0.to_string(), expected.1.to_string())),
                "{expected:?} missing from {exact:?}"
            );
        }
    }
}
