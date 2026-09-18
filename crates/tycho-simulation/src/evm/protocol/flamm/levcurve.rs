// Copyright (c) 2026 Everlong Labs Limited

//! Wei-exact port of the frozen c1 leverage curve `CollRebalancerMath`
//! (`src/hooks/everlong/lev/CollRebalancerMath.sol` @ c104 `80abd43`, linked on Base at
//! `0xC002d0731E6a2E6e80Be754779bCEf6B01Aff0bb`) and its `LevCurveTypes` tuple.
//!
//! Every quantity is a `U256` and every step keeps Solidity's operation order: `/` and
//! `Math.mulDiv` floor, `Math.Rounding.Up` ceils, `Math.sqrt` floors and `Mul512.productGt`
//! compares the two 512-bit products exactly.
//!
//! The library never reverts. Its entrypoints bound `cv` and `debt` by `MAX_INPUT = 1e38` before
//! any raw product forms, and the widest of those products (`sum * sum` in
//! [`root_interval_contains`] and [`cv_required_on_anchor`]) peaks at ~7.1e76 < 2^256, so the
//! wrapping `U256` arithmetic below is exact wherever Solidity's checked arithmetic would have
//! been, and the overflow flags of the raw `mulDiv` helpers are ignored here on the same bound. The
//! private helpers are `pub` so a parity test can drive each one; driven outside its callers'
//! domain a helper wraps where Solidity would panic, and returns zero where Solidity would divide
//! by zero, exactly as the Go port does.

use alloy::primitives::U256;

use super::math::{
    mul512, mul_div_floor_raw, mul_div_up_raw, product_gt, sqrt, PPM, WAD, WAD_SQUARED,
};

/// `MAX_INPUT = 1e38` (`CollRebalancerMath.sol:21`).
pub const MAX_INPUT: U256 = U256::from_limbs([0x098a_2240_0000_0000, 0x4b3b_4ca8_5a86_c47a, 0, 0]);
/// `LEVERAGE_RATIO_WAD = floor(4e18 / 9)`, the only `rWad` the curve accepts (`:26`).
pub const LEVERAGE_RATIO_WAD: U256 = U256::from_limbs([444_444_444_444_444_444, 0, 0, 0]);

// Floors of the frozen 155%-wall curve in normalized h = cv/T and D = d/T space (`:29-34`).
/// `H_ZERO = 9/16` (`:29`).
pub const H_ZERO: U256 = U256::from_limbs([562_500_000_000_000_000, 0, 0, 0]);
/// `H_JOIN` (`:30`).
pub const H_JOIN: U256 = U256::from_limbs([1_010_000_000_000_000_000, 0, 0, 0]);
/// `H_WALL` (`:31`).
pub const H_WALL: U256 = U256::from_limbs([1_882_448_291_726_770_582, 0, 0, 0]);
/// `WIDTH` (`:32`).
pub const WIDTH: U256 = U256::from_limbs([872_448_291_726_770_582, 0, 0, 0]);
/// `D_JOIN` (`:33`).
pub const D_JOIN: U256 = U256::from_limbs([509_975_124_224_178_054, 0, 0, 0]);
/// `D_WALL` (`:34`).
pub const D_WALL: U256 = U256::from_limbs([1_214_482_768_855_981_020, 0, 0, 0]);

// Cubic Bezier controls for phi = dD/dh on the transition (`:37-40`).
pub const P0: U256 = U256::from_limbs([995_037_190_209_989_135, 0, 0, 0]);
pub const P1: U256 = U256::from_limbs([851_783_312_849_706_840, 0, 0, 0]);
pub const P2: U256 = U256::from_limbs([738_044_106_433_170_508, 0, 0, 0]);
pub const P3: U256 = U256::from_limbs([645_161_290_322_580_645, 0, 0, 0]);

// Quartic Bezier controls for the exact integral of the cubic above, `Q0 = 0` (`:43-47`).
pub const Q1: U256 = U256::from_limbs([248_759_297_552_497_283, 0, 0, 0]);
pub const Q2: U256 = U256::from_limbs([461_705_125_764_923_993, 0, 0, 0]);
pub const Q3: U256 = U256::from_limbs([646_216_152_373_216_620, 0, 0, 0]);
pub const Q4: U256 = U256::from_limbs([807_506_474_953_861_782, 0, 0, 0]);

/// `AT_OR_BELOW_TARGET_SPREAD_CAP_PPM` (`:70`): the deleverage spread ceiling on collateral
/// released while the position is at or below its 200% target.
pub const TARGET_SPREAD_CAP_PPM: U256 = U256::from_limbs([13_000, 0, 0, 0]);
/// `_deleverageProRata`'s dust guard (`:468`): anchors below it are charged the posted rate.
pub const DUST_ANCHOR_FLOOR: U256 = U256::from_limbs([101, 0, 0, 0]);

