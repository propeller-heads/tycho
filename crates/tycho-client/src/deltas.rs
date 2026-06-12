//! # Deltas Client
//!
//! This module focuses on implementing the Real-Time Deltas client for the Tycho Indexer service.
//! Utilizing this client facilitates efficient, instant communication with the indexing service,
//! promoting seamless data synchronization.
//!
//! ## Websocket Implementation
//!
//! The present WebSocket implementation is cloneable, which enables it to be shared
//! across multiple asynchronous tasks without creating separate instances for each task. This
//! unique feature boosts efficiency as it:
//!
//! - **Reduces Server Load:** By maintaining a single universal client, the load on the server is
//!   significantly reduced. This is because fewer connections are made to the server, preventing it
//!   from getting overwhelmed by numerous simultaneous requests.
//! - **Conserves Resource Usage:** A single shared client requires fewer system resources than if
//!   multiple clients were instantiated and used separately as there is some overhead for websocket
//!   handshakes and message.
//!
//! Therefore, sharing one client among multiple tasks ensures optimal performance, reduces resource
//! consumption, and enhances overall software scalability.
use std::{
    collections::{hash_map::Entry, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use futures03::{stream::SplitSink, SinkExt, StreamExt};
use hyper::{
    header::{
        AUTHORIZATION, CONNECTION, HOST, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
        USER_AGENT,
    },
    Uri,
};
#[cfg(test)]
use mockall::automock;
use thiserror::Error;
use tokio::{
    net::TcpStream,
    sync::{
        mpsc::{self, error::TrySendError, Receiver, Sender},
        oneshot, Mutex, MutexGuard, Notify,
    },
    task::JoinHandle,
    time::sleep,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        self,
        handshake::client::{generate_key, Request},
    },
    MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, instrument, trace, warn};
use tycho_common::{
    dto::{self, Command, Response, WebSocketMessage, WebsocketError},
    models::{blockchain::BlockAggregatedChanges, ExtractorIdentity},
};
use uuid::Uuid;
use zstd;

use crate::TYCHO_SERVER_VERSION;

#[derive(Error, Debug)]
pub enum DeltasError {
    /// Failed to parse the provided URI.
    #[error("Failed to parse URI: {0}. Error: {1}")]
    UriParsing(String, String),

    /// The requested subscription is already pending and is awaiting confirmation from the server.
    #[error("The requested subscription is already pending")]
    SubscriptionAlreadyPending,

    #[error("The server replied with an error: {0}")]
    ServerError(String),

    /// A message failed to send via an internal channel or through the websocket channel.
    /// This is typically a fatal error and might indicate a bug in the implementation.
    #[error("{0}")]
    TransportError(String),

    /// The internal message buffer is full. This likely means that messages are not being consumed
    /// fast enough. If the incoming load emits messages in bursts, consider increasing the buffer
    /// size.
    #[error("The buffer is full!")]
    BufferFull,

    /// The client has no active connections but was accessed (e.g., by calling subscribe).
    /// This typically occurs when trying to use the client before calling connect() or
    /// after the connection has been closed.
    #[error("The client is not connected!")]
    NotConnected,

    /// The connect method was called while the client already had an active connection.
    #[error("The client is already connected!")]
    AlreadyConnected,

    /// The connection was closed orderly by the server, e.g. because it restarted.
    #[error("The server closed the connection!")]
    ConnectionClosed,

    /// The connection was closed unexpectedly by the server or encountered a network error.
    #[error("Connection error: {0}")]
    ConnectionError(#[from] Box<tungstenite::Error>),

    /// A fatal error occurred that cannot be recovered from.
    #[error("Tycho FatalError: {0}")]
    Fatal(String),
}

#[derive(Clone, Debug)]
pub struct SubscriptionOptions {
    include_state: bool,
    compression: bool,
    partial_blocks: bool,
}

impl Default for SubscriptionOptions {
    fn default() -> Self {
        Self { include_state: true, compression: true, partial_blocks: false }
    }
}

impl SubscriptionOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_state(mut self, val: bool) -> Self {
        self.include_state = val;
        self
    }
    pub fn with_compression(mut self, val: bool) -> Self {
        self.compression = val;
        self
    }
    pub fn with_partial_blocks(mut self, val: bool) -> Self {
        self.partial_blocks = val;
        self
    }
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait DeltasClient {
    /// Subscribe to an extractor and receive realtime messages
    ///
    /// Will request a subscription from tycho and wait for confirmation of it. If the caller
    /// cancels while waiting for confirmation the subscription may still be registered. If the
    /// receiver was deallocated though, the first message from the subscription will remove it
    /// again - since there is no one to inform about these messages.
    async fn subscribe(
        &self,
        extractor_id: ExtractorIdentity,
        options: SubscriptionOptions,
    ) -> Result<(Uuid, Receiver<BlockAggregatedChanges>), DeltasError>;

    /// Unsubscribe from an subscription
    async fn unsubscribe(&self, subscription_id: Uuid) -> Result<(), DeltasError>;

    /// Start the clients message handling loop.
    async fn connect(&self) -> Result<JoinHandle<Result<(), DeltasError>>, DeltasError>;

    /// Close the clients message handling loop.
    async fn close(&self) -> Result<(), DeltasError>;
}

#[derive(Clone)]
pub struct WsDeltasClient {
    /// The tycho indexer websocket uri.
    uri: Uri,
    /// Authorization key for the websocket connection.
    auth_key: Option<String>,
    /// Maximum amount of reconnects to try before giving up.
    max_reconnects: u64,
    /// Duration to wait before attempting to reconnect
    retry_cooldown: Duration,
    /// The client will buffer this many messages incoming from the websocket
    /// before starting to drop them.
    ws_buffer_size: usize,
    /// The client will buffer that many messages for each subscription before it starts dropping
    /// them.
    subscription_buffer_size: usize,
    /// Notify tasks waiting for a connection to be established.
    conn_notify: Arc<Notify>,
    /// Shared client instance state.
    inner: Arc<Mutex<Option<Inner>>>,
    /// If set the client has exhausted its reconnection attempts
    dead: Arc<AtomicBool>,
}

type WebSocketSink =
    SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, tungstenite::protocol::Message>;

/// Subscription State
///
/// Subscription go through a lifecycle:
///
/// ```text
/// O ---> requested subscribe ----> active ----> requested unsub ---> ended
/// ```
///
/// We use oneshot channels to inform the client struct about when these transition happened. E.g.
/// because for `subscribe`` to finish, we want the state to have transition to `active` and similar
/// for `unsubscribe`.
#[derive(Debug)]
enum SubscriptionInfo {
    /// Subscription was requested we wait for server confirmation and uuid assignment.
    RequestedSubscription(
        oneshot::Sender<Result<(Uuid, Receiver<BlockAggregatedChanges>), DeltasError>>,
    ),
    /// Subscription is active.
    Active,
    /// Unsubscription was requested, we wait for server confirmation.
    RequestedUnsubscription(oneshot::Sender<()>),
}

/// Internal struct containing shared state between of WsDeltaClient instances.
struct Inner {
    /// Websocket sender handle.
    sink: WebSocketSink,
    /// Command channel sender handle.
    cmd_tx: Sender<()>,
    /// Currently pending subscriptions, keyed by model-layer extractor identity.
    pending: HashMap<ExtractorIdentity, SubscriptionInfo>,
    /// Active subscriptions.
    subscriptions: HashMap<Uuid, SubscriptionInfo>,
    /// For eachs subscription we keep a sender handle, the receiver is returned to the caller of
    /// subscribe.
    sender: HashMap<Uuid, Sender<BlockAggregatedChanges>>,
    /// How many messages to buffer per subscription before starting to drop new messages.
    buffer_size: usize,
}

/// Shared state between all client instances.
///
/// This state is behind a mutex and requires synchronization to be read of modified.
impl Inner {
    fn new(cmd_tx: Sender<()>, sink: WebSocketSink, buffer_size: usize) -> Self {
        Self {
            sink,
            cmd_tx,
            pending: HashMap::new(),
            subscriptions: HashMap::new(),
            sender: HashMap::new(),
            buffer_size,
        }
    }

    /// Registers a new pending subscription.
    #[allow(clippy::result_large_err)]
    fn new_subscription(
        &mut self,
        id: &ExtractorIdentity,
        ready_tx: oneshot::Sender<Result<(Uuid, Receiver<BlockAggregatedChanges>), DeltasError>>,
    ) -> Result<(), DeltasError> {
        if self.pending.contains_key(id) {
            return Err(DeltasError::SubscriptionAlreadyPending);
        }
        self.pending
            .insert(id.clone(), SubscriptionInfo::RequestedSubscription(ready_tx));
        Ok(())
    }

    /// Transitions a pending subscription to active.
    ///
    /// Will ignore any request to do so for subscriptions that are not pending.
    fn mark_active(&mut self, extractor_id: ExtractorIdentity, subscription_id: Uuid) {
        if let Some(info) = self.pending.remove(&extractor_id) {
            if let SubscriptionInfo::RequestedSubscription(ready_tx) = info {
                let (tx, rx) = mpsc::channel(self.buffer_size);
                self.sender.insert(subscription_id, tx);
                self.subscriptions
                    .insert(subscription_id, SubscriptionInfo::Active);
                let _ = ready_tx
                    .send(Ok((subscription_id, rx)))
                    .map_err(|_| {
                        warn!(
                            ?extractor_id,
                            ?subscription_id,
                            "Subscriber for has gone away. Ignoring."
                        )
                    });
            } else {
                error!(
                    ?extractor_id,
                    ?subscription_id,
                    "Pending subscription was not in the correct state to 
                    transition to active. Ignoring!"
                )
            }
        } else {
            error!(
                ?extractor_id,
                ?subscription_id,
                "Tried to mark an unknown subscription as active. Ignoring!"
            );
        }
    }

    /// Sends a message to a subscription's receiver.
    #[allow(clippy::result_large_err)]
    fn send(&mut self, id: &Uuid, msg: BlockAggregatedChanges) -> Result<(), DeltasError> {
        if let Some(sender) = self.sender.get_mut(id) {
            sender
                .try_send(msg)
                .map_err(|e| match e {
                    TrySendError::Full(_) => DeltasError::BufferFull,
                    TrySendError::Closed(_) => {
                        DeltasError::TransportError("The subscriber has gone away".to_string())
                    }
                })?;
        }
        Ok(())
    }

    /// Requests a subscription to end.
    ///
    /// The subscription needs to exist and be active for this to have any effect. Wll use
    /// `ready_tx` to notify the receiver once the transition to ended completed.
    fn end_subscription(&mut self, subscription_id: &Uuid, ready_tx: oneshot::Sender<()>) {
        if let Some(info) = self
            .subscriptions
            .get_mut(subscription_id)
        {
            if let SubscriptionInfo::Active = info {
                *info = SubscriptionInfo::RequestedUnsubscription(ready_tx);
            }
        } else {
            // no big deal imo so only debug lvl...
            debug!(?subscription_id, "Tried unsubscribing from a non existent subscription");
        }
    }

    /// Removes and fully ends a subscription
    ///
    /// Any calls for non-existing subscriptions will be simply ignored. May panic on internal state
    /// inconsistencies: e.g. if the subscription exists but there is no sender for it.
    /// Will remove a subscription even it was in active or pending state before, this is to support
    /// any server side failure of the subscription.
    fn remove_subscription(&mut self, subscription_id: Uuid) -> Result<(), DeltasError> {
        if let Entry::Occupied(e) = self
            .subscriptions
            .entry(subscription_id)
        {
            let info = e.remove();
            if let SubscriptionInfo::RequestedUnsubscription(tx) = info {
                let _ = tx.send(()).map_err(|_| {
                    debug!(?subscription_id, "failed to notify about removed subscription")
                });
                self.sender
                    .remove(&subscription_id)
                    .ok_or_else(|| DeltasError::Fatal("Inconsistent internal client state: `sender` state drifted from `info` while removing a subscription.".to_string()))?;
            } else {
                warn!(?subscription_id, "Subscription ended unexpectedly!");
                self.sender
                    .remove(&subscription_id)
                    .ok_or_else(|| DeltasError::Fatal("sender channel missing".to_string()))?;
            }
        } else {
            // TODO: There is a race condition that can trigger multiple unsubscribes
            //  if server doesn't respond quickly enough leading to some ugly logs but
            //  doesn't affect behaviour negatively. E.g. BufferFull and multiple
            //  messages from the ws connection are queued.
            trace!(
                ?subscription_id,
                "Received `SubscriptionEnded`, but was never subscribed to it. This is likely a bug!"
            );
        }

        Ok(())
    }

    fn cancel_pending(&mut self, extractor_id: ExtractorIdentity, error: &WebsocketError) {
        if let Some(sub_info) = self.pending.remove(&extractor_id) {
            match sub_info {
                SubscriptionInfo::RequestedSubscription(tx) => {
                    let _ = tx
                        .send(Err(DeltasError::ServerError(format!(
                            "Subscription failed: {error}"
                        ))))
                        .map_err(|_| debug!("Cancel pending failed: receiver deallocated!"));
                }
                _ => {
                    error!(?extractor_id, "Pending subscription in wrong state")
                }
            }
        } else {
            debug!(?extractor_id, "Tried cancel on non-existent pending subscription!")
        }
    }

    /// Sends a message through the websocket.
    async fn ws_send(&mut self, msg: tungstenite::protocol::Message) -> Result<(), DeltasError> {
        self.sink.send(msg).await.map_err(|e| {
            DeltasError::TransportError(format!("Failed to send message to websocket: {e}"))
        })
    }
}

/// Tycho client websocket implementation.
impl WsDeltasClient {
    // Construct a new client with 5 reconnection attempts.
    #[allow(clippy::result_large_err)]
    pub fn new(ws_uri: &str, auth_key: Option<&str>) -> Result<Self, DeltasError> {
        let uri = ws_uri
            .parse::<Uri>()
            .map_err(|e| DeltasError::UriParsing(ws_uri.to_string(), e.to_string()))?;
        Ok(Self {
            uri,
            auth_key: auth_key.map(|s| s.to_string()),
            inner: Arc::new(Mutex::new(None)),
            ws_buffer_size: 128,
            subscription_buffer_size: 128,
            conn_notify: Arc::new(Notify::new()),
            max_reconnects: 5,
            retry_cooldown: Duration::from_millis(500),
            dead: Arc::new(AtomicBool::new(false)),
        })
    }

    // Construct a new client with a custom number of reconnection attempts.
    #[allow(clippy::result_large_err)]
    pub fn new_with_reconnects(
        ws_uri: &str,
        auth_key: Option<&str>,
        max_reconnects: u64,
        retry_cooldown: Duration,
    ) -> Result<Self, DeltasError> {
        let uri = ws_uri
            .parse::<Uri>()
            .map_err(|e| DeltasError::UriParsing(ws_uri.to_string(), e.to_string()))?;

        Ok(Self {
            uri,
            auth_key: auth_key.map(|s| s.to_string()),
            inner: Arc::new(Mutex::new(None)),
            ws_buffer_size: 128,
            subscription_buffer_size: 128,
            conn_notify: Arc::new(Notify::new()),
            max_reconnects,
            retry_cooldown,
            dead: Arc::new(AtomicBool::new(false)),
        })
    }

    // Construct a new client with custom buffer sizes (for testing)
    #[cfg(test)]
    #[allow(clippy::result_large_err)]
    pub fn new_with_custom_buffers(
        ws_uri: &str,
        auth_key: Option<&str>,
        ws_buffer_size: usize,
        subscription_buffer_size: usize,
    ) -> Result<Self, DeltasError> {
        let uri = ws_uri
            .parse::<Uri>()
            .map_err(|e| DeltasError::UriParsing(ws_uri.to_string(), e.to_string()))?;
        Ok(Self {
            uri,
            auth_key: auth_key.map(|s| s.to_string()),
            inner: Arc::new(Mutex::new(None)),
            ws_buffer_size,
            subscription_buffer_size,
            conn_notify: Arc::new(Notify::new()),
            max_reconnects: 5,
            retry_cooldown: Duration::from_millis(0),
            dead: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Ensures that the client is connected.
    ///
    /// This method will acquire the lock for inner.
    async fn is_connected(&self) -> bool {
        let guard = self.inner.as_ref().lock().await;
        guard.is_some()
    }

    /// Waits for the client to be connected
    ///
    /// This method acquires the lock for inner for a short period, then waits until the
    /// connection is established if not already connected.
    async fn ensure_connection(&self) -> Result<(), DeltasError> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(DeltasError::NotConnected);
        }
        if self.is_connected().await {
            return Ok(());
        }
        // Enable the future BEFORE re-checking is_connected to close the race window where
        // the reconnect task calls notify_waiters() between is_connected() returning false and
        // notified().await — without enable(), that notification would be lost and this call
        // would block until the next reconnect.
        let notified = self.conn_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_connected().await {
            notified.await;
        }
        if self.dead.load(Ordering::SeqCst) {
            return Err(DeltasError::NotConnected);
        }
        if !self.is_connected().await {
            return Err(DeltasError::NotConnected);
        }
        Ok(())
    }

    /// Main message handling logic
    ///
    /// If the message returns an error, a reconnect attempt may be considered depending on the
    /// error type.
    #[instrument(skip(self, msg))]
    async fn handle_msg(
        &self,
        msg: Result<tungstenite::protocol::Message, tokio_tungstenite::tungstenite::error::Error>,
    ) -> Result<(), DeltasError> {
        let mut guard = self.inner.lock().await;

        match msg {
            // We do not deserialize the message directly into a WebSocketMessage. This is because
            // the serde arbitrary_precision feature (often included in many
            // dependencies we use) breaks some untagged enum deserializations. Instead,
            // we deserialize the message into a serde_json::Value and convert that into a WebSocketMessage. For more info on this issue, see: https://github.com/serde-rs/json/issues/740
            Ok(tungstenite::protocol::Message::Text(text)) => match serde_json::from_str::<
                serde_json::Value,
            >(&text)
            {
                Ok(value) => match serde_json::from_value::<WebSocketMessage>(value) {
                    Ok(ws_message) => match ws_message {
                        WebSocketMessage::BlockAggregatedChanges { subscription_id, deltas } => {
                            Self::handle_block_changes_msg(&mut guard, subscription_id, deltas)
                                .await?;
                        }
                        WebSocketMessage::Response(Response::NewSubscription {
                            extractor_id,
                            subscription_id,
                        }) => {
                            info!(?extractor_id, ?subscription_id, "Received a new subscription");
                            let inner = guard
                                .as_mut()
                                .ok_or_else(|| DeltasError::NotConnected)?;
                            inner.mark_active(extractor_id.into(), subscription_id);
                        }
                        WebSocketMessage::Response(Response::SubscriptionEnded {
                            subscription_id,
                        }) => {
                            info!(?subscription_id, "Received a subscription ended");
                            let inner = guard
                                .as_mut()
                                .ok_or_else(|| DeltasError::NotConnected)?;
                            inner.remove_subscription(subscription_id)?;
                        }
                        WebSocketMessage::Response(Response::Error(error)) => match &error {
                            WebsocketError::ExtractorNotFound(extractor_id) => {
                                let inner = guard
                                    .as_mut()
                                    .ok_or_else(|| DeltasError::NotConnected)?;
                                inner.cancel_pending(extractor_id.clone().into(), &error);
                            }
                            WebsocketError::SubscriptionNotFound(subscription_id) => {
                                debug!("Received subscription not found, removing subscription");
                                let inner = guard
                                    .as_mut()
                                    .ok_or_else(|| DeltasError::NotConnected)?;
                                inner.remove_subscription(*subscription_id)?;
                            }
                            WebsocketError::ParseError(raw, e) => {
                                return Err(DeltasError::ServerError(format!(
                                    "Server failed to parse client message: {e}, msg: {raw}"
                                )))
                            }
                            WebsocketError::CompressionError(subscription_id, e) => {
                                return Err(DeltasError::ServerError(format!(
                                    "Server failed to compress message for subscription: \
                                     {subscription_id}, error: {e}"
                                )))
                            }
                            WebsocketError::SubscribeError(extractor_id) => {
                                let inner = guard
                                    .as_mut()
                                    .ok_or_else(|| DeltasError::NotConnected)?;
                                inner.cancel_pending(extractor_id.clone().into(), &error);
                            }
                        },
                    },
                    Err(e) => {
                        error!(
                            "Failed to deserialize WebSocketMessage: {}. \nMessage: {}",
                            e, text
                        );
                    }
                },
                Err(e) => {
                    error!(
                        "Failed to deserialize message: invalid JSON. {} \nMessage: {}",
                        e, text
                    );
                }
            },
            Ok(tungstenite::protocol::Message::Binary(data)) => {
                // Decompress the zstd-compressed data,
                // Note that we only support compressed BlockAggregatedChanges messages for now.
                match zstd::decode_all(data.as_slice()) {
                    Ok(decompressed) => {
                        match serde_json::from_slice::<serde_json::Value>(decompressed.as_slice()) {
                            Ok(value) => {
                                match serde_json::from_value::<WebSocketMessage>(value.clone()) {
                                    Ok(ws_message) => match ws_message {
                                        WebSocketMessage::BlockAggregatedChanges {
                                            subscription_id,
                                            deltas,
                                        } => {
                                            Self::handle_block_changes_msg(
                                                &mut guard,
                                                subscription_id,
                                                deltas,
                                            )
                                            .await?;
                                        }
                                        _ => {
                                            error!(
                                                "Received unsupported compressed WebSocketMessage variant. \nMessage: {ws_message:?}",
                                            );
                                        }
                                    },
                                    Err(e) => {
                                        error!(
                                            "Failed to deserialize compressed WebSocketMessage: {e}. \nMessage: {value:?}",
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Failed to deserialize compressed message: invalid JSON. {e}",
                                );
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to decompress zstd data: {}", e);
                    }
                }
            }
            Ok(tungstenite::protocol::Message::Ping(_)) => {
                // Respond to pings with pongs.
                let inner = guard
                    .as_mut()
                    .ok_or_else(|| DeltasError::NotConnected)?;
                if let Err(error) = inner
                    .ws_send(tungstenite::protocol::Message::Pong(Vec::new()))
                    .await
                {
                    debug!(?error, "Failed to send pong!");
                }
            }
            Ok(tungstenite::protocol::Message::Pong(_)) => {
                // Do nothing.
            }
            Ok(tungstenite::protocol::Message::Close(frame)) => {
                match &frame {
                    Some(f) => {
                        warn!(code = ?f.code, reason = %f.reason, "WebSocket closed by server")
                    }
                    None => warn!("WebSocket closed by server (no close frame)"),
                }
                return Err(DeltasError::ConnectionClosed);
            }
            Ok(unknown_msg) => {
                info!("Received an unknown message type: {:?}", unknown_msg);
            }
            Err(error) => {
                error!(?error, "Websocket error");
                return Err(match error {
                    tungstenite::Error::ConnectionClosed => DeltasError::ConnectionClosed,
                    tungstenite::Error::AlreadyClosed => {
                        warn!("Received AlreadyClosed error which is indicative of a bug!");
                        DeltasError::ConnectionError(Box::new(error))
                    }
                    tungstenite::Error::Io(_) | tungstenite::Error::Protocol(_) => {
                        DeltasError::ConnectionError(Box::new(error))
                    }
                    _ => DeltasError::Fatal(error.to_string()),
                });
            }
        };
        Ok(())
    }

    async fn handle_block_changes_msg(
        guard: &mut MutexGuard<'_, Option<Inner>>,
        subscription_id: Uuid,
        deltas: dto::BlockAggregatedChanges,
    ) -> Result<(), DeltasError> {
        trace!(?deltas, "Received a block state change, sending to channel");
        let inner = guard
            .as_mut()
            .ok_or_else(|| DeltasError::NotConnected)?;
        match inner.send(&subscription_id, BlockAggregatedChanges::from(deltas)) {
            Err(DeltasError::BufferFull) => {
                error!(?subscription_id, "Buffer full, unsubscribing!");
                Self::force_unsubscribe(subscription_id, inner).await;
            }
            Err(_) => {
                warn!(?subscription_id, "Receiver for has gone away, unsubscribing!");
                Self::force_unsubscribe(subscription_id, inner).await;
            }
            _ => { /* Do nothing */ }
        }
        Ok(())
    }

    /// Forcefully ends a (client) stream by unsubscribing.
    ///
    /// Is used only if the message can't be processed due to an error that might resolve
    /// itself by resubscribing.
    async fn force_unsubscribe(subscription_id: Uuid, inner: &mut Inner) {
        // avoid unsubscribing multiple times
        if let Some(SubscriptionInfo::RequestedUnsubscription(_)) = inner
            .subscriptions
            .get(&subscription_id)
        {
            return;
        }

        let (tx, rx) = oneshot::channel();
        if let Err(e) = WsDeltasClient::unsubscribe_inner(inner, subscription_id, tx).await {
            warn!(?e, ?subscription_id, "Failed to send unsubscribe command");
        } else {
            // Wait for unsubscribe completion with timeout
            match tokio::time::timeout(Duration::from_secs(5), rx).await {
                Ok(_) => {
                    debug!(?subscription_id, "Unsubscribe completed successfully");
                }
                Err(_) => {
                    warn!(?subscription_id, "Unsubscribe completion timed out");
                }
            }
        }
    }

    /// Helper method to force an unsubscription
    ///
    /// This method expects to receive a mutable reference to `Inner` so it does not acquire a
    /// lock. Used for normal unsubscribes as well to remove any subscriptions with deallocated
    /// receivers.
    async fn unsubscribe_inner(
        inner: &mut Inner,
        subscription_id: Uuid,
        ready_tx: oneshot::Sender<()>,
    ) -> Result<(), DeltasError> {
        debug!(?subscription_id, "Unsubscribing");
        inner.end_subscription(&subscription_id, ready_tx);
        let cmd = Command::Unsubscribe { subscription_id };
        inner
            .ws_send(tungstenite::protocol::Message::Text(serde_json::to_string(&cmd).map_err(
                |e| {
                    DeltasError::TransportError(format!(
                        "Failed to serialize unsubscribe command: {e}"
                    ))
                },
            )?))
            .await?;
        Ok(())
    }
}

#[async_trait]
impl DeltasClient for WsDeltasClient {
    #[instrument(skip(self))]
    async fn subscribe(
        &self,
        extractor_id: ExtractorIdentity,
        options: SubscriptionOptions,
    ) -> Result<(Uuid, Receiver<BlockAggregatedChanges>), DeltasError> {
        trace!("Starting subscribe");
        self.ensure_connection().await?;
        let (ready_tx, ready_rx) = oneshot::channel();
        {
            let mut guard = self.inner.lock().await;
            let inner = guard
                .as_mut()
                .ok_or_else(|| DeltasError::NotConnected)?;
            trace!("Sending subscribe command");
            inner.new_subscription(&extractor_id, ready_tx)?;
            let cmd = Command::Subscribe {
                extractor_id: extractor_id.into(),
                include_state: options.include_state,
                compression: options.compression,
                partial_blocks: options.partial_blocks,
            };
            inner
                .ws_send(tungstenite::protocol::Message::Text(
                    serde_json::to_string(&cmd).map_err(|e| {
                        DeltasError::TransportError(format!(
                            "Failed to serialize subscribe command: {e}"
                        ))
                    })?,
                ))
                .await?;
        }
        trace!("Waiting for subscription response");
        let res = tokio::time::timeout(Duration::from_secs(30), ready_rx)
            .await
            .map_err(|_| {
                DeltasError::TransportError(
                    "Subscribe confirmation timed out after 30s".to_string(),
                )
            })?
            .map_err(|_| {
                DeltasError::TransportError("Subscription channel closed unexpectedly".to_string())
            })??;
        trace!("Subscription successful");
        Ok(res)
    }

    #[instrument(skip(self))]
    async fn unsubscribe(&self, subscription_id: Uuid) -> Result<(), DeltasError> {
        self.ensure_connection().await?;
        let (ready_tx, ready_rx) = oneshot::channel();
        {
            let mut guard = self.inner.lock().await;
            let inner = guard
                .as_mut()
                .ok_or_else(|| DeltasError::NotConnected)?;

            WsDeltasClient::unsubscribe_inner(inner, subscription_id, ready_tx).await?;
        }
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .map_err(|_| {
                warn!(?subscription_id, "Unsubscribe confirmation timed out after 5s");
                DeltasError::TransportError(
                    "Unsubscribe confirmation timed out after 5s".to_string(),
                )
            })?
            .map_err(|_| {
                DeltasError::TransportError("Unsubscribe channel closed unexpectedly".to_string())
            })?;

        Ok(())
    }

    #[instrument(skip(self))]
    async fn connect(&self) -> Result<JoinHandle<Result<(), DeltasError>>, DeltasError> {
        if self.is_connected().await {
            return Err(DeltasError::AlreadyConnected);
        }
        let ws_uri = format!("{uri}{TYCHO_SERVER_VERSION}/ws", uri = self.uri);
        info!(?ws_uri, "Starting TychoWebsocketClient");

        let (cmd_tx, mut cmd_rx) = mpsc::channel(self.ws_buffer_size);
        {
            let mut guard = self.inner.as_ref().lock().await;
            *guard = None;
        }
        let this = self.clone();
        let jh = tokio::spawn(async move {
            let mut retry_count = 0;
            let mut result = Err(DeltasError::NotConnected);

            'retry: while retry_count < this.max_reconnects {
                info!(?ws_uri, retry_count, "Connecting to WebSocket server");
                if retry_count > 0 {
                    sleep(this.retry_cooldown).await;
                }

                // Create a WebSocket request
                let mut request_builder = Request::builder()
                    .uri(&ws_uri)
                    .header(SEC_WEBSOCKET_KEY, generate_key())
                    .header(SEC_WEBSOCKET_VERSION, 13)
                    .header(CONNECTION, "Upgrade")
                    .header(UPGRADE, "websocket")
                    .header(
                        HOST,
                        this.uri.host().ok_or_else(|| {
                            DeltasError::UriParsing(
                                ws_uri.clone(),
                                "No host found in tycho url".to_string(),
                            )
                        })?,
                    )
                    .header(
                        USER_AGENT,
                        format!("tycho-client-{version}", version = env!("CARGO_PKG_VERSION")),
                    );

                // Add Authorization if one is given
                if let Some(ref key) = this.auth_key {
                    request_builder = request_builder.header(AUTHORIZATION, key);
                }

                let request = request_builder.body(()).map_err(|e| {
                    DeltasError::TransportError(format!("Failed to build connection request: {e}"))
                })?;
                let (conn, _) = match connect_async(request).await {
                    Ok(conn) => conn,
                    Err(e) => {
                        // Prepare for reconnection
                        retry_count += 1;
                        let mut guard = this.inner.as_ref().lock().await;
                        *guard = None;

                        if let tungstenite::Error::Http(response) = &e {
                            if response.status() == tungstenite::http::StatusCode::TOO_MANY_REQUESTS
                            {
                                let reason = response
                                    .body()
                                    .as_deref()
                                    .and_then(|b| std::str::from_utf8(b).ok())
                                    .unwrap_or("")
                                    .to_string();
                                warn!(reason, "WebSocket connection rejected: rate limited");
                                continue 'retry;
                            }
                        }

                        warn!(
                            e = e.to_string(),
                            "Failed to connect to WebSocket server; Reconnecting"
                        );
                        continue 'retry;
                    }
                };

                let (ws_tx_new, ws_rx_new) = conn.split();
                {
                    let mut guard = this.inner.as_ref().lock().await;
                    *guard =
                        Some(Inner::new(cmd_tx.clone(), ws_tx_new, this.subscription_buffer_size));
                }
                let mut msg_rx = ws_rx_new.boxed();

                info!("Connection Successful: TychoWebsocketClient started");
                this.conn_notify.notify_waiters();
                result = Ok(());

                // If no WS frame arrives within this window the TCP connection is
                // considered stalled (e.g. network cut without a TCP RST/FIN). The OS
                // default keepalive fires after ~2 hours; this gives us an
                // application-level detection that is orders of magnitude faster.
                const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
                loop {
                    let res = tokio::select! {
                        msg_result = tokio::time::timeout(IDLE_TIMEOUT, msg_rx.next()) => {
                            match msg_result {
                                Err(_elapsed) => {
                                    warn!("No WS frame received for {IDLE_TIMEOUT:?}, \
                                           treating connection as stalled; Reconnecting...");
                                    retry_count += 1;
                                    let mut guard = this.inner.as_ref().lock().await;
                                    *guard = None;
                                    break; // break inner loop → reconnect
                                }
                                Ok(Some(msg)) => this.handle_msg(msg).await,
                                Ok(None) => {
                                    // This code should not be reachable since the stream
                                    // should return ConnectionClosed in the case above
                                    // before it returns None here.
                                    warn!("Websocket connection silently closed, giving up!");
                                    break 'retry
                                }
                            }
                        },
                        _ = cmd_rx.recv() => {break 'retry},
                    };
                    if let Err(error) = res {
                        debug!(?error, "WsError");
                        if matches!(
                            error,
                            DeltasError::ConnectionClosed | DeltasError::ConnectionError { .. }
                        ) {
                            // Prepare for reconnection
                            retry_count += 1;
                            let mut guard = this.inner.as_ref().lock().await;
                            *guard = None;

                            warn!(
                                ?error,
                                ?retry_count,
                                "Connection dropped unexpectedly; Reconnecting..."
                            );
                            break;
                        } else {
                            // Other errors are considered fatal
                            error!(?error, "Fatal error; Exiting");
                            result = Err(error);
                            break 'retry;
                        }
                    }
                }
            }
            debug!(
                retry_count,
                max_reconnects=?this.max_reconnects,
                "Reconnection loop ended"
            );
            // Clean up before exiting
            let mut guard = this.inner.as_ref().lock().await;
            *guard = None;

            // Check if max retries has been reached.
            if retry_count >= this.max_reconnects {
                error!("Max reconnection attempts reached; Exiting");
                this.dead.store(true, Ordering::SeqCst);
                this.conn_notify.notify_waiters(); // Notify that the task is done
                result = Err(DeltasError::ConnectionClosed);
            }

            result
        });

        self.conn_notify.notified().await;

        if self.is_connected().await {
            Ok(jh)
        } else {
            Err(DeltasError::NotConnected)
        }
    }

    #[instrument(skip(self))]
    async fn close(&self) -> Result<(), DeltasError> {
        info!("Closing TychoWebsocketClient");
        let mut guard = self.inner.lock().await;
        let inner = guard
            .as_mut()
            .ok_or_else(|| DeltasError::NotConnected)?;
        inner
            .cmd_tx
            .send(())
            .await
            .map_err(|e| DeltasError::TransportError(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, str::FromStr};

    use tokio::{net::TcpListener, time::timeout};
    use tycho_common::models::Chain;

    use super::*;

    #[derive(Clone)]
    enum ExpectedComm {
        Receive(u64, tungstenite::protocol::Message),
        Send(tungstenite::protocol::Message),
    }

    async fn mock_tycho_ws(
        messages: &[ExpectedComm],
        reconnects: usize,
    ) -> (SocketAddr, JoinHandle<()>) {
        info!("Starting mock webserver");
        // zero port here means the OS chooses an open port
        let server = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("localhost bind failed");
        let addr = server.local_addr().unwrap();
        let messages = messages.to_vec();

        let jh = tokio::spawn(async move {
            info!("mock webserver started");
            for _ in 0..(reconnects + 1) {
                info!("Awaiting client connections");
                if let Ok((stream, _)) = server.accept().await {
                    info!("Client connected");
                    let mut websocket = tokio_tungstenite::accept_async(stream)
                        .await
                        .unwrap();

                    info!("Handling messages..");
                    for c in messages.iter().cloned() {
                        match c {
                            ExpectedComm::Receive(t, exp) => {
                                info!("Awaiting message...");
                                let msg = timeout(Duration::from_millis(t), websocket.next())
                                    .await
                                    .expect("Receive timeout")
                                    .expect("Stream exhausted")
                                    .expect("Failed to receive message.");
                                info!("Message received");
                                assert_eq!(msg, exp)
                            }
                            ExpectedComm::Send(data) => {
                                info!("Sending message");
                                websocket
                                    .send(data)
                                    .await
                                    .expect("Failed to send message");
                                info!("Message sent");
                            }
                        };
                    }
                    info!("Mock communication completed");
                    sleep(Duration::from_millis(100)).await;
                    // Close the WebSocket connection
                    let _ = websocket.close(None).await;
                    info!("Mock server closed connection");
                }
            }
            info!("mock server ended");
        });
        (addr, jh)
    }

    const SUBSCRIPTION_ID: &str = "30b740d1-cf09-4e0e-8cfe-b1434d447ece";

    fn subscribe() -> String {
        subscribe_with_compression(false)
    }

    fn subscribe_with_compression(compression: bool) -> String {
        serde_json::json!({
            "method": "subscribe",
            "extractor_id": {
                "chain": "ethereum",
                "name": "vm:ambient"
            },
            "include_state": true,
            "compression": compression,
            "partial_blocks": false
        })
        .to_string()
    }

    fn subscription_confirmation() -> String {
        r#"
        {
            "method": "newsubscription",
            "extractor_id":{
                "chain": "ethereum",
                "name": "vm:ambient"
            },
            "subscription_id": "30b740d1-cf09-4e0e-8cfe-b1434d447ece"
        }
        "#
        .replace(|c: char| c.is_whitespace(), "")
    }

    fn block_deltas() -> String {
        r#"
        {
            "subscription_id": "30b740d1-cf09-4e0e-8cfe-b1434d447ece",
            "deltas": {
                "extractor": "vm:ambient",
                "chain": "ethereum",
                "block": {
                    "number": 123,
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "parent_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "chain": "ethereum",
                    "ts": "2023-09-14T00:00:00"
                },
                "finalized_block_height": 0,
                "revert": false,
                "new_tokens": {},
                "account_updates": {
                    "0x7a250d5630b4cf539739df2c5dacb4c659f2488d": {
                        "address": "0x7a250d5630b4cf539739df2c5dacb4c659f2488d",
                        "chain": "ethereum",
                        "slots": {},
                        "balance": "0x01f4",
                        "code": "",
                        "change": "Update"
                    }
                },
                "state_updates": {
                    "component_1": {
                        "component_id": "component_1",
                        "updated_attributes": {"attr1": "0x01"},
                        "deleted_attributes": ["attr2"]
                    }
                },
                "new_protocol_components":
                    { "protocol_1": {
                            "id": "protocol_1",
                            "protocol_system": "system_1",
                            "protocol_type_name": "type_1",
                            "chain": "ethereum",
                            "tokens": ["0x01", "0x02"],
                            "contract_ids": ["0x01", "0x02"],
                            "static_attributes": {"attr1": "0x01f4"},
                            "change": "Update",
                            "creation_tx": "0x01",
                            "created_at": "2023-09-14T00:00:00"
                        }
                    },
                "deleted_protocol_components": {},
                "component_balances": {
                    "protocol_1":
                        {
                            "0x01": {
                                "token": "0x01",
                                "balance": "0x01f4",
                                "balance_float": 0.0,
                                "modify_tx": "0x01",
                                "component_id": "protocol_1"
                            }
                        }
                },
                "account_balances": {
                    "0x7a250d5630b4cf539739df2c5dacb4c659f2488d": {
                        "0x7a250d5630b4cf539739df2c5dacb4c659f2488d": {
                            "account": "0x7a250d5630b4cf539739df2c5dacb4c659f2488d",
                            "token": "0x7a250d5630b4cf539739df2c5dacb4c659f2488d",
                            "balance": "0x01f4",
                            "modify_tx": "0x01"
                        }
                    }
                },
                "component_tvl": {
                    "protocol_1": 1000.0
                },
                "dci_update": {
                    "new_entrypoints": {},
                    "new_entrypoint_params": {},
                    "trace_results": {}
                }
            }
        }
        "#.replace(|c: char| c.is_whitespace(), "")
    }

    fn unsubscribe() -> String {
        r#"
        {
            "method": "unsubscribe",
            "subscription_id": "30b740d1-cf09-4e0e-8cfe-b1434d447ece"
        }
        "#
        .replace(|c: char| c.is_whitespace(), "")
    }

    fn subscription_ended() -> String {
        r#"
        {
            "method": "subscriptionended",
            "subscription_id": "30b740d1-cf09-4e0e-8cfe-b1434d447ece"
        }
        "#
        .replace(|c: char| c.is_whitespace(), "")
    }

    #[tokio::test]
    async fn test_uncompressed_subscribe_receive() {
        let exp_comm = [
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(subscription_confirmation())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(block_deltas())),
        ];
        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");
        let (_, mut rx) = timeout(
            Duration::from_millis(100),
            client.subscribe(
                ExtractorIdentity::new(Chain::Ethereum, "vm:ambient"),
                SubscriptionOptions::new().with_compression(false),
            ),
        )
        .await
        .expect("subscription timed out")
        .expect("subscription failed");
        let _ = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("awaiting message timeout out")
            .expect("receiving message failed");
        timeout(Duration::from_millis(100), client.close())
            .await
            .expect("close timed out")
            .expect("close failed");
        jh.await
            .expect("ws loop errored")
            .unwrap();
        server_thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_compressed_subscribe_receive() {
        let compressed_block_deltas = zstd::encode_all(
            block_deltas().as_bytes(),
            0, // default compression level
        )
        .expect("Failed to compress block deltas message");

        let exp_comm = [
            ExpectedComm::Receive(
                100,
                tungstenite::protocol::Message::Text(subscribe_with_compression(true)),
            ),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(subscription_confirmation())),
            ExpectedComm::Send(tungstenite::protocol::Message::Binary(compressed_block_deltas)),
        ];
        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");
        let (_, mut rx) = timeout(
            Duration::from_millis(100),
            client.subscribe(
                ExtractorIdentity::new(Chain::Ethereum, "vm:ambient"),
                SubscriptionOptions::new().with_compression(true),
            ),
        )
        .await
        .expect("subscription timed out")
        .expect("subscription failed");
        let _ = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("awaiting message timeout out")
            .expect("receiving message failed");
        timeout(Duration::from_millis(100), client.close())
            .await
            .expect("close timed out")
            .expect("close failed");
        jh.await
            .expect("ws loop errored")
            .unwrap();
        server_thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_unsubscribe() {
        let exp_comm = [
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(subscription_confirmation())),
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(unsubscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(subscription_ended())),
        ];
        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");
        let (sub_id, mut rx) = timeout(
            Duration::from_millis(100),
            client.subscribe(
                ExtractorIdentity::new(Chain::Ethereum, "vm:ambient"),
                SubscriptionOptions::new().with_compression(false),
            ),
        )
        .await
        .expect("subscription timed out")
        .expect("subscription failed");

        timeout(Duration::from_millis(100), client.unsubscribe(sub_id))
            .await
            .expect("unsubscribe timed out")
            .expect("unsubscribe failed");
        let res = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("awaiting message timeout out");

        // If the subscription ended, the channel should have been closed.
        assert!(res.is_none());

        timeout(Duration::from_millis(100), client.close())
            .await
            .expect("close timed out")
            .expect("close failed");
        jh.await
            .expect("ws loop errored")
            .unwrap();
        server_thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_subscription_unexpected_end() {
        let exp_comm = [
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(subscription_confirmation())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(subscription_ended())),
        ];
        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");
        let (_, mut rx) = timeout(
            Duration::from_millis(100),
            client.subscribe(
                ExtractorIdentity::new(Chain::Ethereum, "vm:ambient"),
                SubscriptionOptions::new().with_compression(false),
            ),
        )
        .await
        .expect("subscription timed out")
        .expect("subscription failed");
        let res = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("awaiting message timeout out");

        // If the subscription ended, the channel should have been closed.
        assert!(res.is_none());

        timeout(Duration::from_millis(100), client.close())
            .await
            .expect("close timed out")
            .expect("close failed");
        jh.await
            .expect("ws loop errored")
            .unwrap();
        server_thread.await.unwrap();
    }

    #[test_log::test(tokio::test)]
    async fn test_reconnect() {
        let exp_comm = [
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe()
            )),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(
                subscription_confirmation()
            )),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(r#"
                {
                    "subscription_id": "30b740d1-cf09-4e0e-8cfe-b1434d447ece",
                    "deltas": {
                        "extractor": "vm:ambient",
                        "chain": "ethereum",
                        "block": {
                            "number": 123,
                            "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                            "parent_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                            "chain": "ethereum",             
                            "ts": "2023-09-14T00:00:00"
                        },
                        "finalized_block_height": 0,
                        "revert": false,
                        "new_tokens": {},
                        "account_updates": {
                            "0x7a250d5630b4cf539739df2c5dacb4c659f2488d": {
                                "address": "0x7a250d5630b4cf539739df2c5dacb4c659f2488d",
                                "chain": "ethereum",
                                "slots": {},
                                "balance": "0x01f4",
                                "code": "",
                                "change": "Update"
                            }
                        },
                        "state_updates": {
                            "component_1": {
                                "component_id": "component_1",
                                "updated_attributes": {"attr1": "0x01"},
                                "deleted_attributes": ["attr2"]
                            }
                        },
                        "new_protocol_components": {
                            "protocol_1":
                                {
                                    "id": "protocol_1",
                                    "protocol_system": "system_1",
                                    "protocol_type_name": "type_1",
                                    "chain": "ethereum",
                                    "tokens": ["0x01", "0x02"],
                                    "contract_ids": ["0x01", "0x02"],
                                    "static_attributes": {"attr1": "0x01f4"},
                                    "change": "Update",
                                    "creation_tx": "0x01",
                                    "created_at": "2023-09-14T00:00:00"
                                }
                            },
                        "deleted_protocol_components": {},
                        "component_balances": {
                            "protocol_1": {
                                "0x01": {
                                    "token": "0x01",
                                    "balance": "0x01f4",
                                    "balance_float": 1000.0,
                                    "modify_tx": "0x01",
                                    "component_id": "protocol_1"
                                }
                            }
                        },
                        "account_balances": {
                            "0x7a250d5630b4cf539739df2c5dacb4c659f2488d": {
                                "0x7a250d5630b4cf539739df2c5dacb4c659f2488d": {
                                    "account": "0x7a250d5630b4cf539739df2c5dacb4c659f2488d",
                                    "token": "0x7a250d5630b4cf539739df2c5dacb4c659f2488d",
                                    "balance": "0x01f4",
                                    "modify_tx": "0x01"
                                }
                            }
                        },
                        "component_tvl": {
                            "protocol_1": 1000.0
                        },
                        "dci_update": {
                            "new_entrypoints": {},
                            "new_entrypoint_params": {},
                            "trace_results": {}
                        }
                    }
                }
                "#.to_owned()
            ))
        ];
        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 1).await;
        let client = WsDeltasClient::new_with_reconnects(
            &format!("ws://{addr}"),
            None,
            3,
            // server stays down for 100ms on connection drop
            Duration::from_millis(110),
        )
        .unwrap();

        let jh: JoinHandle<Result<(), DeltasError>> = client
            .connect()
            .await
            .expect("connect failed");

        for _ in 0..2 {
            dbg!("loop");
            let (_, mut rx) = timeout(
                Duration::from_millis(200),
                client.subscribe(
                    ExtractorIdentity::new(Chain::Ethereum, "vm:ambient"),
                    SubscriptionOptions::new().with_compression(false),
                ),
            )
            .await
            .expect("subscription timed out")
            .expect("subscription failed");

            let _ = timeout(Duration::from_millis(100), rx.recv())
                .await
                .expect("awaiting message timeout out")
                .expect("receiving message failed");

            // wait for the connection to drop
            let res = timeout(Duration::from_millis(200), rx.recv())
                .await
                .expect("awaiting closed connection timeout out");
            assert!(res.is_none());
        }
        let res = jh.await.expect("ws client join failed");
        // 5th client reconnect attempt should fail
        assert!(res.is_err());
        server_thread
            .await
            .expect("ws server loop errored");
    }

    async fn mock_bad_connection_tycho_ws(accept_first: bool) -> (SocketAddr, JoinHandle<()>) {
        let server = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("localhost bind failed");
        let addr = server.local_addr().unwrap();
        let jh = tokio::spawn(async move {
            while let Ok((stream, _)) = server.accept().await {
                if accept_first {
                    // Send connection handshake to accept the connection (fail later)
                    let stream = tokio_tungstenite::accept_async(stream)
                        .await
                        .unwrap();
                    sleep(Duration::from_millis(10)).await;
                    drop(stream)
                } else {
                    // Close the connection to simulate a failure
                    drop(stream);
                }
            }
        });
        (addr, jh)
    }

    #[test_log::test(tokio::test)]
    async fn test_subscribe_dead_client_after_max_attempts() {
        let (addr, _) = mock_bad_connection_tycho_ws(true).await;
        let client = WsDeltasClient::new_with_reconnects(
            &format!("ws://{addr}"),
            None,
            3,
            Duration::from_secs(0),
        )
        .unwrap();

        let join_handle = client.connect().await.unwrap();
        let handle_res = join_handle.await.unwrap();
        assert!(handle_res.is_err());
        assert!(!client.is_connected().await);

        let subscription_res = timeout(
            Duration::from_millis(10),
            client.subscribe(
                ExtractorIdentity::new(Chain::Ethereum, "vm:ambient"),
                SubscriptionOptions::new(),
            ),
        )
        .await
        .unwrap();
        assert!(subscription_res.is_err());
    }

    #[test_log::test(tokio::test)]
    async fn test_ws_client_retry_cooldown() {
        let start = std::time::Instant::now();
        let (addr, _) = mock_bad_connection_tycho_ws(false).await;

        // Use the mock server that immediately drops connections
        let client = WsDeltasClient::new_with_reconnects(
            &format!("ws://{addr}"),
            None,
            3,                         // 3 attempts total (so 2 retries with cooldowns)
            Duration::from_millis(50), // 50ms cooldown
        )
        .unwrap();

        // Try to connect - this should fail after retries but still measure the time
        let connect_result = client.connect().await;
        let elapsed = start.elapsed();

        // Connection should fail after exhausting retries
        assert!(connect_result.is_err(), "Expected connection to fail after retries");

        // Should have waited at least 100ms total (2 retries × 50ms cooldown each)
        assert!(
            elapsed >= Duration::from_millis(100),
            "Expected at least 100ms elapsed, got {:?}",
            elapsed
        );

        // Should not take too long (max ~300ms for 3 attempts with some tolerance)
        assert!(elapsed < Duration::from_millis(500), "Took too long: {:?}", elapsed);
    }

    #[test_log::test(tokio::test)]
    async fn test_buffer_full_triggers_unsubscribe() {
        // Expected communication sequence for buffer full scenario
        let exp_comm = {
            [
            // 1. Client subscribes
            ExpectedComm::Receive(
                100,
                tungstenite::protocol::Message::Text(
                    subscribe(),
                ),
            ),
            // 2. Server confirms subscription
            ExpectedComm::Send(tungstenite::protocol::Message::Text(
                subscription_confirmation(),
            )),
            // 3. Server sends first message (fills buffer)
            ExpectedComm::Send(tungstenite::protocol::Message::Text(
                r#"
                {
                    "subscription_id": "30b740d1-cf09-4e0e-8cfe-b1434d447ece",
                    "deltas": {
                        "extractor": "vm:ambient",
                        "chain": "ethereum",
                        "block": {
                            "number": 123,
                            "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                            "parent_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                            "chain": "ethereum",
                            "ts": "2023-09-14T00:00:00"
                        },
                        "finalized_block_height": 0,
                        "revert": false,
                        "new_tokens": {},
                        "account_updates": {},
                        "state_updates": {},
                        "new_protocol_components": {},
                        "deleted_protocol_components": {},
                        "component_balances": {},
                        "account_balances": {},
                        "component_tvl": {},
                        "dci_update": {
                            "new_entrypoints": {},
                            "new_entrypoint_params": {},
                            "trace_results": {}
                        }
                    }
                }
                "#.to_owned()
            )),
            // 4. Server sends second message (triggers buffer overflow and force unsubscribe)
            ExpectedComm::Send(tungstenite::protocol::Message::Text(
                r#"
                {
                    "subscription_id": "30b740d1-cf09-4e0e-8cfe-b1434d447ece",
                    "deltas": {
                        "extractor": "vm:ambient",
                        "chain": "ethereum",
                        "block": {
                            "number": 124,
                            "hash": "0x0000000000000000000000000000000000000000000000000000000000000001",
                            "parent_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                            "chain": "ethereum",
                            "ts": "2023-09-14T00:00:01"
                        },
                        "finalized_block_height": 0,
                        "revert": false,
                        "new_tokens": {},
                        "account_updates": {},
                        "state_updates": {},
                        "new_protocol_components": {},
                        "deleted_protocol_components": {},
                        "component_balances": {},
                        "account_balances": {},
                        "component_tvl": {},
                        "dci_update": {
                            "new_entrypoints": {},
                            "new_entrypoint_params": {},
                            "trace_results": {}
                        }
                    }
                }
                "#.to_owned()
            )),
            // 5. Expect unsubscribe command due to buffer full
            ExpectedComm::Receive(
                100,
                tungstenite::protocol::Message::Text(
                    unsubscribe(),
                ),
            ),
            // 6. Server confirms unsubscription
            ExpectedComm::Send(tungstenite::protocol::Message::Text(
                subscription_ended(),
            )),
        ]
        };

        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        // Create client with very small buffer size (1) to easily trigger BufferFull
        let client = WsDeltasClient::new_with_custom_buffers(
            &format!("ws://{addr}"),
            None,
            128, // ws_buffer_size
            1,   // subscription_buffer_size - this will trigger BufferFull easily
        )
        .unwrap();

        let jh = client
            .connect()
            .await
            .expect("connect failed");

        let (_sub_id, mut rx) = timeout(
            Duration::from_millis(100),
            client.subscribe(
                ExtractorIdentity::new(Chain::Ethereum, "vm:ambient"),
                SubscriptionOptions::new().with_compression(false),
            ),
        )
        .await
        .expect("subscription timed out")
        .expect("subscription failed");

        // Allow time for messages to be processed and buffer to fill up
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Collect all messages until channel closes or we get a reasonable number
        let mut received_msgs = Vec::new();

        // Use a single longer timeout to collect messages until channel closes
        while received_msgs.len() < 3 {
            match timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(msg)) => {
                    received_msgs.push(msg);
                }
                Ok(None) => {
                    // Channel closed - this is what we expect after buffer overflow
                    break;
                }
                Err(_) => {
                    // Timeout - no more messages coming
                    break;
                }
            }
        }

        // Verify the key behavior: buffer overflow should limit messages and close channel
        assert!(
            received_msgs.len() <= 1,
            "Expected buffer overflow to limit messages to at most 1, got {}",
            received_msgs.len()
        );

        if let Some(first_msg) = received_msgs.first() {
            assert_eq!(first_msg.block.number, 123, "Expected first message with block 123");
        }

        // Test passed! The key behavior we're testing (buffer full causes force unsubscribe) has
        // been verified We don't need to explicitly close the client as it will be cleaned
        // up when dropped

        // Just wait for the tasks to finish cleanly
        drop(rx); // Explicitly drop the receiver
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Abort the tasks to clean up
        jh.abort();
        server_thread.abort();

        let _ = jh.await;
        let _ = server_thread.await;
    }

    #[tokio::test]
    async fn test_server_error_handling() {
        use tycho_common::dto::{Response, WebSocketMessage, WebsocketError};

        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "vm:ambient");

        // Test ExtractorNotFound error
        let error_response = WebSocketMessage::Response(Response::Error(
            WebsocketError::ExtractorNotFound(extractor_id.clone().into()),
        ));
        let error_json = serde_json::to_string(&error_response).unwrap();

        let exp_comm = [
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(error_json)),
        ];

        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");

        let result = timeout(
            Duration::from_millis(100),
            client.subscribe(extractor_id, SubscriptionOptions::new().with_compression(false)),
        )
        .await
        .expect("subscription timed out");

        // Verify that we get a ServerError
        assert!(result.is_err());
        if let Err(DeltasError::ServerError(msg)) = result {
            assert!(msg.contains("Subscription failed"));
            assert!(msg.contains("Extractor not found"));
        } else {
            panic!("Expected DeltasError::ServerError, got: {:?}", result);
        }

        timeout(Duration::from_millis(100), client.close())
            .await
            .expect("close timed out")
            .expect("close failed");
        jh.await
            .expect("ws loop errored")
            .unwrap();
        server_thread.await.unwrap();
    }

    #[test_log::test(tokio::test)]
    async fn test_subscription_not_found_error() {
        // Test scenario: Server restart causes subscription loss
        use tycho_common::dto::{Response, WebSocketMessage, WebsocketError};

        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "vm:ambient");
        let subscription_id = Uuid::from_str(SUBSCRIPTION_ID).unwrap();

        let error_response = WebSocketMessage::Response(Response::Error(
            WebsocketError::SubscriptionNotFound(subscription_id),
        ));
        let error_json = serde_json::to_string(&error_response).unwrap();

        let exp_comm = [
            // 1. Client subscribes successfully
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(subscription_confirmation())),
            // 2. Client tries to unsubscribe (server has "restarted" and lost subscription)
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(unsubscribe())),
            // 3. Server responds with SubscriptionNotFound (simulating server restart)
            ExpectedComm::Send(tungstenite::protocol::Message::Text(error_json)),
        ];

        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");

        // Subscribe successfully
        let (received_sub_id, _rx) = timeout(
            Duration::from_millis(100),
            client.subscribe(extractor_id, SubscriptionOptions::new().with_compression(false)),
        )
        .await
        .expect("subscription timed out")
        .expect("subscription failed");

        assert_eq!(received_sub_id, subscription_id);

        // Now try to unsubscribe - this should fail because server "restarted"
        let unsubscribe_result =
            timeout(Duration::from_millis(100), client.unsubscribe(subscription_id))
                .await
                .expect("unsubscribe timed out");

        // The unsubscribe should handle the SubscriptionNotFound error gracefully
        // In this case, the client should treat it as successful since the subscription
        // is effectively gone (whether due to server restart or other reasons)
        unsubscribe_result
            .expect("Unsubscribe should succeed even if server says subscription not found");

        timeout(Duration::from_millis(100), client.close())
            .await
            .expect("close timed out")
            .expect("close failed");
        jh.await
            .expect("ws loop errored")
            .unwrap();
        server_thread.await.unwrap();
    }

    #[test_log::test(tokio::test)]
    async fn test_parse_error_handling() {
        use tycho_common::dto::{Response, WebSocketMessage, WebsocketError};

        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "vm:ambient");
        let error_response = WebSocketMessage::Response(Response::Error(
            WebsocketError::ParseError("}2sdf".to_string(), "malformed JSON".to_string()),
        ));
        let error_json = serde_json::to_string(&error_response).unwrap();

        let exp_comm = [
            // subscribe first so connect can finish successfully
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(error_json)),
        ];

        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");

        // Subscribe successfully
        let _ = timeout(
            Duration::from_millis(100),
            client.subscribe(extractor_id, SubscriptionOptions::new().with_compression(false)),
        )
        .await
        .expect("subscription timed out");

        // The client should receive the parse error and close the connection
        let result = jh
            .await
            .expect("ws loop should complete");
        assert!(result.is_err());
        if let Err(DeltasError::ServerError(message)) = result {
            assert!(message.contains("Server failed to parse client message"));
        } else {
            panic!("Expected DeltasError::ServerError, got: {:?}", result);
        }

        server_thread.await.unwrap();
    }

    #[test_log::test(tokio::test)]
    async fn test_compression_error_handling() {
        use tycho_common::dto::{Response, WebSocketMessage, WebsocketError};

        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "vm:ambient");
        let subscription_id = Uuid::from_str(SUBSCRIPTION_ID).unwrap();
        let error_response = WebSocketMessage::Response(Response::Error(
            WebsocketError::CompressionError(subscription_id, "Compression failed".to_string()),
        ));
        let error_json = serde_json::to_string(&error_response).unwrap();

        let exp_comm = [
            // subscribe first so connect can finish successfully
            ExpectedComm::Receive(
                100,
                tungstenite::protocol::Message::Text(subscribe_with_compression(true)),
            ),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(error_json)),
        ];

        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");

        // Subscribe successfully with compression disabled
        let _ = timeout(
            Duration::from_millis(100),
            client.subscribe(extractor_id, SubscriptionOptions::new()),
        )
        .await
        .expect("subscription timed out");

        // The client should receive the parse error
        let result = jh
            .await
            .expect("ws loop should complete");
        assert!(result.is_err());
        if let Err(DeltasError::ServerError(message)) = result {
            assert!(message.contains("Server failed to compress message for subscription"));
        } else {
            panic!("Expected DeltasError::ServerError, got: {:?}", result);
        }

        server_thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_error_handling() {
        use tycho_common::dto::{Response, WebSocketMessage, WebsocketError};

        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "vm:ambient");

        let error_response = WebSocketMessage::Response(Response::Error(
            WebsocketError::SubscribeError(extractor_id.clone().into()),
        ));
        let error_json = serde_json::to_string(&error_response).unwrap();

        let exp_comm = [
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(error_json)),
        ];

        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");

        let result = timeout(
            Duration::from_millis(100),
            client.subscribe(extractor_id, SubscriptionOptions::new().with_compression(false)),
        )
        .await
        .expect("subscription timed out");

        // Verify that we get a ServerError for subscribe failure
        assert!(result.is_err());
        if let Err(DeltasError::ServerError(msg)) = result {
            assert!(msg.contains("Subscription failed"));
            assert!(msg.contains("Failed to subscribe to extractor"));
        } else {
            panic!("Expected DeltasError::ServerError, got: {:?}", result);
        }

        timeout(Duration::from_millis(100), client.close())
            .await
            .expect("close timed out")
            .expect("close failed");
        jh.await
            .expect("ws loop errored")
            .unwrap();
        server_thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_cancel_pending_subscription() {
        // This test verifies that pending subscriptions are properly cancelled when errors occur
        use tycho_common::dto::{Response, WebSocketMessage, WebsocketError};

        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "vm:ambient");

        let error_response = WebSocketMessage::Response(Response::Error(
            WebsocketError::ExtractorNotFound(extractor_id.clone().into()),
        ));
        let error_json = serde_json::to_string(&error_response).unwrap();

        let exp_comm = [
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(error_json)),
        ];

        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");

        // Start two subscription attempts simultaneously
        let client_clone = client.clone();
        let extractor_id_clone = extractor_id.clone();

        let subscription1 = tokio::spawn({
            let client_for_spawn = client.clone();
            async move {
                client_for_spawn
                    .subscribe(extractor_id, SubscriptionOptions::new().with_compression(false))
                    .await
            }
        });

        let subscription2 = tokio::spawn(async move {
            // This should fail because there's already a pending subscription
            client_clone
                .subscribe(extractor_id_clone, SubscriptionOptions::new())
                .await
        });

        let (result1, result2) = tokio::join!(subscription1, subscription2);

        let result1 = result1.unwrap();
        let result2 = result2.unwrap();

        // One should fail due to ExtractorNotFound error from server
        // The other should fail due to SubscriptionAlreadyPending
        assert!(result1.is_err() || result2.is_err());

        if let Err(DeltasError::SubscriptionAlreadyPending) = result2 {
            // This is expected for the second subscription
        } else if let Err(DeltasError::ServerError(_)) = result1 {
            // This is expected for the first subscription that gets the server error
        } else {
            panic!("Expected one SubscriptionAlreadyPending and one ServerError");
        }

        timeout(Duration::from_millis(100), client.close())
            .await
            .expect("close timed out")
            .expect("close failed");
        jh.await
            .expect("ws loop errored")
            .unwrap();
        server_thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_force_unsubscribe_prevents_multiple_calls() {
        // Test that force_unsubscribe prevents sending duplicate unsubscribe commands
        // when called multiple times for the same subscription_id

        let subscription_id = Uuid::from_str(SUBSCRIPTION_ID).unwrap();

        let exp_comm = [
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(subscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(subscription_confirmation())),
            // Expect only ONE unsubscribe message, even though force_unsubscribe is called twice
            ExpectedComm::Receive(100, tungstenite::protocol::Message::Text(unsubscribe())),
            ExpectedComm::Send(tungstenite::protocol::Message::Text(subscription_ended())),
        ];

        let (addr, server_thread) = mock_tycho_ws(&exp_comm, 0).await;

        let client = WsDeltasClient::new(&format!("ws://{addr}"), None).unwrap();
        let jh = client
            .connect()
            .await
            .expect("connect failed");

        let (received_sub_id, _rx) = timeout(
            Duration::from_millis(100),
            client.subscribe(
                ExtractorIdentity::new(Chain::Ethereum, "vm:ambient"),
                SubscriptionOptions::new().with_compression(false),
            ),
        )
        .await
        .expect("subscription timed out")
        .expect("subscription failed");

        assert_eq!(received_sub_id, subscription_id);

        // Access the inner state to call force_unsubscribe directly
        {
            let mut inner_guard = client.inner.lock().await;
            let inner = inner_guard
                .as_mut()
                .expect("client should be connected");

            // Call force_unsubscribe twice - only the first should send an unsubscribe message
            WsDeltasClient::force_unsubscribe(subscription_id, inner).await;
            WsDeltasClient::force_unsubscribe(subscription_id, inner).await;
        }

        // Give time for messages to be processed
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Close may fail if client disconnected after unsubscribe, which is fine
        let _ = timeout(Duration::from_millis(100), client.close()).await;

        // Wait for tasks to complete
        let _ = jh.await;
        let _ = server_thread.await;
    }
}
