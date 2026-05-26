//! This module contains Tycho Websocket implementation
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use actix::{
    Actor, ActorContext, ActorFutureExt, AsyncContext, SpawnHandle, StreamHandler, WrapFuture,
};
use actix_web::{web, Error, HttpRequest, HttpResponse};
use actix_web_actors::ws;
use metrics::{counter, gauge};
use tracing::{debug, error, info, instrument, trace, warn};
use tycho_common::{
    dto::{BlockAggregatedChanges, Command, Response, WebSocketMessage},
    models::{error::WebsocketError, ExtractorIdentity},
};
use uuid::Uuid;

use crate::extractor::runner::MessageSender;

/// How often heartbeat pings are sent
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// How long before lack of client response causes a timeout
const CLIENT_TIMEOUT: Duration = Duration::from_secs(90);

pub type MessageSenderMap = HashMap<ExtractorIdentity, Arc<dyn MessageSender + Send + Sync>>;

/// Shared application data between all connections
/// The subscribers map is read-only after initialization, so no mutex is needed
pub struct WsData {
    /// There is one extractor subscriber per extractor identity
    pub subscribers: Arc<MessageSenderMap>,
}

impl WsData {
    pub fn new(extractors: MessageSenderMap) -> Self {
        Self { subscribers: Arc::new(extractors) }
    }
}

struct SubscriptionState {
    handle: SpawnHandle,
    chain: String,
    extractor: String,
    partial_blocks: bool,
}

/// Actor handling a single WS connection
///
/// This actor is responsible for:
/// - Receiving and forwarding messages from the extractor
/// - Receiving and handling commands from the client
pub struct WsActor {
    id: Uuid,
    /// Client must send ping at least once per 10 seconds (CLIENT_TIMEOUT), otherwise we drop the
    /// connection.
    heartbeat: Instant,
    app_state: web::Data<WsData>,
    subscriptions: HashMap<Uuid, SubscriptionState>,
    /// Tracks which subscriptions have zstd compression enabled
    compression_enabled: HashMap<Uuid, bool>,
    user_identity: Option<String>,
    client_version: String,
}

