use std::collections::{HashMap, HashSet};

use tracing::{debug, info, instrument, warn};
use tycho_common::models::{
    blockchain::{BlockAggregatedChanges, DCIUpdate},
    protocol::ProtocolComponent,
    Address, Chain, ComponentId, ProtocolSystem,
};

use crate::{
    rpc::{
        ComponentsWithPauses, ProtocolComponentsPaginatedParams, RPCClient, RPC_CLIENT_CONCURRENCY,
    },
    RPCError,
};

#[derive(Clone, Debug)]
pub(crate) enum ComponentFilterVariant {
    Ids(Vec<ComponentId>),
    /// MinimumTVLRange filters components by TVL thresholds:
    /// - `range.0` (remove threshold): components below this are removed from tracking
    /// - `range.1` (add threshold): components above this are added to tracking
    ///
    /// This helps buffer against components that fluctuate on the threshold boundary.
    /// Thresholds are denominated in native token of the chain, for example 1 means 1 ETH on
    /// ethereum.
    MinimumTVLRange {
        range: (f64, f64),
        blocklisted_ids: HashSet<ComponentId>,
    },
}

#[derive(Clone, Debug)]
pub struct ComponentFilter {
    variant: ComponentFilterVariant,
}

impl ComponentFilter {
    /// Creates a `ComponentFilter` that filters components based on a minimum Total Value Locked
    /// (TVL) threshold.
    ///
    /// # Arguments
    ///
    /// * `min_tvl` - The minimum TVL required for a component to be tracked. This is denominated in
    ///   native token of the chain.
    #[expect(
        non_snake_case,
        reason = "named after the enum variant it stands for; renaming the public constructor is a breaking change"
    )]
    #[deprecated(since = "0.9.2", note = "Please use with_tvl_range instead")]
    pub fn MinimumTVL(min_tvl: f64) -> ComponentFilter {
        ComponentFilter {
            variant: ComponentFilterVariant::MinimumTVLRange {
                range: (min_tvl, min_tvl),
                blocklisted_ids: HashSet::new(),
            },
        }
    }

    /// Creates a `ComponentFilter` with a specified TVL range for adding or removing components
    /// from tracking.
    ///
    /// Components that drop below the `remove_tvl_threshold` will be removed from tracking,
    /// while components that exceed the `add_tvl_threshold` will be added to tracking.
    /// This approach helps to reduce fluctuations caused by components hovering around a single
    /// threshold.
    ///
    /// # Arguments
    ///
    /// * `remove_tvl_threshold` - The TVL below which a component will be removed from tracking.
    /// * `add_tvl_threshold` - The TVL above which a component will be added to tracking.
    ///
    /// Note: thresholds are denominated in native token of the chain.
    pub fn with_tvl_range(remove_tvl_threshold: f64, add_tvl_threshold: f64) -> ComponentFilter {
        ComponentFilter {
            variant: ComponentFilterVariant::MinimumTVLRange {
                range: (remove_tvl_threshold, add_tvl_threshold),
                blocklisted_ids: HashSet::new(),
            },
        }
    }

    /// Creates a `ComponentFilter` that **includes only** the components with the specified IDs,
    /// effectively filtering out all other components.
    ///
    /// # Arguments
    ///
    /// * `ids` - A vector of component IDs to include in the filter. Only components with these IDs
    ///   will be tracked.
    #[expect(
        non_snake_case,
        reason = "named after the enum variant it stands for; renaming the public constructor is a breaking change"
    )]
    pub fn Ids(ids: Vec<ComponentId>) -> ComponentFilter {
        ComponentFilter {
            variant: ComponentFilterVariant::Ids(
                ids.into_iter()
                    .map(|id| id.to_lowercase())
                    .collect(),
            ),
        }
    }

    /// Blocklist specific component IDs from tracking regardless of other
    /// filter criteria. IDs are normalized to lowercase.
    ///
    /// Has no effect when the filter variant is `Ids`, since the
    /// inclusion list already defines exactly which components to track.
    pub fn blocklist(mut self, ids: impl IntoIterator<Item = ComponentId>) -> Self {
        match &mut self.variant {
            ComponentFilterVariant::Ids(_) => {
                warn!(
                    "blocklist() has no effect on ComponentFilter::Ids; \
                     remove the component from the ID list instead"
                );
            }
            ComponentFilterVariant::MinimumTVLRange { blocklisted_ids, .. } => {
                blocklisted_ids.extend(
                    ids.into_iter()
                        .map(|id| id.to_lowercase()),
                );
            }
        }
        self
    }

    /// Returns true if the given component ID is blocklisted.
    pub fn is_blocklisted(&self, id: &str) -> bool {
        match &self.variant {
            ComponentFilterVariant::Ids(_) => false,
            ComponentFilterVariant::MinimumTVLRange { blocklisted_ids, .. } => {
                blocklisted_ids.contains(&id.to_lowercase())
            }
        }
    }
}

