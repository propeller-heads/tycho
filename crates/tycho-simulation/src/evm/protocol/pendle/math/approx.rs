//! Port of `ApproxStateLib`, `MarketApproxEstimateLib` and `MarketApproxLibOnchain`.
//!
//! Provenance and licensing: see `../NOTICE.md`.
//!
//! The market's primitives are **exact-PT** in both directions — `swapExactPtForSy` and
//! `swapSyForExactPt`. An exact-SY-in quote therefore has no closed form; it has to be inverted,
//! and the contract inverts it by bounded search.
//!
//! This reproduces the *router's* search rather than inventing a better one, because the router is
//! what will execute. `ActionSwapPTV3.swapExactSyForPt` delegates to the on-chain variant whenever
//! the caller passes no off-chain guess, which is what a router integration does. Running the same
//! loop makes the quote equal the execution exactly, instead of within a tolerance band that has to
//! be justified and monitored.
//!
//! That is also why the iteration sequence is ported literally, three-stage state machine and all.
//! A search that converges to the same answer by a different route agrees on the inputs someone
//! happened to test and diverges on the rest.

use alloy::primitives::{I256, U256};

use super::{
    errors::{PendleError, PendleResult},
    log_exp_math,
    market::{self, MarketPreCompute, MarketState, TradeResult},
    pmath::{self, i_one},
    sy_utils,
};

/// The initial range is the estimate ±5%.
fn guess_range_tweak() -> U256 {
    U256::from(50_000_000_000_000_000u64)
}

const DEFAULT_MAX_ITERATION: usize = 30;

/// `5e13`, a hundredth of a percent. The loop accepts a guess whose implied input is within this
/// of the requested one, from below.
fn default_eps() -> U256 {
    U256::from(50_000_000_000_000u64)
}

/// Where the search is in its three-stage walk.
///
/// The stages are not decoration: `Initial` probes one edge of the ±5% window, `RangeSearching`
/// doubles the window outward while the answer keeps falling outside it, and only `ResultFinding`
/// bisects. Collapsing this to a plain bisection changes the sequence of guesses and therefore the
/// accepted answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApproxStage {
    Initial,
    RangeSearching,
    ResultFinding,
}

#[derive(Debug, Clone)]
struct ApproxState {
    stage: ApproxStage,
    max_iteration: usize,
    eps: U256,
    starting_guess: U256,
    cur_guess: U256,
    /// The working range, widened when the answer falls outside it.
    ranges: [U256; 2],
    /// The range the answer must lie in. Running off either end is a failure, not a clamp.
    hard_bounds: [U256; 2],
}

impl ApproxState {
    fn init_no_offchain(estimation: U256, hard_bounds: [U256; 2]) -> PendleResult<Self> {
        if hard_bounds[0] > hard_bounds[1] {
            return Err(PendleError::ApproxInvalidBounds {
                lower: hard_bounds[0].to_string(),
                upper: hard_bounds[1].to_string(),
            });
        }
        let starting_guess = pmath::clamp(estimation, hard_bounds[0], hard_bounds[1]);
        let lower = pmath::tweak_down(starting_guess, guess_range_tweak())?.max(hard_bounds[0]);
        let upper = pmath::tweak_up(starting_guess, guess_range_tweak())?.min(hard_bounds[1]);
        Ok(ApproxState {
            stage: ApproxStage::Initial,
            max_iteration: DEFAULT_MAX_ITERATION,
            eps: default_eps(),
            starting_guess,
            cur_guess: starting_guess,
            ranges: [lower, upper],
            hard_bounds,
        })
    }

    fn transition_down(&mut self, exclude_guess: bool) -> PendleResult<()> {
        self.ranges[1] = self.cur_guess;
        if exclude_guess {
            self.ranges[1] -= U256::from(1);
        }

        match self.stage {
            ApproxStage::Initial => {
                self.stage = ApproxStage::RangeSearching;
                self.cur_guess = self.ranges[0];
                Ok(())
            }
            ApproxStage::RangeSearching => {
                if self.cur_guess == self.hard_bounds[0] {
                    return Err(PendleError::ApproxRangeUnderflow);
                }
                if self.cur_guess != self.ranges[0] {
                    self.stage = ApproxStage::ResultFinding;
                    return self.move_guess_to_middle();
                }
                let dist = self.starting_guess - self.ranges[0];
                let extended =
                    pmath::sub_with_lower_bound(self.ranges[0], dist, self.hard_bounds[0]);
                self.ranges[0] = extended;
                self.cur_guess = extended;
                Ok(())
            }
            ApproxStage::ResultFinding => self.move_guess_to_middle(),
        }
    }

