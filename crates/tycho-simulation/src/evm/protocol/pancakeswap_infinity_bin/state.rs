use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
};

use itertools::Either;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::evm::protocol::{
    pancakeswap_infinity_bin::{
        attributes::{decode_bin_id, decode_reserves, decode_u16, decode_u32, parse_bin_id},
        math::{
            bin::{get_amounts_out, get_drain_amounts, get_liquidity, get_price_from_id},
            constants::MAX_LIQUIDITY_PER_BIN,
            fee::{calculate_swap_fee, protocol_fee_amount},
        },
    },
    u256_num::u256_to_f64,
    utils::add_fee_markup,
};

// Rough figures, as in `uniswap_v2/state.rs`: a quote's gas only ranks routes, it never changes
// the amount out.
const SWAP_BASE_GAS: u64 = 120_000;
const GAS_PER_BIN_CROSSED: u64 = 10_000;

/// A PancakeSwap Infinity Bin pool.
///
/// `x` is `currency0`, `y` is `currency1`, so `swap_for_y` means `token_in < token_out` by address.
///
/// Only bins with liquidity are kept, and the `BTreeMap` order replaces the on-chain bitmap tree:
/// the next non-empty bin is a range query.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PancakeswapInfinityBinState {
    /// Bin the next swap starts in. u24 on chain.
    pub active_id: u32,
    /// Price ratio between neighbouring bins, in basis points.
    pub bin_step: u16,
    /// LP fee in pips (`1e6` denominator).
    pub lp_fee: u32,
    /// Protocol fee in pips charged when selling token0, the low 12 bits of the packed fee.
    pub protocol_fee_zero_for_one: u16,
    /// Protocol fee in pips charged when selling token1, the high 12 bits.
    pub protocol_fee_one_for_zero: u16,
    /// `bin id -> (reserve_x, reserve_y)`, absolute reserves as indexed from `reserveOfBin`.
    pub bins: BTreeMap<u32, (u128, u128)>,
}

/// Outcome of an exact-input swap.
pub struct BinSwapResult {
    pub amount_out: u128,
    /// Bins stepped into beyond the first. Drives the gas estimate.
    pub bins_crossed: u64,
    pub new_state: PancakeswapInfinityBinState,
}

impl PancakeswapInfinityBinState {
    /// Exact-input swap across as many bins as the input reaches.
    ///
    /// Errors `InvalidInput("Out of liquidity")` when the bins run out first: on chain the whole
    /// swap reverts (`BinPool__OutOfLiquidity`), so a partial fill would quote an unexecutable
    /// trade.
    ///
    /// [`BinPool.sol#L111-L222`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/BinPool.sol#L111-L222)
    pub fn swap(
        &self,
        swap_for_y: bool,
        amount_in: u128,
    ) -> Result<BinSwapResult, SimulationError> {
        // Fixed before the loop, not per bin. Protocol fee is charged first, LP fee on the rest.
        let protocol_fee = self.protocol_fee(swap_for_y);
        let swap_fee = self.swap_fee(swap_for_y);
        let mut new_state = self.clone();
        let mut active = self.active_id;
        let mut left = amount_in;
        let mut amount_out = 0u128;
        let mut bins_crossed = 0u64;

        loop {
            let (x, y) = new_state
                .bins
                .get(&active)
                .copied()
                .unwrap_or((0, 0));
            let reserve_out = if swap_for_y { y } else { x };

            // A bin with nothing on the output side is skipped, not an error.
            if reserve_out > 0 {
                let step = get_amounts_out(
                    reserve_out,
                    swap_fee,
                    self.bin_step,
                    swap_for_y,
                    active,
                    left,
                )?;
                // The protocol's cut leaves the pool: off the input, never off the output.
                let protocol_share = protocol_fee_amount(step.fee_amount, protocol_fee, swap_fee)?;
                let credited = step.amount_in_with_fee - protocol_share;

                let credit = |reserve: u128| {
                    reserve
                        .checked_add(credited)
                        .ok_or_else(|| {
                            SimulationError::InvalidInput(
                                format!("bin {active} reserve overflows crediting {credited}"),
                                None,
                            )
                        })
                };
                let (new_x, new_y) = if swap_for_y {
                    (credit(x)?, y - step.amount_out)
                } else {
                    (x - step.amount_out, credit(y)?)
                };

                let price = get_price_from_id(active, self.bin_step)?;
                if get_liquidity(new_x, new_y, price)? > MAX_LIQUIDITY_PER_BIN {
                    return Err(SimulationError::InvalidInput(
                        format!("bin {active} exceeds MAX_LIQUIDITY_PER_BIN after the swap"),
                        None,
                    ));
                }

                if new_x == 0 && new_y == 0 {
                    new_state.bins.remove(&active);
                } else {
                    new_state
                        .bins
                        .insert(active, (new_x, new_y));
                }

                left -= step.amount_in_with_fee;
                amount_out = amount_out
                    .checked_add(step.amount_out)
                    .ok_or_else(|| {
                        SimulationError::InvalidInput(
                            format!("output overflows u128 at bin {active}"),
                            None,
                        )
                    })?;
            }

            if left == 0 {
                break;
            }
            let Some(next) = new_state.next_bin_with_liquidity(swap_for_y, active) else {
                return Err(SimulationError::InvalidInput("Out of liquidity".to_string(), None));
            };
            active = next;
            bins_crossed += 1;
        }

        if amount_out == 0 {
            return Err(SimulationError::InvalidInput(
                format!("swap of {amount_in} produced no output"),
                None,
            ));
        }
        // The pool narrows the output with `safeInt128` before returning it.
        //
        // [`BinPool.sol#L210-L212`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/BinPool.sol#L210-L212)
        if amount_out > i128::MAX as u128 {
            return Err(SimulationError::InvalidInput(
                format!("output {amount_out} exceeds int128"),
                None,
            ));
        }

        new_state.active_id = active;
        Ok(BinSwapResult { amount_out, bins_crossed, new_state })
    }