/// Information about an entrypoint, including which components use it and what contracts it
/// interacts with
#[derive(Default)]
struct EntrypointRelations {
    /// Set of component ids for components that have this entrypoint
    components: HashSet<ComponentId>,
    /// Set of detected contracts for the entrypoint
    contracts: HashSet<Address>,
}

/// Helper struct to determine which components and contracts are being tracked atm.
#[expect(
    clippy::partial_pub_fields,
    reason = "public API; making `components` and `contracts` private or the rest public is a breaking change"
)]
pub struct ComponentTracker<R: RPCClient> {
    chain: Chain,
    protocol_system: ProtocolSystem,
    filter: ComponentFilter,
    /// We will need to request a snapshot for components/contracts that we did not emit as
    /// snapshot for yet but are relevant now, e.g. because min tvl threshold exceeded.
    pub components: HashMap<ComponentId, ProtocolComponent>,
    /// Map of entrypoint id to its associated components and contracts
    entrypoints: HashMap<String, EntrypointRelations>,
    /// Derived from tracked components. We need this if subscribed to a vm extractor because
    /// updates are emitted on a contract level instead of a component level.
    pub contracts: HashSet<Address>,
    /// Ids of the components the server reported as paused and nothing has unpaused since:
    /// neither a block nor the pause check after a restart. The tracker does not track them.
    paused: HashSet<ComponentId>,
    /// Ids of the components a pause or unpause event decided since the stream was last
    /// subscribed. A server answer does not change whether they are paused: the event is at least
    /// as new, and a server process can lag behind the one that sent it.
    decided_by_event: HashSet<ComponentId>,
    /// Client to retrieve necessary protocol components from the rpc.
    rpc_client: R,
}

