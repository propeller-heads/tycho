use std::collections::{HashMap, HashSet};

use tracing::{debug, instrument, warn};
use tycho_common::models::{
    blockchain::{BlockAggregatedChanges, DCIUpdate},
    protocol::ProtocolComponent,
    Address, Chain, ComponentId, ProtocolSystem,
};

use crate::{
    rpc::{ProtocolComponentsPaginatedParams, RPCClient, RPC_CLIENT_CONCURRENCY},
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
    #[allow(non_snake_case)] // for backwards compatibility
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
    #[allow(non_snake_case)] // for backwards compatibility
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

        self.components = self
            .rpc_client
            .get_protocol_components_paginated(paginated_params)
            .await?
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

        let components = self
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

    pub fn get_tracked_component_ids(&self) -> Vec<ComponentId> {
        self.components
            .keys()
            .cloned()
            .collect()
    }

    /// Given BlockAggregatedChanges, filter out components that are no longer relevant and return
    /// the components that need to be added or removed.
    pub fn filter_updated_components(
        &self,
        deltas: &BlockAggregatedChanges,
    ) -> (Vec<ComponentId>, Vec<ComponentId>) {
        match &self.filter.variant {
            ComponentFilterVariant::Ids(_) => (Default::default(), Default::default()),
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
        }
    }
}

#[cfg(test)]
mod test {
    use tycho_common::Bytes;

    use super::*;
    use crate::rpc::MockRPCClient;

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
            .returning(move |_| Ok(vec![model_for_mock.clone()]));

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
            .returning(move |_| Ok(crate::rpc::Page::new(vec![model_for_mock.clone()], 1, 0, 100)));

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
            .returning(move |_| Ok(vec![model_component.clone()]));

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
}
