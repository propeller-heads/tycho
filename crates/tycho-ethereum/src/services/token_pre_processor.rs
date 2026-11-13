use std::{sync::Arc, time::Duration};

use alloy::{primitives::Address, rpc::types::BlockNumberOrTag, sol_types::SolCall};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use metrics::histogram;
use tracing::{debug, instrument, warn};
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
    services::token_analyzer::{call_request, SimulationDetector},
    BytesCodec,
};

#[derive(Debug, Clone)]
pub struct EthereumTokenPreProcessor {
    rpc: EthereumRpcClient,
    chain: Chain,
}

impl EthereumTokenPreProcessor {
    pub fn new(rpc: &EthereumRpcClient, chain: Chain) -> Self {
        EthereumTokenPreProcessor { rpc: rpc.clone(), chain }
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
/// Longest wait for the node to reach the block of the new tokens. The indexer can receive a
/// block before the node, and the analysis at that block then fails.
const NODE_WAIT: Duration = Duration::from_secs(2);
const NODE_POLL_INTERVAL: Duration = Duration::from_millis(100);

impl EthereumTokenPreProcessor {
    /// Waits until the node has `number`, for at most `NODE_WAIT`. After that the calls go out
    /// anyway: a failed analysis is retried by the analysis cron. Records the wait in the
    /// `token_node_wait_seconds` histogram.
    async fn wait_for_block(&self, number: u64) {
        let start = tokio::time::Instant::now();
        let reached = self
            .poll_until_block(number, start + NODE_WAIT)
            .await;
        let waited = start.elapsed();
        let outcome = if reached { "reached" } else { "timeout" };
        histogram!(
            "token_node_wait_seconds",
            "chain" => self.chain.to_string(),
            "outcome" => outcome,
        )
        .record(waited.as_secs_f64());
        if reached {
            debug!(number, waited_ms = waited.as_millis(), "Node has the block of the new tokens");
        } else {
            warn!(number, waited_ms = waited.as_millis(), "Node did not reach the block in time");
        }
    }

    /// Whether the node reported a head of at least `number` before `deadline`.
    async fn poll_until_block(&self, number: u64, deadline: tokio::time::Instant) -> bool {
        loop {
            match self.rpc.get_block_number().await {
                Ok(head) if head >= number => return true,
                Ok(head) => debug!(head, number, "Node is behind the block of the new tokens"),
                Err(error) => warn!(?error, "Failed to read the node's block number"),
            }
            if tokio::time::Instant::now() + NODE_POLL_INTERVAL > deadline {
                return false;
            }
            tokio::time::sleep(NODE_POLL_INTERVAL).await;
        }
    }

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
            if !matches!(
                analysis.verdict,
                TokenVerdict::Ok | TokenVerdict::Fee | TokenVerdict::VariableFee
            ) {
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
        if let BlockTag::Number(number) = block {
            self.wait_for_block(number).await;
        }
        let analyzer = SimulationDetector::new(&self.rpc, token_finder, self.chain);
        stream::iter(addresses)
            .map(|address| self.get_token(address, &analyzer, block))
            .buffered(ANALYSIS_CONCURRENCY)
            .collect()
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::models::token::TokenOwnerStore;

    use super::*;
    use crate::test_fixtures::{TestFixture, TEST_BLOCK_NUMBER, TOKEN_HOLDERS, USDC_STR, WETH_STR};

    impl TestFixture {
        fn create_token_preprocessor(&self) -> EthereumTokenPreProcessor {
            // We do not enable batching as the token pre-processor does not leverage it currently
            let rpc = self.create_rpc_client(false);

            EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum)
        }
    }

    /// A node whose `eth_blockNumber` answers `head` to every request.
    async fn node_at(server: &mut mockito::ServerGuard, head: u64) -> mockito::Mock {
        server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().expect("body")).expect("json");
                serde_json::json!({ "jsonrpc": "2.0", "id": body["id"], "result": format!("{head:#x}") })
                    .to_string()
                    .into()
            })
            .create_async()
            .await
    }

    fn preprocessor_for(server: &mockito::ServerGuard) -> EthereumTokenPreProcessor {
        let rpc = EthereumRpcClient::new(&server.url()).expect("client");
        EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum)
    }

    #[tokio::test]
    async fn a_node_at_the_block_is_not_waited_for() {
        let mut server = mockito::Server::new_async().await;
        let mock = node_at(&mut server, 100).await;

        let start = std::time::Instant::now();
        preprocessor_for(&server)
            .wait_for_block(100)
            .await;

        assert!(start.elapsed() < NODE_POLL_INTERVAL);
        mock.expect(1).assert_async().await;
    }

    #[tokio::test]
    async fn a_node_behind_the_block_is_waited_for_until_the_limit() {
        let mut server = mockito::Server::new_async().await;
        let _mock = node_at(&mut server, 99).await;

        let start = std::time::Instant::now();
        preprocessor_for(&server)
            .wait_for_block(100)
            .await;

        let waited = start.elapsed();
        assert!(waited >= NODE_WAIT - NODE_POLL_INTERVAL, "waited {waited:?}");
        assert!(waited < NODE_WAIT + Duration::from_secs(1), "waited {waited:?}");
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
