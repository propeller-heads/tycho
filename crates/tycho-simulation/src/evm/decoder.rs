use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use alloy::primitives::{Address, U256};
use thiserror::Error;
use tokio::sync::{RwLock, RwLockReadGuard};
use tracing::{debug, error, info, warn};
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader, FeedMessage, HeaderLike};
use tycho_common::{
    dto::{ChangeType, ProtocolStateDelta},
    models::{blockchain::BlockAggregatedChanges, token::Token, Chain},
    simulation::protocol_sim::{Balances, ProtocolSim},
    Bytes,
};
#[cfg(test)]
use {
    mockall::mock,
    num_bigint::BigUint,
    std::any::Any,
    tycho_common::simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::GetAmountOutResult,
    },
};

use crate::{
    evm::{
        engine_db::{update_engine, SHARED_TYCHO_DB},
        protocol::{
            utils::bytes_to_address,
            vm::{constants::ERC20_PROXY_BYTECODE, erc20_token::IMPLEMENTATION_SLOT},
        },
        tycho_models::{AccountUpdate, ResponseAccount},
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, ProtocolComponent, TryFromWithBlock, Update},
    },
};

#[derive(Error, Debug)]
pub enum StreamDecodeError {
    #[error("{0}")]
    Fatal(String),
}

#[derive(Default)]
struct DecoderState {
    tokens: HashMap<Bytes, Token>,
    states: HashMap<String, Box<dyn ProtocolSim>>,
    components: HashMap<String, ProtocolComponent>,
    // maps contract address to the pools they affect
    contracts_map: HashMap<Bytes, HashSet<String>>,
    // Maps original token address to their new proxy token address
    proxy_token_addresses: HashMap<Address, Address>,
    // Set of failed components, these are components that failed to decode and will not be emitted
    // again TODO: handle more gracefully inside tycho-client. We could fetch the snapshot and
    // try to decode it again.
    failed_components: HashSet<String>,
    // The block number of the last confirmed block decoded via `decode()`.
    current_block_number: u64,
}

type DecodeFut =
    Pin<Box<dyn Future<Output = Result<Box<dyn ProtocolSim>, InvalidSnapshotError>> + Send + Sync>>;
type AccountBalances = HashMap<Bytes, HashMap<Bytes, Bytes>>;
type RegistryFn<H> = dyn Fn(ComponentWithState, H, AccountBalances, Arc<RwLock<DecoderState>>) -> DecodeFut
    + Send
    + Sync;
type FilterFn = fn(&ComponentWithState) -> bool;

/// A decoder to process raw messages.
///
/// This struct decodes incoming messages of type `FeedMessage` and converts it into the
/// `BlockUpdate` struct.
///
/// # Important:
/// - Supports registering exchanges and their associated filters for specific protocol components.
/// - Allows the addition of client-side filters for custom conditions.
///
/// **Note:** Tokens provided via [`set_tokens`](Self::set_tokens) are used to decode startup
/// snapshots and initialize protocol states. This is not an ongoing filter — components arriving
/// after startup include their own token metadata.
pub struct TychoStreamDecoder<H>
where
    H: HeaderLike,
{
    state: Arc<RwLock<DecoderState>>,
    skip_state_decode_failures: bool,
    min_token_quality: u32,
    registry: HashMap<String, Box<RegistryFn<H>>>,
    inclusion_filters: HashMap<String, FilterFn>,
}

impl<H> Default for TychoStreamDecoder<H>
where
    H: HeaderLike + Clone + Sync + Send + 'static + std::fmt::Debug,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<H> TychoStreamDecoder<H>
