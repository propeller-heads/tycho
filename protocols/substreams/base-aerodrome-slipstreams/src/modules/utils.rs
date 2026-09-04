use crate::abi::dynamic_swap_fee_module::events::{
    CustomFeeSet, DynamicFeeReset, FeeCapSet, InitialFeeDisabled, InitialFeeSet, ScalingFactorSet,
};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use substreams::scalar::BigInt;
use substreams_ethereum::{pb::eth::v2 as eth, Event};
use substreams_helper::hex::Hexable;

pub const DYNAMIC_FEE_CONFIG_ATTRIBUTES: [&str; 5] =
    ["dfc_baseFee", "dfc_scalingFactor", "dfc_feeCap", "dfc_initialFeeEnabled", "dfc_initialFee"];

// Earliest deployment among the configured fee modules:
// - 0x090b2a6bb475c00e2256e2095a60887cd710803b at block 44_221_569
// - 0xf4ecd78ebeb6d36cf7f80b5b6b41453515fe2785 at block 44_221_840
// - 0x87d8f999bba9343e8099552426775b51c338e8cb at block 44_394_736
const FIRST_DYNAMIC_FEE_MODULE_DEPLOYMENT_BLOCK: u64 = 44_221_569;

pub fn should_process_dynamic_fee_config(block_number: u64) -> bool {
    block_number >= FIRST_DYNAMIC_FEE_MODULE_DEPLOYMENT_BLOCK
}

pub fn dynamic_fee_config_key(pool: &[u8], attribute: &str) -> String {
    format!("{}:{attribute}", pool.to_hex())
}

/// Key for the fee a factory charges on a tick spacing.
///
/// Scoped by factory because each factory keeps its own tick spacing table, and two factories may
/// enable the same tick spacing at different fees.
pub fn tick_spacing_fee_key(factory: &[u8], tick_spacing: i32) -> String {
    format!("{}:tick_spacing_{tick_spacing}", factory.to_hex())
}

pub fn dynamic_fee_config_initialized_key(pool: &[u8]) -> String {
    dynamic_fee_config_key(pool, "initialized")
}

pub enum DynamicFeeEvent {
    CustomFeeSet(CustomFeeSet),
    ScalingFactorSet(ScalingFactorSet),
    FeeCapSet(FeeCapSet),
    InitialFeeSet(InitialFeeSet),
    InitialFeeDisabled(InitialFeeDisabled),
    DynamicFeeReset(DynamicFeeReset),
}

impl DynamicFeeEvent {
    pub fn match_and_decode(log: &eth::Log) -> Option<Self> {
        if let Some(event) = CustomFeeSet::match_and_decode(log) {
            Some(Self::CustomFeeSet(event))
        } else if let Some(event) = ScalingFactorSet::match_and_decode(log) {
            Some(Self::ScalingFactorSet(event))
        } else if let Some(event) = FeeCapSet::match_and_decode(log) {
            Some(Self::FeeCapSet(event))
        } else if let Some(event) = InitialFeeSet::match_and_decode(log) {
            Some(Self::InitialFeeSet(event))
        } else if let Some(event) = InitialFeeDisabled::match_and_decode(log) {
            Some(Self::InitialFeeDisabled(event))
        } else {
            DynamicFeeReset::match_and_decode(log).map(Self::DynamicFeeReset)
        }
    }

    pub fn pool(&self) -> &[u8] {
        match self {
            Self::CustomFeeSet(event) => &event.pool,
            Self::ScalingFactorSet(event) => &event.pool,
            Self::FeeCapSet(event) => &event.pool,
            Self::InitialFeeSet(event) => &event.pool,
            Self::InitialFeeDisabled(event) => &event.pool,
            Self::DynamicFeeReset(event) => &event.pool,
        }
    }

    pub fn config_updates(&self) -> Vec<(&'static str, BigInt)> {
        match self {
            Self::CustomFeeSet(event) => vec![("dfc_baseFee", event.fee.clone())],
            Self::ScalingFactorSet(event) => {
                vec![("dfc_scalingFactor", event.scaling_factor.clone())]
            }
            Self::FeeCapSet(event) => vec![("dfc_feeCap", event.fee_cap.clone())],
            Self::InitialFeeSet(event) => vec![
                ("dfc_initialFeeEnabled", BigInt::from(1)),
                ("dfc_initialFee", event.initial_fee.clone()),
            ],
            Self::InitialFeeDisabled(_) => vec![
                ("dfc_initialFeeEnabled", BigInt::from(0)),
                ("dfc_initialFee", BigInt::from(0)),
            ],
            Self::DynamicFeeReset(_) => vec![
                ("dfc_scalingFactor", BigInt::from(0)),
                ("dfc_feeCap", BigInt::from(0)),
                ("dfc_initialFeeEnabled", BigInt::from(0)),
                ("dfc_initialFee", BigInt::from(0)),
            ],
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Params {
    pub factories: Vec<String>,
    pub dynamic_fee_modules: Vec<String>,
}

impl Params {
    pub fn parse_from_query(input: &str) -> Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse query params: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dynamic_fee_config_initialized_key, dynamic_fee_config_key,
        should_process_dynamic_fee_config, tick_spacing_fee_key,
    };

    #[test]
    fn starts_processing_at_the_first_configured_fee_module_deployment() {
        assert!(!should_process_dynamic_fee_config(44_221_568));
        assert!(should_process_dynamic_fee_config(44_221_569));
    }

    #[test]
    fn dynamic_fee_config_keys_are_scoped_by_pool() {
        let pool = [0x33; 20];

        assert_eq!(
            dynamic_fee_config_key(&pool, "dfc_baseFee"),
            "0x3333333333333333333333333333333333333333:dfc_baseFee"
        );
        assert_eq!(
            dynamic_fee_config_initialized_key(&pool),
            "0x3333333333333333333333333333333333333333:initialized"
        );
    }

    #[test]
    fn tick_spacing_fee_keys_are_scoped_by_factory() {
        assert_eq!(
            tick_spacing_fee_key(&[0x11; 20], 200),
            "0x1111111111111111111111111111111111111111:tick_spacing_200"
        );
        assert_ne!(tick_spacing_fee_key(&[0x11; 20], 200), tick_spacing_fee_key(&[0x22; 20], 200));
    }
}