/// `_debtNormWad`'s `1.5 WAD` half-law offset (`:601`).
const HALF_LAW_OFFSET: U256 = U256::from_limbs([1_500_000_000_000_000_000, 0, 0, 0]);
/// `3 * WAD` (`:338`).
const THREE_WAD: U256 = U256::from_limbs([3_000_000_000_000_000_000, 0, 0, 0]);
/// `WAD - 1` (`:377`).
const WAD_MINUS_ONE: U256 = U256::from_limbs([999_999_999_999_999_999, 0, 0, 0]);
const ONE: U256 = U256::from_limbs([1, 0, 0, 0]);
const TWO: U256 = U256::from_limbs([2, 0, 0, 0]);
const THREE: U256 = U256::from_limbs([3, 0, 0, 0]);
const FOUR: U256 = U256::from_limbs([4, 0, 0, 0]);
const SIX: U256 = U256::from_limbs([6, 0, 0, 0]);
const EIGHT: U256 = U256::from_limbs([8, 0, 0, 0]);

/// `D_WALL * WAD^2` and `H_WALL * WAD^2`, the numerator base and denominator of
/// `_recoveryDebtAtY`'s `rho` (`:403-404`).
const RHO_WALL_NUMERATOR: U256 = D_WALL.wrapping_mul(WAD_SQUARED);
const RHO_DENOMINATOR: U256 = H_WALL.wrapping_mul(WAD_SQUARED);
/// `H_WALL - D_WALL` (`:403`, `:546`).
const H_WALL_MINUS_D_WALL: U256 = H_WALL.wrapping_sub(D_WALL);

/// `CollRebalancerMath.RecoveryState` (`:72-80`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryState {
    pub ok: bool,
    pub anchor: U256,
    pub base_x: U256,
    pub y: U256,
    pub wall_cv: U256,
    pub wall_debt: U256,
    pub stable_to_wall: U256,
}

/// The four words `_strictAnchor` returns (`:171-175`): the wall is inclusive; `h` is only set on
/// the Hermite piece, where it is the least `h` in `[H_JOIN, H_WALL]` with `debt * h <= cv * D(h)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StrictAnchor {
    pub ok: bool,
    pub anchor: U256,
    pub h: U256,
    pub half_law: bool,
}

/// The three words `leverageQuote` / `deleverageQuote` return: `out` is the stable borrowed (up) or
/// the collateral released (down), zero on a refusal, which leaves the inputs unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quote {
    pub out: U256,
    pub new_collateral: U256,
    pub new_debt: U256,
}

impl Quote {
    /// The `(0, collateral, debt)` refusal triple.
    fn refused(collateral: U256, debt: U256) -> Self {
        Self { out: U256::ZERO, new_collateral: collateral, new_debt: debt }
    }
}

/// `Math.mulDiv` on inputs the curve has already bounded (see the module documentation), so the
/// overflow flag cannot be set.
fn mul_div_floor(x: U256, y: U256, d: U256) -> U256 {
    mul_div_floor_raw(x, y, d).0
}

/// `Math.mulDiv(..., Rounding.Up)` on bounded inputs.
fn mul_div_up(x: U256, y: U256, d: U256) -> U256 {
    mul_div_up_raw(x, y, d).0
}

/// `CollRebalancerMath.frozenParams()` (`:114-119`) field for field (`LevCurveParams`,
/// `LevCurveTypes.sol:38-60`): the 13-word curve tuple, `hZero`, `leverageRatioWad` and
/// `targetSpreadCapPpm`. The library leaves the four caller-owned fields zero.
pub fn frozen_params() -> [U256; 16] {
    [
        H_JOIN,
        H_WALL,
        WIDTH,
        D_JOIN,
        D_WALL,
        P0,
        P1,
        P2,
        P3,
        Q1,
        Q2,
        Q3,
        Q4,
        H_ZERO,
        LEVERAGE_RATIO_WAD,
        TARGET_SPREAD_CAP_PPM,
    ]
}

/// `_markedValue` (`:634-640`) -> `(ok, cv)`: `cv = floor(collateral * price / WAD)`, refused when
/// the 512-bit product's high limb reaches `WAD` (the quotient would not fit) or `cv` exceeds
/// `MAX_INPUT`.
pub fn marked_value(collateral: U256, price: U256) -> (bool, U256) {
    if price.is_zero() {
        return (false, U256::ZERO);
    }
    let (hi, _) = mul512(collateral, price);
    if hi >= WAD {
        return (false, U256::ZERO);
    }
    let cv = mul_div_floor(collateral, price, WAD);
    (cv <= MAX_INPUT, cv)
}

