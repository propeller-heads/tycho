use std::{any::Any, collections::HashMap, fmt};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::GetAmountOutParams, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::{IndicativelyPriced, SignedQuote},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::rfq::{
    client::RFQClient,
    protocols::biconomy_propamm::{
        client::PropAmmClient,
        models::{parse_biguint, biconomy_propamm_price_scale, PropAmmLevelsResponse, PropAmmMakerLevels},
    },
};

/// Indicative state of one directed PropAMM pair.
///
/// `base_token` is the pair's tokenIn and `quote_token` its tokenOut. The levels feed is
/// one-directional: simulating the reverse direction requires the reverse pair's own component.
#[derive(Clone, Serialize, Deserialize)]
pub struct PropAmmState {
    pub base_token: Token,
    pub quote_token: Token,
    pub levels: PropAmmLevelsResponse,
    pub client: PropAmmClient,
}

impl fmt::Debug for PropAmmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PropAmmState")
            .field("base_token", &self.base_token)
            .field("quote_token", &self.quote_token)
            .field("as_of", &self.levels.as_of)
            .finish_non_exhaustive()
    }
}

impl PropAmmState {
    pub fn new(
        base_token: Token,
        quote_token: Token,
        levels: PropAmmLevelsResponse,
        client: PropAmmClient,
    ) -> Self {
        Self { base_token, quote_token, levels, client }
    }

    fn ensure_sell_base(&self, token_in: &Token, token_out: &Token) -> Result<(), SimulationError> {
        if token_in == &self.base_token && token_out == &self.quote_token {
            Ok(())
        } else {
            Err(SimulationError::RecoverableError(format!(
                "PropAMM levels are one-directional ({} -> {}). Got in={}, out={}; use the \
                 reverse pair's component for the other direction",
                self.base_token.address,
                self.quote_token.address,
                token_in.address,
                token_out.address
            )))
        }
    }
}

/// One marginal tranche of a maker's ladder: `size` tokenIn wei tradable at `price`.
struct MarginalSegment {
    maker_idx: usize,
    /// 1e18-scaled price (tokenOut wei per tokenIn wei).
    price: BigUint,
    /// Marginal size in tokenIn wei.
    size: BigUint,
}

/// Expands per-maker cumulative ladders into marginal segments, sorted best-price-first.
///
/// Levels with non-increasing cumulative sizes are skipped defensively. Ties in price keep the
/// makers' original order (stable sort), so consumption is deterministic.
fn expand_marginal_segments(
    makers: &[PropAmmMakerLevels],
) -> Result<Vec<MarginalSegment>, SimulationError> {
    let mut segments = Vec::new();
    for (maker_idx, maker) in makers.iter().enumerate() {
        let mut previous_cumulative = BigUint::zero();
        for level in &maker.levels {
            let cumulative = parse_biguint(&level.size, "level size")?;
            if cumulative <= previous_cumulative {
                continue;
            }
            let size = &cumulative - &previous_cumulative;
            previous_cumulative = cumulative;
            segments.push(MarginalSegment {
                maker_idx,
                price: parse_biguint(&level.price, "level price")?,
                size,
            });
        }
    }
    segments.sort_by(|a, b| b.price.cmp(&a.price));
    Ok(segments)
}

struct SweepResult {
    /// Total tokenOut wei delivered across makers after per-maker settlement rounding.
    delivered: BigUint,
    /// Total tokenIn wei consumed (== amount_in unless liquidity ran out).
    consumed: BigUint,
}

/// Marginal-merge sweep over all makers' ladders.
///
/// Consumes pooled marginal segments best-price-first until `amount_in` is covered, then applies
/// each maker's settlement rounding. PropAMM inventory contracts settle a fill at a single
/// averaged price, so the delivered amount per maker follows this exact three-floor chain:
///
/// 1. `total_out = sum over consumed tranches of floor(take * price / 1e18)`
/// 2. `avg = floor(total_out * 1e18 / amount_in_maker)`
/// 3. `delivered_maker = floor(amount_in_maker * avg / 1e18)`
///
/// which can deliver up to a wei less than `total_out`. Simulating the same chain keeps
/// `get_amount_out` consistent with on-chain settlement to the wei.
fn sweep_amount_out(
    makers: &[PropAmmMakerLevels],
    amount_in: &BigUint,
) -> Result<SweepResult, SimulationError> {
    let scale = biconomy_propamm_price_scale();
    let segments = expand_marginal_segments(makers)?;

    // (amount_in_maker, total_out) accumulators per maker
    let mut consumed_per_maker = vec![(BigUint::zero(), BigUint::zero()); makers.len()];
    let mut remaining = amount_in.clone();

    for segment in &segments {
        if remaining.is_zero() {
            break;
        }
        let take = remaining
            .clone()
            .min(segment.size.clone());
        let tranche_out = (&take * &segment.price) / &scale;
        let (maker_in, maker_out) = &mut consumed_per_maker[segment.maker_idx];
        *maker_in += &take;
        *maker_out += tranche_out;
        remaining -= take;
    }

    let mut delivered = BigUint::zero();
    for (amount_in_maker, total_out) in &consumed_per_maker {
        if amount_in_maker.is_zero() {
            continue;
        }
        let avg = (total_out * &scale) / amount_in_maker;
        delivered += (amount_in_maker * &avg) / &scale;
    }

    Ok(SweepResult { delivered, consumed: amount_in - remaining })
}

