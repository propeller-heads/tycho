use alloy::primitives::U256;
use num_bigint::{BigInt, BigUint};
use num_traits::{One, Signed, ToPrimitive, Zero};
use tycho_common::simulation::errors::SimulationError;

use super::state::{BaselineCurve, BaselineQuoteState};
use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint};

const DEFAULT_GAS: u64 = 300_000;
const BTOKEN_DECIMALS: u8 = 18;
const PPM_DENOMINATOR: u64 = 1_000_000;
const SOLVER_SAFETY_MARGIN: &str = "60000000000000000000";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaselineQuoteResult {
    pub amount_out: BigUint,
    pub gas: BigUint,
    pub state: BaselineQuoteState,
}

pub fn quote_buy_exact_in(
    state: &BaselineQuoteState,
    reserves_in: &BigUint,
) -> Result<BaselineQuoteResult, SimulationError> {
    if reserves_in.is_zero() {
        return Err(invalid_input("invalid amount in"));
    }

    let mut state = state.clone();
    let (tokens_out, _fee, accounting_fee, reserve_delta) =
        solve_buy(&state, &BigInt::from(reserves_in.clone()))?;
    if tokens_out <= BigInt::zero() {
        return Err(no_rate());
    }

    apply_quote_state(&mut state, &tokens_out, &reserve_delta, &accounting_fee);
    Ok(BaselineQuoteResult {
        amount_out: positive_bigint_to_biguint(&tokens_out)?,
        gas: BigUint::from(DEFAULT_GAS),
        state,
    })
}

pub fn quote_sell_exact_in(
    state: &BaselineQuoteState,
    tokens_in: &BigUint,
) -> Result<BaselineQuoteResult, SimulationError> {
    if tokens_in.is_zero() {
        return Err(invalid_input("invalid amount in"));
    }

    let mut state = state.clone();
    let delta_circ = -BigInt::from(tokens_in.clone());
    let (reserve_delta, fee) = quote_swap(&state, &delta_circ)?;
    if reserve_delta <= BigInt::zero() {
        return Err(no_rate());
    }

    apply_quote_state(&mut state, &delta_circ, &reserve_delta, &fee);
    Ok(BaselineQuoteResult {
        amount_out: positive_bigint_to_biguint(&reserve_delta)?,
        gas: BigUint::from(DEFAULT_GAS),
        state,
    })
}

pub(super) fn spot_price(state: &BaselineQuoteState, is_buy: bool) -> Result<f64, SimulationError> {
    let price = compute_active_price(&state.snapshot_curve)?;
    let fee_adjustment = u_to_bi(state.snapshot_curve.swap_fee) * 2u8;
    let price = if is_buy {
        mul_wad(&price, &(wad() + fee_adjustment))
    } else {
        mul_wad(&price, &(wad() - fee_adjustment))
    };
    if price <= BigInt::zero() {
        return Err(invalid_curve_state());
    }

    let price = price
        .to_f64()
        .ok_or_else(|| fatal("active price overflows f64"))?
        / 1e18;
    if is_buy {
        Ok(price)
    } else {
        Ok(1.0 / price)
    }
}

pub(super) fn get_limits(
    state: &BaselineQuoteState,
    is_buy: bool,
) -> Result<(BigUint, BigUint), SimulationError> {
    if is_buy {
        let max_input = positive_bigint_to_biguint(&(u_to_bi(state.total_reserves) / 100u8))?;
        if max_input.is_zero() {
            return Err(no_rate());
        }
        let quote = quote_buy_exact_in(state, &max_input)?;
        return Ok((max_input, quote.amount_out));
    }

    let max_input = u256_to_biguint(state.max_sell_delta);
    if max_input.is_zero() {
        return Err(no_rate());
    }
    let quote = quote_sell_exact_in(state, &max_input)?;
    Ok((max_input, quote.amount_out))
}

fn solve_buy(
    state: &BaselineQuoteState,
    target: &BigInt,
) -> Result<(BigInt, BigInt, BigInt, BigInt), SimulationError> {
    let p = &state.snapshot_curve;
    let price = compute_active_price(p)?;
    let price_with_fee = mul_wad(&price, &(wad() + (u_to_bi(p.swap_fee) * 2u8)));
    if price_with_fee.is_zero() {
        return Err(invalid_curve_state());
    }

    let estimated_delta_wad =
        div_wad(&normalize_wad(target, reserve_decimals(state)?), &price_with_fee)? * 2u8;
    let estimated_delta = denormalize_wad(&estimated_delta_wad, BTOKEN_DECIMALS);
    let total_b_token_max_delta = (u_to_bi(state.total_b_tokens) * 99u8) / 100u8;
    let max_delta = match convexity_safe_max_buy(p)? {
        Some(convexity_max_delta) if convexity_max_delta < total_b_token_max_delta => {
            convexity_max_delta
        }
        _ => total_b_token_max_delta,
    };
    if max_delta <= BigInt::zero() {
        return Err(solver_failed());
    }

    let mut hi =
        if estimated_delta < BigInt::from(2u8) { BigInt::from(2u8) } else { estimated_delta };
    if hi > max_delta {
        hi = max_delta.clone();
    }

    let mut high_cost = quote_buy_exact_out_cost(state, &hi);
    while matches!(&high_cost, Ok((cost, _)) if cost <= target) && hi < max_delta {
        hi *= 2u8;
        if hi > max_delta {
            hi = max_delta.clone();
        }
        high_cost = quote_buy_exact_out_cost(state, &hi);
    }

    let min_delta = min_price_moving_delta(p)?;
    let mut lo = if min_delta > BigInt::one() { min_delta } else { BigInt::one() };
    if lo > max_delta {
        return Err(solver_failed());
    }
    let mut delta = lo.clone();
    let tolerance = solver_tolerance(target);
    while (&hi - &lo) > BigInt::one() {
        let mid = (&lo + &hi) / 2u8;
        match quote_buy_exact_out_cost(state, &mid) {
            Ok((cost, _)) if cost <= *target => {
                lo = mid.clone();
                delta = mid;
                if target - cost <= tolerance {
                    break;
                }
            }
            _ => hi = mid,
        }
    }

    let (cost, quote_fee) = quote_buy_exact_out_cost(state, &delta)?;
    if cost.is_zero() || cost > *target {
        return Err(solver_failed());
    }
    let fee = &quote_fee + (target - &cost);
    Ok((delta, fee, quote_fee, -cost))
}