/// `_strictAnchor` (`:171-198`): the normal-branch solve. Off the domain (`rWad`, `MAX_INPUT`, past
/// the wall) `ok` is false; `cv == 0` is accepted with a zero anchor only when `debt == 0`.
pub fn strict_anchor(cv: U256, debt: U256, r_wad: U256) -> StrictAnchor {
    if r_wad != LEVERAGE_RATIO_WAD || cv > MAX_INPUT || debt > MAX_INPUT {
        return StrictAnchor::default();
    }
    if cv.is_zero() {
        return StrictAnchor {
            ok: debt.is_zero(),
            anchor: U256::ZERO,
            h: U256::ZERO,
            half_law: true,
        };
    }
    // Exact fixed-point domain decisions. The wall is inclusive.
    if product_gt(debt, H_WALL, cv, D_WALL) {
        return StrictAnchor::default();
    }
    if !product_gt(debt, H_JOIN, cv, D_JOIN) {
        let anchor = half_law_anchor(cv, debt);
        return StrictAnchor { ok: !anchor.is_zero(), anchor, h: U256::ZERO, half_law: true };
    }
    let mut lo = H_JOIN;
    let mut hi = H_WALL;
    while lo < hi {
        let mid = (lo + hi) >> 1;
        if product_gt(debt, mid, cv, debt_norm_wad(mid)) {
            lo = mid + ONE;
        } else {
            hi = mid;
        }
    }
    let anchor = mul_div_floor(THREE * cv, WAD, TWO * lo);
    StrictAnchor { ok: !anchor.is_zero(), anchor, h: lo, half_law: false }
}

/// `_halfLawAnchor` (`:201-210`): the upper root of `3(B+d)^2 = 8B*cv`, floored by
/// `(8cv - 6d + 4*isqrt(2cv(2cv-3d))) / 6` and then lifted by one wei when that still satisfies the
/// root interval.
pub fn half_law_anchor(cv: U256, debt: U256) -> U256 {
    let two_cv = TWO * cv;
    let three_debt = THREE * debt;
    // Reject malformed tuples before subtraction.
    if three_debt > two_cv {
        return U256::ZERO;
    }
    let root = sqrt(two_cv * (two_cv - three_debt));
    let anchor = (EIGHT * cv - SIX * debt + FOUR * root) / SIX;
    let plus_one = anchor + ONE;
    if root_interval_contains(cv, debt, plus_one) {
        return plus_one;
    }
    anchor
}

/// `_rootIntervalContains` (`:212-217`): `!(3 * (anchor + debt)^2 > 8 * anchor * cv)`, with `sum^2`
/// and `8B` formed at 256 bits and the outer products compared at 512.
pub fn root_interval_contains(cv: U256, debt: U256, anchor: U256) -> bool {
    let sum = anchor + debt;
    !product_gt(THREE, sum * sum, EIGHT * anchor, cv)
}

/// `deleverageQuote` (`:233-269`): retire `stable_in` of debt and release collateral with
/// output-shrink rounding. A zero `out` is a refusal and returns the inputs unchanged. The strict
/// branch prices the fee pro rata ([`deleverage_pro_rata`]); a solvent state beyond the wall takes
/// the recovery continuation, which keeps the pre-fill cliff spread.
pub fn deleverage_quote(
    collateral: U256,
    debt: U256,
    price: U256,
    r_wad: U256,
    spread_ppm: U256,
    stable_in: U256,
) -> Quote {
    if collateral.is_zero() ||
        price.is_zero() ||
        r_wad != LEVERAGE_RATIO_WAD ||
        spread_ppm >= PPM ||
        stable_in.is_zero() ||
        stable_in > debt ||
        debt > MAX_INPUT
    {
        return Quote::refused(collateral, debt);
    }
    let (marked, cv) = marked_value(collateral, price);
    if !marked || cv.is_zero() {
        return Quote::refused(collateral, debt);
    }
    let strict = strict_anchor(cv, debt, r_wad);
    if !strict.ok || strict.anchor.is_zero() {
        return recovery_deleverage(collateral, debt, cv, price, r_wad, spread_ppm, stable_in);
    }
    let anchor = strict.anchor;

    let new_debt = debt - stable_in;
    let (feasible, cv_required) = cv_required_on_anchor(anchor, new_debt);
    if !feasible {
        return Quote::refused(collateral, debt);
    }
    let collateral_required = mul_div_up(cv_required, WAD, price);
    if collateral_required >= collateral {
        return Quote::refused(collateral, debt);
    }
    let out_gross = collateral - collateral_required;
    let (collateral_out, effective_spread) =
        deleverage_pro_rata(anchor, collateral, debt, new_debt, price, out_gross, spread_ppm);
    if collateral_out.is_zero() {
        return Quote::refused(collateral, debt);
    }
    let new_collateral = collateral - collateral_out;
    if !post_strict_anchor_accepted(
        anchor,
        new_collateral,
        new_debt,
        price,
        r_wad,
        effective_spread,
    ) {
        return Quote::refused(collateral, debt);
    }
    Quote { out: collateral_out, new_collateral, new_debt }
}

