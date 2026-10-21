use num_bigint::BigInt;
use std::ops::Shr;

/// Calculates the amounts of token0 and token1 for a given position
///
/// Source: https://github.com/Uniswap/v4-core/blob/main/src/libraries/Pool.sol
/// Function: modifyLiquidity
///
/// # Arguments
/// * `current_sqrt_price` - Current square root price
/// * `tick_lower` - Lower tick of the position
/// * `tick_upper` - Upper tick of the position
/// * `liquidity_delta` - Amount of liquidity to add/remove
///
/// # Returns
/// * `Result<(BigInt, BigInt), String>` - Token amounts (amount0, amount1)
pub fn calculate_token_amounts(
    current_sqrt_price: BigInt,
    tick_lower: i32,
    tick_upper: i32,
    liquidity_delta: i128,
) -> Result<(BigInt, BigInt), String> {
    let sqrt_price_lower_x96: BigInt = get_sqrt_ratio_at_tick(tick_lower)?;
    let sqrt_price_upper_x96: BigInt = get_sqrt_ratio_at_tick(tick_upper)?;

    // Calculate amounts based on current price relative to the range
    let (amount0, amount1) = if current_sqrt_price < sqrt_price_lower_x96 {
        // Current price is below the range: position in token0
        let amount0 =
            get_amount_0_delta_signed(sqrt_price_lower_x96, sqrt_price_upper_x96, liquidity_delta)?;
        (amount0, BigInt::from(0))
    } else if current_sqrt_price < sqrt_price_upper_x96 {
        // Current price is within the range: position in both tokens
        let amount0 = get_amount_0_delta_signed(
            current_sqrt_price.clone(),
            sqrt_price_upper_x96,
            liquidity_delta,
        )?;

        let amount1 =
            get_amount_1_delta_signed(sqrt_price_lower_x96, current_sqrt_price, liquidity_delta)?;

        (amount0, amount1)
    } else {
        // Current price is above the range: position in token1
        let amount1 =
            get_amount_1_delta_signed(sqrt_price_lower_x96, sqrt_price_upper_x96, liquidity_delta)?;

        (BigInt::from(0), amount1)
    };

    Ok((amount0, amount1))
}

const MAX_TICK: i32 = 887272;

/// Returns the sqrt ratio as a Q64.96 for the given tick. The sqrt ratio is computed as
/// sqrt(1.0001)^tick
/// Adapted from: https://github.com/shuhuiluo/uniswap-v3-sdk-rs/blob/v2.9.1/src/utils/tick_math.rs#L57
fn get_sqrt_ratio_at_tick(tick: i32) -> Result<BigInt, String> {
    let abs_tick = tick.abs();

    if abs_tick > MAX_TICK {
        return Err("Tick out of bounds".to_string());
    }

    // Initialize ratio with either 2^128 / sqrt(1.0001) or 2^128
    let mut ratio = if abs_tick & 0x1 != 0 {
        BigInt::parse_bytes(b"fffcb933bd6fad37aa2d162d1a594001", 16).unwrap()
    } else {
        BigInt::from(1) << 128
    };

    if abs_tick & 0x2 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"fff97272373d413259a46990580e213a", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x4 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"fff2e50f5f656932ef12357cf3c7fdcc", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x8 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"ffe5caca7e10e4e61c3624eaa0941cd0", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x10 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"ffcb9843d60f6159c9db58835c926644", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x20 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"ff973b41fa98c081472e6896dfb254c0", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x40 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"ff2ea16466c96a3843ec78b326b52861", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x80 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"fe5dee046a99a2a811c461f1969c3053", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x100 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"fcbe86c7900a88aedcffc83b479aa3a4", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x200 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"f987a7253ac413176f2b074cf7815e54", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x400 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"f3392b0822b70005940c7a398e4b70f3", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x800 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"e7159475a2c29b7443b29c7fa6e889d9", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x1000 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"d097f3bdfd2022b8845ad8f792aa5825", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x2000 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"a9f746462d870fdf8a65dc1f90e061e5", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x4000 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"70d869a156d2a1b890bb3df62baf32f7", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x8000 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"31be135f97d08fd981231505542fcfa6", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x10000 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"9aa508b5b7a84e1c677de54f3e99bc9", 16).unwrap())
            .shr(128);
    }
    if abs_tick & 0x20000 != 0 {
        ratio =
            (&ratio * BigInt::parse_bytes(b"5d6af8dedb81196699c329225ee604", 16).unwrap()).shr(128);
    }
    if abs_tick & 0x40000 != 0 {
        ratio =
            (&ratio * BigInt::parse_bytes(b"2216e584f5fa1ea926041bedfe98", 16).unwrap()).shr(128);
    }
    if abs_tick & 0x80000 != 0 {
        ratio = (&ratio * BigInt::parse_bytes(b"48a170391f7dc42444e8fa2", 16).unwrap()).shr(128);
    }

    if tick > 0 {
        let max = (BigInt::from(1) << 256) - 1;
        ratio = max / ratio;
    }
    // Add 2^32 - 1 and shift right by 32
    ratio = (ratio + ((BigInt::from(1) << 32) - 1)) >> 32;

    Ok(ratio)
}

