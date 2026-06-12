//! Data Transfer Objects (or structs)
//!
//! These structs serve to serialise and deserialize messages between server and client, they should
//! be very simple and ideally not contain any business logic.
//!
//! Structs in here implement utoipa traits so they can be used to derive an OpenAPI schema.
#![allow(deprecated)]
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    hash::{Hash, Hasher},
};

use chrono::{NaiveDateTime, Utc};
use deepsize::{Context, DeepSizeOf};
use serde::{de, Deserialize, Deserializer, Serialize};
use strum_macros::{Display, EnumString};
use thiserror::Error;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    models::{self, Address, Balance, Code, ComponentId, StoreKey, StoreVal},
    serde_primitives::{
        hex_bytes, hex_bytes_option, hex_hashmap_key, hex_hashmap_key_value, hex_hashmap_value,
    },
    Bytes,
};

/// Currently supported Blockchains
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    EnumString,
    Display,
    Default,
    ToSchema,
    DeepSizeOf,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Chain {
    #[default]
    Ethereum,
    Starknet,
    ZkSync,
    Arbitrum,
    Base,
    Bsc,
    Unichain,
    Polygon,
}

pub use models::TvlThresholdTier;

impl Chain {
    /// Returns a default TVL threshold in native token units for the given tier.
    pub fn default_tvl_threshold(&self, tier: TvlThresholdTier) -> f64 {
        models::Chain::from(*self).default_tvl_threshold(tier)
    }
}

impl From<models::contract::Account> for ResponseAccount {
    fn from(value: models::contract::Account) -> Self {
        ResponseAccount::new(
            value.chain.into(),
            value.address,
            value.title,
            value.slots,
            value.native_balance,
            value
                .token_balances
                .into_iter()
                .map(|(k, v)| (k, v.balance))
                .collect(),
            value.code,
            value.code_hash,
            value.balance_modify_tx,
            value.code_modify_tx,
            value.creation_tx,
        )
    }
}

impl From<models::Chain> for Chain {
    fn from(value: models::Chain) -> Self {
        match value {
            models::Chain::Ethereum => Chain::Ethereum,
            models::Chain::Starknet => Chain::Starknet,
            models::Chain::ZkSync => Chain::ZkSync,
            models::Chain::Arbitrum => Chain::Arbitrum,
            models::Chain::Base => Chain::Base,
            models::Chain::Bsc => Chain::Bsc,
            models::Chain::Unichain => Chain::Unichain,
            models::Chain::Polygon => Chain::Polygon,
        }
    }
}

#[derive(
    Debug,
    PartialEq,
    Default,
    Copy,
    Clone,
    Deserialize,
    Serialize,
    ToSchema,
    EnumString,
    Display,
    DeepSizeOf,
)]
pub enum ChangeType {
    #[default]
    Update,
    Deletion,
    Creation,
    Unspecified,
}

impl From<models::ChangeType> for ChangeType {
    fn from(value: models::ChangeType) -> Self {
        match value {
            models::ChangeType::Update => ChangeType::Update,
            models::ChangeType::Creation => ChangeType::Creation,
            models::ChangeType::Deletion => ChangeType::Deletion,
        }
    }
}

impl ChangeType {
    pub fn merge(&self, other: &Self) -> Self {
        if matches!(self, Self::Creation) {
            Self::Creation
        } else {
            *other
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ExtractorIdentity {
    pub chain: Chain,
    pub name: String,
}

impl ExtractorIdentity {
    pub fn new(chain: Chain, name: &str) -> Self {
        Self { chain, name: name.to_owned() }
    }
}

impl fmt::Display for ExtractorIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.chain, self.name)
    }
}

/// A command sent from the client to the server
#[derive(Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum Command {
    Subscribe {
        extractor_id: ExtractorIdentity,
        include_state: bool,
        /// Enable zstd compression for messages in this subscription.
        /// Defaults to false for backward compatibility.
        #[serde(default)]
        compression: bool,
        /// Enables receiving partial block update messages in this subscription.
        /// Defaults to false for backward compatibility.
        #[serde(default)]
        partial_blocks: bool,
    },
    Unsubscribe {
        subscription_id: Uuid,
    },
}

/// A easy serializable version of `models::error::WebsocketError`
///
/// This serves purely to transfer errors via websocket. It is meant to render
/// similarly to the original struct but does not have server side debug information
/// attached.
///
/// It should contain information needed to handle errors correctly on the client side.
#[derive(Error, Debug, Serialize, Deserialize, Eq, PartialEq, Clone)]
pub enum WebsocketError {
    #[error("Extractor not found: {0}")]
    ExtractorNotFound(ExtractorIdentity),

    #[error("Subscription not found: {0}")]
    SubscriptionNotFound(Uuid),

    #[error("Failed to parse JSON: {1}, msg: {0}")]
    ParseError(String, String),

    #[error("Failed to subscribe to extractor: {0}")]
    SubscribeError(ExtractorIdentity),

    #[error("Failed to compress message for subscription: {0}, error: {1}")]
    CompressionError(Uuid, String),
}

impl From<crate::models::error::WebsocketError> for WebsocketError {
    fn from(value: crate::models::error::WebsocketError) -> Self {
        match value {
            crate::models::error::WebsocketError::ExtractorNotFound(eid) => {
                Self::ExtractorNotFound(eid.into())
            }
            crate::models::error::WebsocketError::SubscriptionNotFound(sid) => {
                Self::SubscriptionNotFound(sid)
            }
            crate::models::error::WebsocketError::ParseError(raw, error) => {
                Self::ParseError(error.to_string(), raw)
            }
            crate::models::error::WebsocketError::SubscribeError(eid) => {
                Self::SubscribeError(eid.into())
            }
            crate::models::error::WebsocketError::CompressionError(sid, error) => {
                Self::CompressionError(sid, error.to_string())
            }
        }
    }
}

/// A response sent from the server to the client
#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Clone)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum Response {
    NewSubscription { extractor_id: ExtractorIdentity, subscription_id: Uuid },
    SubscriptionEnded { subscription_id: Uuid },
    Error(WebsocketError),
}

/// A message sent from the server to the client
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, Display, Clone)]
#[serde(untagged)]
pub enum WebSocketMessage {
    BlockAggregatedChanges { subscription_id: Uuid, deltas: BlockAggregatedChanges },
    Response(Response),
}

#[derive(Debug, PartialEq, Clone, Deserialize, Serialize, Default, ToSchema)]
pub struct Block {
    pub number: u64,
    #[serde(with = "hex_bytes")]
    pub hash: Bytes,
    #[serde(with = "hex_bytes")]
    pub parent_hash: Bytes,
    pub chain: Chain,
    pub ts: NaiveDateTime,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, ToSchema, Eq, Hash, DeepSizeOf)]
#[serde(deny_unknown_fields)]
pub struct BlockParam {
    #[schema(value_type=Option<String>)]
    #[serde(with = "hex_bytes_option", default)]
    pub hash: Option<Bytes>,
    #[deprecated(
        note = "The `chain` field is deprecated and will be removed in a future version."
    )]
    #[serde(default)]
    pub chain: Option<Chain>,
    #[serde(default)]
    pub number: Option<i64>,
}

impl From<&Block> for BlockParam {
    fn from(value: &Block) -> Self {
        // The hash should uniquely identify a block across chains
        BlockParam { hash: Some(value.hash.clone()), chain: None, number: None }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct TokenBalances(#[serde(with = "hex_hashmap_key")] pub HashMap<Bytes, ComponentBalance>);

impl From<HashMap<Bytes, ComponentBalance>> for TokenBalances {
    fn from(value: HashMap<Bytes, ComponentBalance>) -> Self {
        TokenBalances(value)
    }
}

#[derive(Debug, PartialEq, Clone, Default, Deserialize, Serialize)]
pub struct Transaction {
    #[serde(with = "hex_bytes")]
    pub hash: Bytes,
    #[serde(with = "hex_bytes")]
    pub block_hash: Bytes,
    #[serde(with = "hex_bytes")]
    pub from: Bytes,
    #[serde(with = "hex_bytes_option")]
    pub to: Option<Bytes>,
    pub index: u64,
}

impl Transaction {
    pub fn new(hash: Bytes, block_hash: Bytes, from: Bytes, to: Option<Bytes>, index: u64) -> Self {
        Self { hash, block_hash, from, to, index }
    }
}

/// A container for updates grouped by account/component.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct BlockAggregatedChanges {
    pub extractor: String,
    pub chain: Chain,
    pub block: Block,
    pub finalized_block_height: u64,
    pub revert: bool,
    #[serde(with = "hex_hashmap_key", default)]
    pub new_tokens: HashMap<Bytes, ResponseToken>,
    #[serde(alias = "account_deltas", with = "hex_hashmap_key")]
    pub account_updates: HashMap<Bytes, AccountUpdate>,
    #[serde(alias = "state_deltas")]
    pub state_updates: HashMap<String, ProtocolStateDelta>,
    pub new_protocol_components: HashMap<String, ProtocolComponent>,
    pub deleted_protocol_components: HashMap<String, ProtocolComponent>,
    pub component_balances: HashMap<String, TokenBalances>,
    pub account_balances: HashMap<Bytes, HashMap<Bytes, AccountBalance>>,
    pub component_tvl: HashMap<String, f64>,
    pub dci_update: DCIUpdate,
    /// The index of the partial block. None if it's a full block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_block_index: Option<u32>,
}

impl BlockAggregatedChanges {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        extractor: &str,
        chain: Chain,
        block: Block,
        finalized_block_height: u64,
        revert: bool,
        account_updates: HashMap<Bytes, AccountUpdate>,
        state_updates: HashMap<String, ProtocolStateDelta>,
        new_protocol_components: HashMap<String, ProtocolComponent>,
        deleted_protocol_components: HashMap<String, ProtocolComponent>,
        component_balances: HashMap<String, HashMap<Bytes, ComponentBalance>>,
        account_balances: HashMap<Bytes, HashMap<Bytes, AccountBalance>>,
        dci_update: DCIUpdate,
    ) -> Self {
        BlockAggregatedChanges {
            extractor: extractor.to_owned(),
            chain,
            block,
            finalized_block_height,
            revert,
            new_tokens: HashMap::new(),
            account_updates,
            state_updates,
            new_protocol_components,
            deleted_protocol_components,
            component_balances: component_balances
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            account_balances,
            component_tvl: HashMap::new(),
            dci_update,
            partial_block_index: None,
        }
    }

    pub fn merge(mut self, other: Self) -> Self {
        other
            .account_updates
            .into_iter()
            .for_each(|(k, v)| {
                self.account_updates
                    .entry(k)
                    .and_modify(|e| {
                        e.merge(&v);
                    })
                    .or_insert(v);
            });

        other
            .state_updates
            .into_iter()
            .for_each(|(k, v)| {
                self.state_updates
                    .entry(k)
                    .and_modify(|e| {
                        e.merge(&v);
                    })
                    .or_insert(v);
            });

        other
            .component_balances
            .into_iter()
            .for_each(|(k, v)| {
                self.component_balances
                    .entry(k)
                    .and_modify(|e| e.0.extend(v.0.clone()))
                    .or_insert_with(|| v);
            });

        other
            .account_balances
            .into_iter()
            .for_each(|(k, v)| {
                self.account_balances
                    .entry(k)
                    .and_modify(|e| e.extend(v.clone()))
                    .or_insert(v);
            });

        self.component_tvl
            .extend(other.component_tvl);
        self.new_protocol_components
            .extend(other.new_protocol_components);
        self.deleted_protocol_components
            .extend(other.deleted_protocol_components);
        self.revert = other.revert;
        self.block = other.block;

        self
    }

    pub fn get_block(&self) -> &Block {
        &self.block
    }

    pub fn is_revert(&self) -> bool {
        self.revert
    }

    pub fn filter_by_component<F: Fn(&str) -> bool>(&mut self, keep: F) {
        self.state_updates
            .retain(|k, _| keep(k));
        self.component_balances
            .retain(|k, _| keep(k));
        self.component_tvl
            .retain(|k, _| keep(k));
    }

    pub fn filter_by_contract<F: Fn(&Bytes) -> bool>(&mut self, keep: F) {
        self.account_updates
            .retain(|k, _| keep(k));
        self.account_balances
            .retain(|k, _| keep(k));
    }

    pub fn n_changes(&self) -> usize {
        self.account_updates.len() + self.state_updates.len()
    }

    pub fn drop_state(&self) -> Self {
        Self {
            extractor: self.extractor.clone(),
            chain: self.chain,
            block: self.block.clone(),
            finalized_block_height: self.finalized_block_height,
            revert: self.revert,
            new_tokens: self.new_tokens.clone(),
            account_updates: HashMap::new(),
            state_updates: HashMap::new(),
            new_protocol_components: self.new_protocol_components.clone(),
            deleted_protocol_components: self.deleted_protocol_components.clone(),
            component_balances: self.component_balances.clone(),
            account_balances: self.account_balances.clone(),
            component_tvl: self.component_tvl.clone(),
            dci_update: self.dci_update.clone(),
            partial_block_index: self.partial_block_index,
        }
    }

    pub fn is_partial_block(&self) -> bool {
        self.partial_block_index.is_some()
    }
}

impl From<models::blockchain::Block> for Block {
    fn from(value: models::blockchain::Block) -> Self {
        Self {
            number: value.number,
            hash: value.hash,
            parent_hash: value.parent_hash,
            chain: value.chain.into(),
            ts: value.ts,
        }
    }
}

impl From<models::protocol::ComponentBalance> for ComponentBalance {
    fn from(value: models::protocol::ComponentBalance) -> Self {
        Self {
            token: value.token,
            balance: value.balance,
            balance_float: value.balance_float,
            modify_tx: value.modify_tx,
            component_id: value.component_id,
        }
    }
}

impl From<models::contract::AccountBalance> for AccountBalance {
    fn from(value: models::contract::AccountBalance) -> Self {
        Self {
            account: value.account,
            token: value.token,
            balance: value.balance,
            modify_tx: value.modify_tx,
        }
    }
}

impl From<models::blockchain::BlockAggregatedChanges> for BlockAggregatedChanges {
    fn from(value: models::blockchain::BlockAggregatedChanges) -> Self {
        Self {
            extractor: value.extractor,
            chain: value.chain.into(),
            block: value.block.into(),
            finalized_block_height: value.finalized_block_height,
            revert: value.revert,
            account_updates: value
                .account_deltas
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            state_updates: value
                .state_deltas
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            new_protocol_components: value
                .new_protocol_components
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            deleted_protocol_components: value
                .deleted_protocol_components
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            component_balances: value
                .component_balances
                .into_iter()
                .map(|(component_id, v)| {
                    let balances: HashMap<Bytes, ComponentBalance> = v
                        .into_iter()
                        .map(|(k, v)| (k, ComponentBalance::from(v)))
                        .collect();
                    (component_id, balances.into())
                })
                .collect(),
            account_balances: value
                .account_balances
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        v.into_iter()
                            .map(|(k, v)| (k, v.into()))
                            .collect(),
                    )
                })
                .collect(),
            dci_update: value.dci_update.into(),
            new_tokens: value
                .new_tokens
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            component_tvl: value.component_tvl,
            partial_block_index: value.partial_block_index,
        }
    }
}