/// `leverageQuote` (`:283-316`): add `collateral_in` and borrow along the same fixed-anchor book,
/// net of the spread (floor), with the NET payout booked as debt. Strict-only.
pub fn leverage_quote(
    collateral: U256,
    debt: U256,
    price: U256,
    r_wad: U256,
    spread_ppm: U256,
    collateral_in: U256,
) -> Quote {
    if collateral.is_zero() ||
        price.is_zero() ||
        r_wad != LEVERAGE_RATIO_WAD ||
        spread_ppm >= PPM ||
        collateral_in.is_zero() ||
        collateral_in > MAX_INPUT ||
        debt > MAX_INPUT ||
        collateral_in > U256::MAX - collateral
    {
        return Quote::refused(collateral, debt);
    }
    let (marked, cv) = marked_value(collateral, price);
    if !marked || cv.is_zero() {
        return Quote::refused(collateral, debt);
    }
    let strict = strict_anchor(cv, debt, r_wad);
    if !strict.ok || strict.anchor.is_zero() {
        return Quote::refused(collateral, debt);
    }
    let anchor = strict.anchor;

    let new_collateral = collateral + collateral_in;
    let (new_marked, new_cv) = marked_value(new_collateral, price);
    if !new_marked || new_cv <= cv {
        return Quote::refused(collateral, debt);
    }
    let (feasible, debt_cap) = debt_cap_on_anchor(anchor, new_cv);
    if !feasible || debt_cap <= debt {
        return Quote::refused(collateral, debt);
    }
    let gross_out = debt_cap - debt;
    let stable_out = mul_div_floor(gross_out, PPM - spread_ppm, PPM);
    if stable_out.is_zero() {
        return Quote::refused(collateral, debt);
    }
    let new_debt = debt + stable_out;
    if !post_strict_anchor_accepted(anchor, new_collateral, new_debt, price, r_wad, spread_ppm) {
        return Quote::refused(collateral, debt);
    }
    Quote { out: stable_out, new_collateral, new_debt }
}

/// `_cvRequiredOnAnchor` (`:319-340`): the least marked value keeping `anchor` after retiring to
/// `debt`. Half-law piece (`3d*WAD <= 2B*D_JOIN`): `ceil(3(B+d)^2 / (8B))`; Hermite piece: the
/// least `h` with `3d*WAD <= 2B*D(h)`, then `ceil(2B*h / (3*WAD))`; past the wall it is infeasible.
pub fn cv_required_on_anchor(anchor: U256, debt: U256) -> (bool, U256) {
    let three_debt = THREE * debt;
    let two_anchor = TWO * anchor;
    if !product_gt(three_debt, WAD, two_anchor, D_JOIN) {
        let sum = anchor + debt;
        return (true, mul_div_up(THREE, sum * sum, EIGHT * anchor));
    }
    if product_gt(three_debt, WAD, two_anchor, D_WALL) {
        return (false, U256::ZERO);
    }
    let mut lo = H_JOIN;
    let mut hi = H_WALL;
    while lo < hi {
        let mid = (lo + hi) >> 1;
        if product_gt(three_debt, WAD, two_anchor, debt_norm_wad(mid)) {
            lo = mid + ONE;
        } else {
            hi = mid;
        }
    }
    (true, mul_div_up(two_anchor, lo, THREE_WAD))
}

/// `_debtCapOnAnchor` (`:343-356`): the greatest debt whose C1 anchor at `cv` is at least `anchor`,
/// with `h = floor(3cv*WAD / (2B))` confined to `[H_ZERO, H_WALL]`; `isqrt(floor(8B*cv/3)) - B` on
/// the half law.
pub fn debt_cap_on_anchor(anchor: U256, cv: U256) -> (bool, U256) {
    let h = mul_div_floor(THREE * cv, WAD, TWO * anchor);
    if h < H_ZERO || h > H_WALL {
        return (false, U256::ZERO);
    }
    if h <= H_JOIN {
        let root = sqrt(mul_div_floor(EIGHT * anchor, cv, THREE));
        if root <= anchor {
            return (false, U256::ZERO);
        }
        return (true, root - anchor);
    }
    (true, mul_div_floor(cv, debt_norm_wad(h), h))
}