impl WsActor {
    fn new(
        app_state: web::Data<WsData>,
        user_identity: Option<String>,
        client_version: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            heartbeat: Instant::now(),
            app_state,
            subscriptions: HashMap::new(),
            compression_enabled: HashMap::new(),
            user_identity,
            client_version,
        }
    }

    /// Entry point for the WS connection
    #[instrument(skip_all)]
    pub async fn ws_index(
        req: HttpRequest,
        stream: web::Payload,
        data: web::Data<WsData>,
    ) -> Result<HttpResponse, Error> {
        let user_identity = req
            .headers()
            .get("user-identity")
            .map(|value| {
                value
                    .to_str()
                    .unwrap_or("unknown")
                    .to_string()
            });
        let client_version = req
            .headers()
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        let ws_actor = WsActor::new(data, user_identity, client_version);

        ws::start(ws_actor, &req, stream)
    }

    /// Helper method that sends heartbeat ping to client every 5 seconds (HEARTBEAT_INTERVAL)
    /// Also this method checks heartbeats from client
    #[instrument(level = "TRACE", skip_all, fields(WsActor.id = %self.id))]
    fn heartbeat(&mut self, ctx: &mut <Self as Actor>::Context) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            // Check client heartbeats
            if Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                warn!("Websocket Client heartbeat failed, disconnecting!");
                counter!("websocket_connections_dropped", "reason" => "timeout").increment(1);
                ctx.close(Some(ws::CloseReason {
                    code: ws::CloseCode::Away,
                    description: Some("Client heartbeat failed".into()),
                }));
                ctx.stop();
                return;
            }
            // Send ping
            ctx.ping(b"");
        });
    }

    /// Subscribe to an extractor
    ///
    /// This method handles WebSocket subscription requests asynchronously to avoid deadlocks.
    ///
    /// ## Design Decision: Async Spawning vs Blocking
    ///
    /// Previously, this method used `block_on()` while holding a mutex, which caused deadlocks
    /// when multiple clients subscribed simultaneously. The `block_on()` call would block the
    /// entire Actix runtime thread, preventing other actors from processing messages and even
    /// preventing the async operation itself from completing.
    ///
    /// ## Current Approach: Lock-Free Async
    ///
    /// We now use `ctx.spawn()` to handle the subscription asynchronously with direct HashMap
    /// access (no mutex needed since the subscribers map is read-only after initialization).
    /// This approach:
    ///
    /// **Advantages:**
    /// - Prevents runtime deadlocks by not blocking the actor's message processing
    /// - Allows unlimited concurrent subscriptions with zero lock contention
    /// - Eliminates all mutex-related performance overhead and deadlock possibilities
    ///
    /// **Trade-offs:**
    /// - The subscription setup is fire-and-forget - we don't wait for completion
    /// - If the WebSocket disconnects quickly, the subscription future might not complete
    /// - The client gets the NewSubscription response only after the async operation completes
    ///
    /// **Why This Is Acceptable:**
    /// - WebSocket connections are typically long-lived, so the async completion usually succeeds
    /// - The alternative (blocking) would prevent any concurrent subscriptions from working
    /// - Failed subscriptions are handled gracefully with error responses to the client
    ///
    /// ## Implementation Notes:
    ///
    /// 1. We access the subscribers HashMap directly (no mutex needed - it's read-only)
    /// 2. We spawn an async future that handles the extractor subscription
    /// 3. The future's completion handler updates actor state and sends the response to the client
    /// 4. If the future fails, an error response is sent instead
    #[instrument(skip(self, ctx), fields(WsActor.id = %self.id, subscription_id))]
    fn subscribe(
        &mut self,
        ctx: &mut ws::WebsocketContext<Self>,
        extractor_id: &ExtractorIdentity,
        include_state: bool,
        compression: bool,
        partial_blocks: bool,
    ) {
        let extractor_id = extractor_id.clone();
        // Step 1: Direct HashMap access (no mutex needed since map is read-only after
        // initialization)
        let message_sender = {
            debug!(extractor=?extractor_id, "Looking up extractor in subscribers map..");

            if let Some(message_sender) = self
                .app_state
                .subscribers
                .get(&extractor_id)
            {
                message_sender.clone()
            } else {
                let available = self
                    .app_state
                    .subscribers
                    .keys()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>();

                let error = WebsocketError::ExtractorNotFound(extractor_id.clone());
                error!(%error, available_extractors = ?available, "Extractor not found in hashmap");

                ctx.text(
                    serde_json::to_string(&WebSocketMessage::Response(Response::Error(
                        error.into(),
                    )))
                    .expect("WebsocketMessage serialize infallible"),
                );
                return;
            }
        };

        // Step 2: Generate subscription ID and prepare for async operation
        // Generate a unique ID for this subscription
        let subscription_id = Uuid::new_v4();

        // Add the subscription_id to the current tracing span recorded fields
        tracing::Span::current().record("subscription_id", subscription_id.to_string());

        info!(extractor_id = %extractor_id, "Subscribing to extractor");

        debug!(actor_id = %self.id, "About to call message_sender.subscribe() asynchronously");
        let start_time = std::time::Instant::now();
        let actor_id = self.id;
        let extractor_id_for_future = extractor_id.clone();
        let extractor_id_for_error = extractor_id.clone();

        // Step 3: Create async future for subscription setup
        // This future will run independently without blocking the actor's message processing
        // Use async operation instead of block_on to prevent runtime deadlocks
        let fut = async move {
            match message_sender.subscribe().await {
                Ok(mut rx) => {
                    let elapsed = start_time.elapsed();
                    debug!(actor_id = %actor_id, elapsed_ms = elapsed.as_millis(), "subscribe completed successfully");

                    let stream = async_stream::stream! {
                        while let Some(item) = rx.recv().await {
                            if item.revert {
                                // For reverts
                                // Exclude partials if the client requested full blocks
                                if !partial_blocks && item.is_partial() {
                                    continue;
                                }
                            } else {
                                // For new blocks
                                // Only forward items matching the partial/full preference
                                if partial_blocks != item.is_partial() {
                                    continue;
                                }
                            }

                            let result = if include_state {
                                (*item).clone().into()
                            } else {
                                item.drop_state().into()
                            };

                            yield ExtractorEvent::Message(subscription_id, Box::new(result));
                        }
                        // Channel closed: extractor restarted or stopped.
                        yield ExtractorEvent::ChannelClosed(subscription_id);
                    };

                    Some((subscription_id, stream, extractor_id_for_future.clone()))
                }
                Err(err) => {
                    let elapsed = start_time.elapsed();
                    debug!(actor_id = %actor_id, elapsed_ms = elapsed.as_millis(), "subscribe failed");
                    error!(error = %err, "Failed to subscribe to the extractor");
                    None
                }
            }
        };

        // Step 4: Spawn the async future using ctx.spawn()
        // This is fire-and-forget: we don't wait for completion to avoid blocking
        // The future will complete independently and update actor state when done
        ctx.spawn(
            fut.into_actor(self)
                .map(move |result, actor, ctx| {
                    // Step 5: Handle async completion - this runs when the subscription future
                    // finishes If successful: add stream to actor, update
                    // metrics, send success response to client If failed: send
                    // error response to client
                    match result {
                        Some((subscription_id, stream, extractor_id)) => {
                            let handle = ctx.add_stream(stream);
                            let chain = extractor_id.chain.to_string();
                            let extractor = extractor_id.name.to_string();
                            let user_identity = actor
                                .user_identity
                                .as_deref()
                                .unwrap_or("unknown")
                                .to_owned();
                            actor.subscriptions.insert(
                                subscription_id,
                                SubscriptionState {
                                    handle,
                                    chain: chain.clone(),
                                    extractor: extractor.clone(),
                                    partial_blocks,
                                },
                            );
                            actor
                                .compression_enabled
                                .insert(subscription_id, compression);
                            gauge!(
                                "websocket_subscriptions_active",
                                "user_identity" => user_identity,
                                "chain" => chain,
                                "extractor" => extractor,
                                "partial_blocks" => partial_blocks.to_string(),
                            )
                            .increment(1);

                            let message = Response::NewSubscription {
                                extractor_id: extractor_id.into(),
                                subscription_id,
                            };
                            ctx.text(serde_json::to_string(&message).unwrap());
                        }
                        None => {
                            let error = WebsocketError::SubscribeError(extractor_id_for_error);
                            ctx.text(
                                serde_json::to_string(&WebSocketMessage::Response(
                                    Response::Error(error.into()),
                                ))
                                .expect("WebsocketMessage serialize infallible"),
                            );
                        }
                    }
                }),
        );
    }

    #[instrument(skip(self, ctx), fields(WsActor.id = %self.id))]
    fn unsubscribe(&mut self, ctx: &mut ws::WebsocketContext<Self>, subscription_id: Uuid) {
        info!(%subscription_id, "Unsubscribing from subscription");

        if let Some(sub) = self
            .subscriptions
            .remove(&subscription_id)
        {
            debug!("Subscription ID found");
            ctx.cancel_future(sub.handle);
            self.compression_enabled
                .remove(&subscription_id);
            debug!("Cancelled subscription future");
            gauge!(
                "websocket_subscriptions_active",
                "user_identity" => self.user_identity.as_deref().unwrap_or("unknown").to_owned(),
                "chain" => sub.chain,
                "extractor" => sub.extractor,
                "partial_blocks" => sub.partial_blocks.to_string(),
            )
            .decrement(1);

            let message = Response::SubscriptionEnded { subscription_id };
            ctx.text(serde_json::to_string(&message).unwrap());
        } else {
            error!(%subscription_id, "Subscription ID not found");

            let error = WebsocketError::SubscriptionNotFound(subscription_id);
            ctx.text(
                serde_json::to_string(&WebSocketMessage::Response(Response::Error(error.into())))
                    .expect("WebsocketMessage serialize infallible"),
            );
        }
    }
}

impl Actor for WsActor {
    type Context = ws::WebsocketContext<Self>;

    #[instrument(skip_all, fields(WsActor.id = %self.id), name = "WsActor::started")]
    fn started(&mut self, ctx: &mut Self::Context) {
        info!("Websocket connection established");

        gauge!(
            "websocket_connections_active",
            "user_identity" => self.user_identity.as_deref().unwrap_or("unknown").to_owned(),
            "client_version" => self.client_version.clone(),
        )
        .increment(1);

        // Start the heartbeat
        self.heartbeat(ctx);
    }

    #[instrument(skip_all, fields(WsActor.id = %self.id), name = "WsActor::stopped")]
    fn stopped(&mut self, ctx: &mut Self::Context) {
        info!("Websocket connection closed");

        gauge!(
            "websocket_connections_active",
            "user_identity" => self.user_identity.as_deref().unwrap_or("unknown").to_owned(),
            "client_version" => self.client_version.clone(),
        )
        .decrement(1);

        // Close all remaining subscriptions
        let user_identity = self
            .user_identity
            .as_deref()
            .unwrap_or("unknown")
            .to_owned();
        for (_, sub) in self.subscriptions.drain() {
            ctx.cancel_future(sub.handle);
            gauge!(
                "websocket_subscriptions_active",
                "user_identity" => user_identity.clone(),
                "chain" => sub.chain,
                "extractor" => sub.extractor,
                "partial_blocks" => sub.partial_blocks.to_string(),
            )
            .decrement(1);
        }
    }
}

enum ExtractorEvent {
    Message(Uuid, Box<BlockAggregatedChanges>),
    ChannelClosed(Uuid),
}

