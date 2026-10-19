use anyhow::{anyhow, Result};
use serde::Deserialize;

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
    #[serde(with = "hex::serde")]
    pub weighted_factory: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub stable_factory: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub reclamm_factory: Vec<u8>,
    #[serde(default)]
    pub skip_rate_provider_pools: bool,
}

impl DeploymentConfig {
    pub fn parse(input: &str) -> Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse deployment params: {}", e))
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
        assert_eq!(config.weighted_factory.len(), 20);
        assert_eq!(config.reclamm_factory.len(), 20);
        assert!(!config.skip_rate_provider_pools);
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