#[derive(PartialEq, Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct AccountUpdate {
    #[serde(with = "hex_bytes")]
    #[schema(value_type=String)]
    pub address: Bytes,
    pub chain: Chain,
    #[serde(with = "hex_hashmap_key_value")]
    #[schema(value_type=HashMap<String, String>)]
    pub slots: HashMap<Bytes, Bytes>,
    #[serde(with = "hex_bytes_option")]
    #[schema(value_type=Option<String>)]
    pub balance: Option<Bytes>,
    #[serde(with = "hex_bytes_option")]
    #[schema(value_type=Option<String>)]
    pub code: Option<Bytes>,
    pub change: ChangeType,
}

impl AccountUpdate {
    pub fn new(
        address: Bytes,
        chain: Chain,
        slots: HashMap<Bytes, Bytes>,
        balance: Option<Bytes>,
        code: Option<Bytes>,
        change: ChangeType,
    ) -> Self {
        Self { address, chain, slots, balance, code, change }
    }

    pub fn merge(&mut self, other: &Self) {
        self.slots.extend(
            other
                .slots
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        self.balance.clone_from(&other.balance);
        self.code.clone_from(&other.code);
        self.change = self.change.merge(&other.change);
    }
}

impl From<models::contract::AccountDelta> for AccountUpdate {
    fn from(value: models::contract::AccountDelta) -> Self {
        let code = value.code().clone();
        let change_type = value.change_type().into();
        AccountUpdate::new(
            value.address,
            value.chain.into(),
            value
                .slots
                .into_iter()
                .map(|(k, v)| (k, v.unwrap_or_default()))
                .collect(),
            value.balance,
            code,
            change_type,
        )
    }
}

/// Represents the static parts of a protocol component.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, ToSchema)]
pub struct ProtocolComponent {
    /// Unique identifier for this component
    pub id: String,
    /// Protocol system this component is part of
    pub protocol_system: String,
    /// Type of the protocol system
    pub protocol_type_name: String,
    pub chain: Chain,
    /// Token addresses the component operates on
    #[schema(value_type=Vec<String>)]
    pub tokens: Vec<Bytes>,
    /// Contract addresses involved in the components operations (may be empty for
    /// native implementations)
    #[serde(alias = "contract_addresses")]
    #[schema(value_type=Vec<String>)]
    pub contract_ids: Vec<Bytes>,
    /// Constant attributes of the component
    #[serde(with = "hex_hashmap_value")]
    #[schema(value_type=HashMap<String, String>)]
    pub static_attributes: HashMap<String, Bytes>,
    /// Indicates if last change was update, create or delete (for internal use only).
    #[serde(default)]
    pub change: ChangeType,
    /// Transaction hash which created this component
    #[serde(with = "hex_bytes")]
    #[schema(value_type=String)]
    pub creation_tx: Bytes,
    /// Date time of creation in UTC time
    pub created_at: NaiveDateTime,
}

// Manual impl as `NaiveDateTime` structure referenced in `created_at` does not implement DeepSizeOf
impl DeepSizeOf for ProtocolComponent {
    fn deep_size_of_children(&self, ctx: &mut Context) -> usize {
        self.id.deep_size_of_children(ctx) +
            self.protocol_system
                .deep_size_of_children(ctx) +
            self.protocol_type_name
                .deep_size_of_children(ctx) +
            self.chain.deep_size_of_children(ctx) +
            self.tokens.deep_size_of_children(ctx) +
            self.contract_ids
                .deep_size_of_children(ctx) +
            self.static_attributes
                .deep_size_of_children(ctx) +
            self.change.deep_size_of_children(ctx) +
            self.creation_tx
                .deep_size_of_children(ctx)
    }
}

impl<T> From<models::protocol::ProtocolComponent<T>> for ProtocolComponent
where
    T: Into<Address> + Clone,
{
    fn from(value: models::protocol::ProtocolComponent<T>) -> Self {
        Self {
            id: value.id,
            protocol_system: value.protocol_system,
            protocol_type_name: value.protocol_type_name,
            chain: value.chain.into(),
            tokens: value
                .tokens
                .into_iter()
                .map(|t| t.into())
                .collect(),
            contract_ids: value.contract_addresses,
            static_attributes: value.static_attributes,
            change: value.change.into(),
            creation_tx: value.creation_tx,
            created_at: value.created_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct ComponentBalance {
    #[serde(with = "hex_bytes")]
    pub token: Bytes,
    pub balance: Bytes,
    pub balance_float: f64,
    #[serde(with = "hex_bytes")]
    pub modify_tx: Bytes,
    pub component_id: String,
}

#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize, ToSchema)]
/// Represents a change in protocol state.
pub struct ProtocolStateDelta {
    pub component_id: String,
    #[schema(value_type=HashMap<String, String>)]
    pub updated_attributes: HashMap<String, Bytes>,
    pub deleted_attributes: HashSet<String>,
}

impl From<models::protocol::ProtocolComponentStateDelta> for ProtocolStateDelta {
    fn from(value: models::protocol::ProtocolComponentStateDelta) -> Self {
        Self {
            component_id: value.component_id,
            updated_attributes: value.updated_attributes,
            deleted_attributes: value.deleted_attributes,
        }
    }
}

impl ProtocolStateDelta {
    /// Merges 'other' into 'self'.
    ///
    ///
    /// During merge of these deltas a special situation can arise when an attribute is present in
    /// `self.deleted_attributes` and `other.update_attributes``. If we would just merge the sets
    /// of deleted attributes or vice versa, it would be ambiguous and potential lead to a
    /// deletion of an attribute that should actually be present, or retention of an actually
    /// deleted attribute.
    ///
    /// This situation is handled the following way:
    ///
    ///     - If an attribute is deleted and in the next message recreated, it is removed from the
    ///       set of deleted attributes and kept in updated_attributes. This way it's temporary
    ///       deletion is never communicated to the final receiver.
    ///     - If an attribute was updated and is deleted in the next message, it is removed from
    ///       updated attributes and kept in deleted. This way the attributes temporary update (or
    ///       potentially short-lived existence) before its deletion is never communicated to the
    ///       final receiver.
    pub fn merge(&mut self, other: &Self) {
        // either updated and then deleted -> keep in deleted, remove from updated
        self.updated_attributes
            .retain(|k, _| !other.deleted_attributes.contains(k));

        // or deleted and then updated/recreated -> remove from deleted and keep in updated
        self.deleted_attributes.retain(|attr| {
            !other
                .updated_attributes
                .contains_key(attr)
        });

        // simply merge updates
        self.updated_attributes.extend(
            other
                .updated_attributes
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );

        // simply merge deletions
        self.deleted_attributes
            .extend(other.deleted_attributes.iter().cloned());
    }
}

/// Pagination parameter
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, ToSchema, Eq, Hash, DeepSizeOf)]
#[serde(deny_unknown_fields)]
pub struct PaginationParams {
    /// What page to retrieve
    #[serde(default)]
    pub page: i64,
    /// How many results to return per page
    #[serde(default)]
    #[schema(default = 100)]
    pub page_size: i64,
}

impl PaginationParams {
    pub fn new(page: i64, page_size: i64) -> Self {
        Self { page, page_size }
    }
}

impl Default for PaginationParams {
    fn default() -> Self {
        PaginationParams { page: 0, page_size: 100 }
    }
}

/// Defines pagination size limits for request types.
///
/// Different limits apply based on whether compression is enabled,
/// as compressed responses can safely transfer more data.
pub trait PaginationLimits {
    /// Maximum page size when compression is enabled (e.g., zstd)
    const MAX_PAGE_SIZE_COMPRESSED: i64;

    /// Maximum page size when compression is disabled
    const MAX_PAGE_SIZE_UNCOMPRESSED: i64;

    /// Returns the effective maximum page size based on compression setting
    fn effective_max_page_size(compression: bool) -> i64 {
        if compression {
            Self::MAX_PAGE_SIZE_COMPRESSED
        } else {
            Self::MAX_PAGE_SIZE_UNCOMPRESSED
        }
    }

    /// Returns a reference to the pagination parameters
    fn pagination(&self) -> &PaginationParams;
}

/// Macro to implement PaginationLimits for request types
///
/// When INCREASING these limits, ensure to immediately redeploy the servers.
///
/// Why: pagination limits are shared. Clients use these constants to set their max page size.
/// When clients upgrade before servers, they request more than old servers allow and get errors.
macro_rules! impl_pagination_limits {
    ($type:ty, compressed = $comp:expr, uncompressed = $uncomp:expr) => {
        impl $crate::dto::PaginationLimits for $type {
            const MAX_PAGE_SIZE_COMPRESSED: i64 = $comp;
            const MAX_PAGE_SIZE_UNCOMPRESSED: i64 = $uncomp;

            fn pagination(&self) -> &$crate::dto::PaginationParams {
                &self.pagination
            }
        }
    };
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, ToSchema, Eq, Hash, DeepSizeOf)]
#[serde(deny_unknown_fields)]
pub struct PaginationResponse {
    pub page: i64,
    pub page_size: i64,
    /// The total number of items available across all pages of results
    pub total: i64,
}

/// Current pagination information
impl PaginationResponse {
    pub fn new(page: i64, page_size: i64, total: i64) -> Self {
        Self { page, page_size, total }
    }

    pub fn total_pages(&self) -> i64 {
        // ceil(total / page_size)
        (self.total + self.page_size - 1) / self.page_size
    }
}

#[derive(
    Clone, Serialize, Debug, Default, Deserialize, PartialEq, ToSchema, Eq, Hash, DeepSizeOf,
)]
#[serde(deny_unknown_fields)]
pub struct StateRequestBody {
    /// Filters response by contract addresses
    #[serde(alias = "contractIds")]
    #[schema(value_type=Option<Vec<String>>)]
    pub contract_ids: Option<Vec<Bytes>>,
    /// Does not filter response, only required to correctly apply unconfirmed state
    /// from ReorgBuffers
    #[serde(alias = "protocolSystem", default)]
    pub protocol_system: String,
    #[serde(default = "VersionParam::default")]
    pub version: VersionParam,
    #[serde(default)]
    pub chain: Chain,
    #[serde(default)]
    pub pagination: PaginationParams,
}

// When INCREASING these limits, please read the warning in the macro definition.
impl_pagination_limits!(StateRequestBody, compressed = 1200, uncompressed = 100);

impl StateRequestBody {
    pub fn new(
        contract_ids: Option<Vec<Bytes>>,
        protocol_system: String,
        version: VersionParam,
        chain: Chain,
        pagination: PaginationParams,
    ) -> Self {
        Self { contract_ids, protocol_system, version, chain, pagination }
    }

