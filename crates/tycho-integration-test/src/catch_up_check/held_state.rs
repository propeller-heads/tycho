//! The protocol state a consumer of one extractor's feed holds, and how it differs from a
//! snapshot of the same components at the same block.
//!
//! Empty values count as absent, as do zero storage values and balances: a revert sends empty
//! bytes for a key that had no value before the reverted blocks, and EVM storage and balances do
//! not tell zero from unset.
//!
//! TVL, tokens and DCI entry points are not held. The RPC serves TVL only for the latest block, and
//! the DCI data shows in the contracts the client holds.
//!
//! Differences in contracts that only DCI traces reach are known gaps, never mismatches. Which of
//! them a client holds depends on the reorgs it saw, since reverts carry no DCI data, and their
//! storage can differ because `/v1/contract_state` does not filter by protocol system: a snapshot
//! includes the slots other extractors write, while the client only receives this extractor's
//! deltas. So a catch-up bug in the storage of such a contract cannot show as a mismatch.

use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Display, Formatter},
    hash::Hash,
};

use tycho_client::feed::{
    synchronizer::{ComponentWithState, Snapshot, StateSyncMessage},
    BlockHeader,
};
use tycho_common::{
    models::{
        blockchain::BlockAggregatedChanges,
        contract::{Account, AccountBalance, AccountDelta},
        protocol::{
            ComponentBalance, ProtocolComponent, ProtocolComponentState,
            ProtocolComponentStateDelta,
        },
        ChangeType,
    },
    Bytes,
};

/// Values longer than this are shortened in log lines; contract code runs to kilobytes.
const MAX_SHOWN_BYTES: usize = 32;

/// The state of one tracked component.
#[derive(Clone, Debug, PartialEq)]
struct HeldComponent {
    component: ProtocolComponent,
    attributes: HashMap<String, Bytes>,
    balances: HashMap<Bytes, Bytes>,
}

impl HeldComponent {
    fn from_snapshot(snapshot: &ComponentWithState) -> Self {
        let ComponentWithState { state, component, component_tvl: _, entrypoints: _ } = snapshot;
        let ProtocolComponentState { component_id: _, attributes, balances } = state;
        let mut held = Self {
            component: component.clone(),
            attributes: HashMap::new(),
            balances: HashMap::new(),
        };
        for (name, value) in attributes {
            write_non_empty(&mut held.attributes, name, value);
        }
        for (token, balance) in balances {
            write_non_zero(&mut held.balances, token, balance);
        }
        held
    }
}

/// The state of one contract.
#[derive(Clone, Debug, Default, PartialEq)]
struct HeldAccount {
    slots: HashMap<Bytes, Bytes>,
    native_balance: Option<Bytes>,
    code: Option<Bytes>,
    token_balances: HashMap<Bytes, Bytes>,
}

impl HeldAccount {
    fn from_snapshot(account: &Account) -> Self {
        let Account {
            chain: _,
            address: _,
            title: _,
            slots,
            native_balance,
            token_balances,
            code,
            code_hash: _,
            balance_modify_tx: _,
            code_modify_tx: _,
            creation_tx: _,
        } = account;
        let mut held = Self {
            native_balance: non_zero(Some(native_balance)),
            code: non_empty(Some(code)),
            ..Self::default()
        };
        for (slot, value) in slots {
            write_non_zero(&mut held.slots, slot, value);
        }
        for (token, balance) in token_balances {
            let AccountBalance { account: _, token: _, balance, modify_tx: _ } = balance;
            write_non_zero(&mut held.token_balances, token, balance);
        }
        held
    }
}

/// The state a consumer of one extractor's feed holds: its tracked components and the contracts
/// it received.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtractorState {
    components: HashMap<String, HeldComponent>,
    accounts: HashMap<Bytes, HeldAccount>,
}

