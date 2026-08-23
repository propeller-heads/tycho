//! Port of `MarketMathCore.sol` — the AMM itself.
//!
//! Provenance and licensing: see `../NOTICE.md`.
//!
//! The curve runs in **asset units**, not SY units. `total_pt` is already in them; `total_sy` is
//! not, and `total_asset` is what the PY index converts it into. Quoting in SY units is the single
//! failure the brief calls out by name.
//!
//! Everything here depends on `block_time`, through three separate paths:
//!
//! - `rate_scalar = scalar_root * YEAR / time_to_expiry` — the curve flattens as expiry nears
//! - `rate_anchor`, recomputed from `last_ln_implied_rate` at the current `time_to_expiry`
//! - `fee_rate = exp(ln_fee_rate_root * time_to_expiry / YEAR)` — the fee decays to zero at expiry
//!
//! So a quote is only valid for the timestamp it was computed for, and the same state at two
//! timestamps must give two different answers. There is a test for exactly that.

use alloy::primitives::{I256, U256};

use super::{
    errors::{PendleError, PendleResult},
    log_exp_math,
    pmath::{self, i_one},
    sy_utils,
};

/// Seconds in the year the implied rate is quoted against.
const IMPLIED_RATE_TIME: u64 = 365 * 86_400;

/// The proportion cap: PT may not exceed 96% of the pool, measured in asset units.
pub fn max_market_proportion() -> I256 {
    i_one() * I256::try_from(96).unwrap() / I256::try_from(100).unwrap()
}

fn percentage_decimals() -> I256 {
    I256::try_from(100).unwrap()
}

/// The market's tradeable state, in the contract's own units.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MarketState {
    /// PT reserve, in asset units.
    pub total_pt: I256,
    /// SY reserve, in SY units.
    pub total_sy: I256,
    /// Immutable, set at creation.
    pub scalar_root: I256,
    pub expiry: u64,
    /// Fee configuration, resolved for the router that will execute.
    pub ln_fee_rate_root: U256,
    /// Base 100.
    pub reserve_fee_percent: U256,
    /// Updated by every trade; the input to the next trade's anchor.
    pub last_ln_implied_rate: U256,
}

/// The values a trade needs that are expensive enough to compute once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketPreCompute {
    pub rate_scalar: I256,
    pub total_asset: I256,
    pub rate_anchor: I256,
    pub fee_rate: I256,
}

/// What a trade moves, all in SY units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeResult {
    /// Positive when SY leaves the market to the trader.
    pub net_sy_to_account: I256,
    /// The whole fee, before the treasury's cut is split out.
    pub net_sy_fee: I256,
    /// The treasury's cut, which leaves the market alongside `net_sy_to_account`.
    pub net_sy_to_reserve: I256,
}

/// `blockTime >= expiry`, matching `MiniHelpers.isExpired`. Note the market is dead *at* expiry,
/// not after it.
pub fn is_expired(expiry: u64, block_time: u64) -> bool {
    block_time >= expiry
}

/// The contract's `getMarketPreCompute`.
pub fn get_market_pre_compute(
    market: &MarketState,
    index: U256,
    block_time: u64,
) -> PendleResult<MarketPreCompute> {
    if is_expired(market.expiry, block_time) {
        return Err(PendleError::MarketExpired { expiry: market.expiry, block_time });
    }
    let time_to_expiry = market.expiry - block_time;

    let rate_scalar = get_rate_scalar(market, time_to_expiry)?;
    let total_asset = sy_utils::sy_to_asset_i(index, market.total_sy)?;

    if market.total_pt.is_zero() || total_asset.is_zero() {
        return Err(PendleError::MarketZeroTotalPtOrTotalAsset {
            total_pt: market.total_pt.to_string(),
            total_asset: total_asset.to_string(),
        });
    }

    let rate_anchor = get_rate_anchor(
        market.total_pt,
        market.last_ln_implied_rate,
        total_asset,
        rate_scalar,
        time_to_expiry,
    )?;
    let fee_rate = exchange_rate_from_implied_rate(market.ln_fee_rate_root, time_to_expiry)?;

    Ok(MarketPreCompute { rate_scalar, total_asset, rate_anchor, fee_rate })
}

