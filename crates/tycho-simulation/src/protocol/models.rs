//! Pair Properties and ProtocolState
//!
//! This module contains the `ProtocolComponent` struct, which represents the
//! properties of a trading pair. It also contains the `Pair` struct, which
//! represents a trading pair with its properties and corresponding state.
//!
//! Additionally, it contains the `GetAmountOutResult` struct, which
//! represents the result of getting the amount out of a trading pair.
//!
//! The `ProtocolComponent` struct has two fields: `address` and `tokens`.
//! `address` is the address of the trading pair and `tokens` is a vector
//! of `ERC20Token` representing the tokens of the trading pair.
//!
//! Generally this struct contains immutable properties of the pair. These
//! are attributes that will never change - not even through governance.
//!
//! This is in contrast to `ProtocolState`, which includes ideally only
//! attributes that can change.
//!
//! The `Pair` struct combines the former two: `ProtocolComponent` and
//! `ProtocolState` into a single struct.
//!
//! # Note:
//! It's worth emphasizing that although the term "pair" used in this
//! module refers to a trading pair, it does not necessarily imply two
//! tokens only. Some pairs might have more than two tokens.
use std::{collections::HashMap, default::Default, future::Future};

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use tycho_client::feed::{HeaderLike, SynchronizerState};
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

/// Context struct containing attributes for decoders
///
/// This struct can be extended to include additional attributes for other decoders in the future
#[derive(Debug, Clone)]
pub struct DecoderContext {
    pub adapter_path: Option<String>,
    pub vm_traces: Option<bool>,
}

impl DecoderContext {
    pub fn new() -> Self {
        Self { adapter_path: None, vm_traces: None }
    }

    pub fn vm_adapter_path<S: Into<String>>(mut self, path: S) -> Self {
        self.adapter_path = Some(path.into());
        self
    }

    pub fn vm_traces(mut self, trace: bool) -> Self {
        self.vm_traces = Some(trace);
        self
    }
}

impl Default for DecoderContext {
    fn default() -> Self {
        Self::new()
    }
}

/// ProtocolComponent struct represents the properties of a trading pair
///
/// # Fields
///
/// * `address`: String, the address of the trading pair
/// * `tokens`: `Vec<ERC20Token>`, the tokens of the trading pair
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolComponent {
    #[deprecated(since = "0.73.0", note = "Use `id` instead")]
    pub address: Bytes,
    pub id: Bytes,
    pub tokens: Vec<Token>,
    pub protocol_system: String,
    pub protocol_type_name: String,
    pub chain: Chain,
    pub contract_ids: Vec<Bytes>,
    pub static_attributes: HashMap<String, Bytes>,
    pub creation_tx: Bytes,
    pub created_at: NaiveDateTime,
}

impl ProtocolComponent {
    #[allow(deprecated)]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Bytes,
        protocol_system: String,
        protocol_type_name: String,
        chain: Chain,
        tokens: Vec<Token>,
        contract_ids: Vec<Bytes>,
        static_attributes: HashMap<String, Bytes>,
        creation_tx: Bytes,
        created_at: NaiveDateTime,
    ) -> Self {
        ProtocolComponent {
            address: Default::default(),
            id,
            tokens,
            protocol_system,
            protocol_type_name,
            chain,
            contract_ids,
            static_attributes,
            creation_tx,
            created_at,
        }
    }

    pub fn from_with_tokens(
        core_model: tycho_common::models::protocol::ProtocolComponent,
        tokens: Vec<Token>,
    ) -> Self {
        let id = Bytes::from(core_model.id.as_str());
        ProtocolComponent::new(
            id.clone(),
            core_model.protocol_system,
            core_model.protocol_type_name,
            core_model.chain,
            tokens,
            core_model.contract_addresses,
            core_model.static_attributes,
            core_model.creation_tx,
            core_model.created_at,
        )
    }
}

impl From<ProtocolComponent> for tycho_common::models::protocol::ProtocolComponent {
    fn from(component: ProtocolComponent) -> Self {
        tycho_common::models::protocol::ProtocolComponent {
            id: hex::encode(component.id),
            protocol_system: component.protocol_system,
            protocol_type_name: component.protocol_type_name,
            chain: component.chain,
            tokens: component
                .tokens
                .into_iter()
                .map(|t| t.address)
                .collect(),
            static_attributes: component.static_attributes,
            change: Default::default(),
            creation_tx: component.creation_tx,
            created_at: component.created_at,
            contract_addresses: component.contract_ids,
        }
    }
}

pub trait TryFromWithBlock<T, H>
where
    H: HeaderLike,
{
    type Error;

    fn try_from_with_header(
        value: T,
        block: H,
        account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> impl Future<Output = Result<Self, Self::Error>> + Send + Sync
    where
        Self: Sized;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Update {
    pub block_number_or_timestamp: u64,
    /// True when this update is for a partial (pre-confirmation) block, false for full blocks.
    #[serde(default)]
    pub is_partial: bool,
    /// Synchronization state per protocol
    pub sync_states: HashMap<String, SynchronizerState>,
    /// The new and updated states of this block.
    /// VM-backed states that can't be serialized are silently skipped during
    /// serialization and will be absent after a roundtrip.
    #[serde(with = "crate::serde_helpers::protocol_states")]
    pub states: HashMap<String, Box<dyn ProtocolSim>>,
    /// The new pairs that were added in this block
    pub new_pairs: HashMap<String, ProtocolComponent>,
    /// The pairs that were removed in this block
    pub removed_pairs: HashMap<String, ProtocolComponent>,
}

impl Update {
    pub fn new(
        block_number: u64,
        states: HashMap<String, Box<dyn ProtocolSim>>,
        new_pairs: HashMap<String, ProtocolComponent>,
    ) -> Self {
        Update {
            block_number_or_timestamp: block_number,
            is_partial: false,
            sync_states: HashMap::new(),
            states,
            new_pairs,
            removed_pairs: HashMap::new(),
        }
    }

    pub fn set_is_partial(mut self, is_partial: bool) -> Self {
        self.is_partial = is_partial;
        self
    }

    pub fn set_removed_pairs(mut self, pairs: HashMap<String, ProtocolComponent>) -> Self {
        self.removed_pairs = pairs;
        self
    }

    pub fn set_sync_states(mut self, sync_states: HashMap<String, SynchronizerState>) -> Self {
        self.sync_states = sync_states;
        self
    }

    pub fn merge(mut self, other: Update) -> Self {
        self.states.extend(other.states);
        self.new_pairs.extend(other.new_pairs);
        self.removed_pairs
            .extend(other.removed_pairs);
        self
    }
}