impl ExtractorState {
    /// The state a snapshot describes.
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        let mut state = Self::default();
        state.apply_snapshot(snapshot);
        state
    }

    /// Applies one message of the feed the way a consumer does: removed components go, snapshots
    /// replace what is held for their components and contracts, then the deltas apply.
    ///
    /// A component the message both removes and carries a snapshot of counts as removed. Deltas
    /// for components the state does not hold are ignored, since a consumer has no state to apply
    /// them to. An account `Creation` replaces the contract's storage, native balance and code;
    /// its code is otherwise only taken from snapshots, as consumers do.
    pub fn apply(&mut self, message: &StateSyncMessage<BlockHeader>) {
        let StateSyncMessage { header: _, snapshots, deltas, removed_components } = message;
        for component_id in removed_components.keys() {
            self.components.remove(component_id);
        }
        let Snapshot { states, vm_storage } = snapshots;
        for (component_id, state) in states {
            // The client can list a component as both a snapshot and a removal in one message
            // (deferred bug 20). Holding it would keep a component the client no longer updates,
            // which would differ from every later snapshot.
            if removed_components.contains_key(component_id) {
                continue;
            }
            self.components
                .insert(component_id.clone(), HeldComponent::from_snapshot(state));
        }
        self.apply_accounts(vm_storage);
        if let Some(deltas) = deltas {
            self.apply_deltas(deltas);
        }
    }

    /// Whether the state holds component `component_id`.
    pub fn holds_component(&self, component_id: &str) -> bool {
        self.components
            .contains_key(component_id)
    }

    /// Whether the state holds no component.
    pub fn has_no_components(&self) -> bool {
        self.components.is_empty()
    }

    /// The held components, by id.
    pub fn components(&self) -> HashMap<String, ProtocolComponent> {
        let mut components = HashMap::new();
        for (component_id, held) in &self.components {
            components.insert(component_id.clone(), held.component.clone());
        }
        components
    }

    /// The contracts the held components name in `contract_addresses`.
    pub fn component_contracts(&self) -> HashSet<Bytes> {
        let mut contracts = HashSet::new();
        for held in self.components.values() {
            contracts.extend(
                held.component
                    .contract_addresses
                    .iter()
                    .cloned(),
            );
        }
        contracts
    }

    fn apply_snapshot(&mut self, snapshot: &Snapshot) {
        let Snapshot { states, vm_storage } = snapshot;
        for (component_id, state) in states {
            self.components
                .insert(component_id.clone(), HeldComponent::from_snapshot(state));
        }
        self.apply_accounts(vm_storage);
    }

    fn apply_accounts(&mut self, vm_storage: &HashMap<Bytes, Account>) {
        for (address, account) in vm_storage {
            self.accounts
                .insert(address.clone(), HeldAccount::from_snapshot(account));
        }
    }

    fn apply_deltas(&mut self, deltas: &BlockAggregatedChanges) {
        let BlockAggregatedChanges {
            extractor: _,
            chain: _,
            block: _,
            finalized_block_height: _,
            db_committed_block_height: _,
            revert: _,
            partial_block_index: _,
            state_deltas,
            account_deltas,
            component_balances,
            account_balances,
            new_protocol_components,
            deleted_protocol_components,
            // Not held; see the module docs.
            new_tokens: _,
            component_tvl: _,
            dci_update: _,
        } = deltas;
        for (component_id, delta) in state_deltas {
            let Some(held) = self.components.get_mut(component_id) else {
                continue;
            };
            let ProtocolComponentStateDelta {
                component_id: _,
                updated_attributes,
                deleted_attributes,
                created_attributes: _,
            } = delta;
            for (name, value) in updated_attributes {
                write_non_empty(&mut held.attributes, name, value);
            }
            for name in deleted_attributes {
                held.attributes.remove(name);
            }
        }
        for (component_id, balances) in component_balances {
            let Some(held) = self.components.get_mut(component_id) else {
                continue;
            };
            for (token, balance) in balances {
                let ComponentBalance {
                    token: _,
                    balance,
                    balance_float: _,
                    modify_tx: _,
                    component_id: _,
                } = balance;
                write_non_zero(&mut held.balances, token, balance);
            }
        }
        for (address, delta) in account_deltas {
            self.apply_account_delta(address, delta);
        }
        for (address, balances) in account_balances {
            let Some(held) = self.accounts.get_mut(address) else {
                continue;
            };
            for (token, balance) in balances {
                let AccountBalance { account: _, token: _, balance, modify_tx: _ } = balance;
                write_non_zero(&mut held.token_balances, token, balance);
            }
        }
        for (component_id, component) in deleted_protocol_components
            .iter()
            .chain(new_protocol_components)
        {
            match component.change {
                ChangeType::Deletion => {
                    self.components.remove(component_id);
                }
                // Deltas name every new component, tracked or not; a consumer only holds the
                // ones it received a snapshot for.
                ChangeType::Creation | ChangeType::Update => {}
            }
        }
    }

    fn apply_account_delta(&mut self, address: &Bytes, delta: &AccountDelta) {
        // `code` and `change` are private to tycho-common; they are read through their getters.
        let AccountDelta { chain: _, address: _, slots, balance, .. } = delta;
        match delta.change_type() {
            ChangeType::Creation => {
                let token_balances = self
                    .accounts
                    .remove(address)
                    .map(|held| held.token_balances)
                    .unwrap_or_default();
                let mut held = HeldAccount {
                    native_balance: non_zero(balance.as_ref()),
                    code: non_empty(delta.code().as_ref()),
                    token_balances,
                    ..HeldAccount::default()
                };
                for (slot, slot_value) in slots {
                    if let Some(slot_value) = slot_value {
                        write_non_zero(&mut held.slots, slot, slot_value);
                    }
                }
                self.accounts
                    .insert(address.clone(), held);
            }
            ChangeType::Update => {
                let held = self
                    .accounts
                    .entry(address.clone())
                    .or_default();
                for (slot, slot_value) in slots {
                    match slot_value {
                        Some(slot_value) => write_non_zero(&mut held.slots, slot, slot_value),
                        None => {
                            held.slots.remove(slot);
                        }
                    }
                }
                if let Some(balance) = balance {
                    held.native_balance = non_zero(Some(balance));
                }
            }
            ChangeType::Deletion => {
                self.accounts.remove(address);
            }
        }
    }
}