    /// Next bin with liquidity, excluding `from`: lower id selling x, higher selling y.
    ///
    /// Matches `findFirstRight` / `findFirstLeft`, which step off `from` before searching despite
    /// what their natspec says, because the caller has already spent that bin.
    ///
    /// [`BinPool.sol#L293-L297`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/libraries/BinPool.sol#L293-L297)
    fn next_bin_with_liquidity(&self, swap_for_y: bool, from: u32) -> Option<u32> {
        if swap_for_y {
            self.bins
                .range(..from)
                .next_back()
                .map(|(&id, _)| id)
        } else {
            self.bins
                .range(from + 1..)
                .next()
                .map(|(&id, _)| id)
        }
    }

    /// First bin a swap takes output from: the active bin, else the next one in the swap's
    /// direction with reserves on the output side. The same bins `swap` would skip are skipped.
    fn first_bin_with_output(&self, swap_for_y: bool) -> Option<u32> {
        let has_output = |id: u32| {
            self.bins
                .get(&id)
                .is_some_and(|&(x, y)| if swap_for_y { y > 0 } else { x > 0 })
        };
        let mut bin = self.active_id;
        while !has_output(bin) {
            bin = self.next_bin_with_liquidity(swap_for_y, bin)?;
        }
        Some(bin)
    }

    /// Protocol fee in pips for one direction: `zero2one` when selling x.
    fn protocol_fee(&self, swap_for_y: bool) -> u16 {
        if swap_for_y {
            self.protocol_fee_zero_for_one
        } else {
            self.protocol_fee_one_for_zero
        }
    }

    /// Combined fee in pips charged on a bin: protocol fee first, LP fee on the remainder.
    fn swap_fee(&self, swap_for_y: bool) -> u32 {
        let protocol_fee = self.protocol_fee(swap_for_y);
        if protocol_fee == 0 {
            self.lp_fee
        } else {
            calculate_swap_fee(protocol_fee, self.lp_fee)
        }
    }
}

#[typetag::serde]
impl ProtocolSim for PancakeswapInfinityBinState {
    /// Swap fee as a ratio: protocol fee first, LP fee on the remainder.
    ///
    /// The protocol fee is directional, so one number only exists when both halves agree. The
    /// trait allows a panic otherwise, as `uniswap_v4/state.rs` does. The pools indexed so far
    /// carry equal halves, e.g. 3 pips each on the Base USDC/USDT pool.
    fn fee(&self) -> f64 {
        if self.protocol_fee_zero_for_one != self.protocol_fee_one_for_zero {
            unimplemented!(
                "Bin pools charge a directional protocol fee; use spot_price or get_amount_out"
            )
        }
        self.swap_fee(true) as f64 / 1_000_000.0
    }

    /// Price of the bin a buy of `base` fills first, fee markup included. Constant sum inside a
    /// bin, so it holds exactly until that bin empties, unlike a CLMM's marginal price.
    ///
    /// That bin is not always the active one: a burn never moves `activeId`, and a swap that ends
    /// with no input left can stop on a bin whose output side it just drained. When no bin holds
    /// `base`, the active bin's price is returned, as Uniswap v3/v4 price an empty pool; that the
    /// trade cannot fill is `get_limits`' and `get_amount_out`'s to report.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        // Buying base means selling quote: selling y (`swap_for_y` false) when base is x.
        let base_is_x = base < quote;
        let swap_for_y = !base_is_x;
        let bin = self
            .first_bin_with_output(swap_for_y)
            .unwrap_or(self.active_id);
        let price = u256_to_f64(get_price_from_id(bin, self.bin_step)?)? / 2.0f64.powi(128);

        // `price` is y per x, already quote-per-base when base is the lower address.
        let (token0, token1) = if base_is_x { (base, quote) } else { (quote, base) };
        let correction = 10f64.powi(token0.decimals as i32 - token1.decimals as i32);
        let price = if base_is_x { price * correction } else { 1.0 / (price * correction) };
        let swap_fee = self.swap_fee(swap_for_y);
        Ok(add_fee_markup(price, swap_fee as f64 / 1_000_000.0))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        // currency0 is the lower address, so selling it is the x -> y direction.
        let swap_for_y = token_in < token_out;
        let amount = amount_in
            .to_u128()
            .filter(|amount| *amount <= i128::MAX as u128)
            .ok_or_else(|| {
                SimulationError::InvalidInput(
                    format!("amount_in exceeds int128: {amount_in}"),
                    None,
                )
            })?;

        let BinSwapResult { amount_out, bins_crossed, new_state } =
            self.swap(swap_for_y, amount)?;
        let gas = SWAP_BASE_GAS + bins_crossed * GAS_PER_BIN_CROSSED;

