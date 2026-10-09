// Copyright (c) 2026 Everlong Labs Limited

//! Reverts of the deployed c104 contracts (`80abd43`), one variant per modelled custom error, plus
//! the four non-custom reverts the quoting path can raise and the `require` strings of the Morpho
//! Blue singleton the financing account drives.
//!
//! Every function of the port returns [`FlammError`] exactly where the Solidity it ports reverts,
//! so a refusal maps 1:1 onto the revert a real fill would hit. The 53 custom errors are the
//! `error` signatures the quoting path can reach, declared under `src/core`, `src/factory`,
//! `src/hooks`, `src/interfaces` and `src/libraries` of the c104 tree: the curve's, the hooks',
//! the Router's, the account's, the gate's and the pool's. The c104 tree declares more, and the
//! owner / curator / keeper gates, the scheduling and initializer errors, the config validation,
//! the ERC20 errors, the hooks' caller checks (`NotPool()`) and the settlement's consistency
//! guards (`FeeMismatch()`, `FillMismatch()`, which a quote prices itself and so cannot fire)
//! among them are not modelled: nothing a quote evaluates can reach them, and no recorded
//! fixture revert carries their selectors.
//! [`FeedReverted`](FlammError::FeedReverted) is a Chainlink aggregator's `latestRoundData`
//! reverting under `PriceFeed`, whose own revert data bubbles and is not modelled.
//!
//! [`FlammError::from_revert_data`] classifies raw revert data the way the fixture generators
//! recorded it: empty data is OpenZeppelin 4.8 `Math.mulDiv`'s bare `require(denominator > prod1)`;
//! a 36-byte `Panic(uint256)` is a Solidity panic (0x11 checked arithmetic, 0x12 division by zero,
//! 0x32 index out of bounds); an `Error(string)` payload is one of Morpho Blue's `ErrorsLib`
//! strings; anything else is read as a custom-error selector followed by whatever arguments the
//! error declares, so a parameterised error classifies at the length the chain reverts with.

use std::fmt;