impl<R> ComponentTracker<R>
where
    R: RPCClient,
{
    pub fn new(chain: Chain, protocol_system: &str, filter: ComponentFilter, rpc: R) -> Self {
        Self {
            chain,
            protocol_system: protocol_system.to_string(),
            filter,
            components: Default::default(),
            contracts: Default::default(),
            rpc_client: rpc,
            entrypoints: Default::default(),
            paused: Default::default(),
            decided_by_event: Default::default(),
        }
    }

    /// Retrieves all components that belong to the system we are streaming that have sufficient
    /// tvl. Also detects which contracts are relevant for simulating on those components.
    pub async fn initialise_components(&mut self) -> Result<(), RPCError> {
        let (component_ids, tvl_gt) = match &self.filter.variant {
            ComponentFilterVariant::Ids(ids) => (Some(ids.clone()), None),
            ComponentFilterVariant::MinimumTVLRange { range: (_, upper_tvl_threshold), .. } => {
                (None, Some(*upper_tvl_threshold))
            }
        };
        let mut paginated_params = ProtocolComponentsPaginatedParams::new(
            self.chain,
            self.protocol_system.as_str(),
            RPC_CLIENT_CONCURRENCY,
        );
        if let Some(ids) = component_ids {
            paginated_params = paginated_params.with_component_ids(ids);
        }
        if let Some(tvl) = tvl_gt {
            paginated_params = paginated_params.with_tvl_gt(tvl);
        }

        let response = self
            .rpc_client
            .get_protocol_components_paginated(paginated_params)
            .await?;
        self.components = self
            .not_paused(response)
            .into_iter()
            .map(|comp| (comp.id.clone(), comp))
            .filter(|(id, _)| !self.filter.is_blocklisted(id))
            .collect::<HashMap<_, _>>();

        self.reinitialize_contracts();

        Ok(())
    }

    /// Initialise the tracked contracts list from tracked components and their entrypoints
    fn reinitialize_contracts(&mut self) {
        // Add contracts from all tracked components
        self.contracts = self
            .components
            .values()
            .flat_map(|comp| comp.contract_addresses.iter().cloned())
            .collect();

        // Add contracts from entrypoints that are linked to tracked components
        let tracked_component_ids = self
            .components
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        for entrypoint in self.entrypoints.values() {
            if !entrypoint
                .components
                .is_disjoint(&tracked_component_ids)
            {
                self.contracts
                    .extend(entrypoint.contracts.iter().cloned());
            }
        }
    }

    /// Update the tracked contracts list with contracts associated with the given components
    pub(crate) fn update_contracts(&mut self, components: Vec<ComponentId>) {
        // Only process components that are actually being tracked.
        let mut tracked_component_ids = HashSet::new();

        // Add contracts from the components
        for comp in components {
            if let Some(component) = self.components.get(&comp) {
                self.contracts.extend(
                    component
                        .contract_addresses
                        .iter()
                        .cloned(),
                );
                tracked_component_ids.insert(comp);
            }
        }

        // Identify entrypoints linked to the given components
        for entrypoint in self.entrypoints.values() {
            if !entrypoint
                .components
                .is_disjoint(&tracked_component_ids)
            {
                self.contracts
                    .extend(entrypoint.contracts.iter().cloned());
            }
        }
    }

    /// Add new components to be tracked
    #[instrument(skip(self, new_components))]
    pub async fn start_tracking(
        &mut self,
        new_components: &[&ComponentId],
    ) -> Result<(), RPCError> {
        let new_components: Vec<_> = new_components
            .iter()
            .filter(|id| !self.filter.is_blocklisted(id))
            .copied()
            .collect();

        if new_components.is_empty() {
            return Ok(());
        }

        let response = self
            .rpc_client
            .get_protocol_components(
                crate::rpc::ProtocolComponentsParams::new(
                    self.chain,
                    self.protocol_system.as_str(),
                )
                .with_component_ids(
                    new_components
                        .into_iter()
                        .cloned()
                        .collect(),
                ),
            )
            .await?
            .into_data();
        let components = self
            .not_paused(response)
            .into_iter()
            .map(|comp| (comp.id.clone(), comp))
            .collect::<HashMap<_, _>>();

        // Update components and contracts
        let component_ids: Vec<_> = components.keys().cloned().collect();
        let component_count = component_ids.len();
        self.components.extend(components);
        self.update_contracts(component_ids);

        debug!(n_components = component_count, "StartedTracking");
        Ok(())
    }

    /// Stop tracking components
    #[instrument(skip(self, to_remove))]
    pub fn stop_tracking<'a, I: IntoIterator<Item = &'a ComponentId> + std::fmt::Debug>(
        &mut self,
        to_remove: I,
    ) -> HashMap<ComponentId, ProtocolComponent> {
        let mut removed_components = HashMap::new();

        for component_id in to_remove {
            if let Some(component) = self.components.remove(component_id) {
                removed_components.insert(component_id.clone(), component);
            }
        }

        // Refresh the tracked contracts list. This is more reliable and efficient than directly
        // removing contracts from the list because some contracts are shared between components.
        self.reinitialize_contracts();

        debug!(n_components = removed_components.len(), "StoppedTracking");
        removed_components
    }

    /// Updates the tracked entrypoints and contracts based on the given DCI data.
    pub fn process_entrypoints(&mut self, dci_update: &DCIUpdate) {
        // Update detected contracts for entrypoints
        for (entrypoint, traces) in &dci_update.trace_results {
            self.entrypoints
                .entry(entrypoint.clone())
                .or_default()
                .contracts
                .extend(traces.accessed_slots.keys().cloned());
        }

        // Update linked components for entrypoints
        for (component, entrypoints) in &dci_update.new_entrypoints {
            for entrypoint in entrypoints {
                let entrypoint_info = self
                    .entrypoints
                    .entry(entrypoint.external_id.clone())
                    .or_default();
                entrypoint_info
                    .components
                    .insert(component.clone());
                // If the component is tracked, add the detected contracts to the tracker
                if self.components.contains_key(component) {
                    self.contracts.extend(
                        entrypoint_info
                            .contracts
                            .iter()
                            .cloned(),
                    );
                }
            }
        }
    }

    /// Get related contracts for the given component ids. Assumes that the components are already
    /// tracked, either by calling `start_tracking` or `initialise_components`.
    ///
    /// # Arguments
    ///
    /// * `ids` - A vector of component IDs to get the contracts for.
    ///
    /// # Returns
    ///
    /// A HashSet of contract IDs. Components that are not tracked will be logged and skipped.
    pub fn get_contracts_by_component<'a, I: IntoIterator<Item = &'a String>>(
        &self,
        ids: I,
    ) -> HashSet<Address> {
        ids.into_iter()
            .filter_map(|cid| {
                if let Some(comp) = self.components.get(cid) {
                    // Collect contracts from all entrypoints linked to this component
                    let dci_contracts: HashSet<Address> = self
                        .entrypoints
                        .values()
                        .filter(|ep| ep.components.contains(cid))
                        .flat_map(|ep| ep.contracts.iter().cloned())
                        .collect();
                    Some(
                        comp.contract_addresses
                            .clone()
                            .into_iter()
                            .chain(dci_contracts)
                            .collect::<HashSet<_>>(),
                    )
                } else {
                    warn!(
                        "Requested component is not tracked: {cid}. Skipping fetching contracts..."
                    );
                    None
                }
            })
            .flatten()
            .collect()
    }

    /// Records the ids a components response reported as paused, so the tracker does not track
    /// them until they are unpaused. Skips the ids a pause or unpause event decided.
    pub(crate) fn record_paused(&mut self, paused: HashSet<ComponentId>) {
        let paused: HashSet<ComponentId> = paused
            .into_iter()
            .filter(|id| !self.decided_by_event.contains(id))
            .collect();
        if !paused.is_empty() {
            self.log_paused(&paused, "Recorded paused components");
        }
        self.paused.extend(paused);
    }

    /// Records the components `response` reported as paused, and returns the components the
    /// tracker does not consider paused.
    fn not_paused(&mut self, response: ComponentsWithPauses) -> Vec<ProtocolComponent> {
        self.record_paused(response.paused_component_ids().clone());
        response
            .into_components()
            .into_iter()
            .filter(|component| !self.paused.contains(&component.id))
            .collect()
    }

    /// Whether the latest pause or unpause event since the stream was last subscribed unpaused
    /// component `id`.
    pub(crate) fn unpaused_by_event(&self, id: &ComponentId) -> bool {
        self.decided_by_event.contains(id) && !self.paused.contains(id)
    }

    /// Forgets which components a pause or unpause event decided, so server answers apply to
    /// them again. Call it on every new subscription, since the events sent between two
    /// subscriptions never reach the client.
    pub(crate) fn forget_pause_events(&mut self) {
        self.decided_by_event.clear();
    }

    /// Asks the server which components are paused now, and records the answer. Call it after
    /// missing blocks, since their pause and unpause events are lost.
    ///
    /// Asks about every tracked component and every component recorded as paused. Returns the
    /// ids that were recorded as paused and no longer are. Keeps what a pause or unpause event
    /// decided.
    pub(crate) async fn refresh_paused(&mut self) -> Result<HashSet<ComponentId>, RPCError> {
        let mut ids: HashSet<ComponentId> = self
            .components
            .keys()
            .cloned()
            .collect();
        ids.extend(self.paused.iter().cloned());
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        let params = ProtocolComponentsPaginatedParams::new(
            self.chain,
            self.protocol_system.as_str(),
            RPC_CLIENT_CONCURRENCY,
        )
        .with_component_ids(ids.into_iter().collect());
        let response = self
            .rpc_client
            .get_protocol_components_paginated(params)
            .await?;
        let paused = response.paused_component_ids().clone();

        let unpaused: HashSet<ComponentId> = response
            .into_components()
            .into_iter()
            .map(|component| component.id)
            .filter(|id| {
                !paused.contains(id) &&
                    self.paused.contains(id) &&
                    !self.decided_by_event.contains(id)
            })
            .collect();
        for id in &unpaused {
            self.paused.remove(id);
        }
        self.record_paused(paused);
        Ok(unpaused)
    }

    /// Returns the components of `unpaused` that are not tracked and pass the filter. With a TVL
    /// filter, asks the server which of them are above the add threshold, and records the ones it
    /// reports as paused.
    pub(crate) async fn unpaused_to_track(
        &mut self,
        unpaused: &HashSet<ComponentId>,
    ) -> Result<Vec<ComponentId>, RPCError> {
        let untracked: Vec<ComponentId> = unpaused
            .iter()
            .filter(|id| !self.components.contains_key(*id) && !self.paused.contains(*id))
            .cloned()
            .collect();
        if untracked.is_empty() {
            return Ok(Vec::new());
        }
        let add_tvl = match &self.filter.variant {
            ComponentFilterVariant::Ids(ids) => {
                return Ok(untracked
                    .into_iter()
                    .filter(|id| ids.contains(&id.to_lowercase()))
                    .collect());
            }
            ComponentFilterVariant::MinimumTVLRange { range: (_, add_tvl), .. } => *add_tvl,
        };
        let params = ProtocolComponentsPaginatedParams::new(
            self.chain,
            self.protocol_system.as_str(),
            RPC_CLIENT_CONCURRENCY,
        )
        .with_component_ids(untracked)
        .with_tvl_gt(add_tvl);
        let response = self
            .rpc_client
            .get_protocol_components_paginated(params)
            .await?;
        Ok(self
            .not_paused(response)
            .into_iter()
            .map(|component| component.id)
            .filter(|id| !self.filter.is_blocklisted(id))
            .collect())
    }

    /// Records the components a block pauses, and removes the ones it unpauses.
    pub(crate) fn record_pause_events(&mut self, deltas: &BlockAggregatedChanges) {
        for id in &deltas.unpaused_components {
            self.paused.remove(id);
        }
        self.paused
            .extend(deltas.paused_components.iter().cloned());
        self.decided_by_event.extend(
            deltas
                .unpaused_components
                .iter()
                .chain(&deltas.paused_components)
                .cloned(),
        );
    }

    /// Removes the paused components from `components`, logs them, and returns their ids.
    pub(crate) fn drop_paused_from(
        &self,
        components: &mut HashMap<ComponentId, ProtocolComponent>,
    ) -> Vec<ComponentId> {
        let dropped: Vec<ComponentId> = components
            .keys()
            .filter(|id| self.paused.contains(*id))
            .cloned()
            .collect();
        if !dropped.is_empty() {
            self.log_paused(&dropped, "Dropping paused components");
        }
        for id in &dropped {
            components.remove(id);
        }
        dropped
    }

    /// Stops tracking the tracked components that are paused, logs them, and returns them.
    pub(crate) fn stop_tracking_paused(&mut self) -> HashMap<ComponentId, ProtocolComponent> {
        let paused: Vec<ComponentId> = self
            .paused
            .iter()
            .filter(|id| self.components.contains_key(*id))
            .cloned()
            .collect();
        if paused.is_empty() {
            return HashMap::new();
        }
        self.log_paused(&paused, "Stopped tracking paused components");
        self.stop_tracking(&paused)
    }

    /// Logs `message` with the paused `components`. With an id filter, the user asked for these
    /// components by id, so it logs at info. With a TVL filter, pauses are routine, so it logs at
    /// debug.
    fn log_paused(&self, components: &impl std::fmt::Debug, message: &str) {
        match &self.filter.variant {
            ComponentFilterVariant::Ids(_) => info!(?components, "{message}"),
            ComponentFilterVariant::MinimumTVLRange { .. } => debug!(?components, "{message}"),
        }
    }

    pub fn get_tracked_component_ids(&self) -> Vec<ComponentId> {
        self.components
            .keys()
            .cloned()
            .collect()
    }

    /// Records the block's pauses and unpauses, then returns the components to start and stop
    /// tracking after the block, as `(to_add, to_remove)`, following the client's filter. With an
    /// id filter, a block only adds back components it unpauses.
    ///
    /// Pauses override the filter. `to_remove` holds every component the block pauses. `to_add`
    /// leaves out every component that is still paused.
    pub fn filter_updated_components(
        &mut self,
        deltas: &BlockAggregatedChanges,
    ) -> (Vec<ComponentId>, Vec<ComponentId>) {
        self.record_pause_events(deltas);
        let (mut to_add, mut to_remove) = match &self.filter.variant {
            ComponentFilterVariant::Ids(ids) => {
                let to_add = deltas
                    .unpaused_components
                    .iter()
                    .filter(|id| ids.contains(&id.to_lowercase()))
                    .cloned()
                    .collect();
                (to_add, Vec::new())
            }
            ComponentFilterVariant::MinimumTVLRange { range: (remove_tvl, add_tvl), .. } => {
                let (mut to_add, mut to_remove): (Vec<_>, Vec<_>) = deltas
                    .component_tvl
                    .iter()
                    .filter(|(_, &tvl)| tvl < *remove_tvl || tvl > *add_tvl)
                    .map(|(id, _)| id.clone())
                    .partition(|id| deltas.component_tvl[id] > *add_tvl);

                // Never add blocklisted components
                to_add.retain(|id| !self.filter.is_blocklisted(id));

                // Remove any currently tracked components that are now blocklisted
                for id in self.components.keys() {
                    if self.filter.is_blocklisted(id) && !to_remove.contains(id) {
                        to_remove.push(id.clone());
                    }
                }

                (to_add, to_remove)
            }
        };

        to_add.retain(|id| !self.paused.contains(id));
        to_remove.retain(|id| !deltas.paused_components.contains(id));
        to_remove.extend(deltas.paused_components.iter().cloned());

        (to_add, to_remove)
    }
}

