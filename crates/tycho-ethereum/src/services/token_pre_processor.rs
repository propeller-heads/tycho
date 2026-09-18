use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy::{primitives::Address, rpc::types::BlockNumberOrTag, sol_types::SolCall};
use async_trait::async_trait;
use futures::{stream, StreamExt};
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
    BytesCodec, RPCError,
};

// At most 12 concurrent RPC calls per get_tokens invocation: three per token.
const MAX_CONCURRENT_TOKENS: usize = 4;

#[derive(Debug, Clone)]
pub struct EthereumTokenPreProcessor {
    rpc: EthereumRpcClient,
    chain: Chain,
    settlement_contract: Address,
    enrichment_budget: Option<Duration>,
}

impl EthereumTokenPreProcessor {
    pub fn new(rpc: &EthereumRpcClient, chain: Chain, settlement_contract: Address) -> Self {
        EthereumTokenPreProcessor {
            rpc: rpc.clone(),
            chain,
            settlement_contract,
            enrichment_budget: None,
        }
    }

    /// Enables deferred enrichment. Callers must persist pending tokens and run recovery before
    /// enabling this; older clients do not understand their readiness status.
    pub fn with_enrichment_budget(mut self, budget: Duration) -> Self {
        self.enrichment_budget = Some(budget);
        self
    }

    /// Retries metadata and analysis for a token that was left pending.
    ///
    /// Returns `Err` when the RPC did not answer (transport failure, provider error, timeout)
    /// so the caller can keep the token pending. Everything the RPC did answer is treated as it
    /// is on the hot path: a reverting or undecodable `symbol`/`decimals` call receives the
    /// legacy fallbacks, and a token without a funded owner receives a Bad verdict. The caller
    /// owns the timeout for this attempt.
    pub async fn recover_token(
        &self,
        address: Bytes,
        finder: Arc<dyn TokenOwnerFinding>,
        block: BlockTag,
    ) -> Result<Token, String> {
        let detector = EthCallDetector::new(&self.rpc, finder, self.settlement_contract);
        self.fetch_token(address, &detector, block, true)
            .await
    }

    #[cfg(test)]
    async fn call_symbol(&self, token: Address) -> String {
        let calldata = symbolCall {}.abi_encode();
        let result = self
            .rpc
            .eth_call(call_request(None, token, calldata), BlockNumberOrTag::Latest)
            .await;
        Self::decode_symbol(token, result)
    }

