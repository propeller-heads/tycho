// Copyright (c) 2026 Everlong Labs Limited

//! Reverts of the deployed c104 contracts (`80abd43`), one variant per custom error, plus the four
//! non-custom reverts the quoting path can raise and the `require` strings of the Morpho Blue
//! singleton the financing account drives.
//!
//! Every function of the port returns [`FlammError`] exactly where the Solidity it ports reverts,
//! so a refusal maps 1:1 onto the revert a real fill would hit. The 147 custom errors are the
//! `error` signatures declared under `src/core`, `src/factory`, `src/hooks`, `src/interfaces` and
//! `src/libraries` of the c104 tree together with the OpenZeppelin ERC20 / Initializable errors
//! their code can raise. The quoting path reaches the curve's, the hooks', the Router's, the
//! account's, the gate's and the pool's custom errors, the Morpho `require` strings and the
//! non-custom reverts; the remaining variants are declared so the whole tree shares one error type
//! and one selector table. [`FeedReverted`](FlammError::FeedReverted) is a Chainlink aggregator's
//! `latestRoundData` reverting under `PriceFeed`, whose own revert data bubbles and is not
//! modelled (`pricefeed.go` `errFeedReverted`).
//!
//! [`FlammError::from_revert_data`] classifies raw revert data the way the fixture generators
//! recorded it: empty data is OpenZeppelin 4.8 `Math.mulDiv`'s bare `require(denominator > prod1)`;
//! a 36-byte `Panic(uint256)` is a Solidity panic (0x11 checked arithmetic, 0x12 division by zero,
//! 0x32 index out of bounds); a 4-byte word is a custom-error selector; an `Error(string)` payload
//! is one of Morpho Blue's `ErrorsLib` strings.

use std::fmt;

