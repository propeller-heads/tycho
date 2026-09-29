use anyhow::{anyhow, Result};
use serde::{Deserialize, Deserializer};

/// Every generation of a factory family deploys through the same `create` signature, so a family
/// takes a comma-separated list of factory addresses.
fn address_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Vec<u8>>, D::Error> {
    let raw = String::deserialize(deserializer)?;
    raw.split(',')
        .filter(|address| !address.is_empty())
        .map(|address| hex::decode(address).map_err(serde::de::Error::custom))
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeploymentConfig {
    #[serde(with = "hex::serde")]
    pub vault: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub vault_extension: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub batch_router: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub permit2: Vec<u8>,
    #[serde(deserialize_with = "address_list")]
    pub weighted_factory: Vec<Vec<u8>>,
    #[serde(deserialize_with = "address_list")]
    pub stable_factory: Vec<Vec<u8>>,
    #[serde(with = "hex::serde")]
    pub reclamm_factory: Vec<u8>,
    /// `StableSurgePoolFactory` generations; optional, empty when the chain has none.
    #[serde(default, deserialize_with = "address_list")]
    pub stable_surge_factory: Vec<Vec<u8>>,
    /// The `StableSurgeHook` those factories attach to every pool (`getStableSurgeHook()`). Its
    /// storage holds each pool's surge parameters, so it is indexed alongside the pool.
    #[serde(default, with = "hex::serde")]
    pub stable_surge_hook: Vec<u8>,
    #[serde(default)]
    pub skip_rate_provider_pools: bool,
}

impl DeploymentConfig {
    pub fn parse(input: &str) -> Result<Self> {
        let config: Self = serde_qs::from_str(input)
            .map_err(|e| anyhow!("Failed to parse deployment params: {}", e))?;
        // A StableSurge pool is only quotable with its hook's storage, so the two come together.
        match (config.stable_surge_factory.is_empty(), config.stable_surge_hook.len()) {
            (true, 0) | (false, 20) => Ok(config),
            _ => Err(anyhow!("stable_surge_factory and stable_surge_hook must be set together")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mainnet_deployment_params() {
        let config = DeploymentConfig::parse(
            "vault=ba1333333333a1ba1108e8412f11850a5c319ba9\
             &vault_extension=0e8b07657d719b86e06bf0806d6729e3d528c9a9\
             &batch_router=136f1efcc3f8f88516b9e94110d56fdbfb1778d1\
             &permit2=000000000022d473030f116ddee9f6b43ac78ba3\
             &weighted_factory=201efd508c8dfe9de1a13c2452863a78cb2a86cc\
             &stable_factory=b9d01ca61b9c181da1051bfdd28e1097e920ab14\
             &reclamm_factory=3ccd78683effffddc1a16f5553c896ac6d3ab7ff",
        )
        .unwrap();

        assert_eq!(config.vault.len(), 20);
        assert_eq!(
            config.weighted_factory,
            vec![hex::decode("201efd508c8dfe9de1a13c2452863a78cb2a86cc").unwrap()]
        );
        assert_eq!(config.reclamm_factory.len(), 20);
        assert!(!config.skip_rate_provider_pools);
        assert!(config.stable_surge_factory.is_empty());
    }

    #[test]
    fn parses_factory_lists_and_stable_surge_params() {
        let config = DeploymentConfig::parse(
            "vault=ba1333333333a1ba1108e8412f11850a5c319ba9\
             &vault_extension=0e8b07657d719b86e06bf0806d6729e3d528c9a9\
             &batch_router=85a80afee867adf27b50bdb7b76da70f1e853062\
             &permit2=000000000022d473030f116ddee9f6b43ac78ba3\
             &weighted_factory=76578ecf9a141296ec657847fb45b0585bcda3a6,4bdcc2fb18aeb9e2d281b0278d946445070eada7\
             &stable_factory=f5cddf6fed9c589f1be04899f48f9738531dad59\
             &reclamm_factory=3ccd78683effffddc1a16f5553c896ac6d3ab7ff\
             &stable_surge_factory=6b5da774890db7b7b96c6f44e6a4b0f657399e2e,db8d758bcb971e482b2c45f7f8a7740283a1bd3a\
             &stable_surge_hook=6817149cb753bf529565b4d023d7507ed2ff4bc0",
        )
        .unwrap();

        assert_eq!(config.weighted_factory.len(), 2);
        assert_eq!(config.stable_surge_factory.len(), 2);
        assert_eq!(config.stable_surge_hook.len(), 20);
    }

    #[test]
    fn rejects_stable_surge_factory_without_hook() {
        let result = DeploymentConfig::parse(
            "vault=ba1333333333a1ba1108e8412f11850a5c319ba9\
             &vault_extension=0e8b07657d719b86e06bf0806d6729e3d528c9a9\
             &batch_router=85a80afee867adf27b50bdb7b76da70f1e853062\
             &permit2=000000000022d473030f116ddee9f6b43ac78ba3\
             &weighted_factory=201efd508c8dfe9de1a13c2452863a78cb2a86cc\
             &stable_factory=f5cddf6fed9c589f1be04899f48f9738531dad59\
             &reclamm_factory=3ccd78683effffddc1a16f5553c896ac6d3ab7ff\
             &stable_surge_factory=db8d758bcb971e482b2c45f7f8a7740283a1bd3a",
        );

        assert!(result.is_err());
    }

    #[test]
    fn parses_skip_rate_provider_pools_param() {
        let config = DeploymentConfig::parse(
            "vault=ba1333333333a1ba1108e8412f11850a5c319ba9\
             &vault_extension=0e8b07657d719b86e06bf0806d6729e3d528c9a9\
             &batch_router=136f1efcc3f8f88516b9e94110d56fdbfb1778d1\
             &permit2=000000000022d473030f116ddee9f6b43ac78ba3\
             &weighted_factory=201efd508c8dfe9de1a13c2452863a78cb2a86cc\
             &stable_factory=b9d01ca61b9c181da1051bfdd28e1097e920ab14\
             &reclamm_factory=3ccd78683effffddc1a16f5553c896ac6d3ab7ff\
             &skip_rate_provider_pools=true",
        )
        .unwrap();

        assert!(config.skip_rate_provider_pools);
    }
}