where
    H: HeaderLike + Clone + Sync + Send + 'static + std::fmt::Debug,
{
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(DecoderState::default())),
            skip_state_decode_failures: false,
            min_token_quality: 100,
            registry: HashMap::new(),
            inclusion_filters: HashMap::new(),
        }
    }

    /// Provides token metadata used to decode startup snapshots and initialize protocol states.
    ///
    /// This is not an ongoing stream filter. Components arriving after startup include their
    /// own token metadata for decoding.
    pub async fn set_tokens(&self, tokens: HashMap<Bytes, Token>) {
        let mut guard = self.state.write().await;
        guard.tokens = tokens;
    }

    pub fn skip_state_decode_failures(&mut self, skip: bool) {
        self.skip_state_decode_failures = skip;
    }

    /// Sets the minimum token quality for decoding.
    ///
    /// Tokens arriving in stream deltas below this threshold are ignored. Defaults to 100.
    /// Set this to the same value used in [`load_all_tokens()`](crate::utils::load_all_tokens) to
    /// apply consistent filtering.
    pub fn min_token_quality(&mut self, quality: u32) {
        self.min_token_quality = quality;
    }

    /// Registers a decoder for a given exchange with a decoder context.
    ///
    /// This method maps an exchange identifier to a specific protocol simulation type.
    /// The associated type must implement the `TryFromWithBlock` trait to enable decoding
    /// of state updates from `ComponentWithState` objects. This allows the decoder to transform
    /// the component data into the appropriate protocol simulation type based on the current
    /// blockchain state and the provided block header.
    /// For example, to register a decoder for the `uniswap_v2` exchange with an additional decoder
    /// context, you must call this function with
    /// `register_decoder_with_context::<UniswapV2State>("uniswap_v2", context)`.
    /// This ensures that the exchange ID `uniswap_v2` is properly associated with the
    /// `UniswapV2State` decoder for use in the protocol stream.
    pub fn register_decoder_with_context<T>(&mut self, exchange: &str, context: DecoderContext)
    where
        T: ProtocolSim
            + TryFromWithBlock<ComponentWithState, H, Error = InvalidSnapshotError>
            + Send
            + 'static,
    {
        let decoder = Box::new(
            move |component: ComponentWithState,
                  header: H,
                  account_balances: AccountBalances,
                  state: Arc<RwLock<DecoderState>>| {
                let context = context.clone();
                Box::pin(async move {
                    let guard = state.read().await;
                    T::try_from_with_header(
                        component,
                        header,
                        &account_balances,
                        &guard.tokens,
                        &context,
                    )
                    .await
                    .map(|c| Box::new(c) as Box<dyn ProtocolSim>)
                }) as DecodeFut
            },
        );
        self.registry
            .insert(exchange.to_string(), decoder);
    }

    /// Registers a decoder for a given exchange.
    ///
    /// This method maps an exchange identifier to a specific protocol simulation type.
    /// The associated type must implement the `TryFromWithBlock` trait to enable decoding
    /// of state updates from `ComponentWithState` objects. This allows the decoder to transform
    /// the component data into the appropriate protocol simulation type based on the current
    /// blockchain state and the provided block header.
    /// For example, to register a decoder for the `uniswap_v2` exchange, you must call
    /// this function with `register_decoder::<UniswapV2State>("uniswap_v2", vm_attributes)`.
    /// This ensures that the exchange ID `uniswap_v2` is properly associated with the
    /// `UniswapV2State` decoder for use in the protocol stream.
    pub fn register_decoder<T>(&mut self, exchange: &str)
    where
        T: ProtocolSim
            + TryFromWithBlock<ComponentWithState, H, Error = InvalidSnapshotError>
            + Send
            + 'static,
    {
        let context = DecoderContext::new();
        self.register_decoder_with_context::<T>(exchange, context);
    }

    /// Registers a client-side filter function for a given exchange.
    ///
    /// Associates a filter function with an exchange ID, enabling custom filtering of protocol
    /// components. The filter function is applied client-side to refine the data received from the
    /// stream. It can be used to exclude certain components based on attributes or conditions that
    /// are not supported by the server-side filtering logic. This is particularly useful for
    /// implementing custom behaviors, such as:
    /// - Filtering out pools with specific attributes (e.g., unsupported features).
    /// - Blacklisting pools based on custom criteria.
    /// - Excluding pools that do not meet certain requirements (e.g., token pairs or liquidity
    ///   constraints).
    ///
    /// For example, you might use a filter to exclude pools that are not fully supported in the
    /// protocol, or to ignore pools with certain attributes that are irrelevant to your
    /// application.
    pub fn register_filter(&mut self, exchange: &str, predicate: FilterFn) {
        self.inclusion_filters
            .insert(exchange.to_string(), predicate);
    }

    /// Decodes a `FeedMessage` into a `BlockUpdate` containing the updated states of protocol
    /// components
    pub async fn decode(&self, msg: &FeedMessage<H>) -> Result<Update, StreamDecodeError> {
        // stores all states updated in this tick/msg
        let mut updated_states = HashMap::new();
        let mut new_pairs = HashMap::new();
        let mut removed_pairs = HashMap::new();
        let mut contracts_map = HashMap::new();
        let mut msg_failed_components = HashSet::new();

        let header = msg
            .state_msgs
            .values()
            .next()
            .ok_or_else(|| StreamDecodeError::Fatal("Missing block!".into()))?
            .header
            .clone();

        let block_number_or_timestamp = header
            .clone()
            .block_number_or_timestamp();
        let current_block = header.clone().block();
        let is_partial = current_block
            .as_ref()
            .map(|h| h.partial_block_index.is_some())
            .unwrap_or(false);

        for (protocol, protocol_msg) in msg.state_msgs.iter() {
            // Add any new tokens
            if let Some(deltas) = protocol_msg.deltas.as_ref() {
                let mut state_guard = self.state.write().await;

                let new_tokens = deltas
                    .new_tokens
                    .iter()
                    .filter(|(addr, t)| {
                        t.quality >= self.min_token_quality &&
                            !state_guard.tokens.contains_key(*addr)
                    })
                    .map(|(addr, t)| (addr.clone(), t.clone()))
                    .collect::<HashMap<Bytes, Token>>();

                if !new_tokens.is_empty() {
                    debug!(n = new_tokens.len(), "NewTokens");
                    state_guard.tokens.extend(new_tokens);
                }
            }

            // Remove untracked components
            {
                let mut state_guard = self.state.write().await;
                let removed_components: Vec<(String, ProtocolComponent)> = protocol_msg
                    .removed_components
                    .iter()
                    .map(|(id, comp)| {
                        if *id != comp.id {
                            error!(
                                "Component id mismatch in removed components {id} != {}",
                                comp.id
                            );
                            return Err(StreamDecodeError::Fatal("Component id mismatch".into()));
                        }

                        let tokens = comp
                            .tokens
                            .iter()
                            .flat_map(|addr| state_guard.tokens.get(addr).cloned())
                            .collect::<Vec<_>>();

                        if tokens.len() == comp.tokens.len() {
                            Ok(Some((
                                id.clone(),
                                ProtocolComponent::from_with_tokens(comp.clone(), tokens),
                            )))
                        } else {
                            Ok(None)
                        }
                    })
                    .collect::<Result<Vec<Option<(String, ProtocolComponent)>>, StreamDecodeError>>(
                    )?
                    .into_iter()
                    .flatten()
                    .collect();

                // Remove components from state and add to removed_pairs
                for (id, component) in removed_components {
                    state_guard.components.remove(&id);
                    state_guard.states.remove(&id);
                    removed_pairs.insert(id, component);
                }

                // UPDATE VM STORAGE
                info!(
                    "Processing {} contracts from snapshots",
                    protocol_msg
                        .snapshots
                        .get_vm_storage()
                        .len()
                );

                let mut proxy_token_accounts: HashMap<Address, AccountUpdate> = HashMap::new();
                let mut storage_by_address: HashMap<Address, ResponseAccount> = HashMap::new();
                for (key, value) in protocol_msg
                    .snapshots
                    .get_vm_storage()
                    .iter()
                {
                    let account: ResponseAccount = value.clone().into();

                    if state_guard.tokens.contains_key(key) {
                        let original_address = account.address;
                        // To work with Tycho's token overwrites system, if we get account
                        // snapshots for a token we must handle them with a proxy/wrapper
                        // contract.
                        // Note: storage for the original contract must be set at the proxy
                        // contract address. This is because the proxy contract uses
                        // delegatecall to the original (implementation) contract.

                        // Handle proxy token accounts
                        let (impl_addr, proxy_state) = match state_guard
                            .proxy_token_addresses
                            .get(&original_address)
                        {
                            Some(impl_addr) => {
                                // Token already has a proxy contract, simply update it.

                                // Note: we apply the snapshot as an update. This is to cover the
                                // case where a contract may be stale as it stopped being tracked
                                // for some reason (e.g. due to a drop in tvl) and is now being
                                // tracked again.
                                let proxy_state = AccountUpdate::new(
                                    original_address,
                                    value.chain,
                                    account.slots.clone(),
                                    Some(account.native_balance),
                                    None,
                                    ChangeType::Update,
                                );
                                (*impl_addr, proxy_state)
                            }
                            None => {
                                // Token does not have a proxy contract yet, create one

                                // Assign original token contract to new address
                                let impl_addr = generate_proxy_token_address(
                                    state_guard.proxy_token_addresses.len() as u32,
                                )?;
                                state_guard
                                    .proxy_token_addresses
                                    .insert(original_address, impl_addr);

                                // Add proxy token contract at original token address
                                let proxy_state = create_proxy_token_account(
                                    original_address,
                                    Some(impl_addr),
                                    &account.slots,
                                    value.chain,
                                    Some(account.native_balance),
                                );

                                (impl_addr, proxy_state)
                            }
                        };

                        proxy_token_accounts.insert(original_address, proxy_state);

                        // Assign original token contract to the implementation address
                        let impl_update = ResponseAccount {
                            address: impl_addr,
                            slots: HashMap::new(),
                            ..account.clone()
                        };
                        storage_by_address.insert(impl_addr, impl_update);
                    } else {
                        // Not a token, apply snapshot to the account at its original address
                        storage_by_address.insert(account.address, account);
                    }
                }

                // Split proxy accounts by change type:
                // - Creation: new proxies that must overwrite any existing placeholder
                // - Update: existing proxies whose storage is being refreshed (handled normally)
                let mut proxy_creates: Vec<AccountUpdate> = Vec::new();
                let mut proxy_updates: HashMap<Address, AccountUpdate> = HashMap::new();
                for (addr, update) in proxy_token_accounts {
                    if matches!(update.change, ChangeType::Creation) {
                        proxy_creates.push(update);
                    } else {
                        proxy_updates.insert(addr, update);
                    }
                }

                info!("Updating engine with {} contracts from snapshots", storage_by_address.len());
                update_engine(
                    SHARED_TYCHO_DB.clone(),
                    header.clone().block(),
                    Some(storage_by_address),
                    proxy_updates,
                )
                .map_err(|e| StreamDecodeError::Fatal(e.to_string()))?;

                // Force-overwrite new proxy token accounts so that authoritative vm_storage data
                // always wins over any empty placeholder previously inserted by another
                // decoder's snapshot loop (which uses init_account / init-if-not-exists).
                if !proxy_creates.is_empty() {
                    SHARED_TYCHO_DB
                        .force_update_accounts(proxy_creates)
                        .map_err(|e| StreamDecodeError::Fatal(e.to_string()))?;
                }
                info!("Engine updated");
                drop(state_guard);
            }

            // Construct a contract to token balances map: HashMap<ContractAddress,
            // HashMap<TokenAddress, Balance>>
            let account_balances = protocol_msg
                .clone()
                .snapshots
                .get_vm_storage()
                .iter()
                .filter_map(|(addr, acc)| {
                    if acc.token_balances.is_empty() {
                        return None;
                    }
                    let balances = acc
                        .token_balances
                        .iter()
                        .map(|(token_addr, ab)| (token_addr.clone(), ab.balance.clone()))
                        .collect::<HashMap<Bytes, Bytes>>();
                    Some((addr.clone(), balances))
                })
                .collect::<AccountBalances>();

            let mut new_components = HashMap::new();
            let mut count_token_skips = 0;
            let mut components_to_store = HashMap::new();
            {
                let state_guard = self.state.read().await;

                // PROCESS SNAPSHOTS
                'snapshot_loop: for (id, snapshot) in protocol_msg
                    .snapshots
                    .get_states()
                    .clone()
                {
                    // Skip any unsupported pools
                    if self
                        .inclusion_filters
                        .get(protocol.as_str())
                        .is_some_and(|predicate| !predicate(&snapshot))
                    {
                        continue;
                    }

                    // Construct component from snapshot
                    let mut component_tokens = Vec::new();
                    let mut new_tokens_accounts = HashMap::new();
                    for token in snapshot.component.tokens.clone() {
                        match state_guard.tokens.get(&token) {
                            Some(token) => {
                                component_tokens.push(token.clone());

                                // If the token is not an existing proxy token, we need to add it to
                                // the simulation engine
                                let token_address = match bytes_to_address(&token.address) {
                                    Ok(addr) => addr,
                                    Err(_) => {
                                        count_token_skips += 1;
                                        msg_failed_components.insert(id.clone());
                                        warn!(
                                            "Token address could not be decoded {}, ignoring pool {:x?}",
                                            token.address, id
                                        );
                                        continue 'snapshot_loop;
                                    }
                                };
                                // Deploy a proxy account without an implementation set
                                if !state_guard
                                    .proxy_token_addresses
                                    .contains_key(&token_address)
                                {
                                    new_tokens_accounts.insert(
                                        token_address,
                                        create_proxy_token_account(
                                            token_address,
                                            None,
                                            &HashMap::new(),
                                            snapshot.component.chain,
                                            None,
                                        ),
                                    );
                                }
                            }
                            None => {
                                count_token_skips += 1;
                                msg_failed_components.insert(id.clone());
                                debug!("Token not found {}, ignoring pool {:x?}", token, id);
                                continue 'snapshot_loop;
                            }
                        }
                    }
                    let component = ProtocolComponent::from_with_tokens(
                        snapshot.component.clone(),
                        component_tokens,
                    );

                    // Add new tokens to the simulation engine
                    if !new_tokens_accounts.is_empty() {
                        update_engine(
                            SHARED_TYCHO_DB.clone(),
                            header.clone().block(),
                            None,
                            new_tokens_accounts,
                        )
                        .map_err(|e| StreamDecodeError::Fatal(e.to_string()))?;
                    }

                    // collect contracts:ids mapping for states that should update on contract
                    // changes (non-manual updates)
                    if !component
                        .static_attributes
                        .contains_key("manual_updates")
                    {
                        for contract in &component.contract_ids {
                            contracts_map
                                .entry(contract.clone())
                                .or_insert_with(HashSet::new)
                                .insert(id.clone());
                        }
                        // Add DCI contracts so changes to these contracts trigger
                        // an update
                        for (_, tracing) in snapshot.entrypoints.iter() {
                            for contract in tracing.accessed_slots.keys().cloned() {
                                contracts_map
                                    .entry(contract)
                                    .or_insert_with(HashSet::new)
                                    .insert(id.clone());
                            }
                        }
                    }

                    // Collect new pairs (components)
                    new_pairs.insert(id.clone(), component.clone());

                    // Store component for later batch insertion
                    components_to_store.insert(id.clone(), component);

                    // Construct state from snapshot
                    if let Some(state_decode_f) = self.registry.get(protocol.as_str()) {
                        match state_decode_f(
                            snapshot,
                            header.clone(),
                            account_balances.clone(),
                            self.state.clone(),
                        )
                        .await
                        {
                            Ok(state) => {
                                new_components.insert(id.clone(), state);
                            }
                            Err(e) => {
                                if self.skip_state_decode_failures {
                                    warn!(pool = id, error = %e, "StateDecodingFailure");
                                    msg_failed_components.insert(id.clone());
                                    continue 'snapshot_loop;
                                } else {
                                    error!(pool = id, error = %e, "StateDecodingFailure");
                                    return Err(StreamDecodeError::Fatal(format!("{e}")));
                                }
                            }
                        }
                    } else if self.skip_state_decode_failures {
                        warn!(pool = id, "MissingDecoderRegistration");
                        msg_failed_components.insert(id.clone());
                        continue 'snapshot_loop;
                    } else {
                        error!(pool = id, "MissingDecoderRegistration");
                        return Err(StreamDecodeError::Fatal(format!(
                            "Missing decoder registration for: {id}"
                        )));
                    }
                }
            }

            // Batch insert components into state
            if !components_to_store.is_empty() {
                let mut state_guard = self.state.write().await;
                for (id, component) in components_to_store {
                    state_guard
                        .components
                        .insert(id, component);
                }
            }

            if !protocol_msg.snapshots.states.is_empty() {
                info!("Decoded {} snapshots for protocol {protocol}", new_components.len());
            }
            if count_token_skips > 0 {
                info!("Skipped {count_token_skips} pools due to missing tokens");
            }

            //TODO: should we remove failed components for new_components?
            updated_states.extend(new_components);

            // PROCESS DELTAS
            if let Some(deltas) = protocol_msg.deltas.clone() {
                // Update engine with account changes
                let mut state_guard = self.state.write().await;

                let mut account_update_by_address: HashMap<Address, AccountUpdate> = HashMap::new();
                // New proxy token accounts that must overwrite any existing placeholder.
                let mut new_proxy_accounts: Vec<AccountUpdate> = Vec::new();
                for (key, value) in deltas.account_deltas.iter() {
                    let mut update: AccountUpdate = value.clone().into();

                    // TEMP PATCH (ENG-4993)
                    //
                    // The indexer may emit Creation deltas with no code for EOA addresses.
                    // Treat them as EOAs (empty code) rather than downgrading to Update, which
                    // would skip init_account and cause "uninitialized account" warnings.
                    if update.code.is_none() && matches!(update.change, ChangeType::Creation) {
                        error!(
                            update = ?update,
                            "FaultyCreationDelta"
                        );
                        update.code = Some(vec![]);
                    }

                    if state_guard.tokens.contains_key(key) {
                        let original_address = update.address;
                        // If the account is a token, we need to handle it with a proxy contract.
                        // Storage updates apply to the proxy contract (at original address).
                        // Code updates (if any) apply to the token implementation contract (at
                        // impl_addr).

                        // Handle proxy contract updates
                        let impl_addr = match state_guard
                            .proxy_token_addresses
                            .get(&original_address)
                        {
                            Some(impl_addr) => {
                                // Token already has a proxy contract.

                                // Apply the storage update to proxy contract
                                let proxy_update = AccountUpdate { code: None, ..update.clone() };
                                account_update_by_address.insert(original_address, proxy_update);

                                *impl_addr
                            }
                            None => {
                                // Token does not have a proxy contract yet, create one

                                // Assign original token (implementation) contract to new proxy
                                // address
                                let impl_addr = generate_proxy_token_address(
                                    state_guard.proxy_token_addresses.len() as u32,
                                )?;
                                state_guard
                                    .proxy_token_addresses
                                    .insert(original_address, impl_addr);

                                // Create proxy token account with original account's storage (at
                                // original address). Track it separately so it can be
                                // force-overwritten and win over any placeholder that another
                                // decoder's snapshot loop may have written earlier.
                                let proxy_state = create_proxy_token_account(
                                    original_address,
                                    Some(impl_addr),
                                    &update.slots,
                                    update.chain,
                                    update.balance,
                                );
                                new_proxy_accounts.push(proxy_state);

                                impl_addr
                            }
                        };

                        // Apply code update to token implementation contract
                        if update.code.is_some() {
                            let impl_update = AccountUpdate {
                                address: impl_addr,
                                slots: HashMap::new(),
                                ..update.clone()
                            };
                            account_update_by_address.insert(impl_addr, impl_update);
                        }
                    } else {
                        // Not a token, apply update to the account at its original address
                        account_update_by_address.insert(update.address, update);
                    }
                }
                drop(state_guard);

                let state_guard = self.state.read().await;
                info!("Updating engine with {} contract deltas", deltas.account_deltas.len());
                update_engine(
                    SHARED_TYCHO_DB.clone(),
                    header.clone().block(),
                    None,
                    account_update_by_address,
                )
                .map_err(|e| StreamDecodeError::Fatal(e.to_string()))?;

                // Force-overwrite any newly-created proxy token accounts so they always win
                // over placeholder entries inserted by other decoders' snapshot loops.
                if !new_proxy_accounts.is_empty() {
                    SHARED_TYCHO_DB
                        .force_update_accounts(new_proxy_accounts)
                        .map_err(|e| StreamDecodeError::Fatal(e.to_string()))?;
                }
                info!("Engine updated");

                // Collect all pools related to the updated accounts
                let mut pools_to_update = HashSet::new();
                for (account, _update) in deltas.account_deltas {
                    // get new pools related to the account updated
                    pools_to_update.extend(
                        contracts_map
                            .get(&account)
                            .cloned()
                            .unwrap_or_default(),
                    );
                    // get existing pools related to the account updated
                    pools_to_update.extend(
                        state_guard
                            .contracts_map
                            .get(&account)
                            .cloned()
                            .unwrap_or_default(),
                    );
                }

                // Collect all balance changes this block
                let all_balances = Balances {
                    component_balances: deltas
                        .component_balances
                        .iter()
                        .map(|(pool_id, bals)| {
                            let mut balances = HashMap::new();
                            for (t, b) in bals {
                                balances.insert(t.clone(), b.balance.clone());
                            }
                            pools_to_update.insert(pool_id.clone());
                            (pool_id.clone(), balances)
                        })
                        .collect(),
                    account_balances: deltas
                        .account_balances
                        .iter()
                        .map(|(account, bals)| {
                            let mut balances = HashMap::new();
                            for (t, b) in bals {
                                balances.insert(t.clone(), b.balance.clone());
                            }
                            pools_to_update.extend(
                                contracts_map
                                    .get(account)
                                    .cloned()
                                    .unwrap_or_default(),
                            );
                            (account.clone(), balances)
                        })
                        .collect(),
                };

                // update states with protocol state deltas (attribute changes etc.)
                for (id, update) in deltas.state_deltas {
                    // TODO: is this needed?
                    let update_with_block = Self::add_block_info_to_delta(
                        ProtocolStateDelta::from(update),
                        current_block.clone(),
                    );
                    match Self::apply_update(
                        &id,
                        update_with_block,
                        &mut updated_states,
                        &state_guard,
                        &all_balances,
                    ) {
                        Ok(_) => {
                            pools_to_update.remove(&id);
                        }
                        Err(e) => {
                            if self.skip_state_decode_failures {
                                warn!(pool = id, error = %e, "Failed to apply state update, marking component as removed");
                                // Remove from updated_states if it was there
                                updated_states.remove(&id);
                                // Try to get component from new_pairs first, then from state
                                if let Some(component) = new_pairs.remove(&id) {
                                    removed_pairs.insert(id.clone(), component);
                                } else if let Some(component) = state_guard.components.get(&id) {
                                    removed_pairs.insert(id.clone(), component.clone());
                                } else {
                                    // Component not found in new_pairs or state, this shouldn't
                                    // happen
                                    warn!(pool = id, "Component not found in new_pairs or state, cannot add to removed_pairs");
                                }
                                pools_to_update.remove(&id);

                                // Add to failed components
                                msg_failed_components.insert(id.clone());
                            } else {
                                return Err(e);
                            }
                        }
                    }
                }

                // update remaining pools linked to updated contracts/updated balances
                for pool in pools_to_update {
                    // TODO: is this needed?
                    let default_delta_with_block = Self::add_block_info_to_delta(
                        ProtocolStateDelta::default(),
                        current_block.clone(),
                    );
                    match Self::apply_update(
                        &pool,
                        default_delta_with_block,
                        &mut updated_states,
                        &state_guard,
                        &all_balances,
                    ) {
                        Ok(_) => {}
                        Err(e) => {
                            if self.skip_state_decode_failures {
                                warn!(pool = pool, error = %e, "Failed to apply contract/balance update, marking component as removed");
                                // Remove from updated_states if it was there
                                updated_states.remove(&pool);
                                // Try to get component from new_pairs first, then from state
                                if let Some(component) = new_pairs.remove(&pool) {
                                    removed_pairs.insert(pool.clone(), component);
                                } else if let Some(component) = state_guard.components.get(&pool) {
                                    removed_pairs.insert(pool.clone(), component.clone());
                                } else {
                                    // Component not found in new_pairs or state, this shouldn't
                                    // happen
                                    warn!(pool = pool, "Component not found in new_pairs or state, cannot add to removed_pairs");
                                }

                                // Add to failed components
                                msg_failed_components.insert(pool.clone());
                            } else {
                                return Err(e);
                            }
                        }
                    }
                }
            };
        }

        // Persist the newly added/updated states
        let mut state_guard = self.state.write().await;

        // Update failed components with any new ones
        state_guard
            .failed_components
            .extend(msg_failed_components);

        // Remove any failed components from Updates
        // Perf: we could do it directly in the decoder logic to avoid some steps, but this logic is
        // complex and this is more robust.
        updated_states.retain(|id, _| {
            !state_guard
                .failed_components
                .contains(id)
        });
        new_pairs.retain(|id, _| {
            !state_guard
                .failed_components
                .contains(id)
        });

        state_guard
            .states
            .extend(updated_states.clone());

        state_guard.current_block_number = block_number_or_timestamp;

        // Add new components to persistent state
        for (id, component) in new_pairs.iter() {
            state_guard
                .components
                .insert(id.clone(), component.clone());
        }

        // Remove components from persistent state
        for id in removed_pairs.keys() {
            state_guard.components.remove(id);
        }

        for (key, values) in contracts_map {
            state_guard
                .contracts_map
                .entry(key)
                .or_insert_with(HashSet::new)
                .extend(values);
        }

        // Send the tick with all updated states
        Ok(Update::new(block_number_or_timestamp, updated_states, new_pairs)
            .set_is_partial(is_partial)
            .set_removed_pairs(removed_pairs)
            .set_sync_states(msg.sync_states.clone()))
    }

    /// Applies pending deltas from one or more `TxDeltaIndexer`s against the current confirmed
    /// state and returns an ephemeral `Update`.
    ///
    /// This is the read-only counterpart of `decode()`. It clones pool states, applies the
    /// supplied `pending_deltas`, and returns the result — **without writing back** to
    /// `DecoderState`. Calling this method twice with the same input produces identical results.
    ///
    /// Only native protocols are supported. VM protocols (extractor prefix `"vm:"`) are rejected
    /// at registration time in
    /// [`with_pending_indexer`](crate::evm::stream::ProtocolStreamBuilder::with_pending_indexer).
    ///
    /// # Parameters
    /// * `pending_deltas` — map from extractor name to the `BlockAggregatedChanges` produced by the
    ///   corresponding `TxDeltaIndexer::generate_deltas()` call.
    /// * `header` — the target block header. Its `block_number_or_timestamp()` is stamped on the
    ///   returned [`Update`]; its `block_number` and `block_timestamp` are injected into each state
    ///   delta so that protocols relying on block context (e.g. aerodrome slipstreams, etherfi)
    ///   receive correct values.
    pub async fn apply_deltas_ephemeral(
        &self,
        pending_deltas: &HashMap<String, BlockAggregatedChanges>,
        header: H,
    ) -> Result<Update, StreamDecodeError> {
        let block_number_or_timestamp = header
            .clone()
            .block_number_or_timestamp();
        let current_block = header.block();
        let state_guard = self.state.read().await;

        let mut updated_states: HashMap<String, Box<dyn ProtocolSim>> = HashMap::new();

        for deltas in pending_deltas.values() {
            let all_balances = Balances {
                component_balances: deltas
                    .component_balances
                    .iter()
                    .map(|(pool_id, bals)| {
                        let balances = bals
                            .iter()
                            .map(|(t, b)| (t.clone(), b.balance.clone()))
                            .collect();
                        (pool_id.clone(), balances)
                    })
                    .collect(),
                account_balances: HashMap::new(),
            };

            for (id, state_delta) in &deltas.state_deltas {
                let dto_delta = Self::add_block_info_to_delta(
                    ProtocolStateDelta::from(state_delta.clone()),
                    current_block.clone(),
                );
                if let Err(e) = Self::apply_update(
                    id,
                    dto_delta,
                    &mut updated_states,
                    &state_guard,
                    &all_balances,
                ) {
                    warn!(pool = id, error = %e, "EphemeralDeltaTransitionError");
                }
            }
        }

        Ok(Update::new(block_number_or_timestamp, updated_states, HashMap::new()))
    }

    /// Add current block information (number and timestamp) to a ProtocolStateDelta.
    fn add_block_info_to_delta(
        mut delta: ProtocolStateDelta,
        block_header_opt: Option<BlockHeader>,
    ) -> ProtocolStateDelta {
        if let Some(header) = block_header_opt {
            // Add block_number and block_timestamp attributes to ensure pool states
            // receive current block information during delta_transition
            delta.updated_attributes.insert(
                "block_number".to_string(),
                Bytes::from(header.number.to_be_bytes().to_vec()),
            );
            delta.updated_attributes.insert(
                "block_timestamp".to_string(),
                Bytes::from(header.timestamp.to_be_bytes().to_vec()),
            );
        }
        delta
    }

    fn apply_update(
        id: &String,
        update: ProtocolStateDelta,
        updated_states: &mut HashMap<String, Box<dyn ProtocolSim>>,
        state_guard: &RwLockReadGuard<'_, DecoderState>,
        all_balances: &Balances,
    ) -> Result<(), StreamDecodeError> {
        match updated_states.entry(id.clone()) {
            Entry::Occupied(mut entry) => {
                // If state exists in updated_states, apply the delta to it
                let state: &mut Box<dyn ProtocolSim> = entry.get_mut();
                state
                    .delta_transition(update, &state_guard.tokens, all_balances)
                    .map_err(|e| {
                        error!(pool = id, error = ?e, "DeltaTransitionError");
                        StreamDecodeError::Fatal(format!("TransitionFailure: {e:?}"))
                    })?;
            }
            Entry::Vacant(_) => {
                match state_guard.states.get(id) {
                    // If state does not exist in updated_states, apply the delta to the stored
                    // state
                    Some(stored_state) => {
                        let mut state = stored_state.clone();
                        state
                            .delta_transition(update, &state_guard.tokens, all_balances)
                            .map_err(|e| {
                                error!(pool = id, error = ?e, "DeltaTransitionError");
                                StreamDecodeError::Fatal(format!("TransitionFailure: {e:?}"))
                            })?;
                        updated_states.insert(id.clone(), state);
                    }
                    None => debug!(pool = id, reason = "MissingState", "DeltaTransitionError"),
                }
            }
        }
        Ok(())
    }
}

