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

use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        fluid::find_max_valid_u256,
        fluid_v2::{
            math::{self, SwapOutcome, SwapPool},
            vm::{self, TokenLiquidity},
        },
        safe_math::{safe_add_u256, safe_div_u256, safe_mul_u256},
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
        utils::add_fee_markup,
    },
};

/// Gas of a whole swap through `FluidV2Executor`, measured on a mainnet fork at block 26331500
/// where every settle went through Fluid Liquidity. A D4 swap always syncs a borrow leg with
/// Liquidity. Fluid's docs put serving both settles from the dex balance about 75k lower.
const GAS_D3_THROUGH_LIQUIDITY: u64 = 255_000;
const GAS_D4: u64 = 277_000;
const GAS_SAVED_SETTLING_FROM_DEX_BALANCE: u64 = 75_000;
const GAS_PER_INITIALIZED_TICK_CROSSED: u64 = 21_492;

const EXCHANGE_PRICES_PRECISION: u64 = 1_000_000_000_000;
const TOKENS_DECIMALS_PRECISION: u8 = 9;
const ROUNDING_FACTOR: u64 = 1_000_000_000;
const MIN_AMOUNT: u64 = 10_000;
const MAX_AMOUNT_BITS: usize = 128;
const MAX_ADJUSTED_AMOUNT_BITS: usize = 86;
const SQRT_PRICE_EXPONENT_BITS: usize = 8;
const SQRT_PRICE_COEFFICIENT_BITS: usize = 64;
const FLUID_NATIVE_TOKEN: [u8; 20] = [0xee; 20];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DexType {
    /// Smart collateral: LP liquidity is supplied to Fluid Liquidity.
    D3,
    /// Smart debt: LP liquidity is borrowed from Fluid Liquidity.
    D4,
}

