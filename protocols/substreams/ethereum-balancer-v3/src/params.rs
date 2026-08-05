use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::{de, Deserialize, Deserializer};

const ADDRESS_BYTES: usize = 20;

/// The factory deployments of one pool family, resolved by factory address.
///
/// Balancer deploys a new factory contract per pool-type generation while keeping the `create`
/// signature and the `PoolCreated` event stable, so several generations can be indexed by the same
/// decoder. The deployment params key each address by a version label, which is retained here to
/// name the generation in diagnostics — the label itself carries no meaning to the indexing logic.
#[derive(Debug, Clone, Default)]
pub struct FactoryVersions(HashMap<Vec<u8>, String>);

impl FactoryVersions {
    /// Returns the version label configured for the factory at `address`, or `None` when `address`
    /// is not a configured factory of this family.
    pub fn version_of(&self, address: &[u8]) -> Option<&str> {
        self.0.get(address).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of configured generations. Only the manifest checks care how many there are; the
    /// indexing path resolves by address.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl<'de> Deserialize<'de> for FactoryVersions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let by_version = HashMap::<String, String>::deserialize(deserializer)?;
        let mut by_address: HashMap<Vec<u8>, String> = HashMap::with_capacity(by_version.len());

        for (version, address) in by_version {
            let address = hex::decode(
                address
                    .strip_prefix("0x")
                    .unwrap_or(&address),
            )
            .map_err(|e| {
                de::Error::custom(format!(
                    "factory address for version `{version}` is not valid hex: {e}"
                ))
            })?;
            if address.len() != ADDRESS_BYTES {
                return Err(de::Error::custom(format!(
                    "factory address for version `{version}` must be {ADDRESS_BYTES} bytes, got {}",
                    address.len()
                )));
            }
            // Two labels pointing at one contract would make the reported version arbitrary, and
            // is far more likely to be a copy-paste slip than a deliberate alias.
            if let Some(existing) = by_address.insert(address.clone(), version.clone()) {
                return Err(de::Error::custom(format!(
                    "factory {} is configured under both `{existing}` and `{version}`",
                    hex::encode(&address)
                )));
            }
        }

        Ok(Self(by_address))
    }
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
    /// Weighted pool factories, keyed by version label. Absent means the family is not deployed
    /// on this chain, or that the consuming module does not look at factories.
    #[serde(default)]
    pub weighted_factories: FactoryVersions,
    #[serde(default)]
    pub stable_factories: FactoryVersions,
    #[serde(default)]
    pub reclamm_factories: FactoryVersions,
    #[serde(default)]
    pub skip_rate_provider_pools: bool,
}