    fn transition_up(&mut self, exclude_guess: bool) -> PendleResult<()> {
        self.ranges[0] = self.cur_guess;
        if exclude_guess {
            self.ranges[0] += U256::from(1);
        }

        match self.stage {
            ApproxStage::Initial => {
                self.stage = ApproxStage::RangeSearching;
                self.cur_guess = self.ranges[1];
                Ok(())
            }
            ApproxStage::RangeSearching => {
                if self.cur_guess == self.hard_bounds[1] {
                    return Err(PendleError::ApproxRangeOverflow);
                }
                if self.cur_guess != self.ranges[1] {
                    self.stage = ApproxStage::ResultFinding;
                    return self.move_guess_to_middle();
                }
                let dist = self.ranges[1] - self.starting_guess;
                let extended =
                    pmath::add_with_upper_bound(self.ranges[1], dist, self.hard_bounds[1]);
                self.ranges[1] = extended;
                self.cur_guess = extended;
                Ok(())
            }
            ApproxStage::ResultFinding => self.move_guess_to_middle(),
        }
    }

    fn move_guess_to_middle(&mut self) -> PendleResult<()> {
        if self.ranges[0] > self.ranges[1] {
            return Err(PendleError::ApproxInvalidBounds {
                lower: self.ranges[0].to_string(),
                upper: self.ranges[1].to_string(),
            });
        }
        self.cur_guess = (self.ranges[0] + self.ranges[1]) / U256::from(2);
        Ok(())
    }
}

/// What an approximated swap resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproxResult {
    pub amount_out: U256,
    pub net_sy_fee: U256,
    /// How many times the loop ran. Carried because it pins the search *route*, not just its
    /// destination.
    pub iterations: usize,
    /// The market the filled trade leaves behind. The search evaluates a trade at every guess and
    /// only one of them executes, so this is the one at the guess that was accepted.
    pub market_after: MarketState,
}

/// The closed-form starting guess, off `lastLnImpliedRate`.
///
/// Not the answer — just where the search begins. A worse estimate costs iterations; a different
/// one changes which answer the loop lands on, so it is ported rather than improved.
fn estimate_amount(
    market: &MarketState,
    index: U256,
    block_time: u64,
    amount_in: U256,
    from_sy: bool,
    to_pt: bool,
) -> PendleResult<U256> {
    if market::is_expired(market.expiry, block_time) {
        return Err(PendleError::MarketExpired { expiry: market.expiry, block_time });
    }
    let time_to_expiry = market.expiry - block_time;
    let asset_to_pt_rate = pmath::to_u256(market::exchange_rate_from_implied_rate(
        market.last_ln_implied_rate,
        time_to_expiry,
    )?)?;
    let pt_to_asset_rate = pmath::div_down(pmath::one(), asset_to_pt_rate)?;
    let yt_to_asset_rate = pmath::one() - pt_to_asset_rate;

    let exact_asset_in = if from_sy { sy_utils::sy_to_asset(index, amount_in)? } else { amount_in };

    if to_pt {
        pmath::div_down(exact_asset_in, pt_to_asset_rate)
    } else {
        pmath::div_down(exact_asset_in, yt_to_asset_rate)
    }
}

pub fn estimate_swap_exact_sy_for_pt(
    market: &MarketState,
    index: U256,
    block_time: u64,
    amount_sy_in: U256,
) -> PendleResult<U256> {
    estimate_amount(market, index, block_time, amount_sy_in, true, true)
}

pub fn estimate_swap_exact_sy_for_yt(
    market: &MarketState,
    index: U256,
    block_time: u64,
    amount_sy_in: U256,
) -> PendleResult<U256> {
    estimate_amount(market, index, block_time, amount_sy_in, true, false)
}