/// A Fluid DEX v2 D3 or D4 pool: Uniswap-v3-style concentrated liquidity whose amounts are
/// normalized by Fluid Liquidity exchange prices and paid out through the dex and Liquidity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FluidV2State {
    dex_type: DexType,
    token0: Token,
    token1: Token,
    /// The DEX v2 resolver whose `getTokenBaseState` reads the tokens' Liquidity state.
    resolver: Bytes,
    tick_spacing: i32,
    dex_variables: DexVariables,
    dex_variables2: DexVariables2,
    /// Raw-adjusted reserves, bounded by the pool only while per-pool accounting runs.
    token0_reserve: U256,
    token1_reserve: U256,
    /// Initialized ticks and their net liquidity.
    ticks: BTreeMap<i32, i128>,
    token0_liquidity: TokenLiquidity,
    token1_liquidity: TokenLiquidity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DexVariables {
    pub current_tick: i32,
    /// The stored sqrt price: a 64-bit coefficient and an 8-bit exponent, rounded down.
    pub current_sqrt_price_x96: U256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DexVariables2 {
    pub protocol_fee_0_to_1: u64,
    pub protocol_fee_1_to_0: u64,
    pub protocol_cut_fee: u64,
    pub token0_decimals: u8,
    pub token1_decimals: u8,
    pub active_liquidity: u128,
    /// Set once `stopPerPoolAccounting` ran: reserves no longer bound the pool.
    pub per_pool_accounting_stopped: bool,
    /// Whether the pool asks its controller for the fee of every swap.
    pub fetch_dynamic_fee: bool,
    pub fee_version: u8,
    /// The constant LP fee of fee version 0, in pips.
    pub lp_fee: u64,
}

/// One exact-input swap quote.
struct Quote {
    amount_out: U256,
    gas: u64,
    state: FluidV2State,
}

impl FluidV2State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dex_type: DexType,
        token0: Token,
        token1: Token,
        resolver: Bytes,
        tick_spacing: i32,
        dex_variables: DexVariables,
        dex_variables2: DexVariables2,
        token0_reserve: U256,
        token1_reserve: U256,
        ticks: BTreeMap<i32, i128>,
        token0_liquidity: TokenLiquidity,
        token1_liquidity: TokenLiquidity,
    ) -> Self {
        Self {
            dex_type,
            token0,
            token1,
            resolver,
            tick_spacing,
            dex_variables,
            dex_variables2,
            token0_reserve,
            token1_reserve,
            ticks,
            token0_liquidity,
            token1_liquidity,
        }
    }

    /// Reads both tokens' Liquidity state through the DEX v2 resolver in the shared VM database.
    pub(super) fn fetch_token_liquidity(
        resolver: &Bytes,
        token0: &Bytes,
        token1: &Bytes,
    ) -> Result<(TokenLiquidity, TokenLiquidity), SimulationError> {
        let engine = create_engine(SHARED_TYCHO_DB.clone(), false).expect("Infallible");
        let resolver = alloy::primitives::Address::from_slice(resolver);
        let token0 = vm::fetch_token_liquidity(&engine, resolver, to_fluid_token(token0))?;
        let token1 = vm::fetch_token_liquidity(&engine, resolver, to_fluid_token(token1))?;
        Ok((token0, token1))
    }

    fn zero_for_one(&self, token_in: &Bytes, token_out: &Bytes) -> Result<bool, SimulationError> {
        if *token_in == self.token0.address && *token_out == self.token1.address {
            Ok(true)
        } else if *token_in == self.token1.address && *token_out == self.token0.address {
            Ok(false)
        } else {
            Err(SimulationError::InvalidInput(
                format!("Fluid v2 pool does not trade {token_in} for {token_out}"),
                None,
            ))
        }
    }

    /// Rejects the pools whose fee this simulation cannot reproduce.
    fn check_supported(&self) -> Result<(), SimulationError> {
        if self.dex_variables2.fetch_dynamic_fee {
            return Err(SimulationError::FatalError(
                "Fluid v2 pools that fetch their fee from a controller are not supported"
                    .to_string(),
            ));
        }
        if self.dex_variables2.fee_version != 0 {
            return Err(SimulationError::FatalError(format!(
                "Fluid v2 fee version {} is not supported",
                self.dex_variables2.fee_version
            )));
        }
        Ok(())
    }

    fn token_liquidity(&self, is_token0: bool) -> &TokenLiquidity {
        if is_token0 {
            &self.token0_liquidity
        } else {
            &self.token1_liquidity
        }
    }

    /// The exchange price the pool math runs on: supply for D3, borrow for D4.
    fn exchange_price(&self, is_token0: bool) -> Result<U256, SimulationError> {
        let liquidity = self.token_liquidity(is_token0);
        match self.dex_type {
            // D3 prices a token Liquidity does not list at 1.
            DexType::D3
                if liquidity
                    .supply_exchange_price
                    .is_zero() =>
            {
                Ok(U256::from(EXCHANGE_PRICES_PRECISION))
            }
            DexType::D3 => Ok(liquidity.supply_exchange_price),
            DexType::D4
                if liquidity
                    .borrow_exchange_price
                    .is_zero() =>
            {
                Err(SimulationError::FatalError(
                    "Fluid v2 D4 token has no borrow exchange price".to_string(),
                ))
            }
            DexType::D4 => Ok(liquidity.borrow_exchange_price),
        }
    }

    fn decimals(&self, is_token0: bool) -> u8 {
        if is_token0 {
            self.dex_variables2.token0_decimals
        } else {
            self.dex_variables2.token1_decimals
        }
    }

    fn swap_pool(&self, zero_for_one: bool) -> SwapPool<'_> {
        SwapPool {
            ticks: &self.ticks,
            tick_spacing: self.tick_spacing,
            tick: self.dex_variables.current_tick,
            sqrt_price_x96: decode_sqrt_price(
                self.dex_variables
                    .current_sqrt_price_x96,
                zero_for_one,
            ),
            liquidity: self.dex_variables2.active_liquidity,
            protocol_fee: if zero_for_one {
                self.dex_variables2.protocol_fee_0_to_1
            } else {
                self.dex_variables2.protocol_fee_1_to_0
            },
            protocol_cut_fee: self.dex_variables2.protocol_cut_fee,
            lp_fee: self.dex_variables2.lp_fee,
        }
    }

    fn quote(&self, amount_in: U256, zero_for_one: bool) -> Result<Quote, SimulationError> {
        self.check_supported()?;
        verify_amount(amount_in)?;
        let token_in =
            Normalization::new(self.decimals(zero_for_one), self.exchange_price(zero_for_one)?);
        let token_out =
            Normalization::new(self.decimals(!zero_for_one), self.exchange_price(!zero_for_one)?);

        let amount_in_raw = token_in.to_raw_round_down(amount_in)?;
        verify_adjusted_amount(amount_in_raw)?;
        let outcome =
            math::swap_in(&self.swap_pool(zero_for_one), zero_for_one, amount_in_raw, None)?;
        verify_adjusted_amount(outcome.amount_out_raw)?;
        let amount_out = token_out.to_amount_round_down(outcome.amount_out_raw)?;
        verify_amount(amount_out)?;

        let fee_out = token_out
            .to_amount_round_down(safe_add_u256(outcome.protocol_fee_raw, outcome.lp_fee_raw)?)?;
        let settled_from_dex_balance =
            self.check_settles(zero_for_one, amount_in, amount_out, fee_out)?;
        let state = self.after_swap(zero_for_one, amount_in_raw, &outcome, amount_out, fee_out)?;
        let base_gas = match self.dex_type {
            DexType::D3 if settled_from_dex_balance => {
                GAS_D3_THROUGH_LIQUIDITY - GAS_SAVED_SETTLING_FROM_DEX_BALANCE
            }
            DexType::D3 => GAS_D3_THROUGH_LIQUIDITY,
            DexType::D4 => GAS_D4,
        };
        Ok(Quote {
            amount_out,
            gas: base_gas + GAS_PER_INITIALIZED_TICK_CROSSED * outcome.initialized_ticks_crossed,
            state,
        })
    }

    /// Checks that the dex can settle both legs, and returns whether it serves both from its own
    /// token balance without calling Fluid Liquidity.
    ///
    /// D3 supplies the token in and withdraws the token out. A leg that fits the dex balance stays
    /// there; a larger one goes to Liquidity, which rejects it while that leg is paused. D4 pays
    /// back the token in and borrows the token out plus the swap fee, and syncs every borrow leg
    /// with Liquidity.
    ///
    /// Not modeled: Liquidity also rejects a leg too small to change the dex's stored position
    /// (`UserModule__OperateAmountInsufficient`). Only dust swaps hit it, such as the fee leg of
    /// a D4 swap of 0.01 USDC.
    fn check_settles(
        &self,
        zero_for_one: bool,
        amount_in: U256,
        amount_out: U256,
        fee_out: U256,
    ) -> Result<bool, SimulationError> {
        let liquidity_in = self.token_liquidity(zero_for_one);
        let liquidity_out = self.token_liquidity(!zero_for_one);
        match self.dex_type {
            DexType::D3 => {
                let in_from_balance = amount_in <= liquidity_in.float_balance;
                let out_from_balance = amount_out <= liquidity_out.float_balance;
                if !in_from_balance && liquidity_in.supply_paused {
                    return Err(liquidity_rejects("supply", &self.token_address(zero_for_one)));
                }
                let payout_cap = if liquidity_out.withdraw_paused {
                    liquidity_out.float_balance
                } else {
                    liquidity_out.available_for_swap_out_withdraw
                };
                if amount_out > payout_cap {
                    return Err(payout_exceeded(amount_out, payout_cap));
                }
                Ok(in_from_balance && out_from_balance)
            }
            DexType::D4 => {
                if liquidity_in.payback_paused {
                    return Err(liquidity_rejects("payback", &self.token_address(zero_for_one)));
                }
                if liquidity_out.borrow_paused {
                    return Err(liquidity_rejects("borrow", &self.token_address(!zero_for_one)));
                }
                let borrowed = safe_add_u256(amount_out, fee_out)?;
                if borrowed > liquidity_out.available_for_swap_out_borrow {
                    return Err(payout_exceeded(
                        borrowed,
                        liquidity_out.available_for_swap_out_borrow,
                    ));
                }
                Ok(false)
            }
        }
    }

    fn token_address(&self, is_token0: bool) -> Bytes {
        if is_token0 {
            self.token0.address.clone()
        } else {
            self.token1.address.clone()
        }
    }

    /// The pool after a swap. The payout lowers what the dex can pay out next.
    fn after_swap(
        &self,
        zero_for_one: bool,
        amount_in_raw: U256,
        outcome: &SwapOutcome,
        amount_out: U256,
        fee_out: U256,
    ) -> Result<FluidV2State, SimulationError> {
        let mut state = self.clone();
        state.dex_variables.current_tick = outcome.tick;
        state
            .dex_variables
            .current_sqrt_price_x96 = encode_sqrt_price(outcome.sqrt_price_x96);
        state.dex_variables2.active_liquidity = outcome.liquidity;

        if !self
            .dex_variables2
            .per_pool_accounting_stopped
        {
            let (reserve_in, reserve_out) = if zero_for_one {
                (&mut state.token0_reserve, &mut state.token1_reserve)
            } else {
                (&mut state.token1_reserve, &mut state.token0_reserve)
            };
            *reserve_in = safe_add_u256(*reserve_in, amount_in_raw)?;
            if *reserve_in >> MAX_AMOUNT_BITS != U256::ZERO {
                return Err(SimulationError::InvalidInput(
                    "Fluid v2 swap overflows the pool's token reserves".to_string(),
                    None,
                ));
            }
            *reserve_out = reserve_out
                .checked_sub(outcome.amount_out_raw)
                .ok_or_else(|| {
                    SimulationError::InvalidInput(
                        "Fluid v2 swap pays out more than the pool's token reserves".to_string(),
                        None,
                    )
                })?;
        }

        let liquidity_out =
            if zero_for_one { &mut state.token1_liquidity } else { &mut state.token0_liquidity };
        let paid = safe_add_u256(amount_out, fee_out)?;
        liquidity_out.float_balance = liquidity_out
            .float_balance
            .saturating_sub(amount_out);
        liquidity_out.available_for_swap_out_withdraw = liquidity_out
            .available_for_swap_out_withdraw
            .saturating_sub(amount_out);
        liquidity_out.available_for_swap_out_borrow = liquidity_out
            .available_for_swap_out_borrow
            .saturating_sub(paid);
        Ok(state)
    }

    /// Applies the pool attributes of `delta`: packed pool variables, reserves and ticks.
    fn apply_attributes(&mut self, delta: &ProtocolStateDelta) -> Result<(), TransitionError> {
        let attributes = &delta.updated_attributes;
        if let Some(value) = attributes.get("dex_variables") {
            self.dex_variables = DexVariables::from_packed(U256::from_be_slice(value));
        }
        if let Some(value) = attributes.get("dex_variables2") {
            self.dex_variables2 = DexVariables2::from_packed(U256::from_be_slice(value));
        }
        if let Some(value) = attributes.get("token0/token_reserves") {
            self.token0_reserve = U256::from_be_slice(value);
        }
        if let Some(value) = attributes.get("token1/token_reserves") {
            self.token1_reserve = U256::from_be_slice(value);
        }
        for (key, value) in attributes {
            let Some(tick) = parse_tick_key(key)? else {
                continue;
            };
            let net_liquidity =
                decode_net_liquidity(value).map_err(TransitionError::DecodeError)?;
            if net_liquidity == 0 {
                self.ticks.remove(&tick);
            } else {
                self.ticks.insert(tick, net_liquidity);
            }
        }
        for key in &delta.deleted_attributes {
            if let Some(tick) = parse_tick_key(key)? {
                self.ticks.remove(&tick);
            }
        }
        Ok(())
    }
}