/// The contract's `calcTrade`.
///
/// `net_pt_to_account` is positive when PT leaves the market to the trader (SY→PT) and negative
/// when PT arrives (PT→SY). The fee is applied in **rate space**, not to the amount: buying PT
/// divides the pre-fee rate by `feeRate`, selling multiplies. The two branches are not mirror
/// images of each other, and the second one is raw multiplication and division rather than the
/// fixed-point helpers — transcribed as-is, because that is what the contract does.
pub fn calc_trade(
    market: &MarketState,
    comp: &MarketPreCompute,
    index: U256,
    net_pt_to_account: I256,
) -> PendleResult<TradeResult> {
    let pre_fee_exchange_rate = get_exchange_rate(
        market.total_pt,
        comp.total_asset,
        comp.rate_scalar,
        comp.rate_anchor,
        net_pt_to_account,
    )?;

    let pre_fee_asset_to_account = -pmath::div_down_i(net_pt_to_account, pre_fee_exchange_rate)?;
    let mut fee = comp.fee_rate;

    if net_pt_to_account.is_positive() {
        let post_fee_exchange_rate = pmath::div_down_i(pre_fee_exchange_rate, fee)?;
        if post_fee_exchange_rate < i_one() {
            return Err(PendleError::MarketExchangeRateBelowOne {
                rate: post_fee_exchange_rate.to_string(),
            });
        }
        fee = pmath::mul_down_i(pre_fee_asset_to_account, i_one() - fee)?;
    } else {
        let scaled = pre_fee_asset_to_account
            .checked_mul(i_one() - fee)
            .ok_or(PendleError::Overflow { operation: "calcTrade fee" })?;
        if fee.is_zero() {
            return Err(PendleError::DivisionByZero { operation: "calcTrade fee" });
        }
        fee = -(scaled / fee);
    }

    let reserve_fee_percent = pmath::to_i256(market.reserve_fee_percent)?;
    let net_asset_to_reserve = fee
        .checked_mul(reserve_fee_percent)
        .ok_or(PendleError::Overflow { operation: "calcTrade reserve" })? /
        percentage_decimals();
    let net_asset_to_account = pmath::sub_no_neg(pre_fee_asset_to_account, fee)
        .or_else(|_| Ok::<I256, PendleError>(pre_fee_asset_to_account - fee))?;

    // The trader's side rounds against them and the protocol's side rounds toward it, which is
    // why these three conversions do not all use the same helper.
    let net_sy_to_account = if net_asset_to_account.is_negative() {
        sy_utils::asset_to_sy_up_i(index, net_asset_to_account)?
    } else {
        sy_utils::asset_to_sy_i(index, net_asset_to_account)?
    };

    Ok(TradeResult {
        net_sy_to_account,
        net_sy_fee: sy_utils::asset_to_sy_i(index, fee)?,
        net_sy_to_reserve: sy_utils::asset_to_sy_i(index, net_asset_to_reserve)?,
    })
}

/// The contract's `executeTradeCore`, without the state write.
///
/// The two checks it adds over `calc_trade` are the ones a quote must not skip: the market is dead
/// at expiry, and it cannot hand out more PT than it holds.
pub fn execute_trade(
    market: &MarketState,
    index: U256,
    net_pt_to_account: I256,
    block_time: u64,
) -> PendleResult<TradeResult> {
    if is_expired(market.expiry, block_time) {
        return Err(PendleError::MarketExpired { expiry: market.expiry, block_time });
    }
    if market.total_pt <= net_pt_to_account {
        return Err(PendleError::MarketInsufficientPtForTrade {
            total_pt: market.total_pt.to_string(),
            required: net_pt_to_account.to_string(),
        });
    }
    let comp = get_market_pre_compute(market, index, block_time)?;
    calc_trade(market, &comp, index, net_pt_to_account)
}