/// `_recoveryDeleverage` (`:360-399`): a missed-wall state retires debt along its conservative
/// continuation on the wall anchor. The partial retirement's `y` is the least `y` in `[0, y0(+1)]`
/// with `D(y) >= newDebt`; the whole release is charged the PRE-fill cliff spread
/// ([`deleverage_spread`]).
#[allow(clippy::too_many_arguments)]
pub fn recovery_deleverage(
    collateral: U256,
    debt: U256,
    cv: U256,
    price: U256,
    r_wad: U256,
    spread_ppm: U256,
    stable_in: U256,
) -> Quote {
    let recovery = recovery_state(cv, debt, r_wad);
    if !recovery.ok || stable_in > recovery.stable_to_wall {
        return Quote::refused(collateral, debt);
    }

    let new_debt = debt - stable_in;
    let mut y_new = U256::ZERO;
    if stable_in != recovery.stable_to_wall {
        let mut lo = U256::ZERO;
        let mut hi = if recovery.y < WAD_MINUS_ONE { recovery.y + ONE } else { recovery.y };
        if recovery_debt_at_y(recovery.wall_cv, hi) < new_debt {
            return Quote::refused(collateral, debt);
        }
        while lo < hi {
            let mid = (lo + hi) >> 1;
            if recovery_debt_at_y(recovery.wall_cv, mid) < new_debt {
                lo = mid + ONE;
            } else {
                hi = mid;
            }
        }
        y_new = lo;
    }

    let invariant_cv = if y_new.is_zero() {
        recovery.wall_cv
    } else {
        mul_div_up(recovery.wall_cv, WAD + y_new, WAD - y_new)
    };
    let collateral_required = mul_div_up(invariant_cv, WAD, price);
    if collateral_required >= collateral {
        return Quote::refused(collateral, debt);
    }
    let out_gross = collateral - collateral_required;
    let effective_spread = deleverage_spread(cv, debt, spread_ppm);
    let collateral_out = mul_div_floor(out_gross, PPM - effective_spread, PPM);
    if collateral_out.is_zero() {
        return Quote::refused(collateral, debt);
    }
    let new_collateral = collateral - collateral_out;
    if !post_any_anchor_accepted(
        recovery.anchor,
        new_collateral,
        new_debt,
        price,
        r_wad,
        effective_spread,
    ) {
        return Quote::refused(collateral, debt);
    }
    Quote { out: collateral_out, new_collateral, new_debt }
}

/// `_recoveryDebtAtY` (`:401-405`): `z = ceil(wallCv(1+y)/(1-y))` (`wallCv` at `y = 0`) times
/// `rho(y) = (D_WALL + (H_WALL-D_WALL)y^2) / H_WALL`, floored.
pub fn recovery_debt_at_y(wall_cv: U256, y: U256) -> U256 {
    let z = if y.is_zero() { wall_cv } else { mul_div_up(wall_cv, WAD + y, WAD - y) };
    let rho_numerator = RHO_WALL_NUMERATOR + H_WALL_MINUS_D_WALL * y * y;
    mul_div_floor(z, rho_numerator, RHO_DENOMINATOR)
}

/// `_deleverageSpread` (`:415-418`), the whole-fill cliff read off the PRE-fill state: the cap when
/// `2 * debt >= cv` and the posted spread exceeds it. Reachable only from the recovery branch.
pub fn deleverage_spread(cv: U256, debt: U256, posted_spread: U256) -> U256 {
    if TWO * debt >= cv && posted_spread > TARGET_SPREAD_CAP_PPM {
        return TARGET_SPREAD_CAP_PPM;
    }
    posted_spread
}

