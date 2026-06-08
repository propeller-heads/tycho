pub mod blockchain;
pub mod contract;
pub mod error;
pub mod protocol;
pub mod token;

use std::{collections::HashMap, fmt::Display, str::FromStr};

use arrayvec::ArrayString;
use deepsize::DeepSizeOf;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use token::Token;

use crate::{dto, Bytes};

/// Address hash literal type to uniquely identify contracts/accounts on a
/// blockchain.
pub type Address = Bytes;

/// Block hash literal type to uniquely identify a block in the chain and
/// likely across chains.
pub type BlockHash = Bytes;

/// Transaction hash literal type to uniquely identify a transaction in the
/// chain and likely across chains.
pub type TxHash = Bytes;

/// Smart contract code is represented as a byte vector containing opcodes.
pub type Code = Bytes;

/// The hash of a contract's code is used to identify it.
pub type CodeHash = Bytes;

/// The balance of an account is a big endian serialised integer of variable size.
pub type Balance = Bytes;

/// Key literal type of the contract store.
pub type StoreKey = Bytes;

/// Key literal type of the attribute store.
pub type AttrStoreKey = String;

/// Value literal type of the contract store.
pub type StoreVal = Bytes;

/// A binary key-value store for an account.
pub type ContractStore = HashMap<StoreKey, StoreVal>;
pub type ContractStoreDeltas = HashMap<StoreKey, Option<StoreVal>>;
pub type AccountToContractStoreDeltas = HashMap<Address, ContractStoreDeltas>;

/// Component id literal type to uniquely identify a component.
pub type ComponentId = String;

/// Protocol system literal type to uniquely identify a protocol system.
pub type ProtocolSystem = String;

/// Entry point id literal type to uniquely identify an entry point.
pub type EntryPointId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Error, Debug, PartialEq)]
pub enum ChainAddressError {
    #[error("address is {0} bytes, max is 32")]
    TooLong(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChainTokenConfig {
    pub address: ChainAddress,
    pub symbol: ArrayString<8>,
    pub decimals: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TvlThresholds {
    pub low: u64,
    pub medium: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CustomChainConfig {
    pub name: ArrayString<32>,
    pub chain_id: u64,
    pub block_time_secs: u64,
    pub native: ChainTokenConfig,
    pub wrapped_native: ChainTokenConfig,
    pub default_tvl_thresholds: TvlThresholds,
}

impl DeepSizeOf for CustomChainConfig {
    fn deep_size_of(&self) -> usize {
        0
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default, DeepSizeOf)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    #[default]
    Ethereum,
    Starknet,
    ZkSync,
    Arbitrum,
    Base,
    Bsc,
    Unichain,
    Polygon,
    Custom(CustomChainConfig),
}

impl FromStr for Chain {
    type Err = strum::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ethereum" => Ok(Chain::Ethereum),
            "starknet" => Ok(Chain::Starknet),
            "zksync" => Ok(Chain::ZkSync),
            "arbitrum" => Ok(Chain::Arbitrum),
            "base" => Ok(Chain::Base),
            "bsc" => Ok(Chain::Bsc),
            "unichain" => Ok(Chain::Unichain),
            "polygon" => Ok(Chain::Polygon),
            _ => Err(strum::ParseError::VariantNotFound),
        }
    }
}

impl Display for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Chain::Ethereum => f.write_str("ethereum"),
            Chain::Starknet => f.write_str("starknet"),
            Chain::ZkSync => f.write_str("zksync"),
            Chain::Arbitrum => f.write_str("arbitrum"),
            Chain::Base => f.write_str("base"),
            Chain::Bsc => f.write_str("bsc"),
            Chain::Unichain => f.write_str("unichain"),
            Chain::Polygon => f.write_str("polygon"),
            Chain::Custom(cfg) => f.write_str(cfg.name.as_str()),
        }
    }
}

impl From<dto::Chain> for Chain {
    fn from(value: dto::Chain) -> Self {
        match value {
            dto::Chain::Ethereum => Chain::Ethereum,
            dto::Chain::Starknet => Chain::Starknet,
            dto::Chain::ZkSync => Chain::ZkSync,
            dto::Chain::Arbitrum => Chain::Arbitrum,
            dto::Chain::Base => Chain::Base,
            dto::Chain::Bsc => Chain::Bsc,
            dto::Chain::Unichain => Chain::Unichain,
            dto::Chain::Polygon => Chain::Polygon,
            dto::Chain::Custom(cfg) => Chain::Custom(cfg),
        }
    }
}