fn quote_buy_exact_out_cost(
    state: &BaselineQuoteState,
    tokens_out: &BigInt,
) -> Result<(BigInt, BigInt), SimulationError> {
    let (reserve_delta, fee) = quote_swap(state, tokens_out)?;
    Ok((reserve_delta.abs(), fee))
}

fn solver_tolerance(target: &BigInt) -> BigInt {
    let tolerance = target / PPM_DENOMINATOR;
    if tolerance > BigInt::one() {
        tolerance
    } else {
        BigInt::one()
    }
}

fn min_price_moving_delta(params: &BaselineCurve) -> Result<BigInt, SimulationError> {
    let supply_circ = mul_wad(&u_to_bi(params.supply), &u_to_bi(params.circ));
    let convexity_supply = mul_wad(&u_to_bi(params.convexity_exp), &u_to_bi(params.total_supply));
    let k_factor = mul_wad(&(u_to_bi(params.convexity_exp) - wad()), &u_to_bi(params.supply))
        + mul_wad(&(u_to_bi(params.convexity_exp) + wad()), &u_to_bi(params.circ));
    let blv_value = mul_wad(&u_to_bi(params.blv), &u_to_bi(params.circ));

    if u_to_bi(params.reserves) <= blv_value || convexity_supply.is_zero() || k_factor.is_zero() {
        return Ok(BigInt::zero());
    }

    let buffer = u_to_bi(params.reserves) - blv_value;
    let denominator = mul_wad(&buffer, &convexity_supply);
    if denominator.is_zero() {
        return Ok(BigInt::zero());
    }

    Ok(full_mul_div(&supply_circ, &supply_circ, &denominator)? / k_factor)
}

fn convexity_safe_max_buy(params: &BaselineCurve) -> Result<Option<BigInt>, SimulationError> {
    if u_to_bi(params.convexity_exp) < wad() || params.supply.is_zero() {
        return Ok(None);
    }

    let exp_arg = (&bi(SOLVER_SAFETY_MARGIN) * wad()) / u_to_bi(params.convexity_exp);
    let r = exp_wad(&exp_arg)?;
    let denominator = &r + wad();

    let supply_term = full_mul_div(&r, &u_to_bi(params.supply), &denominator)?;
    let circ_term = full_mul_div(&wad(), &u_to_bi(params.circ), &denominator)?;
    let max_buy_wad = zero_floor_sub(&supply_term, &circ_term);

    Ok(Some(denormalize_wad(&max_buy_wad, BTOKEN_DECIMALS)))
}

fn quote_swap(
    state: &BaselineQuoteState,
    delta_circ_native: &BigInt,
) -> Result<(BigInt, BigInt), SimulationError> {
    if delta_circ_native < &BigInt::zero()
        && delta_circ_native.abs() > u_to_bi(state.max_sell_delta)
    {
        return Err(trade_exceeds_limit());
    }

    let (lower, upper) = if delta_circ_native > &BigInt::zero() {
        let lower = u_to_bi(state.quote_block_buy_delta_circ);
        let upper = &lower + delta_circ_native;
        (lower, upper)
    } else {
        let lower = -u_to_bi(state.quote_block_sell_delta_circ);
        let upper = -(u_to_bi(state.quote_block_sell_delta_circ) + delta_circ_native.abs());
        (lower, upper)
    };

    let (before_user, before_inv) = quote_cumulative_from_snapshot(state, &lower)?;
    let (after_user, after_inv) = quote_cumulative_from_snapshot(state, &upper)?;

    let mut delta_user_wad = after_user - before_user;
    let delta_invariant_wad = after_inv - before_inv;
    if delta_user_wad > delta_invariant_wad {
        delta_user_wad = delta_invariant_wad.clone();
    }
    delta_user_wad = apply_sell_floor(state, &lower, &upper, delta_user_wad, &delta_invariant_wad)?;

    if delta_user_wad < BigInt::zero() {
        let user_pay = denormalize_wad_up(&delta_user_wad.abs(), reserve_decimals(state)?);
        let curve_need = denormalize_wad_up(&delta_invariant_wad.abs(), reserve_decimals(state)?);
        return Ok((-user_pay.clone(), user_pay - curve_need));
    }

    let user_receive = denormalize_wad(&delta_user_wad, reserve_decimals(state)?);
    let curve_release = denormalize_wad(&delta_invariant_wad, reserve_decimals(state)?);
    Ok((user_receive.clone(), curve_release - user_receive))
}

