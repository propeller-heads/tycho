//! SY (ERC-5115) profiling and per-token conversion classification.
//!
//! Every SY wraps a different protocol, so there is no single deposit/redeem formula. What is
//! generic is the *evidence*: `previewDeposit` / `previewRedeem` are pure views, so one probe of
//! a single token unit at creation says which of two closed-form conversions — if either — the
//! SY implements for that token. Tokens matching neither are left out of the component, which
//! is what makes an unquotable SY edge absent rather than silently wrong.
//!
//! Classification is per `(SY, token)` pair, not per SY: wrappers commonly accept their base
//! token 1:1 *and* the accounting asset at the index rate.

use substreams::scalar::BigInt;

/// Which closed-form conversion an SY applies to one specific token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenClass {
    /// One token unit is one SY unit, up to a decimal rescale.
    OneToOne,
    /// The token is treated as the accounting asset and converted at `exchangeRate()`.
    IndexRate,
}

impl TokenClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenClass::OneToOne => "one_to_one",
            TokenClass::IndexRate => "index_rate",
        }
    }
}

/// Fixed-point scale of `SY.exchangeRate()`. The rate is always 1e18-scaled even where the SY
/// and its accounting asset have different decimals — the decimal gap is absorbed into the rate
/// itself, so it must never be treated as `10^sy_decimals`.
const RATE_ONE: u64 = 1_000_000_000_000_000_000;

/// Relative tolerance for matching a probe against a prediction, as a reciprocal: a prediction is
/// accepted when it is within 1/1e6 of the observed value.
///
/// The bound is set by the spread the SYs charge on entry, not by rounding. Swept over the 46 live
/// SYs at one block, every `previewRedeem` probe matches its prediction to the unit, and the
/// `previewDeposit` probes that do not split in two: one or two wei where the SY converts through
/// the wrapped protocol's own floor-divide first, and seven SYs taking a real 1e-7 to 1e-6 haircut
/// on entry. Those seven are what a wei-level tolerance would drop. It stays far under a haircut
/// large enough to mean the SY is doing something else entirely (0.5% on `0x457904b5...59d6`), so
/// a token matching neither closed form is still excluded rather than rounded into the nearer
/// class, and the two predictions never both match unless they are arithmetically identical.
const TOLERANCE_RECIPROCAL: u64 = 1_000_000;

pub fn pow10(exponent: u32) -> BigInt {
    BigInt::from(10).pow(exponent)
}

/// True when `observed` is within the relative tolerance of `predicted`.
fn matches(observed: &BigInt, predicted: &BigInt) -> bool {
    if predicted.clone() == BigInt::zero() {
        return false;
    }
    let difference = if observed > predicted {
        observed.clone() - predicted.clone()
    } else {
        predicted.clone() - observed.clone()
    };
    difference * BigInt::from(TOLERANCE_RECIPROCAL) <= predicted.clone()
}

/// Picks the class whose prediction matches the probe.
///
/// `OneToOne` is only asserted where the two predictions genuinely diverge. Where they coincide
/// — an SY whose `exchangeRate` currently sits at exactly one unit — the evidence cannot
/// separate them, and the tie goes to `IndexRate`: it *is* the identity while the rate is one,
/// and it stays correct once the rate moves away. Freezing such an SY as `OneToOne` would leave
/// the label right today and quietly wrong by `rate - 1` forever after.
fn classify(observed: &BigInt, one_to_one: &BigInt, index_rate: &BigInt) -> Option<TokenClass> {
    if matches(observed, one_to_one) && !matches(one_to_one, index_rate) {
        return Some(TokenClass::OneToOne);
    }
    if matches(observed, index_rate) {
        return Some(TokenClass::IndexRate);
    }
    None
}