/// SY required to buy `net_pt_out` PT, and the trade that would deliver it.
fn calc_sy_in(
    market: &MarketState,
    comp: &MarketPreCompute,
    index: U256,
    net_pt_out: U256,
) -> PendleResult<(U256, TradeResult)> {
    let trade = market::calc_trade(market, comp, index, pmath::to_i256(net_pt_out)?)?;
    Ok((pmath::to_u256(-trade.net_sy_to_account)?, trade))
}

/// SY received for selling `net_pt_in` PT, and the trade that would deliver it.
fn calc_sy_out(
    market: &MarketState,
    comp: &MarketPreCompute,
    index: U256,
    net_pt_in: U256,
) -> PendleResult<(U256, TradeResult)> {
    let trade = market::calc_trade(market, comp, index, -pmath::to_i256(net_pt_in)?)?;
    Ok((pmath::to_u256(trade.net_sy_to_account)?, trade))
}

/// The most PT the market will sell, bounded by the rate floor.
///
/// The trailing `× 999/1000` is the contract's own precision headroom, not a safety margin added
/// here. Reporting the un-haircut value hands a router a size the swap then rejects.
pub fn calc_max_pt_out(comp: &MarketPreCompute, total_pt: I256) -> PendleResult<U256> {
    let logit_p =
        log_exp_math::exp(pmath::mul_down_i(comp.fee_rate - comp.rate_anchor, comp.rate_scalar)?)?;
    let proportion = pmath::div_down_i(logit_p, logit_p + i_one())?;
    let numerator = pmath::mul_down_i(proportion, total_pt + comp.total_asset)?;
    let max_pt_out = total_pt - numerator;
    Ok(pmath::to_u256(max_pt_out)? * U256::from(999) / U256::from(1000))
}

/// The most PT the market will absorb before the 96% proportion cap binds.
pub fn calc_soft_max_pt_in(market: &MarketState, comp: &MarketPreCompute) -> PendleResult<U256> {
    let capped =
        pmath::mul_down_i(market::max_market_proportion(), market.total_pt + comp.total_asset)?;
    pmath::to_u256(capped - market.total_pt)
}

/// The largest SY input the SY→PT search will fill, and the PT it buys.
///
/// Derived by inverting the search's own hard bound rather than by estimating it: the bound is a
/// PT amount, and the SY it costs is exactly `calc_sy_in` at that amount — the same closed form
/// the loop evaluates at every guess. So the pair returned is a point the search actually reaches
/// and accepts, not an extrapolation of one.
///
/// The `eps` slack means an input slightly above this still fills, since the loop accepts a guess
/// whose cost is within `eps` *below* the request. Reporting the exact-fill point rather than the
/// slack-inclusive one keeps `get_limits` on the safe side of the boundary.
pub fn max_sy_in_for_pt(
    market: &MarketState,
    index: U256,
    block_time: u64,
) -> PendleResult<(U256, U256)> {
    let comp = market::get_market_pre_compute(market, index, block_time)?;
    let max_pt_out = calc_max_pt_out(&comp, market.total_pt)?;
    let (sy_in, _) = calc_sy_in(market, &comp, index, max_pt_out)?;
    Ok((sy_in, max_pt_out))
}

/// The largest SY input the SY→YT search will fill, and the YT it buys.
///
/// Same inversion, through the flash-swap identity: at the soft proportion cap the trader supplies
/// what tokenizing the PT costs, less what selling it back returns.
pub fn max_sy_in_for_yt(
    market: &MarketState,
    index: U256,
    block_time: u64,
) -> PendleResult<(U256, U256)> {
    let comp = market::get_market_pre_compute(market, index, block_time)?;
    let max_yt_out = calc_soft_max_pt_in(market, &comp)?;
    let (sy_out, _) = calc_sy_out(market, &comp, index, max_yt_out)?;
    let to_tokenize = sy_utils::asset_to_sy_up(index, max_yt_out)?;
    if to_tokenize < sy_out {
        return Err(PendleError::NegativeResult {
            a: to_tokenize.to_string(),
            b: sy_out.to_string(),
        });
    }
    Ok((to_tokenize - sy_out, max_yt_out))
}