fn apply_sell_floor(
    state: &BaselineQuoteState,
    before_delta_circ: &BigInt,
    after_delta_circ: &BigInt,
    delta_user_wad: BigInt,
    delta_invariant_wad: &BigInt,
) -> Result<BigInt, SimulationError> {
    if after_delta_circ >= before_delta_circ {
        return Ok(delta_user_wad);
    }

    let sell_amount_wad = normalize_wad(&(before_delta_circ - after_delta_circ), BTOKEN_DECIMALS);
    let floor_receipt_wad = mul_wad(&u_to_bi(state.snapshot_curve.blv), &sell_amount_wad);
    if delta_user_wad >= floor_receipt_wad {
        return Ok(delta_user_wad);
    }

    let floor_receipt_native = denormalize_wad(&floor_receipt_wad, reserve_decimals(state)?);
    let curve_release_native = if delta_invariant_wad > &BigInt::zero() {
        denormalize_wad(delta_invariant_wad, reserve_decimals(state)?)
    } else {
        BigInt::zero()
    };
    if floor_receipt_native > curve_release_native {
        return Err(trade_exceeds_limit());
    }

    Ok(floor_receipt_wad)
}

fn quote_cumulative_from_snapshot(
    state: &BaselineQuoteState,
    cumulative_delta_circ_native: &BigInt,
) -> Result<(BigInt, BigInt), SimulationError> {
    if cumulative_delta_circ_native.is_zero() {
        return Ok((BigInt::zero(), BigInt::zero()));
    }
    let delta_circ_wad = to_wad_signed(cumulative_delta_circ_native, BTOKEN_DECIMALS);
    let (user_delta, _, invariant_delta) = compute_swap(&state.snapshot_curve, &delta_circ_wad)?;
    Ok((user_delta, invariant_delta))
}

fn compute_swap(
    params: &BaselineCurve,
    delta_circ: &BigInt,
) -> Result<(BigInt, BigInt, BigInt), SimulationError> {
    let circ = u_to_bi(params.circ);
    if circ.is_zero() {
        return compute_zero_circ_swap(params, delta_circ);
    }

    let c1 = &circ + delta_circ;
    if c1 < BigInt::zero() {
        return Err(trade_exceeds_limit());
    }
    if c1.is_zero() {
        let blv_value = mul_wad(&u_to_bi(params.blv), &circ);
        return Ok((
            blv_value.clone(),
            u_to_bi(params.reserves) - &blv_value,
            u_to_bi(params.reserves),
        ));
    }

    let x1 = u_to_bi(params.supply) - delta_circ;
    if x1 <= BigInt::zero() {
        return Err(trade_exceeds_limit());
    }

    let new_buffer = if x1 >= c1 {
        let ratio = div_wad(&x1, &c1)?;
        check_pow_limit(&ratio, &u_to_bi(params.convexity_exp))?;
        let ratio_pow_n = pow_wad(&ratio, &u_to_bi(params.convexity_exp))?;
        full_mul_div_up(&u_to_bi(params.last_invariant), &wad(), &ratio_pow_n)?
    } else {
        let inv_ratio =
            if delta_circ < &BigInt::zero() { div_wad(&c1, &x1)? } else { div_wad_up(&c1, &x1)? };
        check_pow_limit(&inv_ratio, &u_to_bi(params.convexity_exp))?;
        let inv_ratio_pow_n = pow_wad(&inv_ratio, &u_to_bi(params.convexity_exp))?;
        full_mul_div_up(&u_to_bi(params.last_invariant), &inv_ratio_pow_n, &wad())?
    };

    let price_before = compute_active_price(params)?;
    let price_after_denominator = mul_wad(&x1, &c1);
    if price_after_denominator.is_zero() {
        return Err(invalid_curve_state());
    }
    let price_after = u_to_bi(params.blv)
        + full_mul_div(
            &new_buffer,
            &mul_wad(&u_to_bi(params.convexity_exp), &u_to_bi(params.total_supply)),
            &price_after_denominator,
        )?;
    if price_after == price_before {
        return Err(fatal("price must change"));
    }

    let new_reserves = &new_buffer + mul_wad_up(&u_to_bi(params.blv), &c1);
    let invariant_delta = u_to_bi(params.reserves) - new_reserves;
    let fee = compute_fee(params, delta_circ, &new_buffer, &invariant_delta)?;
    let user_delta = &invariant_delta - &fee;
    Ok((user_delta, fee, invariant_delta))
}