fn get_rate_anchor(
    total_pt: I256,
    last_ln_implied_rate: U256,
    total_asset: I256,
    rate_scalar: I256,
    time_to_expiry: u64,
) -> PendleResult<I256> {
    let new_exchange_rate = exchange_rate_from_implied_rate(last_ln_implied_rate, time_to_expiry)?;
    if new_exchange_rate < i_one() {
        return Err(PendleError::MarketExchangeRateBelowOne { rate: new_exchange_rate.to_string() });
    }
    let proportion = pmath::div_down_i(total_pt, total_pt + total_asset)?;
    let ln_proportion = log_proportion(proportion)?;
    Ok(new_exchange_rate - pmath::div_down_i(ln_proportion, rate_scalar)?)
}

/// The implied rate the market would carry after a trade. Not on the quote path itself, but it is
/// what `last_ln_implied_rate` becomes, so it is here for the state transition and for tests.
pub fn get_ln_implied_rate(
    total_pt: I256,
    total_asset: I256,
    rate_scalar: I256,
    rate_anchor: I256,
    time_to_expiry: u64,
) -> PendleResult<U256> {
    let exchange_rate =
        get_exchange_rate(total_pt, total_asset, rate_scalar, rate_anchor, I256::ZERO)?;
    let ln_rate = pmath::to_u256(log_exp_math::ln(exchange_rate)?)?;
    let scaled = ln_rate
        .checked_mul(U256::from(IMPLIED_RATE_TIME))
        .ok_or(PendleError::Overflow { operation: "lnImpliedRate" })?;
    if time_to_expiry == 0 {
        return Err(PendleError::DivisionByZero { operation: "lnImpliedRate" });
    }
    Ok(scaled / U256::from(time_to_expiry))
}

/// `E = e^(r·t)`. Public because the router's estimator needs the same conversion.
pub fn exchange_rate_from_implied_rate(
    ln_implied_rate: U256,
    time_to_expiry: u64,
) -> PendleResult<I256> {
    let rt = ln_implied_rate
        .checked_mul(U256::from(time_to_expiry))
        .ok_or(PendleError::Overflow { operation: "exchangeRateFromImpliedRate" })? /
        U256::from(IMPLIED_RATE_TIME);
    log_exp_math::exp(pmath::to_i256(rt)?)
}

fn get_exchange_rate(
    total_pt: I256,
    total_asset: I256,
    rate_scalar: I256,
    rate_anchor: I256,
    net_pt_to_account: I256,
) -> PendleResult<I256> {
    let numerator = pmath::sub_no_neg(total_pt, net_pt_to_account)?;
    let proportion = pmath::div_down_i(numerator, total_pt + total_asset)?;

    if proportion > max_market_proportion() {
        return Err(PendleError::MarketProportionTooHigh {
            proportion: proportion.to_string(),
            max: max_market_proportion().to_string(),
        });
    }

    let ln_proportion = log_proportion(proportion)?;
    let exchange_rate = pmath::div_down_i(ln_proportion, rate_scalar)? + rate_anchor;

    if exchange_rate < i_one() {
        return Err(PendleError::MarketExchangeRateBelowOne { rate: exchange_rate.to_string() });
    }
    Ok(exchange_rate)
}

fn log_proportion(proportion: I256) -> PendleResult<I256> {
    if proportion == i_one() {
        return Err(PendleError::MarketProportionMustNotEqualOne);
    }
    let logit_p = pmath::div_down_i(proportion, i_one() - proportion)?;
    log_exp_math::ln(logit_p)
}