/// Handle incoming messages from the extractor and forward them to the WS connection.
///
/// `ExtractorEvent::ChannelClosed` is emitted when the extractor's channel closes (e.g. after a
/// restart). It lets the actor send `SubscriptionEnded` to the client and clean up the
/// subscription without dropping the entire WebSocket connection.
impl StreamHandler<ExtractorEvent> for WsActor {
    #[instrument(skip_all, fields(WsActor.id = %self.id))]
    fn handle(&mut self, msg: ExtractorEvent, ctx: &mut Self::Context) {
        match msg {
            ExtractorEvent::Message(subscription_id, deltas) => {
                trace!("Forwarding message to client");
                let msg =
                    WebSocketMessage::BlockAggregatedChanges { deltas: *deltas, subscription_id };
                let json_str = serde_json::to_string(&msg).unwrap();

                // Check if compression is enabled for this subscription
                let compression_enabled = self
                    .compression_enabled
                    .get(&subscription_id)
                    .copied()
                    .unwrap_or(false);

                if compression_enabled {
                    // Compress the message using zstd
                    match zstd::encode_all(json_str.as_bytes(), 0) {
                        Ok(compressed) => {
                            trace!("Sending compressed message (compressed size: {} bytes, original size: {} bytes)",
                                compressed.len(), json_str.len());
                            ctx.binary(compressed);
                        }
                        Err(e) => {
                            error!(
                                error = %e,
                                "Failed to compress message for subscription"
                            );
                            counter!("websocket_compression_errors",).increment(1);

                            // Send compression error to client
                            let compression_error =
                                WebsocketError::CompressionError(subscription_id, e);
                            let error_msg = WebSocketMessage::Response(Response::Error(
                                compression_error.into(),
                            ));
                            ctx.text(
                                serde_json::to_string(&error_msg)
                                    .expect("WebsocketMessage serialize infallible"),
                            );
                        }
                    }
                } else {
                    // Send uncompressed text message (backward compatible)
                    ctx.text(json_str);
                }
            }
            ExtractorEvent::ChannelClosed(subscription_id) => {
                // Extractor channel closed — clean up this subscription and notify the client.
                // Other subscriptions on this connection are unaffected.
                warn!(
                    %subscription_id,
                    "Extractor channel closed; notifying client and ending subscription"
                );
                if let Some(sub) = self
                    .subscriptions
                    .remove(&subscription_id)
                {
                    self.compression_enabled
                        .remove(&subscription_id);
                    gauge!(
                        "websocket_subscriptions_active",
                        "user_identity" => self.user_identity.as_deref().unwrap_or("unknown").to_owned(),
                        "chain" => sub.chain,
                        "extractor" => sub.extractor,
                        "partial_blocks" => sub.partial_blocks.to_string(),
                    )
                    .decrement(1);
                }
                let message = Response::SubscriptionEnded { subscription_id };
                ctx.text(serde_json::to_string(&message).unwrap());
            }
        }
    }

    fn finished(&mut self, ctx: &mut <Self as Actor>::Context) {
        // If no subscriptions remain, close the connection. If other subscriptions are still
        // active, keep the connection open; they were unaffected by this stream ending.
        if self.subscriptions.is_empty() {
            ctx.close(Some(ws::CloseReason {
                code: ws::CloseCode::Normal,
                description: Some("All subscriptions ended".into()),
            }));
            ctx.stop();
        }
    }
}