fn compute_zero_circ_swap(
    params: &BaselineCurve,
    delta_circ: &BigInt,
) -> Result<(BigInt, BigInt, BigInt), SimulationError> {
    if delta_circ <= &BigInt::zero() {
        return Err(trade_exceeds_limit());
    }

    let x1 = u_to_bi(params.supply) - delta_circ;
    if x1 <= BigInt::zero() {
        return Err(trade_exceeds_limit());
    }

    let new_buffer = if delta_circ >= &x1 {
        let ratio = div_wad_up(delta_circ, &x1)?;
        check_pow_limit(&ratio, &u_to_bi(params.convexity_exp))?;
        let ratio_pow_n = pow_wad(&ratio, &u_to_bi(params.convexity_exp))?;
        full_mul_div_up(&u_to_bi(params.last_invariant), &ratio_pow_n, &wad())?
    } else {
        let inv_ratio = div_wad(&x1, delta_circ)?;
        check_pow_limit(&inv_ratio, &u_to_bi(params.convexity_exp))?;
        let inv_ratio_pow_n = pow_wad(&inv_ratio, &u_to_bi(params.convexity_exp))?;
        full_mul_div_up(&u_to_bi(params.last_invariant), &wad(), &inv_ratio_pow_n)?
    };

    let invariant_delta =
        u_to_bi(params.reserves) - (&new_buffer + mul_wad_up(&u_to_bi(params.blv), delta_circ));
    let buffer_reserves_denominator = mul_wad(delta_circ, &x1);
    if buffer_reserves_denominator.is_zero() {
        return Err(invalid_curve_state());
    }
    let buffer_reserves = full_mul_div_up(
        &new_buffer,
        &mul_wad_up(&u_to_bi(params.convexity_exp), &u_to_bi(params.total_supply)),
        &buffer_reserves_denominator,
    )?;
    let payment = mul_wad_up(
        delta_circ,
        &(mul_wad_up(&u_to_bi(params.blv), &(wad() + (u_to_bi(params.swap_fee) * 2u8)))
            + buffer_reserves),
    );
    Ok((-payment.clone(), invariant_delta.clone() + payment, invariant_delta))
}

fn compute_fee(
    params: &BaselineCurve,
    delta_circ: &BigInt,
    new_buffer: &BigInt,
    invariant_delta: &BigInt,
) -> Result<BigInt, SimulationError> {
    let abs_delta = delta_circ.abs();
    let c1 = u_to_bi(params.circ) + delta_circ;
    let x1 = u_to_bi(params.supply) - delta_circ;

    if delta_circ > &BigInt::zero() {
        let marginal_premium = full_mul_div_up(
            new_buffer,
            &mul_wad_up(&u_to_bi(params.convexity_exp), &u_to_bi(params.total_supply)),
            &mul_wad(&c1, &x1),
        )?;
        let marginal_cost = mul_wad_up(
            &abs_delta,
            &(mul_wad_up(&u_to_bi(params.blv), &(wad() + (u_to_bi(params.swap_fee) * 2u8)))
                + marginal_premium),
        );
        return Ok(zero_floor_sub(&marginal_cost, &invariant_delta.abs()));
    }

    let marginal_premium = full_mul_div(
        new_buffer,
        &mul_wad(&u_to_bi(params.convexity_exp), &u_to_bi(params.total_supply)),
        &mul_wad_up(&c1, &x1),
    )?;
    let marginal_receipt = mul_wad(
        &abs_delta,
        &(u_to_bi(params.blv)
            + mul_wad(&marginal_premium, &(wad() - (u_to_bi(params.swap_fee) * 2u8)))),
    );
    Ok(zero_floor_sub(invariant_delta, &marginal_receipt))
}

fn compute_active_price(params: &BaselineCurve) -> Result<BigInt, SimulationError> {
    if u_to_bi(params.circ).is_zero() {
        return Ok(u_to_bi(params.blv));
    }
    let buffer = u_to_bi(params.reserves) - mul_wad(&u_to_bi(params.blv), &u_to_bi(params.circ));
    if buffer < BigInt::zero() {
        return Err(invalid_curve_state());
    }
    let premium_denominator = mul_wad(&u_to_bi(params.supply), &u_to_bi(params.circ));
    if premium_denominator.is_zero() {
        return Err(invalid_curve_state());
    }
    let premium = full_mul_div(
        &buffer,
        &mul_wad(&u_to_bi(params.convexity_exp), &u_to_bi(params.total_supply)),
        &premium_denominator,
    )?;
    Ok(u_to_bi(params.blv) + premium)
}

fn apply_quote_state(
    state: &mut BaselineQuoteState,
    delta_circ: &BigInt,
    reserve_delta: &BigInt,
    fee: &BigInt,
) {
    settle_pending_surplus(state);

    let next_total_b_tokens = u_to_bi(state.total_b_tokens) - delta_circ;
    state.total_b_tokens = bi_to_u(&next_total_b_tokens);
    state.total_reserves = bi_to_u(&(u_to_bi(state.total_reserves) - reserve_delta - fee));
    record_pending_liquidity_fee(state, &next_total_b_tokens, fee);

    if delta_circ > &BigInt::zero() {
        state.quote_block_buy_delta_circ =
            bi_to_u(&(u_to_bi(state.quote_block_buy_delta_circ) + delta_circ));
    } else if delta_circ < &BigInt::zero() {
        state.quote_block_sell_delta_circ =
            bi_to_u(&(u_to_bi(state.quote_block_sell_delta_circ) + delta_circ.abs()));
    }
    state.max_sell_delta = bi_to_u(&(u_to_bi(state.max_sell_delta) - delta_circ.abs()));
}