impl DexVariables {
    pub fn from_packed(value: U256) -> Self {
        let sign_is_positive = value.bit(0);
        let abs_tick = bits(value, 1, 19).to::<i32>();
        Self {
            current_tick: if sign_is_positive { abs_tick } else { -abs_tick },
            current_sqrt_price_x96: bits(value, 20, 72),
        }
    }
}

impl DexVariables2 {
    pub fn from_packed(value: U256) -> Self {
        let decimals = |offset| match bits(value, offset, 4).to::<u8>() {
            // 4 bits cannot hold 18, so 15 stands for it.
            15 => 18,
            decimals => decimals,
        };
        Self {
            protocol_fee_0_to_1: bits(value, 0, 12).to(),
            protocol_fee_1_to_0: bits(value, 12, 12).to(),
            protocol_cut_fee: bits(value, 24, 6).to(),
            token0_decimals: decimals(30),
            token1_decimals: decimals(34),
            active_liquidity: bits(value, 38, 102).to(),
            per_pool_accounting_stopped: value.bit(140),
            fetch_dynamic_fee: value.bit(141),
            fee_version: bits(value, 152, 4).to(),
            lp_fee: bits(value, 156, 16).to(),
        }
    }
}

/// Converts between token amounts and the pool's raw-adjusted amounts: 9 decimals, divided by
/// the Liquidity exchange price.
struct Normalization {
    numerator: U256,
    denominator: U256,
    exchange_price: U256,
}

