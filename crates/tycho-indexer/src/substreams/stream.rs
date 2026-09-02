use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Error};
use async_stream::try_stream;
use futures03::{Stream, StreamExt};
use metrics::{counter, gauge};
use once_cell::sync::Lazy;
use prost::Message as ProstMessage;
use tokio::time::sleep;
use tokio_retry::strategy::ExponentialBackoff;
use tracing::{error, info, trace, warn};

use crate::{
    pb::sf::substreams::{
        rpc::{
            v2::{response::Message, BlockScopedData, BlockUndoSignal, Response},
            v3::Request,
        },
        v1::Package,
    },
    substreams::SubstreamsEndpoint,
};

#[allow(clippy::large_enum_variant)]
pub enum BlockResponse {
    New(BlockScopedData),
    Undo(BlockUndoSignal),
    Ended,
}

pub struct SubstreamsStream {
    stream: Pin<Box<dyn Stream<Item = Result<BlockResponse, Error>> + Send>>,
}

impl SubstreamsStream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: Arc<SubstreamsEndpoint>,
        cursor: Option<String>,
        package: Option<Package>,
        output_module_name: String,
        start_block: i64,
        end_block: u64,
        final_blocks_only: bool,
        extractor_id: String,
        partial_blocks: bool,
    ) -> Self {
        SubstreamsStream {
            stream: Box::pin(stream_blocks(
                endpoint,
                cursor,
                package,
                output_module_name,
                start_block,
                end_block,
                final_blocks_only,
                extractor_id,
                partial_blocks,
            )),
        }
    }
}

static DEFAULT_BACKOFF: Lazy<ExponentialBackoff> =
    Lazy::new(|| ExponentialBackoff::from_millis(500).max_delay(Duration::from_secs(45)));

/// Consecutive `Unauthenticated` retries allowed once the endpoint has proven the credential by
/// delivering a block. With `DEFAULT_BACKOFF` these span about three minutes.
const MAX_UNAUTHENTICATED_RETRIES: u32 = 5;

/// Whether an `Unauthenticated` status from the endpoint should be retried.
///
/// Returns false while `block_received` is false: a credential that has never delivered a block
/// cannot be distinguished from a misconfigured one, and the token is read once at startup, so
/// retrying it would hide the problem rather than fix it. Once a block has arrived the credential
/// is proven, so a later rejection is the endpoint's and is retried up to
/// `MAX_UNAUTHENTICATED_RETRIES` times.
fn should_retry_unauthenticated(block_received: bool, retries_used: u32) -> bool {
    block_received && retries_used < MAX_UNAUTHENTICATED_RETRIES
}

async fn wait_for_next_retry(
    backoff: &mut ExponentialBackoff,
    retry_count: &mut u32,
    extractor_id: &str,
) -> Result<(), Error> {
    if let Some(duration) = backoff.next() {
        info!("Will try to reconnect after {:?}", duration);
        sleep(duration).await;
        *retry_count += 1;
        Ok(())
    } else {
        counter!("substreams_failure", "extractor" => extractor_id.to_string(), "cause" => "max_retries_exceeded").increment(1);
        Err(anyhow!("Backoff requested to stop retrying, quitting"))
    }
}