#[cfg(test)]
mod test {
    use tycho_common::Bytes;

    use super::*;
    use crate::rpc::{none_paused, ComponentsWithPauses, MockRPCClient};

    fn with_mocked_rpc() -> ComponentTracker<MockRPCClient> {
        let rpc = MockRPCClient::new();
        ComponentTracker::new(
            Chain::Ethereum,
            "uniswap-v2",
            ComponentFilter::with_tvl_range(0.0, 0.0),
            rpc,
        )
    }

    fn components_response() -> (Vec<Address>, ProtocolComponent) {
        let contract_addresses = vec![Bytes::from("0x1234"), Bytes::from("0xbabe")];
        let component = ProtocolComponent {
            id: "Component1".to_string(),
            contract_addresses: contract_addresses.clone(),
            ..Default::default()
        };
        (contract_addresses, component)
    }

    #[tokio::test]
    async fn test_initialise_components() {
        let mut tracker = with_mocked_rpc();
        let (contract_ids, model_component) = components_response();
        let exp_component = model_component.clone();
        let model_for_mock = model_component.clone();
        tracker
            .rpc_client
            .expect_get_protocol_components_paginated()
            .returning(move |_| Ok(none_paused(vec![model_for_mock.clone()])));

        tracker
            .initialise_components()
            .await
            .expect("Retrieving components failed");

        assert_eq!(
            tracker
                .components
                .get("Component1")
                .expect("Component1 not tracked"),
            &exp_component
        );
        assert_eq!(tracker.contracts, contract_ids.into_iter().collect());
    }

