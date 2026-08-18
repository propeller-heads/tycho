pub mod blockchain;
pub mod chain_config;
pub mod contract;
pub mod error;
pub mod protocol;
pub mod token;

use std::{collections::HashMap, fmt::Display, str::FromStr};

pub use blockchain::{BlockChanges, TxWithContractChanges};
use chain_config::{
    chain_registry, ChainConfigError, ChainConfigRegistry, CustomChainConfig, CustomChainId,
    TvlThresholdTier,
};
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

/// A blockchain Tycho indexes or simulates over.
///
/// Built-in chains are first-class variants: their config (id, native token, block time, TVL
/// tiers) is compile-time and total, so they never fail to resolve. `Custom` is the escape hatch
/// for a chain a self-hoster runs without upstreaming a variant — its config lives in the
/// process-wide registry (see [`chain_config`]) rather than in the enum.
///
/// `Custom` wraps a [`CustomChainId`] whose inner name is private, so a `Chain::Custom` can only be
/// built through the crate's registry-validating constructors ([`Chain::custom`],
/// [`Chain::from_str`], `From<dto::Chain>`) — it cannot be fabricated directly. Because the
/// registry is set-once, a validated `Chain::Custom` stays resolvable for its whole lifetime, so
/// the config accessors only panic on an unreachable invariant; [`Chain::try_id`] and its siblings
/// expose non-panicking variants. The one reachable failure is decoding wire data for a chain the
/// local registry lacks (`From<dto::Chain>`), which panics at the decode boundary.
///
/// Prefer adding a first-class variant for any chain Tycho officially supports; reserve `Custom`
/// for the self-host case. Full rationale in the custom-chain decision record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
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
    Plasma,
    Robinhood,
    /// User-defined chain resolved via the [`chain_config`] registry; see the enum docs.
    Custom(CustomChainId),
}

impl DeepSizeOf for Chain {
    fn deep_size_of_children(&self, _context: &mut deepsize::Context) -> usize {
        0
    }
}

impl Chain {
    /// Parses only the built-in chains, ignoring any custom-chain registry.
    pub fn builtin_from_str(s: &str) -> Option<Self> {
        match s {
            "ethereum" => Some(Chain::Ethereum),
            "starknet" => Some(Chain::Starknet),
            "zksync" => Some(Chain::ZkSync),
            "arbitrum" => Some(Chain::Arbitrum),
            "base" => Some(Chain::Base),
            "bsc" => Some(Chain::Bsc),
            "unichain" => Some(Chain::Unichain),
            "polygon" => Some(Chain::Polygon),
            "plasma" => Some(Chain::Plasma),
            "robinhood" => Some(Chain::Robinhood),
            _ => None,
        }
    }

    /// Builds a custom-chain identity, validating that `name` has a config in the chain registry.
    /// Returns [`ChainConfigError::UnknownChain`] when it is absent so typos never silently become
    /// custom chains.
    pub fn custom(name: &str) -> Result<Self, ChainConfigError> {
        CustomChainId::checked(name, chain_registry()).map(Chain::Custom)
    }
}

impl FromStr for Chain {
    type Err = ChainConfigError;

    /// Resolves a chain name. Built-in chains parse directly; any other name is accepted only if it
    /// is registered in the global chain config registry, otherwise
    /// [`ChainConfigError::UnknownChain`] is returned so typos never silently become custom chains.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(chain) = Self::builtin_from_str(s) {
            return Ok(chain);
        }
        Self::custom(s)
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
            Chain::Plasma => f.write_str("plasma"),
            Chain::Robinhood => f.write_str("robinhood"),
            Chain::Custom(name) => f.write_str(name.as_str()),
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
            dto::Chain::Plasma => Chain::Plasma,
            dto::Chain::Robinhood => Chain::Robinhood,
            dto::Chain::Custom(name) => Chain::custom(name.as_str()).unwrap_or_else(|e| {
                panic!(
                    "received custom chain '{name}' with no registered config: {e}; install it via \
                     the chain config file (TYCHO_CHAINS_CONFIG, default ./chains.yaml) or \
                     init_chain_registry before decoding wire data"
                )
            }),
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

