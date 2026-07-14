//! Value types describing a blockchain's configuration: token metadata, TVL thresholds, and the
//! full `CustomChainConfig` used to describe user-defined chains. Re-exported from `models`.

use std::{collections::HashMap, sync::OnceLock};

use arrayvec::ArrayString;
use deepsize::DeepSizeOf;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChainAddress {
    bytes: [u8; 32],
    len: u8,
}

impl ChainAddress {
    pub fn new(bytes: &[u8]) -> Result<Self, ChainConfigError> {
        if bytes.len() > 32 {
            return Err(ChainConfigError::AddressTooLong(bytes.len()));
        }
        let mut arr = [0u8; 32];
        arr[..bytes.len()].copy_from_slice(bytes);
        Ok(Self { bytes: arr, len: bytes.len() as u8 })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// Serialized as a `0x`-prefixed hex string so config files and wire payloads use the same
/// representation a human writes (e.g. `"0x0000...0000"`).
impl Serialize for ChainAddress {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{}", hex::encode(self.as_bytes())))
    }
}

impl<'de> Deserialize<'de> for ChainAddress {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        let raw = hex::decode(s.trim_start_matches("0x"))
            .map_err(|e| serde::de::Error::custom(format!("invalid hex address '{s}': {e}")))?;
        ChainAddress::new(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Error, Debug, PartialEq)]
pub enum ChainConfigError {
    #[error("invalid hex address '{0}': {1}")]
    InvalidAddress(String, String),
    #[error("address is {0} bytes, max is 32")]
    AddressTooLong(usize),
    #[error("symbol '{0}' too long (max 8 chars)")]
    SymbolTooLong(String),
    #[error("chain name '{0}' too long (max 32 chars)")]
    NameTooLong(String),
    #[error("unknown chain '{0}': not a built-in chain and no custom config registered")]
    UnknownChain(String),
    #[error("duplicate custom chain config for '{0}'")]
    DuplicateChain(String),
    #[error("failed to read chain config file: {0}")]
    Io(String),
    #[error("failed to parse chain config: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct ChainTokenConfig {
    #[schema(value_type = String)]
    pub(crate) address: ChainAddress,
    #[schema(value_type = String)]
    pub(crate) symbol: ArrayString<8>,
    pub(crate) decimals: u8,
}

impl ChainTokenConfig {
    pub fn try_new(
        address_hex: &str,
        symbol: &str,
        decimals: u8,
    ) -> Result<Self, ChainConfigError> {
        let raw = hex::decode(address_hex.trim_start_matches("0x"))
            .map_err(|e| ChainConfigError::InvalidAddress(address_hex.to_owned(), e.to_string()))?;
        let address = ChainAddress::new(&raw)?;
        let symbol = ArrayString::from(symbol)
            .map_err(|_| ChainConfigError::SymbolTooLong(symbol.to_owned()))?;
        Ok(Self { address, symbol, decimals })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
pub struct TvlThresholds {
    pub(crate) low: f64,
    pub(crate) medium: f64,
}

impl TvlThresholds {
    pub fn new(low: f64, medium: f64) -> Self {
        Self { low, medium }
    }
}

impl PartialEq for TvlThresholds {
    fn eq(&self, other: &Self) -> bool {
        self.low.to_bits() == other.low.to_bits() && self.medium.to_bits() == other.medium.to_bits()
    }
}

impl Eq for TvlThresholds {}

impl std::hash::Hash for TvlThresholds {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.low.to_bits().hash(state);
        self.medium.to_bits().hash(state);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct CustomChainConfig {
    #[schema(value_type = String)]
    pub(crate) name: ArrayString<32>,
    pub(crate) chain_id: u64,
    pub block_time_secs: u64,
    pub(crate) native: ChainTokenConfig,
    pub(crate) wrapped_native: ChainTokenConfig,
    pub(crate) default_tvl_thresholds: TvlThresholds,
}

impl CustomChainConfig {
    pub fn try_new(
        name: &str,
        chain_id: u64,
        block_time_secs: u64,
        native: ChainTokenConfig,
        wrapped_native: ChainTokenConfig,
        default_tvl_thresholds: TvlThresholds,
    ) -> Result<Self, ChainConfigError> {
        let name =
            ArrayString::from(name).map_err(|_| ChainConfigError::NameTooLong(name.to_owned()))?;
        Ok(Self { name, chain_id, block_time_secs, native, wrapped_native, default_tvl_thresholds })
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }
}

impl DeepSizeOf for CustomChainConfig {
    fn deep_size_of_children(&self, _context: &mut deepsize::Context) -> usize {
        0
    }
}

/// Name of a custom chain, guaranteed at construction time to have a matching config in the chain
/// registry. The inner name is private, so a `CustomChainId` — and therefore a `Chain::Custom` —
/// can only be minted through the crate's validating constructors (`Chain::custom`,
/// `Chain::from_str`, `From<dto::Chain>`), never fabricated directly. Serializes transparently as
/// the bare chain name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CustomChainId(ArrayString<32>);

impl CustomChainId {
    /// Builds an id after confirming `name` is registered, returning
    /// [`ChainConfigError::UnknownChain`] when it is absent and [`ChainConfigError::NameTooLong`]
    /// when it exceeds 32 bytes.
    pub(crate) fn checked(
        name: &str,
        registry: &ChainConfigRegistry,
    ) -> Result<Self, ChainConfigError> {
        if !registry.contains(name) {
            return Err(ChainConfigError::UnknownChain(name.to_owned()));
        }
        ArrayString::from(name)
            .map(Self)
            .map_err(|_| ChainConfigError::NameTooLong(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// TVL threshold tiers for chain-aware filtering defaults.
///
/// TVL is denominated in each chain's native token. Since native tokens have different USD values,
/// the same numeric threshold produces wildly different USD-equivalent filters across chains.
/// These tiers provide sensible defaults targeting equivalent USD values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TvlThresholdTier {
    /// Filters out dust pools (~$20K USD equivalent in native token).
    Low,
    /// Filters for pools with meaningful liquidity (~$200K USD equivalent in native token).
    Medium,
}

/// Env var holding the path the registry reads custom-chain config from. Unset means "no custom
/// chains"; there is no default path, so loading never depends on the current working directory.
const TYCHO_CHAINS_CONFIG_ENV: &str = "TYCHO_CHAINS_CONFIG";

/// On-disk shape of the custom-chain config file: a top-level `chains:` list. This is the format of
/// the indexer's `chains.yaml` (its `--chain-config` file), so a consumer can point
/// `TYCHO_CHAINS_CONFIG` at the same file the indexer uses.
#[derive(Debug, Default, Deserialize)]
struct ChainConfigFile {
    #[serde(default)]
    chains: Vec<CustomChainConfig>,
}

/// Resolves the full configuration of custom chains by name.
///
/// Built-in chains are resolved without this registry; it only holds user-defined chains loaded
/// from a YAML source. Build it explicitly via [`ChainConfigRegistry::from_configs`],
/// [`ChainConfigRegistry::from_yaml_file`], or [`ChainConfigRegistry::load_default`], then install
/// it as the process-wide registry with [`init_chain_registry`].
#[derive(Debug, Default, Clone)]
pub struct ChainConfigRegistry {
    custom: HashMap<String, CustomChainConfig>,
}

impl ChainConfigRegistry {
    /// An empty registry — only built-in chains will resolve.
    pub fn empty() -> Self {
        Self { custom: HashMap::new() }
    }

    /// Builds a registry from custom chain configs, keyed by chain name. Rejects duplicate names
    /// with [`ChainConfigError::DuplicateChain`] rather than silently overriding, so an ambiguous
    /// config fails loudly at construction.
    pub fn from_configs(
        configs: impl IntoIterator<Item = CustomChainConfig>,
    ) -> Result<Self, ChainConfigError> {
        let mut custom = HashMap::new();
        for cfg in configs {
            let name = cfg.name().to_owned();
            if custom
                .insert(name.clone(), cfg)
                .is_some()
            {
                return Err(ChainConfigError::DuplicateChain(name));
            }
        }
        Ok(Self { custom })
    }

    /// Reads custom chain configs from the path in `TYCHO_CHAINS_CONFIG`.
    ///
    /// Returns an empty registry when the env var is unset, so runs on built-in chains need no
    /// config. When it is set, the file is loaded as-is: a missing, unreadable, or malformed file
    /// is an error (the operator asked for a config, so a broken one is a mistake, not "no custom
    /// chains"), leaving the caller to decide how to react. Install the result with
    /// [`init_chain_registry`] to make it the process-wide registry.
    pub fn load_default() -> Result<Self, ChainConfigError> {
        let Ok(path) = std::env::var(TYCHO_CHAINS_CONFIG_ENV) else {
            return Ok(Self::empty());
        };
        Self::from_yaml_file(&path)
    }

    /// Like [`load_default`](Self::load_default) but never fails: a broken `TYCHO_CHAINS_CONFIG`
    /// (missing, unreadable, or malformed file) is logged and degrades to an empty registry rather
    /// than aborting. Used by [`chain_registry`] to lazily initialise the process-wide registry, so
    /// a consumer only needs to set `TYCHO_CHAINS_CONFIG` — no explicit init call. Call
    /// [`load_default`](Self::load_default) directly when you need to react to the error.
    pub fn load_default_or_empty() -> Self {
        Self::load_default().unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                "failed to load custom chain config from TYCHO_CHAINS_CONFIG; falling back to an \
                 empty registry"
            );
            Self::empty()
        })
    }

    /// Parses a registry from a YAML file with a top-level `chains:` list.
    pub fn from_yaml_file(path: &str) -> Result<Self, ChainConfigError> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| ChainConfigError::Io(e.to_string()))?;
        Self::from_yaml_str(&contents)
    }

    /// Parses a registry from a YAML string with a top-level `chains:` list.
    pub fn from_yaml_str(contents: &str) -> Result<Self, ChainConfigError> {
        let file: ChainConfigFile =
            serde_yaml::from_str(contents).map_err(|e| ChainConfigError::Parse(e.to_string()))?;
        Self::from_configs(file.chains)
    }

    /// Returns the config for a custom chain by name, if registered.
    pub fn get(&self, name: &str) -> Option<&CustomChainConfig> {
        self.custom.get(name)
    }

    /// Whether a custom chain with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.custom.contains_key(name)
    }
}

static CHAIN_REGISTRY: OnceLock<ChainConfigRegistry> = OnceLock::new();

/// Returns the process-wide chain config registry.
///
/// On first access, lazily initialises itself from `TYCHO_CHAINS_CONFIG` via
/// [`ChainConfigRegistry::load_default_or_empty`] — so a consumer that wants custom chains only
/// needs to set that env var, no explicit init call. A run with the env var unset resolves
/// built-in chains from an empty registry. Install a registry explicitly with
/// [`init_chain_registry`] before first access to override the lazy load (e.g. the indexer
/// building one from its own config).
pub fn chain_registry() -> &'static ChainConfigRegistry {
    CHAIN_REGISTRY.get_or_init(ChainConfigRegistry::load_default_or_empty)
}

