use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tycho_common::simulation::errors::SimulationError;

use super::enums::FeeAmount;

/// Fees are in hundredths of a bip; a swap step divides by `FEE_DENOMINATOR - fee`.
const FEE_DENOMINATOR: u32 = 1_000_000;

/// A pool's swap fee together with the tick spacing its liquidity sits on.
///
/// Canonical Uniswap V3 ties the spacing to the fee, which is what [`FeeAmount`] encodes. Forks
/// that let the factory owner enable arbitrary `(fee, tickSpacing)` pairs need both values given
/// explicitly through [`FeeTier::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeTier {
    fee: u32,
    tick_spacing: u16,
}

impl FeeTier {
    /// Creates a fee tier from a fee in hundredths of a bip and a tick spacing.
    ///
    /// Errors when `fee` is not below 1_000_000 (100%), which the swap math divides by, or when
    /// `tick_spacing` is zero.
    pub fn new(fee: u32, tick_spacing: u16) -> Result<Self, SimulationError> {
        if fee >= FEE_DENOMINATOR {
            return Err(SimulationError::InvalidInput(
                format!("Fee {fee} must be below {FEE_DENOMINATOR}"),
                None,
            ));
        }
        if tick_spacing == 0 {
            return Err(SimulationError::InvalidInput("Tick spacing must be positive".into(), None));
        }
        Ok(FeeTier { fee, tick_spacing })
    }

    /// The swap fee in hundredths of a bip.
    pub fn fee(&self) -> u32 {
        self.fee
    }

    /// The spacing between initializable ticks.
    pub fn tick_spacing(&self) -> u16 {
        self.tick_spacing
    }
}

impl From<FeeAmount> for FeeTier {
    fn from(fee: FeeAmount) -> Self {
        let tick_spacing = match fee {
            FeeAmount::Lowest => 1,
            FeeAmount::Lowest2 => 2,
            FeeAmount::Lowest3 => 3,
            FeeAmount::Lowest4 => 4,
            FeeAmount::Low => 10,
            FeeAmount::MediumLow => 50,
            FeeAmount::Medium => 60,
            FeeAmount::MediumHigh => 100,
            FeeAmount::High => 200,
        };
        FeeTier { fee: fee as u32, tick_spacing }
    }
}

/// Serde for `UniswapV3State::fee`. A fee that has a `FeeAmount` variant is written as that
/// variant's name, the form readers that know only `FeeAmount` expect; any other fee is written as
/// a number. Both forms are read.
pub(super) mod fee_serde {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum SerializedFee {
        Named(FeeAmount),
        Pips(u32),
    }

    pub(in crate::evm::protocol::uniswap_v3) fn serialize<S: Serializer>(
        fee: &u32,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let named = i32::try_from(*fee)
            .ok()
            .and_then(|fee| FeeAmount::try_from(fee).ok());
        match named {
            Some(fee) => SerializedFee::Named(fee),
            None => SerializedFee::Pips(*fee),
        }
        .serialize(serializer)
    }

    pub(in crate::evm::protocol::uniswap_v3) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<u32, D::Error> {
        match SerializedFee::deserialize(deserializer)? {
            SerializedFee::Named(fee) => Ok(fee as u32),
            SerializedFee::Pips(fee) => Ok(fee),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::lowest(FeeAmount::Lowest, 100, 1)]
    #[case::lowest2(FeeAmount::Lowest2, 200, 2)]
    #[case::low(FeeAmount::Low, 500, 10)]
    #[case::medium_low(FeeAmount::MediumLow, 2500, 50)]
    #[case::medium(FeeAmount::Medium, 3000, 60)]
    #[case::medium_high(FeeAmount::MediumHigh, 5000, 100)]
    #[case::high(FeeAmount::High, 10_000, 200)]
    fn test_fee_amount_keeps_legacy_spacing(
        #[case] fee: FeeAmount,
        #[case] expected_fee: u32,
        #[case] expected_spacing: u16,
    ) {
        let tier = FeeTier::from(fee);

        assert_eq!(tier.fee(), expected_fee);
        assert_eq!(tier.tick_spacing(), expected_spacing);
    }

    #[rstest]
    #[case::half_bip(50, 10)]
    #[case::zero_fee(0, 1)]
    #[case::largest_fee(999_999, 1)]
    fn test_new_accepts_fee_outside_fee_amount(#[case] fee: u32, #[case] tick_spacing: u16) {
        let tier = FeeTier::new(fee, tick_spacing).unwrap();

        assert_eq!(tier.fee(), fee);
        assert_eq!(tier.tick_spacing(), tick_spacing);
    }

    #[rstest]
    #[case::full_fee(1_000_000, 1)]
    #[case::zero_spacing(500, 0)]
    fn test_new_rejects_out_of_range(#[case] fee: u32, #[case] tick_spacing: u16) {
        assert!(matches!(
            FeeTier::new(fee, tick_spacing),
            Err(SimulationError::InvalidInput(_, None))
        ));
    }
}