fn native_xpl(chain: Chain) -> Token {
    Token::new(
        &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
        "XPL",
        18,
        0,
        &[Some(2300)],
        chain,
        100,
    )
}

/// Looks up a custom chain's config in the registry, returning [`ChainConfigError::UnknownChain`]
/// when it is absent.
fn try_resolve_custom<'a>(
    id: &CustomChainId,
    registry: &'a ChainConfigRegistry,
) -> Result<&'a CustomChainConfig, ChainConfigError> {
    registry
        .get(id.as_str())
        .ok_or_else(|| ChainConfigError::UnknownChain(id.as_str().to_owned()))
}

/// Unwraps a registry lookup that cannot fail for a validly-constructed `Chain::Custom`: a
/// [`CustomChainId`] is only minted after registry validation and the registry is set-once, so a
/// missing entry is an internal invariant violation rather than bad input.
fn expect_registered<T>(result: Result<T, ChainConfigError>) -> T {
    result.unwrap_or_else(|e| {
        panic!(
            "internal invariant violation resolving custom chain config: {e}; Chain::Custom is \
             validated against the set-once chain registry at construction"
        )
    })
}

fn native_custom(chain: Chain, cfg: &CustomChainConfig) -> Token {
    let addr = Bytes::from(cfg.native.address.as_bytes().to_vec());
    Token::new(
        &addr,
        cfg.native.symbol.as_str(),
        cfg.native.decimals as u32,
        0,
        &[Some(2300)],
        chain,
        100,
    )
}

fn wrapped_native_bsc(chain: Chain, address: &str) -> Token {
    Token::new(&Bytes::from_str(address).unwrap(), "WBNB", 18, 0, &[Some(2300)], chain, 100)
}

fn wrapped_native_pol(chain: Chain, address: &str) -> Token {
    Token::new(&Bytes::from_str(address).unwrap(), "WMATIC", 18, 0, &[Some(2300)], chain, 100)
}

fn wrapped_native_xpl(chain: Chain, address: &str) -> Token {
    Token::new(&Bytes::from_str(address).unwrap(), "WXPL", 18, 0, &[Some(2300)], chain, 100)
}

fn wrapped_native_custom(chain: Chain, cfg: &CustomChainConfig) -> Token {
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
        chain,
        100,
    )
}

impl Chain {
    /// Returns the numeric chain id. Panics if a custom chain has no registered config — an
    /// unreachable invariant for a validly-constructed `Chain::Custom`; use [`Chain::try_id`] for a
    /// non-panicking variant.
    pub fn id(&self) -> u64 {
        expect_registered(self.try_id())
    }

    /// Returns the numeric chain id, or [`ChainConfigError::UnknownChain`] when a custom chain has
    /// no registered config.
    pub fn try_id(&self) -> Result<u64, ChainConfigError> {
        Ok(match self {
            Chain::Ethereum => 1,
            Chain::ZkSync => 324,
            Chain::Arbitrum => 42161,
            Chain::Starknet => 0,
            Chain::Base => 8453,
            Chain::Bsc => 56,
            Chain::Unichain => 130,
            Chain::Polygon => 137,
            Chain::Plasma => 9745,
            Chain::Robinhood => 4663,
            Chain::Custom(id) => try_resolve_custom(id, chain_registry())?.chain_id,
        })
    }

    /// Returns a default TVL threshold in native token units for the given tier.
    ///
    /// Values are approximate and target a USD-equivalent range, not a precise conversion.
    /// Native token prices used: ETH ~$2,000, POL ~$0.10, BNB ~$630.
    /// These prices are volatile, and used as a reference. They should not be updated often,
    /// unless big price movements occour, making an update necessary.
    ///
    /// Panics if a custom chain has no registered config; use [`Chain::try_default_tvl_threshold`]
    /// for a non-panicking variant.
    pub fn default_tvl_threshold(&self, tier: TvlThresholdTier) -> f64 {
        expect_registered(self.try_default_tvl_threshold(tier))
    }