    fn decode_symbol(token: Address, result: Result<Bytes, RPCError>) -> String {
        let result = match result {
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

    #[cfg(test)]
    async fn call_decimals(&self, token: Address) -> u8 {
        let calldata = decimalsCall {}.abi_encode();
        let result = self
            .rpc
            .eth_call(call_request(None, token, calldata), BlockNumberOrTag::Latest)
            .await;
        Self::decode_decimals(token, result)
    }

    fn decode_decimals(token: Address, result: Result<Bytes, RPCError>) -> u8 {
        let result = match result {
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

    async fn call_metadata(&self, token: Address, strict: bool) -> Result<(String, u8), String> {
        let requests = [
            call_request(None, token, symbolCall {}.abi_encode()),
            call_request(None, token, decimalsCall {}.abi_encode()),
        ];
        match self
            .rpc
            .eth_call_pair(requests, BlockNumberOrTag::Latest)
            .await
        {
            Ok([symbol, decimals]) => {
                // Strict mode defers only what a later retry could change. A revert means the
                // contract has no usable `symbol`/`decimals`; retrying it forever would keep the
                // token pending and its pools unavailable, where legacy stored the fallbacks and
                // let analysis decide the quality. Undecodable return data is handled the same
                // way inside the decode helpers.
                if strict {
                    for result in [&symbol, &decimals] {
                        if let Err(e) = result {
                            if !e.is_execution_reverted() {
                                return Err(e.to_string());
                            }
                        }
                    }
                }
                Ok((Self::decode_symbol(token, symbol), Self::decode_decimals(token, decimals)))
            }
            Err(e) => {
                if strict {
                    return Err(e.to_string());
                }
                warn!(?e, ?token, "Failed to fetch token metadata batch, using existing fallbacks");
                Ok((format!("0x{:x}", token), 18))
            }
        }
    }

    async fn get_token(
        &self,
        address: Bytes,
        detector: &EthCallDetector,
        block: BlockTag,
    ) -> Token {
        match self
            .fetch_token(address.clone(), detector, block, self.enrichment_budget.is_some())
            .await
        {
            Ok(token) => token,
            Err(error) => {
                warn!(?address, %error, "Token enrichment deferred");
                Token::pending(&address, self.chain)
            }
        }
    }

    async fn fetch_token(
        &self,
        address: Bytes,
        detector: &EthCallDetector,
        block: BlockTag,
        strict: bool,
    ) -> Result<Token, String> {
        let token_address = Address::from_bytes(&address);
        let (metadata, analysis) = tokio::join!(
            self.call_metadata(token_address, strict),
            detector.analyze(address.clone(), block),
        );

        let (symbol, decimals) = metadata?;
        if strict {
            if let Err(error) = &analysis {
                return Err(error.clone());
            }
        }
        let (token_quality, gas, tax) = analysis.unwrap_or_else(|e| {
            warn!(address=?address, error=?e, "TokenDetectionFailure");
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

        Ok(Token {
            metadata_status: Default::default(),
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
        })
    }
}

#[async_trait]
impl TokenPreProcessor for EthereumTokenPreProcessor {
    // Named explicitly: this span is an on-chain metadata fetch and would otherwise
    // be indistinguishable in traces from the storage-layer `get_tokens` spans.
    #[instrument(
        name = "fetch_onchain_token_metadata",
        skip_all,
        fields(n_addresses = tracing::field::Empty, block = ?block)
    )]
    async fn get_tokens(
        &self,
        addresses: Vec<Bytes>,
        token_finder: Arc<dyn TokenOwnerFinding>,
        block: BlockTag,
    ) -> Vec<Token> {
        let mut seen = HashSet::new();
        let addresses: Vec<_> = addresses
            .into_iter()
            .filter(|a| seen.insert(a.clone()))
            .collect();
        // Recorded after deduplication so the span agrees with the
        // `token_enrichment_unknown_tokens` metric, which counts unique addresses.
        tracing::Span::current().record("n_addresses", addresses.len());
        let detector = EthCallDetector::new(&self.rpc, token_finder, self.settlement_contract);
        let mut tokens: Vec<_> = addresses
            .iter()
            .map(|a| Token::pending(a, self.chain))
            .collect();
        {
            let mut work = stream::iter(addresses.into_iter().enumerate())
                .map(|(index, address)| {
                    let token = self.get_token(address, &detector, block);
                    async move { (index, token.await) }
                })
                .buffer_unordered(MAX_CONCURRENT_TOKENS);
            let collect = async {
                while let Some((index, token)) = work.next().await {
                    tokens[index] = token;
                }
            };
            // One deadline includes queued tokens and RPC retries. Per-token timeouts would
            // multiply the budget across waves. Dropping the stream cancels unfinished requests.
            match self.enrichment_budget {
                Some(budget) => {
                    if tokio::time::timeout(budget, collect)
                        .await
                        .is_err()
                    {
                        warn!(budget_ms = budget.as_millis(), "Token enrichment deadline exceeded");
                    }
                }
                None => collect.await,
            }
        }
        tokens
    }
}

#[cfg(test)]
mod live_benchmark;

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        str::FromStr,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use alloy::{
        primitives::{address, U256},
        sol_types::SolValue,
    };
    use mockito::{Matcher, Server};
    use rstest::rstest;
    use serde_json::{json, Value};
    use tokio::sync::{mpsc, oneshot};
    use tycho_common::models::token::TokenOwnerStore;

    use super::*;
    use crate::{
        rpc::config::RPCRetryConfig,
        test_fixtures::{TestFixture, TEST_BLOCK_NUMBER, TOKEN_HOLDERS, USDC_STR, WETH_STR},
    };

    const COWSWAP_SETTLEMENT: Address = address!("c9f2e6ea1637E499406986ac50ddC92401ce1f58");

    fn mock_processor(server: &mockito::ServerGuard) -> EthereumTokenPreProcessor {
        let rpc = EthereumRpcClient::new(&server.url())
            .unwrap()
            .with_batching(crate::rpc::config::RPCBatchingConfig::Disabled);
        EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, COWSWAP_SETTLEMENT)
    }

    #[tokio::test]
    async fn test_get_tokens_batches_metadata_and_keeps_analysis_at_requested_block() {
        let mut server = Server::new_async().await;
        let response = server.mock("POST", "/")
            .with_header("content-type", "application/json")
            .with_body_from_request(|request| {
                let request: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                let response = if let Some(calls) = request.as_array() {
                    assert_eq!(calls.len(), 2);
                    Value::Array(calls.iter().rev().map(|call| {
                        assert_eq!(call["method"], "eth_call");
                        assert_eq!(call["params"][1], "latest");
                        let input = call["params"][0]["input"].as_str().unwrap();
                        let result = if input == Bytes::from(symbolCall {}.abi_encode()).to_string() {
                            "TEST".abi_encode()
                        } else {
                            assert_eq!(input, Bytes::from(decimalsCall {}.abi_encode()).to_string());
                            decimalsCall::abi_encode_returns(&6)
                        };
                        json!({"jsonrpc": "2.0", "id": call["id"], "result": Bytes::from(result)})
                    }).collect())
                } else {
                    assert_eq!(request["method"], "eth_call");
                    assert_eq!(request["params"][1], "0x64");
                    assert!(request["params"][2].is_object());
                    let result: Vec<u8> = [1, 1, 1, 0, 100_000, 0, 0, 100_000, 30_000, 25_000]
                        .into_iter().flat_map(|value| U256::from(value).to_be_bytes::<32>()).collect();
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": Bytes::from(result)})
                };
                response.to_string().into_bytes()
            }).expect(2).create_async().await;
        let address = Address::repeat_byte(1).to_bytes();
        let owners = TokenOwnerStore::new(
            [(address.clone(), (Address::repeat_byte(2).to_bytes(), 200_000u64.into()))].into(),
        );
        let rpc = EthereumRpcClient::new(&server.url()).unwrap();
        let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, COWSWAP_SETTLEMENT);
        let tokens = processor
            .get_tokens(vec![address.clone()], Arc::new(owners), BlockTag::Number(100))
            .await;
        let expected =
            vec![Token::new(&address, "TEST", 6, 0, &[Some(27_500)], Chain::Ethereum, 100)];
        assert_eq!(serde_json::to_value(tokens).unwrap(), serde_json::to_value(expected).unwrap());
        response.assert_async().await;
    }

    #[derive(Debug)]
    struct GatedTokenOwnerFinder {
        started: mpsc::UnboundedSender<(Bytes, oneshot::Sender<()>)>,
    }

    #[async_trait]
    impl TokenOwnerFinding for GatedTokenOwnerFinder {
        async fn find_owner(
            &self,
            token: Bytes,
            _min_balance: Bytes,
        ) -> Result<Option<(Bytes, Bytes)>, String> {
            let (release, wait) = oneshot::channel();
            self.started
                .send((token, release))
                .unwrap();
            wait.await.unwrap();
            Ok(None)
        }
    }

    #[tokio::test]
    async fn test_get_tokens_fetches_metadata_and_analysis_concurrently() {
        let mut server = Server::new_async().await;
        let address = Address::repeat_byte(1).to_bytes();
        let (started, mut incoming) = mpsc::unbounded_channel();
        let mut mocks = Vec::new();
        for (call, result) in [
            (symbolCall {}.abi_encode(), "TEST".abi_encode()),
            (decimalsCall {}.abi_encode(), decimalsCall::abi_encode_returns(&6)),
        ] {
            let id = Arc::new(AtomicU64::new(0));
            let request_id = id.clone();
            let started = started.clone();
            let mock = server
                .mock("POST", "/")
                .match_body(Matcher::PartialJson(json!({
                    "method": "eth_call",
                    "params": [{"to": address, "input": Bytes::from(call.clone())}, "latest"]
                })))
                .with_header_from_request("content-type", move |request| {
                    let request: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                    request_id.store(request["id"].as_u64().unwrap(), Ordering::SeqCst);
                    "application/json".to_string()
                })
                .with_chunked_body(move |writer| {
                    let (release, wait) = oneshot::channel();
                    started
                        .send((Bytes::from(call.clone()), release))
                        .unwrap();
                    // The test releases responses only after all three operations have started.
                    wait.blocking_recv()
                        .map_err(std::io::Error::other)?;
                    let response = json!({
                        "jsonrpc": "2.0", "id": id.load(Ordering::SeqCst),
                        "result": Bytes::from(result.clone())
                    });
                    writer.write_all(response.to_string().as_bytes())
                })
                .expect(1)
                .create_async()
                .await;
            mocks.push(mock);
        }

        let processor = mock_processor(&server);
        let mut fetch = Box::pin(processor.get_tokens(
            vec![address.clone()],
            Arc::new(GatedTokenOwnerFinder { started }),
            BlockTag::Latest,
        ));
        let mut releases = Vec::new();
        let mut calls = Vec::new();
        let started = tokio::time::timeout(Duration::from_secs(5), async {
            for _ in 0..3 {
                tokio::select! {
                    result = &mut fetch => panic!("fetch finished before release: {result:?}"),
                    next = incoming.recv() => {
                        let (call, release) = next.unwrap();
                        calls.push(call);
                        releases.push(release);
                    }
                }
            }
        })
        .await;
        for release in releases {
            release.send(()).unwrap();
        }
        // Drop queued release handles as well if the test timed out, unblocking mock threads.
        drop(incoming);
        started.expect("symbol, decimals and analysis should overlap");
        assert!(calls.contains(&Bytes::from(symbolCall {}.abi_encode())));
        assert!(calls.contains(&Bytes::from(decimalsCall {}.abi_encode())));
        assert!(calls.contains(&address));
        let tokens = tokio::time::timeout(Duration::from_secs(5), fetch)
            .await
            .unwrap();
        assert_eq!(tokens[0].symbol, "TEST");
        assert_eq!(tokens[0].decimals, 6);
        for mock in mocks {
            mock.assert_async().await;
        }
    }

    #[tokio::test]
    async fn test_get_tokens_refills_capacity_and_preserves_order() {
        let mut server = Server::new_async().await;
        // Input order deliberately differs from address order.
        let addresses: Vec<_> = (1..=MAX_CONCURRENT_TOKENS * 2 + 1)
            .rev()
            .map(|i| Address::repeat_byte(i as u8).to_bytes())
            .collect();
        let metadata = server
            .mock("POST", "/")
            .with_body_from_request(|request| {
                let request: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x"})
                    .to_string()
                    .into_bytes()
            })
            .expect(addresses.len() * 2)
            .create_async()
            .await;
        let processor = mock_processor(&server);
        let (started, mut incoming) = mpsc::unbounded_channel();
        let finder = Arc::new(GatedTokenOwnerFinder { started });
        let mut fetch = Box::pin(processor.get_tokens(addresses.clone(), finder, BlockTag::Latest));

        let mut releases = HashMap::new();
        for _ in 0..MAX_CONCURRENT_TOKENS {
            let (address, release) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    result = &mut fetch => panic!("fetch finished before release: {result:?}"),
                    next = incoming.recv() => next.unwrap(),
                }
            })
            .await
            .expect("tokens should start concurrently");
            assert!(releases
                .insert(address, release)
                .is_none());
        }
        for address in &addresses[..MAX_CONCURRENT_TOKENS] {
            assert!(releases.contains_key(address));
        }

        // Hold the first token pending while repeatedly freeing another slot. A replacement
        // must start after each completion, without waiting for the first token to finish.
        let first_release = releases.remove(&addresses[0]).unwrap();
        let mut next_release = releases.remove(&addresses[1]).unwrap();
        for expected in &addresses[MAX_CONCURRENT_TOKENS..] {
            assert!(futures::poll!(&mut fetch).is_pending());
            assert!(incoming.try_recv().is_err(), "token concurrency limit exceeded");

            next_release.send(()).unwrap();
            let (address, release) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    result = &mut fetch => panic!("fetch finished before release: {result:?}"),
                    next = incoming.recv() => next.unwrap(),
                }
            })
            .await
            .expect("a free slot should be refilled while the first token is pending");
            assert_eq!(&address, expected);
            next_release = release;
        }

        next_release.send(()).unwrap();
        for release in releases.into_values() {
            release.send(()).unwrap();
        }
        assert!(futures::poll!(&mut fetch).is_pending());
        first_release.send(()).unwrap();

        let tokens = tokio::time::timeout(Duration::from_secs(5), fetch)
            .await
            .expect("all tokens should finish after release");
        assert_eq!(
            tokens
                .iter()
                .map(|token| token.address.clone())
                .collect::<Vec<_>>(),
            addresses
        );
        assert!(tokens
            .iter()
            .all(|token| token.quality == 10));
        metadata.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_tokens_preserves_metadata_and_analysis_results() {
        let mut server = Server::new_async().await;
        let addresses: Vec<_> = (1..=3)
            .map(|i| Address::repeat_byte(i).to_bytes())
            .collect();
        let holders: Vec<_> = (11..=13)
            .map(|i| Address::repeat_byte(i).to_bytes())
            .collect();
        let owners = TokenOwnerStore::new(
            addresses
                .iter()
                .cloned()
                .zip(
                    holders
                        .into_iter()
                        .map(|holder| (holder, 200_000u64.into())),
                )
                .collect(),
        );
        let rpc = server
            .mock("POST", "/")
            .with_body_from_request(|request| {
                let request: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                assert_eq!(request["method"], "eth_call");
                let tx = &request["params"][0];
                let to = Address::from_str(tx["to"].as_str().unwrap()).unwrap();
                let input = tx["input"].as_str().unwrap();
                let response = if input == Bytes::from(symbolCall {}.abi_encode()).to_string() {
                    assert_eq!(request["params"][1], "latest");
                    json!({"result": Bytes::from(format!("{}\0", "👩‍💻".repeat(256)).abi_encode())})
                } else if input == Bytes::from(decimalsCall {}.abi_encode()).to_string() {
                    assert_eq!(request["params"][1], "latest");
                    json!({"result": Bytes::from(decimalsCall::abi_encode_returns(&(to[0] + 5)))})
                } else {
                    assert!(input.starts_with("0x521c6539"));
                    assert_eq!(request["params"][1], "0x64");
                    assert!(request["params"][2].is_object(), "analysis needs state overrides");
                    if to[0] == 12 {
                        json!({"error": {"code": 3, "message": "execution reverted"}})
                    } else {
                        let received = if to[0] == 13 { 99_000 } else { 100_000 };
                        // Analyzer ABI: three success flags, five balances, then two gas costs.
                        let result: Vec<u8> =
                            [1, 1, 1, 0, received, 0, 0, received, 30_000, 25_000]
                                .into_iter()
                                .flat_map(|value| U256::from(value).to_be_bytes::<32>())
                                .collect();
                        json!({"result": Bytes::from(result)})
                    }
                };
                let mut response = response;
                response["jsonrpc"] = json!("2.0");
                response["id"] = request["id"].clone();
                response.to_string().into_bytes()
            })
            .expect(9)
            .create_async()
            .await;
        let tokens = mock_processor(&server)
            .get_tokens(addresses.clone(), Arc::new(owners), BlockTag::Number(100))
            .await;
        let symbol = "👩‍💻".repeat(255);
        let expected = vec![
            Token::new(&addresses[0], &symbol, 6, 0, &[Some(27_500)], Chain::Ethereum, 100),
            Token::new(&addresses[1], &symbol, 7, 0, &[], Chain::Ethereum, 10),
            Token::new(&addresses[2], &symbol, 8, 100, &[Some(27_500)], Chain::Ethereum, 10),
        ];
        // Token equality compares addresses only; compare every returned field here.
        assert_eq!(serde_json::to_value(tokens).unwrap(), serde_json::to_value(expected).unwrap());
        rpc.assert_async().await;
    }

    #[rstest]
    #[case::rpc_error(json!({"error": {"code": -32602, "message": "invalid params"}}))]
    #[case::invalid_abi(json!({"result": "0x"}))]
    #[tokio::test]
    async fn test_get_tokens_preserves_fallbacks(#[case] response: Value) {
        let mut server = Server::new_async().await;
        let rpc = server
            .mock("POST", "/")
            .with_body_from_request(move |request| {
                let request: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                let mut response = response.clone();
                response["jsonrpc"] = json!("2.0");
                response["id"] = request["id"].clone();
                response.to_string().into_bytes()
            })
            .expect(3)
            .create_async()
            .await;
        let address = Address::repeat_byte(1).to_bytes();
        let owners = TokenOwnerStore::new(
            [(address.clone(), (Address::repeat_byte(2).to_bytes(), 200_000u64.into()))].into(),
        );
        let tokens = mock_processor(&server)
            .get_tokens(vec![address.clone()], Arc::new(owners), BlockTag::Latest)
            .await;
        let expected =
            vec![Token::new(&address, &address.to_string(), 18, 0, &[], Chain::Ethereum, 10)];
        assert_eq!(serde_json::to_value(tokens).unwrap(), serde_json::to_value(expected).unwrap());
        rpc.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_tokens_keeps_legacy_fallbacks_when_the_batch_transport_fails() {
        // Without an enrichment budget the hot path never defers: a batch that fails at the
        // transport level still yields a ready token with the legacy fallbacks, and analysis
        // (no funded owner here) decides the quality.
        let mut server = Server::new_async().await;
        let unavailable = server
            .mock("POST", "/")
            .with_status(503)
            .expect(1)
            .create_async()
            .await;
        let rpc = EthereumRpcClient::new(&server.url())
            .unwrap()
            .with_retry(RPCRetryConfig::new(0, 1, 1));
        let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, COWSWAP_SETTLEMENT);
        let address = Address::repeat_byte(1).to_bytes();
        let tokens = processor
            .get_tokens(
                vec![address.clone()],
                Arc::new(TokenOwnerStore::new(Default::default())),
                BlockTag::Latest,
            )
            .await;
        let expected =
            vec![Token::new(&address, &address.to_string(), 18, 0, &[], Chain::Ethereum, 10)];
        assert_eq!(serde_json::to_value(tokens).unwrap(), serde_json::to_value(expected).unwrap());
        unavailable.assert_async().await;
    }

    #[tokio::test]
    async fn test_get_tokens_empty() {
        let mut server = Server::new_async().await;
        let rpc = server
            .mock("POST", "/")
            .expect(0)
            .create_async()
            .await;
        let (started, mut incoming) = mpsc::unbounded_channel();
        let tokens = mock_processor(&server)
            .get_tokens(Vec::new(), Arc::new(GatedTokenOwnerFinder { started }), BlockTag::Latest)
            .await;
        assert!(tokens.is_empty());
        assert!(incoming.try_recv().is_err());
        rpc.assert_async().await;
    }

    impl TestFixture {
        fn create_token_preprocessor(&self) -> EthereumTokenPreProcessor {
            // These fixtures expect individual calls; batching is covered by dedicated tests.
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