/// A revert of the deployed pool, its hooks, its Router, its financing account or its price feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FlammError {
    /// `BadLoanIndex()`
    BadLoanIndex,
    /// `BadVenueId()`
    BadVenueId,
    /// `CurveAmplification()`
    CurveAmplification,
    /// `CurveDomain()`
    CurveDomain,
    /// `CurveSpan()`
    CurveSpan,
    /// `DebtCapExceeded()`
    DebtCapExceeded,
    /// `DrawOutsideDrawnSet()`
    DrawOutsideDrawnSet,
    /// `ExactDelta()`
    ExactDelta,
    /// `ExitWorsensLedger()`
    ExitWorsensLedger,
    /// `Expired()`
    Expired,
    /// `FeatureDisabled(uint8)`
    FeatureDisabled,
    /// `FeeOutOfBounds()`
    FeeOutOfBounds,
    /// `FillInvalid()`
    FillInvalid,
    /// `FillPriceBand()`
    FillPriceBand,
    /// `FillValueDrop()`
    FillValueDrop,
    /// `FrameUnquotable()`
    FrameUnquotable,
    /// `GlobalPaused()`
    GlobalPaused,
    /// `HookInvalid()`
    HookInvalid,
    /// `InsufficientCollateral(uint256)`
    InsufficientCollateral,
    /// `InsufficientLiquidity(uint256)`
    InsufficientLiquidity,
    /// `InvalidAmount()`
    InvalidAmount,
    /// `InvalidConfig()`
    InvalidConfig,
    /// `InvalidPair()`
    InvalidPair,
    /// `InvalidPrice(address)`
    InvalidPrice,
    /// `IrmUnreadable()`
    IrmUnreadable,
    /// `LedgerBoundBreached()`
    LedgerBoundBreached,
    /// `LevBelowFloor()`
    LevBelowFloor,
    /// `LevPaused()`
    LevPaused,
    /// `LevValueLeak()`
    LevValueLeak,
    /// `LeverageDisabled()`
    LeverageDisabled,
    /// `NoRepaySnapshot()`
    NoRepaySnapshot,
    /// `NothingToFill()`
    NothingToFill,
    /// `NotionalCap()`
    NotionalCap,
    /// `OracleBand()`
    OracleBand,
    /// `OutputAboveCeiling()`
    OutputAboveCeiling,
    /// `Paused()`
    Paused,
    /// `PegBroken()`
    PegBroken,
    /// `PriceBand()`
    PriceBand,
    /// `PriceUnchecked()`
    PriceUnchecked,
    /// `RateCeiling()`
    RateCeiling,
    /// `RoomExceeded()`
    RoomExceeded,
    /// `RoomExhausted()`
    RoomExhausted,
    /// `SequencerDown()`
    SequencerDown,
    /// `SequencerGrace()`
    SequencerGrace,
    /// `Slippage()`
    Slippage,
    /// `SpreadUnavailable()`
    SpreadUnavailable,
    /// `StalePrice(address)`
    StalePrice,
    /// `SupplyCapExceeded()`
    SupplyCapExceeded,
    /// `Unhealthy()`
    Unhealthy,
    /// `UnknownToken(address)`
    UnknownToken,
    /// `Unsupported()`
    Unsupported,
    /// `VenueDisabled()`
    VenueDisabled,
    /// `VenueIsRetired()`
    VenueIsRetired,
    /// Solady `FixedPointMathLib.lnWad`'s `LnWadUndefined()`: the argument read as a non-positive
    /// `int256`.
    LnWadUndefined,
    /// OpenZeppelin 4.8 `Math.mulDiv`'s bare `require(denominator > prod1)` (empty revert data):
    /// the quotient does not fit 256 bits, or the denominator is zero under a product wider
    /// than 256 bits.
    MulDivOverflow,
    /// Solidity `Panic(0x11)`: checked arithmetic overflow or underflow.
    PanicArithmetic,
    /// Solidity `Panic(0x12)`: division or modulo by zero.
    PanicDivZero,
    /// Solidity `Panic(0x32)`: array index out of bounds.
    PanicIndex,
    /// Morpho Blue `UtilsLib.toUint128`'s `require(x <= type(uint128).max, "max uint128
    /// exceeded")`.
    MorphoMaxUint128Exceeded,
    /// Morpho Blue `require(totalBorrowAssets <= totalSupplyAssets, "insufficient liquidity")`
    /// after a withdraw or a borrow.
    MorphoInsufficientLiquidity,
    /// Morpho Blue `require(_isHealthy(...), "insufficient collateral")` after a borrow or a
    /// collateral withdrawal.
    MorphoInsufficientCollateral,
    /// Morpho Blue `require(UtilsLib.exactlyOneZero(assets, shares), "inconsistent input")`.
    MorphoInconsistentInput,
    /// Morpho Blue `require(assets != 0, "zero assets")` on the collateral entries.
    MorphoZeroAssets,
    /// The market's rate model reverted under Morpho's own accrual (`IIrm.borrowRate`): Blue
    /// bubbles the IRM's revert data, whatever it is, so the port names the class rather than
    /// the payload.
    MorphoIrmReverted,
    /// The market oracle reverted under Morpho's health check (`IOracle.price`): bubbled like the
    /// IRM's.
    MorphoOracleReverted,
    /// A Chainlink aggregator's `latestRoundData` reverted under `PriceFeed._read` /
    /// `_requireSequencer` (`PriceFeed.sol:159`, `:169`); the aggregator's own revert data
    /// bubbles and has no selector here.
    FeedReverted,
}