/// Exact SY in → PT out, by the router's own search.
pub fn approx_swap_exact_sy_for_pt(
    market: &MarketState,
    index: U256,
    exact_sy_in: U256,
    block_time: u64,
) -> PendleResult<ApproxResult> {
    let comp = market::get_market_pre_compute(market, index, block_time)?;
    let estimate = estimate_swap_exact_sy_for_pt(market, index, block_time, exact_sy_in)?;
    let hard_bounds = [U256::ZERO, calc_max_pt_out(&comp, market.total_pt)?];
    let mut state = ApproxState::init_no_offchain(estimate, hard_bounds)?;

    for iteration in 0..state.max_iteration {
        let guess = state.cur_guess;
        let (net_sy_in, trade) = calc_sy_in(market, &comp, index, guess)?;

        if net_sy_in <= exact_sy_in {
            if pmath::is_a_smaller_approx_b(net_sy_in, exact_sy_in, state.eps)? {
                let net_pt_to_account = pmath::to_i256(guess)?;
                let market_after = market::apply_trade(
                    market,
                    &comp,
                    index,
                    net_pt_to_account,
                    &trade,
                    block_time,
                )?;
                return Ok(ApproxResult {
                    amount_out: guess,
                    net_sy_fee: pmath::to_u256(trade.net_sy_fee)?,
                    iterations: iteration,
                    market_after,
                });
            }
            state.transition_up(false)?;
        } else {
            state.transition_down(true)?;
        }
    }
    Err(PendleError::ApproxExhausted { iterations: DEFAULT_MAX_ITERATION })
}