    #[tokio::test]
    async fn test_start_tracking() {
        let mut tracker = with_mocked_rpc();
        let (contract_ids, model_component) = components_response();
        let exp_contracts = contract_ids.into_iter().collect();
        let component_id = model_component.id.clone();
        let components_arg = [&component_id];
        let model_for_mock = model_component.clone();
        tracker
            .rpc_client
            .expect_get_protocol_components()
            .returning(move |_| {
                Ok(crate::rpc::Page::new(none_paused(vec![model_for_mock.clone()]), 1, 0, 100))
            });

        tracker
            .start_tracking(&components_arg)
            .await
            .expect("Tracking components failed");

        assert_eq!(&tracker.contracts, &exp_contracts);
        assert!(tracker
            .components
            .contains_key("Component1"));
    }

    #[test]
    fn test_stop_tracking() {
        let mut tracker = with_mocked_rpc();
        let (contract_ids, model_component) = components_response();
        tracker
            .components
            .insert("Component1".to_string(), model_component.clone());
        tracker.contracts.extend(contract_ids);
        let components_arg = ["Component1".to_string(), "Component2".to_string()];
        let exp = [("Component1".to_string(), model_component)]
            .into_iter()
            .collect();

        let res = tracker.stop_tracking(&components_arg);

        assert_eq!(res, exp);
        assert!(tracker.contracts.is_empty());
    }