impl From<dto::ChangeType> for ChangeType {
    fn from(value: dto::ChangeType) -> Self {
        match value {
            dto::ChangeType::Update => ChangeType::Update,
            dto::ChangeType::Creation => ChangeType::Creation,
            dto::ChangeType::Deletion => ChangeType::Deletion,
            dto::ChangeType::Unspecified => ChangeType::Update,
        }
    }
}

fn native_eth(chain: Chain) -> Token {
    Token::new(
        &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
        "ETH",
        18,
        0,
        &[Some(2300)],
        chain,
        100,
    )
}

fn native_bsc(chain: Chain) -> Token {
    Token::new(
        &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
        "BNB",
        18,
        0,
        &[Some(2300)],
        chain,
        100,
    )
}

fn wrapped_native_eth(chain: Chain, address: &str) -> Token {
    Token::new(&Bytes::from_str(address).unwrap(), "WETH", 18, 0, &[Some(2300)], chain, 100)
}

fn native_pol(chain: Chain) -> Token {
    Token::new(
        &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
        "POL",
        18,
        0,
        &[Some(2300)],
        chain,
        100,
    )
}

fn native_custom(cfg: &CustomChainConfig) -> Token {
    let addr = Bytes::from(cfg.native.address.as_bytes().to_vec());
    Token::new(
        &addr,
        cfg.native.symbol.as_str(),
        cfg.native.decimals as u32,
        0,
        &[Some(2300)],
        Chain::Custom(*cfg),
        100,
    )
}

fn wrapped_native_bsc(chain: Chain, address: &str) -> Token {
    Token::new(&Bytes::from_str(address).unwrap(), "WBNB", 18, 0, &[Some(2300)], chain, 100)
}

fn wrapped_native_pol(chain: Chain, address: &str) -> Token {
    Token::new(&Bytes::from_str(address).unwrap(), "WMATIC", 18, 0, &[Some(2300)], chain, 100)
}

fn wrapped_native_custom(cfg: &CustomChainConfig) -> Token {
    let addr = Bytes::from(
        cfg.wrapped_native
            .address
            .as_bytes()
            .to_vec(),
    );
    Token::new(
        &addr,
        cfg.wrapped_native.symbol.as_str(),
        cfg.wrapped_native.decimals as u32,
        0,
        &[Some(2300)],
        Chain::Custom(*cfg),
        100,
    )
}

impl Chain {
    pub fn id(&self) -> u64 {
        match self {
            Chain::Ethereum => 1,
            Chain::ZkSync => 324,
            Chain::Arbitrum => 42161,
            Chain::Starknet => 0,
            Chain::Base => 8453,
            Chain::Bsc => 56,
            Chain::Unichain => 130,
            Chain::Polygon => 137,
            Chain::Custom(cfg) => cfg.chain_id,
        }
    }

    /// Returns a default TVL threshold in native token units for the given tier.
    ///
    /// Values are approximate and target a USD-equivalent range, not a precise conversion.
    /// Native token prices used: ETH ~$2,000, POL ~$0.10, BNB ~$630.
    /// These prices are volatile, and used as a reference. They should not be updated often,
    /// unless big price movements occour, making an update necessary.
    pub fn default_tvl_threshold(&self, tier: TvlThresholdTier) -> f64 {
        match (self, tier) {
            // ETH-native chains: 10 ETH ≈ $20K, 100 ETH ≈ $200K.
            // Starknet uses ETH-denominated TVL in Tycho (STRK tracked separately).
            (
                Chain::Ethereum |
                Chain::Starknet |
                Chain::ZkSync |
                Chain::Arbitrum |
                Chain::Base |
                Chain::Unichain,
                TvlThresholdTier::Low,
            ) => 10.0,
            (
                Chain::Ethereum |
                Chain::Starknet |
                Chain::ZkSync |
                Chain::Arbitrum |
                Chain::Base |
                Chain::Unichain,
                TvlThresholdTier::Medium,
            ) => 100.0,

            // Polygon (POL ≈ $0.10): 200_000 POL ≈ $20K, 2_000_000 POL ≈ $200K
            (Chain::Polygon, TvlThresholdTier::Low) => 200_000.0,
            (Chain::Polygon, TvlThresholdTier::Medium) => 2_000_000.0,

            // BSC (BNB ≈ $630): 32 BNB ≈ $20K, 320 BNB ≈ $200K
            (Chain::Bsc, TvlThresholdTier::Low) => 32.0,
            (Chain::Bsc, TvlThresholdTier::Medium) => 320.0,

            (Chain::Custom(cfg), TvlThresholdTier::Low) => cfg.default_tvl_thresholds.low as f64,
            (Chain::Custom(cfg), TvlThresholdTier::Medium) => {
                cfg.default_tvl_thresholds.medium as f64
            }
        }
    }