fn settle_pending_surplus(state: &mut BaselineQuoteState) {
    if !state.should_settle_pending_surplus || state.pending_surplus.is_zero() {
        state.should_settle_pending_surplus = false;
        return;
    }

    let buffer_threshold = mul_wad(&u_to_bi(state.total_supply), &bi("950000000000000000"));
    if u_to_bi(state.total_b_tokens) < buffer_threshold {
        state.total_reserves =
            bi_to_u(&(u_to_bi(state.total_reserves) + u_to_bi(state.pending_surplus)));
    }
    state.pending_surplus = U256::ZERO;
    state.should_settle_pending_surplus = false;
}

fn record_pending_liquidity_fee(
    state: &mut BaselineQuoteState,
    next_total_b_tokens: &BigInt,
    fee: &BigInt,
) {
    if fee <= &BigInt::zero() {
        return;
    }
    let buffer_threshold = mul_wad(&u_to_bi(state.total_supply), &bi("950000000000000000"));
    if next_total_b_tokens >= &buffer_threshold {
        return;
    }
    let liquidity_fee = mul_wad(fee, &u_to_bi(state.liquidity_fee_pct));
    if liquidity_fee > BigInt::zero() {
        state.pending_surplus = bi_to_u(&(u_to_bi(state.pending_surplus) + liquidity_fee));
    }
}

fn pow_wad(x: &BigInt, y: &BigInt) -> Result<BigInt, SimulationError> {
    if x <= &BigInt::zero() {
        return Err(invalid_curve_state());
    }

    let ln_x = ln_wad(x)?;
    let exponent = sdiv(&(ln_x * y), &wad());
    let res = exp_wad(&exponent)?;
    if res.is_zero() {
        return Err(trade_exceeds_limit());
    }
    Ok(res)
}

fn ln_wad(x: &BigInt) -> Result<BigInt, SimulationError> {
    if x <= &BigInt::zero() || x.bits() > 256 {
        return Err(invalid_curve_state());
    }

    let r = 256i64 - x.bits() as i64;
    let mut x96 = x << r as usize;
    x96 >>= 159usize;

    let mut p = bi("43456485725739037958740375743393")
        + sar(
            &((bi("24828157081833163892658089445524")
                + sar(&((bi("3273285459638523848632254066296") + &x96) * &x96), 96))
                * &x96),
            96,
        );
    p = sar(&(p * &x96), 96) - bi("11111509109440967052023855526967");
    p = sar(&(p * &x96), 96) - bi("45023709667254063763336534515857");
    p = sar(&(p * &x96), 96) - bi("14706773417378608786704636184526");
    p = p * &x96 - (bi("795164235651350426258249787498") << 96usize);

    let mut q = bi("5573035233440673466300451813936") + &x96;
    q = bi("71694874799317883764090561454958") + sar(&(&x96 * &q), 96);
    q = bi("283447036172924575727196451306956") + sar(&(&x96 * &q), 96);
    q = bi("401686690394027663651624208769553") + sar(&(&x96 * &q), 96);
    q = bi("204048457590392012362485061816622") + sar(&(&x96 * &q), 96);
    q = bi("31853899698501571402653359427138") + sar(&(&x96 * &q), 96);
    q = bi("909429971244387300277376558375") + sar(&(&x96 * &q), 96);

    p = sdiv(&p, &q);
    p = bi("1677202110996718588342820967067443963516166") * p;
    p = bi("16597577552685614221487285958193947469193820559219878177908093499208371")
        * BigInt::from(159 - r)
        + p;
    p += bi("600920179829731861736702779321621459595472258049074101567377883020018308");
    Ok(sar(&p, 174))
}

fn check_pow_limit(ratio: &BigInt, convexity_exp: &BigInt) -> Result<(), SimulationError> {
    if ratio == &wad() {
        return Ok(());
    }
    let ln_ratio = ln_wad(ratio)?;
    if mul_wad(convexity_exp, &ln_ratio) > bi("135000000000000000000") {
        return Err(trade_exceeds_limit());
    }
    Ok(())
}

fn exp_wad(x: &BigInt) -> Result<BigInt, SimulationError> {
    if x <= &bi("-41446531673892822313") {
        return Ok(BigInt::zero());
    }
    if x >= &bi("135305999368893231589") {
        return Err(trade_exceeds_limit());
    }

    let mut x2 = sdiv(&(x << 78usize), &bi("3814697265625"));
    let k = sar(
        &(sdiv(&(x2.clone() << 96usize), &bi("54916777467707473351141471128"))
            + (BigInt::one() << 95usize)),
        96,
    );
    x2 -= &k * bi("54916777467707473351141471128");

    let mut y = &x2 + bi("1346386616545796478920950773328");
    y = sar(&(y * &x2), 96) + bi("57155421227552351082224309758442");
    let mut p = &y + &x2 - bi("94201549194550492254356042504812");
    p = sar(&(p * &y), 96) + bi("28719021644029726153956944680412240");
    p = p * &x2 + (bi("4385272521454847904659076985693276") << 96usize);

    let mut q = &x2 - bi("2855989394907223263936484059900");
    q = sar(&(q * &x2), 96) + bi("50020603652535783019961831881945");
    q = sar(&(q * &x2), 96) - bi("533845033583426703283633433725380");
    q = sar(&(q * &x2), 96) + bi("3604857256930695427073651918091429");
    q = sar(&(q * &x2), 96) - bi("14423608567350463180887372962807573");
    q = sar(&(q * &x2), 96) + bi("26449188498355588339934803723976023");

    let mut r = sdiv(&p, &q);
    r *= bi("3822833074963236453042738258902158003155416615667");
    let shift = 195
        - k.to_i64()
            .ok_or_else(|| trade_exceeds_limit())?;
    if shift < 0 {
        return Err(trade_exceeds_limit());
    }
    Ok(r >> shift as usize)
}