    #[test]
    fn test_get_contracts_by_component() {
        let mut tracker = with_mocked_rpc();
        let (exp_contracts, model_component) = components_response();
        tracker
            .components
            .insert("Component1".to_string(), model_component);
        let components_arg = ["Component1".to_string()];

        let res = tracker.get_contracts_by_component(&components_arg);

        assert_eq!(res, exp_contracts.into_iter().collect());
    }

    #[test]
    fn test_get_tracked_component_ids() {
        let mut tracker = with_mocked_rpc();
        let (_, model_component) = components_response();
        tracker
            .components
            .insert("Component1".to_string(), model_component);
        let exp = vec!["Component1".to_string()];

        let res = tracker.get_tracked_component_ids();

        assert_eq!(res, exp);
    }

    fn with_mocked_rpc_and_blocklist(blocklisted: Vec<&str>) -> ComponentTracker<MockRPCClient> {
        let rpc = MockRPCClient::new();
        let filter = ComponentFilter::with_tvl_range(0.0, 0.0).blocklist(
            blocklisted
                .into_iter()
                .map(String::from),
        );
        ComponentTracker::new(Chain::Ethereum, "uniswap-v2", filter, rpc)
    }

    #[tokio::test]
    async fn test_initialise_skips_blocklisted_components() {
        let mut tracker = with_mocked_rpc_and_blocklist(vec!["component1"]);
        let (_, model_component) = components_response();
        tracker
            .rpc_client
            .expect_get_protocol_components_paginated()
            .returning(move |_| Ok(none_paused(vec![model_component.clone()])));

        tracker
            .initialise_components()
            .await
            .expect("Retrieving components failed");

        assert!(tracker.components.is_empty(), "Blocklisted component should not be in tracker");
    }

    #[tokio::test]
    async fn test_start_tracking_skips_blocklisted() {
        let mut tracker = with_mocked_rpc_and_blocklist(vec!["component1"]);
        let component_id = "Component1".to_string();
        let components_arg = [&component_id];

        tracker
            .start_tracking(&components_arg)
            .await
            .expect("start_tracking should succeed");

        assert!(tracker.components.is_empty(), "Blocklisted component should not be tracked");
    }

    #[test]
    fn test_filter_updated_blocks_blocklisted_add() {
        let mut tracker = with_mocked_rpc_and_blocklist(vec!["blocklisted_pool"]);
        tracker.filter = ComponentFilter::with_tvl_range(5.0, 10.0)
            .blocklist(vec!["blocklisted_pool".to_string()]);

        let deltas = BlockAggregatedChanges {
            component_tvl: HashMap::from([
                ("blocklisted_pool".to_string(), 100.0),
                ("allowed_pool".to_string(), 100.0),
            ]),
            ..Default::default()
        };

        let (to_add, to_remove) = tracker.filter_updated_components(&deltas);
        assert!(
            !to_add.contains(&"blocklisted_pool".to_string()),
            "Blocklisted component should not be in to_add"
        );
        assert!(
            to_add.contains(&"allowed_pool".to_string()),
            "Non-blocklisted component should be in to_add"
        );
        assert!(to_remove.is_empty());
    }

