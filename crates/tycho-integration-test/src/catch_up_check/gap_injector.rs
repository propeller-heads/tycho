//! A deltas client that can end its subscriptions after dropping some of their messages, so the
//! synchronizers resubscribe having missed blocks.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use tokio::{
    sync::{
        mpsc::{self, Receiver, Sender, UnboundedSender},
        watch,
    },
    task::JoinHandle,
};
use tracing::{debug, info};
use tycho_client::{
    deltas::{DeltasClient, SubscriptionOptions},
    feed::BlockHeader,
    DeltasError,
};
use tycho_common::models::{
    blockchain::BlockAggregatedChanges, ExtractorFingerprint, ExtractorIdentity,
};
use uuid::Uuid;

/// Capacity of the channel between a forwarder and its synchronizer. One message, so the wrapped
/// client's subscription buffer still decides when a slow synchronizer overflows it.
const FORWARD_BUFFER: usize = 1;

/// The last block each extractor's subscriptions delivered to its synchronizer.
type LastDelivered = Arc<Mutex<HashMap<ExtractorIdentity, BlockHeader>>>;

/// A new subscription of an extractor that had one before, seen at its first message above the
/// last block the previous subscription delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resubscription {
    pub extractor: String,
    /// That message's block number. The synchronizer skips messages at or below the block it
    /// synced last, so it resumes, catches up or takes a snapshot at this block or a later one.
    pub first_block: u64,
    /// Whether that message is a block whose parent is the last block the previous subscription
    /// delivered, so the subscription missed nothing.
    pub follows_previous: bool,
}

/// Asks every live subscription of a [`GapInjector`] for a gap.
#[derive(Clone)]
pub struct GapTrigger(Arc<watch::Sender<u64>>);

impl GapTrigger {
    /// Makes every live subscription drop its next messages and then end. Subscriptions made
    /// afterwards are not affected.
    pub fn request_gap(&self) {
        self.0
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

/// Wraps a deltas client and passes each subscription's messages through a forwarder task. When a
/// gap is requested, the forwarder drops the subscription's next `gap_blocks` messages and ends
/// it, so the synchronizer resubscribes having missed those blocks, as after a dropped connection
/// or an overflowing subscription buffer.
///
/// Reports each resubscription of an extractor at its first message above the last block the
/// previous subscription delivered, whatever ended the previous one.
#[derive(Clone)]
pub struct GapInjector<D> {
    inner: D,
    gap_blocks: usize,
    gaps: Arc<watch::Sender<u64>>,
    last_delivered: LastDelivered,
    resubscriptions: UnboundedSender<Resubscription>,
}

impl<D> GapInjector<D> {
    /// Wraps `inner`. Resubscriptions are sent to `resubscriptions`; the returned trigger requests
    /// gaps.
    pub fn new(
        inner: D,
        gap_blocks: usize,
        resubscriptions: UnboundedSender<Resubscription>,
    ) -> (Self, GapTrigger) {
        let (gaps, _) = watch::channel(0);
        let gaps = Arc::new(gaps);
        let injector = Self {
            inner,
            gap_blocks,
            gaps: gaps.clone(),
            last_delivered: LastDelivered::default(),
            resubscriptions,
        };
        (injector, GapTrigger(gaps))
    }
}

#[async_trait]
impl<D: DeltasClient + Send + Sync> DeltasClient for GapInjector<D> {
    async fn subscribe(
        &self,
        extractor_id: ExtractorIdentity,
        options: SubscriptionOptions,
    ) -> Result<(Uuid, Receiver<BlockAggregatedChanges>), DeltasError> {
        let (subscription_id, receiver, _) = self
            .subscribe_with_fingerprint(extractor_id, options)
            .await?;
        Ok((subscription_id, receiver))
    }

    async fn subscribe_with_fingerprint(
        &self,
        extractor_id: ExtractorIdentity,
        options: SubscriptionOptions,
    ) -> Result<(Uuid, Receiver<BlockAggregatedChanges>, Option<ExtractorFingerprint>), DeltasError>
    {
        let (subscription_id, inner, fingerprint) = self
            .inner
            .subscribe_with_fingerprint(extractor_id.clone(), options)
            .await?;
        let previous = self
            .last_delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&extractor_id)
            .cloned();
        let (outer, receiver) = mpsc::channel(FORWARD_BUFFER);
        let forwarder = Forwarder {
            extractor: extractor_id,
            inner,
            outer,
            gaps: self.gaps.subscribe(),
            gap_blocks: self.gap_blocks,
            previous,
            last_delivered: self.last_delivered.clone(),
            resubscriptions: self.resubscriptions.clone(),
        };
        tokio::spawn(forwarder.run());
        Ok((subscription_id, receiver, fingerprint))
    }

    async fn unsubscribe(&self, subscription_id: Uuid) -> Result<(), DeltasError> {
        self.inner
            .unsubscribe(subscription_id)
            .await
    }

    async fn connect(&self) -> Result<JoinHandle<Result<(), DeltasError>>, DeltasError> {
        self.inner.connect().await
    }

    async fn close(&self) -> Result<(), DeltasError> {
        self.inner.close().await
    }
}

/// Passes one subscription's messages from the wrapped client to the synchronizer.
struct Forwarder {
    extractor: ExtractorIdentity,
    inner: Receiver<BlockAggregatedChanges>,
    outer: Sender<BlockAggregatedChanges>,
    gaps: watch::Receiver<u64>,
    gap_blocks: usize,
    /// The last block the extractor's previous subscription delivered. Cleared once this
    /// subscription is reported as a resubscription; `None` for the first subscription.
    previous: Option<BlockHeader>,
    last_delivered: LastDelivered,
    resubscriptions: UnboundedSender<Resubscription>,
}

impl Forwarder {
    async fn run(mut self) {
        loop {
            let message = tokio::select! {
                // A requested gap takes effect before any further message is forwarded.
                biased;
                Ok(()) = self.gaps.changed() => break,
                message = self.inner.recv() => message,
            };
            let Some(message) = message else {
                return;
            };
            self.report_resubscription(&message);
            let header = BlockHeader::from(&message);
            if self.outer.send(message).await.is_err() {
                return;
            }
            self.last_delivered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(self.extractor.clone(), header);
        }

        let Self {
            extractor,
            mut inner,
            outer,
            gaps: _,
            gap_blocks,
            previous: _,
            last_delivered: _,
            resubscriptions: _,
        } = self;
        for _ in 0..gap_blocks {
            if inner.recv().await.is_none() {
                return;
            }
        }
        info!(%extractor, gap_blocks, "Ending the subscription after dropping its next messages");
        drop(outer);
        // Keep the wrapped subscription drained until the synchronizer unsubscribes from it, so
        // its buffer does not overflow meanwhile.
        while inner.recv().await.is_some() {}
    }