impl FlammError {
    /// The Solidity signature of the revert, as the `error` declaration spells it.
    pub fn signature(&self) -> &'static str {
        match self {
            Self::BadLoanIndex => "BadLoanIndex()",
            Self::BadVenueId => "BadVenueId()",
            Self::CurveAmplification => "CurveAmplification()",
            Self::CurveDomain => "CurveDomain()",
            Self::CurveSpan => "CurveSpan()",
            Self::DebtCapExceeded => "DebtCapExceeded()",
            Self::DrawOutsideDrawnSet => "DrawOutsideDrawnSet()",
            Self::ExactDelta => "ExactDelta()",
            Self::ExitWorsensLedger => "ExitWorsensLedger()",
            Self::Expired => "Expired()",
            Self::FeatureDisabled => "FeatureDisabled(uint8)",
            Self::FeeOutOfBounds => "FeeOutOfBounds()",
            Self::FillInvalid => "FillInvalid()",
            Self::FillPriceBand => "FillPriceBand()",
            Self::FillValueDrop => "FillValueDrop()",
            Self::FrameUnquotable => "FrameUnquotable()",
            Self::GlobalPaused => "GlobalPaused()",
            Self::HookInvalid => "HookInvalid()",
            Self::InsufficientCollateral => "InsufficientCollateral(uint256)",
            Self::InsufficientLiquidity => "InsufficientLiquidity(uint256)",
            Self::InvalidAmount => "InvalidAmount()",
            Self::InvalidConfig => "InvalidConfig()",
            Self::InvalidPair => "InvalidPair()",
            Self::InvalidPrice => "InvalidPrice(address)",
            Self::IrmUnreadable => "IrmUnreadable()",
            Self::LedgerBoundBreached => "LedgerBoundBreached()",
            Self::LevBelowFloor => "LevBelowFloor()",
            Self::LevPaused => "LevPaused()",
            Self::LevValueLeak => "LevValueLeak()",
            Self::LeverageDisabled => "LeverageDisabled()",
            Self::NoRepaySnapshot => "NoRepaySnapshot()",
            Self::NothingToFill => "NothingToFill()",
            Self::NotionalCap => "NotionalCap()",
            Self::OracleBand => "OracleBand()",
            Self::OutputAboveCeiling => "OutputAboveCeiling()",
            Self::Paused => "Paused()",
            Self::PegBroken => "PegBroken()",
            Self::PriceBand => "PriceBand()",
            Self::PriceUnchecked => "PriceUnchecked()",
            Self::RateCeiling => "RateCeiling()",
            Self::RoomExceeded => "RoomExceeded()",
            Self::RoomExhausted => "RoomExhausted()",
            Self::SequencerDown => "SequencerDown()",
            Self::SequencerGrace => "SequencerGrace()",
            Self::Slippage => "Slippage()",
            Self::SpreadUnavailable => "SpreadUnavailable()",
            Self::StalePrice => "StalePrice(address)",
            Self::SupplyCapExceeded => "SupplyCapExceeded()",
            Self::Unhealthy => "Unhealthy()",
            Self::UnknownToken => "UnknownToken(address)",
            Self::Unsupported => "Unsupported()",
            Self::VenueDisabled => "VenueDisabled()",
            Self::VenueIsRetired => "VenueIsRetired()",
            Self::LnWadUndefined => "LnWadUndefined()",
            Self::MulDivOverflow => "Math.mulDiv overflow (empty revert)",
            Self::PanicArithmetic => "Panic(0x11)",
            Self::PanicDivZero => "Panic(0x12)",
            Self::PanicIndex => "Panic(0x32)",
            Self::MorphoMaxUint128Exceeded => "Error(\"max uint128 exceeded\")",
            Self::MorphoInsufficientLiquidity => "Error(\"insufficient liquidity\")",
            Self::MorphoInsufficientCollateral => "Error(\"insufficient collateral\")",
            Self::MorphoInconsistentInput => "Error(\"inconsistent input\")",
            Self::MorphoZeroAssets => "Error(\"zero assets\")",
            Self::MorphoIrmReverted => "Morpho: irm reverted",
            Self::MorphoOracleReverted => "Morpho: oracle reverted",
            Self::FeedReverted => "latestRoundData reverted (aggregator revert bubbled)",
        }
    }

    /// The 4-byte selector of a custom error, `None` for the non-custom reverts.
    pub fn selector(&self) -> Option<[u8; 4]> {
        SELECTORS
            .iter()
            .find(|(_, e)| e == self)
            .map(|(s, _)| *s)
    }

    /// Classifies raw revert data (see the module documentation). `None` when the data maps to no
    /// known revert, which a caller must treat as a refusal rather than as agreement.
    ///
    /// The selector decides, not the length: 6 of the errors below carry arguments
    /// (`FeatureDisabled(uint8)`, `InsufficientCollateral(uint256)`,
    /// `InsufficientLiquidity(uint256)`, `InvalidPrice(address)`, `StalePrice(address)` and
    /// `UnknownToken(address)`), and the generators recorded those at their real on-chain length
    /// (36 bytes for one word of argument), so matching only 4-byte data left every one of them
    /// unclassified.
    pub fn from_revert_data(data: &[u8]) -> Option<Self> {
        if data.is_empty() {
            return Some(Self::MulDivOverflow);
        }
        let sel: [u8; 4] = data.get(..4)?.try_into().ok()?;
        if data.len() == 36 && sel == PANIC_SELECTOR {
            return match data[35] {
                0x11 if data[4..35].iter().all(|b| *b == 0) => Some(Self::PanicArithmetic),
                0x12 if data[4..35].iter().all(|b| *b == 0) => Some(Self::PanicDivZero),
                0x32 if data[4..35].iter().all(|b| *b == 0) => Some(Self::PanicIndex),
                _ => None,
            };
        }
        if data.len() >= 68 && sel == ERROR_STRING_SELECTOR {
            return Self::from_morpho_require(error_string_payload(data)?);
        }
        if sel == LN_WAD_UNDEFINED_SELECTOR {
            return Some(Self::LnWadUndefined);
        }
        SELECTORS
            .iter()
            .find(|(s, _)| *s == sel)
            .map(|(_, e)| *e)
    }

    /// The variant of one of Morpho Blue's `ErrorsLib` require strings, `None` for any other
    /// string.
    pub fn from_morpho_require(msg: &str) -> Option<Self> {
        match msg {
            "max uint128 exceeded" => Some(Self::MorphoMaxUint128Exceeded),
            "insufficient liquidity" => Some(Self::MorphoInsufficientLiquidity),
            "insufficient collateral" => Some(Self::MorphoInsufficientCollateral),
            "inconsistent input" => Some(Self::MorphoInconsistentInput),
            "zero assets" => Some(Self::MorphoZeroAssets),
            _ => None,
        }
    }
}

