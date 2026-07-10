//! Common test fixtures and utilities for tycho-ethereum tests
//!
//! This module contains shared test constants, helper functions, and fixtures
//! that can be used across multiple test files in the crate.

use std::{collections::HashMap, str::FromStr, sync::LazyLock};

use alloy::{
    primitives::B256,
    rpc::client::{ClientBuilder, ReqwestClient},
};
use tycho_common::{
    models::{blockchain::Block, Address, Chain},
    Bytes,
};

use crate::BytesCodec;

// Common test block constants
pub const TEST_BLOCK_NUMBER: u64 = 23475728;
pub const TEST_BLOCK_HASH: &str =
    "0x7f70ac678819e24c4947a3a95fdab886083892a18ba1a962ebaac31455584042";

// Common Ethereum mainnet contract addresses for testing
pub const BALANCER_VAULT_STR: &str = "0xba12222222228d8ba445958a75a0704d566bf2c8";
pub const STETH_STR: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";
pub const DAI_STR: &str = "0x6b175474e89094c44da98b954eedeac495271d0f";
pub const WBTC_STR: &str = "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599";
pub const USDC_STR: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
pub const USDT_STR: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
pub const WETH_STR: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

// Arbitrum mainnet token addresses
pub const ARB_USDC_STR: &str = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831";
pub const ARB_WETH_STR: &str = "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1";
pub const ARB_ARB_STR: &str = "0x912CE59144191C1204E64559FE8253a0e49E6548";

// Arbitrum token holders (Uniswap V3 pools with large balances)
pub const ARB_USDC_HOLDER_ADDR: &str = "0xC6962004f452bE9203591991D15f6b388e09E8D0"; // USDC/WETH V3
pub const ARB_WETH_HOLDER_ADDR: &str = "0xC6962004f452bE9203591991D15f6b388e09E8D0"; // USDC/WETH V3
pub const ARB_ARB_HOLDER_ADDR: &str = "0x92c63d0e701CAAe670C9415d91C474F686298f00"; // ARB/ETH V3

// BSC mainnet token addresses
// Sell-only fee token: charges a fee when transferring to non-whitelisted addresses but not
// when transferring to/from the LP pool itself.
pub const BSC_SELL_FEE_TOKEN_STR: &str = "0x701add4311e85c1f9c1549319fe2c476bc8a1b8b";
// Liquidity pair that holds the token.
pub const BSC_SELL_FEE_TOKEN_HOLDER_STR: &str = "0x1831Bb2723CED46e1b6c08d2f3ae50b2Ab9427B9";

// Common token addresses array
pub const TOKEN_ADDRESSES: [&str; 5] = [BALANCER_VAULT_STR, STETH_STR, DAI_STR, WBTC_STR, USDC_STR];

// Contract-specific test constants for expected slot counts at TEST_BLOCK_NUMBER
pub const BALANCER_VAULT_EXPECTED_SLOTS: usize = 47690;
pub const STETH_EXPECTED_SLOTS: usize = 789526;

// Known token holders for testing (addresses with large balances)
// Using USV4 pool manager as a holder with known large balances
pub const USDC_HOLDER_ADDR: &str = "0x000000000004444c5dc75cB358380D2e3dE08A90";
pub const USDC_HOLDER_BALANCE: u64 = 74743132960379_u64; // Balance at the test block

// WETH holder - Uniswap V2 WETH-USDC pair has significant WETH balance
pub const WETH_HOLDER_ADDR: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";

/// Lazily-initialized map of Ethereum mainnet token addresses to their known holders and balances.
/// This is useful for `TokenOwnerStore` setup in tests.
pub static TOKEN_HOLDERS: LazyLock<HashMap<Address, (Bytes, Bytes)>> = LazyLock::new(|| {
    HashMap::from([
        (
            Address::from_str(USDC_STR).unwrap(),
            (
                Bytes::from_str(USDC_HOLDER_ADDR).unwrap(),
                Bytes::from_str("0x43f6e8f16703").unwrap(),
            ),
        ),
        (
            Address::from_str(WETH_STR).unwrap(),
            (
                Bytes::from_str(WETH_HOLDER_ADDR).unwrap(),
                Bytes::from_str("0x2386f26fc10000").unwrap(), // ~0.01 WETH
            ),
        ),
    ])
});