    fn ids_filter() -> ComponentFilter {
        ComponentFilter::Ids(vec!["pool".to_string(), "active".to_string()])
    }

    fn tvl_filter() -> ComponentFilter {
        ComponentFilter::with_tvl_range(5.0, 10.0)
    }

    fn new_tracker(filter: ComponentFilter, tracked: &[&str]) -> ComponentTracker<MockRPCClient> {
        let mut tracker =
            ComponentTracker::new(Chain::Ethereum, "uniswap-v2", filter, MockRPCClient::new());
        for id in tracked {
            let component = ProtocolComponent { id: (*id).to_string(), ..Default::default() };
            tracker
                .components
                .insert((*id).to_string(), component);
        }
        tracker
    }

    fn block_changes(
        paused: &[&str],
        unpaused: &[&str],
        tvl: &[(&str, f64)],
    ) -> BlockAggregatedChanges {
        let ids = |ids: &[&str]| {
            ids.iter()
                .map(|id| (*id).to_string())
                .collect()
        };
        BlockAggregatedChanges {
            paused_components: ids(paused),
            unpaused_components: ids(unpaused),
            component_tvl: tvl
                .iter()
                .map(|(id, tvl)| ((*id).to_string(), *tvl))
                .collect(),
            ..Default::default()
        }
    }

    #[rstest::rstest]
    #[case::ids(ids_filter())]
    #[case::tvl(tvl_filter())]
    fn test_filter_updated_paused(#[case] filter: ComponentFilter) {
        let mut tracker = new_tracker(filter, &["pool"]);

        let (to_add, to_remove) =
            tracker.filter_updated_components(&block_changes(&["pool"], &[], &[]));

        assert!(to_add.is_empty());
        assert_eq!(to_remove, vec!["pool".to_string()]);
    }

    /// "pool" was paused by an earlier block; `changes` unpauses a component.
    #[rstest::rstest]
    #[case::ids(ids_filter(), block_changes(&[], &["pool"], &[]), &["pool"])]
    #[case::mixed_case_id(ids_filter(), block_changes(&[], &["Pool"], &[]), &["Pool"])]
    #[case::not_in_ids(ids_filter(), block_changes(&[], &["other_pool"], &[]), &[])]
    #[case::above_tvl(tvl_filter(), block_changes(&[], &["pool"], &[("pool", 20.0)]), &["pool"])]
    #[case::below_tvl(tvl_filter(), block_changes(&[], &["pool"], &[("pool", 7.0)]), &[])]
    #[case::no_tvl(tvl_filter(), block_changes(&[], &["pool"], &[]), &[])]
    fn test_filter_updated_unpaused(
        #[case] filter: ComponentFilter,
        #[case] changes: BlockAggregatedChanges,
        #[case] expected_add: &[&str],
    ) {
        let mut tracker = new_tracker(filter, &[]);
        tracker.record_pause_events(&block_changes(&["pool"], &[], &[]));

        let (to_add, to_remove) = tracker.filter_updated_components(&changes);

        assert_eq!(to_add, expected_add);
        assert!(to_remove.is_empty());
    }

    #[rstest::rstest]
    #[case::above_add_tvl(20.0)]
    #[case::below_remove_tvl(1.0)]
    fn test_filter_updated_tvl_and_pause_in_one_block(#[case] tvl: f64) {
        let mut tracker = new_tracker(tvl_filter(), &["pool"]);

        let (to_add, to_remove) =
            tracker.filter_updated_components(&block_changes(&["pool"], &[], &[("pool", tvl)]));

        assert!(to_add.is_empty());
        assert_eq!(to_remove, vec!["pool".to_string()]);
    }

    #[test]
    fn test_filter_updated_paused_in_earlier_block() {
        let mut tracker = new_tracker(tvl_filter(), &[]);
        tracker.filter_updated_components(&block_changes(&["pool"], &[], &[]));

        let (to_add, _) =
            tracker.filter_updated_components(&block_changes(&[], &[], &[("pool", 20.0)]));

        assert!(to_add.is_empty());
    }

    #[test]
    fn test_filter_updated_unpaused_in_earlier_block() {
        let mut tracker = new_tracker(tvl_filter(), &[]);
        tracker.filter_updated_components(&block_changes(&["pool"], &[], &[]));
        tracker.filter_updated_components(&block_changes(&[], &["pool"], &[]));

        let (to_add, _) =
            tracker.filter_updated_components(&block_changes(&[], &[], &[("pool", 20.0)]));

        assert_eq!(to_add, vec!["pool".to_string()]);
    }