    /// Returns the native token for the chain.
    pub fn native_token(&self) -> Token {
        match self {
            Chain::Ethereum => native_eth(Chain::Ethereum),
            // It was decided that STRK token will be tracked as a dedicated AccountBalance on
            // Starknet accounts and ETH balances will be tracked as a native balance.
            Chain::Starknet => native_eth(Chain::Starknet),
            Chain::ZkSync => native_eth(Chain::ZkSync),
            Chain::Arbitrum => native_eth(Chain::Arbitrum),
            Chain::Base => native_eth(Chain::Base),
            Chain::Bsc => native_bsc(Chain::Bsc),
            Chain::Unichain => native_eth(Chain::Unichain),
            Chain::Polygon => native_pol(Chain::Polygon),
            Chain::Custom(cfg) => native_custom(cfg),
        }
    }

    /// Returns the wrapped native token for the chain.
    pub fn wrapped_native_token(&self) -> Token {
        match self {
            Chain::Ethereum => {
                wrapped_native_eth(Chain::Ethereum, "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
            }
            // Starknet does not have a wrapped native token
            Chain::Starknet => {
                wrapped_native_eth(Chain::Starknet, "0x0000000000000000000000000000000000000000")
            }
            Chain::ZkSync => {
                wrapped_native_eth(Chain::ZkSync, "0x5AEa5775959fBC2557Cc8789bC1bf90A239D9a91")
            }
            Chain::Arbitrum => {
                wrapped_native_eth(Chain::Arbitrum, "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1")
            }
            Chain::Base => {
                wrapped_native_eth(Chain::Base, "0x4200000000000000000000000000000000000006")
            }
            Chain::Bsc => {
                wrapped_native_bsc(Chain::Bsc, "0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c")
            }
            Chain::Unichain => {
                wrapped_native_eth(Chain::Unichain, "0x4200000000000000000000000000000000000006")
            }
            Chain::Polygon => {
                wrapped_native_pol(Chain::Polygon, "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270")
            }
            Chain::Custom(cfg) => wrapped_native_custom(cfg),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ExtractorIdentity {
    pub chain: Chain,
    pub name: String,
}

impl ExtractorIdentity {
    pub fn new(chain: Chain, name: &str) -> Self {
        Self { chain, name: name.to_owned() }
    }
}

impl std::fmt::Display for ExtractorIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.chain, self.name)
    }
}

impl From<ExtractorIdentity> for dto::ExtractorIdentity {
    fn from(value: ExtractorIdentity) -> Self {
        dto::ExtractorIdentity { chain: value.chain.into(), name: value.name }
    }
}

impl From<dto::ExtractorIdentity> for ExtractorIdentity {
    fn from(value: dto::ExtractorIdentity) -> Self {
        Self { chain: value.chain.into(), name: value.name }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct ExtractionState {
    pub name: String,
    pub chain: Chain,
    pub attributes: serde_json::Value,
    pub cursor: Vec<u8>,
    pub block_hash: Bytes,
}

impl ExtractionState {
    pub fn new(
        name: String,
        chain: Chain,
        attributes: Option<serde_json::Value>,
        cursor: &[u8],
        block_hash: Bytes,
    ) -> Self {
        ExtractionState {
            name,
            chain,
            attributes: attributes.unwrap_or_default(),
            cursor: cursor.to_vec(),
            block_hash,
        }
    }
}

#[derive(PartialEq, Debug, Clone, Default, Deserialize, Serialize)]
pub enum ImplementationType {
    #[default]
    Vm,
    Custom,
}

#[derive(PartialEq, Debug, Clone, Default, Deserialize, Serialize)]
pub enum FinancialType {
    #[default]
    Swap,
    Psm,
    Debt,
    Leverage,
}

#[derive(Debug, PartialEq, Clone, Default, Deserialize, Serialize)]
pub struct ProtocolType {
    pub name: String,
    pub financial_type: FinancialType,
    pub attribute_schema: Option<serde_json::Value>,
    pub implementation: ImplementationType,
}

impl ProtocolType {
    pub fn new(
        name: String,
        financial_type: FinancialType,
        attribute_schema: Option<serde_json::Value>,
        implementation: ImplementationType,
    ) -> Self {
        ProtocolType { name, financial_type, attribute_schema, implementation }
    }
}

#[derive(Debug, PartialEq, Eq, Default, Copy, Clone, Deserialize, Serialize, DeepSizeOf)]
pub enum ChangeType {
    #[default]
    Update,
    Deletion,
    Creation,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ContractId {
    pub address: Address,
    pub chain: Chain,
}

/// Uniquely identifies a contract on a specific chain.
impl ContractId {
    pub fn new(chain: Chain, address: Address) -> Self {
        Self { address, chain }
    }

    pub fn address(&self) -> &Address {
        &self.address
    }
}

impl Display for ContractId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: 0x{}", self.chain, hex::encode(&self.address))
    }
}

#[derive(Debug, PartialEq, Clone, Default, Deserialize, Serialize)]
pub struct PaginationParams {
    pub page: i64,
    pub page_size: i64,
}

impl PaginationParams {
    pub fn new(page: i64, page_size: i64) -> Self {
        Self { page, page_size }
    }