/// Helper that gets signed token0 delta
/// Source: https://github.com/shuhuiluo/uniswap-v3-sdk-rs/blob/v2.9.1/src/utils/sqrt_price_math.rs#L422
///
/// ## Arguments
///
/// * `sqrt_ratio_a_x96`: A sqrt price
/// * `sqrt_ratio_b_x96`: Another sqrt price
/// * `liquidity`: The change in liquidity for which to compute the amount0 delta
///
/// ## Returns
///
/// Amount of token0 corresponding to the passed liquidityDelta between the two prices
fn get_amount_0_delta_signed(
    sqrt_ratio_a_x96: BigInt,
    sqrt_ratio_b_x96: BigInt,
    liquidity: i128,
) -> Result<BigInt, String> {
    let sign = !liquidity.is_negative();
    // Create mask for negative numbers
    let mask = if sign { 0u128 } else { u128::MAX };

    // Get absolute value of liquidity using XOR and addition
    let liquidity = mask ^ mask.wrapping_add_signed(liquidity);

    // Convert mask to BigInt (all 1s or all 0s)
    let mask = if sign { BigInt::from(0) } else { -BigInt::from(1) };

    let amount_0 = get_amount_0_delta(sqrt_ratio_a_x96, sqrt_ratio_b_x96, liquidity, sign)?;

    // Apply the mask using XOR and subtraction to restore the sign
    Ok((amount_0 ^ &mask) - mask)
}

/// Gets the amount0 delta between two prices
///
/// Calculates liquidity / sqrt(lower) - liquidity / sqrt(upper),
/// i.e. liquidity * (sqrt(upper) - sqrt(lower)) / (sqrt(upper) * sqrt(lower))
///
/// ## Arguments
///
/// * `sqrt_ratio_a_x96`: A sqrt price assumed to be lower otherwise swapped
/// * `sqrt_ratio_b_x96`: Another sqrt price
/// * `liquidity`: The amount of usable liquidity
/// * `round_up`: Whether to round the amount up or down
///
/// ## Returns
///
/// Amount of token0 required to cover a position of size liquidity between the two passed prices
fn get_amount_0_delta(
    sqrt_ratio_a_x96: BigInt,
    sqrt_ratio_b_x96: BigInt,
    liquidity: u128,
    round_up: bool,
) -> Result<BigInt, String> {
    let (sqrt_ratio_a_x96, sqrt_ratio_b_x96) = if sqrt_ratio_a_x96 < sqrt_ratio_b_x96 {
        (sqrt_ratio_a_x96, sqrt_ratio_b_x96)
    } else {
        (sqrt_ratio_b_x96, sqrt_ratio_a_x96)
    };

    if sqrt_ratio_a_x96 == BigInt::from(0) {
        return Err("Price cannot be zero".to_string());
    }

    let numerator_1 = BigInt::from(liquidity) << 96;
    let numerator_2 = &sqrt_ratio_b_x96 - &sqrt_ratio_a_x96;

    if round_up {
        // For rounding up: ceil(ceil(numerator_1 * numerator_2 / sqrt_ratio_b_x96) /
        // sqrt_ratio_a_x96)
        let temp =
            (&numerator_1 * &numerator_2 + &sqrt_ratio_b_x96 - BigInt::from(1)) / &sqrt_ratio_b_x96;
        Ok((&temp + &sqrt_ratio_a_x96 - BigInt::from(1)) / sqrt_ratio_a_x96)
    } else {
        // For rounding down: floor(floor(numerator_1 * numerator_2 / sqrt_ratio_b_x96) /
        // sqrt_ratio_a_x96)
        Ok((&numerator_1 * &numerator_2) / &sqrt_ratio_b_x96 / sqrt_ratio_a_x96)
    }
}