/// Generate a proxy token address for a given token index
fn generate_proxy_token_address(idx: u32) -> Result<Address, StreamDecodeError> {
    let padded_idx = format!("{idx:x}");
    let padded_zeroes = "0".repeat(33 - padded_idx.len());
    let proxy_token_address = format!("{padded_zeroes}{padded_idx}BAdbaBe");
    let decoded = hex::decode(proxy_token_address).map_err(|e| {
        StreamDecodeError::Fatal(format!("Invalid proxy token address encoding: {e}"))
    })?;

    const ADDRESS_LENGTH: usize = 20;
    if decoded.len() != ADDRESS_LENGTH {
        return Err(StreamDecodeError::Fatal(format!(
            "Invalid proxy token address length: expected {}, got {}",
            ADDRESS_LENGTH,
            decoded.len(),
        )));
    }

    Ok(Address::from_slice(&decoded))
}

/// Create a proxy token account for a token at a given address
///
/// The proxy token account is created at the original token address and points to the new token
/// address.
fn create_proxy_token_account(
    addr: Address,
    new_address: Option<Address>,
    storage: &HashMap<U256, U256>,
    chain: Chain,
    balance: Option<U256>,
) -> AccountUpdate {
    let mut slots = storage.clone();
    if let Some(new_address) = new_address {
        slots.insert(*IMPLEMENTATION_SLOT, U256::from_be_slice(new_address.as_slice()));
    }

    AccountUpdate {
        address: addr,
        chain,
        slots,
        balance,
        code: Some(ERC20_PROXY_BYTECODE.to_vec()),
        change: ChangeType::Creation,
    }
}

