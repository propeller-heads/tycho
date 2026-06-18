use super::{
    mercury_state_store::{state_slot_key, MercuryStateArea},
    mercury_storage::{
        decode_block_pricing, decode_maker, decode_pool, BlockPricingState, MakerState, PoolState,
        SLOT_LEN,
    },
};
use num_bigint::BigInt;
use num_traits::{One, ToPrimitive, Zero};
use substreams::store::{StoreGet, StoreGetString};
use tycho_substreams::prelude::{Attribute, ChangeType};

const WAD: u64 = 1_000_000_000_000_000_000;
const MIN_CONVEXITY_EXP: u64 = 2_000_000_000_000_000_000;
const BUFFER_SAFETY_THRESHOLD_NUMERATOR: u64 = 95;
const BUFFER_SAFETY_THRESHOLD_DENOMINATOR: u64 = 100;

const SNAPSHOT_CURVE_FIELDS: [&str; 8] = [
    "snapshot_curve_blv",
    "snapshot_curve_circ",
    "snapshot_curve_supply",
    "snapshot_curve_swap_fee",
    "snapshot_curve_reserves",
    "snapshot_curve_total_supply",
    "snapshot_curve_convexity_exp",
    "snapshot_curve_last_invariant",
];

const QUOTE_STATE_FIELDS: [&str; 11] = [
    "quote_block_buy_delta_circ",
    "quote_block_sell_delta_circ",
    "total_supply",
    "total_b_tokens",
    "total_reserves",
    "reserve_decimals",
    "liquidity_fee_pct",
    "pending_surplus",
    "should_settle_pending_surplus",
    "max_sell_delta",
    "snapshot_active_price",
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct CurveParams {
    blv: BigInt,
    circ: BigInt,
    supply: BigInt,
    swap_fee: BigInt,
    reserves: BigInt,
    total_supply: BigInt,
    convexity_exp: BigInt,
    last_invariant: BigInt,
}

struct BlockQuoteContext {
    snapshot: CurveParams,
    block_buy_delta_circ: BigInt,
    block_sell_delta_circ: BigInt,
}

pub(crate) fn attributes_from_store(
    store: &StoreGetString,
    component_id: &str,
    read_ordinal: Option<u64>,
    current_block_number: u64,
    change: ChangeType,
) -> Option<Vec<Attribute>> {
    load_state(store, component_id, read_ordinal).and_then(|(pool, maker, pricing)| {
        attributes_from_state(&pool, &maker, &pricing, current_block_number, change)
    })
}

fn load_state(
    store: &StoreGetString,
    component_id: &str,
    read_ordinal: Option<u64>,
) -> Option<(PoolState, MakerState, BlockPricingState)> {
    let pool = read_slots::<8>(store, component_id, MercuryStateArea::Pool, read_ordinal)?;
    let maker = read_slots::<4>(store, component_id, MercuryStateArea::Maker, read_ordinal)?;
    let block_pricing =
        read_slots::<4>(store, component_id, MercuryStateArea::BlockPricing, read_ordinal)?;
    Some((decode_pool(&pool), decode_maker(&maker), decode_block_pricing(&block_pricing)))
}

fn attributes_from_state(
    pool: &PoolState,
    maker: &MakerState,
    pricing: &BlockPricingState,
    current_block_number: u64,
    change: ChangeType,
) -> Option<Vec<Attribute>> {
    let quote_context = preview_block_pricing(pool, maker, pricing, current_block_number)?;
    let active_price = compute_active_price(&quote_context.snapshot)?;
    let max_sell_delta = max_sell_delta(&quote_context, pool.b_token_decimals);
    let same_block = pricing.block_number == current_block_number;

    let snapshot_values = [
        quote_context.snapshot.blv,
        quote_context.snapshot.circ,
        quote_context.snapshot.supply,
        quote_context.snapshot.swap_fee,
        quote_context.snapshot.reserves,
        quote_context.snapshot.total_supply,
        quote_context.snapshot.convexity_exp,
        quote_context.snapshot.last_invariant,
    ];
    let quote_values = [
        if same_block { pricing.block_buy_delta_circ.clone() } else { BigInt::zero() },
        if same_block { pricing.block_sell_delta_circ.clone() } else { BigInt::zero() },
        pool.total_supply.clone(),
        pool.total_b_tokens.clone(),
        pool.total_reserves.clone(),
        BigInt::from(pool.reserve_decimals),
        pool.liquidity_fee_pct.clone(),
        pool.pending_surplus.clone(),
        BigInt::from(u8::from(should_settle_pending_surplus(pool, pricing, current_block_number))),
        max_sell_delta,
        active_price,
    ];

    let mut attributes = Vec::with_capacity(SNAPSHOT_CURVE_FIELDS.len() + QUOTE_STATE_FIELDS.len());
    attributes.extend(
        SNAPSHOT_CURVE_FIELDS
            .iter()
            .zip(snapshot_values)
            .map(|(name, value)| uint_attribute(name, value, change)),
    );
    attributes.extend(
        QUOTE_STATE_FIELDS
            .iter()
            .zip(quote_values)
            .map(|(name, value)| {
                if *name == "should_settle_pending_surplus" {
                    bool_attribute(name, !value.is_zero(), change)
                } else {
                    uint_attribute(name, value, change)
                }
            }),
    );

    Some(attributes)
}

fn preview_block_pricing(
    pool: &PoolState,
    maker: &MakerState,
    pricing: &BlockPricingState,
    current_block_number: u64,
) -> Option<BlockQuoteContext> {
    let committed = committed_curve_params(pool, maker, pricing, current_block_number)?;

    if pricing.block_number.is_zero() {
        return Some(BlockQuoteContext {
            snapshot: committed,
            block_buy_delta_circ: BigInt::zero(),
            block_sell_delta_circ: BigInt::zero(),
        });
    }

    if pricing.block_number == current_block_number {
        return Some(BlockQuoteContext {
            snapshot: apply_pool_snapshot(&committed, pricing),
            block_buy_delta_circ: pricing.block_buy_delta_circ.clone(),
            block_sell_delta_circ: pricing.block_sell_delta_circ.clone(),
        });
    }

    let snapshot =
        if !pricing.block_buy_delta_circ.is_zero() || !pricing.block_sell_delta_circ.is_zero() {
            curve_params_from_deferred_state(
                &committed,
                &preview_deferred_maker_state(pool, maker, &committed, pricing)?,
            )
        } else {
            committed
        };

    Some(BlockQuoteContext {
        snapshot,
        block_buy_delta_circ: BigInt::zero(),
        block_sell_delta_circ: BigInt::zero(),
    })
}

fn committed_curve_params(
    pool: &PoolState,
    maker: &MakerState,
    pricing: &BlockPricingState,
    current_block_number: u64,
) -> Option<CurveParams> {
    let mut params = stored_curve_params(pool, maker);
    if pricing.block_number == 0 || pricing.block_number == current_block_number {
        return Some(params);
    }

    if in_safety(pool) {
        let surplus_native = safety_surplus_native(pool, &params)?;
        if surplus_native > BigInt::zero() {
            params.reserves -= normalize_wad(&surplus_native, pool.reserve_decimals);
        }
        return Some(params);
    }

    if pool.pending_surplus > BigInt::zero() {
        params.reserves += normalize_wad(&pool.pending_surplus, pool.reserve_decimals);
    }

    Some(params)
}

fn stored_curve_params(pool: &PoolState, maker: &MakerState) -> CurveParams {
    CurveParams {
        blv: maker.blv_price.clone(),
        circ: normalize_wad(
            &(pool.total_supply.clone() - pool.total_b_tokens.clone()),
            pool.b_token_decimals,
        ),
        supply: normalize_wad(&pool.total_b_tokens, pool.b_token_decimals),
        swap_fee: maker.swap_fee.clone(),
        reserves: normalize_wad(&pool.total_reserves, pool.reserve_decimals),
        total_supply: normalize_wad(&pool.total_supply, pool.b_token_decimals),
        convexity_exp: maker.convexity_exp.clone(),
        last_invariant: maker.last_invariant.clone(),
    }
}

fn apply_pool_snapshot(committed: &CurveParams, pricing: &BlockPricingState) -> CurveParams {
    CurveParams {
        blv: committed.blv.clone(),
        swap_fee: committed.swap_fee.clone(),
        convexity_exp: committed.convexity_exp.clone(),
        last_invariant: pricing.start_last_invariant.clone(),
        total_supply: committed.total_supply.clone(),
        reserves: pricing.start_reserves.clone(),
        supply: pricing.start_supply.clone(),
        circ: committed.total_supply.clone() - pricing.start_supply.clone(),
    }
}

struct DeferredMakerState {
    blv_price: BigInt,
    convexity_exp: BigInt,
    last_invariant: BigInt,
    max_circ: BigInt,
    max_reserves: BigInt,
}

fn curve_params_from_deferred_state(
    committed: &CurveParams,
    next_state: &DeferredMakerState,
) -> CurveParams {
    CurveParams {
        swap_fee: committed.swap_fee.clone(),
        total_supply: committed.total_supply.clone(),
        reserves: committed.reserves.clone(),
        supply: committed.supply.clone(),
        circ: committed.circ.clone(),
        blv: next_state.blv_price.clone(),
        convexity_exp: next_state.convexity_exp.clone(),
        last_invariant: next_state.last_invariant.clone(),
    }
}

fn preview_deferred_maker_state(
    pool: &PoolState,
    maker: &MakerState,
    committed: &CurveParams,
    pricing: &BlockPricingState,
) -> Option<DeferredMakerState> {
    let mut next = DeferredMakerState {
        blv_price: committed.blv.clone(),
        convexity_exp: committed.convexity_exp.clone(),
        last_invariant: pricing.start_last_invariant.clone(),
        max_circ: maker.max_circ.clone(),
        max_reserves: maker.max_reserves.clone(),
    };

    if pricing.block_buy_delta_circ.is_zero() && pricing.block_sell_delta_circ.is_zero() {
        return Some(next);
    }

    let mut current = committed.clone();
    let previous = apply_pool_snapshot(committed, pricing);
    current.last_invariant = previous.last_invariant.clone();

    if !in_safety(pool) {
        if current.convexity_exp > BigInt::from(MIN_CONVEXITY_EXP) {
            preview_translate_convexity(&mut next, &current, &previous)?;
        } else {
            next.blv_price = compute_next_blv(&current, &previous.supply, &previous.reserves)?;
        }

        current.blv = next.blv_price.clone();
        current.convexity_exp = next.convexity_exp.clone();
        next.last_invariant = compute_invariant(&current)?;
    }

    Some(next)
}

fn preview_translate_convexity(
    next: &mut DeferredMakerState,
    params: &CurveParams,
    previous: &CurveParams,
) -> Option<()> {
    if params.circ.is_zero() || next.max_circ.is_zero() {
        return Some(());
    }

    let current_book_price = div_wad(&params.reserves, &params.circ)?;
    let max_book_price = div_wad(&next.max_reserves, &next.max_circ)?;
    let below_ath = current_book_price < max_book_price;
    let n = params.convexity_exp.clone();

    if !below_ath {
        next.max_circ = params.circ.clone();
        next.max_reserves = params.reserves.clone();
    }

    let buffer = params.reserves.clone() - mul_wad(&params.blv, &params.circ);
    if buffer.is_zero() {
        return Some(());
    }

    let mut n_floor = BigInt::from(MIN_CONVEXITY_EXP);
    if params.circ > previous.circ {
        let previous_price = compute_active_price(previous)?;
        if previous_price <= params.blv {
            return Some(());
        }
        n_floor = n_floor.max(full_mul_div(
            &(previous_price - &params.blv),
            &mul_wad(&params.supply, &params.circ),
            &mul_wad(&buffer, &params.total_supply),
        )?);
    } else {
        let curve_buffer = if params.supply >= params.circ {
            let ratio = div_wad(&params.supply, &params.circ)?;
            let ratio_pow_n = pow_wad(&ratio, &n)?;
            full_mul_div(&params.last_invariant, &BigInt::from(WAD), &ratio_pow_n)?
        } else {
            let inv_ratio = div_wad(&params.circ, &params.supply)?;
            let inv_ratio_pow_n = pow_wad(&inv_ratio, &n)?;
            mul_wad(&params.last_invariant, &inv_ratio_pow_n)
        };

        if curve_buffer >= buffer {
            return Some(());
        }
        n_floor = n_floor.max(full_mul_div(&n, &curve_buffer, &buffer)?);
    }

    if below_ath {
        let n_dominance =
            compute_minimum_convexity_exp(params, &next.max_reserves, &next.max_circ)?;
        if n_dominance > BigInt::zero() {
            n_floor = n_floor.max(n_dominance);
        }
    }

    n_floor = n_floor.max(compute_xyk_dominance_floor(params)?);

    let xyk_price = div_wad(&params.reserves, &params.supply)?;
    if xyk_price > params.blv {
        n_floor = n_floor.max(full_mul_div(
            &mul_wad(&(xyk_price - &params.blv), &params.supply),
            &params.circ,
            &mul_wad(&buffer, &params.total_supply),
        )?);
    }

    if n_floor >= n {
        return Some(());
    }

    if below_ath && !try_recompute_max_reserves(next, params, &n_floor, &buffer)? {
        return Some(());
    }

    next.convexity_exp = n_floor;
    Some(())
}

fn compute_active_price(params: &CurveParams) -> Option<BigInt> {
    if params.circ.is_zero() {
        return Some(params.blv.clone());
    }

    let buffer = params.reserves.clone() - mul_wad(&params.blv, &params.circ);
    if buffer < BigInt::zero() {
        return None;
    }
    let denominator = mul_wad(&params.supply, &params.circ);
    if denominator.is_zero() {
        return None;
    }
    let premium = (&buffer * mul_wad(&params.convexity_exp, &params.total_supply)) / denominator;
    Some(params.blv.clone() + premium)
}

fn safety_surplus_native(pool: &PoolState, params: &CurveParams) -> Option<BigInt> {
    if params.circ.is_zero() {
        return Some(BigInt::zero());
    }

    let ratio = div_wad(&params.supply, &params.circ)?;
    let ratio_pow_n = pow_wad(&ratio, &params.convexity_exp)?;
    let implied_buffer = full_mul_div_up(&params.last_invariant, &BigInt::from(WAD), &ratio_pow_n)?;
    let implied_reserves = implied_buffer + mul_wad_up(&params.blv, &params.circ);

    if params.reserves <= implied_reserves {
        return Some(BigInt::zero());
    }

    Some(denormalize_wad(&(params.reserves.clone() - implied_reserves), pool.reserve_decimals))
}

fn compute_invariant(params: &CurveParams) -> Option<BigInt> {
    if params.circ.is_zero() {
        return Some(params.last_invariant.clone());
    }

    let buffer = params.reserves.clone() - mul_wad(&params.blv, &params.circ);
    if buffer < BigInt::zero() {
        return None;
    }

    if params.supply >= params.circ {
        let ratio = div_wad_up(&params.supply, &params.circ)?;
        let ratio_pow_n = pow_wad(&ratio, &params.convexity_exp)?;
        Some(mul_wad_up(&buffer, &ratio_pow_n))
    } else {
        let inv_ratio = div_wad(&params.circ, &params.supply)?;
        let inv_ratio_pow_n = pow_wad(&inv_ratio, &params.convexity_exp)?;
        div_wad_up(&buffer, &inv_ratio_pow_n)
    }
}

fn compute_next_blv(
    params: &CurveParams,
    previous_supply: &BigInt,
    previous_reserves: &BigInt,
) -> Option<BigInt> {
    if params.convexity_exp != BigInt::from(MIN_CONVEXITY_EXP) {
        return None;
    }

    let previous_circ = &params.total_supply - previous_supply;
    if previous_circ.is_zero() {
        return Some(params.blv.clone());
    }

    let penalty_numerator = full_mul_div_up(
        &full_mul_div_up(previous_supply, previous_supply, &BigInt::from(WAD))?,
        &params.circ,
        &BigInt::from(WAD),
    )?;
    let denominator_base = full_mul_div(&params.supply, &previous_circ, &BigInt::from(WAD))?;
    let penalty_denominator =
        full_mul_div(&denominator_base, &denominator_base, &BigInt::from(WAD))?;
    if penalty_denominator.is_zero() {
        return Some(params.blv.clone());
    }

    let previous_blv_value = mul_wad_up(&params.blv, &(&params.total_supply - previous_supply));
    let penalty = full_mul_div_up(
        &(previous_reserves - previous_blv_value),
        &penalty_numerator,
        &penalty_denominator,
    )?;
    let book_price = div_wad(&params.reserves, &params.circ)?;
    let max_blv = if book_price > penalty { book_price - penalty } else { BigInt::zero() };

    let target_numerator = mul_wad(&params.reserves, &sqrt_wad(&params.total_supply)?);
    let target_denominator =
        mul_wad(&params.circ, &(sqrt_wad(&params.circ)? + sqrt_wad(&params.total_supply)?));
    let target_blv = div_wad(&target_numerator, &target_denominator)?;

    Some(
        max_blv
            .min(target_blv)
            .max(params.blv.clone()),
    )
}

fn compute_xyk_dominance_floor(params: &CurveParams) -> Option<BigInt> {
    if params.circ.is_zero() || params.reserves.is_zero() {
        return Some(BigInt::zero());
    }

    let buffer = params.reserves.clone() - mul_wad(&params.blv, &params.circ);
    let circ_ratio = div_wad(&params.circ, &params.total_supply)?;
    let buffer_ratio = div_wad(&buffer, &params.reserves)?;
    if buffer_ratio.is_zero() {
        return Some(BigInt::from(u128::MAX));
    }

    let linear_coeff = (&circ_ratio * 2u8) - BigInt::from(WAD);
    let discriminant = ((&linear_coeff * &linear_coeff) / BigInt::from(WAD))
        + div_wad(&(8u8 * mul_wad(&circ_ratio, &circ_ratio)), &buffer_ratio)?;
    let root = sqrt_wad(&discriminant)?;
    Some((-linear_coeff + root) / 2u8)
}

fn compute_minimum_convexity_exp(
    params: &CurveParams,
    max_reserves: &BigInt,
    max_circ: &BigInt,
) -> Option<BigInt> {
    let max_buffer = max_reserves - mul_wad(&params.blv, max_circ);
    let buffer = params.reserves.clone() - mul_wad(&params.blv, &params.circ);
    let min_supply = &params.total_supply - max_circ;
    let position_ratio =
        div_wad(&mul_wad(max_circ, &params.supply), &mul_wad(&params.circ, &min_supply))?;
    if position_ratio <= BigInt::from(WAD) {
        return Some(BigInt::zero());
    }

    let ln_position_ratio = ln_wad(&position_ratio)?;
    let ln_buffer_ratio = ln_wad(&div_wad(&max_buffer, &buffer)?)?;
    if ln_buffer_ratio <= BigInt::zero() {
        return Some(BigInt::zero());
    }

    div_wad(&ln_buffer_ratio, &ln_position_ratio)
}

fn try_recompute_max_reserves(
    next: &mut DeferredMakerState,
    params: &CurveParams,
    n_new: &BigInt,
    buffer: &BigInt,
) -> Option<bool> {
    let min_supply = &params.total_supply - &next.max_circ;
    if min_supply.is_zero() {
        return Some(false);
    }

    let num = full_mul_div(&next.max_circ, &params.supply, &BigInt::from(WAD))?;
    let den = full_mul_div(&params.circ, &min_supply, &BigInt::from(WAD))?;
    if den.is_zero() {
        return Some(false);
    }
    let position_ratio = full_mul_div(&num, &BigInt::from(WAD), &den)?;

    if position_ratio > BigInt::from(WAD) {
        let ln_position_ratio = ln_wad(&position_ratio)?;
        if mul_wad(n_new, &ln_position_ratio) > dec("135000000000000000000") {
            return Some(false);
        }
    }

    let growth = pow_wad(&position_ratio, n_new)?;
    if growth.is_zero() {
        return Some(false);
    }

    let blv_at_max_circ = full_mul_div(&params.blv, &next.max_circ, &BigInt::from(WAD))?;
    if !buffer.is_zero() {
        let max_uint128 = (BigInt::one() << 128usize) - 1u8;
        if blv_at_max_circ > max_uint128 {
            return Some(false);
        }
        let max_implied_buffer = max_uint128 - &blv_at_max_circ;
        if growth > full_mul_div(&max_implied_buffer, &BigInt::from(WAD), buffer)? {
            return Some(false);
        }
    }

    next.max_reserves = full_mul_div(buffer, &growth, &BigInt::from(WAD))? + blv_at_max_circ;
    Some(true)
}

fn max_sell_delta(context: &BlockQuoteContext, b_token_decimals: u8) -> BigInt {
    let snapshot_circ = denormalize_wad(&context.snapshot.circ, b_token_decimals);
    if context.block_sell_delta_circ >= snapshot_circ {
        return BigInt::zero();
    }
    let remaining = snapshot_circ - &context.block_sell_delta_circ;
    if context.block_buy_delta_circ >= remaining {
        return BigInt::zero();
    }
    remaining - &context.block_buy_delta_circ
}

fn should_settle_pending_surplus(
    pool: &PoolState,
    pricing: &BlockPricingState,
    current_block_number: u64,
) -> bool {
    pricing.block_number != 0
        && pricing.block_number != current_block_number
        && pool.pending_surplus > BigInt::zero()
}

fn in_safety(pool: &PoolState) -> bool {
    &pool.total_b_tokens * BUFFER_SAFETY_THRESHOLD_DENOMINATOR
        >= &pool.total_supply * BUFFER_SAFETY_THRESHOLD_NUMERATOR
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

fn mul_wad(x: &BigInt, y: &BigInt) -> BigInt {
    (x * y) / BigInt::from(WAD)
}

fn mul_wad_up(x: &BigInt, y: &BigInt) -> BigInt {
    ceil_div(&(x * y), &BigInt::from(WAD)).unwrap_or_default()
}

fn div_wad(x: &BigInt, y: &BigInt) -> Option<BigInt> {
    checked_div(&(x * BigInt::from(WAD)), y)
}

fn div_wad_up(x: &BigInt, y: &BigInt) -> Option<BigInt> {
    ceil_div(&(x * BigInt::from(WAD)), y)
}

fn full_mul_div(x: &BigInt, y: &BigInt, d: &BigInt) -> Option<BigInt> {
    checked_div(&(x * y), d)
}

fn full_mul_div_up(x: &BigInt, y: &BigInt, d: &BigInt) -> Option<BigInt> {
    ceil_div(&(x * y), d)
}

fn checked_div(x: &BigInt, y: &BigInt) -> Option<BigInt> {
    (!y.is_zero()).then(|| x / y)
}

fn ceil_div(x: &BigInt, y: &BigInt) -> Option<BigInt> {
    if y.is_zero() {
        return None;
    }
    let quotient = x / y;
    let remainder = x % y;
    if remainder > BigInt::zero() {
        Some(quotient + 1u8)
    } else {
        Some(quotient)
    }
}

fn pow_wad(x: &BigInt, y: &BigInt) -> Option<BigInt> {
    if x <= &BigInt::zero() {
        return None;
    }
    let exponent = (&ln_wad(x)? * y) / BigInt::from(WAD);
    let result = exp_wad(&exponent)?;
    (!result.is_zero()).then_some(result)
}

fn ln_wad(x: &BigInt) -> Option<BigInt> {
    if x <= &BigInt::zero() || x.bits() > 256 {
        return None;
    }

    let r = 256i64 - x.bits() as i64;
    let mut x96 = x << r as usize;
    x96 >>= 159usize;

    let mut p = dec("43456485725739037958740375743393")
        + sar(
            &((dec("24828157081833163892658089445524")
                + sar(&((dec("3273285459638523848632254066296") + &x96) * &x96), 96))
                * &x96),
            96,
        );
    p = sar(&(p * &x96), 96) - dec("11111509109440967052023855526967");
    p = sar(&(p * &x96), 96) - dec("45023709667254063763336534515857");
    p = sar(&(p * &x96), 96) - dec("14706773417378608786704636184526");
    p = p * &x96 - (dec("795164235651350426258249787498") << 96usize);

    let mut q = dec("5573035233440673466300451813936") + &x96;
    q = dec("71694874799317883764090561454958") + sar(&(&x96 * &q), 96);
    q = dec("283447036172924575727196451306956") + sar(&(&x96 * &q), 96);
    q = dec("401686690394027663651624208769553") + sar(&(&x96 * &q), 96);
    q = dec("204048457590392012362485061816622") + sar(&(&x96 * &q), 96);
    q = dec("31853899698501571402653359427138") + sar(&(&x96 * &q), 96);
    q = dec("909429971244387300277376558375") + sar(&(&x96 * &q), 96);

    p = &p / &q;
    p = dec("1677202110996718588342820967067443963516166") * p;
    p = dec("16597577552685614221487285958193947469193820559219878177908093499208371")
        * BigInt::from(159 - r)
        + p;
    p += dec("600920179829731861736702779321621459595472258049074101567377883020018308");
    Some(sar(&p, 174))
}

fn exp_wad(x: &BigInt) -> Option<BigInt> {
    if x <= &dec("-41446531673892822313") {
        return Some(BigInt::zero());
    }
    if x >= &dec("135305999368893231589") {
        return None;
    }

    let mut x2 = (x << 78usize) / dec("3814697265625");
    let k = sar(
        &(((x2.clone() << 96usize) / dec("54916777467707473351141471128"))
            + (BigInt::one() << 95usize)),
        96,
    );
    x2 -= &k * dec("54916777467707473351141471128");

    let mut y = &x2 + dec("1346386616545796478920950773328");
    y = sar(&(y * &x2), 96) + dec("57155421227552351082224309758442");
    let mut p = &y + &x2 - dec("94201549194550492254356042504812");
    p = sar(&(p * &y), 96) + dec("28719021644029726153956944680412240");
    p = p * &x2 + (dec("4385272521454847904659076985693276") << 96usize);

    let mut q = &x2 - dec("2855989394907223263936484059900");
    q = sar(&(q * &x2), 96) + dec("50020603652535783019961831881945");
    q = sar(&(q * &x2), 96) - dec("533845033583426703283633433725380");
    q = sar(&(q * &x2), 96) + dec("3604857256930695427073651918091429");
    q = sar(&(q * &x2), 96) - dec("14423608567350463180887372962807573");
    q = sar(&(q * &x2), 96) + dec("26449188498355588339934803723976023");

    let mut result = &p / &q;
    result *= dec("3822833074963236453042738258902158003155416615667");
    let shift = 195 - k.to_i64()?;
    (shift >= 0).then(|| result >> shift as usize)
}

fn sqrt_wad(x: &BigInt) -> Option<BigInt> {
    sqrt_floor(&(x * BigInt::from(WAD)))
}

fn sqrt_floor(x: &BigInt) -> Option<BigInt> {
    if x < &BigInt::zero() {
        return None;
    }
    if x.is_zero() {
        return Some(BigInt::zero());
    }
    let mut z = x.clone();
    let mut y = (x + 1u8) >> 1usize;
    while y < z {
        z = y.clone();
        y = ((x / &y) + &y) >> 1usize;
    }
    Some(z)
}

fn sar(x: &BigInt, shift: usize) -> BigInt {
    x >> shift
}

fn dec(value: &str) -> BigInt {
    BigInt::parse_bytes(value.as_bytes(), 10).expect("invalid decimal constant")
}

fn pow10(exp: u8) -> BigInt {
    BigInt::from(10u8).pow(exp.into())
}

fn read_slots<const N: usize>(
    store: &StoreGetString,
    component_id: &str,
    area: MercuryStateArea,
    read_ordinal: Option<u64>,
) -> Option<[[u8; SLOT_LEN]; N]> {
    let slots = (0..N)
        .map(|offset| {
            let key = state_slot_key(component_id, area, offset as u8);
            let value = read_ordinal.map_or_else(
                || store.get_last(&key),
                |ord| {
                    store
                        .get_at(ord, &key)
                        .or_else(|| store.get_last(&key))
                },
            );
            slot_from_store_value(value)
        })
        .collect::<Option<Vec<_>>>()?;
    slots.try_into().ok()
}

fn slot_from_store_value(value: Option<String>) -> Option<[u8; SLOT_LEN]> {
    value.map_or(Some([0; SLOT_LEN]), |value| slot_from_hex(&value))
}

fn slot_from_hex(value: &str) -> Option<[u8; SLOT_LEN]> {
    let bytes = hex::decode(
        value
            .strip_prefix("0x")
            .unwrap_or(value),
    )
    .ok()?;
    if bytes.len() != SLOT_LEN {
        return None;
    }
    let mut slot = [0; SLOT_LEN];
    slot.copy_from_slice(&bytes);
    Some(slot)
}

fn uint_attribute(name: &str, value: BigInt, change: ChangeType) -> Attribute {
    Attribute {
        name: name.to_string(),
        value: value
            .to_biguint()
            .unwrap_or_default()
            .to_bytes_be(),
        change: change.into(),
    }
}

fn bool_attribute(name: &str, value: bool, change: ChangeType) -> Attribute {
    Attribute { name: name.to_string(), value: vec![u8::from(value)], change: change.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::mercury_storage::{BlockPricingState, MakerState, PoolState};
    use std::collections::HashMap;

    fn dec(value: &str) -> BigInt {
        BigInt::parse_bytes(value.as_bytes(), 10).unwrap()
    }

    fn slot(hex_slot: &str) -> [u8; SLOT_LEN] {
        let bytes = hex::decode(hex_slot).unwrap();
        let mut slot = [0; SLOT_LEN];
        slot.copy_from_slice(&bytes);
        slot
    }

    fn attr_map(attributes: Vec<Attribute>) -> HashMap<String, Vec<u8>> {
        attributes
            .into_iter()
            .map(|attribute| (attribute.name, attribute.value))
            .collect()
    }

    fn uint_value(attributes: &HashMap<String, Vec<u8>>, name: &str) -> BigInt {
        BigInt::from_bytes_be(num_bigint::Sign::Plus, attributes.get(name).unwrap())
    }

    fn mainnet_pool() -> PoolState {
        decode_pool(&[
            slot("000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
            slot("000000000000000000000000000000000000000000115eec47f6cf7e35000000"),
            slot("000000000007a8d635345986a11bb8930000000000000310fa4031172299cd21"),
            slot("00000000000000108e4d3803273adfe700000000000000000003584b7279fb79"),
            slot("000000000000000000001212f5abc4d5e0330396dd2ba9de8d911e31cf16b18f"),
            slot("0000000006f05b59d3b200008044f710c58b6ea6a178cc540f9f1cd758f7d1b2"),
            slot("00000000000000003a85cedd4058f43706f05b59d3b2000003782dace9d90000"),
            slot("000000000000000001de07f14ffd899100000000000000002703df3e2ae5f74c"),
        ])
    }

    fn mainnet_maker() -> MakerState {
        decode_maker(&[
            slot("000000000000000000000000000000000000000000000000045fe2077d45cc01"),
            slot("000000000009d012d510b59e2de0b2050000000000000000002386f26fc10000"),
            slot("0000000000000001ff92b9e15ad389e7000000000000031ab5212e1eb33bbd56"),
            slot("000000000000000000000000000000000000000000000000000dae04f1d4853c"),
        ])
    }

    fn stale_mainnet_pricing() -> BlockPricingState {
        decode_block_pricing(&[
            slot("000000000007a8bb1a4f82a3b1cbb893000000000000031103ff0d5ad5e6ac38"),
            slot("000000000000001b1ae4d6e2ef50000000000000000000000000000000000000"),
            slot("000000000000000000000000000000000000000000000000000dae04f1d4853c"),
            slot("00000000000000000000000000000000000000000000000000000000018271d6"),
        ])
    }

    fn safety_pool() -> PoolState {
        PoolState {
            reserve: [0; 20],
            paused: false,
            total_supply: dec("1000000000000000000000"),
            total_reserves: dec("1100000000000000000000"),
            total_b_tokens: dec("970000000000000000000"),
            pending_surplus: BigInt::zero(),
            settled_reserves: BigInt::zero(),
            fee_recipient: [0; 20],
            reserve_decimals: 18,
            b_token_decimals: 18,
            creator: [0; 20],
            creator_fee_pct: BigInt::zero(),
            protocol_fee_pct: BigInt::zero(),
            liquidity_fee_pct: dec("500000000000000000"),
            creator_claimable: BigInt::zero(),
            protocol_claimable: BigInt::zero(),
            pending_yield: BigInt::zero(),
        }
    }

    fn safety_maker() -> MakerState {
        MakerState {
            initialized: true,
            blv_price: dec("1000000000000000000"),
            swap_fee: dec("3000000000000000"),
            max_circ: BigInt::zero(),
            max_reserves: BigInt::zero(),
            convexity_exp: dec("2000000000000000000"),
            last_invariant: dec("100000000000000000000"),
        }
    }

    fn stale_safety_pricing() -> BlockPricingState {
        BlockPricingState {
            start_reserves: dec("1000000000000000000000"),
            start_supply: dec("970000000000000000000"),
            block_buy_delta_circ: BigInt::zero(),
            block_sell_delta_circ: BigInt::zero(),
            start_last_invariant: dec("100000000000000000000"),
            block_number: 99,
        }
    }

    fn base_pool() -> PoolState {
        decode_pool(&[
            slot("0000000000000000000000004200000000000000000000000000000000000006"),
            slot("0000000000000000000000000000000000000001431e0fae6d7217caa0000000"),
            slot("000000005222c9c1d35dbedbc9509c17000000000000000024af58a803a4ba30"),
            slot("0000000000000000296becbd757787050000000000000000000116d4b4a72bb4"),
            slot("00000000000000000000121201422cf811f6f97186db733d9f390ab01f27ceac"),
            slot("00000000000000000000000001422cf811f6f97186db733d9f390ab01f27ceac"),
            slot("0000000000000000000000000000000006f05b59d3b2000003782dace9d90000"),
            slot("0000000000000000043e47a2632fc1a60000000000000000028b973adeec43f1"),
        ])
    }

    fn base_maker() -> MakerState {
        decode_maker(&[
            slot("0000000000000000000000000000000000000000000000000000000116e1cd01"),
            slot("00000000204fce5e3e250261100000000000000000000000000aa87bee538000"),
            slot("00000000000000001bc16d674ec800000000000000000000016345785d8a0000"),
            slot("00000000000000000000000000000000000000000000000002106d18cfd71e33"),
        ])
    }

    fn stale_base_pricing() -> BlockPricingState {
        decode_block_pricing(&[
            slot("00000000516ccef1eda5a2a56d509c1700000000000000002528bf326c82af98"),
            slot("0000000000b5facfe5b81c365c00000000000000000000000000000000000000"),
            slot("00000000000000000000000000000000000000000000000002106d18cfd71e33"),
            slot("0000000000000000000000000000000000000000000000000000000002d34125"),
        ])
    }

    #[test]
    fn reconstructs_stale_pending_mainnet_quote_state_fixture() {
        let attributes = attr_map(
            attributes_from_state(
                &mainnet_pool(),
                &mainnet_maker(),
                &stale_mainnet_pricing(),
                25_329_977,
                ChangeType::Update,
            )
            .unwrap(),
        );

        assert_eq!(uint_value(&attributes, "snapshot_curve_blv"), dec("1231324299740620"));
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_circ"),
            dec("11740210256556103974537069")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_supply"),
            dec("9259789743443896025462931")
        );
        assert_eq!(uint_value(&attributes, "snapshot_curve_swap_fee"), dec("10000000000000000"));
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_reserves"),
            dec("14480280762177710966938")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_total_supply"),
            dec("21000000000000000000000000")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_convexity_exp"),
            dec("36862730277523524071")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_last_invariant"),
            dec("3850660307984176")
        );
        assert_eq!(uint_value(&attributes, "quote_block_buy_delta_circ"), BigInt::zero());
        assert_eq!(uint_value(&attributes, "quote_block_sell_delta_circ"), BigInt::zero());
        assert_eq!(uint_value(&attributes, "max_sell_delta"), dec("11740210256556103974537069"));
        assert_eq!(uint_value(&attributes, "snapshot_active_price"), dec("1404179194607758"));
    }

    #[test]
    fn reconstructs_stale_safety_preview_by_skimming_surplus() {
        let pool = safety_pool();
        let maker = safety_maker();
        let pricing = stale_safety_pricing();
        let attributes = attr_map(
            attributes_from_state(&pool, &maker, &pricing, 100, ChangeType::Update).unwrap(),
        );

        let raw_reserves = normalize_wad(&pool.total_reserves, pool.reserve_decimals);
        let preview_reserves = uint_value(&attributes, "snapshot_curve_reserves");

        assert!(preview_reserves < raw_reserves);
        assert_eq!(uint_value(&attributes, "quote_block_buy_delta_circ"), BigInt::zero());
        assert_eq!(uint_value(&attributes, "quote_block_sell_delta_circ"), BigInt::zero());
    }

    #[test]
    fn reconstructs_stale_pending_base_quote_state_fixture() {
        let attributes = attr_map(
            attributes_from_state(
                &base_pool(),
                &base_maker(),
                &stale_base_pricing(),
                47_411_888,
                ChangeType::Update,
            )
            .unwrap(),
        );

        assert_eq!(uint_value(&attributes, "snapshot_curve_blv"), dec("18280922"));
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_circ"),
            dec("74580172945667604581975024617")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_supply"),
            dec("25419827054332395418024975383")
        );
        assert_eq!(uint_value(&attributes, "snapshot_curve_swap_fee"), dec("3000000000000000"));
        assert_eq!(uint_value(&attributes, "snapshot_curve_reserves"), dec("2643735562725090788"));
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_total_supply"),
            dec("100000000000000000000000000000")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_convexity_exp"),
            dec("2000000000000000000")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_last_invariant"),
            dec("148738755891173032")
        );
        assert_eq!(uint_value(&attributes, "quote_block_buy_delta_circ"), BigInt::zero());
        assert_eq!(uint_value(&attributes, "quote_block_sell_delta_circ"), BigInt::zero());
        assert_eq!(uint_value(&attributes, "total_supply"), dec("100000000000000000000000000000"));
        assert_eq!(uint_value(&attributes, "total_b_tokens"), dec("25419827054332395418024975383"));
        assert_eq!(uint_value(&attributes, "total_reserves"), dec("2643428984928647728"));
        assert_eq!(uint_value(&attributes, "pending_surplus"), dec("306577796443060"));
        assert_eq!(uint_value(&attributes, "max_sell_delta"), dec("74580172945667604581975024617"));
        assert_eq!(uint_value(&attributes, "snapshot_active_price"), dec("153351186"));
    }

    #[test]
    fn reconstructs_stale_mainnet_quote_state_without_pending_deltas() {
        let mut pricing = stale_mainnet_pricing();
        pricing.block_buy_delta_circ = BigInt::zero();
        pricing.block_sell_delta_circ = BigInt::zero();
        let attributes = attr_map(
            attributes_from_state(
                &mainnet_pool(),
                &mainnet_maker(),
                &pricing,
                25_329_977,
                ChangeType::Update,
            )
            .unwrap(),
        );

        assert_eq!(uint_value(&attributes, "snapshot_curve_blv"), dec("1231324299740620"));
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_circ"),
            dec("11740210256556103974537069")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_supply"),
            dec("9259789743443896025462931")
        );
        assert_eq!(uint_value(&attributes, "snapshot_curve_swap_fee"), dec("10000000000000000"));
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_reserves"),
            dec("14480280762177710966938")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_total_supply"),
            dec("21000000000000000000000000")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_convexity_exp"),
            dec("36862730277523524071")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_last_invariant"),
            dec("3850510957577532")
        );
        assert_eq!(uint_value(&attributes, "quote_block_buy_delta_circ"), BigInt::zero());
        assert_eq!(uint_value(&attributes, "quote_block_sell_delta_circ"), BigInt::zero());
        assert_eq!(uint_value(&attributes, "total_supply"), dec("21000000000000000000000000"));
        assert_eq!(uint_value(&attributes, "total_b_tokens"), dec("9259789743443896025462931"));
        assert_eq!(uint_value(&attributes, "total_reserves"), dec("14480279820671714446625"));
        assert_eq!(uint_value(&attributes, "reserve_decimals"), dec("18"));
        assert_eq!(uint_value(&attributes, "liquidity_fee_pct"), dec("500000000000000000"));
        assert_eq!(uint_value(&attributes, "pending_surplus"), dec("941505996520313"));
        assert_eq!(
            attributes
                .get("should_settle_pending_surplus")
                .unwrap(),
            &vec![1]
        );
        assert_eq!(uint_value(&attributes, "max_sell_delta"), dec("11740210256556103974537069"));
        assert_eq!(uint_value(&attributes, "snapshot_active_price"), dec("1404179194607758"));
    }

    #[test]
    fn exposes_same_block_deltas_and_pool_snapshot() {
        let mut pricing = stale_mainnet_pricing();
        pricing.block_number = 25_329_977;
        let attributes = attr_map(
            attributes_from_state(
                &mainnet_pool(),
                &mainnet_maker(),
                &pricing,
                25_329_977,
                ChangeType::Update,
            )
            .unwrap(),
        );

        assert_eq!(
            uint_value(&attributes, "quote_block_sell_delta_circ"),
            dec("500000000000000000000")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_reserves"),
            dec("14480982061446959901752")
        );
        assert_eq!(
            uint_value(&attributes, "snapshot_curve_supply"),
            dec("9259289743443896025462931")
        );
        assert_eq!(
            attributes
                .get("should_settle_pending_surplus")
                .unwrap(),
            &vec![0]
        );
    }

    #[test]
    fn treats_missing_store_slots_as_zero_but_rejects_malformed_values() {
        assert_eq!(slot_from_store_value(None), Some([0; SLOT_LEN]));

        let zero_prefixed = format!("0x{}", "00".repeat(SLOT_LEN));
        assert_eq!(slot_from_store_value(Some(zero_prefixed)), Some([0; SLOT_LEN]));

        assert!(slot_from_store_value(Some("0x1234".to_string())).is_none());
        assert!(slot_from_store_value(Some("not hex".to_string())).is_none());
    }
}