impl DeploymentConfig {
    pub fn parse(input: &str) -> Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse deployment params: {}", e))
    }

    /// True when no pool factory of any family is configured, which leaves component discovery
    /// with nothing to match against.
    pub fn has_no_factories(&self) -> bool {
        self.weighted_factories.is_empty() &&
            self.stable_factories.is_empty() &&
            self.reclamm_factories.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_PARAMS: &str = "vault=ba1333333333a1ba1108e8412f11850a5c319ba9\
         &vault_extension=0e8b07657d719b86e06bf0806d6729e3d528c9a9\
         &batch_router=136f1efcc3f8f88516b9e94110d56fdbfb1778d1\
         &permit2=000000000022d473030f116ddee9f6b43ac78ba3";

    fn address(hex_address: &str) -> Vec<u8> {
        hex::decode(hex_address).expect("test address must be hex")
    }

    #[test]
    fn parses_mainnet_deployment_params() {
        let config = DeploymentConfig::parse(&format!(
            "{BASE_PARAMS}\
             &weighted_factories[v1]=201efd508c8dfe9de1a13c2452863a78cb2a86cc\
             &stable_factories[v1]=b9d01ca61b9c181da1051bfdd28e1097e920ab14\
             &reclamm_factories[v1]=3ccd78683effffddc1a16f5553c896ac6d3ab7ff"
        ))
        .unwrap();

        assert_eq!(config.vault.len(), ADDRESS_BYTES);
        assert_eq!(
            config
                .weighted_factories
                .version_of(&address("201efd508c8dfe9de1a13c2452863a78cb2a86cc")),
            Some("v1")
        );
        assert_eq!(
            config
                .reclamm_factories
                .version_of(&address("3ccd78683effffddc1a16f5553c896ac6d3ab7ff")),
            Some("v1")
        );
        assert!(!config.skip_rate_provider_pools);
        assert!(!config.has_no_factories());
    }

    #[test]
    fn resolves_several_generations_of_one_family() {
        let config = DeploymentConfig::parse(&format!(
            "{BASE_PARAMS}\
             &weighted_factories[v1]=201efd508c8dfe9de1a13c2452863a78cb2a86cc\
             &weighted_factories[v2]=0x5f2a3e0e4b6e1e0d0f8a9b7c6d5e4f3a2b1c0d9e"
        ))
        .unwrap();

        assert_eq!(
            config
                .weighted_factories
                .version_of(&address("201efd508c8dfe9de1a13c2452863a78cb2a86cc")),
            Some("v1")
        );
        // The `0x` prefix is accepted because both spellings appear in existing manifests.
        assert_eq!(
            config
                .weighted_factories
                .version_of(&address("5f2a3e0e4b6e1e0d0f8a9b7c6d5e4f3a2b1c0d9e")),
            Some("v2")
        );
    }

    #[test]
    fn unconfigured_factory_address_does_not_resolve() {
        let config = DeploymentConfig::parse(&format!(
            "{BASE_PARAMS}&weighted_factories[v1]=201efd508c8dfe9de1a13c2452863a78cb2a86cc"
        ))
        .unwrap();

        assert_eq!(
            config
                .weighted_factories
                .version_of(&address("3ccd78683effffddc1a16f5553c896ac6d3ab7ff")),
            None
        );
        assert!(config.stable_factories.is_empty());
    }

    #[test]
    fn omitting_every_factory_is_reported() {
        let config = DeploymentConfig::parse(BASE_PARAMS).unwrap();

        assert!(config.has_no_factories());
    }

    #[test]
    fn rejects_one_factory_under_two_versions() {
        let error = DeploymentConfig::parse(&format!(
            "{BASE_PARAMS}\
             &weighted_factories[v1]=201efd508c8dfe9de1a13c2452863a78cb2a86cc\
             &weighted_factories[v2]=201efd508c8dfe9de1a13c2452863a78cb2a86cc"
        ))
        .expect_err("one address under two version labels must be rejected");

        assert!(
            error
                .to_string()
                .contains("configured under both"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_factory_address_of_the_wrong_length() {
        let error = DeploymentConfig::parse(&format!(
            "{BASE_PARAMS}&weighted_factories[v1]=201efd508c8dfe9de1a13c"
        ))
        .expect_err("a truncated factory address must be rejected");

        assert!(
            error
                .to_string()
                .contains("must be 20 bytes"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parses_skip_rate_provider_pools_param() {
        let config = DeploymentConfig::parse(&format!(
            "{BASE_PARAMS}\
             &weighted_factories[v1]=201efd508c8dfe9de1a13c2452863a78cb2a86cc\
             &skip_rate_provider_pools=true"
        ))
        .unwrap();

        assert!(config.skip_rate_provider_pools);
    }

    /// Extracts a module's params from a manifest, so the shipped strings are checked as written
    /// rather than copied into the test and left to drift.
    fn manifest_params(manifest: &str, module: &str) -> String {
        let prefix = format!("  {module}: ");
        manifest
            .lines()
            .find_map(|line| line.strip_prefix(prefix.as_str()))
            .unwrap_or_else(|| panic!("no `{module}` params in manifest"))
            .trim()
            .to_string()
    }

    #[test]
    fn every_shipped_manifest_configures_the_factories_it_means_to() {
        let manifests = [
            ("mainnet", include_str!("../substreams.yaml")),
            ("base", include_str!("../base-balancer-v3.yaml")),
            ("arbitrum", include_str!("../arbitrum-balancer-v3.yaml")),
            ("gnosis", include_str!("../gnosis-balancer-v3.yaml")),
        ];

        for (chain, manifest) in manifests {
            for module in ["map_components", "store_token_mapping", "map_protocol_changes"] {
                let params = manifest_params(manifest, module);
                let config = DeploymentConfig::parse(&params)
                    .unwrap_or_else(|e| panic!("{chain} {module} params must parse: {e}"));

                // Two weighted and three stable generations are deployed on every chain we ship.
                assert_eq!(config.weighted_factories.len(), 2, "{chain} {module} weighted");
                assert_eq!(config.stable_factories.len(), 3, "{chain} {module} stable");
                // reCLAMM is pinned to one generation: `balancer-maths-rust` implements the first
                // generation separately, so only the newest may share this decoder.
                assert_eq!(config.reclamm_factories.len(), 1, "{chain} {module} reclamm");
            }
        }
    }
}