/// Installs the process-wide chain config registry explicitly, before any call to
/// [`chain_registry`]. Returns the passed registry back as `Err` if it was already initialised.
pub fn init_chain_registry(registry: ChainConfigRegistry) -> Result<(), ChainConfigRegistry> {
    CHAIN_REGISTRY.set(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_YAML: &str = r#"
chains:
  - name: testchain
    chain_id: 9999
    block_time_secs: 5
    native:
      address: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      symbol: TST
      decimals: 18
    wrapped_native:
      address: "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
      symbol: WTST
      decimals: 18
    default_tvl_thresholds:
      low: 50.0
      medium: 500.0
"#;

    #[test]
    fn parses_yaml_and_resolves_by_name() {
        let registry = ChainConfigRegistry::from_yaml_str(SAMPLE_YAML).unwrap();
        assert!(registry.contains("testchain"));
        let cfg = registry
            .get("testchain")
            .expect("testchain present");
        assert_eq!(cfg.name(), "testchain");
        assert_eq!(cfg.chain_id, 9999);
        assert_eq!(cfg.block_time_secs, 5);
    }

    #[test]
    fn unknown_chain_is_absent() {
        let registry = ChainConfigRegistry::from_yaml_str(SAMPLE_YAML).unwrap();
        assert!(!registry.contains("nope"));
        assert!(registry.get("nope").is_none());
    }

    #[test]
    fn empty_registry_has_no_custom_chains() {
        assert!(!ChainConfigRegistry::empty().contains("testchain"));
    }

    #[test]
    fn from_configs_keys_by_name() {
        let cfg = CustomChainConfig::try_new(
            "mychain",
            42,
            2,
            ChainTokenConfig::try_new("0x00", "AAA", 18).unwrap(),
            ChainTokenConfig::try_new("0x01", "WAAA", 18).unwrap(),
            TvlThresholds::new(1.0, 2.0),
        )
        .unwrap();
        let registry = ChainConfigRegistry::from_configs([cfg]).unwrap();
        assert!(registry.contains("mychain"));
        assert_eq!(
            registry
                .get("mychain")
                .unwrap()
                .chain_id,
            42
        );
    }

    #[test]
    fn empty_chains_list_parses_to_empty_registry() {
        let registry = ChainConfigRegistry::from_yaml_str("chains: []").unwrap();
        assert!(!registry.contains("anything"));
    }

    // These tests mutate the process-global `TYCHO_CHAINS_CONFIG` env var; they rely on nextest
    // running each test in its own process (repo convention). Under plain `cargo test` they would
    // race on the shared env.
    #[test]
    fn load_default_returns_empty_when_env_unset() {
        std::env::remove_var(TYCHO_CHAINS_CONFIG_ENV);
        assert!(!ChainConfigRegistry::load_default()
            .unwrap()
            .contains("anything"));
    }

    #[test]
    fn load_default_errors_when_env_points_at_missing_file() {
        let missing = format!(
            "{}/tycho-chain-config-missing-{}.yaml",
            std::env::temp_dir().display(),
            std::process::id()
        );
        std::env::set_var(TYCHO_CHAINS_CONFIG_ENV, &missing);
        assert!(matches!(ChainConfigRegistry::load_default(), Err(ChainConfigError::Io(_))));
    }

    #[test]
    fn chain_registry_lazy_loads_from_env() {
        let path = format!(
            "{}/tycho-lazy-chain-{}.yaml",
            std::env::temp_dir().display(),
            std::process::id()
        );
        std::fs::write(&path, SAMPLE_YAML).unwrap();
        std::env::set_var(TYCHO_CHAINS_CONFIG_ENV, &path);
        // No explicit init_chain_registry — first access lazily loads from the env path.
        assert!(chain_registry().contains("testchain"));
    }

    #[test]
    fn load_default_or_empty_degrades_to_empty_on_missing_file() {
        let missing = format!(
            "{}/tycho-chain-config-degrade-{}.yaml",
            std::env::temp_dir().display(),
            std::process::id()
        );
        std::env::set_var(TYCHO_CHAINS_CONFIG_ENV, &missing);
        // A broken config must not panic here; it degrades to an empty registry.
        assert!(!ChainConfigRegistry::load_default_or_empty().contains("anything"));
    }

    #[test]
    fn from_configs_duplicate_name_errors() {
        let first = CustomChainConfig::try_new(
            "dup",
            1,
            2,
            ChainTokenConfig::try_new("0x00", "AAA", 18).unwrap(),
            ChainTokenConfig::try_new("0x01", "WAAA", 18).unwrap(),
            TvlThresholds::new(1.0, 2.0),
        )
        .unwrap();
        let second = CustomChainConfig::try_new(
            "dup",
            2,
            2,
            ChainTokenConfig::try_new("0x00", "AAA", 18).unwrap(),
            ChainTokenConfig::try_new("0x01", "WAAA", 18).unwrap(),
            TvlThresholds::new(1.0, 2.0),
        )
        .unwrap();
        assert_eq!(
            ChainConfigRegistry::from_configs([first, second]).unwrap_err(),
            ChainConfigError::DuplicateChain("dup".to_owned())
        );
    }
}