// Create the Stream implementation that streams blocks with auto-reconnection.
//
// On the first connection, `cursor` is empty (fresh start) and `start_block_num`
// determines where Substreams begins (inclusive). After the first block arrives,
// `latest_cursor` is populated from the response. On any subsequent reconnection
// (hot reconnect within the same process), `latest_cursor` is sent as
// `start_cursor` which takes precedence over `start_block_num`.
#[allow(clippy::too_many_arguments)]
fn stream_blocks(
    endpoint: Arc<SubstreamsEndpoint>,
    cursor: Option<String>,
    package: Option<Package>,
    output_module_name: String,
    start_block_num: i64,
    stop_block_num: u64,
    final_blocks_only: bool,
    extractor_id: String,
    partial_blocks: bool,
) -> impl Stream<Item = Result<BlockResponse, Error>> {
    let mut latest_cursor = cursor.unwrap_or_default();
    let mut latest_block = start_block_num as u64;
    let mut retry_count = 0;
    let mut backoff = DEFAULT_BACKOFF.clone();
    let mut block_received = false;
    let mut unauthenticated_retries = 0;

    try_stream! {
        'retry_loop: loop {
            if retry_count > 0 {
                warn!("Blockstreams disconnected, connecting again");
            }

            let result = endpoint.clone().substreams(Request {
                start_block_num,
                start_cursor: latest_cursor.clone(),
                stop_block_num,
                final_blocks_only,
                package: package.clone(),
                params: Default::default(),
                network: String::new(), // TODO: check if we need to set the network?
                output_module: output_module_name.clone(),
                // There is usually no good reason for you to consume the stream development mode (so switching `true`
                // to `false`). If you do switch it, be aware that more than one output module will be send back to you,
                // and the current code in `process_block_scoped_data` (within your 'main.rs' file) expects a single
                // module.
                production_mode: true,
                debug_initial_store_snapshot_for_modules: vec![],
                dev_output_modules: vec![],
                limit_processed_blocks: u64::MAX,
                progress_messages_interval_ms: 30 * 1000,
                partial_blocks,
                noop_mode: false,
            }).await;

            match result {
                Ok(stream) => {
                    for await response in stream {
                        match process_substreams_response(response).await {
                            BlockProcessedResult::BlockScopedData(block_scoped_data) => {
                                if let Some(block) = block_scoped_data.clock.clone() {
                                    // Only measure lag if the msg is a full block or the last partial
                                    // TODO: substreams is looking to update the partial block service to be faster than the final block confirmation.
                                    // This means .is_last_partial will be unset for the last partial. We'd need to update this logic when that happens
                                    // to monitor for the first partial of the next block as the indicator that the previous block is complete.
                                    if !block_scoped_data.is_partial || block_scoped_data.is_last_partial.is_some_and(|last_partial| last_partial) {
                                        if let Some(block_ts) = block.timestamp {
                                            let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards!?").as_millis();
                                            let lag = now.saturating_sub((block_ts.seconds * 1000) as u128);
                                            gauge!("substreams_lag_millis", "extractor" => extractor_id.clone()).set(lag as f64);
                                        }
                                    }
                                    latest_block = block.number;
                                };

                                gauge!("block_message_size_bytes", "extractor" => extractor_id.clone()).set(block_scoped_data.encoded_len() as f64);

                                // Reset backoff because we got a good value from the stream
                                backoff = DEFAULT_BACKOFF.clone();

                                // The endpoint accepted the credential, so any later rejection of
                                // it is the endpoint's problem, not a misconfiguration.
                                block_received = true;
                                unauthenticated_retries = 0;

                                let cursor = block_scoped_data.cursor.clone();
                                yield BlockResponse::New(block_scoped_data);

                                latest_cursor = cursor;
                            },
                            BlockProcessedResult::BlockUndoSignal(block_undo_signal) => {
                                // Reset backoff because we got a good value from the stream
                                backoff = DEFAULT_BACKOFF.clone();

                                let to_block = block_undo_signal.last_valid_block.clone().unwrap_or_default().number;
                                counter!(
                                    "chain_reorg",
                                    "extractor" => extractor_id.clone(),
                                    "to_block" => to_block.to_string(),
                                    "from_block" => latest_block.to_string()
                                )
                                .increment(1);

                                let cursor = block_undo_signal.last_valid_cursor.clone();
                                yield BlockResponse::Undo(block_undo_signal);

                                latest_cursor = cursor;
                            },
                            BlockProcessedResult::Skip() => {},
                            BlockProcessedResult::TonicError(status) => {
                                if status.code() == tonic::Code::Unauthenticated {
                                    counter!("substreams_failure", "extractor" => extractor_id.clone(), "cause" => "unauthenticated").increment(1);

                                    // Forward the error to the stream consumer, which treats it as fatal
                                    if !should_retry_unauthenticated(block_received, unauthenticated_retries) {
                                        error!("Endpoint rejected the credential, giving up: {:#}", status);
                                        return Err(anyhow::Error::new(status.clone()))?;
                                    }

                                    unauthenticated_retries += 1;
                                    warn!(unauthenticated_retries, "Endpoint rejected a proven credential, reconnecting");
                                    wait_for_next_retry(&mut backoff, &mut retry_count, &extractor_id).await?;
                                    continue 'retry_loop;
                                }

                                error!("Received tonic error {:#}", status);
                                counter!("substreams_failure", "extractor" => extractor_id.clone(), "cause" => status.code().to_string()).increment(1);

                                // If we reach this point, we must wait a bit before retrying
                                wait_for_next_retry(&mut backoff, &mut retry_count, &extractor_id).await?;
                                continue 'retry_loop;
                            },
                        }
                    }

                    info!("Stream completed, reached end block");
                    yield BlockResponse::Ended;
                    return;
                },
                Err(e) => {
                    // An endpoint that rejects the credential before opening the stream surfaces it
                    // here rather than as a stream error.
                    let unauthenticated = e
                        .downcast_ref::<tonic::Status>()
                        .is_some_and(|status| status.code() == tonic::Code::Unauthenticated);

                    if unauthenticated {
                        counter!("substreams_failure", "extractor" => extractor_id.clone(), "cause" => "unauthenticated").increment(1);

                        if !should_retry_unauthenticated(block_received, unauthenticated_retries) {
                            error!("Endpoint rejected the credential, giving up: {:#}", e);
                            return Err(e)?;
                        }

                        unauthenticated_retries += 1;
                        warn!(unauthenticated_retries, "Endpoint rejected a proven credential, reconnecting");
                    } else {
                        counter!("substreams_failure", "module" => output_module_name.clone(), "cause" => "connection_error").increment(1);
                    }

                    error!("Unable to connect to endpoint: {:#}", e);

                    // If we reach this point, we must wait a bit before retrying
                    wait_for_next_retry(&mut backoff, &mut retry_count, &extractor_id).await?;
                }
            }
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum BlockProcessedResult {
    Skip(),
    BlockScopedData(BlockScopedData),
    BlockUndoSignal(BlockUndoSignal),
    TonicError(tonic::Status),
}

async fn process_substreams_response(
    result: Result<Response, tonic::Status>,
) -> BlockProcessedResult {
    let response = match result {
        Ok(v) => v,
        Err(e) => return BlockProcessedResult::TonicError(e),
    };

    match response.message {
        Some(Message::Session(session)) => {
            tracing::Span::current().record("sf_trace_id", &session.trace_id);
            info!(
                ?session.resolved_start_block,
                ?session.linear_handoff_block,
                ?session.max_parallel_workers,
                ?session.trace_id,
                "SubstreamSessionInit"
            );
            BlockProcessedResult::Skip()
        }
        Some(Message::BlockScopedData(block_scoped_data)) => {
            BlockProcessedResult::BlockScopedData(block_scoped_data)
        }
        Some(Message::BlockUndoSignal(block_undo_signal)) => {
            BlockProcessedResult::BlockUndoSignal(block_undo_signal)
        }
        Some(Message::Progress(progress)) => {
            // The `ModulesProgress` messages goal is to report active parallel processing happening
            // either to fill up backward (relative to your request's start block) some missing
            // state or pre-process forward blocks (again relative).
            //
            // You could log that in trace or accumulate to push as metrics. Here a snippet of code
            // that prints progress to standard out. If your `BlockScopedData` messages seems to
            // never arrive in production mode, it's because progresses is happening but
            // not yet for the output module you requested.
            //
            // let progresses: Vec<_> = progress
            //     .modules
            //     .iter()
            //     .filter_map(|module| {
            //         use crate::pb::sf::substreams::rpc::v2::module_progress::Type;

            //         if let Type::ProcessedRanges(range) = module.r#type.as_ref().unwrap() {
            //             Some(format!(
            //                 "{} @ [{}]",
            //                 module.name,
            //                 range
            //                     .processed_ranges
            //                     .iter()
            //                     .map(|x| x.to_string())
            //                     .collect::<Vec<_>>()
            //                     .join(", ")
            //             ))
            //         } else {
            //             None
            //         }
            //     })
            //     .collect();

            trace!("Progress {:?}", progress);

            BlockProcessedResult::Skip()
        }
        None => {
            warn!("Got None on substream message");
            BlockProcessedResult::Skip()
        }
        _ => BlockProcessedResult::Skip(),
    }
}

impl Stream for SubstreamsStream {
    type Item = Result<BlockResponse, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::substreams::mock::{start_scripted_mock_substreams, MockResponse};

    async fn stream_against(script: Vec<MockResponse>) -> (SubstreamsStream, MockRequests) {
        let (captured, addr) = start_scripted_mock_substreams(script).await;
        let endpoint = SubstreamsEndpoint::new(format!("http://{addr}"), Some("token".to_string()))
            .await
            .expect("endpoint");
        let stream = SubstreamsStream::new(
            Arc::new(endpoint),
            None,
            None,
            "test_module".to_string(),
            42,
            0,
            false,
            "test_extractor".to_string(),
            false,
        );
        (stream, captured)
    }

    type MockRequests = std::sync::Arc<std::sync::Mutex<Vec<Request>>>;

    #[tokio::test]
    async fn test_unauthenticated_before_first_block_is_fatal() {
        let (mut stream, captured) = stream_against(vec![MockResponse::Unauthenticated]).await;

        let item = stream
            .next()
            .await
            .expect("stream should yield an item");
        let Err(err) = item else {
            panic!("a credential that never worked must not be retried");
        };

        assert!(
            err.to_string()
                .contains("Unauthenticated"),
            "unexpected error: {err}"
        );
        assert_eq!(captured.lock().unwrap().len(), 1, "the endpoint must not be dialled again");
    }

    #[tokio::test]
    async fn test_unauthenticated_after_first_block_reconnects() {
        let (mut stream, captured) = stream_against(vec![
            MockResponse::BlockThenUnauthenticated { cursor: "cursor-1".to_string() },
            MockResponse::Ok,
        ])
        .await;

        let block = stream
            .next()
            .await
            .expect("stream should yield a block")
            .expect("first block should not error");
        assert!(matches!(block, BlockResponse::New(_)));

        let ended = stream
            .next()
            .await
            .expect("stream should yield an item")
            .expect("a proven credential must be retried, not surfaced as an error");
        assert!(matches!(ended, BlockResponse::Ended));

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 2, "the stream should have reconnected once");
        assert_eq!(
            requests[1].start_cursor, "cursor-1",
            "the reconnect should resume from the last cursor"
        );
    }

    #[test]
    fn test_unauthenticated_not_retried_before_first_block() {
        assert!(!should_retry_unauthenticated(false, 0));
    }

    #[test]
    fn test_unauthenticated_retried_after_first_block() {
        assert!(should_retry_unauthenticated(true, 0));
    }

    #[test]
    fn test_unauthenticated_retries_are_bounded() {
        assert!(should_retry_unauthenticated(true, MAX_UNAUTHENTICATED_RETRIES - 1));
        assert!(!should_retry_unauthenticated(true, MAX_UNAUTHENTICATED_RETRIES));
    }
}
