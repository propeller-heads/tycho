use std::collections::HashMap;

use futures::{stream::select_all, StreamExt};
use tycho_client::feed::{synchronizer::ComponentWithState, FeedMessage};
use tycho_common::{
    models::token::Token,
    simulation::{errors::SimulationError, protocol_sim::ProtocolSim},
    Bytes,
};

use crate::{
    evm::decoder::TychoStreamDecoder,
    protocol::{
        errors::InvalidSnapshotError,
        models::{TryFromWithBlock, Update},
    },
    rfq::{client::RFQClient, models::TimestampHeader},
};

/// `RFQStreamBuilder` is a utility for constructing and managing a merged stream of RFQ (Request
/// For Quote) providers in Tycho.
///
/// It allows you to:
/// - Register multiple `RFQClient` implementations, each providing its own stream of RFQ price
///   updates.
/// - Dynamically decode incoming updates into `Update` objects using `TychoStreamDecoder`.
///
/// The `build` method consumes the builder and runs the event loop, sending decoded `Update`s
/// through the provided `mpsc::Sender`. It returns an error if decoding an update or forwarding
/// it to the channel fails.
///
/// ### Error Handling:
/// - Each `RFQClient`'s stream is expected to yield `Result<(String, StateSyncMessage), RFQError>`.
/// - If a client's stream returns an `Err` (e.g., `RFQError::FatalError`), the client is
///   **removed** from the merged stream, and the system continues running without it.
#[derive(Default)]
pub struct RFQStreamBuilder {
    clients: Vec<Box<dyn RFQClient>>,
    decoder: TychoStreamDecoder<TimestampHeader>,
}

impl RFQStreamBuilder {
    pub fn new() -> Self {
        Self { clients: Vec::new(), decoder: TychoStreamDecoder::new() }
    }

    pub fn add_client<T>(mut self, name: &str, provider: Box<dyn RFQClient>) -> Self
    where
        T: ProtocolSim
            + TryFromWithBlock<ComponentWithState, TimestampHeader, Error = InvalidSnapshotError>
            + Send
            + 'static,
    {
        self.clients.push(provider);
        self.decoder.register_decoder::<T>(name);
        self
    }

    pub async fn build(self, tx: tokio::sync::mpsc::Sender<Update>) -> Result<(), SimulationError> {
        let streams: Vec<_> = self
            .clients
            .into_iter()
            .map(|provider| provider.stream())
            .collect();

        let mut merged = select_all(streams);

        while let Some(next) = merged.next().await {
            match next {
                Ok((provider, msg)) => {
                    let update = self
                        .decoder
                        .decode(&FeedMessage {
                            state_msgs: HashMap::from([(provider.clone(), msg)]),
                            sync_states: HashMap::new(),
                        })
                        .await
                        .map_err(|e| {
                            SimulationError::RecoverableError(format!("Decoding error: {e}"))
                        })?;
                    tx.send(update).await.map_err(|e| {
                        SimulationError::RecoverableError(format!(
                            "Failed to send update through channel: {e}"
                        ))
                    })?;
                }
                Err(e) => {
                    tracing::error!(
                        "RFQ stream fatal error: {e}. Assuming this stream will not emit more messages."
                    );
                }
            }
        }

        Ok(())
    }

    /// Provides token metadata used to decode startup snapshots and initialize protocol states.
    ///
    /// This is not an ongoing stream filter. Components arriving after startup include their
    /// own token metadata for decoding.
    pub async fn set_tokens(self, tokens: HashMap<Bytes, Token>) -> Self {
        self.decoder.set_tokens(tokens).await;
        self
    }
}

#[cfg(test)]
mod tests {
    use std::{any::Any, time::Duration};

    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use num_bigint::BigUint;
    use serde::{Deserialize, Serialize};
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::IntervalStream;
    use tycho_client::feed::synchronizer::{Snapshot, StateSyncMessage};
    use tycho_common::{
        dto::ProtocolStateDelta,
        models::{
            protocol::{GetAmountOutParams, ProtocolComponent, ProtocolComponentState},
            token::Token,
        },
        simulation::{
            errors::{SimulationError, TransitionError},
            indicatively_priced::SignedQuote,
            protocol_sim::{Balances, GetAmountOutResult},
        },
        Bytes,
    };

    use super::*;
    use crate::{protocol::models::DecoderContext, rfq::errors::RFQError};

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct DummyProtocol;

    #[typetag::serde]
    impl ProtocolSim for DummyProtocol {
        fn fee(&self) -> f64 {
            unimplemented!("Not needed for this test")
        }

        fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
            unimplemented!("Not needed for this test")
        }