    pub fn from_block(protocol_system: &str, block: BlockParam) -> Self {
        Self {
            contract_ids: None,
            protocol_system: protocol_system.to_string(),
            version: VersionParam { timestamp: None, block: Some(block.clone()) },
            chain: block.chain.unwrap_or_default(),
            pagination: PaginationParams::default(),
        }
    }

    pub fn from_timestamp(protocol_system: &str, timestamp: NaiveDateTime, chain: Chain) -> Self {
        Self {
            contract_ids: None,
            protocol_system: protocol_system.to_string(),
            version: VersionParam { timestamp: Some(timestamp), block: None },
            chain,
            pagination: PaginationParams::default(),
        }
    }
}

/// Response from Tycho server for a contract state request.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, ToSchema, DeepSizeOf)]
pub struct StateRequestResponse {
    pub accounts: Vec<ResponseAccount>,
    pub pagination: PaginationResponse,
}

impl StateRequestResponse {
    pub fn new(accounts: Vec<ResponseAccount>, pagination: PaginationResponse) -> Self {
        Self { accounts, pagination }
    }
}

#[derive(PartialEq, Clone, Serialize, Deserialize, Default, ToSchema, DeepSizeOf)]
#[serde(rename = "Account")]
/// Account struct for the response from Tycho server for a contract state request.
///
/// Code is serialized as a hex string instead of a list of bytes.
pub struct ResponseAccount {
    pub chain: Chain,
    /// The address of the account as hex encoded string
    #[schema(value_type=String, example="0xc9f2e6ea1637E499406986ac50ddC92401ce1f58")]
    #[serde(with = "hex_bytes")]
    pub address: Bytes,
    /// The title of the account usualy specifying its function within the protocol
    #[schema(value_type=String, example="Protocol Vault")]
    pub title: String,
    /// Contract storage map of hex encoded string values
    #[schema(value_type=HashMap<String, String>, example=json!({"0x....": "0x...."}))]
    #[serde(with = "hex_hashmap_key_value")]
    pub slots: HashMap<Bytes, Bytes>,
    /// The balance of the account in the native token
    #[schema(value_type=String, example="0x00")]
    #[serde(with = "hex_bytes")]
    pub native_balance: Bytes,
    /// Balances of this account in other tokens (only tokens balance that are
    /// relevant to the protocol are returned here)
    #[schema(value_type=HashMap<String, String>, example=json!({"0x....": "0x...."}))]
    #[serde(with = "hex_hashmap_key_value")]
    pub token_balances: HashMap<Bytes, Bytes>,
    /// The accounts code as hex encoded string
    #[schema(value_type=String, example="0xBADBABE")]
    #[serde(with = "hex_bytes")]
    pub code: Bytes,
    /// The hash of above code
    #[schema(value_type=String, example="0x123456789")]
    #[serde(with = "hex_bytes")]
    pub code_hash: Bytes,
    /// Transaction hash which last modified native balance
    #[schema(value_type=String, example="0x8f1133bfb054a23aedfe5d25b1d81b96195396d8b88bd5d4bcf865fc1ae2c3f4")]
    #[serde(with = "hex_bytes")]
    pub balance_modify_tx: Bytes,
    /// Transaction hash which last modified code
    #[schema(value_type=String, example="0x8f1133bfb054a23aedfe5d25b1d81b96195396d8b88bd5d4bcf865fc1ae2c3f4")]
    #[serde(with = "hex_bytes")]
    pub code_modify_tx: Bytes,
    /// Transaction hash which created the account
    #[deprecated(note = "The `creation_tx` field is deprecated.")]
    #[schema(value_type=Option<String>, example="0x8f1133bfb054a23aedfe5d25b1d81b96195396d8b88bd5d4bcf865fc1ae2c3f4")]
    #[serde(with = "hex_bytes_option")]
    pub creation_tx: Option<Bytes>,
}

impl ResponseAccount {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: Chain,
        address: Bytes,
        title: String,
        slots: HashMap<Bytes, Bytes>,
        native_balance: Bytes,
        token_balances: HashMap<Bytes, Bytes>,
        code: Bytes,
        code_hash: Bytes,
        balance_modify_tx: Bytes,
        code_modify_tx: Bytes,
        creation_tx: Option<Bytes>,
    ) -> Self {
        Self {
            chain,
            address,
            title,
            slots,
            native_balance,
            token_balances,
            code,
            code_hash,
            balance_modify_tx,
            code_modify_tx,
            creation_tx,
        }
    }
}

/// Implement Debug for ResponseAccount manually to avoid printing the code field.
impl fmt::Debug for ResponseAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseAccount")
            .field("chain", &self.chain)
            .field("address", &self.address)
            .field("title", &self.title)
            .field("slots", &self.slots)
            .field("native_balance", &self.native_balance)
            .field("token_balances", &self.token_balances)
            .field("code", &format!("[{} bytes]", self.code.len()))
            .field("code_hash", &self.code_hash)
            .field("balance_modify_tx", &self.balance_modify_tx)
            .field("code_modify_tx", &self.code_modify_tx)
            .field("creation_tx", &self.creation_tx)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct AccountBalance {
    #[serde(with = "hex_bytes")]
    pub account: Bytes,
    #[serde(with = "hex_bytes")]
    pub token: Bytes,
    #[serde(with = "hex_bytes")]
    pub balance: Bytes,
    #[serde(with = "hex_bytes")]
    pub modify_tx: Bytes,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContractId {
    #[serde(with = "hex_bytes")]
    #[schema(value_type=String)]
    pub address: Bytes,
    pub chain: Chain,
}

/// Uniquely identifies a contract on a specific chain.
impl ContractId {
    pub fn new(chain: Chain, address: Bytes) -> Self {
        Self { address, chain }
    }

    pub fn address(&self) -> &Bytes {
        &self.address
    }
}

impl fmt::Display for ContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: 0x{}", self.chain, hex::encode(&self.address))
    }
}

/// The version of the requested state, given as either a timestamp or a block.
///
/// If block is provided, the state at that exact block is returned. Will error if the block
/// has not been processed yet. If timestamp is provided, the state at the latest block before
/// that timestamp is returned.
/// Defaults to the current time.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, ToSchema, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct VersionParam {
    pub timestamp: Option<NaiveDateTime>,
    pub block: Option<BlockParam>,
}

impl DeepSizeOf for VersionParam {
    fn deep_size_of_children(&self, ctx: &mut Context) -> usize {
        if let Some(block) = &self.block {
            return block.deep_size_of_children(ctx);
        }

        0
    }
}

impl VersionParam {
    pub fn new(timestamp: Option<NaiveDateTime>, block: Option<BlockParam>) -> Self {
        Self { timestamp, block }
    }

    pub fn at_block(chain: Chain, block_number: u64) -> Self {
        Self::new(
            None,
            Some({
                #[allow(deprecated)]
                BlockParam { hash: None, chain: Some(chain), number: Some(block_number as i64) }
            }),
        )
    }
}

impl Default for VersionParam {
    fn default() -> Self {
        VersionParam { timestamp: Some(Utc::now().naive_utc()), block: None }
    }
}

#[deprecated(note = "Use StateRequestBody instead")]
#[derive(Serialize, Deserialize, Default, Debug, IntoParams)]
pub struct StateRequestParameters {
    /// The minimum TVL of the protocol components to return, denoted in the chain's native token.
    #[param(default = 0)]
    pub tvl_gt: Option<u64>,
    /// The minimum inertia of the protocol components to return.
    #[param(default = 0)]
    pub inertia_min_gt: Option<u64>,
    /// Whether to include ERC20 balances in the response.
    #[serde(default = "default_include_balances_flag")]
    pub include_balances: bool,
    #[serde(default)]
    pub pagination: PaginationParams,
}

impl StateRequestParameters {
    pub fn new(include_balances: bool) -> Self {
        Self {
            tvl_gt: None,
            inertia_min_gt: None,
            include_balances,
            pagination: PaginationParams::default(),
        }
    }

    pub fn to_query_string(&self) -> String {
        let mut parts = vec![format!("include_balances={}", self.include_balances)];

        if let Some(tvl_gt) = self.tvl_gt {
            parts.push(format!("tvl_gt={tvl_gt}"));
        }

        if let Some(inertia) = self.inertia_min_gt {
            parts.push(format!("inertia_min_gt={inertia}"));
        }

        let mut res = parts.join("&");
        if !res.is_empty() {
            res = format!("?{res}");
        }
        res
    }
}

#[derive(
    Serialize, Deserialize, Debug, Default, PartialEq, ToSchema, Eq, Hash, Clone, DeepSizeOf,
)]
#[serde(deny_unknown_fields)]
pub struct TokensRequestBody {
    /// Filters tokens by addresses
    #[serde(alias = "tokenAddresses")]
    #[schema(value_type=Option<Vec<String>>)]
    pub token_addresses: Option<Vec<Bytes>>,
    /// Quality is between 0-100, where:
    ///  - 100: Normal ERC-20 Token behavior
    ///  - 75: Rebasing token
    ///  - 50: Fee-on-transfer token
    ///  - 10: Token analysis failed at first detection
    ///  - 5: Token analysis failed multiple times (after creation)
    ///  - 0: Failed to extract attributes, like Decimal or Symbol
    #[serde(default)]
    pub min_quality: Option<i32>,
    /// Filters tokens by recent trade activity
    #[serde(default)]
    pub traded_n_days_ago: Option<u64>,
    /// Max page size supported is 3000
    #[serde(default)]
    pub pagination: PaginationParams,
    /// Filter tokens by blockchain, default 'ethereum'
    #[serde(default)]
    pub chain: Chain,
}

// When INCREASING these limits, please read the warning in the macro definition.
impl_pagination_limits!(TokensRequestBody, compressed = 12900, uncompressed = 3000);

/// Response from Tycho server for a tokens request.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, ToSchema, Eq, Hash, DeepSizeOf)]
pub struct TokensRequestResponse {
    pub tokens: Vec<ResponseToken>,
    pub pagination: PaginationResponse,
}

impl TokensRequestResponse {
    pub fn new(tokens: Vec<ResponseToken>, pagination_request: &PaginationResponse) -> Self {
        Self { tokens, pagination: pagination_request.clone() }
    }
}

#[derive(
    PartialEq, Debug, Clone, Serialize, Deserialize, Default, ToSchema, Eq, Hash, DeepSizeOf,
)]
#[serde(rename = "Token")]
/// Token struct for the response from Tycho server for a tokens request.
pub struct ResponseToken {
    pub chain: Chain,
    /// The address of this token as hex encoded string
    #[schema(value_type=String, example="0xc9f2e6ea1637E499406986ac50ddC92401ce1f58")]
    #[serde(with = "hex_bytes")]
    pub address: Bytes,
    /// A shorthand symbol for this token (not unique)
    #[schema(value_type=String, example="WETH")]
    pub symbol: String,
    /// The number of decimals used to represent token values
    pub decimals: u32,
    /// The tax this token charges on transfers in basis points
    pub tax: u64,
    /// Gas usage of the token, currently is always a single averaged value
    pub gas: Vec<Option<u64>>,
    /// Quality is between 0-100, where:
    ///  - 100: Normal ERC-20 Token behavior
    ///  - 75: Rebasing token
    ///  - 50: Fee-on-transfer token
    ///  - 10: Token analysis failed at first detection
    ///  - 5: Token analysis failed multiple times (after creation)
    ///  - 0: Failed to extract attributes, like Decimal or Symbol
    pub quality: u32,
}

impl From<models::token::Token> for ResponseToken {
    fn from(value: models::token::Token) -> Self {
        Self {
            chain: value.chain.into(),
            address: value.address,
            symbol: value.symbol,
            decimals: value.decimals,
            tax: value.tax,
            gas: value.gas,
            quality: value.quality,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default, ToSchema, Clone, DeepSizeOf)]
#[serde(deny_unknown_fields)]
pub struct ProtocolComponentsRequestBody {
    /// Filters by protocol, required to correctly apply unconfirmed state from
    /// ReorgBuffers
    pub protocol_system: String,
    /// Filter by component ids
    #[schema(value_type=Option<Vec<String>>)]
    #[serde(alias = "componentAddresses")]
    pub component_ids: Option<Vec<ComponentId>>,
    /// The minimum TVL of the protocol components to return, denoted in the chain's
    /// native token.
    #[serde(default)]
    pub tvl_gt: Option<f64>,
    #[serde(default)]
    pub chain: Chain,
    /// Max page size supported is 500
    #[serde(default)]
    pub pagination: PaginationParams,
}

// When INCREASING these limits, please read the warning in the macro definition.
impl_pagination_limits!(ProtocolComponentsRequestBody, compressed = 2550, uncompressed = 500);

// Implement PartialEq where tvl is considered equal if the difference is less than 1e-6
impl PartialEq for ProtocolComponentsRequestBody {
    fn eq(&self, other: &Self) -> bool {
        let tvl_close_enough = match (self.tvl_gt, other.tvl_gt) {
            (Some(a), Some(b)) => (a - b).abs() < 1e-6,
            (None, None) => true,
            _ => false,
        };

        self.protocol_system == other.protocol_system &&
            self.component_ids == other.component_ids &&
            tvl_close_enough &&
            self.chain == other.chain &&
            self.pagination == other.pagination
    }
}

// Implement Eq without any new logic
impl Eq for ProtocolComponentsRequestBody {}

impl Hash for ProtocolComponentsRequestBody {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.protocol_system.hash(state);
        self.component_ids.hash(state);

