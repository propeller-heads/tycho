//! The state a client holds, folded from the messages it applied.

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    hash::Hash,
};

use crate::{
    dto,
    models::{
        blockchain::{Block, BlockAggregatedChanges, DCIUpdate, EntryPoint, TracingResult},
        contract::{AccountBalance, AccountDelta},
        protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
        token::Token,
        Address, AttrStoreKey, Balance, ChangeType, Code, ComponentId, EntryPointId, StoreKey,
        StoreVal,
    },
    Bytes,
};

/// State a client holds after applying a sequence of full-block and revert messages.
///
/// Every value is the latest one written. Empty bytes count as absent: a revert sends them for a
/// slot or balance that had no value at its target. The fields fall in three groups, which
/// [`DeltaStateModel::check_against`] compares differently:
/// - Compared exactly: `extractor`, `head`, and what reverts restore: components, attributes,
///   component balances, storage slots.
/// - Not undone by reverts: tokens, TVL, DCI entry points and traced contracts. Revert messages
///   carry none of them and the client never removes them, so entries written on an orphaned branch
///   stay. A client that caught up past an orphaned branch it never saw lacks them.
/// - Not reliably restored by the indexer today: native balance and code (never restored) and
///   account token balances (restored only for accounts whose storage changed in the reverted
///   range).
///
/// Two equal models are clients a consumer could not tell apart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeltaStateModel {
    /// Extractor of the applied messages. `None` before the first message.
    pub extractor: Option<String>,
    /// Block of the last applied message: the tip the client believes in. `None` before the
    /// first message.
    pub head: Option<Block>,
    /// Components that exist: created and not deleted since. Stored with `change` set to
    /// `Creation`, since a revert rewrites it.
    pub components: HashMap<ComponentId, ProtocolComponent>,
    /// Attribute values by component id, then attribute name. Dropped with the component.
    pub component_attributes: HashMap<ComponentId, HashMap<AttrStoreKey, Bytes>>,
    /// Token balances by component id, then token address. Dropped with the component.
    pub component_balances: HashMap<ComponentId, HashMap<Address, Balance>>,
    /// Storage slot values by account address, then slot. An account `Creation` replaces the
    /// account's slots, as the consumer does.
    pub account_storage: HashMap<Address, HashMap<StoreKey, StoreVal>>,
    /// Native balance by account address. Not reliably restored by reverts.
    pub account_native_balances: HashMap<Address, Balance>,
    /// Contract code by account address, set by account `Creation` only, as the consumer does.
    /// Not reliably restored by reverts.
    pub account_code: HashMap<Address, Code>,
    /// Token balances held by accounts, by account address, then token address. Not reliably
    /// restored by reverts.
    pub account_token_balances: HashMap<Address, HashMap<Address, Balance>>,
    /// Tokens by address. Not undone by reverts.
    pub tokens: HashMap<Address, Token>,
    /// Latest TVL by component id. Not undone by reverts.
    pub component_tvl: HashMap<ComponentId, f64>,
    /// Entry points linked to each component, by component id. Not undone by reverts.
    pub component_entrypoints: HashMap<ComponentId, HashSet<EntryPointId>>,
    /// Contracts the traces of each entry point accessed, by entry point id. Decides which
    /// contracts' deltas the client keeps. Not undone by reverts.
    pub entrypoint_contracts: HashMap<EntryPointId, HashSet<Address>>,
}

