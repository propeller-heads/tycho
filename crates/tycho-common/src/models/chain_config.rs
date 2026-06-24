//! Value types describing a blockchain's configuration: token metadata, TVL thresholds, and the
//! full `CustomChainConfig` used to describe user-defined chains. Re-exported from `models`.

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
    pub fn new(bytes: &[u8]) -> Result<Self, ChainAddressError> {
        if bytes.len() > 32 {
            return Err(ChainAddressError::TooLong(bytes.len()));
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
pub enum ChainAddressError {
    #[error("address is {0} bytes, max is 32")]
    TooLong(usize),
}

#[derive(Error, Debug, PartialEq)]
pub enum ChainConfigError {
    #[error("invalid hex address '{0}': {1}")]
    InvalidAddress(String, String),
    #[error("address '{0}': {1}")]
    AddressTooLong(String, String),
    #[error("symbol '{0}' too long (max 8 chars)")]
    SymbolTooLong(String),
    #[error("chain name '{0}' too long (max 32 chars)")]
    NameTooLong(String),
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
        let address = ChainAddress::new(&raw)
            .map_err(|e| ChainConfigError::AddressTooLong(address_hex.to_owned(), e.to_string()))?;
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
