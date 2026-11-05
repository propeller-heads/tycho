use std::sync::Arc;

use alloy::{primitives::Address, rpc::types::BlockNumberOrTag, sol_types::SolCall};
use async_trait::async_trait;
use tracing::{instrument, warn};
use tycho_common::{
    models::{
        blockchain::BlockTag,
        token::{Token, TokenQuality},
        Chain,
    },
    traits::{TokenAnalyzer, TokenOwnerFinding, TokenPreProcessor},
    Bytes,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    erc20::{decimalsCall, symbolCall},
    rpc::EthereumRpcClient,
    services::token_analyzer::{call_request, EthCallDetector},
    BytesCodec,
};

#[derive(Debug, Clone)]
pub struct EthereumTokenPreProcessor {
    rpc: EthereumRpcClient,
    chain: Chain,
    settlement_contract: Address,
}

impl EthereumTokenPreProcessor {
    pub fn new(rpc: &EthereumRpcClient, chain: Chain, settlement_contract: Address) -> Self {
        EthereumTokenPreProcessor { rpc: rpc.clone(), chain, settlement_contract }
    }

    async fn call_symbol(&self, token: Address) -> String {
        let calldata = symbolCall {}.abi_encode();

        let result = match self
            .rpc
            .eth_call(call_request(None, token, calldata), BlockNumberOrTag::Latest)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                warn!(?e, ?token, "Failed to call symbol function, using address as fallback");
                return format!("0x{:x}", token);
            }
        };

        match symbolCall::abi_decode_returns_validate(&result) {
            Ok(symbol) => symbol,
            Err(e) => {
                warn!(
                    ?e,
                    ?token,
                    "Failed to decode symbol function result, using address as fallback"
                );
                format!("0x{:x}", token)
            }
        }
    }

    async fn call_decimals(&self, token: Address) -> u8 {
        let calldata = decimalsCall {}.abi_encode();

        let result = match self
            .rpc
            .eth_call(call_request(None, token, calldata), BlockNumberOrTag::Latest)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                warn!(?e, ?token, "Failed to call decimals function, using default decimals 18");
                return 18;
            }
        };

        match decimalsCall::abi_decode_returns_validate(&result) {
            Ok(decimals) => decimals,
            Err(e) => {
                warn!(
                    ?e,
                    ?token,
                    "Failed to decode decimals function result, using default decimals 18"
                );
                18
            }
        }
    }
}

