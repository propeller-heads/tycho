use std::{sync::Arc, time::Duration};

use alloy::{primitives::Address, rpc::types::BlockNumberOrTag, sol_types::SolCall};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use tracing::{instrument, warn};
use tycho_common::{
    models::{
        blockchain::BlockTag,
        token::{AnalyzedToken, Token, TokenVerdict},
        Chain,
    },
    traits::{TokenAnalyzer, TokenOwnerFinding, TokenPreProcessor},
    Bytes,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    erc20::{decimalsCall, symbolCall},
    rpc::EthereumRpcClient,
    services::token_analyzer::{call_request, AnalyzerKind},
    BytesCodec,
};

#[derive(Debug, Clone)]
pub struct EthereumTokenPreProcessor {
    rpc: EthereumRpcClient,
    chain: Chain,
    analyzer: AnalyzerKind,
}

impl EthereumTokenPreProcessor {
    pub fn new(rpc: &EthereumRpcClient, chain: Chain, analyzer: AnalyzerKind) -> Self {
        EthereumTokenPreProcessor { rpc: rpc.clone(), chain, analyzer }
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

/// New tokens analysed at the same time.
const ANALYSIS_CONCURRENCY: usize = 8;
/// Longest a new token's analysis may hold up the block it appears in. A token that takes
/// longer gets no analysis, and the analysis cron retries it.
const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(5);

impl EthereumTokenPreProcessor {
    async fn get_token(
        &self,
        address: Bytes,
        analyzer: &dyn TokenAnalyzer<Error = String>,
        block: BlockTag,
    ) -> AnalyzedToken {
        let token_address = Address::from_bytes(&address);
        let (symbol, decimals, analysis) = tokio::join!(
            self.call_symbol(token_address),
            self.call_decimals(token_address),
            tokio::time::timeout(ANALYSIS_TIMEOUT, analyzer.analyze(address.clone(), block)),
        );
        let analysis = analysis
            .unwrap_or_else(|_| Err(format!("analysis took longer than {ANALYSIS_TIMEOUT:?}")))
            .inspect_err(|error| warn!(?address, error, "TokenDetectionFailure"))
            .ok();

        let mut token = Token {
            address: address.clone(),
            symbol: symbol
                .replace('\0', "")
                .graphemes(true)
                .take(255)
                .collect::<String>(),
            decimals: decimals.into(),
            tax: 0,
            gas: Vec::new(),
            chain: self.chain,
            // Flag the token as bad; the analysis cron retries tokens of quality 6-10.
            quality: TokenVerdict::Blocked.quality(),
        };
        if let Some(analysis) = &analysis {
            if !matches!(analysis.verdict, TokenVerdict::Ok | TokenVerdict::Fee) {
                warn!(?address, verdict = ?analysis.verdict, reason = ?analysis.reason, "BadToken");
            }
            token.apply_analysis(analysis);
        }
        AnalyzedToken { token, analysis }
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
    ) -> Vec<AnalyzedToken> {
        let analyzer = self
            .analyzer
            .build(&self.rpc, token_finder);
        stream::iter(addresses)
            .map(|address| self.get_token(address, analyzer.as_ref(), block))
            .buffered(ANALYSIS_CONCURRENCY)
            .collect()
            .await
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

            let analyzer = AnalyzerKind::EthCall { settlement_contract: COWSWAP_SETTLEMENT };
            EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, analyzer)
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
            .map(|t| (t.token.symbol.clone(), t.token.decimals, t.token.quality))
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
