//! Kuru CLOB + AMM vault: a port of `OrderBook.placeAndExecuteMarket{Buy,Sell}` matching
//! (Kuru-Labs/Kuru-contracts-dex-public `OrderBook.sol`, `AbstractAMM.sol`). Integer ops mirror
//! the Solidity ones one for one; Solidity reverts (checked overflow, `toU96`) map to errors.
//!
//! The book is held per price level (sum of order sizes): a market order fills a level's orders
//! FIFO but every quantity it produces depends on the level total only.
use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
};

use alloy::primitives::U256;
use num_bigint::BigUint;
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

use super::decoder::apply_attribute;
use crate::evm::protocol::{
    safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256, safe_sub_u256},
    u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    utils::solidity_math::{mul_div, mul_div_rounding_up as mul_div_up},
};

/// `vaultPricePrecision`: vault prices and `bestAsk()`/`bestBid()` are 1e18-scaled.
pub(super) const VPP: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const BPS: u64 = 10_000;
const DOUBLE_BPS: u64 = 20_000;
/// Solidity has no loop bound (it runs out of gas); this is ours.
const MAX_STEPS: usize = 100_000;
const BASE_GAS: u64 = 150_000;
const GAS_PER_LEVEL: u64 = 30_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vault {
    pub best_bid: U256,
    pub bid_partial: U256,
    pub best_ask: U256,
    pub ask_partial: U256,
    pub ask_size: U256,
    pub bid_size: U256,
    pub spread: U256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KuruState {
    pub base: Bytes,
    pub quote: Bytes,
    pub price_precision: U256,
    pub size_precision: U256,
    pub base_mult: U256,
    pub quote_mult: U256,
    pub taker_fee_bps: U256,
    pub maker_fee_bps: U256,
    pub active: bool,
    /// price (pricePrecision units) -> total size (sizePrecision units)
    pub bids: BTreeMap<u32, U256>,
    pub asks: BTreeMap<u32, U256>,
    pub vault: Vault,
}

/// Distinguishes "the book cannot fill this" from a revert of the ported arithmetic.
pub const INSUFFICIENT_LIQUIDITY: &str = "Kuru: insufficient book liquidity";

type R<T> = Result<T, SimulationError>;

fn revert(what: &str) -> SimulationError {
    SimulationError::InvalidInput(format!("Kuru: {what}"), None)
}
fn to_u96(x: U256) -> R<U256> {
    if x.bit_len() > 96 {
        return Err(revert("Uint96Overflow"));
    }
    Ok(x)
}
fn to_u32(x: U256) -> R<u32> {
    u32::try_from(x).map_err(|_| revert("Uint32Overflow"))
}
fn add96(a: U256, b: U256) -> R<U256> {
    to_u96(safe_add_u256(a, b)?)
}
/// FixedPointMathLib.mulDivRound (round half up).
fn mul_div_round(v: U256, m: U256, d: U256) -> R<U256> {
    safe_div_u256(safe_add_u256(safe_mul_u256(v, m)?, d / U256::from(2))?, d)
}
fn u(x: u64) -> U256 {
    U256::from(x)
}

impl KuruState {
    fn pp(&self) -> U256 {
        self.price_precision
    }

    /// `_getOBAsk()`: lowest ask, 1e18-scaled, or `uint256.max`.
    fn ob_ask(&self) -> U256 {
        match self.asks.keys().next() {
            Some(p) => U256::from(*p) * VPP / self.pp(),
            None => U256::MAX,
        }
    }

    /// `_getOBBid()`: highest bid, 1e18-scaled, or 0.
    fn ob_bid(&self) -> U256 {
        match self.bids.keys().next_back() {
            Some(p) => U256::from(*p) * VPP / self.pp(),
            None => U256::ZERO,
        }
    }

    /// `bestAsk()` -> (is_vault, price_1e18)
    fn best_ask(&self) -> (bool, U256) {
        let first_left = self
            .asks
            .keys()
            .next()
            .map_or(U256::ZERO, |p| U256::from(*p) * VPP / self.pp());
        let v = &self.vault;
        if !first_left.is_zero() {
            if v.best_ask != U256::MAX {
                if v.ask_size.is_zero() || first_left == v.best_ask {
                    return (false, first_left);
                }
                if first_left.min(v.best_ask) == v.best_ask {
                    return (true, v.best_ask);
                }
            }
            return (false, first_left);
        }
        if v.best_ask != U256::MAX && !v.ask_size.is_zero() {
            (true, v.best_ask)
        } else {
            (false, U256::ZERO)
        }
    }

    /// `bestBid()` -> (is_vault, price_1e18)
    fn best_bid(&self) -> (bool, U256) {
        let empty = U256::from(u32::MAX) * VPP / self.pp();
        let first_right = self
            .bids
            .keys()
            .next_back()
            .map_or(empty, |p| U256::from(*p) * VPP / self.pp());
        let v = &self.vault;
        if first_right != empty {
            if !v.best_bid.is_zero() {
                if v.bid_size.is_zero() || first_right == v.best_bid {
                    return (false, first_right);
                }
                if first_right.max(v.best_bid) == v.best_bid {
                    return (true, v.best_bid);
                }
            }
            return (false, first_right);
        }
        if !v.best_bid.is_zero() && !v.bid_size.is_zero() {
            (true, v.best_bid)
        } else {
            (false, U256::MAX)
        }
    }

    /// Fill `size` at one level; returns what is left (`_fillSizeForPrice` on the level total).
    fn fill_level(book: &mut BTreeMap<u32, U256>, price: u32, size: U256) -> U256 {
        let level = book
            .get(&price)
            .copied()
            .unwrap_or_default();
        if size >= level {
            book.remove(&price);
            size - level
        } else {
            if !size.is_zero() {
                book.insert(price, level - size);
            }
            U256::ZERO
        }
    }

    /// `AbstractAMM._fillVaultBuyMatch`
    fn fill_vault_buy_match(&mut self, break_point: U256, quote_size: U256) -> R<(U256, U256)> {
        let (pp, sp) = (self.pp(), self.size_precision);
        let s = self.vault.spread;
        let v = &mut self.vault;
        let mut price = v.best_ask;
        let mut quote_input = safe_mul_u256(quote_size, VPP)? / pp;
        let mut last = v.ask_size;
        let mut partial_bid = v.bid_partial;
        let mut available = safe_sub_u256(last, v.ask_partial)?;
        let mut size_filled = U256::ZERO;
        if quote_input >= mul_div_up(price, available, sp)? {
            v.ask_partial = U256::ZERO;
        }
        let mut steps = 0;
        while price < break_point && !quote_input.is_zero() {
            steps += 1;
            if steps > MAX_STEPS {
                return Err(revert("vault loop"));
            }
            let need = mul_div_up(price, available, sp)?;
            if need > quote_input {
                let filled = to_u96(safe_mul_u256(quote_input, sp)? / price)?;
                size_filled = add96(size_filled, filled)?;
                v.ask_partial = add96(v.ask_partial, filled)?;
                quote_input = U256::ZERO;
                break;
            }
            size_filled = add96(size_filled, available)?;
            quote_input -= need;
            last = to_u96(mul_div(last, u(DOUBLE_BPS), u(DOUBLE_BPS) + s)?)?;
            available = last;
            price = mul_div_round(price, u(BPS) + s, u(BPS))?;
            partial_bid = to_u96(mul_div(partial_bid, u(BPS), u(BPS) + s)?)?;
        }
        if price != v.best_ask {
            v.best_ask = price;
            v.best_bid = mul_div_round(price, u(BPS), u(BPS) + s)?;
            v.ask_size = last;
            v.bid_size = to_u96(mul_div(last, u(DOUBLE_BPS) + s, u(DOUBLE_BPS))?)?;
        }
        v.bid_partial = partial_bid;
        Ok((to_u96(mul_div(quote_input, pp, VPP)?)?, size_filled))
    }

    /// `AbstractAMM._fillVaultForSell`; returns (size_left, quote_owed_1e18)
    fn fill_vault_for_sell(&mut self, break_point: U256, size: U256) -> R<(U256, U256)> {
        let sp = self.size_precision;
        let s = self.vault.spread;
        let v = &mut self.vault;
        let mut price = v.best_bid;
        let mut available = safe_sub_u256(v.bid_size, v.bid_partial)?;
        let mut last = v.bid_size;
        let mut size_left = size;
        let mut owed = U256::ZERO;
        if size_left >= available {
            v.bid_partial = U256::ZERO;
        }
        let mut steps = 0;
        while price > break_point && !size_left.is_zero() {
            steps += 1;
            if steps > MAX_STEPS {
                return Err(revert("vault loop"));
            }
            if available > size_left {
                v.bid_partial = add96(v.bid_partial, size_left)?;
                owed += mul_div(price, size_left, sp)?;
                size_left = U256::ZERO;
                break;
            }
            size_left -= available;
            owed += mul_div(price, available, sp)?;
            last = to_u96(mul_div(last, u(DOUBLE_BPS) + s, u(DOUBLE_BPS))?)?;
            available = last;
            price = mul_div_round(price, u(BPS), u(BPS) + s)?;
        }
        if price != v.best_bid {
            v.best_bid = price;
            v.best_ask = mul_div_round(price, u(BPS) + s, u(BPS))?;
            v.bid_size = last;
            v.ask_size = to_u96(mul_div(last, u(DOUBLE_BPS), u(DOUBLE_BPS) + s)?)?;
        }
        Ok((size_left, owed))
    }

    /// `_marketBuyMatch`; `quote_size` in pricePrecision units. Returns (quote_left, base_out).
    pub fn market_buy(&mut self, mut quote_size: U256) -> R<(U256, U256)> {
        to_u96(quote_size)?;
        let (pp, sp) = (self.pp(), self.size_precision);
        let mut credit = U256::ZERO;
        let mut steps = 0;
        let (mut is_vault, mut best) = self.best_ask();
        while !quote_size.is_zero() && !best.is_zero() {
            steps += 1;
            if steps > MAX_STEPS {
                return Err(revert("match loop"));
            }
            if is_vault {
                let (left, filled) = self.fill_vault_buy_match(self.ob_ask(), quote_size)?;
                quote_size = left;
                credit = add96(credit, filled)?;
            } else {
                // uint96 * uint96 is uint96 in Solidity: checked there.
                let fillable = to_u96(
                    safe_mul_u256(to_u96(safe_mul_u256(quote_size, sp)?)?, VPP)? /
                        safe_mul_u256(best, pp)?,
                )?;
                let px = to_u32(best * pp / VPP)?;
                let left = Self::fill_level(&mut self.asks, px, fillable);
                credit = add96(credit, fillable - left)?;
                quote_size =
                    to_u96(mul_div(safe_mul_u256(best, pp)?, left, safe_mul_u256(sp, VPP)?)?)?;
            }
            (is_vault, best) = self.best_ask();
        }
        if credit.is_zero() {
            return Ok((quote_size, U256::ZERO));
        }
        let mut out = safe_mul_u256(credit, self.base_mult)? / sp;
        if !self.taker_fee_bps.is_zero() {
            out -= mul_div_up(out, self.taker_fee_bps, u(BPS))?;
        }
        Ok((quote_size, out))
    }

    /// `_matchAggressiveSell(0, size)`; `size` in sizePrecision units. Returns (size_left,
    /// quote_out).
    pub fn market_sell(&mut self, mut size: U256) -> R<(U256, U256)> {
        to_u96(size)?;
        let (pp, sp) = (self.pp(), self.size_precision);
        let (mut ob_quote, mut vault_quote) = (U256::ZERO, U256::ZERO);
        let mut steps = 0;
        let (mut is_vault, mut best) = self.best_bid();
        while !size.is_zero() && best != U256::MAX {
            steps += 1;
            if steps > MAX_STEPS {
                return Err(revert("match loop"));
            }
            if is_vault {
                // limit 0: `_getOBBid() >= 0` always, so the break point is the OB bid.
                let (left, owed) = self.fill_vault_for_sell(self.ob_bid(), size)?;
                size = left;
                vault_quote += owed;
            } else {
                let px = to_u32(best * pp / VPP)?;
                let left = Self::fill_level(&mut self.bids, px, size);
                ob_quote = add96(ob_quote, to_u96(mul_div(size - left, U256::from(px), sp)?)?)?;
                size = left;
            }
            (is_vault, best) = self.best_bid();
        }
        if ob_quote.is_zero() && vault_quote.is_zero() {
            return Ok((size, U256::ZERO));
        }
        let mut out = safe_mul_u256(ob_quote, self.quote_mult)? / pp +
            safe_mul_u256(vault_quote, self.quote_mult)? / VPP;
        if !self.taker_fee_bps.is_zero() {
            out -= mul_div_up(out, self.taker_fee_bps, u(BPS))?;
        }
        Ok((size, out))
    }

    /// Exact-in swap in token units. Input dust below one precision unit stays with the caller,
    /// as on chain (the market takes `uint96` sizes in its own precision).
    pub fn swap(&self, amount_in: U256, sell_base: bool) -> R<(U256, U256, Self)> {
        if !self.active {
            return Err(revert("market not active"));
        }
        let mut next = self.clone();
        let (left, out, used) = if sell_base {
            let size = to_u96(safe_mul_u256(amount_in, self.size_precision)? / self.base_mult)?;
            let (left, out) = next.market_sell(size)?;
            (left, out, safe_mul_u256(size, self.base_mult)? / self.size_precision)
        } else {
            let q = to_u96(safe_mul_u256(amount_in, self.pp())? / self.quote_mult)?;
            let (left, out) = next.market_buy(q)?;
            (left, out, safe_mul_u256(q, self.quote_mult)? / self.pp())
        };
        if !left.is_zero() {
            return Err(SimulationError::InvalidInput(INSUFFICIENT_LIQUIDITY.into(), None));
        }
        Ok((out, used, next))
    }

    fn side(&self, token_in: &Bytes, token_out: &Bytes) -> R<bool> {
        if *token_in == self.base && *token_out == self.quote {
            Ok(true)
        } else if *token_in == self.quote && *token_out == self.base {
            Ok(false)
        } else {
            Err(revert("token pair is not this market"))
        }
    }

    fn levels_touched(&self, next: &Self) -> u64 {
        (self
            .asks
            .len()
            .abs_diff(next.asks.len()) +
            self.bids
                .len()
                .abs_diff(next.bids.len())) as u64
    }
}

#[typetag::serde]
impl ProtocolSim for KuruState {
    fn fee(&self) -> f64 {
        f64::from(self.taker_fee_bps) / BPS as f64
    }

    /// Top of book incl. taker fee. `base`/`quote` are the caller's pair, not the market's.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let fee = 1.0 - self.fee();
        if base.address == self.base && quote.address == self.quote {
            let (_, ask) = self.best_ask();
            if ask.is_zero() {
                return Err(SimulationError::InvalidInput(INSUFFICIENT_LIQUIDITY.into(), None));
            }
            Ok(u256_to_f64(ask)? / 1e18 / fee)
        } else if base.address == self.quote && quote.address == self.base {
            let (_, bid) = self.best_bid();
            if bid == U256::MAX || bid.is_zero() {
                return Err(SimulationError::InvalidInput(INSUFFICIENT_LIQUIDITY.into(), None));
            }
            Ok(1e18 / u256_to_f64(bid)? / fee)
        } else {
            Err(revert("token pair is not this market"))
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let sell_base = self.side(&token_in.address, &token_out.address)?;
        let (out, _, next) = self.swap(biguint_to_u256(&amount_in), sell_base)?;
        let gas = BASE_GAS + GAS_PER_LEVEL * self.levels_touched(&next);
        Ok(GetAmountOutResult::new(u256_to_biguint(out), BigUint::from(gas), Box::new(next)))
    }

    /// Resting OB liquidity on the far side plus the vault's current quote (further vault steps
    /// only add to it).
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let sell_base = self.side(&sell_token, &buy_token)?;
        let (sp, pp) = (self.size_precision, self.pp());
        let (mut size, mut notional) = (U256::ZERO, U256::ZERO);
        for (p, s) in if sell_base { &self.bids } else { &self.asks } {
            size += *s;
            notional += *s * U256::from(*p) / sp;
        }
        let v = &self.vault;
        let (vault_size, vault_px) = if sell_base {
            (v.bid_size.saturating_sub(v.bid_partial), v.best_bid)
        } else {
            (v.ask_size.saturating_sub(v.ask_partial), v.best_ask)
        };
        if !vault_size.is_zero() && vault_px != U256::MAX && !vault_px.is_zero() {
            size += vault_size;
            // vault price is 1e18-scaled; notional stays in pricePrecision units
            notional += mul_div(mul_div(vault_size, vault_px, VPP)?, pp, sp)?;
        }
        let base = size * self.base_mult / sp;
        let quote = notional * self.quote_mult / pp;
        Ok(if sell_base {
            (u256_to_biguint(base), u256_to_biguint(quote))
        } else {
            (u256_to_biguint(quote), u256_to_biguint(base))
        })
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        for (k, v) in delta.updated_attributes {
            apply_attribute(self, &k, Some(&v)).map_err(TransitionError::DecodeError)?;
        }
        for k in delta.deleted_attributes {
            apply_attribute(self, &k, None).map_err(TransitionError::DecodeError)?;
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
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MON/USDC-shaped market: pp 1e8, sp 1e10, MON 18 dec, USDC 6 dec.
    fn market() -> KuruState {
        KuruState {
            base: Bytes::from([0u8; 20]),
            quote: Bytes::from([1u8; 20]),
            price_precision: u(100_000_000),
            size_precision: u(10_000_000_000),
            base_mult: U256::from(10u64).pow(u(18)),
            quote_mult: u(1_000_000),
            taker_fee_bps: U256::ZERO,
            maker_fee_bps: U256::ZERO,
            active: true,
            bids: BTreeMap::from([(2_880_000, u(5_000 * 10_000_000_000))]),
            asks: BTreeMap::from([
                (2_890_000, u(1_000 * 10_000_000_000)),
                (2_900_000, u(2_000 * 10_000_000_000)),
            ]),
            vault: Vault { best_ask: U256::MAX, spread: u(30), ..Default::default() },
        }
    }

    #[test]
    fn buy_walks_levels_exactly() {
        let mut m = market();
        // 1000 MON @ 0.0289 = 28.9 USDC, then 0.029 for the rest
        let q = u(2_890_000_000 + 100_000_000); // 28.9 + 1 USDC in pp 1e8
        let (left, out) = m.market_buy(q).unwrap();
        assert!(left.is_zero());
        // level 1: 1000 MON; level 2: 1 USDC / 0.029 = 34.482758620 MON (floored to sp)
        let best = u(2_900_000) * VPP / u(100_000_000);
        let lvl2 = u(100_000_000) * u(10_000_000_000) * VPP / (best * u(100_000_000));
        let want =
            (u(1000 * 10_000_000_000) + lvl2) * U256::from(10u64).pow(u(18)) / u(10_000_000_000);
        // the level-2 remainder is re-derived with a floored mulDiv, so a few size units drop
        assert!(out <= want && want - out < u(1_000_000_000_000), "{out} vs {want}");
        assert!(!m.asks.contains_key(&2_890_000));
    }

    #[test]
    fn sell_and_fee() {
        let mut m = market();
        m.taker_fee_bps = u(2);
        let (left, out) = m
            .market_sell(u(100 * 10_000_000_000))
            .unwrap();
        assert!(left.is_zero());
        let gross = u(100 * 10_000_000_000) * u(2_880_000) / u(10_000_000_000) * u(1_000_000) /
            u(100_000_000);
        assert_eq!(out, gross - mul_div_up(gross, u(2), u(BPS)).unwrap());
    }

    #[test]
    fn exhausted_book_errors() {
        let m = market();
        assert!(matches!(
            m.swap(U256::from(10u64).pow(u(30)), true),
            Err(SimulationError::InvalidInput(msg, _)) if msg == INSUFFICIENT_LIQUIDITY
        ));
    }

    #[test]
    fn vault_buy_steps_price() {
        let mut m = market();
        m.asks.clear();
        m.vault = Vault {
            best_ask: u(29_000_000_000_000_000),
            best_bid: u(28_913_260_219_341_975),
            ask_size: u(100 * 10_000_000_000),
            bid_size: u(100_150 * 100_000_000),
            spread: u(30),
            ..Default::default()
        };
        let before = m.vault.best_ask;
        let (left, out) = m
            .market_buy(u(10 * 100_000_000))
            .unwrap(); // 10 USDC
        assert!(left.is_zero() && !out.is_zero());
        assert!(m.vault.best_ask > before);
        assert_eq!(m.vault.best_bid, mul_div_round(m.vault.best_ask, u(BPS), u(BPS + 30)).unwrap());
    }

    fn tok(b: &Bytes, dec: u32) -> Token {
        Token::new(b, "T", dec, 0, &[], Default::default(), 100)
    }

    #[test]
    fn sell_walks_ob_then_vault() {
        let mut m = market();
        // vault bid 0.02878 sits below the OB bid 0.0288: OB first, then the vault
        m.vault = Vault {
            best_bid: u(28_780_000_000_000_000),
            best_ask: mul_div_round(u(28_780_000_000_000_000), u(BPS + 30), u(BPS)).unwrap(),
            bid_size: u(1_000 * 10_000_000_000),
            ask_size: u(998 * 10_000_000_000),
            spread: u(30),
            ..Default::default()
        };
        let size = u(5_500 * 10_000_000_000); // 5000 on the book + 500 into the vault
        let (left, out) = m.clone().market_sell(size).unwrap();
        assert!(left.is_zero());
        let ob = u(5_000) * u(2_880_000) * u(1_000_000) / u(100_000_000);
        let vault = u(500) * u(28_780_000_000_000_000) * u(1_000_000) / VPP;
        assert_eq!(out, ob + vault);
        let mut after = m.clone();
        after.market_sell(size).unwrap();
        assert!(after.bids.is_empty());
        assert_eq!(after.vault.bid_partial, u(500 * 10_000_000_000));
    }

    #[test]
    fn protocol_sim_surface() {
        let m = market();
        let (base, quote) = (tok(&m.base, 18), tok(&m.quote, 6));
        // buying base costs the best ask; buying quote costs 1/best bid
        assert!((m.spot_price(&base, &quote).unwrap() - 0.0289).abs() < 1e-12);
        assert!((m.spot_price(&quote, &base).unwrap() - 1.0 / 0.0288).abs() < 1e-9);
        let (max_in, max_out) = m
            .get_limits(quote.address.clone(), base.address.clone())
            .unwrap();
        // the whole ask side: 1000 @ 0.0289 + 2000 @ 0.029 = 86.9 USDC for 3000 MON
        assert_eq!(max_in, BigUint::from(86_900_000u64));
        assert_eq!(max_out, BigUint::from(3_000u64) * BigUint::from(10u64).pow(18));
        let r = m
            .get_amount_out(max_in, &quote, &base)
            .unwrap();
        assert!(r.amount <= max_out);
        let bad = tok(&Bytes::from([9u8; 20]), 18);
        assert!(m
            .get_amount_out(BigUint::from(1u8), &bad, &base)
            .is_err());
    }

    #[test]
    fn delta_transition_updates_levels_and_vault() {
        let mut m = market();
        let delta = ProtocolStateDelta {
            updated_attributes: HashMap::from([
                ("a/2890000".to_string(), Bytes::from(u128::to_be_bytes(7).to_vec())),
                ("b/2870000".to_string(), Bytes::from(u128::to_be_bytes(9).to_vec())),
                ("vault_spread".to_string(), Bytes::from(vec![40u8])),
                ("active".to_string(), Bytes::from(vec![0u8])),
            ]),
            deleted_attributes: ["a/2900000".to_string()].into(),
            ..Default::default()
        };
        m.delta_transition(delta, &HashMap::new(), &Default::default())
            .unwrap();
        assert_eq!(m.asks, BTreeMap::from([(2_890_000, u(7))]));
        assert_eq!(m.bids.get(&2_870_000), Some(&u(9)));
        assert_eq!(m.vault.spread, u(40));
        assert!(!m.active);
        assert!(m.swap(u(1), true).is_err());
    }
}