#[async_trait]
impl TokenPreProcessor for EthereumTokenPreProcessor {
    // Named explicitly: this span is an on-chain metadata fetch and would otherwise
    // be indistinguishable in traces from the storage-layer `get_tokens` spans.
    #[instrument(
        name = "fetch_onchain_token_metadata",
        skip_all,
        fields(n_addresses = addresses.len(), block = ?block)
    )]
    async fn get_tokens(
        &self,
        addresses: Vec<Bytes>,
        token_finder: Arc<dyn TokenOwnerFinding>,
        block: BlockTag,
    ) -> Vec<Token> {
        let mut tokens_info = Vec::new();

        for address in addresses {
            let token_address = Address::from_bytes(&address);

            // Make RPC calls directly for symbol and decimals
            let symbol = self.call_symbol(token_address).await;
            let decimals = self.call_decimals(token_address).await;

            let detector =
                EthCallDetector::new(&self.rpc, token_finder.clone(), self.settlement_contract);

            let (token_quality, gas, tax) = detector
                .analyze(address.clone(), block)
                .await
                .unwrap_or_else(|e| {
                    warn!(error=?e, "TokenDetectionFailure");
                    (TokenQuality::bad("Detection failed"), None, None)
                });

            let mut quality = 100;

            if let TokenQuality::Bad { reason } = token_quality {
                warn!(address=?address, ?reason, "BadToken");
                // Flag this token as bad using quality, an external script is responsible for
                // analyzing these tokens again.
                quality = 10;
            };

            // If quality is 100 but it's a fee token, set quality to 50
            if quality == 100 && tax.is_some_and(|tax_value| tax_value > 0) {
                quality = 50;
            }

            tokens_info.push(Token {
                address,
                symbol: symbol
                    .replace('\0', "")
                    .graphemes(true)
                    .take(255)
                    .collect::<String>(),
                decimals: decimals.into(),
                tax: tax.unwrap_or(0),
                gas: gas
                    .map(|g| vec![Some(g)])
                    .unwrap_or_else(Vec::new),
                chain: self.chain,
                quality,
            });
        }

        tokens_info
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::address;
    use tycho_common::models::token::TokenOwnerStore;

    use super::*;
    use crate::test_fixtures::{TestFixture, TEST_BLOCK_NUMBER, TOKEN_HOLDERS, USDC_STR, WETH_STR};

    const COWSWAP_SETTLEMENT: Address = address!("c9f2e6ea1637E499406986ac50ddC92401ce1f58");

    impl TestFixture {
        fn create_token_preprocessor(&self) -> EthereumTokenPreProcessor {
            // We do not enable batching as the token pre-processor does not leverage it currently
            let rpc = self.create_rpc_client(false);

            EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, COWSWAP_SETTLEMENT)
        }
    }

    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_call_symbol() {
        let fixture = TestFixture::new();
        let processor = fixture.create_token_preprocessor();

        // Test WETH symbol
        let weth_address = Address::from_str(WETH_STR).expect("Failed to parse WETH address");
        let symbol = processor
            .call_symbol(weth_address)
            .await;
        assert_eq!(symbol, "WETH", "Expected WETH symbol");

        // Test USDC symbol
        let usdc_address = Address::from_str(USDC_STR).expect("Failed to parse USDC address");
        let symbol = processor
            .call_symbol(usdc_address)
            .await;
        assert_eq!(symbol, "USDC", "Expected USDC symbol");
    }

    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_call_decimals() {
        let fixture = TestFixture::new();
        let processor = fixture.create_token_preprocessor();

        // Test WETH decimals (18)
        let weth_address = Address::from_str(WETH_STR).expect("Failed to parse WETH address");
        let decimals = processor
            .call_decimals(weth_address)
            .await;
        assert_eq!(decimals, 18, "Expected WETH to have 18 decimals");

        // Test USDC decimals (6)
        let usdc_address = Address::from_str(USDC_STR).expect("Failed to parse USDC address");
        let decimals = processor
            .call_decimals(usdc_address)
            .await;
        assert_eq!(decimals, 6, "Expected USDC to have 6 decimals");
    }

    #[tokio::test]
    #[ignore = "require archive RPC connection"]
    async fn test_get_tokens() {
        let fixture = TestFixture::new();
        let processor = fixture.create_token_preprocessor();

        let tf = TokenOwnerStore::new(TOKEN_HOLDERS.clone());

        let fake_address: &str = "0xA0b86991c7456b36c1d19D4a2e9Eb0cE3606eB48";
        let addresses = vec![
            Bytes::from_str(WETH_STR).unwrap(),
            Bytes::from_str(USDC_STR).unwrap(),
            Bytes::from_str(fake_address).unwrap(),
        ];

        let results = processor
            .get_tokens(addresses, Arc::new(tf), BlockTag::Number(TEST_BLOCK_NUMBER))
            .await;
        assert_eq!(results.len(), 3);
        let relevant_attrs: Vec<(String, u32, u32)> = results
            .iter()
            .map(|t| (t.symbol.clone(), t.decimals, t.quality))
            .collect();
        assert_eq!(
            relevant_attrs,
            vec![
                ("WETH".to_string(), 18, 100),
                ("USDC".to_string(), 6, 100),
                (fake_address.to_lowercase(), 18, 10)
            ]
        );
    }
}