/// `_deleverageProRata` (`:450-501`) -> `(out, effSpread)`: the cap is charged only on collateral
/// released while the fill is at or below target, i.e. while `3 * debt >= anchor`, and the posted
/// spread on the rest. The split is at `T = (anchor + 2) / 3` with
/// `collAtTarget = ceil(cvRequired(anchor, T) * WAD / price)`; each leg floors separately and
/// `effSpread = posted - floor((posted - cap) * grossCapped / outGross)` feeds the strict
/// post-anchor gate.
#[allow(clippy::too_many_arguments)]
pub fn deleverage_pro_rata(
    anchor: U256,
    collateral: U256,
    debt: U256,
    new_debt: U256,
    price: U256,
    out_gross: U256,
    posted_spread: U256,
) -> (U256, U256) {
    let posted = || (mul_div_floor(out_gross, PPM - posted_spread, PPM), posted_spread);
    // Nothing to blend: no cap configured, or the fill starts above target.
    if posted_spread <= TARGET_SPREAD_CAP_PPM || THREE * debt < anchor {
        return posted();
    }
    // Dust guard: sub-101-wei anchors pay the posted rate.
    if anchor < DUST_ANCHOR_FLOOR {
        return posted();
    }
    // The whole fill stays at or below target.
    if THREE * new_debt >= anchor {
        return (mul_div_floor(out_gross, PPM - TARGET_SPREAD_CAP_PPM, PPM), TARGET_SPREAD_CAP_PPM);
    }
    // The fill crosses out: split the release at the crossing debt T.
    let (ok, cv_at_target) = cv_required_on_anchor(anchor, (anchor + TWO) / THREE);
    if !ok {
        return posted();
    }
    let coll_at_target = mul_div_up(cv_at_target, WAD, price);
    if coll_at_target >= collateral {
        return posted();
    }
    let mut gross_capped = collateral - coll_at_target;
    if gross_capped > out_gross {
        gross_capped = out_gross;
    }
    let out = mul_div_floor(gross_capped, PPM - TARGET_SPREAD_CAP_PPM, PPM) +
        mul_div_floor(out_gross - gross_capped, PPM - posted_spread, PPM);
    // `out_gross` is positive from every caller; a zero here is a test driving the helper past its
    // domain, where Solidity's `/` would panic and the Go port answers zero.
    let discount = ((posted_spread - TARGET_SPREAD_CAP_PPM) * gross_capped)
        .checked_div(out_gross)
        .unwrap_or_default();
    (out, posted_spread - discount)
}

/// `_postStrictAnchorAccepted` (`:503-517`): the post-fill state must sit on the strict branch with
/// an anchor that does not fall (spread 0) or strictly rises (any spread).
pub fn post_strict_anchor_accepted(
    pre_anchor: U256,
    collateral: U256,
    debt: U256,
    price: U256,
    r_wad: U256,
    spread_ppm: U256,
) -> bool {
    let (marked, cv) = marked_value(collateral, price);
    if !marked {
        return false;
    }
    let post = strict_anchor(cv, debt, r_wad);
    if !post.ok {
        return false;
    }
    if spread_ppm.is_zero() {
        post.anchor >= pre_anchor
    } else {
        post.anchor > pre_anchor
    }
}

/// `_postAnyAnchorAccepted` (`:519-532`): as the strict gate, on the best-effort anchor.
pub fn post_any_anchor_accepted(
    pre_anchor: U256,
    collateral: U256,
    debt: U256,
    price: U256,
    r_wad: U256,
    spread_ppm: U256,
) -> bool {
    let (marked, cv) = marked_value(collateral, price);
    if !marked {
        return false;
    }
    let post_anchor = anchor_best_effort(cv, debt, r_wad);
    if spread_ppm.is_zero() {
        post_anchor >= pre_anchor
    } else {
        post_anchor > pre_anchor
    }
}

/// `_recoveryState` (`:534-562`): for `debt * H_WALL > cv * D_WALL` with `debt < cv`,
/// `y = isqrt(floor((debt*H_WALL - cv*D_WALL) * WAD^2 / (cv*(H_WALL-D_WALL))))`,
/// `wallCv = floor(cv(1-y)/(1+y))`, `wallDebt = floor(wallCv*D_WALL/H_WALL)`; the anchor is the
/// strict anchor at the wall and `baseX = debt + floor((cv-debt)*y/WAD)`.
pub fn recovery_state(cv: U256, debt: U256, r_wad: U256) -> RecoveryState {
    let mut recovery = RecoveryState::default();
    if r_wad != LEVERAGE_RATIO_WAD ||
        cv.is_zero() ||
        cv > MAX_INPUT ||
        debt > MAX_INPUT ||
        debt >= cv ||
        !product_gt(debt, H_WALL, cv, D_WALL)
    {
        return recovery;
    }
    // y^2 = (rho - rhoW) / (1 - rhoW), using the exact rational fixed wall DW/HW.
    let numerator = debt * H_WALL - cv * D_WALL;
    let denominator = cv * H_WALL_MINUS_D_WALL;
    let y = sqrt(mul_div_floor(numerator, WAD_SQUARED, denominator));
    if y.is_zero() || y >= WAD {
        return recovery;
    }
    recovery.y = y;
    recovery.wall_cv = mul_div_floor(cv, WAD - y, WAD + y);
    if recovery.wall_cv.is_zero() {
        return recovery;
    }
    recovery.wall_debt = mul_div_floor(recovery.wall_cv, D_WALL, H_WALL);
    if recovery.wall_debt >= debt {
        return recovery;
    }
    let wall = strict_anchor(recovery.wall_cv, recovery.wall_debt, r_wad);
    if !wall.ok || wall.anchor.is_zero() {
        return recovery;
    }
    recovery.anchor = wall.anchor;
    recovery.base_x = debt + mul_div_floor(cv - debt, y, WAD);
    recovery.stable_to_wall = debt - recovery.wall_debt;
    recovery.ok = true;
    recovery
}