/// A part of the state a comparison checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    /// A tracked component itself.
    Component,
    Attribute,
    ComponentBalance,
    /// A contract a tracked component names in `contract_addresses`, itself.
    Account,
    Storage,
    NativeBalance,
    Code,
    AccountTokenBalance,
    /// Any part of a contract that only the DCI traces of tracked components reach.
    DciAccount,
}

impl Field {
    /// Whether a new difference in this field fails the comparison: [`Status::Mismatch`] or
    /// [`Status::KnownGap`].
    pub fn status(self) -> Status {
        match self {
            Field::Component |
            Field::Attribute |
            Field::ComponentBalance |
            Field::Account |
            Field::Storage => Status::Mismatch,
            // Reverts don't restore native balances, code or most account token balances, so a
            // client's values here depend on the reorgs it saw.
            Field::NativeBalance | Field::Code | Field::AccountTokenBalance => Status::KnownGap,
            // Reverts carry no DCI data, so which of these contracts a client holds depends on the
            // reorgs it saw. Their storage can differ too: `/v1/contract_state` does not filter by
            // protocol system, so a snapshot includes slots other extractors write.
            Field::DciAccount => Status::KnownGap,
        }
    }

    /// The field's name in metrics and logs.
    pub fn label(self) -> &'static str {
        match self {
            Field::Component => "component",
            Field::Attribute => "attribute",
            Field::ComponentBalance => "component_balance",
            Field::Account => "account",
            Field::Storage => "storage",
            Field::NativeBalance => "native_balance",
            Field::Code => "code",
            Field::AccountTokenBalance => "account_token_balance",
            Field::DciAccount => "dci_account",
        }
    }
}

/// Whether a difference fails the comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    /// The client must match the snapshot here: reverts restore this field.
    Mismatch,
    /// The server does not restore this field on reverts yet, or it is in a contract only DCI
    /// traces reach, so the client may differ.
    KnownGap,
    /// The state held when the gap was forced already differed from its snapshot in exactly this
    /// way, so the difference does not come from the gap.
    PreExisting,
}

impl Status {
    /// The status's name in metrics and logs.
    pub fn label(self) -> &'static str {
        match self {
            Status::Mismatch => "mismatch",
            Status::KnownGap => "known_gap",
            Status::PreExisting => "pre_existing",
        }
    }
}

/// One entry in which the state a client holds differs from a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Difference {
    pub field: Field,
    /// The component id or contract address the difference is in.
    pub entity: String,
    pub kind: DifferenceKind,
}

/// How the client's state and the snapshot differ in one entry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DifferenceKind {
    /// Only the client holds the entity.
    OnlyHeld,
    /// Only the snapshot has the entity.
    OnlyInSnapshot,
    /// The entity's value under `key` (an attribute name, token, slot, `native_balance` or
    /// `code`) differs. `None` is an absent value.
    Value { key: String, held: Option<Bytes>, snapshot: Option<Bytes> },
}