impl DeltaStateModel {
    /// Applies one full-block or revert message the way a client does: every value in the message
    /// overwrites the held one, deletions and empty values remove it, DCI data is added to what
    /// is held, and the head moves to the message's block.
    ///
    /// Components apply after the message's attributes and balances, so a deleted component's
    /// state goes with it. `deleted_protocol_components` applies before `new_protocol_components`,
    /// each component by its `change`.
    ///
    /// # Panics
    ///
    /// On a partial-block message, or one from another extractor or chain than the messages
    /// applied before.
    pub fn apply(&mut self, message: &BlockAggregatedChanges) {
        let BlockAggregatedChanges {
            extractor,
            chain,
            block,
            // The watermarks and the revert flag drive the server's retention; what a client
            // holds does not depend on them.
            finalized_block_height: _,
            db_committed_block_height: _,
            revert: _,
            state_deltas,
            account_deltas,
            new_tokens,
            new_protocol_components,
            deleted_protocol_components,
            component_balances,
            account_balances,
            component_tvl,
            dci_update,
            partial_block_index,
        } = message;
        assert!(
            partial_block_index.is_none(),
            "the model applies full blocks only; block {} is partial {partial_block_index:?}",
            block.number
        );
        if let Some(held) = &self.extractor {
            assert_eq!(held, extractor, "block {} is from another extractor", block.number);
        }
        if let Some(head) = &self.head {
            assert_eq!(head.chain, *chain, "block {} is from another chain", block.number);
        }

        for (component_id, delta) in state_deltas {
            self.apply_state_delta(component_id, delta);
        }
        for (component_id, balances) in component_balances {
            let held = self
                .component_balances
                .entry(component_id.clone())
                .or_default();
            for (token, balance) in balances {
                let ComponentBalance {
                    token: _,
                    balance,
                    balance_float: _,
                    modify_tx: _,
                    component_id: _,
                } = balance;
                write(held, token, balance);
            }
            drop_if_empty(&mut self.component_balances, component_id);
        }
        for (address, delta) in account_deltas {
            self.apply_account_delta(address, delta);
        }
        for (account, balances) in account_balances {
            let held = self
                .account_token_balances
                .entry(account.clone())
                .or_default();
            for (token, balance) in balances {
                let AccountBalance { account: _, token: _, balance, modify_tx: _ } = balance;
                write(held, token, balance);
            }
            drop_if_empty(&mut self.account_token_balances, account);
        }
        for (component_id, component) in deleted_protocol_components
            .iter()
            .chain(new_protocol_components)
        {
            self.apply_component(component_id, component);
        }
        for (address, token) in new_tokens {
            self.tokens
                .insert(address.clone(), token.clone());
        }
        for (component_id, tvl) in component_tvl {
            self.component_tvl
                .insert(component_id.clone(), *tvl);
        }
        self.apply_dci(dci_update);
        self.extractor = Some(extractor.clone());
        self.head = Some(block.clone());
    }