    /// Like [`Chain::default_tvl_threshold`] but returns [`ChainConfigError::UnknownChain`] when a
    /// custom chain has no registered config.
    pub fn try_default_tvl_threshold(
        &self,
        tier: TvlThresholdTier,
    ) -> Result<f64, ChainConfigError> {
        Ok(match (self, tier) {
            // ETH-native chains: 10 ETH ≈ $20K, 100 ETH ≈ $200K.
            // Starknet uses ETH-denominated TVL in Tycho (STRK tracked separately).
            (
                Chain::Ethereum |
                Chain::Starknet |
                Chain::ZkSync |
                Chain::Arbitrum |
                Chain::Base |
                Chain::Unichain |
                Chain::Robinhood,
                TvlThresholdTier::Low,
            ) => 10.0,
            (
                Chain::Ethereum |
                Chain::Starknet |
                Chain::ZkSync |
                Chain::Arbitrum |
                Chain::Base |
                Chain::Unichain |
                Chain::Robinhood,
                TvlThresholdTier::Medium,
            ) => 100.0,

            // Polygon (POL ≈ $0.10): 200_000 POL ≈ $20K, 2_000_000 POL ≈ $200K
            (Chain::Polygon, TvlThresholdTier::Low) => 200_000.0,
            (Chain::Polygon, TvlThresholdTier::Medium) => 2_000_000.0,

            // Plasma (XPL ≈ $0.10): 200_000 XPL ≈ $20K, 2_000_000 XPL ≈ $200K
            (Chain::Plasma, TvlThresholdTier::Low) => 200_000.0,
            (Chain::Plasma, TvlThresholdTier::Medium) => 2_000_000.0,

            // BSC (BNB ≈ $630): 32 BNB ≈ $20K, 320 BNB ≈ $200K
            (Chain::Bsc, TvlThresholdTier::Low) => 32.0,
            (Chain::Bsc, TvlThresholdTier::Medium) => 320.0,

            (Chain::Custom(id), TvlThresholdTier::Low) => {
                try_resolve_custom(id, chain_registry())?
                    .default_tvl_thresholds
                    .low
            }
            (Chain::Custom(id), TvlThresholdTier::Medium) => {
                try_resolve_custom(id, chain_registry())?
                    .default_tvl_thresholds
                    .medium
            }
        })
    }

    /// Returns the native token for the chain. Panics if a custom chain has no registered config;
    /// use [`Chain::try_native_token`] for a non-panicking variant.
    pub fn native_token(&self) -> Token {
        expect_registered(self.try_native_token())
    }

    /// Like [`Chain::native_token`] but returns [`ChainConfigError::UnknownChain`] when a custom
    /// chain has no registered config.
    pub fn try_native_token(&self) -> Result<Token, ChainConfigError> {
        Ok(match self {
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
            Chain::Plasma => native_xpl(Chain::Plasma),
            Chain::Robinhood => native_eth(Chain::Robinhood),
            Chain::Custom(id) => native_custom(*self, try_resolve_custom(id, chain_registry())?),
        })
    }

    /// Returns the wrapped native token for the chain. Panics if a custom chain has no registered
    /// config; use [`Chain::try_wrapped_native_token`] for a non-panicking variant.
    pub fn wrapped_native_token(&self) -> Token {
        expect_registered(self.try_wrapped_native_token())
    }

    /// Like [`Chain::wrapped_native_token`] but returns [`ChainConfigError::UnknownChain`] when a
    /// custom chain has no registered config.
    pub fn try_wrapped_native_token(&self) -> Result<Token, ChainConfigError> {
        Ok(match self {
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
            Chain::Plasma => {
                wrapped_native_xpl(Chain::Plasma, "0x6100E367285b01F48D07953803A2d8dCA5D19873")
            }
            // aeWETH: the Arbitrum bridge wrapper for native ETH, exposing the WETH9 interface.
            Chain::Robinhood => {
                wrapped_native_eth(Chain::Robinhood, "0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73")
            }
            Chain::Custom(id) => {
                wrapped_native_custom(*self, try_resolve_custom(id, chain_registry())?)
            }
        })
    }