impl Display for Difference {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let Self { field, entity, kind } = self;
        let field = field.label();
        match kind {
            DifferenceKind::OnlyHeld => write!(f, "{field} {entity}: held, not in the snapshot"),
            DifferenceKind::OnlyInSnapshot => {
                write!(f, "{field} {entity}: in the snapshot, not held")
            }
            DifferenceKind::Value { key, held, snapshot } => write!(
                f,
                "{field} {entity} {key}: held {}, snapshot {}",
                ShownValue(held.as_ref()),
                ShownValue(snapshot.as_ref())
            ),
        }
    }
}

/// Displays a value for a log line, shortened when long.
struct ShownValue<'a>(Option<&'a Bytes>);

impl Display for ShownValue<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let Some(value) = self.0 else {
            return write!(f, "absent");
        };
        if value.len() <= MAX_SHOWN_BYTES {
            return write!(f, "{value}");
        }
        let shown = Bytes::from(value[..MAX_SHOWN_BYTES].to_vec());
        write!(f, "{shown}... ({} bytes)", value.len())
    }
}

/// Lists every entry in which `held` differs from `snapshot`, sorted.
///
/// Compares the components `held` holds, the contracts they name, and every contract in
/// `snapshot`: a snapshot request also names the contracts the components' DCI traces reach.
/// Contracts only `held` has and no held component names are not compared, since the client may
/// keep them from components it no longer tracks.
pub fn diff(held: &ExtractorState, snapshot: &ExtractorState) -> Vec<Difference> {
    let mut differences = Vec::new();
    for (component_id, component) in &held.components {
        let Some(fresh) = snapshot.components.get(component_id) else {
            differences.push(Difference {
                field: Field::Component,
                entity: component_id.clone(),
                kind: DifferenceKind::OnlyHeld,
            });
            continue;
        };
        diff_maps(
            &mut differences,
            Field::Attribute,
            component_id,
            &component.attributes,
            &fresh.attributes,
            String::clone,
        );
        diff_maps(
            &mut differences,
            Field::ComponentBalance,
            component_id,
            &component.balances,
            &fresh.balances,
            Bytes::to_string,
        );
    }

    let named = held.component_contracts();
    let mut addresses: HashSet<&Bytes> = named.iter().collect();
    addresses.extend(snapshot.accounts.keys());
    for address in addresses {
        let held_account = held.accounts.get(address);
        let fresh_account = snapshot.accounts.get(address);
        let reached_through_dci = !named.contains(address);
        let account_field =
            |field: Field| if reached_through_dci { Field::DciAccount } else { field };
        let entity = address.to_string();
        let (held_account, fresh_account) = match (held_account, fresh_account) {
            (None, None) => continue,
            (Some(_), None) => {
                differences.push(Difference {
                    field: account_field(Field::Account),
                    entity,
                    kind: DifferenceKind::OnlyHeld,
                });
                continue;
            }
            (None, Some(_)) => {
                differences.push(Difference {
                    field: account_field(Field::Account),
                    entity,
                    kind: DifferenceKind::OnlyInSnapshot,
                });
                continue;
            }
            (Some(held_account), Some(fresh_account)) => (held_account, fresh_account),
        };
        let HeldAccount { slots, native_balance, code, token_balances } = held_account;
        diff_maps(
            &mut differences,
            account_field(Field::Storage),
            &entity,
            slots,
            &fresh_account.slots,
            Bytes::to_string,
        );
        diff_value(
            &mut differences,
            account_field(Field::NativeBalance),
            &entity,
            "native_balance",
            native_balance.as_ref(),
            fresh_account.native_balance.as_ref(),
        );
        diff_value(
            &mut differences,
            account_field(Field::Code),
            &entity,
            "code",
            code.as_ref(),
            fresh_account.code.as_ref(),
        );
        diff_maps(
            &mut differences,
            account_field(Field::AccountTokenBalance),
            &entity,
            token_balances,
            &fresh_account.token_balances,
            Bytes::to_string,
        );
    }
    differences.sort();
    differences
}

/// Adds a difference for every key whose value differs between `held` and `snapshot`.
fn diff_maps<K: Eq + Hash>(
    differences: &mut Vec<Difference>,
    field: Field,
    entity: &str,
    held: &HashMap<K, Bytes>,
    snapshot: &HashMap<K, Bytes>,
    describe: impl Fn(&K) -> String,
) {
    let only_in_snapshot = snapshot
        .keys()
        .filter(|key| !held.contains_key(*key));
    for key in held.keys().chain(only_in_snapshot) {
        diff_value(differences, field, entity, &describe(key), held.get(key), snapshot.get(key));
    }
}