    /// The state after applying `messages` in order to an empty state.
    ///
    /// # Panics
    ///
    /// Like [`DeltaStateModel::apply`].
    pub fn fold<'a>(messages: impl IntoIterator<Item = &'a BlockAggregatedChanges>) -> Self {
        let mut state = Self::default();
        for message in messages {
            state.apply(message);
        }
        state
    }

    /// Checks a client's state against the canonical chain and against a client that applied
    /// every message the server sent.
    ///
    /// `extractor`, `head` and the fields reverts restore must equal `canonical`. Every other
    /// field must hold every entry `canonical` holds, since a missing one is data the chain has
    /// and the client lost, and no entry `connected` lacks, since no message wrote it. TVL and the
    /// fields the indexer does not reliably restore are compared by key only: a value written on
    /// an orphaned branch stays until the key's next update, so it can match neither.
    ///
    /// Account token balances are only required to hold every canonical entry. A revert empties
    /// an orphaned branch's balance only for accounts whose storage changed in its range, so which
    /// of those balances a client keeps depends on the reverts it saw: a connected client can lack
    /// one that a client which caught up past the revert keeps.
    ///
    /// Returns the first rule the state breaks, in field order; within a field, the entry with
    /// the smallest debug form.
    pub fn check_against(&self, canonical: &Self, connected: &Self) -> Result<(), StateMismatch> {
        let Self {
            extractor,
            head,
            components,
            component_attributes,
            component_balances,
            account_storage,
            account_native_balances,
            account_code,
            account_token_balances,
            tokens,
            component_tvl,
            component_entrypoints,
            entrypoint_contracts,
        } = self;
        let Self {
            extractor: canonical_extractor,
            head: canonical_head,
            components: canonical_components,
            component_attributes: canonical_attributes,
            component_balances: canonical_component_balances,
            account_storage: canonical_storage,
            account_native_balances: canonical_native_balances,
            account_code: canonical_code,
            account_token_balances: canonical_token_balances,
            tokens: canonical_tokens,
            component_tvl: canonical_tvl,
            component_entrypoints: canonical_entrypoints,
            entrypoint_contracts: canonical_contracts,
        } = canonical;
        // A connected client's restored fields may hold an abandoned branch after an extractor
        // restart, so only the fields compared by entry are bounded by it.
        let Self {
            extractor: _,
            head: _,
            components: _,
            component_attributes: _,
            component_balances: _,
            account_storage: _,
            account_native_balances: connected_native_balances,
            account_code: connected_code,
            account_token_balances: _,
            tokens: connected_tokens,
            component_tvl: connected_tvl,
            component_entrypoints: connected_entrypoints,
            entrypoint_contracts: connected_contracts,
        } = connected;

        same_value("extractor", extractor, canonical_extractor)?;
        same_value("head", head, canonical_head)?;
        same_entries("components", components, canonical_components)?;
        same_entries(
            "component_attributes",
            &flatten(component_attributes),
            &flatten(canonical_attributes),
        )?;
        same_entries(
            "component_balances",
            &flatten(component_balances),
            &flatten(canonical_component_balances),
        )?;
        same_entries("account_storage", &flatten(account_storage), &flatten(canonical_storage))?;
        bounded(
            "account_native_balances",
            keys(account_native_balances),
            keys(canonical_native_balances),
            keys(connected_native_balances),
        )?;
        bounded("account_code", keys(account_code), keys(canonical_code), keys(connected_code))?;
        covers(
            "account_token_balances",
            &flatten(account_token_balances)
                .into_keys()
                .collect(),
            &flatten(canonical_token_balances)
                .into_keys()
                .collect(),
        )?;
        bounded("tokens", keys(tokens), keys(canonical_tokens), keys(connected_tokens))?;
        bounded("component_tvl", keys(component_tvl), keys(canonical_tvl), keys(connected_tvl))?;
        bounded(
            "component_entrypoints",
            set_pairs(component_entrypoints),
            set_pairs(canonical_entrypoints),
            set_pairs(connected_entrypoints),
        )?;
        bounded(
            "entrypoint_contracts",
            set_pairs(entrypoint_contracts),
            set_pairs(canonical_contracts),
            set_pairs(connected_contracts),
        )
    }

    fn apply_state_delta(
        &mut self,
        component_id: &ComponentId,
        delta: &ProtocolComponentStateDelta,
    ) {
        let ProtocolComponentStateDelta {
            component_id: _,
            updated_attributes,
            deleted_attributes,
            // An indexer-internal hint that the wire does not carry.
            created_attributes: _,
        } = delta;
        let held = self
            .component_attributes
            .entry(component_id.clone())
            .or_default();
        for (name, value) in updated_attributes {
            write(held, name, value);
        }
        for name in deleted_attributes {
            held.remove(name);
        }
        drop_if_empty(&mut self.component_attributes, component_id);
    }

    fn apply_account_delta(&mut self, address: &Address, delta: &AccountDelta) {
        let AccountDelta { chain: _, address: _, slots, balance, code, change } = delta;
        match change {
            ChangeType::Creation => {
                let mut storage = HashMap::new();
                for (slot, value) in slots {
                    if let Some(value) = value {
                        write(&mut storage, slot, value);
                    }
                }
                replace(&mut self.account_storage, address, storage);
                replace_bytes(&mut self.account_code, address, code.as_ref());
                replace_bytes(&mut self.account_native_balances, address, balance.as_ref());
            }
            ChangeType::Update => {
                let storage = self
                    .account_storage
                    .entry(address.clone())
                    .or_default();
                for (slot, value) in slots {
                    match value {
                        Some(value) => write(storage, slot, value),
                        None => {
                            storage.remove(slot);
                        }
                    }
                }
                drop_if_empty(&mut self.account_storage, address);
                if let Some(balance) = balance {
                    write(&mut self.account_native_balances, address, balance);
                }
                // The consumer takes code from an account creation only.
            }
            ChangeType::Deletion => {
                self.account_storage.remove(address);
                self.account_native_balances
                    .remove(address);
                self.account_code.remove(address);
            }
        }
    }

    fn apply_component(&mut self, component_id: &ComponentId, component: &ProtocolComponent) {
        match component.change {
            ChangeType::Creation | ChangeType::Update => {
                let stored =
                    ProtocolComponent { change: ChangeType::Creation, ..component.clone() };
                self.components
                    .insert(component_id.clone(), stored);
            }
            ChangeType::Deletion => {
                self.components.remove(component_id);
                self.component_attributes
                    .remove(component_id);
                self.component_balances
                    .remove(component_id);
            }
        }
    }

    /// Adds DCI data like `ComponentTracker::process_entrypoints` does.
    fn apply_dci(&mut self, update: &DCIUpdate) {
        let DCIUpdate {
            new_entrypoints,
            // Tracing parameters decide what the server traces, not what a client keeps.
            new_entrypoint_params: _,
            trace_results,
        } = update;
        for (component_id, entrypoints) in new_entrypoints {
            for EntryPoint { external_id, target: _, signature: _ } in entrypoints {
                self.component_entrypoints
                    .entry(component_id.clone())
                    .or_default()
                    .insert(external_id.clone());
            }
        }
        for (entrypoint_id, TracingResult { retriggers: _, accessed_slots }) in trace_results {
            for contract in accessed_slots.keys() {
                self.entrypoint_contracts
                    .entry(entrypoint_id.clone())
                    .or_default()
                    .insert(contract.clone());
            }
        }
    }
}

