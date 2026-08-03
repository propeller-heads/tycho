use serde::Deserialize;

/// Deployment-specific addresses and storage layout for a BopAMM venue.
///
/// Supplied through substreams `params` (see `substreams.yaml`) so the module can be
/// re-pointed at a different deployment — or a redeploy with a different storage layout —
/// without code changes. Addresses and the maker slot are hex (no `0x` prefix).
#[derive(Clone, Deserialize)]
pub struct DeploymentConfig {
    /// Settlement contract / swap entrypoint; anchors component ids.
    #[serde(with = "hex::serde")]
    pub settlement: Vec<u8>,
    /// Pricing module holding the asset config and the global maker slot.
    #[serde(with = "hex::serde")]
    pub module: Vec<u8>,
    /// PrioUpdateRegistry holding the per-book quote lanes.
    #[serde(with = "hex::serde")]
    pub registry: Vec<u8>,
    /// Quote-side hub token shared across every book.
    #[serde(with = "hex::serde")]
    pub usdc: Vec<u8>,
    /// Base storage slot of the module's `mapping(assetId => assetConfig)`.
    pub asset_config_base_slot: u64,
    /// Storage slot of the module's global maker wallet.
    #[serde(with = "hex::serde")]
    pub maker_slot: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_params() {
        let config: DeploymentConfig = serde_qs::from_str(
            "settlement=db13ad0fcd134e9c48f2fdaea8f6751a0f5349ca\
             &module=bc60639345dfa607d73b74e88c2d54d8b8ad7cc3\
             &registry=da7afeed01fe625cf15d187a19f94b45f00b8c5f\
             &usdc=a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48\
             &asset_config_base_slot=3\
             &maker_slot=1471eb6eb2c5e789fc3de43f8ce62938c7d1836ec861730447e2ada8fd81017b",
        )
        .unwrap();
        assert_eq!(config.settlement.len(), 20);
        assert_eq!(config.maker_slot.len(), 32);
        assert_eq!(config.asset_config_base_slot, 3);
    }
}