        // Handle the f64 `tvl_gt` field by converting it into a hashable integer
        if let Some(tvl) = self.tvl_gt {
            // Convert f64 to bits and hash those bits
            tvl.to_bits().hash(state);
        } else {
            // Use a constant value to represent None
            state.write_u8(0);
        }

        self.chain.hash(state);
        self.pagination.hash(state);
    }
}

impl ProtocolComponentsRequestBody {
    pub fn system_filtered(system: &str, tvl_gt: Option<f64>, chain: Chain) -> Self {
        Self {
            protocol_system: system.to_string(),
            component_ids: None,
            tvl_gt,
            chain,
            pagination: Default::default(),
        }
    }

    pub fn id_filtered(system: &str, ids: Vec<String>, chain: Chain) -> Self {
        Self {
            protocol_system: system.to_string(),
            component_ids: Some(ids),
            tvl_gt: None,
            chain,
            pagination: Default::default(),
        }
    }
}

impl ProtocolComponentsRequestBody {
    pub fn new(
        protocol_system: String,
        component_ids: Option<Vec<String>>,
        tvl_gt: Option<f64>,
        chain: Chain,
        pagination: PaginationParams,
    ) -> Self {
        Self { protocol_system, component_ids, tvl_gt, chain, pagination }
    }
}

#[deprecated(note = "Use ProtocolComponentsRequestBody instead")]
#[derive(Serialize, Deserialize, Default, Debug, IntoParams)]
pub struct ProtocolComponentRequestParameters {
    /// The minimum TVL of the protocol components to return, denoted in the chain's native token.
    #[param(default = 0)]
    pub tvl_gt: Option<f64>,
}

impl ProtocolComponentRequestParameters {
    pub fn tvl_filtered(min_tvl: f64) -> Self {
        Self { tvl_gt: Some(min_tvl) }
    }
}

impl ProtocolComponentRequestParameters {
    pub fn to_query_string(&self) -> String {
        if let Some(tvl_gt) = self.tvl_gt {
            return format!("?tvl_gt={tvl_gt}");
        }
        String::new()
    }
}

/// Response from Tycho server for a protocol components request.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, ToSchema, DeepSizeOf)]
pub struct ProtocolComponentRequestResponse {
    pub protocol_components: Vec<ProtocolComponent>,
    pub pagination: PaginationResponse,
}

impl ProtocolComponentRequestResponse {
    pub fn new(
        protocol_components: Vec<ProtocolComponent>,
        pagination: PaginationResponse,
    ) -> Self {
        Self { protocol_components, pagination }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, ToSchema, Eq, Hash)]
#[serde(deny_unknown_fields)]
#[deprecated]
pub struct ProtocolId {
    pub id: String,
    pub chain: Chain,
}

impl From<ProtocolId> for String {
    fn from(protocol_id: ProtocolId) -> Self {
        protocol_id.id
    }
}

impl AsRef<str> for ProtocolId {
    fn as_ref(&self) -> &str {
        &self.id
    }
}

/// Protocol State struct for the response from Tycho server for a protocol state request.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, ToSchema, DeepSizeOf)]
pub struct ResponseProtocolState {
    /// Component id this state belongs to
    pub component_id: String,
    /// Attributes of the component. If an attribute's value is a `bigint`,
    /// it will be encoded as a big endian signed hex string.
    #[schema(value_type=HashMap<String, String>)]
    #[serde(with = "hex_hashmap_value")]
    pub attributes: HashMap<String, Bytes>,
    /// Sum aggregated balances of the component
    #[schema(value_type=HashMap<String, String>)]
    #[serde(with = "hex_hashmap_key_value")]
    pub balances: HashMap<Bytes, Bytes>,
}

impl From<models::protocol::ProtocolComponentState> for ResponseProtocolState {
    fn from(value: models::protocol::ProtocolComponentState) -> Self {
        Self {
            component_id: value.component_id,
            attributes: value.attributes,
            balances: value.balances,
        }
    }
}

fn default_include_balances_flag() -> bool {
    true
}

/// Max page size supported is 100
#[derive(Clone, Debug, Serialize, PartialEq, ToSchema, Default, Eq, Hash, DeepSizeOf)]
#[serde(deny_unknown_fields)]
pub struct ProtocolStateRequestBody {
    /// Filters response by protocol components ids
    #[serde(alias = "protocolIds")]
    pub protocol_ids: Option<Vec<String>>,
    /// Filters by protocol, required to correctly apply unconfirmed state from
    /// ReorgBuffers
    #[serde(alias = "protocolSystem")]
    pub protocol_system: String,
    #[serde(default)]
    pub chain: Chain,
    /// Whether to include account balances in the response. Defaults to true.
    #[serde(default = "default_include_balances_flag")]
    pub include_balances: bool,
    #[serde(default = "VersionParam::default")]
    pub version: VersionParam,
    #[serde(default)]
    pub pagination: PaginationParams,
}

// When INCREASING these limits, please read the warning in the macro definition.
impl_pagination_limits!(ProtocolStateRequestBody, compressed = 360, uncompressed = 100);

impl ProtocolStateRequestBody {
    pub fn id_filtered<I, T>(ids: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        Self {
            protocol_ids: Some(
                ids.into_iter()
                    .map(Into::into)
                    .collect(),
            ),
            ..Default::default()
        }
    }
}

/// Custom deserializer for ProtocolStateRequestBody to support backwards compatibility with the old
/// ProtocolIds format.
/// To be removed when the old format is no longer supported.
impl<'de> Deserialize<'de> for ProtocolStateRequestBody {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum ProtocolIdOrString {
            Old(Vec<ProtocolId>),
            New(Vec<String>),
        }

        struct ProtocolStateRequestBodyVisitor;

        impl<'de> de::Visitor<'de> for ProtocolStateRequestBodyVisitor {
            type Value = ProtocolStateRequestBody;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct ProtocolStateRequestBody")
            }

            fn visit_map<V>(self, mut map: V) -> Result<ProtocolStateRequestBody, V::Error>
            where
                V: de::MapAccess<'de>,
            {
                let mut protocol_ids = None;
                let mut protocol_system = None;
                let mut version = None;
                let mut chain = None;
                let mut include_balances = None;
                let mut pagination = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "protocol_ids" | "protocolIds" => {
                            let value: ProtocolIdOrString = map.next_value()?;
                            protocol_ids = match value {
                                ProtocolIdOrString::Old(ids) => {
                                    Some(ids.into_iter().map(|p| p.id).collect())
                                }
                                ProtocolIdOrString::New(ids_str) => Some(ids_str),
                            };
                        }
                        "protocol_system" | "protocolSystem" => {
                            protocol_system = Some(map.next_value()?);
                        }
                        "version" => {
                            version = Some(map.next_value()?);
                        }
                        "chain" => {
                            chain = Some(map.next_value()?);
                        }
                        "include_balances" => {
                            include_balances = Some(map.next_value()?);
                        }
                        "pagination" => {
                            pagination = Some(map.next_value()?);
                        }
                        _ => {
                            return Err(de::Error::unknown_field(
                                &key,
                                &[
                                    "contract_ids",
                                    "protocol_system",
                                    "version",
                                    "chain",
                                    "include_balances",
                                    "pagination",
                                ],
                            ))
                        }
                    }
                }

                Ok(ProtocolStateRequestBody {
                    protocol_ids,
                    protocol_system: protocol_system.unwrap_or_default(),
                    version: version.unwrap_or_else(VersionParam::default),
                    chain: chain.unwrap_or_else(Chain::default),
                    include_balances: include_balances.unwrap_or(true),
                    pagination: pagination.unwrap_or_else(PaginationParams::default),
                })
            }
        }

        deserializer.deserialize_struct(
            "ProtocolStateRequestBody",
            &[
                "contract_ids",
                "protocol_system",
                "version",
                "chain",
                "include_balances",
                "pagination",
            ],
            ProtocolStateRequestBodyVisitor,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, ToSchema, DeepSizeOf)]
pub struct ProtocolStateRequestResponse {
    pub states: Vec<ResponseProtocolState>,
    pub pagination: PaginationResponse,
}

impl ProtocolStateRequestResponse {
    pub fn new(states: Vec<ResponseProtocolState>, pagination: PaginationResponse) -> Self {
        Self { states, pagination }
    }
}

#[derive(Serialize, Clone, PartialEq, Hash, Eq)]
pub struct ProtocolComponentId {
    pub chain: Chain,
    pub system: String,
    pub id: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "status", content = "message")]
#[schema(example = json!({"status": "NotReady", "message": "No db connection"}))]
pub enum Health {
    Ready,
    Starting(String),
    NotReady(String),
}

#[derive(Serialize, Deserialize, Debug, Default, PartialEq, ToSchema, Eq, Hash, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProtocolSystemsRequestBody {
    #[serde(default)]
    pub chain: Chain,
    #[serde(default)]
    pub pagination: PaginationParams,
}

// When INCREASING these limits, please read the warning in the macro definition.
impl_pagination_limits!(ProtocolSystemsRequestBody, compressed = 100, uncompressed = 100);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, ToSchema, Eq, Hash)]
pub struct ProtocolSystemsRequestResponse {
    /// List of currently supported protocol systems
    pub protocol_systems: Vec<String>,
    /// Protocol systems that use Dynamic Contract Indexing (DCI).
    /// Clients should only fetch entrypoints for these protocols.
    #[serde(default)]
    pub dci_protocols: Vec<String>,
    pub pagination: PaginationResponse,
}

impl ProtocolSystemsRequestResponse {
    pub fn new(
        protocol_systems: Vec<String>,
        dci_protocols: Vec<String>,
        pagination: PaginationResponse,
    ) -> Self {
        Self { protocol_systems, dci_protocols, pagination }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct DCIUpdate {
    /// Map of component id to the new entrypoints associated with the component
    pub new_entrypoints: HashMap<ComponentId, HashSet<EntryPoint>>,
    /// Map of entrypoint id to the new entrypoint params associtated with it (and optionally the
    /// component linked to those params)
    pub new_entrypoint_params: HashMap<String, HashSet<(TracingParams, String)>>,
    /// Map of entrypoint id to its trace result
    pub trace_results: HashMap<String, TracingResult>,
}

impl From<models::blockchain::DCIUpdate> for DCIUpdate {
    fn from(value: models::blockchain::DCIUpdate) -> Self {
        Self {
            new_entrypoints: value
                .new_entrypoints
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        v.into_iter()
                            .map(|v| v.into())
                            .collect(),
                    )
                })
                .collect(),
            new_entrypoint_params: value
                .new_entrypoint_params
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        v.into_iter()
                            .map(|(params, i)| (params.into(), i))
                            .collect(),
                    )
                })
                .collect(),
            trace_results: value
                .trace_results
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default, PartialEq, ToSchema, Eq, Hash, Clone)]
#[serde(deny_unknown_fields)]
pub struct ComponentTvlRequestBody {
    #[serde(default)]
    pub chain: Chain,
    /// Filters protocol components by protocol system
    /// Useful when `component_ids` is omitted to fetch all components under a specific system.
    #[serde(alias = "protocolSystem")]
    pub protocol_system: Option<String>,
    #[serde(default)]
    pub component_ids: Option<Vec<String>>,
    #[serde(default)]
    pub pagination: PaginationParams,
}

// When INCREASING these limits, please read the warning in the macro definition.
impl_pagination_limits!(ComponentTvlRequestBody, compressed = 100, uncompressed = 100);

impl ComponentTvlRequestBody {
    pub fn system_filtered(system: &str, chain: Chain) -> Self {
        Self {
            chain,
            protocol_system: Some(system.to_string()),
            component_ids: None,
            pagination: Default::default(),
        }
    }

    pub fn id_filtered(ids: Vec<String>, chain: Chain) -> Self {
        Self {
            chain,
            protocol_system: None,
            component_ids: Some(ids),
            pagination: Default::default(),
        }
    }
}
// #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, ToSchema, Eq, Hash)]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ComponentTvlRequestResponse {
    pub tvl: HashMap<String, f64>,
    pub pagination: PaginationResponse,
}

impl ComponentTvlRequestResponse {
    pub fn new(tvl: HashMap<String, f64>, pagination: PaginationResponse) -> Self {
        Self { tvl, pagination }
    }
}

#[derive(
    Serialize, Deserialize, Debug, Default, PartialEq, ToSchema, Eq, Hash, Clone, DeepSizeOf,
)]
pub struct TracedEntryPointRequestBody {
    #[serde(default)]
    pub chain: Chain,
    /// Filters by protocol, required to correctly apply unconfirmed state from
    /// ReorgBuffers
    pub protocol_system: String,
    /// Filter by component ids
    #[schema(value_type = Option<Vec<String>>)]
    pub component_ids: Option<Vec<ComponentId>>,
    /// Max page size supported is 100
    #[serde(default)]
    pub pagination: PaginationParams,
}