/// The string of an ABI-encoded `Error(string)` payload (selector, offset word, length word,
/// bytes), or `None` when the encoding is malformed.
pub fn error_string_payload(data: &[u8]) -> Option<&str> {
    if data.len() < 68 || data[..4] != ERROR_STRING_SELECTOR {
        return None;
    }
    let offset = word_as_usize(&data[4..36])?;
    let len_at = 4usize.checked_add(offset)?;
    let len = word_as_usize(data.get(len_at..len_at.checked_add(32)?)?)?;
    let start = len_at.checked_add(32)?;
    let bytes = data.get(start..start.checked_add(len)?)?;
    std::str::from_utf8(bytes).ok()
}

fn word_as_usize(word: &[u8]) -> Option<usize> {
    if word.len() != 32 || word[..24].iter().any(|b| *b != 0) {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&word[24..]);
    usize::try_from(u64::from_be_bytes(buf)).ok()
}

impl fmt::Display for FlammError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "everlong-flamm: {}", self.signature())
    }
}

impl std::error::Error for FlammError {}

/// `Panic(uint256)`.
const PANIC_SELECTOR: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];
/// `Error(string)`.
const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
/// Solady `LnWadUndefined()`.
const LN_WAD_UNDEFINED_SELECTOR: [u8; 4] = [0x16, 0x15, 0xe6, 0x38];