/// Adds a difference when `held` and `snapshot` differ.
fn diff_value(
    differences: &mut Vec<Difference>,
    field: Field,
    entity: &str,
    key: &str,
    held: Option<&Bytes>,
    snapshot: Option<&Bytes>,
) {
    if held == snapshot {
        return;
    }
    differences.push(Difference {
        field,
        entity: entity.to_string(),
        kind: DifferenceKind::Value {
            key: key.to_string(),
            held: held.cloned(),
            snapshot: snapshot.cloned(),
        },
    });
}

/// Stores `value` under `key`, or removes the key when `value` is empty.
fn write_non_empty<K: Eq + Hash + Clone>(map: &mut HashMap<K, Bytes>, key: &K, value: &Bytes) {
    match non_empty(Some(value)) {
        Some(value) => {
            map.insert(key.clone(), value);
        }
        None => {
            map.remove(key);
        }
    }
}

/// Stores a storage value or balance under `key`, or removes the key when it is zero or empty.
fn write_non_zero<K: Eq + Hash + Clone>(map: &mut HashMap<K, Bytes>, key: &K, value: &Bytes) {
    match non_zero(Some(value)) {
        Some(value) => {
            map.insert(key.clone(), value);
        }
        None => {
            map.remove(key);
        }
    }
}

/// `value`, unless it is absent or empty.
fn non_empty(value: Option<&Bytes>) -> Option<Bytes> {
    value
        .filter(|value| !value.is_empty())
        .cloned()
}