    /// Returns the expected block time in seconds for the chain. Panics if a custom chain has no
    /// registered config; use [`Chain::try_block_time_secs`] for a non-panicking variant.
    pub fn block_time_secs(&self) -> u64 {
        expect_registered(self.try_block_time_secs())
    }

    /// Like [`Chain::block_time_secs`] but returns [`ChainConfigError::UnknownChain`] when a custom
    /// chain has no registered config.
    pub fn try_block_time_secs(&self) -> Result<u64, ChainConfigError> {
        Ok(match self {
            Chain::Ethereum => 12,
            Chain::Starknet => 2,
            Chain::ZkSync => 3,
            Chain::Arbitrum => 1,
            Chain::Base => 2,
            Chain::Bsc => 1,
            Chain::Unichain => 1,
            Chain::Polygon => 2,
            Chain::Plasma => 1,
            Chain::Robinhood => 1,
            Chain::Custom(id) => try_resolve_custom(id, chain_registry())?.block_time_secs,
        })
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

// The custom-chain tests below install the process-wide chain registry, which is a set-once
// `OnceLock`. They must run in isolated processes (we use nextest), so each test gets a fresh
// registry; under a shared-process runner they would contend over the same global.
#[cfg(test)]
mod tests {
    use arrayvec::ArrayString;

    use super::{
        chain_config::{
            init_chain_registry, ChainAddress, ChainConfigError, ChainTokenConfig, TvlThresholds,
        },
        *,
    };

    fn test_config() -> CustomChainConfig {
        CustomChainConfig::try_new(
            "testchain",
            9999,
            5,
            ChainTokenConfig::try_new("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "TST", 18)
                .unwrap(),
            ChainTokenConfig::try_new("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "WTST", 18)
                .unwrap(),
            TvlThresholds::new(50.0, 500.0),
        )
        .unwrap()
    }

    fn init_test_registry() {
        init_chain_registry(ChainConfigRegistry::from_configs([test_config()]).unwrap())
            .expect("chain registry already initialised; run tests under nextest");
    }

    #[test]
    fn test_custom_chain_display() {
        init_test_registry();
        assert_eq!(
            Chain::custom("testchain")
                .unwrap()
                .to_string(),
            "testchain"
        );
    }

    #[test]
    fn test_from_str_custom_returns_err() {
        assert!("custom".parse::<Chain>().is_err());
        assert!("unknown".parse::<Chain>().is_err());
    }

    #[test]
    fn test_custom_unregistered_returns_err() {
        init_test_registry();
        assert_eq!(Chain::custom("nope"), Err(ChainConfigError::UnknownChain("nope".to_owned())));
    }

    #[test]
    fn test_from_dto_registered_custom_roundtrips() {
        init_test_registry();
        let dto_chain: dto::Chain = Chain::custom("testchain")
            .unwrap()
            .into();
        let chain: Chain = dto_chain.into();
        assert_eq!(chain.id(), 9999);
    }

    #[test]
    #[should_panic(expected = "no registered config")]
    fn test_from_dto_unregistered_custom_panics() {
        let dto_chain = dto::Chain::Custom(ArrayString::from("nope").unwrap());
        let _: Chain = dto_chain.into();
    }

    #[test]
    fn test_try_accessors_ok_for_registered_custom() {
        init_test_registry();
        let chain = Chain::custom("testchain").unwrap();
        assert_eq!(chain.try_id().unwrap(), 9999);
        assert_eq!(chain.try_block_time_secs().unwrap(), 5);
        assert_eq!(
            chain
                .try_default_tvl_threshold(TvlThresholdTier::Low)
                .unwrap(),
            50.0
        );
        assert_eq!(chain.try_native_token().unwrap().symbol, "TST");
        assert_eq!(
            chain
                .try_wrapped_native_token()
                .unwrap()
                .symbol,
            "WTST"
        );
    }

    #[test]
    fn test_try_accessors_err_for_unregistered_custom() {
        // A `CustomChainId` that bypassed registry validation (only reachable via direct
        // deserialization) surfaces as an error rather than a panic through the `try_*` accessors.
        let ghost: Chain = serde_json::from_str(r#"{"custom":"ghostchain"}"#).unwrap();
        assert_eq!(ghost.try_id(), Err(ChainConfigError::UnknownChain("ghostchain".to_owned())));
        assert!(ghost.try_native_token().is_err());
    }

    #[test]
    fn test_chain_stays_small() {
        // Regression: Custom carries only a 32-char name, so Chain stays a small Copy enum rather
        // than embedding the full config (~200 bytes) as it did when Custom held CustomChainConfig.
        assert!(
            std::mem::size_of::<Chain>() <= 40,
            "Chain is {} bytes",
            std::mem::size_of::<Chain>()
        );
    }

    #[test]
    fn test_custom_chain_id() {
        init_test_registry();
        let chain = Chain::custom("testchain").unwrap();
        assert_eq!(chain.id(), 9999);
    }

    #[test]
    fn test_custom_chain_tvl_thresholds() {
        init_test_registry();
        let chain = Chain::custom("testchain").unwrap();
        assert_eq!(chain.default_tvl_threshold(TvlThresholdTier::Low), 50.0);
        assert_eq!(chain.default_tvl_threshold(TvlThresholdTier::Medium), 500.0);
    }

    #[test]
    fn test_custom_chain_native_token() {
        init_test_registry();
        let chain = Chain::custom("testchain").unwrap();
        let token = chain.native_token();
        assert_eq!(token.symbol, "TST");
        assert_eq!(token.decimals, 18);
        assert_eq!(token.chain, chain);
        assert_eq!(token.address, Bytes::from(vec![0xAA; 20]));
    }

    #[test]
    fn test_custom_chain_wrapped_native_token() {
        init_test_registry();
        let chain = Chain::custom("testchain").unwrap();
        let token = chain.wrapped_native_token();
        assert_eq!(token.symbol, "WTST");
        assert_eq!(token.chain, chain);
        assert_eq!(token.address, Bytes::from(vec![0xBB; 20]));
    }

    #[test]
    fn test_chain_address_new_rejects_oversized_input() {
        assert_eq!(ChainAddress::new(&[0u8; 33]), Err(ChainConfigError::AddressTooLong(33)));
    }

    #[test]
    fn test_robinhood_chain_id() {
        assert_eq!(Chain::Robinhood.id(), 4663);
    }

    #[test]
    fn test_robinhood_chain_display() {
        assert_eq!(Chain::Robinhood.to_string(), "robinhood");
    }

    #[test]
    fn test_robinhood_chain_from_str() {
        assert_eq!("robinhood".parse::<Chain>().unwrap(), Chain::Robinhood);
    }

    #[test]
    fn test_robinhood_native_token() {
        let token = Chain::Robinhood.native_token();
        assert_eq!(token.symbol, "ETH");
        assert_eq!(token.chain, Chain::Robinhood);
        assert_eq!(
            token.address,
            Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap()
        );
    }

    #[test]
    fn test_robinhood_wrapped_native_token() {
        let token = Chain::Robinhood.wrapped_native_token();
        assert_eq!(token.symbol, "WETH");
        assert_eq!(token.chain, Chain::Robinhood);
        assert_eq!(
            token.address,
            Bytes::from_str("0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73").unwrap()
        );
    }

    #[test]
    fn test_robinhood_default_tvl_threshold() {
        assert_eq!(Chain::Robinhood.default_tvl_threshold(TvlThresholdTier::Low), 10.0);
        assert_eq!(Chain::Robinhood.default_tvl_threshold(TvlThresholdTier::Medium), 100.0);
    }

    #[test]
    fn test_robinhood_block_time_secs() {
        assert_eq!(Chain::Robinhood.block_time_secs(), 1);
    }

    #[test]
    fn test_chain_address_as_bytes_returns_active_slice() {
        let addr = ChainAddress::new(&[0xAA; 20]).unwrap();
        assert_eq!(addr.as_bytes(), &[0xAA; 20]);
        assert_eq!(addr.as_bytes().len(), 20);
    }
}