/// Lazily-initialized map of BSC mainnet token addresses to their known holders and balances.
pub static BSC_TOKEN_HOLDERS: LazyLock<HashMap<Address, (Bytes, Bytes)>> = LazyLock::new(|| {
    HashMap::from([(
        Address::from_str(BSC_SELL_FEE_TOKEN_STR).unwrap(),
        (
            Bytes::from_str(BSC_SELL_FEE_TOKEN_HOLDER_STR).unwrap(),
            // 200_000 raw units → amount = 100_000 = MIN_AMOUNT, safe for any decimal count.
            Bytes::from_str("0x030d40").unwrap(),
        ),
    )])
});

/// Lazily-initialized map of Arbitrum mainnet token addresses to their known holders and balances.
pub static ARB_TOKEN_HOLDERS: LazyLock<HashMap<Address, (Bytes, Bytes)>> = LazyLock::new(|| {
    HashMap::from([
        (
            Address::from_str(ARB_USDC_STR).unwrap(),
            (
                Bytes::from_str(ARB_USDC_HOLDER_ADDR).unwrap(),
                Bytes::from_str("0x09184e72a000").unwrap(), // 1M USDC (6 decimals)
            ),
        ),
        (
            Address::from_str(ARB_WETH_STR).unwrap(),
            (
                Bytes::from_str(ARB_WETH_HOLDER_ADDR).unwrap(),
                Bytes::from_str("0x3635c9adc5dea00000").unwrap(), // ~1000 WETH (18 decimals)
            ),
        ),
        (
            Address::from_str(ARB_ARB_STR).unwrap(),
            (
                Bytes::from_str(ARB_ARB_HOLDER_ADDR).unwrap(),
                Bytes::from_str("0x3635c9adc5dea00000").unwrap(), // 1000 ARB (18 decimals)
            ),
        ),
    ])
});

pub static TEST_SLOTS: LazyLock<HashMap<B256, B256>> = LazyLock::new(|| {
    HashMap::from([
        (
            B256::from_str("0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap(),
            B256::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap(),
        ),
        (
            B256::from_str("0000000000000000000000000000000000000000000000000000000000000003")
                .unwrap(),
            B256::from_str("00000000000000000000006048a8c631fb7e77eca533cf9c29784e482391e700")
                .unwrap(),
        ),
        (
            B256::from_str("00015ea75c6f99b2e8663793de8ab1ce7c52e3295bf307bbf9990d4af56f7035")
                .unwrap(),
            B256::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap(),
        ),
    ])
});

static RPC_CLIENT: LazyLock<ReqwestClient> = LazyLock::new(|| {
    let url = std::env::var("RPC_URL")
        .expect("RPC_URL must be set for testing")
        .parse()
        .expect("Invalid RPC_URL");
    ClientBuilder::default().http(url)
});

/// Test fixture for creating blocks and RPC clients
pub struct TestFixture {
    pub block: Block,
    pub inner_rpc: ReqwestClient,
    pub url: String,
}

impl TestFixture {
    /// Creates a new test fixture with the default test block and rpc url parsed from env
    pub fn new() -> Self {
        let chain = Chain::Ethereum;

        // Clone the static RPC client to support reuse across tests
        let inner_rpc = RPC_CLIENT.clone();

        let block_hash = B256::from_str(TEST_BLOCK_HASH).expect("expected valid block hash");
        let block = Block::new(
            TEST_BLOCK_NUMBER,
            chain,
            block_hash.to_bytes(),
            Default::default(),
            Default::default(),
        );

        let url = std::env::var("RPC_URL").expect("RPC_URL must be set for testing");
        Self { block, inner_rpc, url }
    }

    /// Creates a test fixture for Arbitrum using `ARB_RPC_URL`.
    pub fn new_arbitrum() -> Self {
        let url = std::env::var("ARB_RPC_URL").expect("ARB_RPC_URL must be set for Arbitrum tests");
        let inner_rpc = ClientBuilder::default().http(
            url.parse()
                .expect("Invalid ARB_RPC_URL"),
        );
        let block = Block::new(
            0,
            Chain::Arbitrum,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        Self { block, inner_rpc, url }
    }

    /// Creates a test fixture for BSC using `BSC_RPC_URL`.
    pub fn new_bsc() -> Self {
        let url = std::env::var("BSC_RPC_URL").expect("BSC_RPC_URL must be set for BSC tests");
        let inner_rpc = ClientBuilder::default().http(
            url.parse()
                .expect("Invalid BSC_RPC_URL"),
        );
        let block =
            Block::new(0, Chain::Bsc, Default::default(), Default::default(), Default::default());
        Self { block, inner_rpc, url }
    }
}

impl Default for TestFixture {
    fn default() -> Self {
        Self::new()
    }
}