/// Classifies a `tokensIn` entry from a `previewDeposit` probe of one whole token unit.
///
/// `observed` must be the SY-unit output the SY predicts for exactly `10^token_decimals` of the
/// token. Both predictions are then independent of the token's own decimals, which cancel
/// against the probe amount.
pub fn classify_deposit(
    observed: &BigInt,
    sy_decimals: u32,
    asset_decimals: u32,
    exchange_rate: &BigInt,
) -> Option<TokenClass> {
    if exchange_rate.clone() == BigInt::zero() {
        return None;
    }
    let one_to_one = pow10(sy_decimals);
    // The token enters as the accounting asset, then converts to SY: `assetToSy` divides by the
    // rate, and the rate carries the sy/asset decimal gap, so the quotient is already in SY
    // units.
    let index_rate = pow10(asset_decimals) * BigInt::from(RATE_ONE) / exchange_rate.clone();
    classify(observed, &one_to_one, &index_rate)
}

/// Classifies a `tokensOut` entry from a `previewRedeem` probe of one whole SY unit.
///
/// `observed` is the token output the SY predicts for `10^sy_decimals` of itself.
pub fn classify_redeem(
    observed: &BigInt,
    token_decimals: u32,
    sy_decimals: u32,
    asset_decimals: u32,
    exchange_rate: &BigInt,
) -> Option<TokenClass> {
    if exchange_rate.clone() == BigInt::zero() {
        return None;
    }
    let one_to_one = pow10(token_decimals);
    // Mirror of the deposit direction: `syToAsset` multiplies by the rate, leaving asset-decimal
    // units, which are then rescaled to the token's own decimals.
    let asset_amount = pow10(sy_decimals) * exchange_rate.clone() / BigInt::from(RATE_ONE);
    let index_rate = asset_amount * pow10(token_decimals) / pow10(asset_decimals);
    classify(observed, &one_to_one, &index_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixture below is a value read from Ethereum mainnet, not one computed from this
    /// module's own constants. That matters: predictions derived from `RATE_ONE` would agree
    /// with a wrongly-scaled `RATE_ONE` and the tests would pass while every quote was wrong.
    ///
    /// Every fixture within one module is a single-block snapshot: the rate and the previews
    /// must come from the same block, or the rate accrues between reads and the prediction is
    /// compared against a probe taken under a different rate.
    ///
    /// SY-wstETH `0xcbc72d92...c0bc` at block 25803485 -- 18-decimal SY over 18-decimal stETH.
    mod wsteth_sy {
        pub const SY_DECIMALS: u32 = 18;
        pub const ASSET_DECIMALS: u32 = 18;
        pub const EXCHANGE_RATE: u64 = 1_242_190_142_783_145_750;
        /// `previewDeposit(wstETH, 1e18)` -- the SY's own wrapper token, taken at par.
        pub const DEPOSIT_WSTETH: u64 = 1_000_000_000_000_000_000;
        /// `previewDeposit(stETH, 1e18)` -- the accounting asset, converted at the rate.
        pub const DEPOSIT_STETH: u64 = 805_029_733_821_172_431;
        /// `previewRedeem(1e18)` to stETH.
        pub const REDEEM_STETH: u64 = 1_242_190_142_783_145_750;
        /// `previewRedeem(1e18)` to wstETH.
        pub const REDEEM_WSTETH: u64 = 1_000_000_000_000_000_000;
    }

    /// SY `0x457904b5...59d6` at block 25813586 -- 18-decimal SY over 6-decimal USDC. The
    /// decimal gap lives in the rate, which is why the rate is ~1.12e6 rather than ~1.12e18.
    mod usdc_sy {
        pub const SY_DECIMALS: u32 = 18;
        pub const ASSET_DECIMALS: u32 = 6;
        pub const USDC_DECIMALS: u32 = 6;
        pub const EXCHANGE_RATE: u64 = 1_121_519;
        /// `previewDeposit(USDC, 1e6)`.
        pub const DEPOSIT_USDC: u64 = 891_647_606_956_028_309;
        /// `previewRedeem(1e18)` to USDC. Below the rate: this SY takes a haircut on exit, so
        /// neither closed form explains it.
        pub const REDEEM_USDC: u64 = 1_115_911;
    }

    /// SY-siUSD `0x9f30507c...a920` at block 25810474 -- 18-decimal SY over an 18-decimal asset
    /// that also redeems to 6-decimal USDC. The only live shape where the exit token's decimals
    /// differ from the accounting asset's, so it is the only fixture that exercises the redeem
    /// rescale.
    mod cross_decimal_sy {
        pub const SY_DECIMALS: u32 = 18;
        pub const ASSET_DECIMALS: u32 = 18;
        pub const USDC_DECIMALS: u32 = 6;
        pub const EXCHANGE_RATE: u64 = 1_084_785_195_710_495_612;
        /// `previewRedeem(1e18)` to USDC.
        pub const REDEEM_USDC: u64 = 1_084_785;
    }

    /// A real wrapper taken at par, where the two predictions are far apart -- the only case in
    /// which `OneToOne` is provable.
    #[test]
    fn wsteth_deposit_of_wrapper_token_is_one_to_one() {
        assert_eq!(
            classify_deposit(
                &BigInt::from(wsteth_sy::DEPOSIT_WSTETH),
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::OneToOne)
        );
    }

    /// The same SY converts its accounting asset at the rate. The observed value sits one wei
    /// above the exact quotient, so this also pins the tolerance.
    #[test]
    fn wsteth_deposit_of_accounting_asset_is_index_rate() {
        assert_eq!(
            classify_deposit(
                &BigInt::from(wsteth_sy::DEPOSIT_STETH),
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
    }

    #[test]
    fn wsteth_redeem_splits_by_token() {
        assert_eq!(
            classify_redeem(
                &BigInt::from(wsteth_sy::REDEEM_STETH),
                wsteth_sy::ASSET_DECIMALS,
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
        assert_eq!(
            classify_redeem(
                &BigInt::from(wsteth_sy::REDEEM_WSTETH),
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::OneToOne)
        );
    }

    /// The decimal axis: an 18-decimal SY over a 6-decimal asset. Fails if the index is treated
    /// as `10^sy_decimals` instead of 1e18.
    #[test]
    fn deposit_across_a_decimal_gap_is_index_rate() {
        assert_eq!(
            classify_deposit(
                &BigInt::from(usdc_sy::DEPOSIT_USDC),
                usdc_sy::SY_DECIMALS,
                usdc_sy::ASSET_DECIMALS,
                &BigInt::from(usdc_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
    }

    /// A real exit that neither closed form explains -- this SY redeems ~0.5% below the rate.
    /// It must be excluded, not rounded into the nearer class.
    #[test]
    fn redeem_with_a_haircut_is_rejected() {
        assert_eq!(
            classify_redeem(
                &BigInt::from(usdc_sy::REDEEM_USDC),
                usdc_sy::USDC_DECIMALS,
                usdc_sy::SY_DECIMALS,
                usdc_sy::ASSET_DECIMALS,
                &BigInt::from(usdc_sy::EXCHANGE_RATE),
            ),
            None
        );
    }

    /// Redeeming an 18-decimal SY into a 6-decimal token: the asset-unit result has to be
    /// rescaled to the exit token's decimals. Without that rescale the prediction is off by
    /// twelve orders of magnitude and the token would be dropped as unquotable.
    #[test]
    fn redeem_rescales_to_the_exit_token_decimals() {
        assert_eq!(
            classify_redeem(
                &BigInt::from(cross_decimal_sy::REDEEM_USDC),
                cross_decimal_sy::USDC_DECIMALS,
                cross_decimal_sy::SY_DECIMALS,
                cross_decimal_sy::ASSET_DECIMALS,
                &BigInt::from(cross_decimal_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
    }

    /// A rate of exactly one unit makes both predictions identical, so creation-time evidence
    /// cannot separate them and the tie must go to `IndexRate`.
    #[test]
    fn rate_of_one_ties_to_index_rate() {
        let observed = pow10(18);
        let rate = BigInt::from(RATE_ONE);
        assert_eq!(classify_deposit(&observed, 18, 18, &rate), Some(TokenClass::IndexRate));
        assert_eq!(classify_redeem(&observed, 18, 18, 18, &rate), Some(TokenClass::IndexRate));
    }

    /// A zero rate is a paused or uninitialised SY, not a 1:1 wrapper.
    #[test]
    fn zero_rate_is_rejected() {
        let observed = pow10(18);
        assert_eq!(classify_deposit(&observed, 18, 18, &BigInt::zero()), None);
        assert_eq!(classify_redeem(&observed, 18, 18, 18, &BigInt::zero()), None);
    }
}
