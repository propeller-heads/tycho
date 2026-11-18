//! Admin pauses known to this process, re-read from the pause table every [`REFRESH_INTERVAL`].
//!
//! An admin call writes only the pause table, and reaches only one of the processes that serve
//! clients. Each process therefore re-reads the table and keeps its own [`PauseRegistry`].

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio::{
    sync::watch,
    time::{interval_at, Instant, MissedTickBehavior},
};
use tracing::{info, warn};
use tycho_common::{
    models::{protocol::ComponentPause, Chain, ComponentId, ProtocolSystem},
    storage::{ComponentPauseGateway, StorageError},
};

/// How often [`PauseRefresher`] re-reads the pause table.
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// A component's admin pause starting or ending.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PauseEvent {
    Pause(ComponentId),
    Unpause(ComponentId),
}

/// The admin pauses this process knows of: the paused components of each protocol system, and
/// the pause events not yet sent to clients.
///
/// Events are queued only for the protocol systems this process extracts, since only their
/// extractors send events to clients.
#[derive(Debug, Default)]
pub struct PauseRegistry {
    systems: HashMap<ProtocolSystem, SystemPauses>,
    extracted_systems: HashSet<ProtocolSystem>,
}

#[derive(Debug, Default)]
struct SystemPauses {
    /// Components paused right now.
    paused: HashSet<ComponentId>,
    /// Events not yet sent to clients, oldest first.
    pending: VecDeque<PauseEvent>,
}

impl PauseRegistry {
    /// Builds the registry from the pauses in effect on `chain`. Later refreshes queue events
    /// for `extracted_systems`; the loaded pauses queue none, since they started before this
    /// process did.
    ///
    /// # Errors
    /// Returns the storage error if the pauses cannot be read.
    pub async fn load<G: ComponentPauseGateway>(
        gateway: &G,
        chain: &Chain,
        extracted_systems: HashSet<ProtocolSystem>,
    ) -> Result<Self, StorageError> {
        let pauses = gateway
            .get_component_pauses(chain)
            .await?;
        let systems = group_by_system(pauses)
            .into_iter()
            .map(|(system, paused)| (system, SystemPauses { paused, pending: VecDeque::new() }))
            .collect();
        Ok(Self { systems, extracted_systems })
    }

    /// Whether component `id` of protocol `system` is paused right now.
    pub fn is_paused(&self, system: &str, id: &str) -> bool {
        self.systems
            .get(system)
            .is_some_and(|pauses| pauses.paused.contains(id))
    }

    /// Removes and returns the queued events of protocol `system`, oldest first.
    pub fn drain_events(&mut self, system: &str) -> Vec<PauseEvent> {
        self.systems
            .get_mut(system)
            .map(|pauses| pauses.pending.drain(..).collect())
            .unwrap_or_default()
    }

    /// Makes `current` the paused set of every system. For each extracted system, queues one
    /// event for each component whose pause started or ended.
    ///
    /// Returns whether any paused set changed.
    fn apply(&mut self, mut current: HashMap<ProtocolSystem, HashSet<ComponentId>>) -> bool {
        let mut changed = false;
        for (system, pauses) in &mut self.systems {
            let queue_events = self.extracted_systems.contains(system);
            changed |= pauses.update(
                current
                    .remove(system)
                    .unwrap_or_default(),
                queue_events,
            );
        }
        for (system, paused) in current {
            let queue_events = self.extracted_systems.contains(&system);
            changed |= self
                .systems
                .entry(system)
                .or_default()
                .update(paused, queue_events);
        }
        changed
    }
}

impl SystemPauses {
    /// Replaces the paused set with `current`. With `queue_events`, also queues a `Pause` for
    /// each newly paused component and an `Unpause` for each one no longer paused.
    ///
    /// Returns whether the set changed.
    fn update(&mut self, current: HashSet<ComponentId>, queue_events: bool) -> bool {
        if self.paused == current {
            return false;
        }
        if queue_events {
            self.pending.extend(
                current
                    .difference(&self.paused)
                    .cloned()
                    .map(PauseEvent::Pause),
            );
            self.pending.extend(
                self.paused
                    .difference(&current)
                    .cloned()
                    .map(PauseEvent::Unpause),
            );
        }
        self.paused = current;
        true
    }
}

fn group_by_system(pauses: Vec<ComponentPause>) -> HashMap<ProtocolSystem, HashSet<ComponentId>> {
    let mut grouped: HashMap<ProtocolSystem, HashSet<ComponentId>> = HashMap::new();
    for pause in pauses {
        grouped
            .entry(pause.protocol_system)
            .or_default()
            .insert(pause.record.component_id);
    }
    grouped
}

/// Re-reads the pause table into a [`PauseRegistry`] every [`REFRESH_INTERVAL`]. The only writer
/// of its paused sets.
pub struct PauseRefresher<G> {
    gateway: G,
    chain: Chain,
    registry: Arc<RwLock<PauseRegistry>>,
    changes: watch::Sender<()>,
}