/// Total depth in tokenIn wei across all makers' ladders.
fn total_depth(makers: &[PropAmmMakerLevels]) -> Result<BigUint, SimulationError> {
    Ok(expand_marginal_segments(makers)?
        .iter()
        .fold(BigUint::zero(), |acc, segment| acc + &segment.size))
}

/// Best (highest) marginal price across all makers, 1e18-scaled.
fn best_price(makers: &[PropAmmMakerLevels]) -> Result<Option<BigUint>, SimulationError> {
    Ok(expand_marginal_segments(makers)?
        .into_iter()
        .map(|segment| segment.price)
        .max())
}

#[typetag::serde]
impl ProtocolSim for PropAmmState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let best = best_price(&self.levels.makers)?.ok_or_else(|| {
            SimulationError::RecoverableError("No liquidity available".to_string())
        })?;
        let best = best.to_f64().ok_or_else(|| {
            SimulationError::RecoverableError("Can't convert best price to f64".into())
        })?;
        // Levels prices are 1e18-scaled tokenOut-wei per tokenIn-wei; convert to a human price.
        let price = best *
            10f64
                .powi(self.base_token.decimals as i32 - self.quote_token.decimals as i32 - 18i32);

        if base.address == self.base_token.address && quote.address == self.quote_token.address {
            Ok(price)
        } else if base.address == self.quote_token.address &&
            quote.address == self.base_token.address
        {
            Ok(1.0 / price)
        } else {
            Err(SimulationError::RecoverableError(format!(
                "Invalid token addresses: {}, {}",
                base.address, quote.address
            )))
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.ensure_sell_base(token_in, token_out)?;

        if self.levels.makers.is_empty() {
            return Err(SimulationError::RecoverableError("No liquidity".into()));
        }

        let sweep = sweep_amount_out(&self.levels.makers, &amount_in)?;
        let res = GetAmountOutResult {
            amount: sweep.delivered,
            gas: BigUint::from(265_000u64), // Gas estimate from PropAMM firm quotes
            new_state: self.clone_box(),    // The state doesn't change after a swap
        };

        if sweep.consumed < amount_in {
            return Err(SimulationError::InvalidInput(
                format!(
                    "Pool has not enough liquidity to support complete swap. input amount: \
                     {amount_in}, consumed amount: {}",
                    sweep.consumed
                ),
                Some(res),
            ));
        }

        Ok(res)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if !(sell_token == self.base_token.address && buy_token == self.quote_token.address) {
            return Err(SimulationError::RecoverableError(format!(
                "PropAMM levels are one-directional ({} -> {}). Got sell={sell_token}, \
                 buy={buy_token}; use the reverse pair's component for the other direction",
                self.base_token.address, self.quote_token.address
            )));
        }

        let sell_limit = total_depth(&self.levels.makers)?;
        if sell_limit.is_zero() {
            return Ok((BigUint::zero(), BigUint::zero()));
        }

        // The buy limit applies the same settlement rounding as get_amount_out at full depth, so
        // the advertised limit is always reachable.
        let buy_limit = sweep_amount_out(&self.levels.makers, &sell_limit)?.delivered;
        Ok((sell_limit, buy_limit))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        // RFQ updates arrive as full API snapshots, not block deltas.
        Err(TransitionError::DecodeError(
            "PropAMM RFQ state is snapshot-based and does not support deltas".into(),
        ))
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        if let Some(other_state) = other
            .as_any()
            .downcast_ref::<PropAmmState>()
        {
            self.base_token == other_state.base_token &&
                self.quote_token == other_state.quote_token &&
                self.levels == other_state.levels
        } else {
            false
        }
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }
}