    #[rstest::rstest]
    #[case::ids(ids_filter())]
    #[case::tvl(tvl_filter())]
    #[tokio::test]
    async fn test_initialise_skips_paused(#[case] filter: ComponentFilter) {
        let mut tracker = new_tracker(filter, &[]);
        let paused = ProtocolComponent { id: "pool".to_string(), ..Default::default() };
        let active = ProtocolComponent { id: "active".to_string(), ..Default::default() };
        tracker
            .rpc_client
            .expect_get_protocol_components_paginated()
            .returning(move |_| {
                Ok(ComponentsWithPauses::new(
                    vec![paused.clone(), active.clone()],
                    HashSet::from(["pool".to_string()]),
                ))
            });

        tracker
            .initialise_components()
            .await
            .expect("initialise components");

        assert_eq!(tracker.get_tracked_component_ids(), vec!["active".to_string()]);
        assert!(tracker.paused.contains("pool"));
    }

    #[tokio::test]
    async fn test_start_tracking_skips_paused() {
        let mut tracker = new_tracker(tvl_filter(), &[]);
        let paused = ProtocolComponent { id: "pool".to_string(), ..Default::default() };
        tracker
            .rpc_client
            .expect_get_protocol_components()
            .returning(move |_| {
                Ok(crate::rpc::Page::new(
                    ComponentsWithPauses::new(
                        vec![paused.clone()],
                        HashSet::from(["pool".to_string()]),
                    ),
                    1,
                    0,
                    100,
                ))
            });
        let pool = "pool".to_string();

        tracker
            .start_tracking(&[&pool])
            .await
            .expect("start tracking");

        assert!(tracker.components.is_empty());
        assert!(tracker.paused.contains("pool"));
    }

    /// With a TVL filter, a paused component is not tracked, so the refresh asks about it only
    /// because it is recorded as paused.
    #[tokio::test]
    async fn test_refresh_paused_untracked_pool() {
        let mut tracker = new_tracker(tvl_filter(), &[]);
        tracker.record_pause_events(&block_changes(&["pool"], &[], &[]));
        let pool = ProtocolComponent { id: "pool".to_string(), ..Default::default() };
        tracker
            .rpc_client
            .expect_get_protocol_components_paginated()
            .returning(move |_| Ok(none_paused(vec![pool.clone()])));

        let unpaused = tracker
            .refresh_paused()
            .await
            .expect("refresh paused");

        assert_eq!(unpaused, HashSet::from(["pool".to_string()]));
        assert!(!tracker.paused.contains("pool"));
    }

    #[rstest::rstest]
    #[case::in_ids(&["Pool"], &[], &["Pool"])]
    #[case::not_in_ids(&["other"], &[], &[])]
    #[case::tracked(&["active"], &["active"], &[])]
    #[tokio::test]
    async fn test_unpaused_to_track_ids(
        #[case] unpaused: &[&str],
        #[case] tracked: &[&str],
        #[case] expected: &[&str],
    ) {
        let mut tracker = new_tracker(ids_filter(), tracked);
        let unpaused = unpaused
            .iter()
            .map(|id| (*id).to_string())
            .collect();

        let to_track = tracker
            .unpaused_to_track(&unpaused)
            .await
            .expect("unpaused to track");

        assert_eq!(to_track, expected);
    }

    #[rstest::rstest]
    #[case::above_add_tvl(false, &["pool"])]
    #[case::still_paused(true, &[])]
    #[tokio::test]
    async fn test_unpaused_to_track_tvl(#[case] server_paused: bool, #[case] expected: &[&str]) {
        let mut tracker = new_tracker(tvl_filter(), &[]);
        let expected_params = ProtocolComponentsPaginatedParams::new(
            Chain::Ethereum,
            "uniswap-v2",
            RPC_CLIENT_CONCURRENCY,
        )
        .with_component_ids(vec!["pool".to_string()])
        .with_tvl_gt(10.0);
        tracker
            .rpc_client
            .expect_get_protocol_components_paginated()
            .withf(move |params| params == &expected_params)
            .returning(move |_| {
                let pool = ProtocolComponent { id: "pool".to_string(), ..Default::default() };
                let paused = if server_paused {
                    HashSet::from(["pool".to_string()])
                } else {
                    HashSet::new()
                };
                Ok(ComponentsWithPauses::new(vec![pool], paused))
            });

        let to_track = tracker
            .unpaused_to_track(&HashSet::from(["pool".to_string()]))
            .await
            .expect("unpaused to track");

        assert_eq!(to_track, expected);
        assert_eq!(tracker.paused.contains("pool"), server_paused);
    }
}