        Ok(GetAmountOutResult::new(
            BigUint::from(amount_out),
            BigUint::from(gas),
            Box::new(new_state),
        ))
    }

    /// Maximum input the pool absorbs, and its output. `(0, 0)` when the direction is dry.
    ///
    /// Consumers treat this as a floor, so each bin contributes the same `max_amount_in` that
    /// `get_amounts_out` charges, the sum stops at the first bin whose drain input exceeds `u128`,
    /// and both totals stay within `i128::MAX`, since `get_amount_out` takes the input and returns
    /// the output as `int128`, as the pool does. Dust whose output rounds to zero still errors, as
    /// it does on chain. Any other math error is returned.
    fn get_limits(
        &self,
        token_in: Bytes,
        token_out: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let swap_for_y = token_in < token_out;
        let bins = if swap_for_y {
            Either::Left(self.bins.range(..=self.active_id).rev())
        } else {
            Either::Right(self.bins.range(self.active_id..))
        };

        let mut total_in = BigUint::ZERO;
        let mut total_out = BigUint::ZERO;
        let swap_fee = self.swap_fee(swap_for_y);
        let max_amount = BigUint::from(i128::MAX as u128);
        for (&id, &(x, y)) in bins {
            let reserve_out = if swap_for_y { y } else { x };
            if reserve_out == 0 {
                continue;
            }
            let price = get_price_from_id(id, self.bin_step)?;
            let Some(step) = get_drain_amounts(reserve_out, swap_fee, price, swap_for_y)? else {
                break;
            };
            if &total_in + BigUint::from(step.amount_in_with_fee) > max_amount ||
                &total_out + BigUint::from(step.amount_out) > max_amount
            {
                break;
            }
            total_in += BigUint::from(step.amount_in_with_fee);
            total_out += BigUint::from(step.amount_out);
        }
        Ok((total_in, total_out))
    }

    /// Applies an indexer delta.
    ///
    /// Encodings, as emitted by `8_map_protocol_changes.rs` in the substreams package:
    /// - `active_id`: `to_signed_bytes_be` of the u24 id, so 3 bytes below `0x800000`, 4 with a
    ///   leading `0x00` at or above. Decode length-tolerantly; a fixed-width `i24_be_bytes_to_i32`
    ///   copy breaks past that boundary, where the test pool sits
    /// - `fee`: LP fee in pips, emitted at creation; a static-fee pool never changes it
    /// - `protocol_fees/zero2one`, `protocol_fees/one2zero`: 12-bit, big-endian
    /// - `bins/{id}`: raw 32-byte `reserveOfBin` word, **y first 16 bytes, x last**. Swapping them
    ///   decodes cleanly and silently swaps the pool's balances
    /// - `deleted_attributes` under `bins/`: drop that bin
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        for (key, value) in delta.updated_attributes.iter() {
            match key.as_str() {
                "active_id" => self.active_id = decode_bin_id(key, value)?,
                "fee" => self.lp_fee = decode_u32(key, value)?,
                "protocol_fees/zero2one" => {
                    self.protocol_fee_zero_for_one = decode_u16(key, value)?
                }
                "protocol_fees/one2zero" => {
                    self.protocol_fee_one_for_zero = decode_u16(key, value)?
                }
                _ if key.starts_with("bins/") => {
                    let id = parse_bin_id(key)?;
                    let (x, y) = decode_reserves(key, value)?;
                    if x == 0 && y == 0 {
                        self.bins.remove(&id);
                    } else {
                        self.bins.insert(id, (x, y));
                    }
                }
                _ => {}
            }
        }

        for key in delta.deleted_attributes.iter() {
            if key.starts_with("bins/") {
                self.bins.remove(&parse_bin_id(key)?);
            }
        }

        Ok(())
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
        other
            .as_any()
            .downcast_ref::<PancakeswapInfinityBinState>()
            .is_some_and(|other| self == other)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::{
        primitives::{aliases::U24, Address, Bytes as AlloyBytes, FixedBytes, U256},
        providers::{Provider, ProviderBuilder},
        rpc::types::TransactionRequest,
        sol,
        sol_types::SolCall,
    };
    use approx::assert_relative_eq;
    use rstest::rstest;
    use tycho_client::feed::{dto, synchronizer::ComponentWithState};
    use tycho_common::models::Chain;

    use super::{
        super::{attributes::reserve_word, math::constants::REAL_ID_SHIFT},
        *,
    };
    use crate::evm::protocol::test_utils::try_decode_snapshot_with_defaults;

    /// Pool with `bin_step` 10 and a 100-pip LP fee, no protocol fee, so `swap_fee` is 100 pips
    /// and the expectations below reuse the `math::bin` vectors.
    fn bin_state(active_id: u32, bins: &[(u32, (u128, u128))]) -> PancakeswapInfinityBinState {
        PancakeswapInfinityBinState {
            active_id,
            bin_step: 10,
            lp_fee: 100,
            protocol_fee_zero_for_one: 0,
            protocol_fee_one_for_zero: 0,
            bins: bins.iter().copied().collect(),
        }
    }

    /// Fits inside the active bin: no crossing, no move, reserves shift by the step's amounts.
    #[test]
    fn test_swap_within_active_bin() {
        let state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (0, 1_000_000_000))]);

        let result = state.swap(true, 1_000_000).unwrap();

        assert_eq!(result.amount_out, 999_900);
        assert_eq!(result.bins_crossed, 0);
        assert_eq!(result.new_state.active_id, REAL_ID_SHIFT);
        assert_eq!(result.new_state.bins[&REAL_ID_SHIFT], (1_000_000, 1_000_000_000 - 999_900));
    }

    /// Drains the active bin and finishes in the next one down. Per-bin expectations come from the
    /// pinned `BinHelper.getAmountsOut`: 1001 in for 1000 out, then 999 in for 997 out.
    #[test]
    fn test_swap_crosses_into_next_bin() {
        let state = bin_state(
            REAL_ID_SHIFT,
            &[(REAL_ID_SHIFT - 1, (0, 5_000)), (REAL_ID_SHIFT, (0, 1_000))],
        );

        let result = state.swap(true, 2_000).unwrap();

        assert_eq!(result.amount_out, 1_997);
        assert_eq!(result.bins_crossed, 1);
        assert_eq!(result.new_state.active_id, REAL_ID_SHIFT - 1);
        assert_eq!(result.new_state.bins[&REAL_ID_SHIFT], (1_001, 0));
        assert_eq!(result.new_state.bins[&(REAL_ID_SHIFT - 1)], (999, 4_003));
    }

    /// With a protocol fee the pool keeps less than it charges: the protocol's share leaves the
    /// Vault, so the bin is credited `amount_in_with_fee` minus that share. Crediting the full
    /// amount would overstate every reserve after a swap and mis-price the next quote.
    ///
    /// Figures from the pinned Solidity for the pool the harness indexes, 3 pips per side over a
    /// 7-pip LP fee: swap fee 10 pips, so 1e6 in pays 10 units of fee, 3 of which go to the
    /// protocol and 7 stay in the bin.
    #[test]
    fn test_swap_withholds_the_protocol_fee_from_the_bin() {
        let mut state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (0, 1_000_000_000))]);
        state.lp_fee = 7;
        state.protocol_fee_zero_for_one = 3;
        state.protocol_fee_one_for_zero = 3;

        let result = state.swap(true, 1_000_000).unwrap();
        let (x, y) = result.new_state.bins[&REAL_ID_SHIFT];

        assert_eq!(result.amount_out, 999_990, "output nets the whole 10-pip swap fee");
        assert_eq!(x, 999_997, "the protocol's 3 units never reach the bin");
        assert_eq!(y, 1_000_000_000 - 999_990);
    }

    /// Selling y walks up: drains the active bin, skips a bin holding only y, finishes two bins
    /// further. Per-bin figures from the pinned `BinHelper.getAmountsOut` with `swapForY` false:
    /// 1001 in for 1000 out, then 1999 in for 1992 out.
    #[test]
    fn test_swap_selling_y_walks_up() {
        let state = bin_state(
            REAL_ID_SHIFT,
            &[
                (REAL_ID_SHIFT, (1_000, 500)),
                (REAL_ID_SHIFT + 1, (0, 2_000)),
                (REAL_ID_SHIFT + 3, (4_000, 0)),
            ],
        );

        let result = state.swap(false, 3_000).unwrap();

        assert_eq!(result.amount_out, 2_992);
        assert_eq!(result.bins_crossed, 2);
        assert_eq!(result.new_state.active_id, REAL_ID_SHIFT + 3);
        assert_eq!(result.new_state.bins[&REAL_ID_SHIFT], (0, 1_501));
        assert_eq!(result.new_state.bins[&(REAL_ID_SHIFT + 1)], (0, 2_000), "y-only bin untouched");
        assert_eq!(result.new_state.bins[&(REAL_ID_SHIFT + 3)], (2_008, 1_999));
    }

    /// Five bins around the reference id, 1e9 of each token in each.
    fn five_bins() -> PancakeswapInfinityBinState {
        let bins: Vec<_> = (REAL_ID_SHIFT - 2..=REAL_ID_SHIFT + 2)
            .map(|id| (id, (1_000_000_000, 1_000_000_000)))
            .collect();
        bin_state(REAL_ID_SHIFT, &bins)
    }

    /// Protocol fees across several bins, from a copy of `BinPool.swap`'s exact-input loop run
    /// on the pinned libraries. Unequal halves catch the two directions' fees being swapped:
    /// equal ones cannot. The max-fee case pairs the 10% LP cap with the 4000-pip protocol cap,
    /// which `calculateSwapFee` lets add up past 10%.
    #[rstest]
    #[case::selling_x_charges_zero_for_one(
        true, 100, 100, 400, 2_497_504_491, -2,
        [(1_498_549_858, 502_495_509), (2_001_100_121, 0), (2_000_100_021, 0)]
    )]
    #[case::selling_y_charges_one_for_zero(
        false, 100, 100, 400, 2_496_755_989, 2,
        [(0, 2_000_100_051), (0, 2_001_100_151), (503_244_011, 1_497_799_798)]
    )]
    #[case::max_lp_and_protocol_fee(
        true, 100_000, 4_000, 4_000, 2_239_520_717, -2,
        [(1_266_666_666, 760_479_283), (2_112_222_224, 0), (2_111_111_112, 0)]
    )]
    fn test_swap_withholds_directional_protocol_fee_across_bins(
        #[case] swap_for_y: bool,
        #[case] lp_fee: u32,
        #[case] zero_for_one: u16,
        #[case] one_for_zero: u16,
        #[case] expected_out: u128,
        #[case] final_offset: i32,
        // Bins the swap touched, in id order.
        #[case] touched: [(u128, u128); 3],
    ) {
        let mut state = five_bins();
        state.lp_fee = lp_fee;
        state.protocol_fee_zero_for_one = zero_for_one;
        state.protocol_fee_one_for_zero = one_for_zero;

        let result = state
            .swap(swap_for_y, 2_500_000_000)
            .unwrap();

        let final_id = REAL_ID_SHIFT
            .checked_add_signed(final_offset)
            .unwrap();
        assert_eq!(result.amount_out, expected_out);
        assert_eq!(result.new_state.active_id, final_id);
        let touched_ids = if swap_for_y {
            REAL_ID_SHIFT - 2..=REAL_ID_SHIFT
        } else {
            REAL_ID_SHIFT..=REAL_ID_SHIFT + 2
        };
        for (id, reserves) in touched_ids.zip(touched) {
            assert_eq!(result.new_state.bins[&id], reserves, "bin {id}");
        }
    }

    /// Input of exactly the bin's `max_amount_in` (1_000_100_011, from the pinned
    /// `getAmountsOut`) drains it and stops there: nothing is left, so the walk does not step on.
    /// One less takes the partial-fill branch and leaves a unit of y behind.
    #[rstest]
    #[case::exactly_max_drains(1_000_100_011, 1_000_000_000, (1_000_100_011, 0))]
    #[case::one_below_max_is_partial(1_000_100_010, 999_999_999, (1_000_100_010, 1))]
    fn test_swap_at_the_bin_capacity(
        #[case] amount_in: u128,
        #[case] expected_out: u128,
        #[case] active_reserves: (u128, u128),
    ) {
        let state = bin_state(
            REAL_ID_SHIFT,
            &[(REAL_ID_SHIFT - 1, (0, 1_000_000_000)), (REAL_ID_SHIFT, (0, 1_000_000_000))],
        );

        let result = state.swap(true, amount_in).unwrap();

        assert_eq!(result.amount_out, expected_out);
        assert_eq!(result.bins_crossed, 0);
        assert_eq!(result.new_state.active_id, REAL_ID_SHIFT);
        assert_eq!(result.new_state.bins[&REAL_ID_SHIFT], active_reserves);
        assert_eq!(result.new_state.bins[&(REAL_ID_SHIFT - 1)], (0, 1_000_000_000));
    }

    /// A bin at `MAX_LIQUIDITY_PER_BIN` (y = cap >> 128 at price 1) grows by the fee a swap
    /// leaves in it, which reverts `BinPool__MaxLiquidityPerBinExceeded` on chain.
    #[test]
    fn test_swap_errors_past_max_liquidity_per_bin() {
        let state = bin_state(
            REAL_ID_SHIFT,
            &[(REAL_ID_SHIFT, (0, 191_757_638_537_527_648_490_752_896_198_554))],
        );

        let err = state
            .swap(true, 1_000_000_000_000)
            .err()
            .expect("a swap past the per-bin cap must not quote");
        assert!(
            err.to_string()
                .contains("MAX_LIQUIDITY_PER_BIN"),
            "{err}"
        );
    }

    /// The protocol's cut leaves a drained bin, not the input it took, so limits computed from
    /// each bin's `max_amount_in` still quote exactly with unequal protocol fees.
    #[rstest]
    #[case::selling_x(true)]
    #[case::selling_y(false)]
    fn test_limits_quote_exactly_with_protocol_fees(#[case] selling_x: bool) {
        let mut state = five_bins();
        state.protocol_fee_zero_for_one = 100;
        state.protocol_fee_one_for_zero = 400;
        let (token_in, token_out) = if selling_x { (usdc(), usdt()) } else { (usdt(), usdc()) };

        let (max_in, max_out) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .unwrap();

        assert_eq!(
            state
                .get_amount_out(max_in.clone(), &token_in, &token_out)
                .unwrap()
                .amount,
            max_out
        );
        assert!(
            state
                .get_amount_out(max_in + BigUint::from(1u64), &token_in, &token_out)
                .is_err(),
            "one unit over the limit still quoted"
        );
    }

    /// A bin with nothing on the output side costs a crossing, not an error.
    #[test]
    fn test_swap_skips_bin_without_output_reserve() {
        let state = bin_state(
            REAL_ID_SHIFT,
            &[(REAL_ID_SHIFT - 1, (0, 5_000)), (REAL_ID_SHIFT, (5_000, 0))],
        );

        let result = state.swap(true, 999).unwrap();

        assert_eq!(result.amount_out, 997);
        assert_eq!(result.bins_crossed, 1);
        assert_eq!(result.new_state.bins[&REAL_ID_SHIFT], (5_000, 0));
    }

    /// Both on-chain reverts: `BinPool__OutOfLiquidity` when the bins run out before the input
    /// does, and `BinPool__InsufficientAmountUnSpecified` when the output rounds to zero. Neither
    /// may come back as a partial fill or a zero quote, which would not execute.
    #[rstest]
    #[case::bins_run_out(REAL_ID_SHIFT, (0, 1_000), 5_000)]
    #[case::output_rounds_to_zero(REAL_ID_SHIFT - 5_000, (0, 1_000_000_000), 1)]
    fn test_swap_errors(
        #[case] active_id: u32,
        #[case] reserves: (u128, u128),
        #[case] amount_in: u128,
    ) {
        let state = bin_state(active_id, &[(active_id, reserves)]);

        assert!(
            state.swap(true, amount_in).is_err(),
            "quoted {amount_in} where the pool would revert"
        );
    }

    /// Selling x walks down, selling y walks up, the spent bin is excluded either way, and running
    /// out in the swap direction is what makes `swap` error rather than partially fill.
    #[rstest]
    #[case::sells_x_walks_down(&[REAL_ID_SHIFT - 5, REAL_ID_SHIFT, REAL_ID_SHIFT + 7], true, Some(REAL_ID_SHIFT - 5))]
    #[case::sells_y_walks_up(&[REAL_ID_SHIFT - 5, REAL_ID_SHIFT, REAL_ID_SHIFT + 7], false, Some(REAL_ID_SHIFT + 7))]
    #[case::nothing_below(&[REAL_ID_SHIFT], true, None)]
    #[case::nothing_above(&[REAL_ID_SHIFT], false, None)]
    fn test_next_bin_with_liquidity(
        #[case] ids: &[u32],
        #[case] swap_for_y: bool,
        #[case] expected: Option<u32>,
    ) {
        let bins: Vec<_> = ids
            .iter()
            .map(|id| (*id, (1_000, 1_000)))
            .collect();
        let state = bin_state(REAL_ID_SHIFT, &bins);

        assert_eq!(state.next_bin_with_liquidity(swap_for_y, REAL_ID_SHIFT), expected);
    }

    fn token(address: &str, symbol: &str) -> Token {
        Token::new(&Bytes::from_str(address).unwrap(), symbol, 6, 0, &[], Chain::Base, 100)
    }

    /// Base USDC, the lower address of the pair, so currency0 and the x side.
    fn usdc() -> Token {
        token("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", "USDC")
    }

    /// Base USDT, currency1 and the y side.
    fn usdt() -> Token {
        token("0xfde4c96c8593536e31f229ea8f37b2ada2699bb2", "USDT")
    }

    /// An 18-decimal token sorting above both, so it is always the y side.
    fn wei() -> Token {
        Token::new(
            &Bytes::from_str("0xffffffffffffffffffffffffffffffffffffffff").unwrap(),
            "WEI",
            18,
            0,
            &[],
            Chain::Base,
            100,
        )
    }

    /// Reserves sit on the x side and the tokens are passed y -> x, so a direction taken from
    /// anything but address order errors instead of quoting. No crossing, so gas is the base.
    #[test]
    fn test_get_amount_out_follows_address_order() {
        let state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (1_000_000_000, 0))]);

        let result = state
            .get_amount_out(BigUint::from(1_000_000u64), &usdt(), &usdc())
            .unwrap();

        assert_eq!(result.amount, BigUint::from(999_900u64));
        assert_eq!(result.gas, BigUint::from(SWAP_BASE_GAS));
    }

    /// On chain the amount is an `int128`, so anything above that is rejected, not truncated.
    #[test]
    fn test_get_amount_out_rejects_amount_above_int128() {
        let state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (0, 1_000_000_000))]);
        let too_large = BigUint::from(i128::MAX as u128 + 1);

        assert!(
            state
                .get_amount_out(too_large, &usdc(), &usdt())
                .is_err(),
            "amounts above int128 must be rejected, not truncated"
        );
    }

    /// A bin priced near 2^-60 y per x holding almost `u128::MAX` of x pays out more x than
    /// `int128` holds for a modest input. The pool reverts on `safeInt128`, so the quote errors,
    /// and `get_limits` stops short of that bin.
    #[test]
    fn test_output_above_int128_is_rejected() {
        let id = REAL_ID_SHIFT - 41_616;
        let state = bin_state(id, &[(id, (u128::MAX - 1, 0))]);
        let target_out = i128::MAX as u128 + (u128::MAX - i128::MAX as u128) / 2;
        let price = get_price_from_id(id, state.bin_step).unwrap();
        let amount_in = (U256::from(target_out) * price) >> 128;

        let err = state
            .get_amount_out(BigUint::from(u128::try_from(amount_in).unwrap()), &usdt(), &usdc())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("exceeds int128"),
            "{err}"
        );

        let (_, max_out) = state
            .get_limits(usdt().address.clone(), usdc().address.clone())
            .unwrap();
        assert!(max_out <= BigUint::from(i128::MAX as u128), "limit {max_out} past int128");
    }

    /// Drains the active bin and finishes in the next, so gas carries one crossing.
    #[test]
    fn test_get_amount_out_charges_gas_per_crossing() {
        let state = bin_state(
            REAL_ID_SHIFT,
            &[(REAL_ID_SHIFT - 1, (0, 5_000)), (REAL_ID_SHIFT, (0, 1_000))],
        );

        let result = state
            .get_amount_out(BigUint::from(2_000u64), &usdc(), &usdt())
            .unwrap();

        assert_eq!(result.amount, BigUint::from(1_997u64));
        assert_eq!(result.gas, BigUint::from(SWAP_BASE_GAS + GAS_PER_BIN_CROSSED));
    }

    /// Priced at bin `2^23 + 100` rather than 1.0, so an inverted ratio is visible, and against an
    /// 18-decimal token, since two 6-decimal tokens hide a dropped decimals correction. Expected
    /// values come from the pinned `pow` with the 100-pip markup applied.
    #[rstest]
    #[case::base_is_x(usdc(), usdt(), 1.105226220342802)]
    #[case::base_is_y(usdt(), usdc(), 0.9049731282105973)]
    #[case::quote_has_18_decimals(usdc(), wei(), 1.105226220342802e-12)]
    fn test_spot_price(#[case] base: Token, #[case] quote: Token, #[case] expected: f64) {
        let state = bin_state(REAL_ID_SHIFT + 100, &[(REAL_ID_SHIFT + 100, (1_000, 1_000))]);

        assert_relative_eq!(
            state.spot_price(&base, &quote).unwrap(),
            expected,
            max_relative = 1e-12
        );
    }

    /// The active bin holds nothing to buy, as after a burn, so the price is the bin the buy
    /// fills first: liquidity sits at `2^23 + 100`, so the expectations match `test_spot_price`.
    /// The bin in between holds only the side being sold and is skipped, as `swap` skips it.
    #[rstest]
    #[case::buying_x_walks_up(
        usdc(), usdt(), REAL_ID_SHIFT + 90,
        &[(REAL_ID_SHIFT + 95, (0, 1_000)), (REAL_ID_SHIFT + 100, (1_000, 0))],
        1.105226220342802
    )]
    #[case::buying_y_walks_down(
        usdt(), usdc(), REAL_ID_SHIFT + 110,
        &[(REAL_ID_SHIFT + 100, (0, 1_000)), (REAL_ID_SHIFT + 105, (1_000, 0))],
        0.9049731282105973
    )]
    fn test_spot_price_skips_bins_without_base(
        #[case] base: Token,
        #[case] quote: Token,
        #[case] active_id: u32,
        #[case] bins: &[(u32, (u128, u128))],
        #[case] expected: f64,
    ) {
        let state = bin_state(active_id, bins);

        assert_relative_eq!(
            state.spot_price(&base, &quote).unwrap(),
            expected,
            max_relative = 1e-12
        );
    }

    /// Buying x sells y, so the `one2zero` protocol fee applies: swap fee 1100 pips over the
    /// 100-pip LP fee (`calculateSwapFee`), applied to the `base_is_x` price without its markup.
    #[test]
    fn test_spot_price_charges_the_fee_of_selling_quote() {
        let mut state = bin_state(REAL_ID_SHIFT + 100, &[(REAL_ID_SHIFT + 100, (1_000, 1_000))]);
        state.protocol_fee_one_for_zero = 1_000;

        assert_relative_eq!(
            state
                .spot_price(&usdc(), &usdt())
                .unwrap(),
            1.105226220342802 * (1.0 - 0.0001) / (1.0 - 0.0011),
            max_relative = 1e-12
        );
    }

    /// No bin holds the token being bought: the active bin's price, as an empty Uniswap v3 pool
    /// still has one, while the quote path reports that nothing can fill. Active bin `2^23 + 100`,
    /// so the expectation matches `test_spot_price`'s `base_is_y` case.
    #[test]
    fn test_spot_price_falls_back_to_the_active_bin_without_base_liquidity() {
        let state = bin_state(REAL_ID_SHIFT + 100, &[(REAL_ID_SHIFT + 100, (1_000, 0))]);

        assert_relative_eq!(
            state
                .spot_price(&usdt(), &usdc())
                .unwrap(),
            0.9049731282105973,
            max_relative = 1e-12
        );
        assert!(
            state
                .get_amount_out(BigUint::from(1_000u64), &usdc(), &usdt())
                .is_err(),
            "nothing to buy, so the quote must still refuse"
        );
    }

    #[test]
    fn test_fee_charges_protocol_then_lp() {
        let mut state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (1_000, 1_000))]);
        state.lp_fee = 3_000;
        state.protocol_fee_zero_for_one = 1_000;
        state.protocol_fee_one_for_zero = 1_000;

        assert_relative_eq!(state.fee(), 0.003997);
    }

    #[test]
    #[should_panic(expected = "directional protocol fee")]
    fn test_fee_rejects_asymmetric_protocol_fee() {
        let mut state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (1_000, 1_000))]);
        state.protocol_fee_zero_for_one = 1_000;

        state.fee();
    }

    #[test]
    fn test_limits_bound_what_can_be_quoted() {
        let state = bin_state(
            REAL_ID_SHIFT,
            &[(REAL_ID_SHIFT - 1, (0, 5_000)), (REAL_ID_SHIFT, (0, 1_000))],
        );

        let (max_in, max_out) = state
            .get_limits(usdc().address.clone(), usdt().address.clone())
            .unwrap();

        // Everything on the output side is reachable, and the limit itself still quotes.
        assert_eq!(max_out, BigUint::from(6_000u64));
        assert_eq!(
            state
                .get_amount_out(max_in.clone(), &usdc(), &usdt())
                .unwrap()
                .amount,
            max_out
        );
        // One wei past it runs out of bins.
        assert!(
            state
                .get_amount_out(max_in + BigUint::from(1u64), &usdc(), &usdt())
                .is_err(),
            "one wei over the limit still quoted"
        );
    }

    #[test]
    fn test_limits_are_zero_when_direction_is_dry() {
        let state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (0, 1_000))]);

        assert_eq!(
            state
                .get_limits(usdt().address.clone(), usdc().address.clone())
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }

    fn delta(updated: &[(&str, Bytes)], deleted: &[&str]) -> ProtocolStateDelta {
        ProtocolStateDelta {
            component_id: "test_pool".to_string(),
            updated_attributes: updated
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
            deleted_attributes: deleted
                .iter()
                .map(|key| key.to_string())
                .collect(),
        }
    }

    /// Applies a delta to a state, panicking on a decode error.
    fn apply(state: &mut PancakeswapInfinityBinState, delta: ProtocolStateDelta) {
        state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();
    }

    /// A `bins/{id}` word replaces that bin's reserves and leaves every other bin alone.
    #[test]
    fn test_delta_updates_bin_reserves() {
        let mut state = bin_state(
            REAL_ID_SHIFT,
            &[(REAL_ID_SHIFT - 1, (0, 5_000)), (REAL_ID_SHIFT, (0, 1_000))],
        );

        apply(&mut state, delta(&[(&format!("bins/{REAL_ID_SHIFT}"), reserve_word(7, 11))], &[]));

        let touched_bin = state.bins.get(&REAL_ID_SHIFT).unwrap();
        let existing_bin = state
            .bins
            .get(&(REAL_ID_SHIFT - 1))
            .unwrap();
        assert_eq!(*existing_bin, (0, 5_000));
        assert_eq!(*touched_bin, (7, 11));
    }

    /// A bin can be emptied two ways: an all-zero reserve word, or the key in
    /// `deleted_attributes`. The substreams classifies a zeroed bin as a deletion while still
    /// carrying the zero word, so both arrive in practice, and a bin left in the map costs the next
    /// swap a crossing over nothing.
    #[rstest]
    #[case::zero_word(vec![(format!("bins/{REAL_ID_SHIFT}"), reserve_word(0, 0))], vec![])]
    #[case::deleted_attribute(vec![], vec![format!("bins/{REAL_ID_SHIFT}")])]
    fn test_delta_removes_emptied_bin(
        #[case] updated: Vec<(String, Bytes)>,
        #[case] deleted: Vec<String>,
    ) {
        let mut state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (0, 1_000))]);
        let updated: Vec<_> = updated
            .iter()
            .map(|(key, value)| (key.as_str(), value.clone()))
            .collect();
        let deleted: Vec<_> = deleted
            .iter()
            .map(String::as_str)
            .collect();

        apply(&mut state, delta(&updated, &deleted));

        assert!(!state.bins.contains_key(&REAL_ID_SHIFT), "emptied bin stayed in the map");
    }

    /// `active_id` is emitted signed big-endian, so ids at or above `0x800000` arrive as 4 bytes
    /// with a leading `0x00`. The indexed pool sits above that boundary, and a fixed-width decoder
    /// passes the 3-byte case while failing this one.
    #[test]
    fn test_delta_moves_active_id() {
        let mut state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (0, 1_000))]);

        apply(&mut state, delta(&[("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x05]))], &[]));

        assert_eq!(state.active_id, REAL_ID_SHIFT + 5);
    }

    /// Protocol fees move on `ProtocolFeeUpdated`; `fee` is applied too, should it ever be sent.
    /// Values are big-endian and minimal length, so 500 pips is `[0x01, 0xf4]`.
    #[test]
    fn test_delta_updates_fees() {
        let mut state = bin_state(REAL_ID_SHIFT, &[(REAL_ID_SHIFT, (0, 1_000))]);

        apply(
            &mut state,
            delta(
                &[
                    ("fee", Bytes::from(vec![0x01, 0xf4])),
                    ("protocol_fees/zero2one", Bytes::from(vec![0x03, 0xe8])),
                    ("protocol_fees/one2zero", Bytes::from(vec![0x07, 0xd0])),
                    // Unknown keys are skipped rather than rejected.
                    ("balance_owner", Bytes::from(vec![0x01])),
                ],
                &[],
            ),
        );

        assert_eq!(state.lp_fee, 500);
        assert_eq!(state.protocol_fee_zero_for_one, 1000);
        assert_eq!(state.protocol_fee_one_for_zero, 2000);
    }

    sol! {
        /// [`IQuoter.sol`](https://github.com/pancakeswap/infinity-periphery/blob/56a7a4f5076d57d5ed90c38a00421ecbac0d649c/src/interfaces/IQuoter.sol),
        /// the Bin lens.
        struct PoolKey {
            address currency0;
            address currency1;
            address hooks;
            address poolManager;
            uint24 fee;
            bytes32 parameters;
        }

        struct QuoteExactSingleParams {
            PoolKey poolKey;
            bool zeroForOne;
            uint128 exactAmount;
            bytes hookData;
        }

        function quoteExactInputSingle(QuoteExactSingleParams memory params)
            external
            returns (uint256 amountOut, uint256 gasEstimate);
    }

    /// [`BinQuoter.sol`](https://github.com/pancakeswap/infinity-periphery/blob/56a7a4f5076d57d5ed90c38a00421ecbac0d649c/src/pool-bin/lens/BinQuoter.sol)
    /// on Base.
    const BIN_QUOTER: &str = "0xc631f4b0fc2dd68ad45f74b2942628db117dd359";
    /// Block the snapshot fixture was captured at. The quote has to be read at the same height.
    const SNAPSHOT_BLOCK: u64 = 32_980_292;

    fn bin_snapshot() -> ComponentWithState {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/assets/decoder/pancakeswap_infinity_bin_snapshot.json");
        let json = std::fs::read_to_string(path).expect("snapshot fixture");

        // Only the wire type is `Deserialize`; the decoder takes the converted one.
        serde_json::from_str::<dto::ComponentWithState>(&json)
            .expect("snapshot deserializes")
            .into()
    }

    /// The pool key the quoter needs, taken from the snapshot's static attributes rather than
    /// rebuilt, so the test cannot disagree with the indexer about which pool it is asking about.
    fn pool_key(snapshot: &ComponentWithState) -> PoolKey {
        let statics = &snapshot.component.static_attributes;
        let address = |value: &Bytes| Address::from_slice(value.as_ref());

        PoolKey {
            currency0: address(&snapshot.component.tokens[0]),
            currency1: address(&snapshot.component.tokens[1]),
            hooks: Address::ZERO,
            poolManager: address(&statics["pool_manager"]),
            fee: U24::from(u32::from(statics["key_lp_fee"].clone())),
            parameters: FixedBytes::from_slice(statics["parameters"].as_ref()),
        }
    }

    /// The port against the chain, end to end: the state decoded from a real snapshot must quote
    /// what `BinQuoter` quotes for the same input, to the wei.
    ///
    /// Everything else in this module checks the port against the pinned Solidity libraries in
    /// isolation. This is the only check that catches a divergence in how they are composed, or a
    /// snapshot that decodes into the wrong shape.
    #[tokio::test]
    #[ignore = "Requires BASE_RPC_URL pointing at a Base archive node"]
    async fn test_quotes_match_bin_quoter() {
        // Not RPC_URL: by repo convention that is Ethereum, where the quoter has no code.
        let rpc_url = std::env::var("BASE_RPC_URL").expect("BASE_RPC_URL must point at Base");
        let provider = ProviderBuilder::new()
            .connect(&rpc_url)
            .await
            .expect("provider");

        let snapshot = bin_snapshot();
        let key = pool_key(&snapshot);
        let token_x =
            Token::new(&snapshot.component.tokens[0].clone(), "X", 6, 0, &[], Chain::Base, 100);
        let token_y =
            Token::new(&snapshot.component.tokens[1].clone(), "Y", 6, 0, &[], Chain::Base, 100);
        let state = try_decode_snapshot_with_defaults::<PancakeswapInfinityBinState>(snapshot)
            .await
            .expect("snapshot decodes");

        // A revert is `None`, so "no quote" compares on both sides. Only the node's error response
        // counts: a transport failure would otherwise pass every expected revert.
        let quoter = |zero_for_one: bool, amount_in: u128| {
            let params = QuoteExactSingleParams {
                poolKey: key.clone(),
                zeroForOne: zero_for_one,
                exactAmount: amount_in,
                hookData: AlloyBytes::new(),
            };
            let request = TransactionRequest::default()
                .to(Address::from_str(BIN_QUOTER).unwrap())
                .input(
                    quoteExactInputSingleCall { params }
                        .abi_encode()
                        .into(),
                );
            let provider = provider.clone();
            async move {
                let returned = match provider
                    .call(request)
                    .block(SNAPSHOT_BLOCK.into())
                    .await
                {
                    Ok(returned) => returned,
                    Err(err) if err.as_error_resp().is_some() => return None,
                    Err(err) => panic!("quoter call failed: {err}"),
                };
                let quoted = quoteExactInputSingleCall::abi_decode_returns(&returned)
                    .expect("quoter response")
                    .amountOut;
                Some(BigUint::from_bytes_be(&quoted.to_be_bytes::<32>()))
            }
        };

        for (zero_for_one, token_in, token_out) in
            [(true, &token_x, &token_y), (false, &token_y, &token_x)]
        {
            let limit = state
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .expect("limits")
                .0
                .to_u128()
                .expect("limit fits u128");

            // Dust (zero output, so both sides must refuse it), inside the active bin, a few
            // crossings, about 20 crossings (the fixture's bins hold ~4.24 USDC), and the limit.
            let mut crossed = false;
            for amount_in in [1, 1_000, 10_000_000, 100_000_000, limit] {
                let ours = state
                    .get_amount_out(BigUint::from(amount_in), token_in, token_out)
                    .ok();
                crossed |= ours
                    .as_ref()
                    .is_some_and(|ours| ours.gas > BigUint::from(SWAP_BASE_GAS));

                assert_eq!(
                    ours.map(|ours| ours.amount),
                    quoter(zero_for_one, amount_in).await,
                    "zero_for_one={zero_for_one} amount={amount_in}: port and BinQuoter disagree"
                );
            }
            assert!(crossed, "zero_for_one={zero_for_one}: no amount crossed a bin");

            // One past the limit runs out of bins: the quoter reverts, the port must error.
            assert!(
                state
                    .get_amount_out(BigUint::from(limit + 1), token_in, token_out)
                    .is_err(),
                "zero_for_one={zero_for_one}: port quoted past its own limit"
            );
            assert_eq!(
                quoter(zero_for_one, limit + 1).await,
                None,
                "zero_for_one={zero_for_one}: BinQuoter filled past the port's limit"
            );
        }
    }
}