#[async_trait]
impl IndicativelyPriced for PropAmmState {
    /// Requests a binding firm quote for this pair.
    ///
    /// IMPORTANT PropAMM rule: the firm quote expires hard at its `valid_until`. Consumers must
    /// refetch immediately before broadcast and never replay a stale response.
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        Ok(self
            .client
            .request_binding_quote(&params)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::biconomy_propamm::{
        client_builder::PropAmmClientBuilder, models::PropAmmLevel,
    };

    fn weth() -> Token {
        Token::new(
            &Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        )
    }

    fn usdc() -> Token {
        Token::new(
            &Bytes::from_str("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Base,
            100,
        )
    }

    fn empty_propamm_client() -> PropAmmClient {
        PropAmmClientBuilder::new(Chain::Base)
            .build()
            .unwrap()
    }

    fn maker(seed: u8, levels: &[(&str, &str)]) -> PropAmmMakerLevels {
        PropAmmMakerLevels {
            mm: Bytes::from(vec![seed; 20]),
            inventory_contract: Bytes::from(vec![seed.wrapping_add(100); 20]),
            levels: levels
                .iter()
                .map(|(size, price)| PropAmmLevel {
                    size: size.to_string(),
                    price: price.to_string(),
                })
                .collect(),
            nonce: "1".to_string(),
            expires_at: 1784889564,
        }
    }

    fn state_with_makers(makers: Vec<PropAmmMakerLevels>) -> PropAmmState {
        let base = weth();
        let quote = usdc();
        let levels = PropAmmLevelsResponse {
            chain_id: 8453,
            token_in: base.address.clone(),
            token_out: quote.address.clone(),
            merged: vec![],
            makers,
            as_of: 1784889534,
        };
        PropAmmState::new(base, quote, levels, empty_propamm_client())
    }

    fn fixture_state() -> PropAmmState {
        let json = std::fs::read_to_string("src/rfq/protocols/biconomy_propamm/test_responses/levels.json")
            .unwrap();
        let levels: PropAmmLevelsResponse = serde_json::from_str(&json).unwrap();
        PropAmmState::new(weth(), usdc(), levels, empty_propamm_client())
    }

    #[test]
    fn test_get_amount_out_three_floor_chain_loses_a_wei() {
        // Single maker, two cumulative tiers: 10M wei @ 0.275 and another 10M wei @ 0.265
        // (1e18-scaled prices). Swapping 15M crosses the 10M boundary:
        //   total_out = floor(10M * 0.275) + floor(5M * 0.265) = 2_750_000 + 1_325_000 =
        //   4_075_000
        //   avg       = floor(4_075_000 * 1e18 / 15M) = 271_666_666_666_666_666
        //   delivered = floor(15M * avg / 1e18) = 4_074_999 = total_out - 1 wei
        let state = state_with_makers(vec![maker(
            1,
            &[("10000000", "275000000000000000"), ("20000000", "265000000000000000")],
        )]);

        let result = state
            .get_amount_out(BigUint::from(15_000_000u64), &weth(), &usdc())
            .unwrap();

        assert_eq!(result.amount, BigUint::from(4_074_999u64));
        assert_eq!(result.gas, BigUint::from(265_000u64));
    }

    #[test]
    fn test_get_amount_out_two_maker_merge_interleaves_by_price() {
        // Maker A: 100 @ 2.0, then up to 300 cumulative @ 1.6
        // Maker B: 200 @ 1.8
        // Best-price-first consumption of 400: A(100 @ 2.0), B(200 @ 1.8), A(100 @ 1.6)
        //   A: in=200, total_out=200+160=360, avg=1.8e18, delivered=360
        //   B: in=200, total_out=360, avg=1.8e18, delivered=360
        let state = state_with_makers(vec![
            maker(1, &[("100", "2000000000000000000"), ("300", "1600000000000000000")]),
            maker(2, &[("200", "1800000000000000000")]),
        ]);

        let result = state
            .get_amount_out(BigUint::from(400u64), &weth(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(720u64));

        // 250 stops inside maker B's segment: A(100 @ 2.0) -> 200, B(150 @ 1.8) -> 270
        let result = state
            .get_amount_out(BigUint::from(250u64), &weth(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from(470u64));
    }

    #[test]
    fn test_get_amount_out_insufficient_liquidity() {
        // Total depth is 500; asking for 600 must error but still report the partial fill:
        //   A: in=300, total_out=floor(100*2.0)+floor(200*1.6)=520,
        //      avg=1_733_333_333_333_333_333, delivered=519
        //   B: in=200, delivered=360
        let state = state_with_makers(vec![
            maker(1, &[("100", "2000000000000000000"), ("300", "1600000000000000000")]),
            maker(2, &[("200", "1800000000000000000")]),
        ]);

        let result = state.get_amount_out(BigUint::from(600u64), &weth(), &usdc());
        match result {
            Err(SimulationError::InvalidInput(msg, Some(partial))) => {
                assert!(msg.contains("not enough liquidity"));
                assert_eq!(partial.amount, BigUint::from(879u64));
            }
            other => panic!("Expected InvalidInput with partial result, got: {other:?}"),
        }
    }

    #[test]
    fn test_get_amount_out_from_fixture() {
        // 15 WETH consumes maker 1's best tier (10 WETH @ 1878e6) and 5 WETH of maker 2's tier
        // (@ 1877e6): 18_780 USDC + 9_385 USDC, with no rounding loss at these sizes.
        let state = fixture_state();

        let result = state
            .get_amount_out(BigUint::from_str("15000000000000000000").unwrap(), &weth(), &usdc())
            .unwrap();
        assert_eq!(result.amount, BigUint::from_str("28165000000").unwrap());
    }

    #[test]
    fn test_get_amount_out_rejects_reverse_direction() {
        let state = fixture_state();
        let result =
            state.get_amount_out(BigUint::from_str("28165000000").unwrap(), &usdc(), &weth());
        assert!(result.is_err());
    }

    #[test]
    fn test_get_amount_out_no_liquidity() {
        let state = state_with_makers(vec![]);
        let result = state.get_amount_out(BigUint::from(1u64), &weth(), &usdc());
        assert!(result.is_err());
    }

    #[test]
    fn test_skips_non_increasing_cumulative_levels() {
        // The second level repeats the first's cumulative size and must be ignored.
        let state = state_with_makers(vec![maker(
            1,
            &[("100", "2000000000000000000"), ("100", "1500000000000000000")],
        )]);

        let (sell_limit, buy_limit) = state
            .get_limits(weth().address, usdc().address)
            .unwrap();
        assert_eq!(sell_limit, BigUint::from(100u64));
        assert_eq!(buy_limit, BigUint::from(200u64));
    }

    #[test]
    fn test_spot_price() {
        let state = fixture_state();

        // Best merged price is 1878e6 (1e18-scaled tokenOut wei per tokenIn wei); with
        // WETH(18) -> USDC(6) that is a human price of 1878 USDC per WETH.
        let price = state
            .spot_price(&weth(), &usdc())
            .unwrap();
        assert!((price - 1878.0).abs() < 1e-6);

        let inverted = state
            .spot_price(&usdc(), &weth())
            .unwrap();
        assert!((inverted - 1.0 / 1878.0).abs() < 1e-12);
    }

    #[test]
    fn test_spot_price_no_liquidity() {
        let state = state_with_makers(vec![]);
        assert!(state
            .spot_price(&weth(), &usdc())
            .is_err());
    }

    #[test]
    fn test_get_limits_from_fixture() {
        let state = fixture_state();

        let (sell_limit, buy_limit) = state
            .get_limits(weth().address, usdc().address)
            .unwrap();

        // Total depth: maker 1 has 30 WETH cumulative, maker 2 has 20 WETH.
        assert_eq!(sell_limit, BigUint::from_str("50000000000000000000").unwrap());
        // Full sweep with settlement rounding:
        //   maker 1: total_out=56_300 USDC over 30 WETH, avg=1_876_666_666,
        //            delivered=56_299.99998 USDC
        //   maker 2: 37_540 USDC exactly
        assert_eq!(buy_limit, BigUint::from_str("93839999980").unwrap());
    }

    #[test]
    fn test_get_limits_rejects_reverse_direction() {
        let state = fixture_state();
        assert!(state
            .get_limits(usdc().address, weth().address)
            .is_err());
    }

    #[test]
    fn test_get_limits_no_liquidity() {
        let state = state_with_makers(vec![]);
        let (sell_limit, buy_limit) = state
            .get_limits(weth().address, usdc().address)
            .unwrap();
        assert_eq!(sell_limit, BigUint::zero());
        assert_eq!(buy_limit, BigUint::zero());
    }

    #[test]
    fn test_fee_and_eq() {
        let state = fixture_state();
        assert_eq!(state.fee(), 0.0);
        let other = fixture_state();
        assert!(ProtocolSim::eq(&state, &other));

        let different = state_with_makers(vec![maker(1, &[("100", "2000000000000000000")])]);
        assert!(!ProtocolSim::eq(&state, &different));
    }
}