impl Normalization {
    fn new(decimals: u8, exchange_price: U256) -> Self {
        let (numerator, denominator) = if decimals > TOKENS_DECIMALS_PRECISION {
            (U256::from(1u64), ten_pow(decimals - TOKENS_DECIMALS_PRECISION))
        } else {
            (ten_pow(TOKENS_DECIMALS_PRECISION - decimals), U256::from(1u64))
        };
        Self { numerator, denominator, exchange_price }
    }

    /// The raw-adjusted amount credited for `amount` paid in, rounded down for the dex.
    fn to_raw_round_down(&self, amount: U256) -> Result<U256, SimulationError> {
        let raw = safe_div_u256(
            safe_mul_u256(
                safe_mul_u256(amount, U256::from(EXCHANGE_PRICES_PRECISION))?,
                self.numerator,
            )?,
            safe_mul_u256(self.exchange_price, self.denominator)?,
        )?;
        Ok(round_down(raw))
    }

    /// The token amount paid out for `raw`, rounded down for the dex.
    fn to_amount_round_down(&self, raw: U256) -> Result<U256, SimulationError> {
        let amount = safe_div_u256(
            safe_mul_u256(safe_mul_u256(round_down(raw), self.denominator)?, self.exchange_price)?,
            safe_mul_u256(self.numerator, U256::from(EXCHANGE_PRICES_PRECISION))?,
        )?;
        Ok(amount.saturating_sub(U256::from(1u64)))
    }

    /// The token amount whose raw-adjusted value is at least `raw`.
    fn to_amount_round_up(&self, raw: U256) -> Result<U256, SimulationError> {
        let amount = safe_mul_u256(safe_mul_u256(raw, self.exchange_price)?, self.denominator)?;
        let precision = safe_mul_u256(U256::from(EXCHANGE_PRICES_PRECISION), self.numerator)?;
        Ok(amount.div_ceil(precision))
    }
}

/// The dex's rounding idiom: scale by `(1e9 - 1) / 1e9`, then subtract one when non-zero.
fn round_down(amount: U256) -> U256 {
    let scaled =
        amount.saturating_mul(U256::from(ROUNDING_FACTOR - 1)) / U256::from(ROUNDING_FACTOR);
    scaled.saturating_sub(U256::from(1u64))
}

fn verify_amount(amount: U256) -> Result<(), SimulationError> {
    if amount < U256::from(MIN_AMOUNT) || amount >> MAX_AMOUNT_BITS != U256::ZERO {
        return Err(SimulationError::InvalidInput(
            format!("Fluid v2 amount {amount} is outside [1e4, 2^128 - 1]"),
            None,
        ));
    }
    Ok(())
}

fn verify_adjusted_amount(amount: U256) -> Result<(), SimulationError> {
    if amount < U256::from(MIN_AMOUNT) || amount >> MAX_ADJUSTED_AMOUNT_BITS != U256::ZERO {
        return Err(SimulationError::InvalidInput(
            format!("Fluid v2 normalized amount {amount} is outside [1e4, 2^86 - 1]"),
            None,
        ));
    }
    Ok(())
}

fn liquidity_rejects(leg: &str, token: &Bytes) -> SimulationError {
    SimulationError::RecoverableError(format!("Fluid Liquidity has {leg} of {token} paused"))
}

fn payout_exceeded(amount: U256, cap: U256) -> SimulationError {
    SimulationError::InvalidInput(
        format!("Fluid v2 payout {amount} exceeds what the dex can pay out ({cap})"),
        None,
    )
}

/// The stored sqrt price is rounded down. A price-increasing (1 to 0) swap rounds it up, so the
/// dex never quotes the swapper a better price than it holds.
fn decode_sqrt_price(packed: U256, zero_for_one: bool) -> U256 {
    let exponent = bits(packed, 0, SQRT_PRICE_EXPONENT_BITS);
    let mut coefficient = packed >> SQRT_PRICE_EXPONENT_BITS;
    if !exponent.is_zero() && !zero_for_one {
        coefficient += U256::from(1u64);
    }
    coefficient << exponent.to::<usize>()
}

/// `BigMathMinified.toBigNumber(sqrtPrice, 64, 8, ROUND_DOWN)`.
fn encode_sqrt_price(sqrt_price_x96: U256) -> U256 {
    let exponent = sqrt_price_x96
        .bit_len()
        .saturating_sub(SQRT_PRICE_COEFFICIENT_BITS);
    ((sqrt_price_x96 >> exponent) << SQRT_PRICE_EXPONENT_BITS) | U256::from(exponent)
}

fn bits(value: U256, offset: usize, width: usize) -> U256 {
    (value >> offset) & ((U256::from(1u64) << width) - U256::from(1u64))
}

fn ten_pow(exponent: u8) -> U256 {
    U256::from(10u64).pow(U256::from(exponent))
}

fn to_fluid_token(token: &Bytes) -> alloy::primitives::Address {
    if token.iter().all(|byte| *byte == 0) {
        alloy::primitives::Address::from(FLUID_NATIVE_TOKEN)
    } else {
        alloy::primitives::Address::from_slice(token)
    }
}