/// Every modelled custom-error selector and its variant, each selector the keccak of the `error`
/// signature declared in the c104 tree at `80abd43`.
const SELECTORS: &[([u8; 4], FlammError)] = &[
    ([0x04, 0xa2, 0x4d, 0x00], FlammError::BadLoanIndex),
    ([0x4c, 0x85, 0x66, 0xdd], FlammError::BadVenueId),
    ([0xfb, 0x62, 0x0e, 0x8c], FlammError::CurveAmplification),
    ([0xe3, 0x92, 0x2b, 0x5c], FlammError::CurveDomain),
    ([0xb7, 0x9f, 0x5c, 0xa9], FlammError::CurveSpan),
    ([0x7d, 0xb9, 0x10, 0x6f], FlammError::DebtCapExceeded),
    ([0x8c, 0xdb, 0xe7, 0x66], FlammError::DrawOutsideDrawnSet),
    ([0x9d, 0x8c, 0x40, 0x1b], FlammError::ExactDelta),
    ([0x84, 0x36, 0x9e, 0xea], FlammError::ExitWorsensLedger),
    ([0x20, 0x3d, 0x82, 0xd8], FlammError::Expired),
    ([0x6d, 0x2d, 0x9f, 0x49], FlammError::FeatureDisabled),
    ([0x26, 0x36, 0x3b, 0x73], FlammError::FeeOutOfBounds),
    ([0x77, 0x41, 0x74, 0x54], FlammError::FillInvalid),
    ([0xf6, 0x4d, 0xc1, 0xcd], FlammError::FillPriceBand),
    ([0x88, 0x18, 0x22, 0x65], FlammError::FillValueDrop),
    ([0xd6, 0xba, 0x11, 0xd5], FlammError::FrameUnquotable),
    ([0x8b, 0xee, 0x70, 0x4f], FlammError::GlobalPaused),
    ([0x4b, 0xdf, 0xed, 0x04], FlammError::HookInvalid),
    ([0x2b, 0x3b, 0xc9, 0x85], FlammError::InsufficientCollateral),
    ([0xc7, 0x30, 0x33, 0x3f], FlammError::InsufficientLiquidity),
    ([0x2c, 0x52, 0x11, 0xc6], FlammError::InvalidAmount),
    ([0x35, 0xbe, 0x3a, 0xc8], FlammError::InvalidConfig),
    ([0x1e, 0x4f, 0x7d, 0x8c], FlammError::InvalidPair),
    ([0xcd, 0x21, 0x50, 0x06], FlammError::InvalidPrice),
    ([0xa4, 0xfe, 0x5f, 0x8b], FlammError::IrmUnreadable),
    ([0x52, 0xbf, 0x12, 0x35], FlammError::LedgerBoundBreached),
    ([0x2e, 0xa2, 0xdc, 0xe8], FlammError::LevBelowFloor),
    ([0x78, 0xd6, 0x12, 0xf2], FlammError::LevPaused),
    ([0x28, 0x85, 0x17, 0x30], FlammError::LevValueLeak),
    ([0x27, 0x33, 0xd0, 0xd9], FlammError::LeverageDisabled),
    ([0x76, 0xa2, 0x3c, 0xbf], FlammError::NoRepaySnapshot),
    ([0x7c, 0x3f, 0xa3, 0xaf], FlammError::NothingToFill),
    ([0xf9, 0xb4, 0x67, 0x8a], FlammError::NotionalCap),
    ([0xab, 0xa9, 0xac, 0x38], FlammError::OracleBand),
    ([0xc6, 0x52, 0x0d, 0xe3], FlammError::OutputAboveCeiling),
    ([0x9e, 0x87, 0xfa, 0xc8], FlammError::Paused),
    ([0x85, 0xc2, 0xbe, 0x22], FlammError::PegBroken),
    ([0xfe, 0x85, 0xbb, 0x51], FlammError::PriceBand),
    ([0xc0, 0xfd, 0x97, 0x0f], FlammError::PriceUnchecked),
    ([0xb2, 0xef, 0x0f, 0x93], FlammError::RateCeiling),
    ([0xd6, 0xf8, 0xf8, 0x9c], FlammError::RoomExceeded),
    ([0x84, 0xf5, 0x27, 0x0a], FlammError::RoomExhausted),
    ([0x03, 0x2b, 0x3d, 0x00], FlammError::SequencerDown),
    ([0xc3, 0x73, 0x4d, 0xc2], FlammError::SequencerGrace),
    ([0x7d, 0xd3, 0x7f, 0x70], FlammError::Slippage),
    ([0xc8, 0x1f, 0x12, 0x09], FlammError::SpreadUnavailable),
    ([0x81, 0x92, 0x79, 0x29], FlammError::StalePrice),
    ([0xf5, 0x8f, 0x73, 0x3a], FlammError::SupplyCapExceeded),
    ([0x7d, 0xb7, 0xa0, 0x74], FlammError::Unhealthy),
    ([0x81, 0xa3, 0xb1, 0xbe], FlammError::UnknownToken),
    ([0x90, 0xa2, 0xca, 0xf2], FlammError::Unsupported),
    ([0xc8, 0x07, 0x12, 0x40], FlammError::VenueDisabled),
    ([0x57, 0x4d, 0x7f, 0xff], FlammError::VenueIsRetired),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_are_unique_and_round_trip() {
        for (i, (sel, e)) in SELECTORS.iter().enumerate() {
            assert_eq!(FlammError::from_revert_data(sel), Some(*e), "{e:?}");
            assert_eq!(e.selector(), Some(*sel));
            assert!(
                !SELECTORS[..i]
                    .iter()
                    .any(|(s, _)| s == sel),
                "duplicate selector {sel:02x?}"
            );
        }
        assert_eq!(SELECTORS.len(), 53);
    }

    #[test]
    fn non_custom_reverts() {
        assert_eq!(FlammError::from_revert_data(&[]), Some(FlammError::MulDivOverflow));
        let mut panic = vec![0x4e, 0x48, 0x7b, 0x71];
        panic.extend([0u8; 32]);
        panic[35] = 0x11;
        assert_eq!(FlammError::from_revert_data(&panic), Some(FlammError::PanicArithmetic));
        panic[35] = 0x12;
        assert_eq!(FlammError::from_revert_data(&panic), Some(FlammError::PanicDivZero));
        panic[35] = 0x32;
        assert_eq!(FlammError::from_revert_data(&panic), Some(FlammError::PanicIndex));
        panic[35] = 0x01;
        assert_eq!(FlammError::from_revert_data(&panic), None);
        assert_eq!(
            FlammError::from_revert_data(&[0x16, 0x15, 0xe6, 0x38]),
            Some(FlammError::LnWadUndefined)
        );
        assert_eq!(
            FlammError::from_revert_data(&[0xe3, 0x92, 0x2b, 0x5c]),
            Some(FlammError::CurveDomain)
        );
        assert_eq!(FlammError::from_revert_data(&[0, 0, 0, 0]), None);
        assert_eq!(FlammError::LnWadUndefined.selector(), None);
    }

    /// The errors that carry arguments classify at the length they revert with: the selector
    /// then one word per `uint256` / `address`, as the fixture generators recorded them.
    #[test]
    fn parameterised_custom_errors() {
        for (e, words) in [
            (FlammError::InsufficientLiquidity, 1),
            (FlammError::InsufficientCollateral, 1),
            (FlammError::FeatureDisabled, 1),
            (FlammError::StalePrice, 1),
            (FlammError::InvalidPrice, 1),
            (FlammError::UnknownToken, 1),
        ] {
            let mut data = e.selector().unwrap().to_vec();
            data.resize(4 + 32 * words, 0);
            assert_eq!(FlammError::from_revert_data(&data), Some(e), "{e:?}");
        }
        // An unknown selector with arguments is still no classification.
        let mut unknown = vec![0u8; 36];
        unknown[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(FlammError::from_revert_data(&unknown), None);
        // A `Panic(uint256)` with an unmapped code stays unmapped: the panic selector is not in
        // the table.
        let mut panic = vec![0x4e, 0x48, 0x7b, 0x71];
        panic.extend([0u8; 32]);
        panic[35] = 0x51;
        assert_eq!(FlammError::from_revert_data(&panic), None);
    }

    #[test]
    fn morpho_require_strings() {
        fn encode(msg: &str) -> Vec<u8> {
            let mut v = vec![0x08, 0xc3, 0x79, 0xa0];
            let mut off = [0u8; 32];
            off[31] = 0x20;
            v.extend(off);
            let mut len = [0u8; 32];
            len[24..].copy_from_slice(&(msg.len() as u64).to_be_bytes());
            v.extend(len);
            v.extend(msg.as_bytes());
            v.resize(v.len().div_ceil(32) * 32, 0);
            v
        }
        for (msg, e) in [
            ("max uint128 exceeded", FlammError::MorphoMaxUint128Exceeded),
            ("insufficient liquidity", FlammError::MorphoInsufficientLiquidity),
            ("insufficient collateral", FlammError::MorphoInsufficientCollateral),
            ("inconsistent input", FlammError::MorphoInconsistentInput),
            ("zero assets", FlammError::MorphoZeroAssets),
        ] {
            assert_eq!(FlammError::from_revert_data(&encode(msg)), Some(e), "{msg}");
            assert_eq!(e.selector(), None);
        }
        assert_eq!(FlammError::from_revert_data(&encode("market not created")), None);
        assert_eq!(error_string_payload(&encode("zero assets")), Some("zero assets"));
        assert_eq!(error_string_payload(&[0x08, 0xc3, 0x79, 0xa0]), None);
    }
}