/// Writes `value` under `key`, or removes the key when `value` is empty: empty bytes count as
/// absent.
fn write<K: Eq + Hash + Clone>(map: &mut HashMap<K, Bytes>, key: &K, value: &Bytes) {
    if value.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.clone(), value.clone());
    }
}

/// Sets `key` to `value`, or removes it when `value` is absent or empty.
fn replace_bytes(map: &mut HashMap<Address, Bytes>, key: &Address, value: Option<&Bytes>) {
    match value {
        Some(value) => write(map, key, value),
        None => {
            map.remove(key);
        }
    }
}

/// Sets `key` to `inner`, or removes it when `inner` is empty.
fn replace<K: Eq + Hash + Clone, IK, IV>(
    map: &mut HashMap<K, HashMap<IK, IV>>,
    key: &K,
    inner: HashMap<IK, IV>,
) {
    if inner.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.clone(), inner);
    }
}

/// Removes `key` when its inner map is empty, so an empty inner map never differs from a missing
/// one.
fn drop_if_empty<K: Eq + Hash, IK, IV>(map: &mut HashMap<K, HashMap<IK, IV>>, key: &K) {
    if map
        .get(key)
        .is_some_and(HashMap::is_empty)
    {
        map.remove(key);
    }
}

/// Checks that two values are equal.
fn same_value<T: PartialEq + Debug>(
    field: &'static str,
    own: &T,
    canonical: &T,
) -> Result<(), StateMismatch> {
    if own == canonical {
        return Ok(());
    }
    Err(StateMismatch::Differs { field, entry: format!("{own:?}, canonical {canonical:?}") })
}

/// Checks that two maps hold equal values under the same keys.
fn same_entries<K: Eq + Hash + Debug, V: PartialEq>(
    field: &'static str,
    own: &HashMap<K, V>,
    canonical: &HashMap<K, V>,
) -> Result<(), StateMismatch> {
    let differing = own
        .keys()
        .chain(canonical.keys())
        .filter(|key| own.get(*key) != canonical.get(*key));
    match smallest(differing) {
        Some(entry) => Err(StateMismatch::Differs { field, entry }),
        None => Ok(()),
    }
}