fn u_to_bi(value: U256) -> BigInt {
    BigInt::from(u256_to_biguint(value))
}

fn bi_to_u(value: &BigInt) -> U256 {
    if value <= &BigInt::zero() {
        return U256::ZERO;
    }
    biguint_to_u256(
        &value
            .to_biguint()
            .expect("positive value"),
    )
}

fn positive_bigint_to_biguint(value: &BigInt) -> Result<BigUint, SimulationError> {
    value
        .to_biguint()
        .ok_or_else(|| fatal("negative quote result"))
}

fn reserve_decimals(state: &BaselineQuoteState) -> Result<u8, SimulationError> {
    u_to_bi(state.reserve_decimals)
        .to_u8()
        .ok_or_else(|| fatal("reserve decimals exceed u8"))
}

fn normalize_wad(amount: &BigInt, decimals: u8) -> BigInt {
    match decimals.cmp(&18) {
        std::cmp::Ordering::Less => amount * pow10(18 - decimals),
        std::cmp::Ordering::Greater => amount / pow10(decimals - 18),
        std::cmp::Ordering::Equal => amount.clone(),
    }
}

fn denormalize_wad(amount: &BigInt, decimals: u8) -> BigInt {
    match decimals.cmp(&18) {
        std::cmp::Ordering::Less => amount / pow10(18 - decimals),
        std::cmp::Ordering::Greater => amount * pow10(decimals - 18),
        std::cmp::Ordering::Equal => amount.clone(),
    }
}

fn denormalize_wad_up(amount: &BigInt, decimals: u8) -> BigInt {
    match decimals.cmp(&18) {
        std::cmp::Ordering::Less => ceil_div(amount, &pow10(18 - decimals)).unwrap_or_default(),
        std::cmp::Ordering::Greater => amount * pow10(decimals - 18),
        std::cmp::Ordering::Equal => amount.clone(),
    }
}

fn to_wad_signed(amount: &BigInt, decimals: u8) -> BigInt {
    if amount >= &BigInt::zero() {
        normalize_wad(amount, decimals)
    } else {
        -normalize_wad(&amount.abs(), decimals)
    }
}

fn mul_wad(x: &BigInt, y: &BigInt) -> BigInt {
    (x * y) / wad()
}

fn mul_wad_up(x: &BigInt, y: &BigInt) -> BigInt {
    ceil_div(&(x * y), &wad()).unwrap_or_default()
}

fn div_wad(x: &BigInt, y: &BigInt) -> Result<BigInt, SimulationError> {
    checked_div(&(x * wad()), y)
}

fn div_wad_up(x: &BigInt, y: &BigInt) -> Result<BigInt, SimulationError> {
    ceil_div(&(x * wad()), y).ok_or_else(invalid_curve_state)
}

fn full_mul_div(x: &BigInt, y: &BigInt, d: &BigInt) -> Result<BigInt, SimulationError> {
    checked_div(&(x * y), d)
}

fn full_mul_div_up(x: &BigInt, y: &BigInt, d: &BigInt) -> Result<BigInt, SimulationError> {
    ceil_div(&(x * y), d).ok_or_else(invalid_curve_state)
}

fn checked_div(x: &BigInt, y: &BigInt) -> Result<BigInt, SimulationError> {
    if y.is_zero() {
        return Err(invalid_curve_state());
    }
    Ok(x / y)
}

fn ceil_div(x: &BigInt, y: &BigInt) -> Option<BigInt> {
    if y.is_zero() {
        return None;
    }
    let q = x / y;
    let r = x % y;
    if r > BigInt::zero() {
        Some(q + 1u8)
    } else {
        Some(q)
    }
}

fn zero_floor_sub(x: &BigInt, y: &BigInt) -> BigInt {
    if x <= y {
        BigInt::zero()
    } else {
        x - y
    }
}

fn sdiv(x: &BigInt, y: &BigInt) -> BigInt {
    x / y
}

fn sar(x: &BigInt, shift: usize) -> BigInt {
    x >> shift
}

fn pow10(exp: u8) -> BigInt {
    BigInt::from(10u8).pow(u32::from(exp))
}

fn wad() -> BigInt {
    bi("1000000000000000000")
}

fn bi(value: &str) -> BigInt {
    BigInt::parse_bytes(value.as_bytes(), 10).expect("invalid Baseline math constant")
}

fn invalid_input(message: &str) -> SimulationError {
    SimulationError::InvalidInput(message.to_owned(), None)
}

fn no_rate() -> SimulationError {
    SimulationError::RecoverableError("no cached rate".into())
}

