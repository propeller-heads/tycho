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
    /// Builds the registry from the pauses in effect on `chain`, queuing future events for
    /// `extracted_systems`.
    ///
    /// Queues no events: these pauses started before this process did.
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