/// A revert of the deployed pool, its hooks, its Router, its financing account or its price feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FlammError {
    /// `AccountCodeMismatch()`
    AccountCodeMismatch,
    /// `AlreadyBootstrapped()`
    AlreadyBootstrapped,
    /// `AlreadyInitialized()`
    AlreadyInitialized,
    /// `AlreadyRegistered()`
    AlreadyRegistered,
    /// `AnchorOutOfBounds()`
    AnchorOutOfBounds,
    /// `BadKind()`
    BadKind,
    /// `BadLoanIndex()`
    BadLoanIndex,
    /// `BadPriorities()`
    BadPriorities,
    /// `BadReceiver()`
    BadReceiver,
    /// `BadVenueId()`
    BadVenueId,
    /// `BelowMinimumShares()`
    BelowMinimumShares,
    /// `Conservation()`
    Conservation,
    /// `ContextMismatch()`
    ContextMismatch,
    /// `CreateFailed()`
    CreateFailed,
    /// `CurveAmplification()`
    CurveAmplification,
    /// `CurveDomain()`
    CurveDomain,
    /// `CurvePriceOutOfDomain()`
    CurvePriceOutOfDomain,
    /// `CurveReprices()`
    CurveReprices,
    /// `CurveSpan()`
    CurveSpan,
    /// `CustodyMismatch()`
    CustodyMismatch,
    /// `DebtCapExceeded()`
    DebtCapExceeded,
    /// `DebtOutstanding()`
    DebtOutstanding,
    /// `DeployerInvalid(uint8,address)`
    DeployerInvalid,
    /// `DeployerNotReady(uint8)`
    DeployerNotReady,
    /// `DepositCapExceeded()`
    DepositCapExceeded,
    /// `DepositNotPermitted()`
    DepositNotPermitted,
    /// `DialCooldown()`
    DialCooldown,
    /// `DialOutOfEnvelope()`
    DialOutOfEnvelope,
    /// `DrawOutsideDrawnSet()`
    DrawOutsideDrawnSet,
    /// `DuplicateLoanAsset()`
    DuplicateLoanAsset,
    /// `DuplicateVenue()`
    DuplicateVenue,
    /// `ERC20InsufficientAllowance(address,uint256,uint256)`
    ERC20InsufficientAllowance,
    /// `ERC20InsufficientBalance(address,uint256,uint256)`
    ERC20InsufficientBalance,
    /// `ERC20InvalidApprover(address)`
    ERC20InvalidApprover,
    /// `ERC20InvalidReceiver(address)`
    ERC20InvalidReceiver,
    /// `ERC20InvalidSender(address)`
    ERC20InvalidSender,
    /// `ERC20InvalidSpender(address)`
    ERC20InvalidSpender,
    /// `ExactDelta()`
    ExactDelta,
    /// `ExitWorsensLedger()`
    ExitWorsensLedger,
    /// `Expired()`
    Expired,
    /// `FeatureDisabled(uint8)`
    FeatureDisabled,
    /// `FeeMismatch()`
    FeeMismatch,
    /// `FeeOutOfBounds()`
    FeeOutOfBounds,
    /// `FillInvalid()`
    FillInvalid,
    /// `FillMismatch()`
    FillMismatch,
    /// `FillPriceBand()`
    FillPriceBand,
    /// `FillValueDrop()`
    FillValueDrop,
    /// `FrameUnquotable()`
    FrameUnquotable,
    /// `GlobalPaused()`
    GlobalPaused,
    /// `GuardianMayOnlyTighten()`
    GuardianMayOnlyTighten,
    /// `HookInvalid()`
    HookInvalid,
    /// `HookInvalid(address)`
    HookInvalidAddress,
    /// `HookSetNotReady()`
    HookSetNotReady,
    /// `ImplementationCodeChanged()`
    ImplementationCodeChanged,
    /// `ImplementationNotReady()`
    ImplementationNotReady,
    /// `ImplementationVersion(uint32,uint32)`
    ImplementationVersion,
    /// `InsufficientBridge(uint256)`
    InsufficientBridge,
    /// `InsufficientCollateral(uint256)`
    InsufficientCollateral,
    /// `InsufficientLiquidity(uint256)`
    InsufficientLiquidity,
    /// `InvalidAmount()`
    InvalidAmount,
    /// `InvalidBounds()`
    InvalidBounds,
    /// `InvalidConfig()`
    InvalidConfig,
    /// `InvalidInitialization()`
    InvalidInitialization,
    /// `InvalidPair()`
    InvalidPair,
    /// `InvalidPrice(address)`
    InvalidPrice,
    /// `IrmUnreadable()`
    IrmUnreadable,
    /// `KindUnknown()`
    KindUnknown,
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
    /// `LoanAssetInExceeded()`
    LoanAssetInExceeded,
    /// `LoanAssetKnown()`
    LoanAssetKnown,
    /// `LoanAssetOutBelowMin()`
    LoanAssetOutBelowMin,
    /// `LoanLegRequired()`
    LoanLegRequired,
    /// `LoanNotEmpty()`
    LoanNotEmpty,
    /// `LoanRetired()`
    LoanRetired,
    /// `LoanSwapDisabled()`
    LoanSwapDisabled,
    /// `LtvFloorBreached()`
    LtvFloorBreached,
    /// `LtvLltvGap()`
    LtvLltvGap,
    /// `LtvStepTooLarge()`
    LtvStepTooLarge,
    /// `MarketUnknown()`
    MarketUnknown,
    /// `NestedCallback()`
    NestedCallback,
    /// `NoDebt()`
    NoDebt,
    /// `NoDeployerScheduled(uint8)`
    NoDeployerScheduled,
    /// `NoHookSetScheduled()`
    NoHookSetScheduled,
    /// `NoImplementationScheduled()`
    NoImplementationScheduled,
    /// `NoLoanAssetScheduled()`
    NoLoanAssetScheduled,
    /// `NoNewObservation()`
    NoNewObservation,
    /// `NoRepaySnapshot()`
    NoRepaySnapshot,
    /// `NoVenueScheduled()`
    NoVenueScheduled,
    /// `NotBootstrapped()`
    NotBootstrapped,
    /// `NotCurator()`
    NotCurator,
    /// `NotFactory()`
    NotFactory,
    /// `NotGuardianOrCurator()`
    NotGuardianOrCurator,
    /// `NotInitialized()`
    NotInitialized,
    /// `NotInitializing()`
    NotInitializing,
    /// `NotKeeper()`
    NotKeeper,
    /// `NotKeeperOrCurator()`
    NotKeeperOrCurator,
    /// `NotMorpho()`
    NotMorpho,
    /// `NotOwner()`
    NotOwner,
    /// `NotPaused()`
    NotPaused,
    /// `NotPool()`
    NotPool,
    /// `NotProtocolSafe()`
    NotProtocolSafe,
    /// `NotRegistered()`
    NotRegistered,
    /// `NotRouter()`
    NotRouter,
    /// `NothingToFill()`
    NothingToFill,
    /// `NothingToMaintain()`
    NothingToMaintain,
    /// `NothingToMove()`
    NothingToMove,
    /// `NotionalCap()`
    NotionalCap,
    /// `OracleBand()`
    OracleBand,
    /// `OutputAboveCeiling()`
    OutputAboveCeiling,
    /// `OwnershipCannotBeRenounced()`
    OwnershipCannotBeRenounced,
    /// `Paused()`
    Paused,
    /// `PegBroken()`
    PegBroken,
    /// `PoolBindingMismatch()`
    PoolBindingMismatch,
    /// `PredictionMismatch()`
    PredictionMismatch,
    /// `PriceBand()`
    PriceBand,
    /// `PriceFeedMismatch()`
    PriceFeedMismatch,
    /// `PriceUnchecked()`
    PriceUnchecked,
    /// `RateCeiling()`
    RateCeiling,
    /// `Reentrant()`
    Reentrant,
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
    /// `SpreadOutOfBounds()`
    SpreadOutOfBounds,
    /// `SpreadUnavailable()`
    SpreadUnavailable,
    /// `StalePair()`
    StalePair,
    /// `StalePrice(address)`
    StalePrice,
    /// `SupplyCapExceeded()`
    SupplyCapExceeded,
    /// `TooManyLoanAssets()`
    TooManyLoanAssets,
    /// `TooManyVenues()`
    TooManyVenues,
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
    /// `VenueMismatch()`
    VenueMismatch,
    /// `VenueNotEmpty()`
    VenueNotEmpty,
    /// `VenueQuarantined()`
    VenueQuarantined,
    /// `VenueUnknown()`
    VenueUnknown,
    /// `ZeroAddress()`
    ZeroAddress,
    /// `ZeroPrice()`
    ZeroPrice,
    /// `ZeroShares()`
    ZeroShares,
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
    /// The Solidity signature of the revert, as the Go port's sentinel messages carry it.
    pub fn signature(&self) -> &'static str {
        match self {
            Self::AccountCodeMismatch => "AccountCodeMismatch()",
            Self::AlreadyBootstrapped => "AlreadyBootstrapped()",
            Self::AlreadyInitialized => "AlreadyInitialized()",
            Self::AlreadyRegistered => "AlreadyRegistered()",
            Self::AnchorOutOfBounds => "AnchorOutOfBounds()",
            Self::BadKind => "BadKind()",
            Self::BadLoanIndex => "BadLoanIndex()",
            Self::BadPriorities => "BadPriorities()",
            Self::BadReceiver => "BadReceiver()",
            Self::BadVenueId => "BadVenueId()",
            Self::BelowMinimumShares => "BelowMinimumShares()",
            Self::Conservation => "Conservation()",
            Self::ContextMismatch => "ContextMismatch()",
            Self::CreateFailed => "CreateFailed()",
            Self::CurveAmplification => "CurveAmplification()",
            Self::CurveDomain => "CurveDomain()",
            Self::CurvePriceOutOfDomain => "CurvePriceOutOfDomain()",
            Self::CurveReprices => "CurveReprices()",
            Self::CurveSpan => "CurveSpan()",
            Self::CustodyMismatch => "CustodyMismatch()",
            Self::DebtCapExceeded => "DebtCapExceeded()",
            Self::DebtOutstanding => "DebtOutstanding()",
            Self::DeployerInvalid => "DeployerInvalid(uint8,address)",
            Self::DeployerNotReady => "DeployerNotReady(uint8)",
            Self::DepositCapExceeded => "DepositCapExceeded()",
            Self::DepositNotPermitted => "DepositNotPermitted()",
            Self::DialCooldown => "DialCooldown()",
            Self::DialOutOfEnvelope => "DialOutOfEnvelope()",
            Self::DrawOutsideDrawnSet => "DrawOutsideDrawnSet()",
            Self::DuplicateLoanAsset => "DuplicateLoanAsset()",
            Self::DuplicateVenue => "DuplicateVenue()",
            Self::ERC20InsufficientAllowance => {
                "ERC20InsufficientAllowance(address,uint256,uint256)"
            }
            Self::ERC20InsufficientBalance => "ERC20InsufficientBalance(address,uint256,uint256)",
            Self::ERC20InvalidApprover => "ERC20InvalidApprover(address)",
            Self::ERC20InvalidReceiver => "ERC20InvalidReceiver(address)",
            Self::ERC20InvalidSender => "ERC20InvalidSender(address)",
            Self::ERC20InvalidSpender => "ERC20InvalidSpender(address)",
            Self::ExactDelta => "ExactDelta()",
            Self::ExitWorsensLedger => "ExitWorsensLedger()",
            Self::Expired => "Expired()",
            Self::FeatureDisabled => "FeatureDisabled(uint8)",
            Self::FeeMismatch => "FeeMismatch()",
            Self::FeeOutOfBounds => "FeeOutOfBounds()",
            Self::FillInvalid => "FillInvalid()",
            Self::FillMismatch => "FillMismatch()",
            Self::FillPriceBand => "FillPriceBand()",
            Self::FillValueDrop => "FillValueDrop()",
            Self::FrameUnquotable => "FrameUnquotable()",
            Self::GlobalPaused => "GlobalPaused()",
            Self::GuardianMayOnlyTighten => "GuardianMayOnlyTighten()",
            Self::HookInvalid => "HookInvalid()",
            Self::HookInvalidAddress => "HookInvalid(address)",
            Self::HookSetNotReady => "HookSetNotReady()",
            Self::ImplementationCodeChanged => "ImplementationCodeChanged()",
            Self::ImplementationNotReady => "ImplementationNotReady()",
            Self::ImplementationVersion => "ImplementationVersion(uint32,uint32)",
            Self::InsufficientBridge => "InsufficientBridge(uint256)",
            Self::InsufficientCollateral => "InsufficientCollateral(uint256)",
            Self::InsufficientLiquidity => "InsufficientLiquidity(uint256)",
            Self::InvalidAmount => "InvalidAmount()",
            Self::InvalidBounds => "InvalidBounds()",
            Self::InvalidConfig => "InvalidConfig()",
            Self::InvalidInitialization => "InvalidInitialization()",
            Self::InvalidPair => "InvalidPair()",
            Self::InvalidPrice => "InvalidPrice(address)",
            Self::IrmUnreadable => "IrmUnreadable()",
            Self::KindUnknown => "KindUnknown()",
            Self::LedgerBoundBreached => "LedgerBoundBreached()",
            Self::LevBelowFloor => "LevBelowFloor()",
            Self::LevPaused => "LevPaused()",
            Self::LevValueLeak => "LevValueLeak()",
            Self::LeverageDisabled => "LeverageDisabled()",
            Self::LoanAssetInExceeded => "LoanAssetInExceeded()",
            Self::LoanAssetKnown => "LoanAssetKnown()",
            Self::LoanAssetOutBelowMin => "LoanAssetOutBelowMin()",
            Self::LoanLegRequired => "LoanLegRequired()",
            Self::LoanNotEmpty => "LoanNotEmpty()",
            Self::LoanRetired => "LoanRetired()",
            Self::LoanSwapDisabled => "LoanSwapDisabled()",
            Self::LtvFloorBreached => "LtvFloorBreached()",
            Self::LtvLltvGap => "LtvLltvGap()",
            Self::LtvStepTooLarge => "LtvStepTooLarge()",
            Self::MarketUnknown => "MarketUnknown()",
            Self::NestedCallback => "NestedCallback()",
            Self::NoDebt => "NoDebt()",
            Self::NoDeployerScheduled => "NoDeployerScheduled(uint8)",
            Self::NoHookSetScheduled => "NoHookSetScheduled()",
            Self::NoImplementationScheduled => "NoImplementationScheduled()",
            Self::NoLoanAssetScheduled => "NoLoanAssetScheduled()",
            Self::NoNewObservation => "NoNewObservation()",
            Self::NoRepaySnapshot => "NoRepaySnapshot()",
            Self::NoVenueScheduled => "NoVenueScheduled()",
            Self::NotBootstrapped => "NotBootstrapped()",
            Self::NotCurator => "NotCurator()",
            Self::NotFactory => "NotFactory()",
            Self::NotGuardianOrCurator => "NotGuardianOrCurator()",
            Self::NotInitialized => "NotInitialized()",
            Self::NotInitializing => "NotInitializing()",
            Self::NotKeeper => "NotKeeper()",
            Self::NotKeeperOrCurator => "NotKeeperOrCurator()",
            Self::NotMorpho => "NotMorpho()",
            Self::NotOwner => "NotOwner()",
            Self::NotPaused => "NotPaused()",
            Self::NotPool => "NotPool()",
            Self::NotProtocolSafe => "NotProtocolSafe()",
            Self::NotRegistered => "NotRegistered()",
            Self::NotRouter => "NotRouter()",
            Self::NothingToFill => "NothingToFill()",
            Self::NothingToMaintain => "NothingToMaintain()",
            Self::NothingToMove => "NothingToMove()",
            Self::NotionalCap => "NotionalCap()",
            Self::OracleBand => "OracleBand()",
            Self::OutputAboveCeiling => "OutputAboveCeiling()",
            Self::OwnershipCannotBeRenounced => "OwnershipCannotBeRenounced()",
            Self::Paused => "Paused()",
            Self::PegBroken => "PegBroken()",
            Self::PoolBindingMismatch => "PoolBindingMismatch()",
            Self::PredictionMismatch => "PredictionMismatch()",
            Self::PriceBand => "PriceBand()",
            Self::PriceFeedMismatch => "PriceFeedMismatch()",
            Self::PriceUnchecked => "PriceUnchecked()",
            Self::RateCeiling => "RateCeiling()",
            Self::Reentrant => "Reentrant()",
            Self::RoomExceeded => "RoomExceeded()",
            Self::RoomExhausted => "RoomExhausted()",
            Self::SequencerDown => "SequencerDown()",
            Self::SequencerGrace => "SequencerGrace()",
            Self::Slippage => "Slippage()",
            Self::SpreadOutOfBounds => "SpreadOutOfBounds()",
            Self::SpreadUnavailable => "SpreadUnavailable()",
            Self::StalePair => "StalePair()",
            Self::StalePrice => "StalePrice(address)",
            Self::SupplyCapExceeded => "SupplyCapExceeded()",
            Self::TooManyLoanAssets => "TooManyLoanAssets()",
            Self::TooManyVenues => "TooManyVenues()",
            Self::Unhealthy => "Unhealthy()",
            Self::UnknownToken => "UnknownToken(address)",
            Self::Unsupported => "Unsupported()",
            Self::VenueDisabled => "VenueDisabled()",
            Self::VenueIsRetired => "VenueIsRetired()",
            Self::VenueMismatch => "VenueMismatch()",
            Self::VenueNotEmpty => "VenueNotEmpty()",
            Self::VenueQuarantined => "VenueQuarantined()",
            Self::VenueUnknown => "VenueUnknown()",
            Self::ZeroAddress => "ZeroAddress()",
            Self::ZeroPrice => "ZeroPrice()",
            Self::ZeroShares => "ZeroShares()",
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
    pub fn from_revert_data(data: &[u8]) -> Option<Self> {
        match data.len() {
            0 => Some(Self::MulDivOverflow),
            4 => {
                let sel: [u8; 4] = data.try_into().ok()?;
                if sel == LN_WAD_UNDEFINED_SELECTOR {
                    return Some(Self::LnWadUndefined);
                }
                SELECTORS
                    .iter()
                    .find(|(s, _)| *s == sel)
                    .map(|(_, e)| *e)
            }
            36 if data[..4] == PANIC_SELECTOR => match data[35] {
                0x11 if data[4..35].iter().all(|b| *b == 0) => Some(Self::PanicArithmetic),
                0x12 if data[4..35].iter().all(|b| *b == 0) => Some(Self::PanicDivZero),
                0x32 if data[4..35].iter().all(|b| *b == 0) => Some(Self::PanicIndex),
                _ => None,
            },
            n if n >= 68 && data[..4] == ERROR_STRING_SELECTOR => {
                Self::from_morpho_require(error_string_payload(data)?)
            }
            _ => None,
        }
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

/// Every custom-error selector and its variant, as `errors.go` `revertSelectors` lists them.
const SELECTORS: &[([u8; 4], FlammError)] = &[
    ([0xfc, 0x4b, 0x9e, 0x6c], FlammError::AccountCodeMismatch),
    ([0xb8, 0x07, 0xa0, 0xea], FlammError::AlreadyBootstrapped),
    ([0x0d, 0xc1, 0x49, 0xf0], FlammError::AlreadyInitialized),
    ([0x3a, 0x81, 0xd6, 0xfc], FlammError::AlreadyRegistered),
    ([0xfc, 0x57, 0xeb, 0xae], FlammError::AnchorOutOfBounds),
    ([0x2b, 0x43, 0x4c, 0x20], FlammError::BadKind),
    ([0x04, 0xa2, 0x4d, 0x00], FlammError::BadLoanIndex),
    ([0xdc, 0x22, 0x89, 0x8f], FlammError::BadPriorities),
    ([0xe2, 0x90, 0x48, 0x6f], FlammError::BadReceiver),
    ([0x4c, 0x85, 0x66, 0xdd], FlammError::BadVenueId),
    ([0xce, 0xfb, 0x77, 0x38], FlammError::BelowMinimumShares),
    ([0x81, 0x2d, 0x2c, 0x18], FlammError::Conservation),
    ([0xef, 0x6d, 0xae, 0xb0], FlammError::ContextMismatch),
    ([0x7e, 0x16, 0xb8, 0xcd], FlammError::CreateFailed),
    ([0xfb, 0x62, 0x0e, 0x8c], FlammError::CurveAmplification),
    ([0xe3, 0x92, 0x2b, 0x5c], FlammError::CurveDomain),
    ([0xd4, 0x7a, 0xb8, 0x82], FlammError::CurvePriceOutOfDomain),
    ([0x3b, 0xc0, 0x6a, 0x54], FlammError::CurveReprices),
    ([0xb7, 0x9f, 0x5c, 0xa9], FlammError::CurveSpan),
    ([0xb4, 0x0f, 0x79, 0x2e], FlammError::CustodyMismatch),
    ([0x7d, 0xb9, 0x10, 0x6f], FlammError::DebtCapExceeded),
    ([0x56, 0x8d, 0x5a, 0x84], FlammError::DebtOutstanding),
    ([0x63, 0x80, 0x7a, 0xd1], FlammError::DeployerInvalid),
    ([0x3d, 0x9c, 0x05, 0x8e], FlammError::DeployerNotReady),
    ([0x93, 0x5d, 0x63, 0x0c], FlammError::DepositCapExceeded),
    ([0x98, 0xe8, 0x45, 0xfa], FlammError::DepositNotPermitted),
    ([0x45, 0x3b, 0xc6, 0x65], FlammError::DialCooldown),
    ([0x82, 0x33, 0xef, 0xb5], FlammError::DialOutOfEnvelope),
    ([0x8c, 0xdb, 0xe7, 0x66], FlammError::DrawOutsideDrawnSet),
    ([0x52, 0x8d, 0x16, 0x67], FlammError::DuplicateLoanAsset),
    ([0x8c, 0xce, 0x95, 0xa1], FlammError::DuplicateVenue),
    ([0xfb, 0x8f, 0x41, 0xb2], FlammError::ERC20InsufficientAllowance),
    ([0xe4, 0x50, 0xd3, 0x8c], FlammError::ERC20InsufficientBalance),
    ([0xe6, 0x02, 0xdf, 0x05], FlammError::ERC20InvalidApprover),
    ([0xec, 0x44, 0x2f, 0x05], FlammError::ERC20InvalidReceiver),
    ([0x96, 0xc6, 0xfd, 0x1e], FlammError::ERC20InvalidSender),
    ([0x94, 0x28, 0x0d, 0x62], FlammError::ERC20InvalidSpender),
    ([0x9d, 0x8c, 0x40, 0x1b], FlammError::ExactDelta),
    ([0x84, 0x36, 0x9e, 0xea], FlammError::ExitWorsensLedger),
    ([0x20, 0x3d, 0x82, 0xd8], FlammError::Expired),
    ([0x6d, 0x2d, 0x9f, 0x49], FlammError::FeatureDisabled),
    ([0x36, 0xa9, 0xd0, 0x11], FlammError::FeeMismatch),
    ([0x26, 0x36, 0x3b, 0x73], FlammError::FeeOutOfBounds),
    ([0x77, 0x41, 0x74, 0x54], FlammError::FillInvalid),
    ([0x02, 0x4a, 0x31, 0x9c], FlammError::FillMismatch),
    ([0xf6, 0x4d, 0xc1, 0xcd], FlammError::FillPriceBand),
    ([0x88, 0x18, 0x22, 0x65], FlammError::FillValueDrop),
    ([0xd6, 0xba, 0x11, 0xd5], FlammError::FrameUnquotable),
    ([0x8b, 0xee, 0x70, 0x4f], FlammError::GlobalPaused),
    ([0xd3, 0xb3, 0x8b, 0x4c], FlammError::GuardianMayOnlyTighten),
    ([0x4b, 0xdf, 0xed, 0x04], FlammError::HookInvalid),
    ([0x31, 0xc1, 0x1a, 0x6e], FlammError::HookInvalidAddress),
    ([0x5e, 0xcd, 0x35, 0xf4], FlammError::HookSetNotReady),
    ([0x44, 0xab, 0x10, 0xa2], FlammError::ImplementationCodeChanged),
    ([0x69, 0x53, 0x34, 0xe2], FlammError::ImplementationNotReady),
    ([0x0b, 0x29, 0xcb, 0x79], FlammError::ImplementationVersion),
    ([0x10, 0x00, 0xbd, 0xe7], FlammError::InsufficientBridge),
    ([0x2b, 0x3b, 0xc9, 0x85], FlammError::InsufficientCollateral),
    ([0xc7, 0x30, 0x33, 0x3f], FlammError::InsufficientLiquidity),
    ([0x2c, 0x52, 0x11, 0xc6], FlammError::InvalidAmount),
    ([0xa8, 0x83, 0x43, 0x57], FlammError::InvalidBounds),
    ([0x35, 0xbe, 0x3a, 0xc8], FlammError::InvalidConfig),
    ([0xf9, 0x2e, 0xe8, 0xa9], FlammError::InvalidInitialization),
    ([0x1e, 0x4f, 0x7d, 0x8c], FlammError::InvalidPair),
    ([0xcd, 0x21, 0x50, 0x06], FlammError::InvalidPrice),
    ([0xa4, 0xfe, 0x5f, 0x8b], FlammError::IrmUnreadable),
    ([0x65, 0xb4, 0x22, 0x9b], FlammError::KindUnknown),
    ([0x52, 0xbf, 0x12, 0x35], FlammError::LedgerBoundBreached),
    ([0x2e, 0xa2, 0xdc, 0xe8], FlammError::LevBelowFloor),
    ([0x78, 0xd6, 0x12, 0xf2], FlammError::LevPaused),
    ([0x28, 0x85, 0x17, 0x30], FlammError::LevValueLeak),
    ([0x27, 0x33, 0xd0, 0xd9], FlammError::LeverageDisabled),
    ([0xdf, 0xab, 0x7f, 0xcc], FlammError::LoanAssetInExceeded),
    ([0xcd, 0x56, 0x76, 0x2d], FlammError::LoanAssetKnown),
    ([0x35, 0xaa, 0xe9, 0xa5], FlammError::LoanAssetOutBelowMin),
    ([0x81, 0xec, 0x41, 0xe2], FlammError::LoanLegRequired),
    ([0x7f, 0x5c, 0xab, 0x2b], FlammError::LoanNotEmpty),
    ([0x95, 0x5d, 0x4b, 0x2f], FlammError::LoanRetired),
    ([0x63, 0x2d, 0x8a, 0x7f], FlammError::LoanSwapDisabled),
    ([0x77, 0xd4, 0x1c, 0x1e], FlammError::LtvFloorBreached),
    ([0xc0, 0xd0, 0xdb, 0xc5], FlammError::LtvLltvGap),
    ([0x20, 0xa3, 0x98, 0x3b], FlammError::LtvStepTooLarge),
    ([0x73, 0x27, 0x82, 0x26], FlammError::MarketUnknown),
    ([0xba, 0x02, 0xf2, 0xb1], FlammError::NestedCallback),
    ([0x11, 0xa3, 0xfb, 0xc6], FlammError::NoDebt),
    ([0xa9, 0x75, 0xe0, 0x0e], FlammError::NoDeployerScheduled),
    ([0x85, 0x8e, 0xe0, 0x47], FlammError::NoHookSetScheduled),
    ([0x96, 0x4a, 0x1f, 0xe3], FlammError::NoImplementationScheduled),
    ([0x9e, 0x31, 0x9a, 0xa4], FlammError::NoLoanAssetScheduled),
    ([0xbf, 0x15, 0x21, 0x0d], FlammError::NoNewObservation),
    ([0x76, 0xa2, 0x3c, 0xbf], FlammError::NoRepaySnapshot),
    ([0x1d, 0xdd, 0x47, 0x3b], FlammError::NoVenueScheduled),
    ([0x6e, 0x27, 0xae, 0x10], FlammError::NotBootstrapped),
    ([0x56, 0xb3, 0x81, 0xa5], FlammError::NotCurator),
    ([0x32, 0xcc, 0x72, 0x36], FlammError::NotFactory),
    ([0x2c, 0x3b, 0xe8, 0x50], FlammError::NotGuardianOrCurator),
    ([0x87, 0x13, 0x8d, 0x5c], FlammError::NotInitialized),
    ([0xd7, 0xe6, 0xbc, 0xf8], FlammError::NotInitializing),
    ([0xf5, 0x12, 0xb2, 0x78], FlammError::NotKeeper),
    ([0xc1, 0xd8, 0xf9, 0x4b], FlammError::NotKeeperOrCurator),
    ([0xe5, 0x1b, 0x51, 0x23], FlammError::NotMorpho),
    ([0x30, 0xcd, 0x74, 0x71], FlammError::NotOwner),
    ([0x6c, 0xd6, 0x02, 0x01], FlammError::NotPaused),
    ([0x6f, 0x61, 0xf6, 0x41], FlammError::NotPool),
    ([0x1d, 0x6c, 0x5b, 0x13], FlammError::NotProtocolSafe),
    ([0xab, 0xa4, 0x73, 0x39], FlammError::NotRegistered),
    ([0x91, 0x65, 0x52, 0x01], FlammError::NotRouter),
    ([0x7c, 0x3f, 0xa3, 0xaf], FlammError::NothingToFill),
    ([0x07, 0x9a, 0x93, 0xc0], FlammError::NothingToMaintain),
    ([0xd7, 0x7d, 0x93, 0xd3], FlammError::NothingToMove),
    ([0xf9, 0xb4, 0x67, 0x8a], FlammError::NotionalCap),
    ([0xab, 0xa9, 0xac, 0x38], FlammError::OracleBand),
    ([0xc6, 0x52, 0x0d, 0xe3], FlammError::OutputAboveCeiling),
    ([0x2f, 0xab, 0x92, 0xca], FlammError::OwnershipCannotBeRenounced),
    ([0x9e, 0x87, 0xfa, 0xc8], FlammError::Paused),
    ([0x85, 0xc2, 0xbe, 0x22], FlammError::PegBroken),
    ([0x67, 0xbe, 0x49, 0xe5], FlammError::PoolBindingMismatch),
    ([0xd5, 0x3c, 0x78, 0xd1], FlammError::PredictionMismatch),
    ([0xfe, 0x85, 0xbb, 0x51], FlammError::PriceBand),
    ([0x48, 0xe1, 0x48, 0x43], FlammError::PriceFeedMismatch),
    ([0xc0, 0xfd, 0x97, 0x0f], FlammError::PriceUnchecked),
    ([0xb2, 0xef, 0x0f, 0x93], FlammError::RateCeiling),
    ([0xed, 0x3b, 0xa6, 0xa6], FlammError::Reentrant),
    ([0xd6, 0xf8, 0xf8, 0x9c], FlammError::RoomExceeded),
    ([0x84, 0xf5, 0x27, 0x0a], FlammError::RoomExhausted),
    ([0x03, 0x2b, 0x3d, 0x00], FlammError::SequencerDown),
    ([0xc3, 0x73, 0x4d, 0xc2], FlammError::SequencerGrace),
    ([0x7d, 0xd3, 0x7f, 0x70], FlammError::Slippage),
    ([0x73, 0x1d, 0x22, 0x8a], FlammError::SpreadOutOfBounds),
    ([0xc8, 0x1f, 0x12, 0x09], FlammError::SpreadUnavailable),
    ([0x95, 0x34, 0xe2, 0xb0], FlammError::StalePair),
    ([0x81, 0x92, 0x79, 0x29], FlammError::StalePrice),
    ([0xf5, 0x8f, 0x73, 0x3a], FlammError::SupplyCapExceeded),
    ([0x0f, 0xcc, 0x23, 0x7b], FlammError::TooManyLoanAssets),
    ([0x2a, 0xec, 0xb6, 0xac], FlammError::TooManyVenues),
    ([0x7d, 0xb7, 0xa0, 0x74], FlammError::Unhealthy),
    ([0x81, 0xa3, 0xb1, 0xbe], FlammError::UnknownToken),
    ([0x90, 0xa2, 0xca, 0xf2], FlammError::Unsupported),
    ([0xc8, 0x07, 0x12, 0x40], FlammError::VenueDisabled),
    ([0x57, 0x4d, 0x7f, 0xff], FlammError::VenueIsRetired),
    ([0xde, 0x7e, 0x3c, 0xb2], FlammError::VenueMismatch),
    ([0x8d, 0x13, 0xb1, 0xc8], FlammError::VenueNotEmpty),
    ([0xbf, 0x13, 0x9b, 0xcd], FlammError::VenueQuarantined),
    ([0x43, 0x0d, 0x4a, 0x31], FlammError::VenueUnknown),
    ([0xd9, 0x2e, 0x23, 0x3d], FlammError::ZeroAddress),
    ([0x4d, 0xfb, 0xa0, 0x23], FlammError::ZeroPrice),
    ([0x98, 0x11, 0xe0, 0xc7], FlammError::ZeroShares),
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
        assert_eq!(SELECTORS.len(), 147);
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
