use std::{
    future::pending,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloy::sol_types::SolValue;
use mockito::Server;
use rstest::rstest;
use serde_json::{json, Value};
use tycho_common::models::token::{TokenMetadataStatus, TokenOwnerStore};

use super::*;
use crate::rpc::config::RPCRetryConfig;

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn rpc_error_mock(code: i64, message: &'static str) -> impl Fn(&mockito::Request) -> Vec<u8> {
    move |request| {
        let calls: Vec<Value> = serde_json::from_slice(request.body().unwrap()).unwrap();
        serde_json::to_vec(
            &calls
                .iter()
                .map(|call| rpc_error(&call["id"], code, message))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }
}

/// Answers every item of a metadata batch by the ERC-20 function it calls, inside the single
/// array response the batch expects.
fn per_field_mock(
    symbol: impl Fn(&Value) -> Value + Send + Sync + 'static,
    decimals: impl Fn(&Value) -> Value + Send + Sync + 'static,
) -> impl Fn(&mockito::Request) -> Vec<u8> {
    let symbol_input = Bytes::from(symbolCall {}.abi_encode()).to_string();
    move |request| {
        let calls: Vec<Value> = serde_json::from_slice(request.body().unwrap()).unwrap();
        serde_json::to_vec(
            &calls
                .iter()
                .map(|call| {
                    if call["params"][0]["input"] == symbol_input {
                        symbol(call)
                    } else {
                        decimals(call)
                    }
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }
}

#[derive(Debug)]
struct HangingOwnerFinder {
    active: Arc<AtomicUsize>,
}

struct ActiveAttempt(Arc<AtomicUsize>);

impl Drop for ActiveAttempt {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl TokenOwnerFinding for HangingOwnerFinder {
    async fn find_owner(&self, token: Bytes, _: Bytes) -> Result<Option<(Bytes, Bytes)>, String> {
        // One token completes; every other token holds its concurrency slot until cancelled.
        if token == Address::repeat_byte(1).to_bytes() {
            return Ok(None);
        }
        self.active
            .fetch_add(1, Ordering::SeqCst);
        let _attempt = ActiveAttempt(self.active.clone());
        pending().await
    }
}

#[derive(Debug, Default)]
struct CountingOwnerFinder {
    calls: AtomicUsize,
}

#[async_trait]
impl TokenOwnerFinding for CountingOwnerFinder {
    async fn find_owner(&self, _: Bytes, _: Bytes) -> Result<Option<(Bytes, Bytes)>, String> {
        self.calls
            .fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

fn metadata_response(request: &Value) -> Value {
    let result =
        if request["params"][0]["input"] == Bytes::from(symbolCall {}.abi_encode()).to_string() {
            "TEST".abi_encode()
        } else {
            decimalsCall::abi_encode_returns(&6)
        };
    json!({"jsonrpc": "2.0", "id": request["id"], "result": Bytes::from(result)})
}

#[tokio::test]
async fn collection_deadline_preserves_completed_tokens_and_cancels_queued_work() {
    let mut server = Server::new_async().await;
    let _metadata = server
        .mock("POST", "/")
        .with_body_from_request(|request| {
            let calls: Vec<Value> = serde_json::from_slice(request.body().unwrap()).unwrap();
            serde_json::to_vec(
                &calls
                    .iter()
                    .map(metadata_response)
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        })
        .create_async()
        .await;
    let rpc = EthereumRpcClient::new(&server.url()).unwrap();
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, Address::ZERO)
        .with_enrichment_budget(Duration::from_secs(1));
    let active = Arc::new(AtomicUsize::new(0));
    let addresses: Vec<_> = (1..=12)
        .map(|id| Address::repeat_byte(id).to_bytes())
        .collect();
    let mut duplicated = addresses.clone();
    duplicated.push(addresses[0].clone());
    // A per-token timeout would need three waves of 1 s each, exceeding this 2.5 s bound.
    let tokens = tokio::time::timeout(
        Duration::from_millis(2_500),
        processor.get_tokens(
            duplicated,
            Arc::new(HangingOwnerFinder { active: active.clone() }),
            BlockTag::Latest,
        ),
    )
    .await
    .expect("the entire collection must share a deadline");
    assert_eq!(
        tokens
            .iter()
            .map(|t| t.address.clone())
            .collect::<Vec<_>>(),
        addresses
    );
    assert_eq!(tokens[0].metadata_status, TokenMetadataStatus::Ready);
    assert_eq!(tokens[0].decimals, 6);
    assert!(tokens[1..]
        .iter()
        .all(|t| t.metadata_status == TokenMetadataStatus::Pending));
    assert_eq!(active.load(Ordering::SeqCst), 0, "unfinished analyses must be dropped");
}

#[tokio::test]
async fn stalled_metadata_leaves_every_token_pending_even_when_analysis_finishes() {
    // Accepting a connection without sending headers stalls every batched metadata call, while
    // analysis finishes at once. More tokens than concurrency slots: the ones still queued when
    // the deadline hits must come back pending too, in input order.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let rpc =
        EthereumRpcClient::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, Address::ZERO)
        .with_enrichment_budget(Duration::from_millis(50));
    let addresses: Vec<_> = (1..=(MAX_CONCURRENT_TOKENS as u8 + 2))
        .map(|id| Address::repeat_byte(id).to_bytes())
        .collect();
    let finder = Arc::new(CountingOwnerFinder::default());
    let tokens = tokio::time::timeout(
        Duration::from_millis(500),
        processor.get_tokens(addresses.clone(), finder.clone(), BlockTag::Latest),
    )
    .await
    .unwrap();
    assert_eq!(
        tokens
            .iter()
            .map(|t| t.address.clone())
            .collect::<Vec<_>>(),
        addresses
    );
    assert!(tokens
        .iter()
        .all(|t| t.metadata_status == TokenMetadataStatus::Pending && t.quality == 0));
    assert_eq!(
        finder.calls.load(Ordering::SeqCst),
        MAX_CONCURRENT_TOKENS,
        "only tokens that got a concurrency slot run analysis"
    );
}

#[derive(Debug, Clone, Copy)]
enum NoAnswer {
    ProviderError,
    TransportFailure,
}

#[rstest]
#[case::provider_error(NoAnswer::ProviderError)]
#[case::transport_failure(NoAnswer::TransportFailure)]
#[tokio::test]
async fn strict_recovery_stays_pending_when_the_rpc_does_not_answer(#[case] failure: NoAnswer) {
    // A provider error that is neither transient nor a revert is an answer that carries no
    // data; a transport failure is no answer at all. Neither tells anything about the token, so
    // no fallback may be stored and the token must stay pending.
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/").expect(1);
    let mock = match failure {
        NoAnswer::ProviderError => {
            mock.with_body_from_request(rpc_error_mock(-32601, "method not found"))
        }
        NoAnswer::TransportFailure => mock.with_status(503),
    }
    .create_async()
    .await;
    let rpc = EthereumRpcClient::new(&server.url()).unwrap();
    let rpc = match failure {
        NoAnswer::ProviderError => rpc,
        NoAnswer::TransportFailure => rpc.with_retry(RPCRetryConfig::new(0, 1, 1)),
    };
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, Address::ZERO);
    let result = processor
        .recover_token(
            Address::repeat_byte(1).to_bytes(),
            Arc::new(TokenOwnerStore::new(Default::default())),
            BlockTag::Latest,
        )
        .await;
    assert!(result.is_err());
    mock.assert_async().await;
}

#[tokio::test]
async fn strict_recovery_stays_pending_when_analysis_rpc_fails() {
    // Metadata answers, but the analysis simulation gets a provider error: the verdict is
    // unknown, so strict mode must not store a Bad quality it cannot back up.
    let mut server = Server::new_async().await;
    let _rpc = server
        .mock("POST", "/")
        .with_body_from_request(|request| {
            let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
            let response = match body.as_array() {
                Some(calls) => Value::Array(
                    calls
                        .iter()
                        .map(metadata_response)
                        .collect(),
                ),
                None => rpc_error(&body["id"], -32601, "method not found"),
            };
            response.to_string().into_bytes()
        })
        .create_async()
        .await;
    let rpc = EthereumRpcClient::new(&server.url()).unwrap();
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, Address::ZERO);
    let address = Address::repeat_byte(1).to_bytes();
    let owners = TokenOwnerStore::new(
        [(address.clone(), (Address::repeat_byte(9).to_bytes(), Bytes::from("0x0186a0")))].into(),
    );
    let error = processor
        .recover_token(address, Arc::new(owners), BlockTag::Latest)
        .await
        .expect_err("an unanswered analysis must keep the token pending");
    assert!(error.contains("eth_call with state overrides failed"), "{error}");
}

#[tokio::test]
async fn strict_recovery_uses_legacy_fallbacks_when_metadata_reverts() {
    // The contract answered and reverted: retrying later cannot change that, so the token gets
    // the same address symbol and 18 decimals the hot path always stored, and analysis
    // (no funded owner here) still decides the quality.
    let mut server = Server::new_async().await;
    let _revert = server
        .mock("POST", "/")
        .with_body_from_request(rpc_error_mock(3, "execution reverted"))
        .create_async()
        .await;
    let rpc = EthereumRpcClient::new(&server.url()).unwrap();
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, Address::ZERO);
    let address = Address::repeat_byte(1);
    let token = processor
        .recover_token(
            address.to_bytes(),
            Arc::new(TokenOwnerStore::new(Default::default())),
            BlockTag::Latest,
        )
        .await
        .expect("a revert is a definitive answer");
    assert_eq!(token.metadata_status, TokenMetadataStatus::Ready);
    assert_eq!(token.symbol, format!("0x{address:x}"));
    assert_eq!(token.decimals, 18);
    assert_eq!(token.quality, 10);
}

#[tokio::test]
async fn strict_recovery_falls_back_only_for_the_reverting_field() {
    // One batch item reverts while the other answers: only the reverting field takes its
    // legacy fallback; the answered field keeps the on-chain value.
    let mut server = Server::new_async().await;
    let batch = server
        .mock("POST", "/")
        .with_body_from_request(per_field_mock(
            |call| rpc_error(&call["id"], 3, "execution reverted"),
            |call| {
                json!({
                    "jsonrpc": "2.0", "id": call["id"],
                    "result": Bytes::from(decimalsCall::abi_encode_returns(&6))
                })
            },
        ))
        .expect(1)
        .create_async()
        .await;
    let rpc = EthereumRpcClient::new(&server.url()).unwrap();
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, Address::ZERO);
    let address = Address::repeat_byte(1);
    let token = processor
        .recover_token(
            address.to_bytes(),
            Arc::new(TokenOwnerStore::new(Default::default())),
            BlockTag::Latest,
        )
        .await
        .expect("a reverting symbol must not defer a token whose decimals answered");
    assert_eq!(token.metadata_status, TokenMetadataStatus::Ready);
    assert_eq!(token.symbol, format!("0x{address:x}"));
    assert_eq!(token.decimals, 6);
    batch.assert_async().await;
}

#[tokio::test]
async fn strict_recovery_stays_pending_when_one_field_has_no_answer() {
    // One batch item answers while the other gets a provider error: a later retry could still
    // fetch the missing field, so strict mode must keep the token pending.
    let mut server = Server::new_async().await;
    let _batch = server
        .mock("POST", "/")
        .with_body_from_request(per_field_mock(
            |call| {
                json!({
                    "jsonrpc": "2.0", "id": call["id"],
                    "result": Bytes::from("TEST".abi_encode())
                })
            },
            |call| rpc_error(&call["id"], -32601, "method not found"),
        ))
        .create_async()
        .await;
    let rpc = EthereumRpcClient::new(&server.url()).unwrap();
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, Address::ZERO);
    let result = processor
        .recover_token(
            Address::repeat_byte(1).to_bytes(),
            Arc::new(TokenOwnerStore::new(Default::default())),
            BlockTag::Latest,
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn strict_recovery_uses_legacy_fallbacks_when_metadata_is_undecodable() {
    // The contract answered with data that is not a valid `symbol`/`decimals` return: like a
    // revert, retrying cannot change that, so the hot-path fallbacks apply and analysis (no
    // funded owner here) decides the quality.
    let mut server = Server::new_async().await;
    let _undecodable = server
        .mock("POST", "/")
        .with_body_from_request(per_field_mock(
            |call| json!({"jsonrpc": "2.0", "id": call["id"], "result": "0x"}),
            |call| json!({"jsonrpc": "2.0", "id": call["id"], "result": "0x"}),
        ))
        .create_async()
        .await;
    let rpc = EthereumRpcClient::new(&server.url()).unwrap();
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, Address::ZERO);
    let address = Address::repeat_byte(1);
    let token = processor
        .recover_token(
            address.to_bytes(),
            Arc::new(TokenOwnerStore::new(Default::default())),
            BlockTag::Latest,
        )
        .await
        .expect("undecodable return data is a definitive answer");
    assert_eq!(token.metadata_status, TokenMetadataStatus::Ready);
    assert_eq!(token.symbol, format!("0x{address:x}"));
    assert_eq!(token.decimals, 18);
    assert_eq!(token.quality, 10);
}

#[tokio::test]
async fn deadline_covers_retry_backoff() {
    // A retryable transport error with a one-second backoff would exceed the budget several
    // times over. The deadline must cut through the backoff sleep, not wait for it.
    let mut server = Server::new_async().await;
    let unavailable = server
        .mock("POST", "/")
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    let rpc = EthereumRpcClient::new(&server.url())
        .unwrap()
        .with_retry(RPCRetryConfig::new(3, 1_000, 1_000));
    let processor = EthereumTokenPreProcessor::new(&rpc, Chain::Ethereum, Address::ZERO)
        .with_enrichment_budget(Duration::from_millis(200));
    let tokens = tokio::time::timeout(
        Duration::from_millis(700),
        processor.get_tokens(
            vec![Address::repeat_byte(1).to_bytes()],
            Arc::new(TokenOwnerStore::new(Default::default())),
            BlockTag::Latest,
        ),
    )
    .await
    .expect("budget must bound retry backoff");
    assert_eq!(tokens[0].metadata_status, TokenMetadataStatus::Pending);
    unavailable.assert_async().await;
}