impl<G: ComponentPauseGateway> PauseRefresher<G> {
    pub fn new(gateway: G, chain: Chain, registry: Arc<RwLock<PauseRegistry>>) -> Self {
        Self { gateway, chain, registry, changes: watch::Sender::new(()) }
    }

    /// Returns a receiver that is notified after each refresh that changes a paused set.
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.changes.subscribe()
    }

    /// Refreshes the registry every [`REFRESH_INTERVAL`], forever.
    pub async fn run(self) {
        info!(chain = %self.chain, interval = ?REFRESH_INTERVAL, "Started the admin pause refresher");
        let mut ticks = interval_at(Instant::now() + REFRESH_INTERVAL, REFRESH_INTERVAL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            self.refresh().await;
        }
    }

    /// Reads the pause table once and applies it to the registry. On a read error the registry
    /// keeps its state until the next refresh.
    async fn refresh(&self) {
        let pauses = match self
            .gateway
            .get_component_pauses(&self.chain)
            .await
        {
            Ok(pauses) => pauses,
            Err(error) => {
                warn!(%error, "Failed to read admin pauses; keeping the current ones");
                return;
            }
        };
        let changed = self
            .registry
            .write()
            .expect("pause registry lock poisoned")
            .apply(group_by_system(pauses));
        if changed {
            self.changes.send_replace(());
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;
    use tycho_common::models::protocol::PauseRecord;

    use super::*;

    #[expect(
        clippy::allow_attributes,
        reason = "mockall copies these attributes onto several generated items, so an #[expect] would go unfulfilled on some of them"
    )]
    mod gateway_mock {
        use async_trait::async_trait;
        use mockall::mock;
        use tycho_common::models::protocol::{PauseRecord, UnpauseRecord};

        use super::*;

        mock! {
            pub ComponentPauseGateway {}

            #[async_trait]
            impl ComponentPauseGateway for ComponentPauseGateway {
                async fn pause_components(
                    &self,
                    chain: &Chain,
                    system: &str,
                    pauses: &[PauseRecord],
                ) -> Result<Vec<ComponentId>, StorageError>;
                #[allow(
                    clippy::type_complexity,
                    clippy::type_repetition_in_bounds,
                    reason = "signature expanded from `async_trait` inside `mock!` input, which does not accept `expect`"
                )]
                fn unpause_components<'life0, 'life1, 'life2, 'life3, 'life4, 'async_trait>(
                    &'life0 self,
                    chain: &'life1 Chain,
                    component_ids: &'life2 [&'life3 str],
                    unpause: &'life4 UnpauseRecord,
                ) -> ::core::pin::Pin<
                    Box<
                        dyn ::core::future::Future<Output = Result<Vec<ComponentId>, StorageError>>
                            + ::core::marker::Send
                            + 'async_trait,
                    >,
                >
                where
                    'life0: 'async_trait,
                    'life1: 'async_trait,
                    'life2: 'async_trait,
                    'life3: 'async_trait,
                    'life4: 'async_trait,
                    Self: 'async_trait;
                async fn get_component_pauses(
                    &self,
                    chain: &Chain,
                ) -> Result<Vec<ComponentPause>, StorageError>;
            }
        }
    }

    use gateway_mock::MockComponentPauseGateway;

    const EXTRACTED: &str = "uniswap_v2";

    fn pause(system: &str, id: &str) -> ComponentPause {
        ComponentPause {
            protocol_system: system.to_string(),
            record: PauseRecord {
                component_id: id.to_string(),
                reason: None,
                actor: None,
                evidence_url: None,
            },
            paused_at: NaiveDateTime::default(),
        }
    }

    /// A gateway whose reads return `reads` in order.
    fn gateway(reads: Vec<Result<Vec<ComponentPause>, StorageError>>) -> MockComponentPauseGateway {
        let mut gateway = MockComponentPauseGateway::new();
        let mut reads = reads.into_iter();
        gateway
            .expect_get_component_pauses()
            .times(reads.len())
            .returning(move |_| {
                reads
                    .next()
                    .expect("more reads than expected")
            });
        gateway
    }

    /// A refresher whose registry extracts [`EXTRACTED`], was loaded from `first`, and whose
    /// refreshes read `rest`.
    async fn refresher(
        first: Vec<ComponentPause>,
        rest: Vec<Result<Vec<ComponentPause>, StorageError>>,
    ) -> PauseRefresher<MockComponentPauseGateway> {
        let registry = PauseRegistry::load(
            &gateway(vec![Ok(first)]),
            &Chain::Ethereum,
            HashSet::from([EXTRACTED.to_string()]),
        )
        .await
        .expect("load");
        PauseRefresher::new(gateway(rest), Chain::Ethereum, Arc::new(RwLock::new(registry)))
    }

    fn is_paused(
        refresher: &PauseRefresher<MockComponentPauseGateway>,
        system: &str,
        id: &str,
    ) -> bool {
        refresher
            .registry
            .read()
            .expect("lock")
            .is_paused(system, id)
    }

    fn drain_events(
        refresher: &PauseRefresher<MockComponentPauseGateway>,
        system: &str,
    ) -> Vec<PauseEvent> {
        refresher
            .registry
            .write()
            .expect("lock")
            .drain_events(system)
    }

    #[tokio::test]
    async fn test_load() {
        let gateway = gateway(vec![Ok(vec![pause("uniswap_v2", "a"), pause("uniswap_v3", "b")])]);

        let mut registry =
            PauseRegistry::load(&gateway, &Chain::Ethereum, HashSet::from(["uniswap_v2".into()]))
                .await
                .expect("load");

        assert!(registry.is_paused("uniswap_v2", "a"));
        assert!(registry.is_paused("uniswap_v3", "b"));
        assert!(!registry.is_paused("uniswap_v2", "b"));
        assert!(!registry.is_paused("unknown", "a"));
        assert!(registry
            .drain_events("uniswap_v2")
            .is_empty());
        assert!(registry
            .drain_events("uniswap_v3")
            .is_empty());
    }

    #[tokio::test]
    async fn test_load_error() {
        let gateway = gateway(vec![Err(StorageError::Unexpected("db down".to_string()))]);

        let result = PauseRegistry::load(&gateway, &Chain::Ethereum, HashSet::new()).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_refresh_pause_and_unpause() {
        let refresher =
            refresher(vec![pause(EXTRACTED, "a")], vec![Ok(vec![pause(EXTRACTED, "b")])]).await;
        let changes = refresher.subscribe();

        refresher.refresh().await;

        assert!(changes
            .has_changed()
            .expect("sender alive"));
        assert!(!is_paused(&refresher, EXTRACTED, "a"));
        assert!(is_paused(&refresher, EXTRACTED, "b"));
        let events = drain_events(&refresher, EXTRACTED);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events
                .into_iter()
                .collect::<HashSet<_>>(),
            HashSet::from([
                PauseEvent::Unpause("a".to_string()),
                PauseEvent::Pause("b".to_string())
            ])
        );
    }

    #[tokio::test]
    async fn test_refresh_new_system() {
        let refresher = refresher(vec![], vec![Ok(vec![pause(EXTRACTED, "a")])]).await;

        refresher.refresh().await;

        assert!(is_paused(&refresher, EXTRACTED, "a"));
        assert_eq!(drain_events(&refresher, EXTRACTED), vec![PauseEvent::Pause("a".to_string())]);
    }

    #[tokio::test]
    async fn test_refresh_last_pause_of_system_ends() {
        let refresher = refresher(vec![pause(EXTRACTED, "a")], vec![Ok(vec![])]).await;

        refresher.refresh().await;

        assert!(!is_paused(&refresher, EXTRACTED, "a"));
        assert_eq!(drain_events(&refresher, EXTRACTED), vec![PauseEvent::Unpause("a".to_string())]);
    }

    #[tokio::test]
    async fn test_refresh_system_not_extracted() {
        let refresher = refresher(vec![], vec![Ok(vec![pause("uniswap_v3", "a")])]).await;
        let changes = refresher.subscribe();

        refresher.refresh().await;

        assert!(changes
            .has_changed()
            .expect("sender alive"));
        assert!(is_paused(&refresher, "uniswap_v3", "a"));
        assert!(drain_events(&refresher, "uniswap_v3").is_empty());
    }

    #[tokio::test]
    async fn test_refresh_no_change() {
        let refresher =
            refresher(vec![pause(EXTRACTED, "a")], vec![Ok(vec![pause(EXTRACTED, "a")])]).await;
        let changes = refresher.subscribe();

        refresher.refresh().await;

        assert!(!changes
            .has_changed()
            .expect("sender alive"));
        assert!(is_paused(&refresher, EXTRACTED, "a"));
        assert!(drain_events(&refresher, EXTRACTED).is_empty());
    }

    #[tokio::test]
    async fn test_refresh_twice_before_drain() {
        let refresher = refresher(vec![], vec![Ok(vec![pause(EXTRACTED, "a")]), Ok(vec![])]).await;

        refresher.refresh().await;
        refresher.refresh().await;

        assert_eq!(
            drain_events(&refresher, EXTRACTED),
            vec![PauseEvent::Pause("a".to_string()), PauseEvent::Unpause("a".to_string())]
        );
    }

    #[tokio::test]
    async fn test_refresh_read_error() {
        let refresher = refresher(
            vec![pause(EXTRACTED, "a")],
            vec![Err(StorageError::Unexpected("db down".to_string()))],
        )
        .await;
        let changes = refresher.subscribe();

        refresher.refresh().await;

        assert!(!changes
            .has_changed()
            .expect("sender alive"));
        assert!(is_paused(&refresher, EXTRACTED, "a"));
        assert!(drain_events(&refresher, EXTRACTED).is_empty());
    }

    #[tokio::test]
    async fn test_drain_events_twice() {
        let refresher = refresher(vec![], vec![Ok(vec![pause(EXTRACTED, "a")])]).await;
        refresher.refresh().await;
        let mut registry = refresher
            .registry
            .write()
            .expect("lock");

        let first = registry.drain_events(EXTRACTED);
        let second = registry.drain_events(EXTRACTED);

        assert_eq!(first, vec![PauseEvent::Pause("a".to_string())]);
        assert!(second.is_empty());
    }
}