// When INCREASING these limits, please read the warning in the macro definition.
impl_pagination_limits!(TracedEntryPointRequestBody, compressed = 100, uncompressed = 100);

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, ToSchema, Eq, Hash, DeepSizeOf)]
pub struct EntryPoint {
    #[schema(example = "0xEdf63cce4bA70cbE74064b7687882E71ebB0e988:getRate()")]
    /// Entry point id.
    pub external_id: String,
    #[schema(value_type=String, example="0x8f4E8439b970363648421C692dd897Fb9c0Bd1D9")]
    #[serde(with = "hex_bytes")]
    /// The address of the contract to trace.
    pub target: Bytes,
    #[schema(example = "getRate()")]
    /// The signature of the function to trace.
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, Eq, Hash, DeepSizeOf)]
pub enum StorageOverride {
    /// Applies changes incrementally to the existing account storage.
    /// Only modifies the specific storage slots provided in the map while
    /// preserving all other storage slots.
    #[schema(value_type=HashMap<String, String>)]
    Diff(BTreeMap<StoreKey, StoreVal>),

    /// Completely replaces the account's storage state.
    /// Only the storage slots provided in the map will exist after the operation,
    /// and any existing storage slots not included will be cleared/zeroed.
    #[schema(value_type=HashMap<String, String>)]
    Replace(BTreeMap<StoreKey, StoreVal>),
}

impl From<models::blockchain::StorageOverride> for StorageOverride {
    fn from(value: models::blockchain::StorageOverride) -> Self {
        match value {
            models::blockchain::StorageOverride::Diff(diff) => StorageOverride::Diff(diff),
            models::blockchain::StorageOverride::Replace(replace) => {
                StorageOverride::Replace(replace)
            }
        }
    }
}

/// State overrides for an account.
///
/// Used to modify account state. Commonly used for testing contract interactions with specific
/// state conditions or simulating transactions with modified balances/code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, Eq, Hash, DeepSizeOf)]
pub struct AccountOverrides {
    /// Storage slots to override
    pub slots: Option<StorageOverride>,
    #[schema(value_type=Option<String>)]
    /// Native token balance override
    pub native_balance: Option<Balance>,
    #[schema(value_type=Option<String>)]
    /// Contract code override
    pub code: Option<Code>,
}

impl From<models::blockchain::AccountOverrides> for AccountOverrides {
    fn from(value: models::blockchain::AccountOverrides) -> Self {
        AccountOverrides {
            slots: value.slots.map(|s| s.into()),
            native_balance: value.native_balance,
            code: value.code,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, ToSchema, Eq, Hash, DeepSizeOf)]
pub struct RPCTracerParams {
    /// The caller address of the transaction, if not provided tracing uses the default value
    /// for an address defined by the VM.
    #[schema(value_type=Option<String>)]
    #[serde(with = "hex_bytes_option", default)]
    pub caller: Option<Bytes>,
    /// The call data used for the tracing call, this needs to include the function selector
    #[schema(value_type=String, example="0x679aefce")]
    #[serde(with = "hex_bytes")]
    pub calldata: Bytes,
    /// Optionally allow for state overrides so that the call works as expected
    pub state_overrides: Option<BTreeMap<Address, AccountOverrides>>,
    /// Addresses to prune from trace results. Useful for hooks that use mock
    /// accounts/routers that shouldn't be tracked in the final DCI results.
    #[schema(value_type=Option<Vec<String>>)]
    #[serde(default)]
    pub prune_addresses: Option<Vec<Address>>,
}

impl From<models::blockchain::RPCTracerParams> for RPCTracerParams {
    fn from(value: models::blockchain::RPCTracerParams) -> Self {
        RPCTracerParams {
            caller: value.caller,
            calldata: value.calldata,
            state_overrides: value.state_overrides.map(|overrides| {
                overrides
                    .into_iter()
                    .map(|(address, account_overrides)| (address, account_overrides.into()))
                    .collect()
            }),
            prune_addresses: value.prune_addresses,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Clone, Hash, DeepSizeOf, ToSchema)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum TracingParams {
    /// Uses RPC calls to retrieve the called addresses and retriggers
    RPCTracer(RPCTracerParams),
}

impl From<models::blockchain::TracingParams> for TracingParams {
    fn from(value: models::blockchain::TracingParams) -> Self {
        match value {
            models::blockchain::TracingParams::RPCTracer(params) => {
                TracingParams::RPCTracer(params.into())
            }
        }
    }
}

impl From<models::blockchain::EntryPoint> for EntryPoint {
    fn from(value: models::blockchain::EntryPoint) -> Self {
        Self { external_id: value.external_id, target: value.target, signature: value.signature }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, ToSchema, Eq, Clone, DeepSizeOf)]
pub struct EntryPointWithTracingParams {
    /// The entry point object
    pub entry_point: EntryPoint,
    /// The parameters used
    pub params: TracingParams,
}

impl From<models::blockchain::EntryPointWithTracingParams> for EntryPointWithTracingParams {
    fn from(value: models::blockchain::EntryPointWithTracingParams) -> Self {
        Self { entry_point: value.entry_point.into(), params: value.params.into() }
    }
}

#[derive(
    Debug, Clone, PartialEq, PartialOrd, Eq, Ord, Hash, Serialize, Deserialize, DeepSizeOf,
)]
pub struct AddressStorageLocation {
    pub key: StoreKey,
    pub offset: u8,
}

impl AddressStorageLocation {
    pub fn new(key: StoreKey, offset: u8) -> Self {
        Self { key, offset }
    }
}

impl From<models::blockchain::AddressStorageLocation> for AddressStorageLocation {
    fn from(value: models::blockchain::AddressStorageLocation) -> Self {
        Self { key: value.key, offset: value.offset }
    }
}

fn deserialize_retriggers_from_value(
    value: &serde_json::Value,
) -> Result<HashSet<(StoreKey, AddressStorageLocation)>, String> {
    use serde::Deserialize;
    use serde_json::Value;

    let mut result = HashSet::new();

    if let Value::Array(items) = value {
        for item in items {
            if let Value::Array(pair) = item {
                if pair.len() == 2 {
                    let key = StoreKey::deserialize(&pair[0])
                        .map_err(|e| format!("Failed to deserialize key: {}", e))?;

                    // Handle both old format (string) and new format (AddressStorageLocation)
                    let addr_storage = match &pair[1] {
                        Value::String(_) => {
                            // Old format: just a string key with offset defaulted to 0
                            let storage_key = StoreKey::deserialize(&pair[1]).map_err(|e| {
                                format!("Failed to deserialize old format storage key: {}", e)
                            })?;
                            AddressStorageLocation::new(storage_key, 12)
                        }
                        Value::Object(_) => {
                            // New format: AddressStorageLocation struct
                            AddressStorageLocation::deserialize(&pair[1]).map_err(|e| {
                                format!("Failed to deserialize AddressStorageLocation: {}", e)
                            })?
                        }
                        _ => return Err("Invalid retrigger format".to_string()),
                    };

                    result.insert((key, addr_storage));
                }
            }
        }
    }

    Ok(result)
}

#[derive(Serialize, Debug, Default, PartialEq, ToSchema, Eq, Clone, DeepSizeOf)]
pub struct TracingResult {
    #[schema(value_type=HashSet<(String, String)>)]
    pub retriggers: HashSet<(StoreKey, AddressStorageLocation)>,
    #[schema(value_type=HashMap<String,HashSet<String>>)]
    pub accessed_slots: HashMap<Address, HashSet<StoreKey>>,
}

/// Deserialize TracingResult with backward compatibility for retriggers
/// TODO: remove this after offset detection is deployed in production
impl<'de> Deserialize<'de> for TracingResult {
    fn deserialize<D>(deserializer: D) -> Result<TracingResult, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;
        use serde_json::Value;

        let value = Value::deserialize(deserializer)?;
        let mut result = TracingResult::default();

        if let Value::Object(map) = value {
            // Deserialize retriggers using our custom deserializer
            if let Some(retriggers_value) = map.get("retriggers") {
                result.retriggers =
                    deserialize_retriggers_from_value(retriggers_value).map_err(|e| {
                        D::Error::custom(format!("Failed to deserialize retriggers: {}", e))
                    })?;
            }

            // Deserialize accessed_slots normally
            if let Some(accessed_slots_value) = map.get("accessed_slots") {
                result.accessed_slots = serde_json::from_value(accessed_slots_value.clone())
                    .map_err(|e| {
                        D::Error::custom(format!("Failed to deserialize accessed_slots: {}", e))
                    })?;
            }
        }

        Ok(result)
    }
}

impl From<models::blockchain::TracingResult> for TracingResult {
    fn from(value: models::blockchain::TracingResult) -> Self {
        TracingResult {
            retriggers: value
                .retriggers
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            accessed_slots: value.accessed_slots,
        }
    }
}

#[derive(Serialize, PartialEq, ToSchema, Eq, Clone, Debug, Deserialize, DeepSizeOf)]
pub struct TracedEntryPointRequestResponse {
    /// Map of protocol component id to a list of a tuple containing each entry point with its
    /// tracing parameters and its corresponding tracing results.
    #[schema(value_type = HashMap<String, Vec<(EntryPointWithTracingParams, TracingResult)>>)]
    pub traced_entry_points:
        HashMap<ComponentId, Vec<(EntryPointWithTracingParams, TracingResult)>>,
    pub pagination: PaginationResponse,
}

#[derive(Serialize, Deserialize, Debug, Default, PartialEq, ToSchema, Eq, Clone)]
pub struct AddEntryPointRequestBody {
    #[serde(default)]
    pub chain: Chain,
    #[schema(value_type=String)]
    #[serde(default)]
    pub block_hash: Bytes,
    /// The map of component ids to their tracing params to insert
    #[schema(value_type = Vec<(String, Vec<EntryPointWithTracingParams>)>)]
    pub entry_points_with_tracing_data: Vec<(ComponentId, Vec<EntryPointWithTracingParams>)>,
}

#[derive(Serialize, PartialEq, ToSchema, Eq, Clone, Debug, Deserialize)]
pub struct AddEntryPointRequestResponse {
    /// Map of protocol component id to a list of a tuple containing each entry point with its
    /// tracing parameters and its corresponding tracing results.
    #[schema(value_type = HashMap<String, Vec<(EntryPointWithTracingParams, TracingResult)>>)]
    pub traced_entry_points:
        HashMap<ComponentId, Vec<(EntryPointWithTracingParams, TracingResult)>>,
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use maplit::hashmap;
    use rstest::rstest;

    use super::*;

    /// Test backward compatibility for Command::Subscribe compression field.
    /// Should default to false when not specified.
    #[rstest]
    #[case::legacy_format(None, false)]
    #[case::explicit_true(Some(true), true)]
    #[case::explicit_false(Some(false), false)]
    fn test_subscribe_compression_backward_compatibility(
        #[case] compression: Option<bool>,
        #[case] expected: bool,
    ) {
        use serde_json::json;

        let mut json_value = json!({
            "method": "subscribe",
            "extractor_id": {
                "chain": "ethereum",
                "name": "test"
            },
            "include_state": true
        });

        if let Some(value) = compression {
            json_value["compression"] = json!(value);
        }

        let command: Command =
            serde_json::from_value(json_value).expect("Failed to deserialize Subscribe command");

        if let Command::Subscribe { compression, .. } = command {
            assert_eq!(compression, expected);
        } else {
            panic!("Expected Subscribe command");
        }
    }

    /// Test backward compatibility for Command::Subscribe partial_blocks field.
    /// Should default to false when not specified.
    #[rstest]
    #[case::legacy_format(None, false)]
    #[case::explicit_true(Some(true), true)]
    #[case::explicit_false(Some(false), false)]
    fn test_subscribe_partial_blocks_backward_compatibility(
        #[case] partial_blocks: Option<bool>,
        #[case] expected: bool,
    ) {
        use serde_json::json;

        let mut json_value = json!({
            "method": "subscribe",
            "extractor_id": {
                "chain": "ethereum",
                "name": "test"
            },
            "include_state": true
        });

        if let Some(value) = partial_blocks {
            json_value["partial_blocks"] = json!(value);
        }

        let command: Command =
            serde_json::from_value(json_value).expect("Failed to deserialize Subscribe command");

        if let Command::Subscribe { partial_blocks, .. } = command {
            assert_eq!(partial_blocks, expected);
        } else {
            panic!("Expected Subscribe command");
        }
    }