#[cfg(test)]
mock! {
    #[derive(Debug)]
    pub ProtocolSim {
        pub fn fee(&self) -> f64;
        pub fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError>;
        pub fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError>;
        pub fn get_limits(
            &self,
            sell_token: Bytes,
            buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError>;
        pub fn delta_transition(
            &mut self,
            delta: ProtocolStateDelta,
            tokens: &HashMap<Bytes, Token>,
            balances: &Balances,
        ) -> Result<(), TransitionError>;
        pub fn clone_box(&self) -> Box<dyn ProtocolSim>;
        pub fn eq(&self, other: &dyn ProtocolSim) -> bool;
    }
}

#[cfg(test)]
crate::impl_non_serializable_protocol!(MockProtocolSim, "test protocol");

#[cfg(test)]
impl ProtocolSim for MockProtocolSim {
    fn fee(&self) -> f64 {
        self.fee()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.spot_price(base, quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.get_amount_out(amount_in, token_in, token_out)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.get_limits(sell_token, buy_token)
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.delta_transition(delta, tokens, balances)
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        self.clone_box()
    }

    fn as_any(&self) -> &dyn Any {
        panic!("MockProtocolSim does not support as_any")
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        panic!("MockProtocolSim does not support as_any_mut")
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        self.eq(other)
    }

    fn typetag_name(&self) -> &'static str {
        unreachable!()
    }

    fn typetag_deserialize(&self) {
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::address;
    use mockall::predicate::*;
    use rstest::*;
    use tycho_client::feed::BlockHeader;
    use tycho_common::{models::Chain, Bytes};

    use super::*;
    use crate::evm::protocol::uniswap_v2::state::UniswapV2State;

    async fn setup_decoder(set_tokens: bool) -> TychoStreamDecoder<BlockHeader> {
        let mut decoder = TychoStreamDecoder::new();
        decoder.register_decoder::<UniswapV2State>("uniswap_v2");
        if set_tokens {
            let tokens = [
                Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").lpad(20, 0),
                Bytes::from("0xdac17f958d2ee523a2206206994597c13d831ec7").lpad(20, 0),
            ]
            .iter()
            .map(|addr| {
                let addr_str = format!("{addr:x}");
                (
                    addr.clone(),
                    Token::new(addr, &addr_str, 18, 100, &[Some(100_000)], Chain::Ethereum, 100),
                )
            })
            .collect();
            decoder.set_tokens(tokens).await;
        }
        decoder
    }

    fn load_test_msg(name: &str) -> FeedMessage<BlockHeader> {
        use std::{fs, path::Path};

        use tycho_client::feed::dto;
        let project_root = env!("CARGO_MANIFEST_DIR");
        let asset_path = Path::new(project_root).join(format!("tests/assets/decoder/{name}.json"));
        let json_data = fs::read_to_string(asset_path).expect("Failed to read test asset");
        let feed_msg: dto::FeedMessage<BlockHeader> =
            serde_json::from_str(&json_data).expect("Failed to deserialize FeedMsg json!");
        FeedMessage::from(feed_msg)
    }

    #[tokio::test]
    async fn test_decode() {
        let decoder = setup_decoder(true).await;

        let msg = load_test_msg("uniswap_v2_snapshot");
        let res1 = decoder
            .decode(&msg)
            .await
            .expect("decode failure");
        let msg = load_test_msg("uniswap_v2_delta");
        let res2 = decoder
            .decode(&msg)
            .await
            .expect("decode failure");

        assert_eq!(res1.states.len(), 1);
        assert_eq!(res2.states.len(), 1);
        assert_eq!(res1.sync_states.len(), 1);
        assert_eq!(res2.sync_states.len(), 1);
    }

    #[tokio::test]
    async fn test_decode_component_missing_token() {
        let decoder = setup_decoder(false).await;
        let tokens = [Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").lpad(20, 0)]
            .iter()
            .map(|addr| {
                let addr_str = format!("{addr:x}");
                (
                    addr.clone(),
                    Token::new(addr, &addr_str, 18, 100, &[Some(100_000)], Chain::Ethereum, 100),
                )
            })
            .collect();
        decoder.set_tokens(tokens).await;

        let msg = load_test_msg("uniswap_v2_snapshot");
        let res1 = decoder
            .decode(&msg)
            .await
            .expect("decode failure");

        assert_eq!(res1.states.len(), 0);
    }

    #[tokio::test]
    async fn test_decode_component_bad_id() {
        let decoder = setup_decoder(true).await;
        let msg = load_test_msg("uniswap_v2_snapshot_broken_id");

        match decoder.decode(&msg).await {
            Err(StreamDecodeError::Fatal(msg)) => {
                assert_eq!(msg, "Component id mismatch");
            }
            Ok(_) => {
                panic!("Expected failures to be raised")
            }
        }
    }

    #[rstest]
    #[case(true)]
    #[case(false)]
    #[tokio::test]
    async fn test_decode_component_bad_state(#[case] skip_failures: bool) {
        let mut decoder = setup_decoder(true).await;
        decoder.skip_state_decode_failures = skip_failures;

        let msg = load_test_msg("uniswap_v2_snapshot_broken_state");
        match decoder.decode(&msg).await {
            Err(StreamDecodeError::Fatal(msg)) => {
                if !skip_failures {
                    assert_eq!(msg, "Missing attributes reserve0");
                } else {
                    panic!("Expected failures to be ignored. Err: {msg}")
                }
            }
            Ok(res) => {
                if !skip_failures {
                    panic!("Expected failures to be raised")
                } else {
                    assert_eq!(res.states.len(), 0);
                }
            }
        }
    }

    #[tokio::test]
    async fn test_decode_updates_state_on_contract_change() {
        let decoder = setup_decoder(true).await;

        // Create the mock instances
        let mut mock_state = MockProtocolSim::new();

        mock_state
            .expect_clone_box()
            .times(1)
            .returning(|| {
                let mut cloned_mock_state = MockProtocolSim::new();
                // Expect `delta_transition` to be called once with any parameters
                cloned_mock_state
                    .expect_delta_transition()
                    .times(1)
                    .returning(|_, _, _| Ok(()));
                cloned_mock_state
                    .expect_clone_box()
                    .times(1)
                    .returning(|| Box::new(MockProtocolSim::new()));
                Box::new(cloned_mock_state)
            });

        // Insert mock state into `updated_states`
        let pool_id =
            "0x93d199263632a4ef4bb438f1feb99e57b4b5f0bd0000000000000000000005c2".to_string();
        decoder
            .state
            .write()
            .await
            .states
            .insert(pool_id.clone(), Box::new(mock_state) as Box<dyn ProtocolSim>);
        decoder
            .state
            .write()
            .await
            .contracts_map
            .insert(
                Bytes::from("0xba12222222228d8ba445958a75a0704d566bf2c8").lpad(20, 0),
                HashSet::from([pool_id.clone()]),
            );

        // Load a test message containing a contract update
        let msg = load_test_msg("balancer_v2_delta");

        // Decode the message
        let _ = decoder
            .decode(&msg)
            .await
            .expect("decode failure");

        // The mock framework will assert that `delta_transition` was called exactly once
    }

    #[test]
    fn test_generate_proxy_token_address() {
        let idx = 1;
        let generated_address =
            generate_proxy_token_address(idx).expect("proxy token address should be valid");
        assert_eq!(generated_address, address!("000000000000000000000000000000001badbabe"));

        let idx = 123456;
        let generated_address =
            generate_proxy_token_address(idx).expect("proxy token address should be valid");
        assert_eq!(generated_address, address!("00000000000000000000000000001e240badbabe"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_euler_hook_low_pool_manager_balance() {
        let mut decoder = TychoStreamDecoder::new();

        decoder.register_decoder_with_context::<crate::evm::protocol::uniswap_v4::state::UniswapV4State>(
            "uniswap_v4_hooks", DecoderContext::new().vm_traces(true)
        );

        let weth = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let teth = Bytes::from_str("0xd11c452fc99cf405034ee446803b6f6c1f6d5ed8").unwrap();
        let tokens = HashMap::from([
            (
                weth.clone(),
                Token::new(&weth, "WETH", 18, 100, &[Some(100_000)], Chain::Ethereum, 100),
            ),
            (
                teth.clone(),
                Token::new(&teth, "tETH", 18, 100, &[Some(100_000)], Chain::Ethereum, 100),
            ),
        ]);

        decoder.set_tokens(tokens.clone()).await;

        let msg = load_test_msg("euler_hook_snapshot");
        let res = decoder
            .decode(&msg)
            .await
            .expect("decode failure");

        let pool_state = res
            .states
            .get("0xc70d7fbd7fcccdf726e02fed78548b40dc52502b097c7a1ee7d995f4d4396134")
            .expect("Couldn't find target pool");
        let amount_out = pool_state
            .get_amount_out(
                BigUint::from_str("1000000000000000000").unwrap(),
                tokens.get(&teth).unwrap(),
                tokens.get(&weth).unwrap(),
            )
            .expect("Get amount out failed");

        assert_eq!(amount_out.amount, BigUint::from_str("1216190190361759119").unwrap());
    }
}