    fn report_resubscription(&mut self, message: &BlockAggregatedChanges) {
        let Some(previous) = self
            .previous
            .take_if(|previous| message.block.number > previous.number)
        else {
            return;
        };
        let resubscription = Resubscription {
            extractor: self.extractor.name.clone(),
            first_block: message.block.number,
            follows_previous: !message.revert && message.block.parent_hash == previous.hash,
        };
        if self
            .resubscriptions
            .send(resubscription)
            .is_err()
        {
            debug!(extractor = %self.extractor, "Nobody receives resubscriptions any more");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{sync::mpsc::UnboundedReceiver, time::timeout};
    use tycho_common::{
        models::{blockchain::Block, Chain},
        Bytes,
    };

    use super::*;

    /// A deltas client whose subscriptions deliver what the test sends them.
    #[derive(Clone, Default)]
    struct ScriptedDeltasClient {
        /// The senders of the subscriptions made so far, oldest first.
        subscriptions: Arc<Mutex<Vec<Sender<BlockAggregatedChanges>>>>,
    }

    impl ScriptedDeltasClient {
        fn subscription(&self, index: usize) -> Sender<BlockAggregatedChanges> {
            self.subscriptions.lock().unwrap()[index].clone()
        }

        /// Ends subscription `index`, as the server does when the subscription is unsubscribed.
        fn end(&self, index: usize) {
            let (closed, _) = mpsc::channel(1);
            self.subscriptions.lock().unwrap()[index] = closed;
        }
    }

    #[async_trait]
    impl DeltasClient for ScriptedDeltasClient {
        async fn subscribe(
            &self,
            _extractor_id: ExtractorIdentity,
            _options: SubscriptionOptions,
        ) -> Result<(Uuid, Receiver<BlockAggregatedChanges>), DeltasError> {
            let (sender, receiver) = mpsc::channel(16);
            let mut subscriptions = self.subscriptions.lock().unwrap();
            subscriptions.push(sender);
            Ok((Uuid::from_u128(subscriptions.len() as u128), receiver))
        }

        async fn unsubscribe(&self, _subscription_id: Uuid) -> Result<(), DeltasError> {
            Ok(())
        }

        async fn connect(&self) -> Result<JoinHandle<Result<(), DeltasError>>, DeltasError> {
            Ok(tokio::spawn(async { Ok(()) }))
        }

        async fn close(&self) -> Result<(), DeltasError> {
            Ok(())
        }
    }

    struct Setup {
        client: ScriptedDeltasClient,
        injector: GapInjector<ScriptedDeltasClient>,
        trigger: GapTrigger,
        resubscriptions: UnboundedReceiver<Resubscription>,
    }

    fn setup(gap_blocks: usize) -> Setup {
        let client = ScriptedDeltasClient::default();
        let (sender, resubscriptions) = mpsc::unbounded_channel();
        let (injector, trigger) = GapInjector::new(client.clone(), gap_blocks, sender);
        Setup { client, injector, trigger, resubscriptions }
    }

    fn extractor() -> ExtractorIdentity {
        ExtractorIdentity::new(Chain::Ethereum, "uniswap_v2")
    }

    fn hash(number: u64) -> Bytes {
        Bytes::from(number).lpad(32, 0)
    }

    fn block(number: u64) -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            extractor: "uniswap_v2".to_string(),
            chain: Chain::Ethereum,
            block: Block {
                number,
                chain: Chain::Ethereum,
                hash: hash(number),
                parent_hash: hash(number - 1),
                ts: Default::default(),
            },
            ..BlockAggregatedChanges::default()
        }
    }