    /// Test backward compatibility for ProtocolSystemsRequestResponse dci_protocols field.
    /// Should default to empty vec when not specified (old server responses).
    #[rstest]
    #[case::legacy_format(None, vec![])]
    #[case::with_dci(Some(vec!["vm:curve"]), vec!["vm:curve"])]
    #[case::empty_dci(Some(vec![]), vec![])]
    fn test_protocol_systems_dci_backward_compatibility(
        #[case] dci_protocols: Option<Vec<&str>>,
        #[case] expected: Vec<&str>,
    ) {
        use serde_json::json;

        let mut json_value = json!({
            "protocol_systems": ["uniswap_v2", "vm:curve"],
            "pagination": { "page": 0, "page_size": 20, "total": 2 }
        });

        if let Some(dci) = dci_protocols {
            json_value["dci_protocols"] = json!(dci);
        }

        let resp: ProtocolSystemsRequestResponse =
            serde_json::from_value(json_value).expect("Failed to deserialize response");

        assert_eq!(resp.dci_protocols, expected);

        // Verify round-trip
        let serialized = serde_json::to_string(&resp).unwrap();
        let round_tripped: ProtocolSystemsRequestResponse =
            serde_json::from_str(&serialized).unwrap();
        assert_eq!(resp, round_tripped);
    }

    #[test]
    fn test_tracing_result_backward_compatibility() {
        use serde_json::json;

        // Test old format (string storage locations)
        let old_format_json = json!({
            "retriggers": [
                ["0x01", "0x02"],
                ["0x03", "0x04"]
            ],
            "accessed_slots": {
                "0x05": ["0x06", "0x07"]
            }
        });

        let result: TracingResult = serde_json::from_value(old_format_json).unwrap();

        // Check that retriggers were deserialized correctly with offset 0
        assert_eq!(result.retriggers.len(), 2);
        let retriggers_vec: Vec<_> = result.retriggers.iter().collect();
        assert!(retriggers_vec.iter().any(|(k, v)| {
            k == &Bytes::from("0x01") && v.key == Bytes::from("0x02") && v.offset == 12
        }));
        assert!(retriggers_vec.iter().any(|(k, v)| {
            k == &Bytes::from("0x03") && v.key == Bytes::from("0x04") && v.offset == 12
        }));

        // Test new format (AddressStorageLocation objects)
        let new_format_json = json!({
            "retriggers": [
                ["0x01", {"key": "0x02", "offset": 12}],
                ["0x03", {"key": "0x04", "offset": 5}]
            ],
            "accessed_slots": {
                "0x05": ["0x06", "0x07"]
            }
        });

        let result2: TracingResult = serde_json::from_value(new_format_json).unwrap();

        // Check that new format retriggers were deserialized correctly with proper offsets
        assert_eq!(result2.retriggers.len(), 2);
        let retriggers_vec2: Vec<_> = result2.retriggers.iter().collect();
        assert!(retriggers_vec2.iter().any(|(k, v)| {
            k == &Bytes::from("0x01") && v.key == Bytes::from("0x02") && v.offset == 12
        }));
        assert!(retriggers_vec2.iter().any(|(k, v)| {
            k == &Bytes::from("0x03") && v.key == Bytes::from("0x04") && v.offset == 5
        }));
    }

    #[rstest]
    #[case::legacy_format(None, None)]
    #[case::full_block(Some(None), None)]
    #[case::partial_block(Some(Some(1)), Some(1))]
    fn test_block_changes_is_partial_backward_compatibility(
        #[case] has_partial_value: Option<Option<u32>>,
        #[case] expected: Option<u32>,
    ) {
        use serde_json::json;

        let mut json_value = json!({
            "extractor": "test_extractor",
            "chain": "ethereum",
            "block": {
                "number": 100,
                "hash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
                "parent_hash": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
                "chain": "ethereum",
                "ts": "2024-01-01T00:00:00"
            },
            "finalized_block_height": 99,
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
        });

        // Add is_partial field only if specified
        if let Some(partial_value) = has_partial_value {
            json_value["partial_block_index"] = json!(partial_value);
        }

        let block_changes: BlockAggregatedChanges = serde_json::from_value(json_value)
            .expect("Failed to deserialize BlockAggregatedChanges");

        assert_eq!(block_changes.partial_block_index, expected);
    }

    #[test]
    fn test_protocol_components_equality() {
        let body1 = ProtocolComponentsRequestBody {
            protocol_system: "protocol1".to_string(),
            component_ids: Some(vec!["component1".to_string(), "component2".to_string()]),
            tvl_gt: Some(1000.0),
            chain: Chain::Ethereum,
            pagination: PaginationParams::default(),
        };

        let body2 = ProtocolComponentsRequestBody {
            protocol_system: "protocol1".to_string(),
            component_ids: Some(vec!["component1".to_string(), "component2".to_string()]),
            tvl_gt: Some(1000.0 + 1e-7), // Within the tolerance ±1e-6
            chain: Chain::Ethereum,
            pagination: PaginationParams::default(),
        };

        // These should be considered equal due to the tolerance in tvl_gt
        assert_eq!(body1, body2);
    }

    #[test]
    fn test_protocol_components_inequality() {
        let body1 = ProtocolComponentsRequestBody {
            protocol_system: "protocol1".to_string(),
            component_ids: Some(vec!["component1".to_string(), "component2".to_string()]),
            tvl_gt: Some(1000.0),
            chain: Chain::Ethereum,
            pagination: PaginationParams::default(),
        };

        let body2 = ProtocolComponentsRequestBody {
            protocol_system: "protocol1".to_string(),
            component_ids: Some(vec!["component1".to_string(), "component2".to_string()]),
            tvl_gt: Some(1000.0 + 1e-5), // Outside the tolerance ±1e-6
            chain: Chain::Ethereum,
            pagination: PaginationParams::default(),
        };

        // These should not be equal due to the difference in tvl_gt
        assert_ne!(body1, body2);
    }

    #[test]
    fn test_parse_state_request() {
        let json_str = r#"
    {
        "contractIds": [
            "0xb4eccE46b8D4e4abFd03C9B806276A6735C9c092"
        ],
        "protocol_system": "uniswap_v2",
        "version": {
            "timestamp": "2069-01-01T04:20:00",
            "block": {
                "hash": "0x24101f9cb26cd09425b52da10e8c2f56ede94089a8bbe0f31f1cda5f4daa52c4",
                "number": 213,
                "chain": "ethereum"
            }
        }
    }
    "#;

        let result: StateRequestBody = serde_json::from_str(json_str).unwrap();

        let contract0 = "b4eccE46b8D4e4abFd03C9B806276A6735C9c092"
            .parse()
            .unwrap();
        let block_hash = "24101f9cb26cd09425b52da10e8c2f56ede94089a8bbe0f31f1cda5f4daa52c4"
            .parse()
            .unwrap();
        let block_number = 213;

        let expected_timestamp =
            NaiveDateTime::parse_from_str("2069-01-01T04:20:00", "%Y-%m-%dT%H:%M:%S").unwrap();

        let expected = StateRequestBody {
            contract_ids: Some(vec![contract0]),
            protocol_system: "uniswap_v2".to_string(),
            version: VersionParam {
                timestamp: Some(expected_timestamp),
                block: Some(BlockParam {
                    hash: Some(block_hash),
                    chain: Some(Chain::Ethereum),
                    number: Some(block_number),
                }),
            },
            chain: Chain::Ethereum,
            pagination: PaginationParams::default(),
        };

        assert_eq!(result, expected);
    }

    #[test]
    fn test_parse_state_request_dual_interface() {
        let json_common = r#"
    {
        "__CONTRACT_IDS__": [
            "0xb4eccE46b8D4e4abFd03C9B806276A6735C9c092"
        ],
        "version": {
            "timestamp": "2069-01-01T04:20:00",
            "block": {
                "hash": "0x24101f9cb26cd09425b52da10e8c2f56ede94089a8bbe0f31f1cda5f4daa52c4",
                "number": 213,
                "chain": "ethereum"
            }
        }
    }
    "#;

        let json_str_snake = json_common.replace("\"__CONTRACT_IDS__\"", "\"contract_ids\"");
        let json_str_camel = json_common.replace("\"__CONTRACT_IDS__\"", "\"contractIds\"");

        let snake: StateRequestBody = serde_json::from_str(&json_str_snake).unwrap();
        let camel: StateRequestBody = serde_json::from_str(&json_str_camel).unwrap();

        assert_eq!(snake, camel);
    }

    #[test]
    fn test_parse_state_request_unknown_field() {
        let body = r#"
    {
        "contract_ids_with_typo_error": [
            {
                "address": "0xb4eccE46b8D4e4abFd03C9B806276A6735C9c092",
                "chain": "ethereum"
            }
        ],
        "version": {
            "timestamp": "2069-01-01T04:20:00",
            "block": {
                "hash": "0x24101f9cb26cd09425b52da10e8c2f56ede94089a8bbe0f31f1cda5f4daa52c4",
                "parentHash": "0x8d75152454e60413efe758cc424bfd339897062d7e658f302765eb7b50971815",
                "number": 213,
                "chain": "ethereum"
            }
        }
    }
    "#;

        let decoded = serde_json::from_str::<StateRequestBody>(body);

        assert!(decoded.is_err(), "Expected an error due to unknown field");

        if let Err(e) = decoded {
            assert!(
                e.to_string()
                    .contains("unknown field `contract_ids_with_typo_error`"),
                "Error message does not contain expected unknown field information"
            );
        }
    }

    #[test]
    fn test_parse_state_request_no_contract_specified() {
        let json_str = r#"
    {
        "protocol_system": "uniswap_v2",
        "version": {
            "timestamp": "2069-01-01T04:20:00",
            "block": {
                "hash": "0x24101f9cb26cd09425b52da10e8c2f56ede94089a8bbe0f31f1cda5f4daa52c4",
                "number": 213,
                "chain": "ethereum"
            }
        }
    }
    "#;

        let result: StateRequestBody = serde_json::from_str(json_str).unwrap();

        let block_hash = "24101f9cb26cd09425b52da10e8c2f56ede94089a8bbe0f31f1cda5f4daa52c4".into();
        let block_number = 213;
        let expected_timestamp =
            NaiveDateTime::parse_from_str("2069-01-01T04:20:00", "%Y-%m-%dT%H:%M:%S").unwrap();

        let expected = StateRequestBody {
            contract_ids: None,
            protocol_system: "uniswap_v2".to_string(),
            version: VersionParam {
                timestamp: Some(expected_timestamp),
                block: Some(BlockParam {
                    hash: Some(block_hash),
                    chain: Some(Chain::Ethereum),
                    number: Some(block_number),
                }),
            },
            chain: Chain::Ethereum,
            pagination: PaginationParams { page: 0, page_size: 100 },
        };

        assert_eq!(result, expected);
    }