/// `value`, unless it is absent, empty or zero.
fn non_zero(value: Option<&Bytes>) -> Option<Bytes> {
    value
        .filter(|value| !value.is_zero())
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::slice;

    use rstest::rstest;
    use tycho_common::models::Chain;

    use super::*;

    const POOL: &str = "pool";

    fn bytes(value: u64) -> Bytes {
        Bytes::from(value).lpad(32, 0)
    }

    fn address(value: u8) -> Bytes {
        Bytes::from(vec![value; 20])
    }

    fn component(contracts: &[Bytes]) -> ProtocolComponent {
        ProtocolComponent {
            id: POOL.to_string(),
            protocol_system: "vm:test".to_string(),
            contract_addresses: contracts.to_vec(),
            ..ProtocolComponent::default()
        }
    }

    fn component_state(
        contracts: &[Bytes],
        attributes: &[(&str, Bytes)],
        balances: &[(Bytes, Bytes)],
    ) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState::new(
                POOL,
                attributes
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
                balances.iter().cloned().collect(),
            ),
            component: component(contracts),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn account(address: &Bytes, slots: &[(u64, u64)], native_balance: u64) -> Account {
        Account::new(
            Chain::Ethereum,
            address.clone(),
            "contract".to_string(),
            slots
                .iter()
                .map(|(slot, value)| (bytes(*slot), bytes(*value)))
                .collect(),
            bytes(native_balance),
            HashMap::new(),
            Bytes::from(vec![0x60, 0x80]),
            Bytes::from(vec![1]),
            Bytes::zero(32),
            Bytes::zero(32),
            None,
        )
    }

    fn with_token_balance(mut account: Account, token: Bytes, balance: Bytes) -> Account {
        let balance =
            AccountBalance::new(account.address.clone(), token.clone(), balance, Bytes::zero(32));
        account
            .token_balances
            .insert(token, balance);
        account
    }

    fn snapshot(states: Vec<ComponentWithState>, accounts: Vec<Account>) -> Snapshot {
        Snapshot {
            states: states
                .into_iter()
                .map(|state| (state.component.id.clone(), state))
                .collect(),
            vm_storage: accounts
                .into_iter()
                .map(|account| (account.address.clone(), account))
                .collect(),
        }
    }

    fn snapshot_message(snapshot: Snapshot) -> StateSyncMessage<BlockHeader> {
        StateSyncMessage { snapshots: snapshot, ..StateSyncMessage::default() }
    }

    fn delta_message(deltas: BlockAggregatedChanges) -> StateSyncMessage<BlockHeader> {
        StateSyncMessage { deltas: Some(deltas), ..StateSyncMessage::default() }
    }

    fn held(snapshot: &Snapshot) -> ExtractorState {
        let mut state = ExtractorState::default();
        state.apply(&snapshot_message(snapshot.clone()));
        state
    }

    fn value_difference(
        field: Field,
        entity: &str,
        key: &str,
        held: Option<Bytes>,
        snapshot: Option<Bytes>,
    ) -> Difference {
        Difference {
            field,
            entity: entity.to_string(),
            kind: DifferenceKind::Value { key: key.to_string(), held, snapshot },
        }
    }

    #[test]
    fn test_deltas_bring_a_snapshot_to_the_later_snapshot() {
        let contract = address(1);
        let earlier = snapshot(
            vec![component_state(
                slice::from_ref(&contract),
                &[("liquidity", bytes(1)), ("fee", bytes(3))],
                &[(address(9), bytes(10))],
            )],
            vec![account(&contract, &[(1, 1), (2, 2)], 5)],
        );
        let later = snapshot(
            vec![component_state(
                slice::from_ref(&contract),
                &[("liquidity", bytes(2))],
                &[(address(9), bytes(20))],
            )],
            vec![with_token_balance(account(&contract, &[(1, 7)], 6), address(9), bytes(4))],
        );
        let mut state = held(&earlier);

        state.apply(&delta_message(BlockAggregatedChanges {
            state_deltas: HashMap::from([(
                POOL.to_string(),
                ProtocolComponentStateDelta::new(
                    POOL,
                    HashMap::from([("liquidity".to_string(), bytes(2))]),
                    HashSet::from(["fee".to_string()]),
                ),
            )]),
            component_balances: HashMap::from([(
                POOL.to_string(),
                HashMap::from([(
                    address(9),
                    ComponentBalance::new(address(9), bytes(20), 20.0, Bytes::zero(32), POOL),
                )]),
            )]),
            account_deltas: HashMap::from([(
                contract.clone(),
                AccountDelta::new(
                    Chain::Ethereum,
                    contract.clone(),
                    HashMap::from([(bytes(1), Some(bytes(7))), (bytes(2), None)]),
                    Some(bytes(6)),
                    None,
                    ChangeType::Update,
                ),
            )]),
            account_balances: HashMap::from([(
                contract.clone(),
                HashMap::from([(
                    address(9),
                    AccountBalance::new(contract.clone(), address(9), bytes(4), Bytes::zero(32)),
                )]),
            )]),
            ..BlockAggregatedChanges::default()
        }));

        assert_eq!(diff(&state, &ExtractorState::from_snapshot(&later)), Vec::new());
    }

    #[test]
    fn test_account_creation_replaces_the_contract() {
        let contract = address(1);
        let mut state = held(&snapshot(
            vec![component_state(slice::from_ref(&contract), &[], &[])],
            vec![account(&contract, &[(1, 1), (2, 2)], 5)],
        ));

        state.apply(&delta_message(BlockAggregatedChanges {
            account_deltas: HashMap::from([(
                contract.clone(),
                AccountDelta::new(
                    Chain::Ethereum,
                    contract.clone(),
                    HashMap::from([(bytes(3), Some(bytes(3)))]),
                    Some(bytes(5)),
                    Some(Bytes::from(vec![0x60, 0x80])),
                    ChangeType::Creation,
                ),
            )]),
            ..BlockAggregatedChanges::default()
        }));

        let expected = snapshot(
            vec![component_state(slice::from_ref(&contract), &[], &[])],
            vec![account(&contract, &[(3, 3)], 5)],
        );
        assert_eq!(diff(&state, &ExtractorState::from_snapshot(&expected)), Vec::new());
    }

    #[test]
    fn test_removed_and_deleted_components_are_dropped() {
        let mut state = held(&snapshot(vec![component_state(&[], &[], &[])], Vec::new()));
        state.apply(&StateSyncMessage {
            removed_components: HashMap::from([(POOL.to_string(), component(&[]))]),
            ..StateSyncMessage::default()
        });
        assert!(state.has_no_components());

        let mut state = ExtractorState::default();
        state.apply(&StateSyncMessage {
            snapshots: snapshot(vec![component_state(&[], &[], &[])], Vec::new()),
            removed_components: HashMap::from([(POOL.to_string(), component(&[]))]),
            ..StateSyncMessage::default()
        });
        assert!(
            state.has_no_components(),
            "a component a message both snapshots and removes counts as removed"
        );

        let mut state = held(&snapshot(vec![component_state(&[], &[], &[])], Vec::new()));
        state.apply(&delta_message(BlockAggregatedChanges {
            deleted_protocol_components: HashMap::from([(
                POOL.to_string(),
                ProtocolComponent { change: ChangeType::Deletion, ..component(&[]) },
            )]),
            ..BlockAggregatedChanges::default()
        }));
        assert!(state.has_no_components());
    }

    #[test]
    fn test_deltas_for_components_without_a_snapshot_are_ignored() {
        let mut state = ExtractorState::default();
        state.apply(&delta_message(BlockAggregatedChanges {
            state_deltas: HashMap::from([(
                POOL.to_string(),
                ProtocolComponentStateDelta::new(
                    POOL,
                    HashMap::from([("liquidity".to_string(), bytes(2))]),
                    HashSet::new(),
                ),
            )]),
            new_protocol_components: HashMap::from([(
                POOL.to_string(),
                ProtocolComponent { change: ChangeType::Creation, ..component(&[]) },
            )]),
            ..BlockAggregatedChanges::default()
        }));

        assert_eq!(state, ExtractorState::default());
    }

    #[test]
    fn test_equal_states_do_not_differ() {
        let contract = address(1);
        let snapshot = snapshot(
            vec![component_state(
                slice::from_ref(&contract),
                &[("liquidity", bytes(1))],
                &[(address(9), bytes(10))],
            )],
            vec![account(&contract, &[(1, 1)], 5)],
        );

        assert_eq!(diff(&held(&snapshot), &ExtractorState::from_snapshot(&snapshot)), Vec::new());
    }

    #[rstest]
    #[case::empty_attribute(&[("liquidity", Bytes::new())], &[], &[])]
    #[case::zero_balance(&[], &[(address(9), Bytes::zero(32))], &[])]
    #[case::zero_slot(&[], &[], &[(4, 0)])]
    fn test_empty_and_zero_values_count_as_absent(
        #[case] attributes: &[(&str, Bytes)],
        #[case] balances: &[(Bytes, Bytes)],
        #[case] slots: &[(u64, u64)],
    ) {
        let contract = address(1);
        let client = snapshot(
            vec![component_state(slice::from_ref(&contract), attributes, balances)],
            vec![account(&contract, slots, 5)],
        );
        let fresh = snapshot(
            vec![component_state(slice::from_ref(&contract), &[], &[])],
            vec![account(&contract, &[], 5)],
        );

        assert_eq!(diff(&held(&client), &ExtractorState::from_snapshot(&fresh)), Vec::new());
    }

    #[test]
    fn test_component_differences_are_mismatches() {
        let client = snapshot(
            vec![component_state(&[], &[("liquidity", bytes(1))], &[(address(9), bytes(10))])],
            Vec::new(),
        );
        let fresh = snapshot(
            vec![component_state(&[], &[("tick", bytes(4))], &[(address(9), bytes(11))])],
            Vec::new(),
        );

        let differences = diff(&held(&client), &ExtractorState::from_snapshot(&fresh));

        assert_eq!(
            differences,
            vec![
                value_difference(Field::Attribute, POOL, "liquidity", Some(bytes(1)), None),
                value_difference(Field::Attribute, POOL, "tick", None, Some(bytes(4))),
                value_difference(
                    Field::ComponentBalance,
                    POOL,
                    &address(9).to_string(),
                    Some(bytes(10)),
                    Some(bytes(11)),
                ),
            ]
        );
        assert!(differences
            .iter()
            .all(|difference| difference.field.status() == Status::Mismatch));
    }

    #[test]
    fn test_component_missing_from_the_snapshot_is_a_mismatch() {
        let client = snapshot(vec![component_state(&[], &[], &[])], Vec::new());

        let differences = diff(&held(&client), &ExtractorState::default());

        assert_eq!(
            differences,
            vec![Difference {
                field: Field::Component,
                entity: POOL.to_string(),
                kind: DifferenceKind::OnlyHeld,
            }]
        );
        assert_eq!(differences[0].field.status(), Status::Mismatch);
    }

    #[test]
    fn test_named_contract_differences() {
        let contract = address(1);
        let entity = contract.to_string();
        let mut client_account = account(&contract, &[(1, 1)], 5);
        client_account.code = Bytes::from(vec![0x60]);
        client_account.token_balances = HashMap::from([(
            address(9),
            AccountBalance::new(contract.clone(), address(9), bytes(3), Bytes::zero(32)),
        )]);
        let client = snapshot(
            vec![component_state(slice::from_ref(&contract), &[], &[])],
            vec![client_account],
        );
        let fresh = snapshot(
            vec![component_state(slice::from_ref(&contract), &[], &[])],
            vec![account(&contract, &[(1, 2)], 6)],
        );

        let differences = diff(&held(&client), &ExtractorState::from_snapshot(&fresh));

        assert_eq!(
            differences,
            vec![
                value_difference(
                    Field::Storage,
                    &entity,
                    &bytes(1).to_string(),
                    Some(bytes(1)),
                    Some(bytes(2)),
                ),
                value_difference(
                    Field::NativeBalance,
                    &entity,
                    "native_balance",
                    Some(bytes(5)),
                    Some(bytes(6)),
                ),
                value_difference(
                    Field::Code,
                    &entity,
                    "code",
                    Some(Bytes::from(vec![0x60])),
                    Some(Bytes::from(vec![0x60, 0x80])),
                ),
                value_difference(
                    Field::AccountTokenBalance,
                    &entity,
                    &address(9).to_string(),
                    Some(bytes(3)),
                    None,
                ),
            ]
        );
        let statuses: Vec<Status> = differences
            .iter()
            .map(|difference| difference.field.status())
            .collect();
        assert_eq!(
            statuses,
            vec![Status::Mismatch, Status::KnownGap, Status::KnownGap, Status::KnownGap]
        );
    }

    #[rstest]
    #[case::held_only(true, false, DifferenceKind::OnlyHeld)]
    #[case::snapshot_only(false, true, DifferenceKind::OnlyInSnapshot)]
    fn test_named_contract_on_one_side_is_a_mismatch(
        #[case] client_has_it: bool,
        #[case] snapshot_has_it: bool,
        #[case] expected: DifferenceKind,
    ) {
        let contract = address(1);
        let accounts = |present: bool| {
            if present {
                vec![account(&contract, &[(1, 1)], 5)]
            } else {
                Vec::new()
            }
        };
        let client = snapshot(
            vec![component_state(slice::from_ref(&contract), &[], &[])],
            accounts(client_has_it),
        );
        let fresh = snapshot(
            vec![component_state(slice::from_ref(&contract), &[], &[])],
            accounts(snapshot_has_it),
        );

        let differences = diff(&held(&client), &ExtractorState::from_snapshot(&fresh));

        assert_eq!(
            differences,
            vec![Difference {
                field: Field::Account,
                entity: contract.to_string(),
                kind: expected
            }]
        );
        assert_eq!(differences[0].field.status(), Status::Mismatch);
    }

    #[test]
    fn test_contracts_reached_through_dci_are_known_gaps() {
        let traced = address(2);
        let missing = address(3);
        let client =
            snapshot(vec![component_state(&[], &[], &[])], vec![account(&traced, &[(1, 1)], 5)]);
        let fresh = snapshot(
            vec![component_state(&[], &[], &[])],
            vec![account(&traced, &[(1, 2)], 5), account(&missing, &[(1, 1)], 5)],
        );

        let differences = diff(&held(&client), &ExtractorState::from_snapshot(&fresh));

        assert_eq!(
            differences,
            vec![
                value_difference(
                    Field::DciAccount,
                    &traced.to_string(),
                    &bytes(1).to_string(),
                    Some(bytes(1)),
                    Some(bytes(2)),
                ),
                Difference {
                    field: Field::DciAccount,
                    entity: missing.to_string(),
                    kind: DifferenceKind::OnlyInSnapshot,
                },
            ]
        );
        assert!(differences
            .iter()
            .all(|difference| difference.field.status() == Status::KnownGap));
    }

    #[test]
    fn test_contracts_no_held_component_reaches_are_not_compared() {
        let client = snapshot(
            vec![component_state(&[], &[], &[])],
            vec![account(&address(4), &[(1, 1)], 5)],
        );
        let fresh = snapshot(vec![component_state(&[], &[], &[])], Vec::new());

        assert_eq!(diff(&held(&client), &ExtractorState::from_snapshot(&fresh)), Vec::new());
    }

    #[test]
    fn test_long_values_are_shortened_in_logs() {
        let difference =
            value_difference(Field::Code, "0x01", "code", Some(Bytes::from(vec![0xab; 40])), None);

        assert_eq!(
            difference.to_string(),
            format!("code 0x01 code: held 0x{}... (40 bytes), snapshot absent", "ab".repeat(32))
        );
    }
}