/// The tick of a `ticks/{tick}/net-liquidity` attribute key, or `None` for other keys.
pub(super) fn parse_tick_key(key: &str) -> Result<Option<i32>, TransitionError> {
    let Some(rest) = key.strip_prefix("ticks/") else {
        return Ok(None);
    };
    let tick = rest
        .split('/')
        .next()
        .and_then(|tick| tick.parse::<i32>().ok())
        .ok_or_else(|| {
            TransitionError::DecodeError(format!("Malformed Fluid v2 tick attribute {key}"))
        })?;
    Ok(Some(tick))
}

/// Net liquidity is written as a signed big-endian integer of at most 16 bytes.
pub(super) fn decode_net_liquidity(value: &Bytes) -> Result<i128, String> {
    if value.is_empty() {
        return Ok(0);
    }
    if value.len() > 16 {
        return Err(format!("Fluid v2 tick net liquidity {value} exceeds 16 bytes"));
    }
    let fill = if value[0] & 0x80 != 0 { 0xff } else { 0x00 };
    let mut word = [fill; 16];
    word[16 - value.len()..].copy_from_slice(value);
    Ok(i128::from_be_bytes(word))
}

#[typetag::serde]
impl ProtocolSim for FluidV2State {
    /// The fee of a token0 to token1 swap: the protocol fee, then the LP fee on the rest.
    fn fee(&self) -> f64 {
        let protocol_fee = self.dex_variables2.protocol_fee_0_to_1 as f64 / 1e6;
        let lp_fee = self.dex_variables2.lp_fee as f64 / 1e6;
        1.0 - (1.0 - protocol_fee) * (1.0 - lp_fee)
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let zero_for_one = self.zero_for_one(&base.address, &quote.address)?;
        let sqrt_price = u256_to_f64(decode_sqrt_price(
            self.dex_variables
                .current_sqrt_price_x96,
            zero_for_one,
        ))? / 2f64.powi(96);
        let price_0_in_1 = sqrt_price * sqrt_price;
        // Raw-adjusted amounts all carry 9 decimals, so only the exchange prices remain.
        let raw_price = if zero_for_one { price_0_in_1 } else { 1.0 / price_0_in_1 };
        let exchange_price_ratio = u256_to_f64(self.exchange_price(!zero_for_one)?)? /
            u256_to_f64(self.exchange_price(zero_for_one)?)?;
        let protocol_fee = if zero_for_one {
            self.dex_variables2.protocol_fee_0_to_1
        } else {
            self.dex_variables2.protocol_fee_1_to_0
        } as f64 /
            1e6;
        let lp_fee = self.dex_variables2.lp_fee as f64 / 1e6;
        let fee = 1.0 - (1.0 - protocol_fee) * (1.0 - lp_fee);
        Ok(add_fee_markup(raw_price * exchange_price_ratio, fee))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let zero_for_one = self.zero_for_one(&token_in.address, &token_out.address)?;
        let quote = self.quote(biguint_to_u256(&amount_in), zero_for_one)?;
        Ok(GetAmountOutResult::new(
            u256_to_biguint(quote.amount_out),
            BigUint::from(quote.gas),
            Box::new(quote.state),
        ))
    }