/// `anchorBestEffort(cv, debt, rWad)` (`:127-136`): the strict anchor, else the recovery anchor of
/// a solvent missed-wall state, else zero.
pub fn anchor_best_effort(cv: U256, debt: U256, r_wad: U256) -> U256 {
    let strict = strict_anchor(cv, debt, r_wad);
    if strict.ok {
        return strict.anchor;
    }
    let recovery = recovery_state(cv, debt, r_wad);
    if recovery.ok {
        return recovery.anchor;
    }
    U256::ZERO
}

/// `isStateSafe` (`:576-592`): a normal or recovery state whose anchor meets `required_x_anchor`.
pub fn is_state_safe(
    collateral: U256,
    debt: U256,
    price: U256,
    required_x_anchor: U256,
    r_wad: U256,
) -> bool {
    if r_wad != LEVERAGE_RATIO_WAD {
        return false;
    }
    let (marked, cv) = marked_value(collateral, price);
    if !marked {
        return false;
    }
    if cv.is_zero() {
        return collateral.is_zero() && debt.is_zero() && required_x_anchor.is_zero();
    }
    let strict = strict_anchor(cv, debt, r_wad);
    if strict.ok {
        return !strict.anchor.is_zero() && strict.anchor >= required_x_anchor;
    }
    let recovery = recovery_state(cv, debt, r_wad);
    recovery.ok && recovery.anchor >= required_x_anchor
}

/// `anchorAndBase` (`:147-168`) -> `(xAnchor, baseX)`: the value anchor and the stable-value
/// marginal numerator, `(anchor + debt) / 2` on the half law and `floor(cv * phi(h) / WAD)` on the
/// Hermite piece; a missed-wall state reports its recovery pair and anything else `(0, 0)`.
pub fn anchor_and_base(collateral: U256, debt: U256, price: U256, r_wad: U256) -> (U256, U256) {
    let (marked, cv) = marked_value(collateral, price);
    if !marked {
        return (U256::ZERO, U256::ZERO);
    }
    let strict = strict_anchor(cv, debt, r_wad);
    if strict.ok {
        if strict.anchor.is_zero() {
            return (U256::ZERO, U256::ZERO);
        }
        if strict.half_law {
            return (strict.anchor, (strict.anchor + debt) / TWO);
        }
        return (strict.anchor, mul_div_floor(cv, phi_wad(strict.h), WAD));
    }
    let recovery = recovery_state(cv, debt, r_wad);
    if recovery.ok {
        return (recovery.anchor, recovery.base_x);
    }
    (U256::ZERO, U256::ZERO)
}

/// `_phiWad` (`:594-598`): `WAD^2 / isqrt(h*WAD)` on the half law, the cubic Bezier at
/// `x = floor((h - H_JOIN) * WAD / WIDTH)` on the transition. A zero root (`h == 0`, off every
/// caller's domain) answers zero where Solidity would divide by zero, as the Go port does.
pub fn phi_wad(h: U256) -> U256 {
    if h <= H_JOIN {
        let root = sqrt(h * WAD);
        return WAD_SQUARED
            .checked_div(root)
            .unwrap_or_default();
    }
    bezier3(mul_div_floor(h - H_JOIN, WAD, WIDTH))
}

/// `_debtNormWad` (`:600-604`): `2*isqrt(h*WAD) - 1.5 WAD` on the half law,
/// `D_JOIN + floor(WIDTH * B4(x) / WAD)` on the transition. Callers only reach the half-law piece
/// at `h >= H_JOIN`, where the subtraction cannot wrap.
pub fn debt_norm_wad(h: U256) -> U256 {
    if h <= H_JOIN {
        return TWO * sqrt(h * WAD) - HALF_LAW_OFFSET;
    }
    let x = mul_div_floor(h - H_JOIN, WAD, WIDTH);
    D_JOIN + mul_div_floor(WIDTH, bezier4(x), WAD)
}

/// `_lerpFloor` (`:607-610`): `floor(a(1-x) + b*x)` at WAD scale, ceiling the step on a descending
/// leg.
pub fn lerp_floor(a: U256, b: U256, x: U256) -> U256 {
    if b >= a {
        return a + mul_div_floor(b - a, x, WAD);
    }
    a - mul_div_up(a - b, x, WAD)
}

/// `_bezier3` (`:612-619`), de Casteljau over `P0..P3`.
pub fn bezier3(x: U256) -> U256 {
    let a0 = lerp_floor(P0, P1, x);
    let a1 = lerp_floor(P1, P2, x);
    let a2 = lerp_floor(P2, P3, x);
    let b0 = lerp_floor(a0, a1, x);
    let b1 = lerp_floor(a1, a2, x);
    lerp_floor(b0, b1, x)
}