        fn get_amount_out(
            &self,
            _amount_in: BigUint,
            _token_in: &Token,
            _token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            unimplemented!("Not needed for this test")
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            unimplemented!("Not needed for this test")
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
            unimplemented!("Not needed for this test")
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn eq(&self, _other: &dyn ProtocolSim) -> bool {
            unimplemented!("Not needed for this test")
        }
    }

    impl TryFromWithBlock<ComponentWithState, TimestampHeader> for DummyProtocol {
        type Error = InvalidSnapshotError;
        async fn try_from_with_header(
            _value: ComponentWithState,
            _header: TimestampHeader,
            _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
            _all_tokens: &HashMap<Bytes, Token>,
            _decoder_context: &DecoderContext,
        ) -> Result<Self, Self::Error> {
            Ok(DummyProtocol)
        }
    }

    pub struct MockRFQClient {
        name: String,
        interval: Duration,
        error_at_time: Option<u128>,
    }

    impl MockRFQClient {
        pub fn new(name: &str, interval: Duration, error_at_time: Option<u128>) -> Self {
            Self { name: name.to_string(), interval, error_at_time }
        }
    }

    #[async_trait]
    impl RFQClient for MockRFQClient {
        fn stream(
            &self,
        ) -> BoxStream<'static, Result<(String, StateSyncMessage<TimestampHeader>), RFQError>>
        {
            let name = self.name.clone();
            let error_at_time = self.error_at_time;
            let mut current_time: u128 = 0;
            let interval = self.interval;
            let interval =
                IntervalStream::new(tokio::time::interval(self.interval)).map(move |_| {
                    if let Some(error_at_time) = error_at_time {
                        if error_at_time == current_time {
                            return Err(RFQError::FatalError(format!(
                                "{name} stream is dying and can't go on"
                            )));
                        };
                    };
                    let protocol_component =
                        ProtocolComponent { protocol_system: name.clone(), ..Default::default() };

                    let snapshot = Snapshot {
                        states: HashMap::from([(
                            name.clone(),
                            ComponentWithState {
                                state: ProtocolComponentState {
                                    component_id: name.clone(),
                                    attributes: HashMap::new(),
                                    balances: HashMap::new(),
                                },
                                component: protocol_component,
                                component_tvl: None,
                                entrypoints: vec![],
                            },
                        )]),
                        vm_storage: HashMap::new(),
                    };

                    let msg = StateSyncMessage {
                        header: TimestampHeader { timestamp: current_time as u64 },
                        snapshots: snapshot,
                        ..Default::default()
                    };

                    current_time += interval.as_millis();
                    Ok((name.clone(), msg))
                });
            Box::pin(interval)
        }

        async fn request_binding_quote(
            &self,
            _params: &GetAmountOutParams,
        ) -> Result<SignedQuote, RFQError> {
            unimplemented!("Not needed for this test")
        }
    }

    #[tokio::test]
    async fn test_rfq_stream_builder() {
        // This test has two mocked RFQ clients
        // 1. Bebop client that emits a message every 100ms
        // 2. Hashflow client that emits a message every 200m
        let (tx, mut rx) = mpsc::channel::<Update>(10);

        let builder = RFQStreamBuilder::new()
            .add_client::<DummyProtocol>(
                "bebop",
                Box::new(MockRFQClient::new("bebop", Duration::from_millis(100), Some(300))),
            )
            .add_client::<DummyProtocol>(
                "hashflow",
                Box::new(MockRFQClient::new("hashflow", Duration::from_millis(200), None)),
            );

        tokio::spawn(builder.build(tx));

        // Collect only the first 10 messages
        let mut updates = Vec::new();
        for _ in 0..6 {
            let update = rx.recv().await.unwrap();
            updates.push(update);
        }

        // Collect all timestamps per provider
        let bebop_updates: Vec<_> = updates
            .iter()
            .filter(|u| u.new_pairs.contains_key("bebop"))
            .collect();
        let hashflow_updates: Vec<_> = updates
            .iter()
            .filter(|u| u.new_pairs.contains_key("hashflow"))
            .collect();

        assert_eq!(bebop_updates[0].block_number_or_timestamp, 0,);
        assert_eq!(hashflow_updates[0].block_number_or_timestamp, 0,);
        assert_eq!(bebop_updates[1].block_number_or_timestamp, 100);
        assert_eq!(bebop_updates[2].block_number_or_timestamp, 200);
        assert_eq!(hashflow_updates[1].block_number_or_timestamp, 200);
        // At this point the bebop stream dies, and we shouldn't have any more bebop updates, only
        // hashflow
        assert_eq!(bebop_updates.len(), 3);
        assert_eq!(hashflow_updates[2].block_number_or_timestamp, 400);
    }
}