    /// The largest input whose swap the dex accepts, and its output.
    ///
    /// One swap may move the sqrt price at most 20%, so the input that reaches that price bounds
    /// the search; payout caps, reserves and amount bounds can lower it further.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let zero_for_one = self.zero_for_one(&sell_token, &buy_token)?;
        self.check_supported()?;
        let pool = self.swap_pool(zero_for_one);
        let price_limit = math::max_sqrt_price_move(pool.sqrt_price_x96, zero_for_one)?;
        let walk = math::swap_in(
            &pool,
            zero_for_one,
            U256::from(1u64) << MAX_ADJUSTED_AMOUNT_BITS,
            Some(price_limit),
        )?;
        let upper_bound: U256 =
            Normalization::new(self.decimals(zero_for_one), self.exchange_price(zero_for_one)?)
                .to_amount_round_up(walk.amount_in_raw)?;
        let upper_bound = upper_bound.min((U256::from(1u64) << MAX_AMOUNT_BITS) - U256::from(1u64));
        let delta: U256 = (upper_bound >> 20usize).max(U256::from(1u64));
        let (amount_in, quote) =
            find_max_valid_u256(upper_bound, delta, |amount| self.quote(amount, zero_for_one));
        let amount_out = quote.map_or(U256::ZERO, |quote| quote.amount_out);
        Ok((u256_to_biguint(amount_in), u256_to_biguint(amount_out)))
    }

    /// Applies the pool attributes, then reads both tokens' Liquidity state again: a delta also
    /// arrives when only the dex base state or Fluid Liquidity changed.
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.apply_attributes(&delta)?;
        let resolver = self.resolver.clone();
        let (token0_liquidity, token1_liquidity) =
            Self::fetch_token_liquidity(&resolver, &self.token0.address, &self.token1.address)
                .map_err(TransitionError::SimulationError)?;
        self.token0_liquidity = token0_liquidity;
        self.token1_liquidity = token1_liquidity;
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
            .downcast_ref::<Self>()
            .is_some_and(|other| self == other)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::models::Chain;

    use super::*;

    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const USDT: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
    const ETH: &str = "0x0000000000000000000000000000000000000000";
    const WBTC: &str = "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599";

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(20_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn word(hex: &str) -> U256 {
        U256::from_str(hex).unwrap()
    }

    fn liquidity(supply: u64, borrow: u64, withdraw: u128, borrow_cap: u128) -> TokenLiquidity {
        TokenLiquidity {
            supply_exchange_price: U256::from(supply),
            borrow_exchange_price: U256::from(borrow),
            float_balance: U256::ZERO,
            available_for_swap_out_withdraw: U256::from(withdraw),
            available_for_swap_out_borrow: U256::from(borrow_cap),
            supply_paused: false,
            withdraw_paused: false,
            borrow_paused: false,
            payback_paused: false,
        }
    }

    // getTokenBaseState of each token on the DEX v2 resolver at block 26331500.
    fn usdc_liquidity() -> TokenLiquidity {
        liquidity(1_171_773_205_330, 1_236_471_341_746, 2_598_889_586, 49_535_212_464)
    }

    fn usdt_liquidity() -> TokenLiquidity {
        liquidity(1_155_417_702_164, 1_221_235_462_704, 4_121_624_974, 49_412_138_012)
    }

    fn eth_liquidity() -> TokenLiquidity {
        liquidity(
            1_083_871_979_458,
            1_110_897_496_976,
            1_314_008_470_875_027_416,
            20_652_539_020_550_496_307,
        )
    }

    fn wbtc_liquidity() -> TokenLiquidity {
        liquidity(1_011_110_402_971, 1_028_121_493_647, 1_337_193, 61_112_725)
    }

    #[allow(clippy::too_many_arguments)]
    fn pool(
        dex_type: DexType,
        tokens: (Token, Token),
        tick_spacing: i32,
        dex_variables: &str,
        dex_variables2: &str,
        reserves: &str,
        ticks: &[(i32, i128)],
        liquidity: (TokenLiquidity, TokenLiquidity),
    ) -> FluidV2State {
        let reserves = word(reserves);
        FluidV2State::new(
            dex_type,
            tokens.0,
            tokens.1,
            Bytes::from_str("0x143c3927EaF0e50a59b1651C2Eb1e37FB62Aa2a9").unwrap(),
            tick_spacing,
            DexVariables::from_packed(word(dex_variables)),
            DexVariables2::from_packed(word(dex_variables2)),
            reserves & ((U256::from(1u64) << 128) - U256::from(1u64)),
            reserves >> 128,
            ticks.iter().copied().collect(),
            liquidity.0,
            liquidity.1,
        )
    }

    // Pool storage at block 26331500.
    fn d3_usdc_usdt() -> FluidV2State {
        pool(
            DexType::D3,
            (token(USDC, "USDC", 6), token(USDT, "USDT", 6)),
            1,
            "0x00000000000000000000210f1464370c3785fc60580f59913bfeaa2ed210012b",
            "0x0000000000000000000000064000000000000000015e38a1491fd89980000000",
            "0x00000000000000000000033e8eaa928700000000000000000000006a94814dfd",
            &[
                (-915, 8_941_697_813_393),
                (116, 2_296_191_335_129),
                (130, 1_531_347_477_443_537),
                (131, -2_296_191_335_129),
                (134, 2_901_227_474_466_653),
                (145, -2_901_227_474_466_653),
                (150, -1_531_347_477_443_537),
                (1091, -8_941_697_813_393),
            ],
            (usdc_liquidity(), usdt_liquidity()),
        )
    }

    fn d4_usdc_usdt() -> FluidV2State {
        pool(
            DexType::D4,
            (token(USDC, "USDC", 6), token(USDT, "USDT", 6)),
            1,
            "0x0000000000000000000000000000000000000000080c8e902f9d860da21000f5",
            "0x000000000000000000000006400000000000000000c8125a19ac4cd980000000",
            "0x00000000000000000000005f62d6b7f600000000000000000000006d562448f4",
            &[(113, 879_924_586_393_907), (133, -879_924_586_393_907)],
            (usdc_liquidity(), usdt_liquidity()),
        )
    }

    fn d3_usdc_eth() -> FluidV2State {
        pool(
            DexType::D3,
            (token(USDC, "USDC", 6), token(ETH, "ETH", 18)),
            10,
            "0x000000000000000000002b78cc3dce206a4b47e11b2e606235ecf8a481b2558c",
            "0x00000000000000000000001f400000000000000000005e7f8303897d80000000",
            "0x0000000000000000000000004842a8ad000000000000000000000102aae83c86",
            &[
                (-77_790, 416_505_943_288),
                (-77_430, 264_953_229_623),
                (-76_850, 942_005_700_598),
                (-76_390, -942_005_700_598),
                (-76_080, -264_953_229_623),
                (-75_780, -416_505_943_288),
            ],
            (usdc_liquidity(), eth_liquidity()),
        )
    }

    fn d3_wbtc_usdc() -> FluidV2State {
        pool(
            DexType::D3,
            (token(WBTC, "WBTC", 8), token(USDC, "USDC", 6)),
            10,
            "0xfff1df46994405694b05000000000000000000000801e1dc18df777b1293629f",
            "0x00000000000000000000001f4000000000000000000001cabac7da9a00000000",
            "0x000000000000000000000097266e495300000000000000000000000000c9cc2c",
            &[(109_210, 30_784_823_146), (113_260, -30_784_823_146)],
            (wbtc_liquidity(), usdc_liquidity()),
        )
    }

    fn amount_out(
        state: &FluidV2State,
        amount_in: u128,
        zero_for_one: bool,
    ) -> Result<u128, SimulationError> {
        let (token_in, token_out) = if zero_for_one {
            (&state.token0, &state.token1)
        } else {
            (&state.token1, &state.token0)
        };
        state
            .get_amount_out(BigUint::from(amount_in), token_in, token_out)
            .map(|result| result.amount.try_into().unwrap())
    }

    fn pool_named(name: &str) -> FluidV2State {
        match name {
            "d3_usdc_usdt" => d3_usdc_usdt(),
            "d4_usdc_usdt" => d4_usdc_usdt(),
            "d3_usdc_eth" => d3_usdc_eth(),
            "d3_wbtc_usdc" => d3_wbtc_usdc(),
            other => panic!("unknown pool {other}"),
        }
    }

    /// Every amount out was read from a swap through `FluidV2Executor` on a mainnet fork at block
    /// 26331500, the block the pool fixtures come from.
    #[rstest::rstest]
    #[case::d3_usdc_to_usdt_min("d3_usdc_usdt", true, 10_000, 10_006)]
    #[case::d3_usdc_to_usdt("d3_usdc_usdt", true, 7_777_777, 7_783_795)]
    #[case::d3_usdc_to_usdt_crossing_ticks("d3_usdc_usdt", true, 2_000_000_000, 2_000_269_022)]
    #[case::d3_usdt_to_usdc("d3_usdc_usdt", false, 12_345, 12_331)]
    #[case::d3_usdt_to_usdc_large("d3_usdc_usdt", false, 300_000_000, 294_210_295)]
    #[case::d4_usdc_to_usdt("d4_usdc_usdt", true, 1_000_000, 999_724)]
    #[case::d4_usdc_to_usdt_large("d4_usdc_usdt", true, 300_000_000, 299_834_786)]
    #[case::d4_usdt_to_usdc("d4_usdc_usdt", false, 7_777_777, 7_778_291)]
    #[case::d4_usdt_to_usdc_large("d4_usdc_usdt", false, 300_000_000, 299_938_811)]
    #[case::d3_usdc_to_eth("d3_usdc_eth", true, 1_000_000, 440_902_863_163_821)]
    #[case::d3_usdc_to_eth_large("d3_usdc_eth", true, 2_000_000_000, 860_991_947_364_004_597)]
    #[case::d3_eth_to_usdc("d3_usdc_eth", false, 123_456_789_012_345, 279_715)]
    #[case::d3_eth_to_usdc_large("d3_usdc_eth", false, 100_000_000_000_000_000, 225_988_315)]
    #[case::d3_wbtc_to_usdc_min("d3_wbtc_usdc", true, 10_000, 7_598_689)]
    #[case::d3_wbtc_to_usdc("d3_wbtc_usdc", true, 1_000_000, 702_667_607)]
    #[case::d3_usdc_to_wbtc("d3_wbtc_usdc", false, 7_777_777, 10_206)]
    #[case::d3_usdc_to_wbtc_large("d3_wbtc_usdc", false, 300_000_000, 381_687)]
    fn test_amount_out_matches_chain(
        #[case] pool: &str,
        #[case] zero_for_one: bool,
        #[case] amount_in: u128,
        #[case] expected: u128,
    ) {
        assert_eq!(amount_out(&pool_named(pool), amount_in, zero_for_one).unwrap(), expected);
    }

    /// Each of these swaps reverts on chain at block 26331500.
    #[rstest::rstest]
    #[case::amount_below_minimum("d3_usdc_usdt", false, 10_000)]
    #[case::normalized_amount_below_minimum("d3_usdc_eth", true, 12_345)]
    #[case::output_below_minimum("d3_wbtc_usdc", false, 1_000_000)]
    #[case::d3_past_last_tick("d3_usdc_usdt", false, 2_000_000_000)]
    #[case::d4_past_last_tick("d4_usdc_usdt", true, 2_000_000_000)]
    #[case::eth_past_last_tick("d3_usdc_eth", false, 1_000_000_000_000_000_000)]
    #[case::wbtc_past_last_tick("d3_wbtc_usdc", true, 10_000_000)]
    fn test_amount_out_rejects_swaps_the_dex_reverts(
        #[case] pool: &str,
        #[case] zero_for_one: bool,
        #[case] amount_in: u128,
    ) {
        assert!(amount_out(&pool_named(pool), amount_in, zero_for_one).is_err());
    }

    #[test]
    fn test_swap_within_range_keeps_active_liquidity() {
        let state = d3_usdc_usdt();

        let result = state
            .get_amount_out(BigUint::from(1_000_000u64), &state.token0, &state.token1)
            .unwrap();

        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<FluidV2State>()
            .unwrap();
        assert_eq!(
            new_state
                .dex_variables2
                .active_liquidity,
            state.dex_variables2.active_liquidity
        );
        assert_eq!(new_state.dex_variables.current_tick, state.dex_variables.current_tick);
        assert!(
            decode_sqrt_price(
                new_state
                    .dex_variables
                    .current_sqrt_price_x96,
                true
            ) < decode_sqrt_price(
                state
                    .dex_variables
                    .current_sqrt_price_x96,
                true
            )
        );
    }

    #[test]
    fn test_swap_lowers_what_the_dex_can_pay_out_next() {
        let state = d3_usdc_eth();

        let result = state
            .get_amount_out(BigUint::from(300_000_000u64), &state.token0, &state.token1)
            .unwrap();

        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<FluidV2State>()
            .unwrap();
        let paid = biguint_to_u256(&result.amount);
        assert_eq!(
            new_state
                .token1_liquidity
                .available_for_swap_out_withdraw,
            state
                .token1_liquidity
                .available_for_swap_out_withdraw -
                paid
        );
        assert!(new_state.token0_reserve > state.token0_reserve);
        assert!(new_state.token1_reserve < state.token1_reserve);
    }

    #[test]
    fn test_d3_payout_is_capped_by_liquidity_withdrawals() {
        let mut state = d3_usdc_eth();
        state
            .token1_liquidity
            .available_for_swap_out_withdraw = U256::from(400_000_000_000_000u64);

        // 1 USDC pays out 440_902_863_163_821 wei, more than the dex may withdraw.
        assert!(amount_out(&state, 1_000_000, true).is_err());
    }

    #[test]
    fn test_d3_paused_withdrawal_leaves_only_the_dex_balance() {
        let mut state = d3_usdc_eth();
        state.token1_liquidity.withdraw_paused = true;
        state.token1_liquidity.float_balance = U256::from(500_000_000_000_000u64);

        assert_eq!(amount_out(&state, 1_000_000, true).unwrap(), 440_902_863_163_821);
        assert!(amount_out(&state, 7_777_777, true).is_err());
    }

    #[test]
    fn test_d3_paused_supply_rejects_input_beyond_dex_balance() {
        let mut state = d3_usdc_usdt();
        state.token0_liquidity.supply_paused = true;

        assert!(amount_out(&state, 1_000_000, true).is_err());

        state.token0_liquidity.float_balance = U256::from(1_000_000u64);
        assert_eq!(amount_out(&state, 1_000_000, true).unwrap(), 1_000_776);
    }

    #[test]
    fn test_d4_borrow_cap_includes_the_swap_fee() {
        let mut state = d4_usdc_usdt();
        // 1 USDC pays out 999_724 USDT and borrows the fee on top.
        state
            .token1_liquidity
            .available_for_swap_out_borrow = U256::from(999_724u64);

        assert!(amount_out(&state, 1_000_000, true).is_err());

        state
            .token1_liquidity
            .available_for_swap_out_borrow = U256::from(1_000_000u64);
        assert_eq!(amount_out(&state, 1_000_000, true).unwrap(), 999_724);
    }

    #[rstest::rstest]
    #[case::borrow_paused(false, true)]
    #[case::payback_paused(true, false)]
    fn test_d4_paused_liquidity_rejects_swaps(
        #[case] payback_paused: bool,
        #[case] borrow_paused: bool,
    ) {
        let mut state = d4_usdc_usdt();
        state.token0_liquidity.payback_paused = payback_paused;
        state.token1_liquidity.borrow_paused = borrow_paused;

        assert!(amount_out(&state, 1_000_000, true).is_err());
    }

    #[test]
    fn test_unsupported_fees_are_rejected() {
        let mut state = d3_usdc_usdt();
        state.dex_variables2.fee_version = 1;
        assert!(amount_out(&state, 1_000_000, true).is_err());

        let mut state = d3_usdc_usdt();
        state.dex_variables2.fetch_dynamic_fee = true;
        assert!(amount_out(&state, 1_000_000, true).is_err());
    }

    #[rstest::rstest]
    #[case::d3_usdc_usdt_0_to_1("d3_usdc_usdt", true)]
    #[case::d3_usdc_usdt_1_to_0("d3_usdc_usdt", false)]
    #[case::d4_usdc_usdt_0_to_1("d4_usdc_usdt", true)]
    #[case::d3_usdc_eth_1_to_0("d3_usdc_eth", false)]
    #[case::d3_wbtc_usdc_0_to_1("d3_wbtc_usdc", true)]
    fn test_limits_are_the_largest_accepted_swap(#[case] pool: &str, #[case] zero_for_one: bool) {
        let state = pool_named(pool);
        let (sell, buy) = if zero_for_one {
            (&state.token0, &state.token1)
        } else {
            (&state.token1, &state.token0)
        };

        let (limit_in, limit_out) = state
            .get_limits(sell.address.clone(), buy.address.clone())
            .unwrap();

        let limit_in: u128 = limit_in.try_into().unwrap();
        assert_eq!(
            amount_out(&state, limit_in, zero_for_one).unwrap(),
            u128::try_from(limit_out).unwrap()
        );
        assert!(amount_out(&state, limit_in + limit_in / 100, zero_for_one).is_err());
    }

    #[test]
    fn test_spot_price_matches_small_swap() {
        let state = d3_usdc_eth();
        let amount_in = 1_000_000u128;

        let quoted = amount_out(&state, amount_in, true).unwrap() as f64 / 1e18;
        let spot = state
            .spot_price(&state.token0, &state.token1)
            .unwrap();

        // The spot price carries the fee as a markup; the swap takes it from the output.
        let without_fee = spot * (1.0 - state.fee()).powi(2);
        assert!((without_fee - quoted).abs() / quoted < 1e-4, "spot {spot} vs swap {quoted}");
    }

    #[test]
    fn test_apply_attributes_updates_and_removes_ticks() {
        let mut state = d4_usdc_usdt();
        let delta = ProtocolStateDelta {
            updated_attributes: HashMap::from([
                ("ticks/120/net-liquidity".to_string(), Bytes::from(vec![0xff, 0x38])),
                ("ticks/113/net-liquidity".to_string(), Bytes::from(vec![0x00])),
                ("token0/token_reserves".to_string(), Bytes::from(vec![0x01, 0x00])),
            ]),
            deleted_attributes: ["ticks/133/net-liquidity".to_string()].into(),
            ..Default::default()
        };

        state.apply_attributes(&delta).unwrap();

        assert_eq!(state.ticks, BTreeMap::from([(120, -200)]));
        assert_eq!(state.token0_reserve, U256::from(256u64));
    }

    #[test]
    fn test_decode_dex_variables() {
        let state = d3_wbtc_usdc();

        // Stored sign bit 0 means a negative tick.
        assert!(state.dex_variables.current_tick > 0);
        assert_eq!(state.dex_variables2.token0_decimals, 8);
        assert_eq!(state.dex_variables2.token1_decimals, 6);
        assert_eq!(state.dex_variables2.lp_fee, 500);
        assert_eq!(state.dex_variables2.fee_version, 0);
        assert!(
            !state
                .dex_variables2
                .per_pool_accounting_stopped
        );
    }
}