const Q96: u128 = 1 << 96;

/// Helper that gets signed token1 delta
///
/// ## Arguments
///
/// * `sqrt_ratio_a_x96`: A sqrt price
/// * `sqrt_ratio_b_x96`: Another sqrt price
/// * `liquidity`: The change in liquidity for which to compute the amount1 delta
///
/// ## Returns
///
/// Amount of token1 corresponding to the passed liquidityDelta between the two prices
fn get_amount_1_delta_signed(
    sqrt_ratio_a_x96: BigInt,
    sqrt_ratio_b_x96: BigInt,
    liquidity: i128,
) -> Result<BigInt, String> {
    let sign = !liquidity.is_negative();

    // Create mask for negative numbers
    let mask = if sign { 0u128 } else { u128::MAX };

    // Get absolute value of liquidity using XOR and addition
    let liquidity = mask ^ mask.wrapping_add_signed(liquidity);

    // Convert mask to BigInt (all 1s or all 0s)
    let mask = if sign { BigInt::from(0) } else { -BigInt::from(1) };

    let amount_1 = get_amount_1_delta(sqrt_ratio_a_x96, sqrt_ratio_b_x96, liquidity, sign)?;

    // Apply the mask using XOR and subtraction to restore the sign
    Ok((amount_1 ^ &mask) - mask)
}