/// `_bezier4` (`:621-632`), de Casteljau over `Q0 = 0, Q1..Q4`.
pub fn bezier4(x: U256) -> U256 {
    let a0 = lerp_floor(U256::ZERO, Q1, x);
    let a1 = lerp_floor(Q1, Q2, x);
    let a2 = lerp_floor(Q2, Q3, x);
    let a3 = lerp_floor(Q3, Q4, x);
    let b0 = lerp_floor(a0, a1, x);
    let b1 = lerp_floor(a1, a2, x);
    let b2 = lerp_floor(a2, a3, x);
    let c0 = lerp_floor(b0, b1, x);
    let c1 = lerp_floor(b1, b2, x);
    lerp_floor(c0, c1, x)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(v: u64) -> U256 {
        U256::from(v)
    }

    #[test]
    fn constants() {
        assert_eq!(MAX_INPUT, U256::from(10u64).pow(u(38)));
        assert_eq!(LEVERAGE_RATIO_WAD, u(4) * WAD / u(9));
        assert_eq!(H_ZERO, u(9) * WAD / u(16));
        assert_eq!(WIDTH, H_WALL - H_JOIN);
        assert_eq!(RHO_WALL_NUMERATOR, D_WALL * WAD_SQUARED);
        assert_eq!(RHO_DENOMINATOR, H_WALL * WAD_SQUARED);
        assert_eq!(H_WALL_MINUS_D_WALL, H_WALL - D_WALL);
        assert_eq!(THREE_WAD, u(3) * WAD);
        assert_eq!(HALF_LAW_OFFSET, u(3) * WAD / u(2));
        assert_eq!(WAD_MINUS_ONE, WAD - u(1));
        // The frozen curve is C0 at the join: both pieces of D(h) agree there to the wei.
        assert_eq!(debt_norm_wad(H_JOIN), TWO * sqrt(H_JOIN * WAD) - HALF_LAW_OFFSET);
        assert_eq!(bezier4(WAD), Q4);
        assert_eq!(bezier3(U256::ZERO), P0);
        assert_eq!(bezier3(WAD), P3);
    }

    #[test]
    fn genesis_state_is_on_the_half_law() {
        // CR 2.0 at genesis: cv = 2, debt = 1 (WAD scale) sits on the half law, where
        // 3(B + d)^2 = 8B cv reads 3B^2 - 10B + 3 = 0 with upper root B = 3.
        let cv = u(2) * WAD;
        let debt = WAD;
        let a = strict_anchor(cv, debt, LEVERAGE_RATIO_WAD);
        assert!(a.ok && a.half_law);
        assert_eq!(a.anchor, u(3) * WAD);
        // An unlevered mark: 3B^2 = 8B cv, B = 8cv/3.
        assert_eq!(
            strict_anchor(WAD, U256::ZERO, LEVERAGE_RATIO_WAD).anchor,
            u(2_666_666_666_666_666_666)
        );
        assert_eq!(
            anchor_and_base(cv, debt, WAD, LEVERAGE_RATIO_WAD),
            (a.anchor, (a.anchor + debt) / TWO)
        );
        assert!(is_state_safe(cv, debt, WAD, a.anchor, LEVERAGE_RATIO_WAD));
        assert!(!is_state_safe(cv, debt, WAD, a.anchor + ONE, LEVERAGE_RATIO_WAD));
        assert_eq!(strict_anchor(cv, debt, WAD), StrictAnchor::default());
    }

    #[test]
    fn refusals_leave_inputs_unchanged() {
        let q = leverage_quote(WAD, WAD, WAD, LEVERAGE_RATIO_WAD, PPM, WAD);
        assert_eq!(q, Quote::refused(WAD, WAD));
        let q = deleverage_quote(WAD, WAD, WAD, LEVERAGE_RATIO_WAD, u(1), WAD + ONE);
        assert_eq!(q, Quote::refused(WAD, WAD));
        assert_eq!(
            recovery_state(U256::ZERO, U256::ZERO, LEVERAGE_RATIO_WAD),
            RecoveryState::default()
        );
    }

    #[test]
    fn out_of_domain_helpers_do_not_panic() {
        assert_eq!(phi_wad(U256::ZERO), U256::ZERO);
        let _ = debt_norm_wad(U256::ZERO);
        let _ = debt_cap_on_anchor(U256::ZERO, WAD);
        let _ = cv_required_on_anchor(U256::ZERO, WAD);
        let _ = deleverage_pro_rata(WAD, WAD, WAD, U256::ZERO, WAD, U256::ZERO, u(20_000));
        let _ = recovery_debt_at_y(WAD, WAD);
        let _ = half_law_anchor(U256::MAX, U256::MAX);
    }
}
