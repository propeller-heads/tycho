use crate::abi::dynamic_swap_fee_module::events::{
    CustomFeeSet, DynamicFeeReset, FeeCapSet, InitialFeeDisabled, InitialFeeSet, ScalingFactorSet,
    SetCustomFee,
};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use substreams::scalar::BigInt;
use substreams_ethereum::{pb::eth::v2 as eth, Event};
use substreams_helper::hex::Hexable;

pub const DYNAMIC_FEE_MODULE_ATTRIBUTE: &str = "dynamic_fee_module";

pub const DYNAMIC_FEE_CONFIG_ATTRIBUTES: [&str; 5] =
    ["dfc_baseFee", "dfc_scalingFactor", "dfc_feeCap", "dfc_initialFeeEnabled", "dfc_initialFee"];

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
    /// The base fee event of the fee module UP deploys on Robinhood Chain. Same signature and
    /// meaning as [`DynamicFeeEvent::CustomFeeSet`] under a different name; the two modules are
    /// otherwise identical.
    SetCustomFee(SetCustomFee),
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
        } else if let Some(event) = SetCustomFee::match_and_decode(log) {
            Some(Self::SetCustomFee(event))
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
            Self::SetCustomFee(event) => &event.pool,
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
            Self::SetCustomFee(event) => vec![("dfc_baseFee", event.fee.clone())],
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
    /// Protocol type emitted on every component, so each deployment indexed by this package is
    /// registered under its own name for simulation and execution.
    pub protocol_type_name: String,
    /// Deployment block of the earliest configured dynamic fee module, when it is known.
    #[serde(default)]
    pub first_dynamic_fee_module_block: Option<u64>,
    /// Dynamic fee module to mark every new pool with at creation, as a hex address.
    ///
    /// A pool is otherwise marked by its first event from a configured module. That is enough
    /// while the module's `defaultScalingFactor` is zero, because a pool it holds no config for
    /// then pays its tick spacing fee. A module with a non-zero default charges a dynamic fee to
    /// every pool, so a manifest whose factory set such a module before creating its first pool
    /// names it here.
    #[serde(default)]
    pub creation_dynamic_fee_module: Option<String>,
}

impl Params {
    pub fn parse_from_query(input: &str) -> Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse query params: {}", e))
    }

    /// Whether a block can carry an event of a configured dynamic fee module.
    ///
    /// A module emits nothing before it is deployed, so a manifest that declares
    /// `first_dynamic_fee_module_block` skips the blocks below it without walking their logs — on a
    /// chain whose factory long predates its fee module that is most of the indexed history.
    /// Without the parameter every block is scanned, which costs time but never changes the
    /// emitted state.
    pub fn processes_dynamic_fee_config(&self, block_number: u64) -> bool {
        self.first_dynamic_fee_module_block
            .is_none_or(|first_block| block_number >= first_block)
    }

    /// The `creation_dynamic_fee_module` as a raw 20-byte address, or `None` when it is not set.
    ///
    /// Fails when the value is not a 20-byte hex address or is not one of `dynamic_fee_modules`:
    /// a marker naming a module this package does not index would send every pool without a
    /// fee-module event to its tick spacing fee instead of the module's default.
    pub fn creation_dynamic_fee_module_address(&self) -> Result<Option<Vec<u8>>> {
        let Some(module) = self
            .creation_dynamic_fee_module
            .as_deref()
        else {
            return Ok(None);
        };
        let address = hex::decode(module).map_err(|e| {
            anyhow!("creation_dynamic_fee_module {module:?} is not a hex address: {e}")
        })?;
        if address.len() != 20 {
            return Err(anyhow!(
                "creation_dynamic_fee_module {module:?} has {} bytes, expected a 20-byte address",
                address.len()
            ));
        }
        for configured in &self.dynamic_fee_modules {
            let configured_address = hex::decode(configured).map_err(|e| {
                anyhow!("dynamic_fee_modules entry {configured:?} is not a hex address: {e}")
            })?;
            if configured_address == address {
                return Ok(Some(address));
            }
        }
        Err(anyhow!(
            "creation_dynamic_fee_module {module:?} is not one of dynamic_fee_modules {:?}; add it \
             there or remove the marker",
            self.dynamic_fee_modules
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dynamic_fee_config_initialized_key, dynamic_fee_config_key, tick_spacing_fee_key, Params,
    };

    #[test]
    fn creation_dynamic_fee_module_is_optional() {
        let query = "factories[]=11&dynamic_fee_modules[]=22&protocol_type_name=pool";

        let without = Params::parse_from_query(query).expect("valid params");
        let with = Params::parse_from_query(&format!("{query}&creation_dynamic_fee_module=22"))
            .expect("valid params");

        assert_eq!(without.creation_dynamic_fee_module, None);
        assert_eq!(
            with.creation_dynamic_fee_module
                .as_deref(),
            Some("22")
        );
    }

    const MODULE: &str = "8649D7A63499a6Cd5E400EF2DdB4AD317359c23c";

    fn params_with_marker(marker: &str) -> Params {
        Params::parse_from_query(&format!(
            "factories[]=11&dynamic_fee_modules[]={MODULE}&protocol_type_name=pool\
             &creation_dynamic_fee_module={marker}"
        ))
        .expect("valid params")
    }

    #[test]
    fn creation_dynamic_fee_module_address_decodes_a_configured_module() {
        let without = Params::parse_from_query(&format!(
            "factories[]=11&dynamic_fee_modules[]={MODULE}&protocol_type_name=pool"
        ))
        .expect("valid params");

        assert_eq!(
            without
                .creation_dynamic_fee_module_address()
                .expect("no marker"),
            None
        );
        // Membership compares addresses, not their spelling.
        assert_eq!(
            params_with_marker(&MODULE.to_lowercase())
                .creation_dynamic_fee_module_address()
                .expect("configured module"),
            Some(hex::decode(MODULE).expect("hex"))
        );
    }

    #[test]
    fn creation_dynamic_fee_module_address_rejects_a_malformed_address() {
        for marker in ["0x8649D7A63499a6Cd5E400EF2DdB4AD317359c23c", "8649D7A6", "zz"] {
            let error = params_with_marker(marker)
                .creation_dynamic_fee_module_address()
                .expect_err("malformed marker");

            assert!(error.to_string().contains(marker), "{error}");
        }
    }

    #[test]
    fn creation_dynamic_fee_module_address_rejects_an_unconfigured_module() {
        let error = params_with_marker("0Dca13b1039277d82B27ADd86d06195B8E9BBfDb")
            .creation_dynamic_fee_module_address()
            .expect_err("module missing from dynamic_fee_modules");

        assert!(
            error
                .to_string()
                .contains("is not one of dynamic_fee_modules"),
            "{error}"
        );
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