/// Gets the amount1 delta between two prices
///
/// Calculates liquidity * (sqrt(upper) - sqrt(lower))
///
/// ## Arguments
///
/// * `sqrt_ratio_a_x96`: A sqrt price assumed to be lower otherwise swapped
/// * `sqrt_ratio_b_x96`: Another sqrt price
/// * `liquidity`: The amount of usable liquidity
/// * `round_up`: Whether to round the amount up, or down
///
/// ## Returns
///
/// Amount of token1 required to cover a position of size liquidity between the two passed prices
fn get_amount_1_delta(
    sqrt_ratio_a_x96: BigInt,
    sqrt_ratio_b_x96: BigInt,
    liquidity: u128,
    round_up: bool,
) -> Result<BigInt, String> {
    let (sqrt_ratio_a_x96, sqrt_ratio_b_x96) = if sqrt_ratio_a_x96 < sqrt_ratio_b_x96 {
        (sqrt_ratio_a_x96, sqrt_ratio_b_x96)
    } else {
        (sqrt_ratio_b_x96, sqrt_ratio_a_x96)
    };

    let numerator = &sqrt_ratio_b_x96 - &sqrt_ratio_a_x96;
    let denominator = BigInt::from(Q96);

    let liquidity = BigInt::from(liquidity);
    let amount_1 = &liquidity * &numerator / &denominator;

    // Calculate if there's a remainder
    let remainder = (&liquidity * &numerator) % &denominator;
    let carry = remainder > BigInt::from(0) && round_up;

    Ok(if carry { amount_1 + 1 } else { amount_1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigInt;
    use rstest::rstest;
    use std::str::FromStr;
    const MIN_TICK: i32 = -887272;

    #[test]
    fn test_get_sqrt_ratio_at_tick_is_valid_min_tick() {
        assert_eq!(get_sqrt_ratio_at_tick(MIN_TICK).unwrap(), BigInt::from(4295128739_u128));
    }

    #[test]
    fn test_get_sqrt_ratio_at_tick_is_valid_min_tick_add_one() {
        assert_eq!(get_sqrt_ratio_at_tick(MIN_TICK + 1).unwrap(), BigInt::from(4295343490_u128));
    }

    #[test]
    fn test_get_sqrt_ratio_at_tick_is_valid_max_tick() {
        assert_eq!(
            get_sqrt_ratio_at_tick(MAX_TICK).unwrap(),
            BigInt::from_str("1461446703485210103287273052203988822378723970342").unwrap()
        );
    }

    #[test]
    fn test_get_sqrt_ratio_at_tick_is_valid_max_tick_sub_one() {
        assert_eq!(
            get_sqrt_ratio_at_tick(MAX_TICK - 1).unwrap(),
            BigInt::from_str("1461373636630004318706518188784493106690254656249").unwrap()
        );
    }

    #[test]
    fn test_get_sqrt_ratio_at_tick_is_valid_tick_zero() {
        assert_eq!(
            get_sqrt_ratio_at_tick(0).unwrap(),
            BigInt::from_str("79228162514264337593543950336").unwrap()
        );
    }

    #[test]
    fn test_get_sqrt_ratio_at_tick_is_less_than_js_impl_min_tick() {
        let js_min_sqrt_price = BigInt::from(6085630636u64);
        let sol_min_sqrt_price = get_sqrt_ratio_at_tick(MIN_TICK).unwrap();
        assert!(sol_min_sqrt_price < js_min_sqrt_price);
    }

    #[test]
    fn test_get_sqrt_ratio_at_tick_is_greater_than_js_impl_max_tick() {
        let js_max_sqrt_price =
            BigInt::from_str("1033437718471923706666374484006904511252097097914").unwrap();
        let sol_max_sqrt_price = get_sqrt_ratio_at_tick(MAX_TICK).unwrap();
        assert!(sol_max_sqrt_price > js_max_sqrt_price);
    }
    #[test]
    fn test_get_amount_0_delta_returns_0_if_liquidity_is_0() {
        let result = get_amount_0_delta(
            BigInt::from_str("79228162514264337593543950336").unwrap(),
            BigInt::from_str("112045541949572279837463876454").unwrap(),
            0,
            true,
        )
        .unwrap();
        assert_eq!(result, BigInt::from(0));
    }

    #[test]
    fn test_get_amount_0_delta_returns_0_if_prices_are_equal() {
        let result = get_amount_0_delta(
            BigInt::from_str("79228162514264337593543950336").unwrap(),
            BigInt::from_str("79228162514264337593543950336").unwrap(),
            0,
            true,
        )
        .unwrap();
        assert_eq!(result, BigInt::from(0));
    }

    #[test]
    fn test_get_amount_0_delta_reverts_if_price_is_zero() {
        let result = get_amount_0_delta(BigInt::from(0), BigInt::from(1), 1, true);
        assert!(result.is_err());
    }

    #[test]
    fn test_get_amount_0_delta_1_amount_1_for_price_of_1_to_1_21() {
        let sqrt_price_1_1 = BigInt::from_str("79228162514264337593543950336").unwrap();
        let sqrt_price_121_100 = BigInt::from_str("87150978765690771352898345369").unwrap();

        let amount_0 = get_amount_0_delta(
            sqrt_price_1_1.clone(),
            sqrt_price_121_100.clone(),
            1_000_000_000_000_000_000,
            true,
        )
        .unwrap();
        assert_eq!(amount_0, BigInt::from(90_909_090_909_090_910u128));

        let amount_0_rounded_down = get_amount_0_delta(
            sqrt_price_1_1,
            sqrt_price_121_100,
            1_000_000_000_000_000_000,
            false,
        )
        .unwrap();
        assert_eq!(amount_0_rounded_down, amount_0 - 1);
    }

    #[test]
    fn test_get_amount_0_delta_works_for_prices_that_overflow() {
        let sqrt_p_1 = BigInt::from_str("2787593149816327892691964784081045188247552").unwrap();
        let sqrt_p_2 = BigInt::from_str("22300745198530623141535718272648361505980416").unwrap();

        let amount_0_up =
            get_amount_0_delta(sqrt_p_1.clone(), sqrt_p_2.clone(), 1_000_000_000_000_000_000, true)
                .unwrap();
        let amount_0_down =
            get_amount_0_delta(sqrt_p_1, sqrt_p_2, 1_000_000_000_000_000_000, false).unwrap();

        assert_eq!(amount_0_up, amount_0_down + 1);
    }

    #[rstest]
    #[case(
        BigInt::from(79228162514264337593543950336_i128),
        -887270,
        887270,
        2779504125,
        BigInt::from(2779504125_u128),
        BigInt::from(2779504125_u128)
    )]
    #[case(
        BigInt::from(17189630842187678489986852982138_i128),
        -138200,
        107600,
        -79381057257465377800177,
        BigInt::from(-445577797388351_i128),
        BigInt::from(-17222724212376326765220041_i128)
    )]
    fn test_calculate_token_amounts(
        #[case] current_sqrtprice: BigInt,
        #[case] tick_lower: i32,
        #[case] tick_upper: i32,
        #[case] liquidity_delta: i128,
        #[case] expected_amount0: BigInt,
        #[case] expected_amount1: BigInt,
    ) {
        let (amount0, amount1) =
            calculate_token_amounts(current_sqrtprice, tick_lower, tick_upper, liquidity_delta)
                .unwrap();

        assert_eq!(amount0, expected_amount0);
        assert_eq!(amount1, expected_amount1);
    }
}