/// Checks `canonical ⊆ own ⊆ connected`.
fn bounded<T: Eq + Hash + Debug>(
    field: &'static str,
    own: HashSet<T>,
    canonical: HashSet<T>,
    connected: HashSet<T>,
) -> Result<(), StateMismatch> {
    covers(field, &own, &canonical)?;
    match smallest(own.difference(&connected)) {
        Some(entry) => Err(StateMismatch::Unexpected { field, entry }),
        None => Ok(()),
    }
}

/// Checks `canonical ⊆ own`.
fn covers<T: Eq + Hash + Debug>(
    field: &'static str,
    own: &HashSet<T>,
    canonical: &HashSet<T>,
) -> Result<(), StateMismatch> {
    match smallest(canonical.difference(own)) {
        Some(entry) => Err(StateMismatch::Missing { field, entry }),
        None => Ok(()),
    }
}

/// The entry with the smallest debug form, so a mismatch names the same entry on every run.
fn smallest<'a, T: Debug + 'a>(entries: impl Iterator<Item = &'a T>) -> Option<String> {
    entries
        .map(|entry| format!("{entry:?}"))
        .min()
}

fn keys<K: Eq + Hash, V>(map: &HashMap<K, V>) -> HashSet<&K> {
    map.keys().collect()
}

/// The values of a map of maps, keyed by both keys.
fn flatten<K: Eq + Hash, IK: Eq + Hash, IV>(
    map: &HashMap<K, HashMap<IK, IV>>,
) -> HashMap<(&K, &IK), &IV> {
    let mut flat = HashMap::new();
    for (key, inner) in map {
        for (inner_key, value) in inner {
            flat.insert((key, inner_key), value);
        }
    }
    flat
}

fn set_pairs<K: Eq + Hash, IK: Eq + Hash>(map: &HashMap<K, HashSet<IK>>) -> HashSet<(&K, &IK)> {
    let mut pairs = HashSet::new();
    for (key, inner) in map {
        for inner_key in inner {
            pairs.insert((key, inner_key));
        }
    }
    pairs
}

/// The rule a client's state breaks in [`DeltaStateModel::check_against`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateMismatch {
    /// `extractor`, `head` or a field reverts restore differs from the canonical state.
    Differs {
        /// Name of the model field.
        field: &'static str,
        /// The smallest key whose value differs, by its debug form, for the assertion message.
        /// For `extractor` and `head`, the client's value and the canonical one.
        entry: String,
    },
    /// A field compared by entry lacks an entry the canonical state holds.
    Missing {
        /// Name of the model field.
        field: &'static str,
        /// The missing entry, for the assertion message.
        entry: String,
    },
    /// A field compared by entry holds an entry no message the server sent wrote.
    Unexpected {
        /// Name of the model field.
        field: &'static str,
        /// The unexpected entry, for the assertion message.
        entry: String,
    },
}