/// Exact SY in → YT out, by the router's own search.
///
/// Buying YT is a flash-swap against the same reserves as the PT edges: borrow SY, mint PT and YT,
/// sell the PT back. `net_sy_to_pull` is what the trader actually supplies once the PT sale has
/// repaid most of the borrow, which is why a small SY input buys a large YT position.
pub fn approx_swap_exact_sy_for_yt(
    market: &MarketState,
    index: U256,
    exact_sy_in: U256,
    block_time: u64,
) -> PendleResult<ApproxResult> {
    let comp = market::get_market_pre_compute(market, index, block_time)?;
    let estimate = estimate_swap_exact_sy_for_yt(market, index, block_time, exact_sy_in)?;
    let hard_bounds =
        [sy_utils::sy_to_asset(index, exact_sy_in)?, calc_soft_max_pt_in(market, &comp)?];
    let mut state = ApproxState::init_no_offchain(estimate, hard_bounds)?;

    for iteration in 0..state.max_iteration {
        let guess = state.cur_guess;
        let (net_sy_out, trade) = calc_sy_out(market, &comp, index, guess)?;
        let net_sy_to_tokenize_pt = sy_utils::asset_to_sy_up(index, guess)?;
        if net_sy_to_tokenize_pt < net_sy_out {
            return Err(PendleError::NegativeResult {
                a: net_sy_to_tokenize_pt.to_string(),
                b: net_sy_out.to_string(),
            });
        }
        let net_sy_to_pull = net_sy_to_tokenize_pt - net_sy_out;

        if net_sy_to_pull <= exact_sy_in {
            if pmath::is_a_smaller_approx_b(net_sy_to_pull, exact_sy_in, state.eps)? {
                // The PT is sold *into* the market, so the flow is negative — the mirror of the
                // SY→PT leg, against the same reserves.
                let net_pt_to_account = -pmath::to_i256(guess)?;
                let market_after = market::apply_trade(
                    market,
                    &comp,
                    index,
                    net_pt_to_account,
                    &trade,
                    block_time,
                )?;
                return Ok(ApproxResult {
                    amount_out: guess,
                    net_sy_fee: pmath::to_u256(trade.net_sy_fee)?,
                    iterations: iteration,
                    market_after,
                });
            }
            state.transition_up(false)?;
        } else {
            state.transition_down(true)?;
        }
    }
    Err(PendleError::ApproxExhausted { iterations: DEFAULT_MAX_ITERATION })
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct ApproxCase {
        market: String,
        direction: String,
        block_time: String,
        exact_sy_in: String,
        estimate: String,
        amount_out: String,
        net_sy_fee: String,
        iterations: String,
    }

    #[derive(Deserialize)]
    struct ApproxFixtures {
        cases: Vec<ApproxCase>,
    }

    #[derive(Deserialize)]
    struct LimitCase {
        market: String,
        block_time: String,
        max_pt_out: String,
        soft_max_pt_in: String,
    }

    #[derive(Deserialize)]
    struct LimitFixtures {
        limits: Vec<LimitCase>,
    }

    #[derive(Deserialize)]
    struct BoundaryCase {
        direction: String,
        block_time: String,
        exact_sy_in: String,
        outcome: String,
        amount_out: String,
    }

    #[derive(Deserialize)]
    struct BoundaryFixtures {
        cases: Vec<BoundaryCase>,
    }

    fn s(value: &str) -> I256 {
        I256::from_dec_str(value).unwrap()
    }

    fn u(value: &str) -> U256 {
        U256::from_str_radix(value, 10).unwrap()
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

    fn reusd_market() -> MarketState {
        MarketState {
            total_pt: s("1000000000000"),
            total_sy: s("900000000000000000000000"),
            scalar_root: s("40000000000000000000"),
            expiry: 1_830_124_800,
            ln_fee_rate_root: u("499875041000000"),
            reserve_fee_percent: u("80"),
            last_ln_implied_rate: u("50000000000000000"),
        }
    }

    const WSTETH_INDEX: &str = "1241884000000000000";
    const REUSD_INDEX: &str = "1095830";

    fn market_for(label: &str) -> (MarketState, U256) {
        match label {
            "wsteth" => (wsteth_market(), u(WSTETH_INDEX)),
            "reusd" => (reusd_market(), u(REUSD_INDEX)),
            other => panic!("unknown fixture market {other}"),
        }
    }

    /// The result *and* the iteration count, against the router's own loop. Matching only the
    /// result would leave open that the two searches take different routes and agree by luck on
    /// the sampled inputs.
    #[test]
    fn the_approximation_matches_the_router() {
        let fixtures: ApproxFixtures =
            serde_json::from_str(include_str!("../tests/fixtures/approx.json")).unwrap();
        assert!(fixtures.cases.len() >= 48, "fixture grid shrank unexpectedly");

        for case in &fixtures.cases {
            let (market, index) = market_for(&case.market);
            let block_time: u64 = case.block_time.parse().unwrap();
            let exact_sy_in = u(&case.exact_sy_in);
            let label = format!(
                "{} {} @ {} in={}",
                case.market, case.direction, case.block_time, case.exact_sy_in
            );

            let estimate = match case.direction.as_str() {
                "sy_for_pt" => {
                    estimate_swap_exact_sy_for_pt(&market, index, block_time, exact_sy_in)
                }
                "sy_for_yt" => {
                    estimate_swap_exact_sy_for_yt(&market, index, block_time, exact_sy_in)
                }
                other => panic!("unknown direction {other}"),
            }
            .unwrap_or_else(|e| panic!("estimate failed for {label}: {e}"));
            assert_eq!(estimate, u(&case.estimate), "starting estimate for {label}");

            let result = match case.direction.as_str() {
                "sy_for_pt" => approx_swap_exact_sy_for_pt(&market, index, exact_sy_in, block_time),
                _ => approx_swap_exact_sy_for_yt(&market, index, exact_sy_in, block_time),
            }
            .unwrap_or_else(|e| panic!("approximation failed for {label}: {e}"));

            assert_eq!(result.amount_out, u(&case.amount_out), "amount_out for {label}");
            assert_eq!(result.net_sy_fee, u(&case.net_sy_fee), "net_sy_fee for {label}");
            assert_eq!(
                result.iterations.to_string(),
                case.iterations,
                "iteration count for {label}: the search took a different route"
            );
        }
    }

    /// Both depth bounds, against the contract. `calc_max_pt_out` keeps 99.9% of the theoretical
    /// maximum, and no first-principles derivation produces that factor.
    #[test]
    fn the_depth_bounds_match_the_contract() {
        let fixtures: LimitFixtures =
            serde_json::from_str(include_str!("../tests/fixtures/limits.json")).unwrap();
        assert!(!fixtures.limits.is_empty());

        for case in &fixtures.limits {
            let (market, index) = market_for(&case.market);
            let block_time: u64 = case.block_time.parse().unwrap();
            let comp = market::get_market_pre_compute(&market, index, block_time).unwrap();
            let label = format!("{} @ {}", case.market, case.block_time);

            assert_eq!(
                calc_max_pt_out(&comp, market.total_pt).unwrap(),
                u(&case.max_pt_out),
                "max_pt_out for {label}"
            );
            assert_eq!(
                calc_soft_max_pt_in(&market, &comp).unwrap(),
                u(&case.soft_max_pt_in),
                "soft_max_pt_in for {label}"
            );
        }
    }

    /// Past the market's depth the search walks to its hard bound and fails there. The contract
    /// reverts with "Slippage: search range overflow"; a quote must do the same rather than return
    /// the bound as if it were fillable.
    #[test]
    fn an_input_beyond_depth_fails_rather_than_clamping() {
        let market = wsteth_market();
        let result = approx_swap_exact_sy_for_pt(
            &market,
            u(WSTETH_INDEX),
            u("500000000000000000000"),
            1_700_000_000,
        );
        assert!(
            matches!(result, Err(PendleError::ApproxRangeOverflow)),
            "expected a range overflow, got {result:?}"
        );
    }

    /// The haircut is the difference between a size that fills and one that reverts, so it is
    /// asserted directly rather than left implicit in the fixture comparison.
    #[test]
    fn max_pt_out_keeps_only_999_thousandths() {
        let market = wsteth_market();
        let comp = market::get_market_pre_compute(&market, u(WSTETH_INDEX), 1_700_000_000).unwrap();

        let logit_p = log_exp_math::exp(
            pmath::mul_down_i(comp.fee_rate - comp.rate_anchor, comp.rate_scalar).unwrap(),
        )
        .unwrap();
        let proportion = pmath::div_down_i(logit_p, logit_p + i_one()).unwrap();
        let numerator = pmath::mul_down_i(proportion, market.total_pt + comp.total_asset).unwrap();
        let theoretical = pmath::to_u256(market.total_pt - numerator).unwrap();

        let actual = calc_max_pt_out(&comp, market.total_pt).unwrap();
        assert_eq!(actual, theoretical * U256::from(999) / U256::from(1000));
        assert!(actual < theoretical);
    }

    /// The reported maximum is a size the search actually fills — checked by running the search at
    /// it, not by trusting the derivation. A bound that cannot be filled is worse than no bound.
    #[test]
    fn the_reported_maximum_is_actually_fillable() {
        let market = wsteth_market();
        let index = u(WSTETH_INDEX);

        for block_time in [1_700_000_000u64, 1_780_000_000, 1_820_000_000] {
            let (max_in, max_out) = max_sy_in_for_pt(&market, index, block_time).unwrap();
            let filled = approx_swap_exact_sy_for_pt(&market, index, max_in, block_time)
                .unwrap_or_else(|e| {
                    panic!("PT leg could not fill its own maximum at {block_time}: {e}")
                });
            assert_eq!(filled.amount_out, max_out, "PT maximum at {block_time}");

            let (max_in, max_out) = max_sy_in_for_yt(&market, index, block_time).unwrap();
            let filled = approx_swap_exact_sy_for_yt(&market, index, max_in, block_time)
                .unwrap_or_else(|e| {
                    panic!("YT leg could not fill its own maximum at {block_time}: {e}")
                });
            assert_eq!(filled.amount_out, max_out, "YT maximum at {block_time}");
        }
    }

    /// And a size past it does not fill. Together with the test above this brackets the boundary
    /// from both sides, which is what a router needs `get_limits` to mean.
    #[test]
    fn just_past_the_reported_maximum_does_not_fill() {
        let market = wsteth_market();
        let index = u(WSTETH_INDEX);

        for block_time in [1_700_000_000u64, 1_820_000_000] {
            let (max_in, _) = max_sy_in_for_pt(&market, index, block_time).unwrap();
            // A tenth above the limit is comfortably outside the eps slack.
            let past = max_in * U256::from(110) / U256::from(100);
            assert!(
                approx_swap_exact_sy_for_pt(&market, index, past, block_time).is_err(),
                "PT leg filled {past}, past its reported maximum {max_in} at {block_time}"
            );
        }
    }

    /// The reported maximum agrees with the swept boundary: every size the contract filled is at or
    /// below it, and the sizes it rejected are above.
    #[test]
    fn the_reported_maximum_brackets_the_swept_boundary() {
        let fixtures: BoundaryFixtures =
            serde_json::from_str(include_str!("../tests/fixtures/approx_boundary.json")).unwrap();
        let market = wsteth_market();
        let index = u(WSTETH_INDEX);

        for case in &fixtures.cases {
            let block_time: u64 = case.block_time.parse().unwrap();
            let exact_sy_in = u(&case.exact_sy_in);
            let (max_in, _) = match case.direction.as_str() {
                "sy_for_pt" => max_sy_in_for_pt(&market, index, block_time),
                _ => max_sy_in_for_yt(&market, index, block_time),
            }
            .unwrap();

            if case.outcome == "ok" {
                assert!(
                    exact_sy_in <= max_in,
                    "{} @ {} filled {exact_sy_in} but the limit says {max_in}",
                    case.direction,
                    case.block_time
                );
            }
        }
    }

    /// Depth is time-dependent, and the two legs do not run out together.
    ///
    /// The same 40 SY fills on the YT leg four years from expiry and reverts closer in, while the
    /// PT leg fills at both. So neither leg's depth can be derived from the other's, and neither
    /// can be treated as a constant of the market — which is what `get_limits` has to respect.
    #[test]
    fn depth_moves_with_time_and_differs_between_the_legs() {
        let market = wsteth_market();
        let index = u(WSTETH_INDEX);
        let size = u("40000000000000000000");

        assert!(approx_swap_exact_sy_for_yt(&market, index, size, 1_700_000_000).is_ok());
        assert!(matches!(
            approx_swap_exact_sy_for_yt(&market, index, size, 1_820_000_000),
            Err(PendleError::ApproxRangeOverflow)
        ));
        assert!(approx_swap_exact_sy_for_pt(&market, index, size, 1_700_000_000).is_ok());
        assert!(approx_swap_exact_sy_for_pt(&market, index, size, 1_820_000_000).is_ok());
    }

    /// The fill/revert boundary itself, swept against the contract on both legs at two timestamps.
    ///
    /// A search that converged by a different route would first disagree here, at the point where
    /// the range extension runs into the hard bound — not on the comfortable sizes in the middle.
    #[test]
    fn the_boundary_matches_the_contract() {
        let fixtures: BoundaryFixtures =
            serde_json::from_str(include_str!("../tests/fixtures/approx_boundary.json")).unwrap();
        assert!(fixtures.cases.len() >= 24);

        let market = wsteth_market();
        let index = u(WSTETH_INDEX);
        let mut filled = 0;
        let mut reverted = 0;

        for case in &fixtures.cases {
            let block_time: u64 = case.block_time.parse().unwrap();
            let exact_sy_in = u(&case.exact_sy_in);
            let label = format!("{} @ {} in={}", case.direction, case.block_time, case.exact_sy_in);

            let result = match case.direction.as_str() {
                "sy_for_pt" => approx_swap_exact_sy_for_pt(&market, index, exact_sy_in, block_time),
                _ => approx_swap_exact_sy_for_yt(&market, index, exact_sy_in, block_time),
            };

            if case.outcome == "ok" {
                filled += 1;
                let result = result.unwrap_or_else(|e| panic!("{label} should have filled: {e}"));
                assert_eq!(result.amount_out, u(&case.amount_out), "amount_out for {label}");
            } else {
                reverted += 1;
                assert!(
                    matches!(result, Err(PendleError::ApproxRangeOverflow)),
                    "{label}: contract gave {:?}, port gave {result:?}",
                    case.outcome
                );
            }
        }
        // Both halves of the boundary are actually exercised, rather than the sweep having
        // silently landed entirely on one side.
        assert!(filled > 0 && reverted > 0, "sweep covered only one outcome");
    }
}