    #[rstest]
    #[case::deprecated_ids(
        r#"
    {
        "protocol_ids": [
            {
                "id": "0xb4eccE46b8D4e4abFd03C9B806276A6735C9c092",
                "chain": "ethereum"
            }
        ],
        "protocol_system": "uniswap_v2",
        "include_balances": false,
        "version": {
            "timestamp": "2069-01-01T04:20:00",
            "block": {
                "hash": "0x24101f9cb26cd09425b52da10e8c2f56ede94089a8bbe0f31f1cda5f4daa52c4",
                "number": 213,
                "chain": "ethereum"
            }
        }
    }
    "#
    )]
    #[case(
        r#"
    {
        "protocolIds": [
            "0xb4eccE46b8D4e4abFd03C9B806276A6735C9c092"
        ],
        "protocol_system": "uniswap_v2",
        "include_balances": false,
        "version": {
            "timestamp": "2069-01-01T04:20:00",
            "block": {
                "hash": "0x24101f9cb26cd09425b52da10e8c2f56ede94089a8bbe0f31f1cda5f4daa52c4",
                "number": 213,
                "chain": "ethereum"
            }
        }
    }
    "#
    )]
    fn test_parse_protocol_state_request(#[case] json_str: &str) {
        let result: ProtocolStateRequestBody = serde_json::from_str(json_str).unwrap();

        let block_hash = "24101f9cb26cd09425b52da10e8c2f56ede94089a8bbe0f31f1cda5f4daa52c4"
            .parse()
            .unwrap();
        let block_number = 213;

        let expected_timestamp =
            NaiveDateTime::parse_from_str("2069-01-01T04:20:00", "%Y-%m-%dT%H:%M:%S").unwrap();

        let expected = ProtocolStateRequestBody {
            protocol_ids: Some(vec!["0xb4eccE46b8D4e4abFd03C9B806276A6735C9c092".to_string()]),
            protocol_system: "uniswap_v2".to_string(),
            version: VersionParam {
                timestamp: Some(expected_timestamp),
                block: Some(BlockParam {
                    hash: Some(block_hash),
                    chain: Some(Chain::Ethereum),
                    number: Some(block_number),
                }),
            },
            chain: Chain::Ethereum,
            include_balances: false,
            pagination: PaginationParams::default(),
        };

        assert_eq!(result, expected);
    }

    #[rstest]
    #[case::with_protocol_ids(vec![ProtocolId { id: "id1".to_string(), chain: Chain::Ethereum }, ProtocolId { id: "id2".to_string(), chain: Chain::Ethereum }], vec!["id1".to_string(), "id2".to_string()])]
    #[case::with_strings(vec!["id1".to_string(), "id2".to_string()], vec!["id1".to_string(), "id2".to_string()])]
    fn test_id_filtered<T>(#[case] input_ids: Vec<T>, #[case] expected_ids: Vec<String>)
    where
        T: Into<String> + Clone,
    {
        let request_body = ProtocolStateRequestBody::id_filtered(input_ids);

        assert_eq!(request_body.protocol_ids, Some(expected_ids));
    }

    fn create_models_block_changes() -> crate::models::blockchain::BlockAggregatedChanges {
        let base_ts = 1694534400; // Example base timestamp for 2023-09-14T00:00:00

        crate::models::blockchain::BlockAggregatedChanges {
            extractor: "native_name".to_string(),
            block: models::blockchain::Block::new(
                3,
                models::Chain::Ethereum,
                Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000003").unwrap(),
                Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000002").unwrap(),
                chrono::DateTime::from_timestamp(base_ts + 3000, 0).unwrap().naive_utc(),
            ),
            db_committed_block_height: Some(1),
            finalized_block_height: 1,
            revert: true,
            state_deltas: HashMap::from([
                ("pc_1".to_string(), models::protocol::ProtocolComponentStateDelta {
                    component_id: "pc_1".to_string(),
                    updated_attributes: HashMap::from([
                        ("attr_2".to_string(), Bytes::from("0x0000000000000002")),
                        ("attr_1".to_string(), Bytes::from("0x00000000000003e8")),
                    ]),
                    deleted_attributes: HashSet::new(),
                    ..Default::default()
                }),
            ]),
            new_protocol_components: HashMap::from([
                ("pc_2".to_string(), crate::models::protocol::ProtocolComponent {
                    id: "pc_2".to_string(),
                    protocol_system: "native_protocol_system".to_string(),
                    protocol_type_name: "pt_1".to_string(),
                    chain: models::Chain::Ethereum,
                    tokens: vec![
                        Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap(),
                        Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
                    ],
                    contract_addresses: vec![],
                    static_attributes: HashMap::new(),
                    change: models::ChangeType::Creation,
                    creation_tx: Bytes::from_str("0x000000000000000000000000000000000000000000000000000000000000c351").unwrap(),
                    created_at: chrono::DateTime::from_timestamp(base_ts + 5000, 0).unwrap().naive_utc(),
                }),
            ]),
            deleted_protocol_components: HashMap::from([
                ("pc_3".to_string(), crate::models::protocol::ProtocolComponent {
                    id: "pc_3".to_string(),
                    protocol_system: "native_protocol_system".to_string(),
                    protocol_type_name: "pt_2".to_string(),
                    chain: models::Chain::Ethereum,
                    tokens: vec![
                        Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
                        Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(),
                    ],
                    contract_addresses: vec![],
                    static_attributes: HashMap::new(),
                    change: models::ChangeType::Deletion,
                    creation_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000009c41").unwrap(),
                    created_at: chrono::DateTime::from_timestamp(base_ts + 4000, 0).unwrap().naive_utc(),
                }),
            ]),
            component_balances: HashMap::from([
                ("pc_1".to_string(), HashMap::from([
                    (Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(), models::protocol::ComponentBalance {
                        token: Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
                        balance: Bytes::from("0x00000001"),
                        balance_float: 1.0,
                        modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
                        component_id: "pc_1".to_string(),
                    }),
                    (Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(), models::protocol::ComponentBalance {
                        token: Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(),
                        balance: Bytes::from("0x000003e8"),
                        balance_float: 1000.0,
                        modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000007531").unwrap(),
                        component_id: "pc_1".to_string(),
                    }),
                ])),
            ]),
            account_balances: HashMap::from([
                (Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(), HashMap::from([
                    (Bytes::from_str("0x7a250d5630b4cf539739df2c5dacb4c659f2488d").unwrap(), models::contract::AccountBalance {
                        account: Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(),
                        token: Bytes::from_str("0x7a250d5630b4cf539739df2c5dacb4c659f2488d").unwrap(),
                        balance: Bytes::from("0x000003e8"),
                        modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000007531").unwrap(),
                    }),
                    ])),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn test_serialize_deserialize_block_changes() {
        // Test that models::BlockAggregatedChanges serialized as json can be deserialized as
        // dto::BlockAggregatedChanges.

        // Create a models::BlockAggregatedChanges instance
        let block_entity_changes = create_models_block_changes();

        // Serialize the struct into JSON
        let json_data = serde_json::to_string(&block_entity_changes).expect("Failed to serialize");

        // Deserialize the JSON back into a dto::BlockAggregatedChanges struct
        serde_json::from_str::<BlockAggregatedChanges>(&json_data).expect("parsing failed");
    }

    #[test]
    fn test_parse_block_changes() {
        let json_data = r#"
        {
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
                            "balance": "0xb77831d23691653a01",
                            "balance_float": 3.3844151001790677e21,
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
                "new_entrypoints": {
                    "component_1": [
                        {
                            "external_id": "0x01:sig()",
                            "target": "0x01",
                            "signature": "sig()"
                        }
                    ]
                },
                "new_entrypoint_params": {
                    "0x01:sig()": [
                        [
                            {
                                "method": "rpctracer",
                                "caller": "0x01",
                                "calldata": "0x02"
                            },
                            "component_1"
                        ]
                    ]
                },
                "trace_results": {
                    "0x01:sig()": {
                        "retriggers": [
                            ["0x01", {"key": "0x02", "offset": 12}]
                        ],
                        "accessed_slots": {
                            "0x03": ["0x03", "0x04"]
                        }
                    }
                }
            }
        }
        "#;

        serde_json::from_str::<BlockAggregatedChanges>(json_data).expect("parsing failed");
    }

    #[test]
    fn test_parse_websocket_message() {
        let json_data = r#"
        {
            "subscription_id": "5d23bfbe-89ad-4ea3-8672-dc9e973ac9dc",
            "deltas": {
                "type": "BlockAggregatedChanges",
                "extractor": "uniswap_v2",
                "chain": "ethereum",
                "block": {
                    "number": 19291517,
                    "hash": "0xbc3ea4896c0be8da6229387a8571b72818aa258daf4fab46471003ad74c4ee83",
                    "parent_hash": "0x89ca5b8d593574cf6c886f41ef8208bf6bdc1a90ef36046cb8c84bc880b9af8f",
                    "chain": "ethereum",
                    "ts": "2024-02-23T16:35:35"
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
                    "0xde6faedbcae38eec6d33ad61473a04a6dd7f6e28": {
                        "component_id": "0xde6faedbcae38eec6d33ad61473a04a6dd7f6e28",
                        "updated_attributes": {
                            "reserve0": "0x87f7b5973a7f28a8b32404",
                            "reserve1": "0x09e9564b11"
                        },
                        "deleted_attributes": []
                    },
                    "0x99c59000f5a76c54c4fd7d82720c045bdcf1450d": {
                        "component_id": "0x99c59000f5a76c54c4fd7d82720c045bdcf1450d",
                        "updated_attributes": {
                            "reserve1": "0x44d9a8fd662c2f4d03",
                            "reserve0": "0x500b1261f811d5bf423e"
                        },
                        "deleted_attributes": []
                    }
                },
                "new_protocol_components": {},
                "deleted_protocol_components": {},
                "component_balances": {
                    "0x99c59000f5a76c54c4fd7d82720c045bdcf1450d": {
                        "0x9012744b7a564623b6c3e40b144fc196bdedf1a9": {
                            "token": "0x9012744b7a564623b6c3e40b144fc196bdedf1a9",
                            "balance": "0x500b1261f811d5bf423e",
                            "balance_float": 3.779935574269033E23,
                            "modify_tx": "0xe46c4db085fb6c6f3408a65524555797adb264e1d5cf3b66ad154598f85ac4bf",
                            "component_id": "0x99c59000f5a76c54c4fd7d82720c045bdcf1450d"
                        },
                        "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": {
                            "token": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                            "balance": "0x44d9a8fd662c2f4d03",
                            "balance_float": 1.270062661329837E21,
                            "modify_tx": "0xe46c4db085fb6c6f3408a65524555797adb264e1d5cf3b66ad154598f85ac4bf",
                            "component_id": "0x99c59000f5a76c54c4fd7d82720c045bdcf1450d"
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
                "component_tvl": {},
                "dci_update": {
                    "new_entrypoints": {
                        "0xde6faedbcae38eec6d33ad61473a04a6dd7f6e28": [
                            {
                                "external_id": "0x01:sig()",
                                "target": "0x01",
                                "signature": "sig()"
                            }
                        ]
                    },
                    "new_entrypoint_params": {
                        "0x01:sig()": [
                            [
                                {
                                    "method": "rpctracer",
                                    "caller": "0x01",
                                    "calldata": "0x02"
                                },
                                "0xde6faedbcae38eec6d33ad61473a04a6dd7f6e28"
                            ]
                        ]
                    },
                    "trace_results": {
                        "0x01:sig()": {
                            "retriggers": [
                                ["0x01", {"key": "0x02", "offset": 12}]
                            ],
                            "accessed_slots": {
                                "0x03": ["0x03", "0x04"]
                            }
                        }
                    }
                }
            }
        }
        "#;
        serde_json::from_str::<WebSocketMessage>(json_data).expect("parsing failed");
    }

    #[test]
    fn test_protocol_state_delta_merge_update_delete() {
        // Initialize ProtocolStateDelta instances
        let mut delta1 = ProtocolStateDelta {
            component_id: "Component1".to_string(),
            updated_attributes: HashMap::from([(
                "Attribute1".to_string(),
                Bytes::from("0xbadbabe420"),
            )]),
            deleted_attributes: HashSet::new(),
        };
        let delta2 = ProtocolStateDelta {
            component_id: "Component1".to_string(),
            updated_attributes: HashMap::from([(
                "Attribute2".to_string(),
                Bytes::from("0x0badbabe"),
            )]),
            deleted_attributes: HashSet::from(["Attribute1".to_string()]),
        };
        let exp = ProtocolStateDelta {
            component_id: "Component1".to_string(),
            updated_attributes: HashMap::from([(
                "Attribute2".to_string(),
                Bytes::from("0x0badbabe"),
            )]),
            deleted_attributes: HashSet::from(["Attribute1".to_string()]),
        };

        delta1.merge(&delta2);

        assert_eq!(delta1, exp);
    }

    #[test]
    fn test_protocol_state_delta_merge_delete_update() {
        // Initialize ProtocolStateDelta instances
        let mut delta1 = ProtocolStateDelta {
            component_id: "Component1".to_string(),
            updated_attributes: HashMap::new(),
            deleted_attributes: HashSet::from(["Attribute1".to_string()]),
        };
        let delta2 = ProtocolStateDelta {
            component_id: "Component1".to_string(),
            updated_attributes: HashMap::from([(
                "Attribute1".to_string(),
                Bytes::from("0x0badbabe"),
            )]),
            deleted_attributes: HashSet::new(),
        };
        let exp = ProtocolStateDelta {
            component_id: "Component1".to_string(),
            updated_attributes: HashMap::from([(
                "Attribute1".to_string(),
                Bytes::from("0x0badbabe"),
            )]),
            deleted_attributes: HashSet::new(),
        };

        delta1.merge(&delta2);

        assert_eq!(delta1, exp);
    }

    #[test]
    fn test_account_update_merge() {
        // Initialize AccountUpdate instances with same address and valid hex strings for Bytes
        let mut account1 = AccountUpdate::new(
            Bytes::from(b"0x1234"),
            Chain::Ethereum,
            HashMap::from([(Bytes::from("0xaabb"), Bytes::from("0xccdd"))]),
            Some(Bytes::from("0x1000")),
            Some(Bytes::from("0xdeadbeaf")),
            ChangeType::Creation,
        );

        let account2 = AccountUpdate::new(
            Bytes::from(b"0x1234"), // Same id as account1
            Chain::Ethereum,
            HashMap::from([(Bytes::from("0xeeff"), Bytes::from("0x11223344"))]),
            Some(Bytes::from("0x2000")),
            Some(Bytes::from("0xcafebabe")),
            ChangeType::Update,
        );

        // Merge account2 into account1
        account1.merge(&account2);

        // Define the expected state after merge
        let expected = AccountUpdate::new(
            Bytes::from(b"0x1234"), // Same id as before the merge
            Chain::Ethereum,
            HashMap::from([
                (Bytes::from("0xaabb"), Bytes::from("0xccdd")), // Original slot from account1
                (Bytes::from("0xeeff"), Bytes::from("0x11223344")), // New slot from account2
            ]),
            Some(Bytes::from("0x2000")),     // Updated balance
            Some(Bytes::from("0xcafebabe")), // Updated code
            ChangeType::Creation,            // Updated change type
        );

        // Assert the new account1 equals to the expected state
        assert_eq!(account1, expected);
    }

    #[test]
    fn test_block_account_changes_merge() {
        // Prepare account updates
        let old_account_updates: HashMap<Bytes, AccountUpdate> = [(
            Bytes::from("0x0011"),
            AccountUpdate {
                address: Bytes::from("0x00"),
                chain: Chain::Ethereum,
                slots: HashMap::from([(Bytes::from("0x0022"), Bytes::from("0x0033"))]),
                balance: Some(Bytes::from("0x01")),
                code: Some(Bytes::from("0x02")),
                change: ChangeType::Creation,
            },
        )]
        .into_iter()
        .collect();
        let new_account_updates: HashMap<Bytes, AccountUpdate> = [(
            Bytes::from("0x0011"),
            AccountUpdate {
                address: Bytes::from("0x00"),
                chain: Chain::Ethereum,
                slots: HashMap::from([(Bytes::from("0x0044"), Bytes::from("0x0055"))]),
                balance: Some(Bytes::from("0x03")),
                code: Some(Bytes::from("0x04")),
                change: ChangeType::Update,
            },
        )]
        .into_iter()
        .collect();
        // Create initial and new BlockAccountChanges instances
        let block_account_changes_initial = BlockAggregatedChanges {
            extractor: "extractor1".to_string(),
            revert: false,
            account_updates: old_account_updates,
            ..Default::default()
        };

        let block_account_changes_new = BlockAggregatedChanges {
            extractor: "extractor2".to_string(),
            revert: true,
            account_updates: new_account_updates,
            ..Default::default()
        };

        // Merge the new BlockAggregatedChanges into the initial one
        let res = block_account_changes_initial.merge(block_account_changes_new);

        // Create the expected result of the merge operation
        let expected_account_updates: HashMap<Bytes, AccountUpdate> = [(
            Bytes::from("0x0011"),
            AccountUpdate {
                address: Bytes::from("0x00"),
                chain: Chain::Ethereum,
                slots: HashMap::from([
                    (Bytes::from("0x0044"), Bytes::from("0x0055")),
                    (Bytes::from("0x0022"), Bytes::from("0x0033")),
                ]),
                balance: Some(Bytes::from("0x03")),
                code: Some(Bytes::from("0x04")),
                change: ChangeType::Creation,
            },
        )]
        .into_iter()
        .collect();
        let block_account_changes_expected = BlockAggregatedChanges {
            extractor: "extractor1".to_string(),
            revert: true,
            account_updates: expected_account_updates,
            ..Default::default()
        };
        assert_eq!(res, block_account_changes_expected);
    }

    #[test]
    fn test_block_entity_changes_merge() {
        // Initialize two BlockAggregatedChanges instances with different details
        let block_entity_changes_result1 = BlockAggregatedChanges {
            extractor: String::from("extractor1"),
            revert: false,
            state_updates: hashmap! { "state1".to_string() => ProtocolStateDelta::default() },
            new_protocol_components: hashmap! { "component1".to_string() => ProtocolComponent::default() },
            deleted_protocol_components: HashMap::new(),
            component_balances: hashmap! {
                "component1".to_string() => TokenBalances(hashmap! {
                    Bytes::from("0x01") => ComponentBalance {
                            token: Bytes::from("0x01"),
                            balance: Bytes::from("0x01"),
                            balance_float: 1.0,
                            modify_tx: Bytes::from("0x00"),
                            component_id: "component1".to_string()
                        },
                    Bytes::from("0x02") => ComponentBalance {
                        token: Bytes::from("0x02"),
                        balance: Bytes::from("0x02"),
                        balance_float: 2.0,
                        modify_tx: Bytes::from("0x00"),
                        component_id: "component1".to_string()
                    },
                })

            },
            component_tvl: hashmap! { "tvl1".to_string() => 1000.0 },
            ..Default::default()
        };
        let block_entity_changes_result2 = BlockAggregatedChanges {
            extractor: String::from("extractor2"),
            revert: true,
            state_updates: hashmap! { "state2".to_string() => ProtocolStateDelta::default() },
            new_protocol_components: hashmap! { "component2".to_string() => ProtocolComponent::default() },
            deleted_protocol_components: hashmap! { "component3".to_string() => ProtocolComponent::default() },
            component_balances: hashmap! {
                "component1".to_string() => TokenBalances::default(),
                "component2".to_string() => TokenBalances::default()
            },
            component_tvl: hashmap! { "tvl2".to_string() => 2000.0 },
            ..Default::default()
        };

        let res = block_entity_changes_result1.merge(block_entity_changes_result2);

        let expected_block_entity_changes_result = BlockAggregatedChanges {
            extractor: String::from("extractor1"),
            revert: true,
            state_updates: hashmap! {
                "state1".to_string() => ProtocolStateDelta::default(),
                "state2".to_string() => ProtocolStateDelta::default(),
            },
            new_protocol_components: hashmap! {
                "component1".to_string() => ProtocolComponent::default(),
                "component2".to_string() => ProtocolComponent::default(),
            },
            deleted_protocol_components: hashmap! {
                "component3".to_string() => ProtocolComponent::default(),
            },
            component_balances: hashmap! {
                "component1".to_string() => TokenBalances(hashmap! {
                    Bytes::from("0x01") => ComponentBalance {
                            token: Bytes::from("0x01"),
                            balance: Bytes::from("0x01"),
                            balance_float: 1.0,
                            modify_tx: Bytes::from("0x00"),
                            component_id: "component1".to_string()
                        },
                    Bytes::from("0x02") => ComponentBalance {
                        token: Bytes::from("0x02"),
                        balance: Bytes::from("0x02"),
                        balance_float: 2.0,
                        modify_tx: Bytes::from("0x00"),
                        component_id: "component1".to_string()
                        },
                    }),
                "component2".to_string() => TokenBalances::default(),
            },
            component_tvl: hashmap! {
                "tvl1".to_string() => 1000.0,
                "tvl2".to_string() => 2000.0
            },
            ..Default::default()
        };

        assert_eq!(res, expected_block_entity_changes_result);
    }

    #[test]
    fn test_websocket_error_serialization() {
        let extractor_id = ExtractorIdentity::new(Chain::Ethereum, "test_extractor");
        let subscription_id = Uuid::new_v4();

        // Test ExtractorNotFound serialization
        let error = WebsocketError::ExtractorNotFound(extractor_id.clone());
        let json = serde_json::to_string(&error).unwrap();
        let deserialized: WebsocketError = serde_json::from_str(&json).unwrap();
        assert_eq!(error, deserialized);

        // Test SubscriptionNotFound serialization
        let error = WebsocketError::SubscriptionNotFound(subscription_id);
        let json = serde_json::to_string(&error).unwrap();
        let deserialized: WebsocketError = serde_json::from_str(&json).unwrap();
        assert_eq!(error, deserialized);

        // Test ParseError serialization
        let error = WebsocketError::ParseError("{asd".to_string(), "invalid json".to_string());
        let json = serde_json::to_string(&error).unwrap();
        let deserialized: WebsocketError = serde_json::from_str(&json).unwrap();
        assert_eq!(error, deserialized);

        // Test SubscribeError serialization
        let error = WebsocketError::SubscribeError(extractor_id.clone());
        let json = serde_json::to_string(&error).unwrap();
        let deserialized: WebsocketError = serde_json::from_str(&json).unwrap();
        assert_eq!(error, deserialized);

        // Test CompressionError serialization
        let error =
            WebsocketError::CompressionError(subscription_id, "Compression failed".to_string());
        let json = serde_json::to_string(&error).unwrap();
        let deserialized: WebsocketError = serde_json::from_str(&json).unwrap();
        assert_eq!(error, deserialized);
    }

    #[test]
    fn test_websocket_message_with_error_response() {
        let error =
            WebsocketError::ParseError("}asdfas".to_string(), "malformed request".to_string());
        let response = Response::Error(error.clone());
        let message = WebSocketMessage::Response(response);

        let json = serde_json::to_string(&message).unwrap();
        let deserialized: WebSocketMessage = serde_json::from_str(&json).unwrap();

        if let WebSocketMessage::Response(Response::Error(deserialized_error)) = deserialized {
            assert_eq!(error, deserialized_error);
        } else {
            panic!("Expected WebSocketMessage::Response(Response::Error)");
        }
    }

    #[test]
    fn test_websocket_error_conversion_from_models() {
        use crate::models::error;

        let extractor_id =
            crate::models::ExtractorIdentity::new(crate::models::Chain::Ethereum, "test");
        let subscription_id = Uuid::new_v4();

        // Test ExtractorNotFound conversion
        let models_error = error::WebsocketError::ExtractorNotFound(extractor_id.clone());
        let dto_error: WebsocketError = models_error.into();
        assert_eq!(dto_error, WebsocketError::ExtractorNotFound(extractor_id.clone().into()));

        // Test SubscriptionNotFound conversion
        let models_error = error::WebsocketError::SubscriptionNotFound(subscription_id);
        let dto_error: WebsocketError = models_error.into();
        assert_eq!(dto_error, WebsocketError::SubscriptionNotFound(subscription_id));

        // Test ParseError conversion - create a real JSON parse error
        let json_result: Result<serde_json::Value, _> = serde_json::from_str("{invalid json");
        let json_error = json_result.unwrap_err();
        let models_error =
            error::WebsocketError::ParseError("{invalid json".to_string(), json_error);
        let dto_error: WebsocketError = models_error.into();
        if let WebsocketError::ParseError(msg, error_msg) = dto_error {
            // Just check that we have a non-empty error message
            assert!(!error_msg.is_empty(), "Error message should not be empty, got: '{}'", msg);
        } else {
            panic!("Expected ParseError variant");
        }

        // Test SubscribeError conversion
        let models_error = error::WebsocketError::SubscribeError(extractor_id.clone());
        let dto_error: WebsocketError = models_error.into();
        assert_eq!(dto_error, WebsocketError::SubscribeError(extractor_id.into()));

        // Test CompressionError conversion
        let io_error = std::io::Error::other("Compression failed");
        let models_error = error::WebsocketError::CompressionError(subscription_id, io_error);
        let dto_error: WebsocketError = models_error.into();
        if let WebsocketError::CompressionError(sub_id, msg) = &dto_error {
            assert_eq!(*sub_id, subscription_id);
            assert!(msg.contains("Compression failed"));
        } else {
            panic!("Expected CompressionError variant");
        }
    }
}

#[cfg(test)]
mod memory_size_tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn test_state_request_response_memory_size_empty() {
        let response = StateRequestResponse {
            accounts: vec![],
            pagination: PaginationResponse::new(1, 10, 0),
        };

        let size = response.deep_size_of();

        // Should at least include base struct sizes
        assert!(size >= 48, "Empty response should have minimum size of 48 bytes, got {}", size);
        assert!(size < 200, "Empty response should not be too large, got {}", size);
    }

    #[test]
    fn test_state_request_response_memory_size_scales_with_slots() {
        let create_response_with_slots = |slot_count: usize| {
            let mut slots = HashMap::new();
            for i in 0..slot_count {
                let key = vec![i as u8; 32]; // 32-byte key
                let value = vec![(i + 100) as u8; 32]; // 32-byte value
                slots.insert(key.into(), value.into());
            }

            let account = ResponseAccount::new(
                Chain::Ethereum,
                vec![1; 20].into(),
                "Pool".to_string(),
                slots,
                vec![1; 32].into(),
                HashMap::new(),
                vec![].into(), // empty code
                vec![1; 32].into(),
                vec![1; 32].into(),
                vec![1; 32].into(),
                None,
            );

            StateRequestResponse {
                accounts: vec![account],
                pagination: PaginationResponse::new(1, 10, 1),
            }
        };

        let small_response = create_response_with_slots(10);
        let large_response = create_response_with_slots(100);

        let small_size = small_response.deep_size_of();
        let large_size = large_response.deep_size_of();

        // Large response should be significantly bigger
        assert!(
            large_size > small_size * 5,
            "Large response ({} bytes) should be much larger than small response ({} bytes)",
            large_size,
            small_size
        );

        // Each slot should contribute at least 64 bytes (32 + 32 + overhead)
        let size_diff = large_size - small_size;
        let expected_min_diff = 90 * 64; // 90 additional slots * 64 bytes each
        assert!(
            size_diff > expected_min_diff,
            "Size difference ({} bytes) should reflect the additional slot data",
            size_diff
        );
    }
}

#[cfg(test)]
mod pagination_limits_tests {
    use super::*;

    // Test struct for pagination limits
    #[derive(Clone, Debug)]
    struct TestRequestBody {
        pagination: PaginationParams,
    }

    // Implement pagination limits for test struct
    impl_pagination_limits!(TestRequestBody, compressed = 500, uncompressed = 50);

    #[test]
    fn test_effective_max_page_size() {
        // Test effective max with compression enabled
        let max_size = TestRequestBody::effective_max_page_size(true);
        assert_eq!(max_size, 500, "Should return compressed limit when compression is enabled");

        // Test effective max with compression disabled
        let max_size = TestRequestBody::effective_max_page_size(false);
        assert_eq!(max_size, 50, "Should return uncompressed limit when compression is disabled");
    }
}