fn trade_exceeds_limit() -> SimulationError {
    fatal("trade exceeds limit")
}

fn invalid_curve_state() -> SimulationError {
    fatal("invalid curve state")
}

fn solver_failed() -> SimulationError {
    fatal("solver failed")
}

fn fatal(message: &str) -> SimulationError {
    SimulationError::FatalError(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kyber_reference_state() -> BaselineQuoteState {
        BaselineQuoteState {
            snapshot_curve: BaselineCurve {
                blv: u("2000000000000000000"),
                circ: u("500000000000000000000000"),
                supply: u("500000000000000000000000"),
                swap_fee: u("3000000000000000"),
                reserves: u("1500000000000000000000000"),
                total_supply: u("1000000000000000000000000"),
                convexity_exp: u("2000000000000000000"),
                last_invariant: u("500000000000000000000000"),
            },
            quote_block_buy_delta_circ: U256::ZERO,
            quote_block_sell_delta_circ: U256::ZERO,
            total_supply: u("1000000000000000000000000"),
            total_b_tokens: u("500000000000000000000000"),
            total_reserves: u("1500000000000000000000000"),
            reserve_decimals: U256::from(18),
            liquidity_fee_pct: u("1000000000000000000"),
            pending_surplus: U256::ZERO,
            should_settle_pending_surplus: false,
            max_sell_delta: u("100000000000000000000000"),
            snapshot_active_price: U256::ZERO,
        }
    }

    fn mainnet_block_25036613_state() -> BaselineQuoteState {
        BaselineQuoteState {
            snapshot_curve: BaselineCurve {
                blv: u("1231324299740620"),
                circ: u("11748074132403274817879106"),
                supply: u("9251925867596725182120894"),
                swap_fee: u("10000000000000000"),
                reserves: u("14490905092992563416302"),
                total_supply: u("21000000000000000000000000"),
                convexity_exp: u("36862730277523524071"),
                last_invariant: u("3782134669960863"),
            },
            quote_block_buy_delta_circ: U256::ZERO,
            quote_block_sell_delta_circ: u("2934894646429430000000"),
            total_supply: u("21000000000000000000000000"),
            total_b_tokens: u("9254860762243154612120894"),
            total_reserves: u("14486769694864818548473"),
            reserve_decimals: U256::from(18),
            liquidity_fee_pct: u("500000000000000000"),
            pending_surplus: u("7886165239394569"),
            should_settle_pending_surplus: false,
            max_sell_delta: u("11745139237756845387879106"),
            snapshot_active_price: u("1410914696199449"),
        }
    }

    fn base_block_46596028_state() -> BaselineQuoteState {
        BaselineQuoteState {
            snapshot_curve: BaselineCurve {
                blv: u("241125745213791"),
                circ: u("980409407858411724545879468"),
                supply: u("19590592141588275454120532"),
                swap_fee: u("13000000000000000"),
                reserves: u("454264698587187245009689"),
                total_supply: u("1000000000000000000000000000"),
                convexity_exp: u("2000000000000000000"),
                last_invariant: u("86988765264046734566"),
            },
            quote_block_buy_delta_circ: U256::ZERO,
            quote_block_sell_delta_circ: u("626804983954616623185"),
            total_supply: u("1000000000000000000000000000"),
            total_b_tokens: u("19591218946572230070743717"),
            total_reserves: u("454250328436968192392191"),
            reserve_decimals: U256::from(18),
            liquidity_fee_pct: u("173000000000000000"),
            pending_surplus: u("64072862348512188"),
            should_settle_pending_surplus: false,
            max_sell_delta: u("980408781053427769929256283"),
            snapshot_active_price: u("22927126532619896"),
        }
    }

    fn u(value: &str) -> U256 {
        biguint_to_u256(&value.parse::<BigUint>().unwrap())
    }

    #[test]
    fn terminal_sell_pays_blv_floor_without_swap_fee_discount() {
        let mut state = kyber_reference_state();
        state.max_sell_delta = state.snapshot_curve.circ;

        let quote = quote_sell_exact_in(
            &state,
            &"500000000000000000000000"
                .parse()
                .unwrap(),
        )
        .unwrap();

        assert_eq!(
            quote.amount_out,
            "1000000000000000000000000"
                .parse::<BigUint>()
                .unwrap()
        );
        assert_eq!(quote.state.total_b_tokens, u("1000000000000000000000000"));
        assert_eq!(quote.state.total_reserves, U256::ZERO);
        assert_eq!(quote.state.max_sell_delta, U256::ZERO);
    }

    #[test]
    fn sell_floor_raises_same_block_slice_to_blv_when_curve_release_allows() {
        let state = kyber_reference_state();
        let before_delta_circ = BigInt::zero();
        let after_delta_circ = -bi("100000000000000000000");
        let delta_user_wad = bi("150000000000000000000");
        let delta_invariant_wad = bi("250000000000000000000");

        let floored = apply_sell_floor(
            &state,
            &before_delta_circ,
            &after_delta_circ,
            delta_user_wad,
            &delta_invariant_wad,
        )
        .unwrap();

        assert_eq!(floored, bi("200000000000000000000"));
    }

    #[test]
    fn buy_solver_rejects_when_min_price_moving_delta_exceeds_inventory_cap() {
        let mut state = kyber_reference_state();
        state.total_b_tokens = u("1");

        let err = quote_buy_exact_in(&state, &"1000000000000000".parse().unwrap()).unwrap_err();

        assert!(matches!(err, SimulationError::FatalError(message) if message == "solver failed"));
    }

    #[test]
    fn quote_buy_exact_in_matches_current_mercury_solver_reference() {
        let cases = [
            (
                "1000000000000000",
                "166333851409557",
                "499999999833666148590443",
                "1500000000998003107819114",
                "166333851409557",
                "99999999833666148590443",
                "1996007740486",
            ),
            (
                "1000000000000000000",
                "166333693396278608",
                "499999833666306603721392",
                "1500000998002603048057216",
                "166333693396278608",
                "99999833666306603721392",
                "1996446991754338",
            ),
            (
                "123456789012345678901",
                "20532800019488850994",
                "499979467199980511149006",
                "1500123203546066495201988",
                "20532800019488850994",
                "99979467199980511149006",
                "253139965357581291",
            ),
        ];

        for (
            amount_in,
            amount_out,
            total_b_tokens,
            total_reserves,
            buy_delta,
            max_sell_delta,
            pending_surplus,
        ) in cases
        {
            let quote =
                quote_buy_exact_in(&kyber_reference_state(), &amount_in.parse().unwrap()).unwrap();

            assert_eq!(quote.amount_out, amount_out.parse::<BigUint>().unwrap());
            assert_eq!(quote.state.total_b_tokens, u(total_b_tokens));
            assert_eq!(quote.state.total_reserves, u(total_reserves));
            assert_eq!(quote.state.quote_block_buy_delta_circ, u(buy_delta));
            assert_eq!(quote.state.quote_block_sell_delta_circ, U256::ZERO);
            assert_eq!(quote.state.max_sell_delta, u(max_sell_delta));
            assert_eq!(quote.state.pending_surplus, u(pending_surplus));
        }
    }

    #[test]
    fn quote_sell_exact_in_matches_kyber_reference() {
        let cases = [
            (
                "166333998522065",
                "994011974287828",
                "500000000166333998522065",
                "1499999999001996010841214",
                "166333998522065",
                "99999999833666001477935",
                "3992014870958",
            ),
            (
                "166333851406675541",
                "994010215976602201",
                "500000166333851406675541",
                "1499999001997334232052877",
                "166333851406675541",
                "99999833666148593324459",
                "3992449791344922",
            ),
            (
                "20532817144902236677",
                "122690706352897821020",
                "500020532817144902236677",
                "1499876809842260374020308",
                "20532817144902236677",
                "99979467182855097763323",
                "499451386728158672",
            ),
        ];

        for (
            amount_in,
            amount_out,
            total_b_tokens,
            total_reserves,
            sell_delta,
            max_sell_delta,
            pending_surplus,
        ) in cases
        {
            let quote =
                quote_sell_exact_in(&kyber_reference_state(), &amount_in.parse().unwrap()).unwrap();

            assert_eq!(quote.amount_out, amount_out.parse::<BigUint>().unwrap());
            assert_eq!(quote.state.total_b_tokens, u(total_b_tokens));
            assert_eq!(quote.state.total_reserves, u(total_reserves));
            assert_eq!(quote.state.quote_block_buy_delta_circ, U256::ZERO);
            assert_eq!(quote.state.quote_block_sell_delta_circ, u(sell_delta));
            assert_eq!(quote.state.max_sell_delta, u(max_sell_delta));
            assert_eq!(quote.state.pending_surplus, u(pending_surplus));
        }
    }

    #[test]
    fn quote_buy_exact_in_uses_current_mercury_solver_for_mainnet_state() {
        let quote = quote_buy_exact_in(
            &mainnet_block_25036613_state(),
            &"1000000000000000".parse().unwrap(),
        )
        .unwrap();

        assert_eq!(
            quote.amount_out,
            "696600343051683342"
                .parse::<BigUint>()
                .unwrap()
        );
    }

    #[test]
    fn quote_sell_exact_in_matches_live_relay_at_mainnet_block_25036613() {
        let quote = quote_sell_exact_in(
            &mainnet_block_25036613_state(),
            &"1000000000000000000".parse().unwrap(),
        )
        .unwrap();

        assert_eq!(
            quote.amount_out,
            "1400055704843386"
                .parse::<BigUint>()
                .unwrap()
        );
    }

    #[test]
    fn quote_buy_exact_in_matches_live_relay_at_base_block_46596028() {
        let quote =
            quote_buy_exact_in(&base_block_46596028_state(), &"1000000000000000".parse().unwrap())
                .unwrap();

        assert_eq!(
            quote.amount_out,
            "43604497034367396"
                .parse::<BigUint>()
                .unwrap()
        );
    }

    #[test]
    fn quote_sell_exact_in_matches_live_relay_at_base_block_46596028() {
        let quote = quote_sell_exact_in(
            &base_block_46596028_state(),
            &"1000000000000000000".parse().unwrap(),
        )
        .unwrap();

        assert_eq!(
            quote.amount_out,
            "22333017436553081"
                .parse::<BigUint>()
                .unwrap()
        );
    }
}