/// `message` as a client receives it: converted to the WebSocket DTO and back, so it carries
/// only what the wire carries (no committed height, no `created_attributes`, slots as values).
pub fn through_the_wire(message: &BlockAggregatedChanges) -> BlockAggregatedChanges {
    BlockAggregatedChanges::from(dto::BlockAggregatedChanges::from(message.clone()))
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use crate::testing::delta_history::{fixtures::*, CHAIN};

    #[test]
    fn dci_from_an_orphaned_branch_survives_the_revert() {
        // Block 3 links "ep" to "c" and is orphaned by a revert to 2; 3' carries no DCI.
        let canonical = [full_block(1, 0, 0), full_block(2, 0, 0), full_block(3, 1, 0)];
        let orphaned = block_with_dci(3, 0, 0, "c", "ep", byte_address(1));
        let ws = [
            canonical[0].clone(),
            canonical[1].clone(),
            orphaned,
            revert_to(2, 0, 0),
            canonical[2].clone(),
        ];

        let connected = DeltaStateModel::fold(ws.iter().map(Arc::as_ref));
        let canonical = DeltaStateModel::fold(canonical.iter().map(Arc::as_ref));

        assert_eq!(
            connected.component_entrypoints,
            HashMap::from([("c".to_string(), HashSet::from(["ep".to_string()]))])
        );
        assert!(canonical
            .component_entrypoints
            .is_empty());
        assert_eq!(connected.check_against(&canonical, &connected), Ok(()));
        // A client that caught up from 2 never saw block 3 and lacks its DCI data.
        assert_eq!(canonical.check_against(&canonical, &connected), Ok(()));
    }

    #[test]
    fn an_empty_value_counts_as_absent() {
        // Orphaned block 2 writes a slot that did not exist at 1; the revert sends it back empty.
        let slot_delta = |value: Bytes| {
            HashMap::from([(
                byte_address(1),
                AccountDelta::new(
                    CHAIN,
                    byte_address(1),
                    HashMap::from([(Bytes::from(vec![1]), Some(value))]),
                    None,
                    None,
                    ChangeType::Update,
                ),
            )])
        };
        let written = Arc::new(BlockAggregatedChanges {
            account_deltas: slot_delta(Bytes::from(vec![7])),
            ..(*full_block(2, 0, 0)).clone()
        });
        let restored = Arc::new(BlockAggregatedChanges {
            account_deltas: slot_delta(Bytes::new()),
            ..(*revert_to(1, 0, 0)).clone()
        });
        let ws = [full_block(1, 0, 0), written, restored];

        let connected = DeltaStateModel::fold(ws.iter().map(Arc::as_ref));
        let canonical = DeltaStateModel::fold([ws[0].as_ref()]);

        assert_eq!(connected.check_against(&canonical, &connected), Ok(()));
    }

    #[test]
    fn deleting_a_component_drops_its_state() {
        let component = ProtocolComponent {
            id: "c".to_string(),
            change: ChangeType::Creation,
            ..Default::default()
        };
        let created = Arc::new(BlockAggregatedChanges {
            new_protocol_components: HashMap::from([("c".to_string(), component.clone())]),
            state_deltas: HashMap::from([(
                "c".to_string(),
                ProtocolComponentStateDelta::new(
                    "c",
                    HashMap::from([("fee".to_string(), Bytes::from(vec![1]))]),
                    HashSet::new(),
                ),
            )]),
            component_balances: HashMap::from([(
                "c".to_string(),
                HashMap::from([(byte_address(9), component_balance("c", &byte_address(9), 1))]),
            )]),
            ..(*full_block(1, 0, 0)).clone()
        });
        // Normal blocks report a deletion in `new_protocol_components`.
        let deleted = Arc::new(BlockAggregatedChanges {
            new_protocol_components: HashMap::from([(
                "c".to_string(),
                ProtocolComponent { change: ChangeType::Deletion, ..component },
            )]),
            ..(*full_block(2, 0, 0)).clone()
        });

        let state = DeltaStateModel::fold([created.as_ref(), deleted.as_ref()]);

        assert!(state.components.is_empty());
        assert!(state.component_attributes.is_empty());
        assert!(state.component_balances.is_empty());
    }

    #[test]
    fn an_account_creation_replaces_the_account() {
        let account = |slots: HashMap<Bytes, Option<Bytes>>, change: ChangeType| {
            HashMap::from([(
                byte_address(1),
                AccountDelta::new(
                    CHAIN,
                    byte_address(1),
                    slots,
                    Some(one(1)),
                    Some(one(2)),
                    change,
                ),
            )])
        };
        let updated = Arc::new(BlockAggregatedChanges {
            account_deltas: account(HashMap::from([(one(1), Some(one(1)))]), ChangeType::Update),
            ..(*full_block(1, 0, 0)).clone()
        });
        let created = Arc::new(BlockAggregatedChanges {
            account_deltas: account(HashMap::from([(one(2), Some(one(2)))]), ChangeType::Creation),
            ..(*full_block(2, 0, 0)).clone()
        });

        let state = DeltaStateModel::fold([updated.as_ref(), created.as_ref()]);

        assert_eq!(
            state.account_storage,
            HashMap::from([(byte_address(1), HashMap::from([(one(2), one(2))]))])
        );
        assert_eq!(state.account_code, HashMap::from([(byte_address(1), one(2))]));
    }

    #[test]
    #[should_panic(expected = "full blocks only")]
    fn applying_a_partial_block_panics() {
        let partial = BlockAggregatedChanges {
            partial_block_index: Some(0),
            ..(*full_block(1, 0, 0)).clone()
        };

        DeltaStateModel::default().apply(&partial);
    }

    #[test]
    #[should_panic(expected = "another extractor")]
    fn applying_a_block_of_another_extractor_panics() {
        let other = BlockAggregatedChanges {
            extractor: "other".to_string(),
            ..(*full_block(2, 0, 0)).clone()
        };

        DeltaStateModel::fold([full_block(1, 0, 0).as_ref(), &other]);
    }

    #[test]
    fn a_client_missing_canonical_dci_is_rejected() {
        let blocks = [full_block(1, 0, 0), block_with_dci(2, 0, 0, "c", "ep", byte_address(1))];
        let canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        let mut client = canonical.clone();
        client.component_entrypoints.clear();

        let result = client.check_against(&canonical, &canonical);

        assert!(
            matches!(result, Err(StateMismatch::Missing { field: "component_entrypoints", .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_client_with_dci_no_message_wrote_is_rejected() {
        let blocks = [full_block(1, 0, 0), block_with_dci(2, 0, 0, "c", "ep", byte_address(1))];
        let canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        let mut client = canonical.clone();
        client
            .entrypoint_contracts
            .entry("ep".to_string())
            .or_default()
            .insert(byte_address(2));

        let result = client.check_against(&canonical, &canonical);

        assert!(
            matches!(result, Err(StateMismatch::Unexpected { field: "entrypoint_contracts", .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_client_with_a_different_restored_value_is_rejected() {
        let blocks = [full_block(1, 0, 0), full_block(2, 0, 0)];
        let canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        let mut client = canonical.clone();
        client
            .account_storage
            .insert(byte_address(1), HashMap::from([(Bytes::from(vec![1]), Bytes::from(vec![1]))]));

        let result = client.check_against(&canonical, &canonical);

        let entry = format!("{:?}", (&byte_address(1), &Bytes::from(vec![1])));
        assert_eq!(result, Err(StateMismatch::Differs { field: "account_storage", entry }));
    }

    #[test]
    fn a_client_at_another_block_is_rejected() {
        let blocks = [full_block(1, 0, 0), full_block(2, 0, 0)];
        let canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        let client = DeltaStateModel::fold([blocks[0].as_ref()]);

        let result = client.check_against(&canonical, &canonical);

        let entry = format!("{:?}, canonical {:?}", Some(&blocks[0].block), Some(&blocks[1].block));
        assert_eq!(result, Err(StateMismatch::Differs { field: "head", entry }));
    }

    #[test]
    fn a_native_balance_is_compared_by_account_only() {
        // Reverts don't restore native balances, so a value written on an orphaned branch stays.
        let blocks = [full_block(1, 0, 0), full_block(2, 0, 0)];
        let mut canonical = DeltaStateModel::fold(blocks.iter().map(Arc::as_ref));
        canonical
            .account_native_balances
            .insert(byte_address(1), Bytes::from(vec![1]));
        let mut client = canonical.clone();
        client
            .account_native_balances
            .insert(byte_address(1), Bytes::from(vec![2]));

        assert_eq!(client.check_against(&canonical, &client), Ok(()));
    }
}