    pub fn offset(&self) -> i64 {
        self.page * self.page_size
    }
}

impl From<&dto::PaginationParams> for PaginationParams {
    fn from(value: &dto::PaginationParams) -> Self {
        PaginationParams { page: value.page, page_size: value.page_size }
    }
}

#[derive(Error, Debug, PartialEq)]
pub enum MergeError {
    #[error("Can't merge {0} from differring idendities: Expected {1}, got {2}")]
    IdMismatch(String, String, String),
    #[error("Can't merge {0} from different blocks: 0x{1:x} != 0x{2:x}")]
    BlockMismatch(String, Bytes, Bytes),
    #[error("Can't merge {0} from the same transaction: 0x{1:x}")]
    SameTransaction(String, Bytes),
    #[error("Can't merge {0} with lower transaction index: {1} > {2}")]
    TransactionOrderError(String, u64, u64),
    #[error("Cannot merge: {0}")]
    InvalidState(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CustomChainConfig {
        CustomChainConfig {
            name: ArrayString::from("testchain").unwrap(),
            chain_id: 9999,
            block_time_secs: 5,
            native: ChainTokenConfig {
                address: ChainAddress::new(&[0xAA; 20]).unwrap(),
                symbol: ArrayString::from("TST").unwrap(),
                decimals: 18,
            },
            wrapped_native: ChainTokenConfig {
                address: ChainAddress::new(&[0xBB; 20]).unwrap(),
                symbol: ArrayString::from("WTST").unwrap(),
                decimals: 18,
            },
            default_tvl_thresholds: TvlThresholds { low: 50, medium: 500 },
        }
    }

    #[test]
    fn test_custom_chain_display() {
        assert_eq!(Chain::Custom(test_config()).to_string(), "testchain");
    }

    #[test]
    fn test_from_str_custom_returns_err() {
        assert!("custom".parse::<Chain>().is_err());
        assert!("unknown".parse::<Chain>().is_err());
    }

    #[test]
    fn test_custom_chain_id() {
        assert_eq!(Chain::Custom(test_config()).id(), 9999);
    }

    #[test]
    fn test_custom_chain_tvl_thresholds() {
        let chain = Chain::Custom(test_config());
        assert_eq!(chain.default_tvl_threshold(TvlThresholdTier::Low), 50.0);
        assert_eq!(chain.default_tvl_threshold(TvlThresholdTier::Medium), 500.0);
    }

    #[test]
    fn test_custom_chain_native_token() {
        let chain = Chain::Custom(test_config());
        let token = chain.native_token();
        assert_eq!(token.symbol, "TST");
        assert_eq!(token.decimals, 18);
        assert_eq!(token.chain, chain);
        assert_eq!(token.address, Bytes::from(vec![0xAA; 20]));
    }

    #[test]
    fn test_custom_chain_wrapped_native_token() {
        let chain = Chain::Custom(test_config());
        let token = chain.wrapped_native_token();
        assert_eq!(token.symbol, "WTST");
        assert_eq!(token.chain, chain);
        assert_eq!(token.address, Bytes::from(vec![0xBB; 20]));
    }

    #[test]
    fn test_chain_address_new_rejects_oversized_input() {
        assert_eq!(
            ChainAddress::new(&[0u8; 33]),
            Err(ChainAddressError::TooLong(33))
        );
    }

    #[test]
    fn test_chain_address_as_bytes_returns_active_slice() {
        let addr = ChainAddress::new(&[0xAA; 20]).unwrap();
        assert_eq!(addr.as_bytes(), &[0xAA; 20]);
        assert_eq!(addr.as_bytes().len(), 20);
    }
}