/// Handle incoming messages from the WS connection
impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsActor {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        trace!("WsActor {}: StreamHandler::handle called", self.id);
        match msg {
            Ok(ws::Message::Ping(msg)) => {
                trace!("Websocket ping message received");
                self.heartbeat = Instant::now();
                ctx.pong(&msg);
            }
            Ok(ws::Message::Pong(_)) => {
                trace!("Websocket pong message received");
                self.heartbeat = Instant::now();
            }
            Ok(ws::Message::Text(text)) => {
                trace!(actor_id = %self.id, text = %text, "Websocket text message received");

                // Try to deserialize the message to a Message enum
                match serde_json::from_str::<Command>(&text) {
                    Ok(message) => {
                        // Handle the message based on its variant
                        match message {
                            Command::Subscribe {
                                extractor_id,
                                include_state,
                                compression,
                                partial_blocks,
                            } => {
                                trace!(actor_id = %self.id, %extractor_id, "Message handler: Processing subscribe request");
                                self.subscribe(
                                    ctx,
                                    &extractor_id.clone().into(),
                                    include_state,
                                    compression,
                                    partial_blocks,
                                );
                                trace!(actor_id = %self.id, %extractor_id, "Message handler: Subscribe method completed");
                            }
                            Command::Unsubscribe { subscription_id } => {
                                trace!(%subscription_id, "Unsubscribing from subscription");
                                self.unsubscribe(ctx, subscription_id);
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to parse message");

                        let error = WebsocketError::ParseError(text.to_string(), e);
                        ctx.text(
                            serde_json::to_string(&WebSocketMessage::Response(Response::Error(
                                error.into(),
                            )))
                            .expect("WebsocketMessage serialize infallible"),
                        );
                    }
                }
            }
            Ok(ws::Message::Binary(bin)) => {
                trace!("Websocket binary message received");
                ctx.binary(bin)
            }
            Ok(ws::Message::Close(reason)) => {
                trace!(reason = ?reason, "Websocket close message received");
                ctx.close(reason);
                ctx.stop()
            }
            Err(err) => {
                error!(error = %err, "Failed to receive message from websocket");
                counter!("websocket_connections_dropped", "reason" => "network_error").increment(1);
                ctx.stop()
            }
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use actix_rt::time::timeout;
    use actix_test::{start, start_with, TestServerConfig};
    use actix_web::App;
    use actix_web_opentelemetry::RequestTracing;
    use async_trait::async_trait;
    use chrono::DateTime;
    use futures03::SinkExt;
    use rstest::rstest;
    use serde::Deserialize;
    use tokio::{
        net::TcpStream,
        sync::mpsc::{self, error::SendError, Receiver},
    };
    use tokio_stream::StreamExt;
    use tokio_tungstenite::{
        tungstenite::{
            protocol::{frame::coding::CloseCode, CloseFrame},
            Message,
        },
        MaybeTlsStream, WebSocketStream,
    };
    use tracing::{debug, info_span, Instrument};
    use tycho_common::{
        dto::{self, Response},
        models::{
            blockchain::{Block, BlockAggregatedChanges},
            Chain,
        },
        Bytes,
    };

    use super::*;
    use crate::extractor::{runner::ControlMessage, ExtractorMsg};

    pub struct MyMessageSender {
        extractor_id: ExtractorIdentity,
        block_template: BlockAggregatedChanges,
    }

    impl MyMessageSender {
        pub fn new(extractor_id: ExtractorIdentity) -> Self {
            let block_template = Self::default_block(&extractor_id.name);
            Self { extractor_id, block_template }
        }

        /// Creates a default `BlockAggregatedChanges` for testing purposes.
        pub fn default_block(extractor_name: &str) -> BlockAggregatedChanges {
            BlockAggregatedChanges {
                extractor: extractor_name.to_string(),
                block: Block::new(
                    1,
                    Chain::Ethereum,
                    Bytes::zero(32),
                    Bytes::zero(32),
                    DateTime::from_timestamp(0, 0)
                        .unwrap()
                        .naive_utc(),
                ),
                db_committed_block_height: None,
                finalized_block_height: 1,
                revert: false,
                ..Default::default()
            }
        }

        /// Set a custom block template to send instead of the default.
        pub fn with_block(mut self, block: BlockAggregatedChanges) -> Self {
            self.block_template = block;
            self
        }
    }

    #[async_trait]
    impl MessageSender for MyMessageSender {
        async fn subscribe(&self) -> Result<Receiver<ExtractorMsg>, SendError<ControlMessage>> {
            let (tx, rx) = mpsc::channel::<ExtractorMsg>(1);
            let extractor_id = self.extractor_id.clone();
            let block_template = self.block_template.clone();

            // Spawn a task that sends messages every 100ms
            tokio::spawn(async move {
                // clippy thinks applying `instrument` to a loop block is a mistake
                #[allow(clippy::unit_arg)]
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    debug!("Sending DummyMessage");
                    if tx
                        .send(Arc::new(block_template.clone()))
                        .await
                        .is_err()
                    {
                        debug!("Receiver dropped");
                        break;
                    }
                }
                .instrument(info_span!("DummyMessageSender", extractor_id = %extractor_id))
            });

            Ok(rx)
        }
    }

    #[actix_rt::test]
    async fn test_websocket_ping_pong() {
        tracing_subscriber::fmt()
            .with_test_writer()
            .try_init()
            .unwrap_or_else(|_| debug!("Subscriber already initialized"));

        let app_state = web::Data::new(WsData::new(HashMap::new()));
        let server = start(move || {
            App::new()
                .wrap(RequestTracing::new())
                .app_data(app_state.clone())
                .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
        });

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);
        debug!("Connecting to test server at {}", &url);

        // Connect to the server
        let (mut connection, _response) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        debug!("Connected to test server");

        // Test sending ping message and receiving pong message
        connection
            .send(Message::Ping(vec![]))
            .await
            .expect("Failed to send ping message");

        debug!("Sent ping message");

        let msg = timeout(Duration::from_secs(1), connection.next())
            .await
            .expect("Failed to receive message")
            .unwrap()
            .unwrap();

        if let Message::Pong(_) = msg {
            // Pong received as expected
            debug!("Received pong message");
        } else {
            panic!("Unexpected message {msg:?}");
        }

        // Close the connection
        connection
            .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
            .await
            .expect("Failed to send close message");
        debug!("Closed connection");
    }

    async fn wait_for_response<F>(
        connection: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
        mut criteria: F,
    ) -> Result<Message, String>
    where
        F: FnMut(&Message) -> bool,
    {
        loop {
            let response_msg = timeout(Duration::from_secs(5), connection.next())
                .await
                .map_err(|_| "Failed to receive message".to_string())?
                .ok_or("Connection closed".to_string())?
                .map_err(|_| "Failed to receive message".to_string())?;

            if criteria(&response_msg) {
                return Ok(response_msg);
            } else {
                debug!("Message did not meet criteria, waiting for the correct message");
            }
        }
    }

    #[derive(Deserialize)]
    struct DummyDelta {
        #[allow(dead_code)]
        subscription_id: Uuid,
        deltas: dto::BlockAggregatedChanges,
    }

    async fn wait_for_dummy_message(
        connection: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
        extractor_id: ExtractorIdentity,
    ) -> Result<DummyDelta, String> {
        let criteria = move |msg: &Message| {
            if let Message::Text(text) = msg {
                if let Ok(DummyDelta { subscription_id: _, deltas }) =
                    serde_json::from_str::<DummyDelta>(text)
                {
                    debug!(extractor_id = %extractor_id, "Received dummy message");
                    return deltas.extractor == extractor_id.name;
                }
            }
            false
        };

        if let Message::Text(response_text) = wait_for_response(connection, criteria).await? {
            serde_json::from_str(&response_text).map_err(|e| e.to_string())
        } else {
            Err("Received a non-text message".to_string())
        }
    }

    async fn wait_for_new_subscription(
        connection: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Result<Response, String> {
        let criteria = |msg: &Message| {
            if let Message::Text(text) = msg {
                if let Ok(message) = serde_json::from_str::<Response>(text) {
                    matches!(message, Response::NewSubscription { .. })
                } else {
                    false
                }
            } else {
                false
            }
        };

        if let Message::Text(response_text) = wait_for_response(connection, criteria).await? {
            serde_json::from_str(&response_text).map_err(|e| e.to_string())
        } else {
            Err("Received a non-text message".to_string())
        }
    }

    async fn wait_for_subscription_ended(
        connection: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Result<Response, String> {
        let criteria = |msg: &Message| {
            if let Message::Text(text) = msg {
                if let Ok(message) = serde_json::from_str::<Response>(text) {
                    matches!(message, Response::SubscriptionEnded { .. })
                } else {
                    false
                }
            } else {
                false
            }
        };

        if let Message::Text(response_text) = wait_for_response(connection, criteria).await? {
            serde_json::from_str(&response_text).map_err(|e| e.to_string())
        } else {
            Err("Received a non-text message".to_string())
        }
    }

    #[actix_rt::test]
    async fn test_subscribe_and_unsubscribe() -> Result<(), String> {
        tracing_subscriber::fmt()
            .with_test_writer()
            .try_init()
            .unwrap_or_else(|_| debug!("Subscriber already initialized"));

        // Add the extractor handle to AppState
        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "dummy");
        let extractor_id2 = ExtractorIdentity::new(Chain::Ethereum, "dummy2");

        let message_sender = Arc::new(MyMessageSender::new(extractor_id.clone()));
        let message_sender2 = Arc::new(MyMessageSender::new(extractor_id2.clone()));

        let mut subscribers_map = HashMap::new();
        subscribers_map
            .insert(extractor_id.clone(), message_sender as Arc<dyn MessageSender + Send + Sync>);
        subscribers_map
            .insert(extractor_id2.clone(), message_sender2 as Arc<dyn MessageSender + Send + Sync>);

        let app_state = web::Data::new(WsData::new(subscribers_map));

        // Setup WebSocket server and client, similar to existing test
        let server = start_with(
            TestServerConfig::default().client_request_timeout(Duration::from_secs(5)),
            move || {
                App::new()
                    .wrap(RequestTracing::new())
                    .app_data(app_state.clone())
                    .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
            },
        );

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);
        debug!(url = %url, "Connecting to test server");

        // Connect to the server
        let (mut connection, _response) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        debug!("Connected to test server");

        // Create and send a subscribe message from the client
        let action = Command::Subscribe {
            extractor_id: extractor_id.clone().into(),
            include_state: true,
            compression: false,
            partial_blocks: false,
        };
        connection
            .send(Message::Text(serde_json::to_string(&action).unwrap()))
            .await
            .expect("Failed to send subscribe message");
        debug!("Sent subscribe message");

        // Accept the subscription ID
        let response = wait_for_new_subscription(&mut connection)
            .await
            .expect("Failed to get the expected new subscription message");
        let first_subscription_id = if let Response::NewSubscription {
            extractor_id: _extractor_id,
            subscription_id: first_subscription_id,
        } = response
        {
            debug!(first_subscription_id = ?first_subscription_id, "Received first subscription ID");
            first_subscription_id
        } else {
            panic!("Unexpected response: {response:?}");
        };

        // Receive the DummyMessage from the server
        let _message = wait_for_dummy_message(&mut connection, extractor_id.clone())
            .await
            .expect("Failed to get the expected DummyMessage");
        debug!("Received DummyMessage from server");

        // Create and send a second subscribe message from the client
        let action = Command::Subscribe {
            extractor_id: extractor_id2.clone().into(),
            include_state: true,
            compression: false,
            partial_blocks: false,
        };
        connection
            .send(Message::Text(serde_json::to_string(&action).unwrap()))
            .await
            .expect("Failed to send subscribe message");
        debug!("Sent subscribe message for second extractor");

        // Accept the second subscription ID
        let response = wait_for_new_subscription(&mut connection)
            .await
            .expect("Failed to get the expected new subscription message");
        if let Response::NewSubscription {
            extractor_id: _extractor_id2,
            subscription_id: second_subscription_id,
        } = response
        {
            debug!(second_subscription_id = ?second_subscription_id, "Received second subscription ID");
        } else {
            panic!("Unexpected response: {response:?}");
        }

        // Receive the DummyMessage from the second exractor
        let _message = wait_for_dummy_message(&mut connection, extractor_id2.clone())
            .await
            .expect("Failed to get the expected DummyMessage");
        debug!("Received DummyMessage2 from server");

        // Create and send a unsubscribe message from the client
        let action = Command::Unsubscribe { subscription_id: first_subscription_id };
        connection
            .send(Message::Text(serde_json::to_string(&action).unwrap()))
            .await
            .expect("Failed to send unsubscribe message");
        debug!("Sent unsubscribe message");

        // Accept the unsubscription ID
        let response = wait_for_subscription_ended(&mut connection)
            .await
            .expect("Failed to get the expected subscription ended message");
        if let Response::SubscriptionEnded { subscription_id } = response {
            debug!(subscription_id = ?subscription_id,"Received unsubscription ID");
        } else {
            panic!("Unexpected response: {response:?}");
        }

        // Try to receive a DummyMessage from the first extractor (expecting timeout to occur)
        let result =
            timeout(Duration::from_secs(2), wait_for_dummy_message(&mut connection, extractor_id))
                .await;
        assert!(result.is_err(), "Received a message from the first extractor after unsubscribing");

        // Receive the DummyMessage from the second exractor
        let _message = wait_for_dummy_message(&mut connection, extractor_id2)
            .await
            .expect("Failed to get the expected DummyMessage");
        debug!("Received DummyMessage2 from server");

        // Close the connection
        connection
            .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
            .await
            .expect("Failed to send close message");
        debug!("Closed connection");

        Ok(())
    }

    #[actix_rt::test]
    async fn test_subscribe_with_compression() -> Result<(), String> {
        tracing_subscriber::fmt()
            .with_test_writer()
            .try_init()
            .unwrap_or_else(|_| debug!("Subscriber already initialized"));

        // Setup extractor with message sender
        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "compressed_dummy");
        let message_sender = Arc::new(MyMessageSender::new(extractor_id.clone()));

        let mut subscribers_map = HashMap::new();
        subscribers_map
            .insert(extractor_id.clone(), message_sender as Arc<dyn MessageSender + Send + Sync>);

        let app_state = web::Data::new(WsData::new(subscribers_map));

        // Setup WebSocket server
        let server = start_with(
            TestServerConfig::default().client_request_timeout(Duration::from_secs(5)),
            move || {
                App::new()
                    .wrap(RequestTracing::new())
                    .app_data(app_state.clone())
                    .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
            },
        );

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);
        debug!(url = %url, "Connecting to test server");

        // Connect to the server
        let (mut connection, _response) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        debug!("Connected to test server");

        // Subscribe with compression enabled
        let action = Command::Subscribe {
            extractor_id: extractor_id.clone().into(),
            include_state: true,
            compression: true,
            partial_blocks: false,
        };
        connection
            .send(Message::Text(serde_json::to_string(&action).unwrap()))
            .await
            .expect("Failed to send subscribe message");
        debug!("Sent subscribe message with compression=true");

        // Wait for subscription confirmation
        let response = wait_for_new_subscription(&mut connection)
            .await
            .expect("Failed to get the expected new subscription message");
        if let Response::NewSubscription { extractor_id: _extractor_id, subscription_id } = response
        {
            debug!(subscription_id = ?subscription_id, "Received subscription ID");
        } else {
            panic!("Unexpected response: {response:?}");
        }

        // Wait for a binary message (compressed)
        let compressed_msg = timeout(Duration::from_secs(2), connection.next())
            .await
            .expect("Timeout waiting for message")
            .expect("Connection closed")
            .expect("Failed to receive message");

        // Verify it's a binary message and decompress it
        if let Message::Binary(compressed_data) = compressed_msg {
            debug!("Received binary (compressed) message: {} bytes", compressed_data.len());

            // Decompress using zstd
            let decompressed =
                zstd::decode_all(&compressed_data[..]).expect("Failed to decompress message");
            debug!("Decompressed to {} bytes", decompressed.len());

            // Parse as JSON to verify it's valid
            let json_str = String::from_utf8(decompressed).expect("Failed to decode UTF-8");
            let _message: DummyDelta =
                serde_json::from_str(&json_str).expect("Failed to parse decompressed JSON");
            debug!("Successfully parsed compressed message");
        } else {
            panic!("Expected binary message, got: {:?}", compressed_msg);
        }

        // Close the connection
        connection
            .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
            .await
            .expect("Failed to send close message");
        debug!("Closed connection");

        Ok(())
    }

    #[test]
    fn test_msg() {
        // Create and send a subscribe message from the client
        let extractor_id =
            ExtractorIdentity { chain: Chain::Ethereum, name: "vm:ambient".to_owned() };
        let action = Command::Subscribe {
            extractor_id: extractor_id.into(),
            include_state: true,
            compression: false,
            partial_blocks: false,
        };
        let res = serde_json::to_string(&action).unwrap();
        println!("{res}");
    }

    /// Message sender that simulates slow operations to trigger the deadlock
    pub struct SlowMessageSender {
        extractor_id: ExtractorIdentity,
    }

    impl SlowMessageSender {
        pub fn new(extractor_id: ExtractorIdentity) -> Self {
            Self { extractor_id }
        }
    }

    #[async_trait]
    impl MessageSender for SlowMessageSender {
        async fn subscribe(&self) -> Result<Receiver<ExtractorMsg>, SendError<ControlMessage>> {
            debug!("SlowMessageSender::subscribe() starting 200ms delay");
            // Add a delay to increase the window for deadlock
            tokio::time::sleep(Duration::from_millis(200)).await;
            debug!("SlowMessageSender::subscribe() delay completed, creating channel");

            let (tx, rx) = mpsc::channel::<ExtractorMsg>(1);
            let extractor_id = self.extractor_id.clone();

            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    if tx
                        .send(Arc::new(BlockAggregatedChanges {
                            extractor: extractor_id.name.clone(),
                            block: Block::new(
                                1,
                                Chain::Ethereum,
                                Bytes::zero(32),
                                Bytes::zero(32),
                                DateTime::from_timestamp(0, 0)
                                    .unwrap()
                                    .naive_utc(),
                            ),
                            db_committed_block_height: None,
                            finalized_block_height: 1,
                            revert: false,
                            ..Default::default()
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });

            Ok(rx)
        }
    }

    #[actix_rt::test]
    async fn test_deadlock_concurrent_subscriptions() {
        tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::DEBUG)
            .try_init()
            .unwrap_or_else(|_| debug!("Subscriber already initialized"));

        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "deadlock_test");

        // Use SlowMessageSender to recreate the deadlock scenario
        let message_sender = Arc::new(SlowMessageSender::new(extractor_id.clone()));

        let mut subscribers_map = HashMap::new();
        subscribers_map
            .insert(extractor_id.clone(), message_sender as Arc<dyn MessageSender + Send + Sync>);

        let app_state = web::Data::new(WsData::new(subscribers_map));

        let server = start_with(
            TestServerConfig::default().client_request_timeout(Duration::from_secs(10)),
            move || {
                App::new()
                    .wrap(RequestTracing::new())
                    .app_data(app_state.clone())
                    .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
            },
        );

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);

        // Create multiple connections to trigger concurrent subscriptions
        let num_clients = 4;
        let mut connections = Vec::new();

        for i in 0..num_clients {
            let (connection, _) = tokio_tungstenite::connect_async(&url)
                .await
                .unwrap_or_else(|_| panic!("Failed to connect client {i}"));
            connections.push(connection);
        }

        let subscribe_msg = Command::Subscribe {
            extractor_id: extractor_id.clone().into(),
            include_state: true,
            compression: false,
            partial_blocks: false,
        };
        let msg_text = serde_json::to_string(&subscribe_msg).unwrap();

        // Send subscription requests from all clients simultaneously
        let tasks: Vec<_> = connections
            .into_iter()
            .enumerate()
            .map(|(i, mut connection)| {
                let msg_text = msg_text.clone();
                async move {
                    println!("Client {i} sending subscription request");
                    connection
                        .send(Message::Text(msg_text))
                        .await
                        .unwrap_or_else(|_| panic!("Failed to send from client {i}"));

                    // Try to receive response
                    let start = std::time::Instant::now();
                    let result = timeout(Duration::from_secs(3), connection.next()).await;
                    let elapsed = start.elapsed();

                    println!(
                        "Client {} completed in {:?}, success: {}",
                        i,
                        elapsed,
                        result.is_ok()
                    );
                    (i, result.is_ok(), elapsed)
                }
            })
            .collect();

        // Wait for all tasks with a reasonable timeout
        let start_time = std::time::Instant::now();
        let results = futures03::future::join_all(tasks).await;
        let total_time = start_time.elapsed();

        // Analyze results
        let successful = results
            .iter()
            .filter(|(_, success, _)| *success)
            .count();
        let failed = results.len() - successful;

        println!("Test completed in {total_time:?}");
        println!("Results: {successful} successful, {failed} failed");

        // With the original deadlock-prone code, we expect some failures due to timeouts
        // caused by the mutex being held during block_on() calls

        if failed > 0 {
            println!("DEADLOCK ISSUE DETECTED: {failed} out of {num_clients} clients failed");
            println!("This indicates the deadlock problem exists in the original code");
        } else {
            println!("ALL CLIENTS SUCCEEDED - deadlock not reproduced");
        }

        // For the test to be meaningful, we expect at least some clients to fail with original code
        // This test demonstrates the issue that needs to be fixed
    }

    #[test_log::test(actix_rt::test)]
    async fn test_extractor_not_found_error_response() {
        // Create app state with no extractors to trigger ExtractorNotFound
        let app_state = web::Data::new(WsData::new(HashMap::new()));
        let server = start(move || {
            App::new()
                .wrap(RequestTracing::new())
                .app_data(app_state.clone())
                .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
        });

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);

        let (mut connection, _response) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        // Send subscribe request for non-existent extractor
        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "non_existent");
        let action = Command::Subscribe {
            extractor_id: extractor_id.clone().into(),
            include_state: true,
            compression: false,
            partial_blocks: false,
        };
        connection
            .send(Message::Text(serde_json::to_string(&action).unwrap()))
            .await
            .expect("Failed to send subscribe message");

        // Wait for error response
        let response_msg = timeout(Duration::from_secs(1), connection.next())
            .await
            .expect("Failed to receive message")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = response_msg {
            let websocket_message: WebSocketMessage =
                serde_json::from_str(&text).expect("Failed to parse WebSocketMessage");

            if let WebSocketMessage::Response(Response::Error(error)) = websocket_message {
                match error {
                    tycho_common::dto::WebsocketError::ExtractorNotFound(reported_id) => {
                        assert_eq!(reported_id, extractor_id.into());
                    }
                    _ => panic!("Expected ExtractorNotFound error, got: {:?}", error),
                }
            } else {
                panic!("Expected error response, got: {:?}", websocket_message);
            }
        } else {
            panic!("Expected text message, got: {:?}", response_msg);
        }

        connection
            .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
            .await
            .expect("Failed to send close message");
    }

    #[test_log::test(actix_rt::test)]
    async fn test_subscription_not_found_error_response() {
        let app_state = web::Data::new(WsData::new(HashMap::new()));
        let server = start(move || {
            App::new()
                .wrap(RequestTracing::new())
                .app_data(app_state.clone())
                .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
        });

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);

        let (mut connection, _response) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        // Send unsubscribe request for non-existent subscription
        let fake_subscription_id = Uuid::new_v4();
        let action = Command::Unsubscribe { subscription_id: fake_subscription_id };
        connection
            .send(Message::Text(serde_json::to_string(&action).unwrap()))
            .await
            .expect("Failed to send unsubscribe message");

        // Wait for error response
        let response_msg = timeout(Duration::from_secs(1), connection.next())
            .await
            .expect("Failed to receive message")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = response_msg {
            let websocket_message: WebSocketMessage =
                serde_json::from_str(&text).expect("Failed to parse WebSocketMessage");

            if let WebSocketMessage::Response(Response::Error(error)) = websocket_message {
                match error {
                    tycho_common::dto::WebsocketError::SubscriptionNotFound(reported_id) => {
                        assert_eq!(reported_id, fake_subscription_id);
                    }
                    _ => panic!("Expected SubscriptionNotFound error, got: {:?}", error),
                }
            } else {
                panic!("Expected error response, got: {:?}", websocket_message);
            }
        } else {
            panic!("Expected text message, got: {:?}", response_msg);
        }

        connection
            .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
            .await
            .expect("Failed to send close message");
    }

    #[test_log::test(actix_rt::test)]
    async fn test_parse_error_response() {
        let app_state = web::Data::new(WsData::new(HashMap::new()));
        let server = start(move || {
            App::new()
                .wrap(RequestTracing::new())
                .app_data(app_state.clone())
                .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
        });

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);

        let (mut connection, _response) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        // Send malformed JSON
        let malformed_json = r#"{"method":"subscribe","invalid_json"#;
        connection
            .send(Message::Text(malformed_json.to_string()))
            .await
            .expect("Failed to send malformed message");

        // Wait for error response
        let response_msg = timeout(Duration::from_secs(1), connection.next())
            .await
            .expect("Failed to receive message")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = response_msg {
            let websocket_message: WebSocketMessage =
                serde_json::from_str(&text).expect("Failed to parse WebSocketMessage");

            if let WebSocketMessage::Response(Response::Error(error)) = websocket_message {
                match error {
                    tycho_common::dto::WebsocketError::ParseError(error, msg) => {
                        dbg!(&msg, &error);
                        assert!(error.contains("EOF while parsing"));
                        assert_eq!(msg, malformed_json)
                    }
                    _ => panic!("Expected ParseError, got: {:?}", error),
                }
            } else {
                panic!("Expected error response, got: {:?}", websocket_message);
            }
        } else {
            panic!("Expected text message, got: {:?}", response_msg);
        }

        connection
            .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
            .await
            .expect("Failed to send close message");
    }

    pub struct FailingMessageSender {
        _extractor_id: ExtractorIdentity,
    }

    impl FailingMessageSender {
        pub fn new(extractor_id: ExtractorIdentity) -> Self {
            Self { _extractor_id: extractor_id }
        }
    }

    #[async_trait]
    impl MessageSender for FailingMessageSender {
        async fn subscribe(&self) -> Result<Receiver<ExtractorMsg>, SendError<ControlMessage>> {
            // Always return an error to simulate subscription failure
            Err(SendError(ControlMessage::Stop))
        }
    }

    /// Test for partial_blocks subscription filter.
    ///
    /// Filter logic:
    /// - Non-reverts: only delivered when `sub_partial_blocks == is_partial`
    /// - Reverts:
    ///   - Partial subscribers (`send_partials: true`): receive ALL reverts
    ///   - Full block subscribers (`send_partials: false`): only receive full block reverts
    #[rstest]
    // Non-revert blocks: only received when subscription partial_blocks matches is_partial
    #[case::full_block_want_full(false, false, false, true)]
    #[case::full_block_want_partial(false, false, true, false)]
    #[case::partial_block_want_full(true, false, false, false)]
    #[case::partial_block_want_partial(true, false, true, true)]
    // Revert blocks: partial subscribers get all reverts, full subscribers only get full reverts
    #[case::revert_full_want_full(false, true, false, true)]
    #[case::revert_full_want_partial(false, true, true, true)]
    #[case::revert_partial_want_full(true, true, false, false)]
    #[case::revert_partial_want_partial(true, true, true, true)]
    #[actix_rt::test]
    async fn test_partial_blocks_filter(
        #[case] block_is_partial: bool,
        #[case] block_revert: bool,
        #[case] sub_partial_blocks: bool,
        #[case] should_receive: bool,
    ) {
        // Create extractor with unique name based on test parameters
        let extractor_name =
            format!("test_p{}_r{}_s{}", block_is_partial, block_revert, sub_partial_blocks);
        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, &extractor_name);

        let mut block = MyMessageSender::default_block(&extractor_name);
        block.partial_block_index = if block_is_partial { Some(0) } else { None };
        block.revert = block_revert;

        let message_sender = Arc::new(MyMessageSender::new(extractor_id.clone()).with_block(block));

        let mut subscribers_map = HashMap::new();
        subscribers_map
            .insert(extractor_id.clone(), message_sender as Arc<dyn MessageSender + Send + Sync>);

        let app_state = web::Data::new(WsData::new(subscribers_map));

        let server = start_with(
            TestServerConfig::default().client_request_timeout(Duration::from_secs(5)),
            move || {
                App::new()
                    .wrap(RequestTracing::new())
                    .app_data(app_state.clone())
                    .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
            },
        );

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);

        let (mut connection, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("Failed to connect");

        // Subscribe with the test's partial_blocks setting
        let action = Command::Subscribe {
            extractor_id: extractor_id.clone().into(),
            include_state: true,
            compression: false,
            partial_blocks: sub_partial_blocks,
        };
        connection
            .send(Message::Text(serde_json::to_string(&action).unwrap()))
            .await
            .expect("Failed to send subscribe message");

        // Wait for subscription confirmation
        wait_for_new_subscription(&mut connection)
            .await
            .expect("Failed to get subscription confirmation");

        // Check if message is received
        let mut received = false;
        loop {
            match timeout(Duration::from_millis(500), connection.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    // Check if it's a block changes message (not a response)
                    if serde_json::from_str::<DummyDelta>(&text).is_ok() {
                        received = true;
                        break;
                    } else {
                        panic!("Unexpected text message: {text}");
                    }
                }
                Ok(Some(Ok(_))) => continue, // Ping/pong, continue
                Ok(Some(Err(e))) => panic!("Connection error: {e}"),
                Ok(None) => panic!("Connection closed"),
                Err(_) => break, // Timeout - no message received
            }
        }

        assert_eq!(
            received, should_receive,
            "expected should_receive={}, got={}",
            should_receive, received
        );

        // Clean up
        connection
            .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
            .await
            .ok();
    }

    #[test_log::test(actix_rt::test)]
    async fn test_subscribe_error_response() {
        // Tests a special path where the extractor fails the subscription
        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "failing_extractor");
        let message_sender = Arc::new(FailingMessageSender::new(extractor_id.clone()));

        let mut subscribers_map = HashMap::new();
        subscribers_map
            .insert(extractor_id.clone(), message_sender as Arc<dyn MessageSender + Send + Sync>);

        let app_state = web::Data::new(WsData::new(subscribers_map));
        let server = start(move || {
            App::new()
                .wrap(RequestTracing::new())
                .app_data(app_state.clone())
                .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
        });

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);

        let (mut connection, _response) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        // Send subscribe request to failing extractor
        let action = Command::Subscribe {
            extractor_id: extractor_id.clone().into(),
            include_state: true,
            compression: false,
            partial_blocks: false,
        };
        connection
            .send(Message::Text(serde_json::to_string(&action).unwrap()))
            .await
            .expect("Failed to send subscribe message");

        // Wait for error response
        let response_msg = timeout(Duration::from_secs(1), connection.next())
            .await
            .expect("Failed to receive message")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = response_msg {
            let websocket_message: WebSocketMessage =
                serde_json::from_str(&text).expect("Failed to parse WebSocketMessage");

            if let WebSocketMessage::Response(Response::Error(error)) = websocket_message {
                match error {
                    tycho_common::dto::WebsocketError::SubscribeError(reported_id) => {
                        assert_eq!(reported_id, extractor_id.into());
                    }
                    _ => panic!("Expected SubscribeError, got: {:?}", error),
                }
            } else {
                panic!("Expected error response, got: {:?}", websocket_message);
            }
        } else {
            panic!("Expected text message, got: {:?}", response_msg);
        }

        connection
            .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
            .await
            .expect("Failed to send close message");
    }

    /// A sender that immediately closes the channel after subscribe, simulating an extractor
    /// that fails or restarts right after the subscription is established.
    pub struct ClosingMessageSender;

    #[async_trait]
    impl MessageSender for ClosingMessageSender {
        async fn subscribe(&self) -> Result<Receiver<ExtractorMsg>, SendError<ControlMessage>> {
            let (_tx, rx) = mpsc::channel::<ExtractorMsg>(1);
            // _tx is dropped here, closing the channel immediately.
            Ok(rx)
        }
    }

    /// Verifies that when an extractor's channel closes the client receives `SubscriptionEnded`
    /// for that subscription and the WebSocket connection stays open.
    #[actix_rt::test]
    async fn test_subscription_ended_on_channel_close() -> Result<(), String> {
        tracing_subscriber::fmt()
            .with_test_writer()
            .try_init()
            .unwrap_or_else(|_| debug!("Subscriber already initialized"));

        let closing_id = ExtractorIdentity::new(Chain::Ethereum, "closing_extractor");
        let live_id = ExtractorIdentity::new(Chain::Ethereum, "live_extractor");

        let mut subscribers_map = HashMap::new();
        subscribers_map.insert(
            closing_id.clone(),
            Arc::new(ClosingMessageSender) as Arc<dyn MessageSender + Send + Sync>,
        );
        subscribers_map.insert(
            live_id.clone(),
            Arc::new(MyMessageSender::new(live_id.clone())) as Arc<dyn MessageSender + Send + Sync>,
        );

        let app_state = web::Data::new(WsData::new(subscribers_map));
        let server = start_with(
            TestServerConfig::default().client_request_timeout(Duration::from_secs(5)),
            move || {
                App::new()
                    .wrap(RequestTracing::new())
                    .app_data(app_state.clone())
                    .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
            },
        );

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);
        let (mut connection, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        // Subscribe to the live extractor first so we can verify the connection survives.
        connection
            .send(Message::Text(
                serde_json::to_string(&Command::Subscribe {
                    extractor_id: live_id.clone().into(),
                    include_state: false,
                    compression: false,
                    partial_blocks: false,
                })
                .unwrap(),
            ))
            .await
            .expect("Failed to send subscribe");
        wait_for_new_subscription(&mut connection)
            .await
            .expect("Failed to get live subscription");

        // Subscribe to the closing extractor — its channel closes immediately.
        connection
            .send(Message::Text(
                serde_json::to_string(&Command::Subscribe {
                    extractor_id: closing_id.clone().into(),
                    include_state: false,
                    compression: false,
                    partial_blocks: false,
                })
                .unwrap(),
            ))
            .await
            .expect("Failed to send subscribe");
        wait_for_new_subscription(&mut connection)
            .await
            .expect("Failed to get closing subscription");

        // The channel was already closed, so we should receive SubscriptionEnded.
        wait_for_subscription_ended(&mut connection)
            .await
            .expect("Expected SubscriptionEnded when extractor channel closes");

        // The live subscription must still be delivering messages — the connection is intact.
        wait_for_dummy_message(&mut connection, live_id)
            .await
            .expect("Live extractor should still send messages after peer subscription ended");

        connection
            .send(Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })))
            .await
            .ok();

        Ok(())
    }

    /// Verifies that when the last subscription's channel closes, the WS connection itself
    /// is closed — an idle connection with no subscriptions wastes a client connection slot.
    #[actix_rt::test]
    async fn test_connection_closed_when_last_subscription_ends() -> Result<(), String> {
        tracing_subscriber::fmt()
            .with_test_writer()
            .try_init()
            .unwrap_or_else(|_| debug!("Subscriber already initialized"));

        let closing_id = ExtractorIdentity::new(Chain::Ethereum, "only_extractor");
        let mut subscribers_map = HashMap::new();
        subscribers_map.insert(
            closing_id.clone(),
            Arc::new(ClosingMessageSender) as Arc<dyn MessageSender + Send + Sync>,
        );

        let app_state = web::Data::new(WsData::new(subscribers_map));
        let server = start_with(
            TestServerConfig::default().client_request_timeout(Duration::from_secs(5)),
            move || {
                App::new()
                    .wrap(RequestTracing::new())
                    .app_data(app_state.clone())
                    .service(web::resource("/ws/").route(web::get().to(WsActor::ws_index)))
            },
        );

        let url = server
            .url("/ws/")
            .to_string()
            .replacen("http://", "ws://", 1);
        let (mut connection, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("Failed to connect");

        connection
            .send(Message::Text(
                serde_json::to_string(&Command::Subscribe {
                    extractor_id: closing_id.clone().into(),
                    include_state: false,
                    compression: false,
                    partial_blocks: false,
                })
                .unwrap(),
            ))
            .await
            .expect("Failed to send subscribe");
        wait_for_new_subscription(&mut connection)
            .await
            .expect("Failed to get subscription");

        // The channel closes immediately, so we get SubscriptionEnded.
        wait_for_subscription_ended(&mut connection)
            .await
            .expect("Expected SubscriptionEnded");

        // With no subscriptions left the actor should stop, closing the connection.
        let closed = timeout(Duration::from_secs(2), connection.next()).await;
        match closed {
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {}
            other => panic!("Expected connection close, got: {:?}", other),
        }

        Ok(())
    }
}