fn get_rate_scalar(market: &MarketState, time_to_expiry: u64) -> PendleResult<I256> {
    if time_to_expiry == 0 {
        return Err(PendleError::DivisionByZero { operation: "rateScalar" });
    }
    let scaled = market
        .scalar_root
        .checked_mul(I256::try_from(IMPLIED_RATE_TIME).unwrap())
        .ok_or(PendleError::Overflow { operation: "rateScalar" })?;
    let rate_scalar = scaled / I256::try_from(time_to_expiry).unwrap();
    if rate_scalar <= I256::ZERO {
        return Err(PendleError::MarketRateScalarBelowZero { rate_scalar: rate_scalar.to_string() });
    }
    Ok(rate_scalar)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct TradeCase {
        market: String,
        index: String,
        block_time: String,
        total_pt: String,
        total_sy: String,
        net_pt_to_account: String,
        rate_scalar: String,
        total_asset: String,
        rate_anchor: String,
        fee_rate: String,
        net_sy_to_account: String,
        net_sy_fee: String,
        net_sy_to_reserve: String,
    }

    #[derive(Deserialize)]
    struct TradeFixtures {
        trades: Vec<TradeCase>,
    }

    #[derive(Deserialize)]
    struct RevertCase {
        case: String,
        block_time: String,
        net_pt_to_account: String,
        error: String,
    }

    #[derive(Deserialize)]
    struct RevertFixtures {
        reverts: Vec<RevertCase>,
    }

    fn s(value: &str) -> I256 {
        I256::from_dec_str(value).expect("fixture holds a decimal integer")
    }

    fn u(value: &str) -> U256 {
        U256::from_str_radix(value, 10).expect("fixture holds a decimal integer")
    }

    /// The two markets the fixtures were generated from. `scalar_root` and the fee configuration
    /// are not in the per-row JSON because they are constant per market, so they are restated here
    /// and any drift shows up as a mismatch in `rate_scalar`.
    fn market_for(label: &str, case: &TradeCase) -> MarketState {
        let (scalar_root, last_ln_implied_rate) = match label {
            "wsteth" => ("86364560000000000000", "20211000000000000"),
            "reusd" => ("40000000000000000000", "50000000000000000"),
            other => panic!("unknown fixture market {other}"),
        };
        MarketState {
            total_pt: s(&case.total_pt),
            total_sy: s(&case.total_sy),
            scalar_root: s(scalar_root),
            expiry: 1_830_124_800,
            ln_fee_rate_root: u("499875041000000"),
            reserve_fee_percent: u("80"),
            last_ln_implied_rate: u(last_ln_implied_rate),
        }
    }

    fn wsteth_market() -> MarketState {
        MarketState {
            total_pt: s("83658000000000000000"),
            total_sy: s("1429719000000000000000"),
            scalar_root: s("86364560000000000000"),
            expiry: 1_830_124_800,
            ln_fee_rate_root: u("499875041000000"),
            reserve_fee_percent: u("80"),
            last_ln_implied_rate: u("20211000000000000"),
        }
    }

    const WSTETH_INDEX: &str = "1241884000000000000";

    /// Bit-equality against `MarketMathCore` itself, on every intermediate as well as the outputs.
    /// Forty cases: two markets on opposite sides of the decimal axis, four timestamps each, and
    /// trades in both directions.
    #[test]
    fn calc_trade_matches_the_contract() {
        let fixtures: TradeFixtures =
            serde_json::from_str(include_str!("../tests/fixtures/market.json")).unwrap();
        assert!(fixtures.trades.len() >= 40, "fixture grid shrank unexpectedly");

        for case in &fixtures.trades {
            let market = market_for(&case.market, case);
            let index = u(&case.index);
            let block_time: u64 = case.block_time.parse().unwrap();
            let label =
                format!("{} @ {} pt={}", case.market, case.block_time, case.net_pt_to_account);

            let comp = get_market_pre_compute(&market, index, block_time)
                .unwrap_or_else(|e| panic!("pre-compute failed for {label}: {e}"));
            assert_eq!(comp.rate_scalar, s(&case.rate_scalar), "rate_scalar for {label}");
            assert_eq!(comp.total_asset, s(&case.total_asset), "total_asset for {label}");
            assert_eq!(comp.rate_anchor, s(&case.rate_anchor), "rate_anchor for {label}");
            assert_eq!(comp.fee_rate, s(&case.fee_rate), "fee_rate for {label}");

            let trade = calc_trade(&market, &comp, index, s(&case.net_pt_to_account))
                .unwrap_or_else(|e| panic!("calc_trade failed for {label}: {e}"));
            assert_eq!(
                trade.net_sy_to_account,
                s(&case.net_sy_to_account),
                "net_sy_to_account for {label}"
            );
            assert_eq!(trade.net_sy_fee, s(&case.net_sy_fee), "net_sy_fee for {label}");
            assert_eq!(
                trade.net_sy_to_reserve,
                s(&case.net_sy_to_reserve),
                "net_sy_to_reserve for {label}"
            );
        }
    }

    /// The contract reverts on these, so the port must error — and with the matching reason, not
    /// merely some error.
    #[test]
    fn failures_match_the_contract() {
        let fixtures: RevertFixtures =
            serde_json::from_str(include_str!("../tests/fixtures/market_reverts.json")).unwrap();
        assert!(!fixtures.reverts.is_empty());

        for case in &fixtures.reverts {
            let market = wsteth_market();
            let block_time: u64 = case.block_time.parse().unwrap();
            let result =
                execute_trade(&market, u(WSTETH_INDEX), s(&case.net_pt_to_account), block_time);
            let error = result
                .expect_err(&format!("{} should have failed", case.case))
                .clone();
            let matched = matches!(
                (case.error.as_str(), &error),
                ("MarketExpired", PendleError::MarketExpired { .. }) |
                    (
                        "MarketInsufficientPtForTrade",
                        PendleError::MarketInsufficientPtForTrade { .. }
                    ) |
                    ("MarketProportionTooHigh", PendleError::MarketProportionTooHigh { .. }) |
                    (
                        "MarketExchangeRateBelowOne",
                        PendleError::MarketExchangeRateBelowOne { .. }
                    )
            );
            assert!(matched, "{}: contract gave {}, port gave {error}", case.case, case.error);
        }
    }

    /// The brief's central point about this AMM: the same state quoted at two timestamps must give
    /// two different answers, because the scalar, the anchor and the fee all move with time.
    #[test]
    fn the_same_state_quotes_differently_at_two_timestamps() {
        let market = wsteth_market();
        let index = u(WSTETH_INDEX);
        let pt_out = s("1000000000000000000");

        let early = execute_trade(&market, index, pt_out, 1_700_000_000).unwrap();
        let late = execute_trade(&market, index, pt_out, 1_820_000_000).unwrap();

        assert_ne!(
            early.net_sy_to_account, late.net_sy_to_account,
            "a time-independent quote would return the same amount at both timestamps"
        );
        // Closer to expiry, PT is worth more, so buying it costs more SY.
        assert!(late.net_sy_to_account < early.net_sy_to_account);
    }

    /// The fee decays to nothing as expiry approaches: `feeRate = exp(lnFeeRateRoot·t/YEAR)` tends
    /// to `exp(0) = 1`, which is a multiplier of one.
    #[test]
    fn the_fee_decays_toward_expiry() {
        let market = wsteth_market();
        let index = u(WSTETH_INDEX);

        let early = get_market_pre_compute(&market, index, 1_700_000_000).unwrap();
        let late = get_market_pre_compute(&market, index, 1_830_124_700).unwrap();

        assert!(early.fee_rate > late.fee_rate);
        assert!(late.fee_rate >= i_one(), "the fee multiplier never falls below one");

        // The fee is `exp(lnFeeRateRoot · t / YEAR)`, so the excess over one shrinks in proportion
        // to the time left. A hundred seconds from expiry against four years out is a ratio of
        // roughly a million; asserting three orders of magnitude keeps the test about the decay
        // rather than about a hard-coded constant.
        let early_excess = early.fee_rate - i_one();
        let late_excess = late.fee_rate - i_one();
        assert!(
            late_excess * s("1000") < early_excess,
            "fee excess barely moved: {early_excess} -> {late_excess}"
        );
    }

    /// Expiry is inclusive: the market is dead *at* `expiry`, not one second later.
    #[test]
    fn the_market_dies_at_expiry_not_after_it() {
        let market = wsteth_market();
        let index = u(WSTETH_INDEX);
        let pt_out = s("1000000000000000000");

        assert!(execute_trade(&market, index, pt_out, market.expiry - 1).is_ok());
        assert!(matches!(
            execute_trade(&market, index, pt_out, market.expiry),
            Err(PendleError::MarketExpired { .. })
        ));
        assert!(matches!(
            execute_trade(&market, index, pt_out, market.expiry + 1),
            Err(PendleError::MarketExpired { .. })
        ));
    }
}