    async fn subscribe(
        injector: &GapInjector<ScriptedDeltasClient>,
    ) -> Receiver<BlockAggregatedChanges> {
        let (_, receiver) = injector
            .subscribe(extractor(), SubscriptionOptions::new())
            .await
            .unwrap();
        receiver
    }

    async fn receive(receiver: &mut Receiver<BlockAggregatedChanges>) -> Option<u64> {
        timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("no message within a second")
            .map(|message| message.block.number)
    }

    /// Delivers blocks `numbers` through subscription `index` and receives them on `receiver`.
    async fn deliver(
        client: &ScriptedDeltasClient,
        index: usize,
        receiver: &mut Receiver<BlockAggregatedChanges>,
        numbers: &[u64],
    ) {
        for number in numbers {
            client
                .subscription(index)
                .send(block(*number))
                .await
                .unwrap();
            assert_eq!(receive(receiver).await, Some(*number));
        }
    }

    #[tokio::test]
    async fn test_gap_drops_the_next_messages_and_ends_the_subscription() {
        let Setup { client, injector, trigger, .. } = setup(2);
        let mut receiver = subscribe(&injector).await;
        deliver(&client, 0, &mut receiver, &[1, 2]).await;

        trigger.request_gap();
        client
            .subscription(0)
            .send(block(3))
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(100), receiver.recv())
                .await
                .is_err(),
            "the subscription stays open, forwarding nothing, until it dropped two messages"
        );
        client
            .subscription(0)
            .send(block(4))
            .await
            .unwrap();

        assert_eq!(receive(&mut receiver).await, None);
    }

    #[tokio::test]
    async fn test_resubscription_after_a_gap_is_reported_at_its_first_new_block() {
        let Setup { client, injector, trigger, mut resubscriptions } = setup(2);
        let mut receiver = subscribe(&injector).await;
        deliver(&client, 0, &mut receiver, &[1, 2]).await;
        trigger.request_gap();
        for number in 3..=4 {
            client
                .subscription(0)
                .send(block(number))
                .await
                .unwrap();
        }
        assert_eq!(receive(&mut receiver).await, None);

        let mut receiver = subscribe(&injector).await;
        deliver(&client, 1, &mut receiver, &[2, 5, 6]).await;

        assert_eq!(
            resubscriptions.try_recv(),
            Ok(Resubscription {
                extractor: "uniswap_v2".to_string(),
                first_block: 5,
                follows_previous: false,
            })
        );
        assert!(resubscriptions.try_recv().is_err(), "a resubscription is reported once");
    }

    #[tokio::test]
    async fn test_resubscription_that_missed_nothing_follows_the_previous_one() {
        let Setup { client, injector, mut resubscriptions, .. } = setup(2);
        let mut receiver = subscribe(&injector).await;
        deliver(&client, 0, &mut receiver, &[1, 2]).await;
        client.end(0);
        assert_eq!(receive(&mut receiver).await, None);
        assert!(resubscriptions.try_recv().is_err(), "a first subscription is not reported");

        let mut receiver = subscribe(&injector).await;
        deliver(&client, 1, &mut receiver, &[3]).await;

        assert_eq!(
            resubscriptions.try_recv(),
            Ok(Resubscription {
                extractor: "uniswap_v2".to_string(),
                first_block: 3,
                follows_previous: true,
            })
        );
    }

    #[tokio::test]
    async fn test_gap_requested_before_a_subscription_does_not_end_it() {
        let Setup { client, injector, trigger, .. } = setup(2);
        trigger.request_gap();

        let mut receiver = subscribe(&injector).await;

        deliver(&client, 0, &mut receiver, &[1, 2, 3]).await;
    }
}
