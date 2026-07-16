#![allow(deprecated)]
use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt::{self, Debug},
    str::FromStr,
    sync::RwLock,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::primitives::{Address, U256};
use itertools::Itertools;
use num_bigint::BigUint;
use revm::DatabaseRef;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, warn};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use super::{
    constants::{EXTERNAL_ACCOUNT, MAX_BALANCE},
    erc20_token::{Overwrites, TokenProxyOverwriteFactory},
    models::Capability,
    tycho_simulation_contract::TychoSimulationContract,
};
use crate::evm::{
    engine_db::{engine_db_interface::EngineDatabaseInterface, tycho_db::PreCachedDB},
    override_stream::{FailurePolicy, OverrideSnapshot},
    protocol::{
        u256_num::{u256_to_biguint, u256_to_f64},
        utils::bytes_to_address,
    },
    simulation::BlockEnvOverrides,
};

pub struct EVMPoolState<D: EngineDatabaseInterface + Clone + Debug>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    /// The pool's identifier
    id: String,
    /// The pool's token's addresses
    pub tokens: Vec<Bytes>,
    /// The pool's component balances.
    balances: HashMap<Address, U256>,
    /// The contract address for where protocol balances are stored (i.e. a vault contract).
    /// If given, balances will be overwritten here instead of on the pool contract during
    /// simulations. This has been deprecated in favor of `contract_balances`.
    #[deprecated(note = "Use contract_balances instead")]
    balance_owner: Option<Address>,
    /// Read-through cache of spot prices by `(sell, buy)`. Lazily populated by `spot_price`,
    /// eagerly warmed by `update_pool_state`, and cleared whenever pool state changes. Bypassed
    /// for pools with live overrides (see later tasks).
    spot_price_cache: RwLock<HashMap<(Address, Address), f64>>,
    /// Read-through cache of `(sell_limit, buy_limit)` by `(sell, buy)`. Stable per
    /// pool-state-version; cleared whenever pool state changes. Bypassed under live overrides.
    limit_cache: RwLock<HashMap<(Address, Address), (U256, U256)>>,
    /// The supported capabilities of this pool
    capabilities: HashSet<Capability>,
    /// Storage overwrites that will be applied to all simulations. They will be cleared
    /// when ``update_pool_state`` is called, i.e. usually at each block. Hence, the name.
    block_lasting_overwrites: HashMap<Address, Overwrites>,
    /// A set of all contract addresses involved in the simulation of this pool.
    involved_contracts: HashSet<Address>,
    /// A map of contracts to their token balances.
    contract_balances: HashMap<Address, HashMap<Address, U256>>,
    /// Indicates if the protocol uses custom update rules and requires update
    /// triggers to recalculate spot prices ect. Default is to update on all changes on
    /// the pool.
    manual_updates: bool,
    /// Caller (`tx.origin`) for the adapter's `price()` query; `None` defaults to
    /// `EXTERNAL_ACCOUNT`. Set per protocol in the decoder (see `spot_price_caller`).
    spot_price_caller: Option<Address>,
    /// The adapter contract. This is used to interact with the protocol when running simulations
    adapter_contract: TychoSimulationContract<D>,
    /// Tokens for which balance overwrites should be disabled.
    disable_overwrite_tokens: HashSet<Address>,
    /// Tokens whose protocol does not emit token contract storage (e.g. FermiSwap), so they are
    /// bare `TokenProxy` accounts with no implementation in the shared DB. For these, the
    /// overwrites keep transfers in the proxy's local bookkeeping — holders get a custom approval
    /// and the swap recipient a custom balance — so a `transferFrom` never delegates to a real
    /// implementation another VM protocol (curve, balancer) mounted on the same shared token,
    /// which would revert with `SafeERC20FailedOperation` (ENG-6161). Rebase/fee tokens in
    /// `disable_overwrite_tokens` are excluded.
    self_contained_tokens: HashSet<Address>,
    /// Block context overrides applied to this pool's adapter simulations.
    block_overrides: Option<BlockEnvOverrides>,
    /// Live per-block VM overrides (e.g. Titan pAMM oracle prices) read at simulation time.
    ///
    /// When set, the latest [`OverrideSnapshot`] is merged into the pool's storage overwrites and
    /// block environment on every simulation, so sub-block updates are reflected without a Tycho
    /// block update. Takes precedence over [`Self::block_lasting_overwrites`] and
    /// [`Self::block_overrides`] on conflict.
    live_overrides: Option<watch::Receiver<OverrideSnapshot>>,
}

impl<D> Clone for EVMPoolState<D>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            tokens: self.tokens.clone(),
            balances: self.balances.clone(),
            balance_owner: self.balance_owner,
            spot_price_cache: RwLock::new(
                self.spot_price_cache
                    .read()
                    .expect("spot_price_cache poisoned")
                    .clone(),
            ),
            limit_cache: RwLock::new(
                self.limit_cache
                    .read()
                    .expect("limit_cache poisoned")
                    .clone(),
            ),
            capabilities: self.capabilities.clone(),
            block_lasting_overwrites: self.block_lasting_overwrites.clone(),
            involved_contracts: self.involved_contracts.clone(),
            contract_balances: self.contract_balances.clone(),
            manual_updates: self.manual_updates,
            adapter_contract: self.adapter_contract.clone(),
            disable_overwrite_tokens: self.disable_overwrite_tokens.clone(),
            self_contained_tokens: self.self_contained_tokens.clone(),
            block_overrides: self.block_overrides.clone(),
            live_overrides: self.live_overrides.clone(),
        }
    }
}

impl<D> Debug for EVMPoolState<D>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EVMPoolState")
            .field("id", &self.id)
            .field("tokens", &self.tokens)
            .field("balances", &self.balances)
            .field("involved_contracts", &self.involved_contracts)
            .field("contract_balances", &self.contract_balances)
            .finish_non_exhaustive()
    }
}

impl<D> EVMPoolState<D>
where
    D: EngineDatabaseInterface + Clone + Debug + 'static,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    /// Creates a new instance of `EVMPoolState` with the given attributes, with the ability to
    /// simulate a protocol-agnostic transaction.
    ///
    /// See struct definition of `EVMPoolState` for attribute explanations.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        tokens: Vec<Bytes>,
        component_balances: HashMap<Address, U256>,
        balance_owner: Option<Address>,
        contract_balances: HashMap<Address, HashMap<Address, U256>>,
        spot_prices: HashMap<(Address, Address), f64>,
        capabilities: HashSet<Capability>,
        block_lasting_overwrites: HashMap<Address, Overwrites>,
        involved_contracts: HashSet<Address>,
        manual_updates: bool,
        adapter_contract: TychoSimulationContract<D>,
        disable_overwrite_tokens: HashSet<Address>,
        self_contained_tokens: HashSet<Address>,
        block_overrides: Option<BlockEnvOverrides>,
        spot_price_caller: Option<Address>,
    ) -> Self {
        Self {
            id,
            tokens,
            balances: component_balances,
            balance_owner,
            spot_price_cache: RwLock::new(spot_prices),
            limit_cache: RwLock::new(HashMap::new()),
            capabilities,
            block_lasting_overwrites,
            involved_contracts,
            contract_balances,
            manual_updates,
            adapter_contract,
            disable_overwrite_tokens,
            self_contained_tokens,
            block_overrides,
            spot_price_caller,
            live_overrides: None,
        }
    }

    /// Attaches a live override channel (e.g. from a Titan pAMM provider).
    ///
    /// Once set, the latest snapshot is read on every simulation; see [`Self::live_overrides`].
    pub fn set_live_overrides(&mut self, receiver: watch::Receiver<OverrideSnapshot>) {
        self.live_overrides = Some(receiver);
    }

    /// Reads the latest live override snapshot once, if a channel is attached and still fresh.
    ///
    /// The `watch::Ref` guard is released immediately; the returned value is a clone. A single
    /// simulation resolves both its storage overwrites and its block environment from this one
    /// snapshot, so it can never mix storage from one snapshot with a block environment from
    /// another, and never holds the channel's read lock across EVM calls.
    ///
    /// Returns `None` once the snapshot has passed its provider-set expiry, so an expired override
    /// is dropped and the pool transparently reverts to Tycho's indexed state.
    fn get_live_snapshot(&self) -> Option<OverrideSnapshot> {
        let snapshot = self
            .live_overrides
            .as_ref()
            .map(|receiver| receiver.borrow().clone())?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        if snapshot.is_expired(now) {
            return None;
        }
        Some(snapshot)
    }

    /// Runs `simulate` against `live_snapshot` and, when the snapshot's provider opted into
    /// [`FailurePolicy::FallbackToIndexedState`], retries a failure once on the plain indexed
    /// state.
    ///
    /// `InvalidInput` failures are never retried: the simulation itself succeeded and the input
    /// was merely clamped to the pool's limit. Note that under overrides the limit is itself
    /// override-derived (it reflects the size the live maker quote covers), and that clamp is
    /// treated as authoritative rather than retried against the indexed pool's larger limit.
    fn run_with_indexed_fallback<T>(
        pool_id: &str,
        operation: &str,
        live_snapshot: Option<&OverrideSnapshot>,
        mut simulate: impl FnMut(Option<&OverrideSnapshot>) -> Result<T, SimulationError>,
    ) -> Result<T, SimulationError> {
        let attempt = simulate(live_snapshot);
        let Err(error) = &attempt else { return attempt };
        if matches!(error, SimulationError::InvalidInput(..)) {
            return attempt;
        }
        let Some(snapshot) = live_snapshot
            .filter(|snapshot| snapshot.failure_policy == FailurePolicy::FallbackToIndexedState)
        else {
            return attempt;
        };
        debug!(
            pool = %pool_id,
            %error,
            snapshot_block = ?snapshot.block_number,
            snapshot_ts = ?snapshot.block_timestamp,
            expires_at = ?snapshot.expires_at,
            override_accounts = snapshot.storage.len(),
            "{operation} failed with live overrides; retrying on indexed state"
        );
        let retry = simulate(None);
        if let Err(retry_error) = &retry {
            warn!(pool = %pool_id, %retry_error, "{operation} retry on indexed state also failed");
        }
        retry
    }

    /// The block environment to apply to adapter simulations, resolved from a single pre-read
    /// `live` snapshot: its block number/timestamp take precedence over the statically configured
    /// [`Self::block_overrides`].
    fn block_env(&self, live: Option<&OverrideSnapshot>) -> Option<BlockEnvOverrides> {
        let base = self.block_overrides.to_owned();
        let Some(snapshot) = live else {
            return base;
        };
        if snapshot.block_number.is_none() && snapshot.block_timestamp.is_none() {
            return base;
        }
        let mut overrides = base.unwrap_or_default();
        if snapshot.block_number.is_some() {
            overrides.number = snapshot.block_number;
        }
        if snapshot.block_timestamp.is_some() {
            overrides.timestamp = snapshot.block_timestamp;
        }
        Some(overrides)
    }

    /// Ensures the pool supports the given capability
    ///
    /// # Arguments
    ///
    /// * `capability` - The capability that we would like to check for.
    ///
    /// # Returns
    ///
    /// * `Result<(), SimulationError>` - Returns `Ok(())` if the capability is supported, or a
    ///   `SimulationError` otherwise.
    ///
    /// Only used by tests now that `set_spot_prices_with` inlines the capability check via
    /// `compute_spot_price`.
    #[cfg(test)]
    fn ensure_capability(&self, capability: Capability) -> Result<(), SimulationError> {
        if !self.capabilities.contains(&capability) {
            return Err(SimulationError::FatalError(format!(
                "capability {:?} not supported",
                capability.to_string()
            )));
        }
        Ok(())
    }
    /// Sets the spot prices for a pool for all possible pairs of the given tokens.
    ///
    /// # Arguments
    ///
    /// * `tokens` - A hashmap of `Token` instances representing the tokens to calculate spot prices
    ///   for.
    ///
    /// # Returns
    ///
    /// * `Result<(), SimulationError>` - Returns `Ok(())` if the spot prices are successfully set,
    ///   or a `SimulationError` if an error occurs during the calculation or processing.
    ///
    /// # Behavior
    ///
    /// This function performs the following steps:
    /// 1. Ensures the pool has the required capability to perform price calculations.
    /// 2. Iterates over all permutations of token pairs (sell token and buy token). For each pair:
    ///    - Retrieves all possible overwrites, considering the maximum balance limit.
    ///    - Calculates the sell amount limit, considering the overwrites.
    ///    - Invokes the adapter contract's `price` function to retrieve the calculated price for
    ///      the token pair, considering the sell amount limit.
    ///    - Processes the price based on whether the `ScaledPrice` capability is present:
    ///       - If `ScaledPrice` is present, uses the price directly from the adapter contract.
    ///       - If `ScaledPrice` is absent, scales the price by adjusting for token decimals.
    ///    - Stores the calculated price in the `spot_prices` map with the token addresses as the
    ///      key.
    /// 3. Returns `Ok(())` upon successful completion or a `SimulationError` upon failure.
    ///
    /// # Usage
    ///
    /// Spot prices need to be set before attempting to retrieve prices using `spot_price`.
    ///
    /// Tip: Setting spot prices on the pool every time the pool actually changes will result in
    /// faster price fetching than if prices are only set immediately before attempting to retrieve
    /// prices.
    pub fn set_spot_prices(
        &mut self,
        tokens: &HashMap<Bytes, Token>,
    ) -> Result<(), SimulationError> {
        // Read the live snapshot once, so every pair (and both sub-swaps in the no-capability
        // branch) simulates against one consistent snapshot.
        let live_snapshot = self.get_live_snapshot();
        let pool_id = self.id.clone();
        Self::run_with_indexed_fallback(
            &pool_id,
            "Spot prices",
            live_snapshot.as_ref(),
            |snapshot| self.set_spot_prices_with(tokens, snapshot),
        )
    }

    /// Computes the spot price for a single `(sell, buy)` pair against `live_snapshot`'s overrides
    /// (or the plain indexed state when `None`), using the same logic as `set_spot_prices_with`:
    /// the adapter `price` function when `PriceFunction` is supported, otherwise a two-swap
    /// finite-difference. Does not touch the cache.
    fn compute_spot_price(
        &self,
        tokens: &HashMap<Bytes, Token>,
        sell_token_address: Address,
        buy_token_address: Address,
        live_snapshot: Option<&OverrideSnapshot>,
        block_overrides: Option<BlockEnvOverrides>,
    ) -> Result<f64, SimulationError> {
        if self
            .capabilities
            .contains(&Capability::PriceFunction)
        {
            let overwrites = Some(self.get_overwrites(
                vec![sell_token_address, buy_token_address],
                *MAX_BALANCE / U256::from(100),
                live_snapshot,
            )?);

            let (sell_amount_limit, _) = self.get_amount_limits(
                vec![sell_token_address, buy_token_address],
                overwrites.clone(),
                block_overrides.clone(),
            )?;
            let price_result = self.adapter_contract.price(
                &self.id,
                sell_token_address,
                buy_token_address,
                vec![sell_amount_limit / U256::from(100)],
                overwrites,
                self.spot_price_caller,
                block_overrides.clone(),
            )?;

            if self
                .capabilities
                .contains(&Capability::ScaledPrice)
            {
                Ok(*price_result.first().ok_or_else(|| {
                    SimulationError::FatalError("Calculated price array is empty".to_string())
                })?)
            } else {
                let unscaled_price = price_result.first().ok_or_else(|| {
                    SimulationError::FatalError("Calculated price array is empty".to_string())
                })?;
                let sell_token_decimals = self.get_decimals(tokens, &sell_token_address)?;
                let buy_token_decimals = self.get_decimals(tokens, &buy_token_address)?;
                Ok(*unscaled_price * 10f64.powi(sell_token_decimals as i32) /
                    10f64.powi(buy_token_decimals as i32))
            }
        } else {
            // If the pool does not support price function, we need to calculate spot prices by
            // swapping two amounts and use the approximation to get the derivative.
            let overwrites = Some(self.get_overwrites(
                vec![sell_token_address, buy_token_address],
                *MAX_BALANCE / U256::from(100),
                live_snapshot,
            )?);

            // Calculate the first sell amount (x1) as 1% of the maximum limit.
            let x1 =
                self.get_amount_limits(
                    vec![sell_token_address, buy_token_address],
                    overwrites.clone(),
                    block_overrides.clone(),
                )?
                .0 / U256::from(100);

            // Calculate the second sell amount (x2) as x1 + 1% of x1. 1.01% of the max limit
            let x2 = x1 + (x1 / U256::from(100));

            // Perform a swap for the first sell amount (x1) and retrieve the received amount
            // (y1).
            let y1 = self
                .adapter_contract
                .swap(
                    &self.id,
                    sell_token_address,
                    buy_token_address,
                    false,
                    x1,
                    overwrites.clone(),
                    block_overrides.clone(),
                )?
                .0
                .received_amount;

            // Perform a swap for the second sell amount (x2) and retrieve the received amount
            // (y2).
            let y2 = self
                .adapter_contract
                .swap(
                    &self.id,
                    sell_token_address,
                    buy_token_address,
                    false,
                    x2,
                    overwrites,
                    block_overrides.clone(),
                )?
                .0
                .received_amount;

            let sell_token_decimals = self.get_decimals(tokens, &sell_token_address)?;
            let buy_token_decimals = self.get_decimals(tokens, &buy_token_address)?;

            let num = y2 - y1;
            let den = x2 - x1;

            // Calculate the marginal price, adjusting for token decimals.
            let token_correction =
                10f64.powi(sell_token_decimals as i32 - buy_token_decimals as i32);
            let num_f64 = u256_to_f64(num)?;
            let den_f64 = u256_to_f64(den)?;
            if den_f64 == 0.0 {
                return Err(SimulationError::FatalError(
                    "Failed to compute marginal price: denominator converted to 0".into(),
                ));
            }
            Ok(num_f64 / den_f64 * token_correction)
        }
    }

    /// Computes and stores spot prices against `live_snapshot`'s overrides (or the plain indexed
    /// state when `None`).
    fn set_spot_prices_with(
        &mut self,
        tokens: &HashMap<Bytes, Token>,
        live_snapshot: Option<&OverrideSnapshot>,
    ) -> Result<(), SimulationError> {
        let block_overrides = self.block_env(live_snapshot);
        for [sell_token_address, buy_token_address] in self
            .tokens
            .iter()
            .permutations(2)
            .map(|p| [p[0], p[1]])
        {
            let sell_token_address = bytes_to_address(sell_token_address)?;
            let buy_token_address = bytes_to_address(buy_token_address)?;

            let price = self.compute_spot_price(
                tokens,
                sell_token_address,
                buy_token_address,
                live_snapshot,
                block_overrides.clone(),
            )?;

            self.spot_price_cache
                .write()
                .expect("spot_price_cache poisoned")
                .insert((sell_token_address, buy_token_address), price);
        }

        Ok(())
    }

    fn get_decimals(
        &self,
        tokens: &HashMap<Bytes, Token>,
        sell_token_address: &Address,
    ) -> Result<usize, SimulationError> {
        tokens
            .get(&Bytes::from(sell_token_address.as_slice()))
            .map(|t| t.decimals as usize)
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "Failed to scale spot prices! Pool: {} Token 0x{:x} is not available!",
                    self.id, sell_token_address
                ))
            })
    }

    /// Retrieves the sell and buy amount limit for a given pair of tokens and the given overwrites.
    ///
    /// Attempting to swap an amount of the sell token that exceeds the sell amount limit is not
    /// advised and in most cases will result in a revert.
    ///
    /// # Arguments
    ///
    /// * `tokens` - A vec of tokens, where the first token is the sell token and the second is the
    ///   buy token. The order of tokens in the input vector is significant and determines the
    ///   direction of the price query.
    /// * `overwrites` - A hashmap of overwrites to apply to the simulation.
    ///
    /// # Returns
    ///
    /// * `Result<(U256,U256), SimulationError>` - Returns the sell and buy amount limit as a `U256`
    ///   if successful, or a `SimulationError` on failure.
    fn get_amount_limits(
        &self,
        tokens: Vec<Address>,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        block_overrides: Option<BlockEnvOverrides>,
    ) -> Result<(U256, U256), SimulationError> {
        let limits = self.adapter_contract.get_limits(
            &self.id,
            tokens[0],
            tokens[1],
            overwrites,
            block_overrides,
        )?;

        Ok(limits)
    }

    /// Updates the pool state.
    ///
    /// It is assumed this is called on a new block. Therefore, first the pool's overwrites cache is
    /// cleared, then the balances are updated and the spot prices are recalculated.
    ///
    /// # Arguments
    ///
    /// * `tokens` - A hashmap of token addresses to `Token` instances. This is necessary for
    ///   calculating new spot prices.
    /// * `balances` - A `Balances` instance containing all balance updates on the current block.
    fn update_pool_state(
        &mut self,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), SimulationError> {
        // clear cache
        self.adapter_contract
            .engine
            .clear_temp_storage()
            .map_err(|err| {
                SimulationError::FatalError(format!("Failed to clear temporary storage: {err:?}",))
            })?;
        self.block_lasting_overwrites.clear();

        // Set balances. Component balances and contract balances are refreshed independently:
        // hybrid pools (e.g. Balancer V3) carry both, and `get_balance_overwrites` layers
        // contract balances over component balances. Skipping the contract-balance refresh
        // whenever component balances exist would freeze contract balances at their snapshot
        // values while the contract's indexed storage keeps advancing, which breaks any
        // simulation that compares `balanceOf` against stored reserves (e.g. Balancer V3
        // `settle` reverting with `BalanceNotSettled`).
        if let Some(bals) = balances
            .component_balances
            .get(&self.id)
        {
            // Merge delta balances with existing balances instead of replacing them
            // Prevents errors when delta balance changes do not affect all the pool tokens.
            for (token, bal) in bals {
                let addr = bytes_to_address(token).map_err(|_| {
                    SimulationError::FatalError(format!(
                        "Invalid token address in balance update: {token:?}"
                    ))
                })?;
                self.balances
                    .insert(addr, U256::from_be_slice(bal));
            }
        }
        for contract in &self.involved_contracts {
            if let Some(bals) = balances
                .account_balances
                .get(&Bytes::from(contract.as_slice()))
            {
                let contract_entry = self
                    .contract_balances
                    .entry(*contract)
                    .or_default();
                for (token, bal) in bals {
                    let addr = bytes_to_address(token).map_err(|_| {
                        SimulationError::FatalError(format!(
                            "Invalid token address in balance update: {token:?}"
                        ))
                    })?;
                    contract_entry.insert(addr, U256::from_be_slice(bal));
                }
            }
        }

        // reset spot prices
        self.set_spot_prices(tokens)?;
        Ok(())
    }

    fn get_overwrites(
        &self,
        tokens: Vec<Address>,
        max_amount: U256,
        live: Option<&OverrideSnapshot>,
    ) -> Result<HashMap<Address, Overwrites>, SimulationError> {
        let token_overwrites = self.get_token_overwrites(tokens, max_amount)?;

        // Merge `block_lasting_overwrites` with `token_overwrites`
        let mut merged_overwrites =
            self.merge(self.block_lasting_overwrites.clone(), token_overwrites);

        // Live overrides (e.g. Titan pAMM oracle state) take precedence on conflict.
        if let Some(live) = live {
            if !live.storage.is_empty() {
                merged_overwrites = self.merge(merged_overwrites, live.storage.as_ref().clone());
            }
        }

        Ok(merged_overwrites)
    }

    fn get_token_overwrites(
        &self,
        tokens: Vec<Address>,
        max_amount: U256,
    ) -> Result<HashMap<Address, Overwrites>, SimulationError> {
        let sell_token = &tokens[0].clone(); //TODO: need to make it clearer from the interface
        let mut res: Vec<HashMap<Address, Overwrites>> = Vec::new();
        if !self
            .capabilities
            .contains(&Capability::TokenBalanceIndependent)
        {
            res.push(self.get_balance_overwrites()?);
        }

        let mut overwrites = TokenProxyOverwriteFactory::new(*sell_token, None);

        overwrites.set_balance(max_amount, Address::from_slice(&*EXTERNAL_ACCOUNT.0));

        // Set allowance for adapter_address to max_amount
        overwrites.set_allowance(max_amount, self.adapter_contract.address, *EXTERNAL_ACCOUNT);

        res.push(overwrites.get_overwrites());

        // Self-contained tokens (see `self_contained_tokens`): pre-track EXTERNAL_ACCOUNT (the
        // recipient) for each output token, so it's credited locally instead of bootstrapping its
        // balance via the implementation.
        for token in tokens.iter().skip(1) {
            if self
                .self_contained_tokens
                .contains(token) &&
                !self
                    .disable_overwrite_tokens
                    .contains(token)
            {
                let mut recipient = TokenProxyOverwriteFactory::new(*token, None);
                recipient.set_balance(U256::ZERO, *EXTERNAL_ACCOUNT);
                res.push(recipient.get_overwrites());
            }
        }

        // Merge all overwrites into a single HashMap
        Ok(res
            .into_iter()
            .fold(HashMap::new(), |acc, overwrite| self.merge(acc, overwrite)))
    }

    /// Gets all balance overwrites for the pool's tokens.
    ///
    /// If the pool uses component balances, the balances are set for the balance owner (if exists)
    /// or for the pool itself. If the pool uses contract balances, the balances are set for the
    /// contracts involved in the pool.
    ///
    /// # Returns
    ///
    /// * `Result<HashMap<Address, Overwrites>, SimulationError>` - Returns a hashmap of address to
    ///   `Overwrites` if successful, or a `SimulationError` on failure.
    fn get_balance_overwrites(&self) -> Result<HashMap<Address, Overwrites>, SimulationError> {
        let mut balance_overwrites: HashMap<Address, Overwrites> = HashMap::new();

        // Use component balances for overrides
        let address = match self.balance_owner {
            Some(owner) => Some(owner),
            None if !self.contract_balances.is_empty() => None,
            None => Some(self.id.parse().map_err(|_| {
                SimulationError::FatalError(
                    "Failed to get balance overwrites: Pool ID is not an address".into(),
                )
            })?),
        };

        if let Some(address) = address {
            // Only override balances that are explicitly provided in self.balances
            // This preserves existing balances for tokens not updated in delta transitions
            for (token, bal) in &self.balances {
                let mut overwrites = TokenProxyOverwriteFactory::new(*token, None);
                overwrites.set_balance(*bal, address);
                // Self-contained tokens (see `self_contained_tokens`): also grant a custom approval
                // so `transferFrom` from the holder stays local instead of delegating to the impl.
                if self
                    .self_contained_tokens
                    .contains(token)
                {
                    overwrites.set_has_custom_approval(address);
                }
                balance_overwrites.extend(overwrites.get_overwrites());
            }
        }

        // Use contract balances for overrides (will overwrite component balances if they were set
        // for a contract we explicitly track balances for)
        for (contract, balances) in &self.contract_balances {
            for (token, balance) in balances {
                let mut overwrites = TokenProxyOverwriteFactory::new(*token, None);
                overwrites.set_balance(*balance, *contract);
                // Same as above: keep `transferFrom` from this contract local for self-contained
                // tokens (see `self_contained_tokens`).
                if self
                    .self_contained_tokens
                    .contains(token)
                {
                    overwrites.set_has_custom_approval(*contract);
                }
                balance_overwrites.extend(overwrites.get_overwrites());
            }
        }

        // Apply disables for tokens that should not have any balance overrides
        for token in &self.disable_overwrite_tokens {
            balance_overwrites.remove(token);
        }

        Ok(balance_overwrites)
    }

    /// Merges `source` into `target` and returns the result. On a per-slot conflict, `source` wins.
    fn merge(
        &self,
        mut target: HashMap<Address, Overwrites>,
        source: HashMap<Address, Overwrites>,
    ) -> HashMap<Address, Overwrites> {
        for (key, source_inner) in source {
            target
                .entry(key)
                .or_default()
                .extend(source_inner);
        }

        target
    }

    #[cfg(test)]
    pub fn get_involved_contracts(&self) -> HashSet<Address> {
        self.involved_contracts.clone()
    }

    #[cfg(test)]
    pub fn get_manual_updates(&self) -> bool {
        self.manual_updates
    }

    #[cfg(test)]
    pub fn get_spot_price_caller(&self) -> Option<Address> {
        self.spot_price_caller
    }

    /// Simulates a sell of `amount_in` against `live_snapshot`'s overrides (or the plain indexed
    /// state when `None`); see [`ProtocolSim::get_amount_out`] for the caller-facing contract.
    fn get_amount_out_with(
        &self,
        amount_in: &BigUint,
        token_in: &Token,
        token_out: &Token,
        live_snapshot: Option<&OverrideSnapshot>,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let sell_token_address = bytes_to_address(&token_in.address)?;
        let buy_token_address = bytes_to_address(&token_out.address)?;
        let sell_amount = U256::from_be_slice(&amount_in.to_bytes_be());
        let block_overrides = self.block_env(live_snapshot);
        let overwrites = self.get_overwrites(
            vec![sell_token_address, buy_token_address],
            *MAX_BALANCE / U256::from(100),
            live_snapshot,
        )?;
        let (sell_amount_limit, _) = self.get_amount_limits(
            vec![sell_token_address, buy_token_address],
            Some(overwrites.clone()),
            block_overrides.clone(),
        )?;
        let (sell_amount_respecting_limit, sell_amount_exceeds_limit) = if self
            .capabilities
            .contains(&Capability::HardLimits) &&
            sell_amount_limit < sell_amount
        {
            (sell_amount_limit, true)
        } else {
            (sell_amount, false)
        };

        let overwrites_with_sell_limit = self.get_overwrites(
            vec![sell_token_address, buy_token_address],
            sell_amount_limit,
            live_snapshot,
        )?;
        let complete_overwrites = self.merge(overwrites, overwrites_with_sell_limit);

        let (trade, state_changes) = self.adapter_contract.swap(
            &self.id,
            sell_token_address,
            buy_token_address,
            false,
            sell_amount_respecting_limit,
            Some(complete_overwrites),
            block_overrides,
        )?;

        let mut new_state = self.clone();

        // Apply state changes to the new state
        for (address, state_update) in state_changes {
            if let Some(storage) = state_update.storage {
                let block_overwrites = new_state
                    .block_lasting_overwrites
                    .entry(address)
                    .or_default();
                for (slot, value) in storage {
                    let slot = U256::from_str(&slot.to_string()).map_err(|_| {
                        SimulationError::FatalError("Failed to decode slot index".to_string())
                    })?;
                    let value = U256::from_str(&value.to_string()).map_err(|_| {
                        SimulationError::FatalError("Failed to decode slot overwrite".to_string())
                    })?;
                    block_overwrites.insert(slot, value);
                }
            }
        }

        // Update spot prices
        let tokens = HashMap::from([
            (token_in.address.clone(), token_in.clone()),
            (token_out.address.clone(), token_out.clone()),
        ]);
        let _ = new_state.set_spot_prices(&tokens);

        let buy_amount = trade.received_amount;

        if sell_amount_exceeds_limit {
            return Err(SimulationError::InvalidInput(
                format!("Sell amount exceeds limit {sell_amount_limit}"),
                Some(GetAmountOutResult::new(
                    u256_to_biguint(buy_amount),
                    u256_to_biguint(trade.gas_used),
                    Box::new(new_state.clone()),
                )),
            ));
        }
        Ok(GetAmountOutResult::new(
            u256_to_biguint(buy_amount),
            u256_to_biguint(trade.gas_used),
            Box::new(new_state.clone()),
        ))
    }

    /// Computes trade limits against `live_snapshot`'s overrides (or the plain indexed state when
    /// `None`); see [`ProtocolSim::get_limits`] for the caller-facing contract.
    fn get_limits_with(
        &self,
        sell_token: &Bytes,
        buy_token: &Bytes,
        live_snapshot: Option<&OverrideSnapshot>,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let sell_token = bytes_to_address(sell_token)?;
        let buy_token = bytes_to_address(buy_token)?;
        let overwrites = self.get_overwrites(
            vec![sell_token, buy_token],
            *MAX_BALANCE / U256::from(100),
            live_snapshot,
        )?;
        let limits = self.get_amount_limits(
            vec![sell_token, buy_token],
            Some(overwrites),
            self.block_env(live_snapshot),
        )?;
        Ok((u256_to_biguint(limits.0), u256_to_biguint(limits.1)))
    }

    #[cfg(test)]
    pub fn get_balance_owner(&self) -> Option<Address> {
        self.balance_owner
    }

    /// Get the component balances for validation purposes
    pub fn get_balances(&self) -> &HashMap<Address, U256> {
        &self.balances
    }

    #[cfg(test)]
    pub fn get_block_overrides(&self) -> Option<BlockEnvOverrides> {
        let live_snapshot = self.get_live_snapshot();
        self.block_env(live_snapshot.as_ref())
    }
}

impl<D> Serialize for EVMPoolState<D>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("not supported due vm state deps"))
    }
}

impl<'de, D> Deserialize<'de> for EVMPoolState<D>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    fn deserialize<De>(_deserializer: De) -> Result<Self, De::Error>
    where
        De: serde::Deserializer<'de>,
    {
        Err(serde::de::Error::custom("not supported due vm state deps"))
    }
}

#[typetag::serialize]
impl<D> ProtocolSim for EVMPoolState<D>
where
    D: EngineDatabaseInterface + Clone + Debug + 'static,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    fn fee(&self) -> f64 {
        todo!()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let base_address = bytes_to_address(&base.address)?;
        let quote_address = bytes_to_address(&quote.address)?;

        // Fast path: cache hit (all pools).
        if let Some(price) = self
            .spot_price_cache
            .read()
            .expect("spot_price_cache poisoned")
            .get(&(base_address, quote_address))
            .copied()
        {
            return Ok(price);
        }

        // Pools with live overrides derive spot prices from a sub-block snapshot that changes
        // intra-block, so lazily computing and caching would serve stale values. Preserve today's
        // behavior: the eagerly-warmed value is authoritative, and a genuine miss is an error.
        if self.live_overrides.is_some() {
            return Err(SimulationError::FatalError(format!(
                "Spot price not found for base token {base_address} and quote token {quote_address}"
            )));
        }

        // Non-override pool: compute this single pair on demand and cache it.
        let tokens = HashMap::from([
            (base.address.clone(), base.clone()),
            (quote.address.clone(), quote.clone()),
        ]);
        let block_overrides = self.block_env(None);
        let price =
            self.compute_spot_price(&tokens, base_address, quote_address, None, block_overrides)?;
        self.spot_price_cache
            .write()
            .expect("spot_price_cache poisoned")
            .insert((base_address, quote_address), price);
        Ok(price)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        // Read the live snapshot once so overwrites, limits and the swap use one snapshot.
        let live_snapshot = self.get_live_snapshot();
        Self::run_with_indexed_fallback(&self.id, "Swap", live_snapshot.as_ref(), |snapshot| {
            self.get_amount_out_with(&amount_in, token_in, token_out, snapshot)
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let live_snapshot = self.get_live_snapshot();
        Self::run_with_indexed_fallback(&self.id, "Limits", live_snapshot.as_ref(), |snapshot| {
            self.get_limits_with(&sell_token, &buy_token, snapshot)
        })
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(block_number) = delta
            .updated_attributes
            .get("override_block_number")
        {
            let number = <[u8; 8]>::try_from(block_number.as_ref())
                .map(u64::from_be_bytes)
                .map_err(|_| {
                    TransitionError::DecodeError(
                        "override_block_number attribute must be an 8-byte big-endian u64"
                            .to_string(),
                    )
                })?;
            self.block_overrides
                .get_or_insert_with(BlockEnvOverrides::default)
                .number = Some(number);
        }

        if let Some(block_timestamp) = delta
            .updated_attributes
            .get("override_block_timestamp")
        {
            let timestamp = <[u8; 8]>::try_from(block_timestamp.as_ref())
                .map(u64::from_be_bytes)
                .map_err(|_| {
                    TransitionError::DecodeError(
                        "override_block_timestamp attribute must be an 8-byte big-endian u64"
                            .to_string(),
                    )
                })?;
            self.block_overrides
                .get_or_insert_with(BlockEnvOverrides::default)
                .timestamp = Some(timestamp);
        }

        if self.manual_updates {
            // Directly check for "update_marker" in `updated_attributes`
            if let Some(marker) = delta
                .updated_attributes
                .get("update_marker")
            {
                // Assuming `marker` is of type `Bytes`, check its value for "truthiness"
                if !marker.is_empty() && marker[0] != 0 {
                    self.update_pool_state(tokens, balances)?;
                }
            }
        } else {
            self.update_pool_state(tokens, balances)?;
        }

        Ok(())
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        if let Some(other_state) = other
            .as_any()
            .downcast_ref::<EVMPoolState<PreCachedDB>>()
        {
            self.id == other_state.id
        } else {
            false
        }
    }

    /// Implemented manually because `typetag` macro not supports generics
    fn typetag_deserialize(&self) {
        // https://github.com/dtolnay/typetag/blob/21ae0d40c9f73443a20204ab4a134441355b52f7/impl/src/tagged_trait.rs#L140
        unreachable!("Only to catch missing typetag attribute on impl blocks. Not called.")
    }
}

#[cfg(test)]
mod tests {
    use std::default::Default;

    use num_traits::One;
    use revm::{
        primitives::KECCAK_EMPTY,
        state::{AccountInfo, Bytecode},
    };
    use serde_json::Value;
    use tycho_client::feed::BlockHeader;
    use tycho_common::models::Chain;

    use super::*;
    use crate::evm::{
        engine_db::create_engine,
        protocol::vm::{
            constants::{BALANCER_V2, ERC20_PROXY_BYTECODE},
            state_builder::EVMPoolStateBuilder,
        },
        simulation::SimulationEngine,
        tycho_models::AccountUpdate,
    };

    fn dai() -> Token {
        Token::new(
            &Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
            "DAI",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn bal() -> Token {
        Token::new(
            &Bytes::from_str("0xba100000625a3754423978a60c9317c58a424e3d").unwrap(),
            "BAL",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn dai_addr() -> Address {
        bytes_to_address(&dai().address).unwrap()
    }

    fn bal_addr() -> Address {
        bytes_to_address(&bal().address).unwrap()
    }

    async fn setup_pool_state() -> EVMPoolState<PreCachedDB> {
        let data_str = include_str!("assets/balancer_contract_storage_block_20463609.json");
        let data: Value = serde_json::from_str(data_str).expect("Failed to parse JSON");

        let accounts: Vec<AccountUpdate> = serde_json::from_value(data["accounts"].clone())
            .expect("Expected accounts to match AccountUpdate structure");

        // The process-wide `SHARED_TYCHO_DB` holds a single current-block header, so tests
        // sharing it race; a fresh database keeps each pool independent.
        let db = PreCachedDB::new().expect("failed to create test database");
        let engine: SimulationEngine<_> = create_engine(db.clone(), false).unwrap();

        let block = BlockHeader {
            number: 20463609,
            hash: Bytes::from_str(
                "0x4315fd1afc25cc2ebc72029c543293f9fd833eeb305e2e30159459c827733b1b",
            )
            .unwrap(),
            timestamp: 1722875891,
            ..Default::default()
        };

        for account in accounts.clone() {
            engine
                .state
                .init_account(
                    account.address,
                    AccountInfo {
                        balance: account.balance.unwrap_or_default(),
                        nonce: 0u64,
                        code_hash: KECCAK_EMPTY,
                        code: account
                            .code
                            .clone()
                            .map(|arg0: Vec<u8>| Bytecode::new_raw(arg0.into())),
                    },
                    None,
                    false,
                )
                .expect("Failed to initialize account");
        }
        db.update(accounts, Some(block))
            .unwrap();

        let tokens = vec![dai().address, bal().address];
        for token in &tokens {
            engine
                .state
                .init_account(
                    bytes_to_address(token).unwrap(),
                    AccountInfo {
                        balance: U256::from(0),
                        nonce: 0,
                        code_hash: KECCAK_EMPTY,
                        code: Some(Bytecode::new_raw(ERC20_PROXY_BYTECODE.into())),
                    },
                    None,
                    true,
                )
                .expect("Failed to initialize account");
        }

        let block = BlockHeader {
            number: 18485417,
            hash: Bytes::from_str(
                "0x28d41d40f2ac275a4f5f621a636b9016b527d11d37d610a45ac3a821346ebf8c",
            )
            .expect("Invalid block hash"),
            timestamp: 0,
            ..Default::default()
        };
        db.update(vec![], Some(block.clone()))
            .unwrap();

        let pool_id: String =
            "0x4626d81b3a1711beb79f4cecff2413886d461677000200000000000000000011".into();

        let stateless_contracts = HashMap::from([(
            String::from("0x3de27efa2f1aa663ae5d458857e731c129069f29"),
            Some(Vec::new()),
        )]);

        let balances = HashMap::from([
            (dai_addr(), U256::from_str("178754012737301807104").unwrap()),
            (bal_addr(), U256::from_str("91082987763369885696").unwrap()),
        ]);
        let adapter_address =
            Address::from_str("0xA2C5C98A892fD6656a7F39A2f63228C0Bc846270").unwrap();

        EVMPoolStateBuilder::new(pool_id, tokens, adapter_address)
            .balances(balances)
            .balance_owner(Address::from_str("0xBA12222222228d8Ba445958a75a0704d566BF2C8").unwrap())
            .adapter_contract_bytecode(Bytecode::new_raw(BALANCER_V2.into()))
            .stateless_contracts(stateless_contracts)
            .build(db)
            .await
            .expect("Failed to build pool state")
    }

    #[tokio::test]
    async fn test_init() {
        let pool_state = setup_pool_state().await;

        let expected_capabilities = vec![
            Capability::SellSide,
            Capability::BuySide,
            Capability::PriceFunction,
            Capability::HardLimits,
        ]
        .into_iter()
        .collect::<HashSet<_>>();

        let capabilities_adapter_contract = pool_state
            .adapter_contract
            .get_capabilities(
                &pool_state.id,
                bytes_to_address(&pool_state.tokens[0]).unwrap(),
                bytes_to_address(&pool_state.tokens[1]).unwrap(),
            )
            .unwrap();

        assert_eq!(capabilities_adapter_contract, expected_capabilities.clone());

        let capabilities_state = pool_state.clone().capabilities;

        assert_eq!(capabilities_state, expected_capabilities.clone());

        for capability in expected_capabilities.clone() {
            assert!(pool_state
                .clone()
                .ensure_capability(capability)
                .is_ok());
        }

        assert!(pool_state
            .clone()
            .ensure_capability(Capability::MarginalPrice)
            .is_err());

        // Verify all tokens are initialized in the engine
        let engine_accounts = pool_state
            .adapter_contract
            .engine
            .state
            .clone()
            .get_account_storage()
            .expect("Failed to get account storage");
        for token in pool_state.tokens.clone() {
            let account = engine_accounts
                .get_account_info(&bytes_to_address(&token).unwrap())
                .unwrap();
            assert_eq!(account.balance, U256::from(0));
            assert_eq!(account.nonce, 0u64);
            assert_eq!(account.code_hash, KECCAK_EMPTY);
            assert!(account.code.is_some());
        }

        // Verify external account is initialized in the engine
        let external_account = engine_accounts
            .get_account_info(&EXTERNAL_ACCOUNT)
            .unwrap();
        assert_eq!(external_account.balance, U256::from(*MAX_BALANCE));
        assert_eq!(external_account.nonce, 0u64);
        assert_eq!(external_account.code_hash, KECCAK_EMPTY);
        assert!(external_account.code.is_none());
    }

    #[tokio::test]
    async fn test_get_amount_out() -> Result<(), Box<dyn std::error::Error>> {
        let pool_state = setup_pool_state().await;

        let result = pool_state
            .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &dai(), &bal())
            .unwrap();
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<EVMPoolState<PreCachedDB>>()
            .unwrap();
        assert_eq!(result.amount, BigUint::from_str("137780051463393923").unwrap());
        assert_ne!(
            *new_state
                .spot_price_cache
                .read()
                .unwrap(),
            *pool_state
                .spot_price_cache
                .read()
                .unwrap()
        );
        assert!(pool_state
            .block_lasting_overwrites
            .is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_sequential_get_amount_outs() {
        let pool_state = setup_pool_state().await;

        let result = pool_state
            .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &dai(), &bal())
            .unwrap();
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<EVMPoolState<PreCachedDB>>()
            .unwrap();
        assert_eq!(result.amount, BigUint::from_str("137780051463393923").unwrap());
        assert_ne!(
            *new_state
                .spot_price_cache
                .read()
                .unwrap(),
            *pool_state
                .spot_price_cache
                .read()
                .unwrap()
        );

        let new_result = new_state
            .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &dai(), &bal())
            .unwrap();
        let new_state_second_swap = new_result
            .new_state
            .as_any()
            .downcast_ref::<EVMPoolState<PreCachedDB>>()
            .unwrap();

        assert_eq!(new_result.amount, BigUint::from_str("136964651490065626").unwrap());
        assert_ne!(
            *new_state_second_swap
                .spot_price_cache
                .read()
                .unwrap(),
            *new_state
                .spot_price_cache
                .read()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_get_amount_out_dust() {
        let pool_state = setup_pool_state().await;

        let result = pool_state
            .get_amount_out(BigUint::one(), &dai(), &bal())
            .unwrap();

        let _ = result
            .new_state
            .as_any()
            .downcast_ref::<EVMPoolState<PreCachedDB>>()
            .unwrap();
        assert_eq!(result.amount, BigUint::ZERO);
    }

    #[tokio::test]
    async fn test_get_amount_out_sell_limit() {
        let pool_state = setup_pool_state().await;

        let result = pool_state.get_amount_out(
            // sell limit is 100279494253364362835
            BigUint::from_str("100379494253364362835").unwrap(),
            &dai(),
            &bal(),
        );

        assert!(result.is_err());

        match result {
            Err(SimulationError::InvalidInput(msg1, amount_out_result)) => {
                assert_eq!(msg1, "Sell amount exceeds limit 100279494253364362835");
                assert!(amount_out_result.is_some());
            }
            _ => panic!("Test failed: was expecting an Err(SimulationError::RetryDifferentInput(_, _)) value"),
        }
    }

    #[tokio::test]
    async fn test_get_amount_limits() {
        let pool_state = setup_pool_state().await;

        let overwrites = pool_state
            .get_overwrites(
                vec![
                    bytes_to_address(&pool_state.tokens[0]).unwrap(),
                    bytes_to_address(&pool_state.tokens[1]).unwrap(),
                ],
                *MAX_BALANCE / U256::from(100),
                None,
            )
            .unwrap();
        let (dai_limit, _) = pool_state
            .get_amount_limits(
                vec![dai_addr(), bal_addr()],
                Some(overwrites.clone()),
                pool_state.block_env(None),
            )
            .unwrap();
        assert_eq!(dai_limit, U256::from_str("100279494253364362835").unwrap());

        let (bal_limit, _) = pool_state
            .get_amount_limits(
                vec![
                    bytes_to_address(&pool_state.tokens[1]).unwrap(),
                    bytes_to_address(&pool_state.tokens[0]).unwrap(),
                ],
                Some(overwrites),
                pool_state.block_env(None),
            )
            .unwrap();
        assert_eq!(bal_limit, U256::from_str("13997408640689987484").unwrap());
    }

    #[tokio::test]
    async fn test_set_spot_prices() {
        let mut pool_state = setup_pool_state().await;

        pool_state
            .set_spot_prices(
                &vec![bal(), dai()]
                    .into_iter()
                    .map(|t| (t.address.clone(), t))
                    .collect(),
            )
            .unwrap();

        let dai_bal_spot_price = *pool_state
            .spot_price_cache
            .read()
            .unwrap()
            .get(&(
                bytes_to_address(&pool_state.tokens[0]).unwrap(),
                bytes_to_address(&pool_state.tokens[1]).unwrap(),
            ))
            .unwrap();
        let bal_dai_spot_price = *pool_state
            .spot_price_cache
            .read()
            .unwrap()
            .get(&(
                bytes_to_address(&pool_state.tokens[1]).unwrap(),
                bytes_to_address(&pool_state.tokens[0]).unwrap(),
            ))
            .unwrap();
        assert_eq!(dai_bal_spot_price, 0.137_778_914_319_047_9);
        assert_eq!(bal_dai_spot_price, 7.071_503_245_428_246);
    }

    #[tokio::test]
    async fn test_set_spot_prices_without_capability() {
        // Tests set Spot Prices functions when the pool doesn't have PriceFunction capability
        let mut pool_state = setup_pool_state().await;

        pool_state
            .capabilities
            .remove(&Capability::PriceFunction);

        pool_state
            .set_spot_prices(
                &vec![bal(), dai()]
                    .into_iter()
                    .map(|t| (t.address.clone(), t))
                    .collect(),
            )
            .unwrap();

        let dai_bal_spot_price = *pool_state
            .spot_price_cache
            .read()
            .unwrap()
            .get(&(
                bytes_to_address(&pool_state.tokens[0]).unwrap(),
                bytes_to_address(&pool_state.tokens[1]).unwrap(),
            ))
            .unwrap();
        let bal_dai_spot_price = *pool_state
            .spot_price_cache
            .read()
            .unwrap()
            .get(&(
                bytes_to_address(&pool_state.tokens[1]).unwrap(),
                bytes_to_address(&pool_state.tokens[0]).unwrap(),
            ))
            .unwrap();
        assert_eq!(dai_bal_spot_price, 0.13736685496467538);
        assert_eq!(bal_dai_spot_price, 7.050354297665408);
    }

    #[tokio::test]
    async fn test_spot_price_lazy_computes_on_miss() {
        let pool_state = setup_pool_state().await;
        // Non-override pool with a cold cache: the builder does not warm spot prices.
        assert!(pool_state
            .spot_price_cache
            .read()
            .unwrap()
            .is_empty());

        // Reading a pair computes it on demand and caches it.
        let price = pool_state
            .spot_price(&dai(), &bal())
            .unwrap();
        assert!(price > 0.0);
        assert_eq!(
            pool_state
                .spot_price_cache
                .read()
                .unwrap()
                .get(&(dai_addr(), bal_addr()))
                .copied(),
            Some(price)
        );
    }

    #[tokio::test]
    async fn test_get_balance_overwrites_with_component_balances() {
        let pool_state: EVMPoolState<PreCachedDB> = setup_pool_state().await;

        let overwrites = pool_state
            .get_balance_overwrites()
            .unwrap();

        let dai_address = dai_addr();
        let bal_address = bal_addr();
        assert!(overwrites.contains_key(&dai_address));
        assert!(overwrites.contains_key(&bal_address));
    }

    #[tokio::test]
    async fn test_get_balance_overwrites_with_contract_balances() {
        let mut pool_state: EVMPoolState<PreCachedDB> = setup_pool_state().await;

        let contract_address =
            Address::from_str("0xBA12222222228d8Ba445958a75a0704d566BF2C8").unwrap();

        // Ensure no component balances are used
        pool_state.balances.clear();
        pool_state.balance_owner = None;

        // Set contract balances
        let dai_address = dai_addr();
        let bal_address = bal_addr();
        pool_state.contract_balances = HashMap::from([(
            contract_address,
            HashMap::from([
                (dai_address, U256::from_str("7500000000000000000000").unwrap()), // 7500 DAI
                (bal_address, U256::from_str("1500000000000000000000").unwrap()), // 1500 BAL
            ]),
        )]);

        let overwrites = pool_state
            .get_balance_overwrites()
            .unwrap();

        assert!(overwrites.contains_key(&dai_address));
        assert!(overwrites.contains_key(&bal_address));
    }

    #[tokio::test]
    async fn test_balance_merging_during_delta_transition() {
        use std::str::FromStr;

        let mut pool_state = setup_pool_state().await;
        let pool_id = pool_state.id.clone();

        // Test the balance merging logic more directly
        // Setup initial balances including DAI and BAL (which the pool already knows about)
        let dai_addr = dai_addr();
        let bal_addr = bal_addr();
        let new_token = Address::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(); // WETH

        // Clear and setup clean initial state
        pool_state.balances.clear();
        pool_state
            .balances
            .insert(dai_addr, U256::from(1000000000u64));
        pool_state
            .balances
            .insert(bal_addr, U256::from(2000000000u64));
        pool_state
            .balances
            .insert(new_token, U256::from(3000000000u64));

        // Create tokens mapping including the existing DAI and BAL
        let mut tokens = HashMap::new();
        tokens.insert(dai().address.clone(), dai());
        tokens.insert(bal().address.clone(), bal());

        // Simulate a delta transition with only DAI balance update (missing BAL and new_token)
        let mut component_balances = HashMap::new();
        let mut delta_balances = HashMap::new();
        // Only update DAI balance, leave others unchanged in delta
        delta_balances.insert(dai().address.clone(), Bytes::from(vec![0x77, 0x35, 0x94, 0x00])); // 2000000000 (updated value)
        component_balances.insert(pool_id.clone(), delta_balances);

        let balances = Balances { component_balances, account_balances: HashMap::new() };

        // Record initial balance count
        let initial_balance_count = pool_state.balances.len();
        assert_eq!(initial_balance_count, 3);

        // Apply delta transition
        pool_state
            .update_pool_state(&tokens, &balances)
            .unwrap();

        // Verify that all 3 balances are preserved (BAL and new_token should still be there)
        assert_eq!(
            pool_state.balances.len(),
            3,
            "All balances should be preserved after delta transition"
        );
        assert!(
            pool_state
                .balances
                .contains_key(&dai_addr),
            "DAI balance should be present"
        );
        assert!(
            pool_state
                .balances
                .contains_key(&bal_addr),
            "BAL balance should be present"
        );
        assert!(
            pool_state
                .balances
                .contains_key(&new_token),
            "New token balance should be preserved from before delta"
        );

        // Verify that updated token (DAI) has new value
        assert_eq!(
            pool_state.balances[&dai_addr],
            U256::from(2000000000u64),
            "DAI balance should be updated"
        );

        // Verify that non-updated tokens retain their original values
        assert_eq!(
            pool_state.balances[&bal_addr],
            U256::from(2000000000u64),
            "BAL balance should be unchanged"
        );
        assert_eq!(
            pool_state.balances[&new_token],
            U256::from(3000000000u64),
            "New token balance should be unchanged"
        );
    }

    #[tokio::test]
    async fn test_delta_transition_updates_block_overrides() {
        let mut pool_state = setup_pool_state().await;
        pool_state.manual_updates = true;
        pool_state.block_overrides = None;

        let delta = ProtocolStateDelta {
            component_id: pool_state.id.clone(),
            updated_attributes: HashMap::from([
                ("override_block_number".to_string(), Bytes::from(123_u64.to_be_bytes().to_vec())),
                (
                    "override_block_timestamp".to_string(),
                    Bytes::from(456_u64.to_be_bytes().to_vec()),
                ),
            ]),
            deleted_attributes: HashSet::new(),
        };

        pool_state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(
            pool_state.block_overrides,
            Some(BlockEnvOverrides { number: Some(123), timestamp: Some(456) })
        );
    }

    #[tokio::test]
    async fn test_delta_transition_updates_partial_block_overrides() {
        let mut pool_state = setup_pool_state().await;
        pool_state.manual_updates = true;
        pool_state.block_overrides =
            Some(BlockEnvOverrides { number: Some(123), timestamp: Some(456) });

        let delta = ProtocolStateDelta {
            component_id: pool_state.id.clone(),
            updated_attributes: HashMap::from([(
                "override_block_number".to_string(),
                Bytes::from(789_u64.to_be_bytes().to_vec()),
            )]),
            deleted_attributes: HashSet::new(),
        };

        pool_state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(
            pool_state.block_overrides,
            Some(BlockEnvOverrides { number: Some(789), timestamp: Some(456) })
        );
    }

    /// `update_pool_state` must refresh BOTH component balances and tracked contract balances,
    /// not treat them as mutually exclusive. Balancer V3 is hybrid (component balances at the
    /// vault owner + tracked vault contract balances); freezing contract balances while the
    /// vault's indexed reserves advance is what caused the WETH-in `BalanceNotSettled` failures.
    #[tokio::test]
    async fn test_delta_transition_refreshes_both_balance_maps() {
        let mut pool_state = setup_pool_state().await;
        let vault = Address::from_str("0xBA12222222228d8Ba445958a75a0704d566BF2C8").unwrap();
        // Non-manual pool (how Balancer V3 behaves once `manual_updates` is dropped): a contract
        // change routes the pool through delta_transition, which refreshes its balances.
        pool_state.manual_updates = false;
        pool_state.involved_contracts = HashSet::from([vault]);
        pool_state.contract_balances =
            HashMap::from([(vault, HashMap::from([(dai_addr(), U256::from(1u64))]))]);

        let delta = ProtocolStateDelta {
            component_id: pool_state.id.clone(),
            updated_attributes: HashMap::new(),
            deleted_attributes: HashSet::new(),
        };
        let balances = Balances {
            component_balances: HashMap::new(),
            account_balances: HashMap::from([(
                Bytes::from(vault.as_slice()),
                HashMap::from([(dai().address.clone(), Bytes::from(42u64).lpad(32, 0))]),
            )]),
        };

        // The spot-price refresh inside update_pool_state may fail in offline test
        // environments; the balance bookkeeping this test guards happens before it.
        let _ = pool_state.delta_transition(delta, &HashMap::new(), &balances);

        // Contract balance refreshed even though component balances are non-empty (the old
        // mutual-exclusion would have skipped this branch entirely).
        assert_eq!(pool_state.contract_balances[&vault][&dai_addr()], U256::from(42u64));
        // Component balances stay untouched by a contract-balance-only delta.
        assert_eq!(
            pool_state.balances[&dai_addr()],
            U256::from_str("178754012737301807104").unwrap()
        );
    }

    #[test]
    fn should_not_panic_at_typetag_deserialize() {
        let deserialized: Result<Box<dyn ProtocolSim>, _> = serde_json::from_str(
            r#"{"protocol":"EVMPoolState","state":{"reserve_0":1,"reserve_1":2}}"#,
        );

        assert!(deserialized.is_err());
    }

    /// A live snapshot's block number/timestamp take precedence over the static block overrides,
    /// field by field, while an absent or empty snapshot leaves them untouched.
    #[tokio::test]
    async fn test_block_env_prefers_live_overrides() {
        let mut pool_state = setup_pool_state().await;
        pool_state.block_overrides =
            Some(BlockEnvOverrides { number: Some(100), timestamp: Some(1_000) });

        // No live snapshot: the statically configured overrides are used unchanged.
        assert_eq!(
            pool_state.block_env(None),
            Some(BlockEnvOverrides { number: Some(100), timestamp: Some(1_000) })
        );

        // A live snapshot with neither field set does not touch the static overrides.
        let empty = OverrideSnapshot::default();
        assert_eq!(
            pool_state.block_env(Some(&empty)),
            Some(BlockEnvOverrides { number: Some(100), timestamp: Some(1_000) })
        );

        // Present live fields win; unset live fields keep the static value.
        let live = OverrideSnapshot {
            block_number: Some(200),
            block_timestamp: None,
            ..Default::default()
        };
        assert_eq!(
            pool_state.block_env(Some(&live)),
            Some(BlockEnvOverrides { number: Some(200), timestamp: Some(1_000) })
        );

        // With no static overrides, the live block environment stands on its own.
        pool_state.block_overrides = None;
        let live = OverrideSnapshot {
            block_number: Some(300),
            block_timestamp: Some(3_000),
            ..Default::default()
        };
        assert_eq!(
            pool_state.block_env(Some(&live)),
            Some(BlockEnvOverrides { number: Some(300), timestamp: Some(3_000) })
        );
    }

    /// Live storage is merged into the computed overwrites: a fresh contract is added, a value on a
    /// slot the baseline already sets is overridden (live wins on conflict), and empty live storage
    /// is a no-op.
    #[tokio::test]
    async fn test_get_overwrites_applies_live_storage() {
        let pool_state = setup_pool_state().await;
        let tokens = vec![dai_addr(), bal_addr()];
        let max = *MAX_BALANCE / U256::from(100);

        let baseline = pool_state
            .get_overwrites(tokens.clone(), max, None)
            .unwrap();

        // An empty live snapshot leaves the overwrites unchanged.
        let empty = OverrideSnapshot::default();
        assert_eq!(
            pool_state
                .get_overwrites(tokens.clone(), max, Some(&empty))
                .unwrap(),
            baseline
        );

        // Pick an address/slot the baseline already sets, so we can assert live wins the conflict.
        let (conflict_addr, conflict_slot, baseline_val) = {
            let (addr, slots) = baseline
                .iter()
                .next()
                .expect("baseline has overwrites");
            let (slot, val) = slots
                .iter()
                .next()
                .expect("address has slots");
            (*addr, *slot, *val)
        };
        let sentinel = baseline_val + U256::from(1);
        let fresh = Address::from([0xAB; 20]);
        assert!(
            !baseline.contains_key(&fresh),
            "fresh address must originate from the live snapshot"
        );

        let live = OverrideSnapshot {
            storage: std::sync::Arc::new(HashMap::from([
                (fresh, HashMap::from([(U256::from(7), U256::from(123))])),
                (conflict_addr, HashMap::from([(conflict_slot, sentinel)])),
            ])),
            ..Default::default()
        };
        let with_live = pool_state
            .get_overwrites(tokens, max, Some(&live))
            .unwrap();

        assert_eq!(
            with_live
                .get(&fresh)
                .and_then(|slots| slots.get(&U256::from(7))),
            Some(&U256::from(123)),
            "live storage for a fresh contract must be merged in"
        );
        assert_eq!(
            with_live
                .get(&conflict_addr)
                .and_then(|slots| slots.get(&conflict_slot)),
            Some(&sentinel),
            "live override must win on slot conflict"
        );
    }

    /// `get_live_snapshot` returns the attached snapshot only while it is fresh: an expired one is
    /// dropped (so the pool reverts to indexed state), and no channel means no snapshot. Expiry is
    /// pinned to the extremes (`1` = long past, `u64::MAX` = effectively never) so the assertions
    /// are independent of the wall clock.
    #[tokio::test]
    async fn test_get_live_snapshot_drops_expired() {
        let mut pool_state = setup_pool_state().await;

        // No channel attached: nothing to read.
        assert!(pool_state.get_live_snapshot().is_none());

        // A snapshot without an expiry never goes stale.
        let never =
            OverrideSnapshot { block_number: Some(1), expires_at: None, ..Default::default() };
        let (_never_tx, never_rx) = watch::channel(never);
        pool_state.set_live_overrides(never_rx);
        assert_eq!(
            pool_state
                .get_live_snapshot()
                .and_then(|snapshot| snapshot.block_number),
            Some(1)
        );

        // A snapshot whose expiry is far in the future is returned.
        let fresh = OverrideSnapshot {
            block_number: Some(42),
            expires_at: Some(u64::MAX),
            ..Default::default()
        };
        let (_fresh_tx, fresh_rx) = watch::channel(fresh);
        pool_state.set_live_overrides(fresh_rx);
        assert_eq!(
            pool_state
                .get_live_snapshot()
                .and_then(|snapshot| snapshot.block_number),
            Some(42)
        );

        // A snapshot whose expiry is in the past is dropped.
        let expired =
            OverrideSnapshot { block_number: Some(42), expires_at: Some(1), ..Default::default() };
        let (_expired_tx, expired_rx) = watch::channel(expired);
        pool_state.set_live_overrides(expired_rx);
        assert!(pool_state.get_live_snapshot().is_none());
    }

    /// A live snapshot that corrupts the Balancer Vault's low storage slots (pause / reentrancy
    /// state), guaranteeing that any simulation run with it applied reverts.
    fn poison_snapshot(failure_policy: FailurePolicy) -> OverrideSnapshot {
        let vault: Address = "0xBA12222222228d8Ba445958a75a0704d566BF2C8"
            .parse()
            .unwrap();
        let poisoned_slots = (0u64..10)
            .map(|slot| (U256::from(slot), U256::MAX))
            .collect();
        OverrideSnapshot {
            storage: std::sync::Arc::new(HashMap::from([(vault, poisoned_slots)])),
            failure_policy,
            ..Default::default()
        }
    }

    /// With the default [`FailurePolicy::Error`], a snapshot that breaks the simulation surfaces
    /// the failure to the caller.
    #[tokio::test]
    async fn test_failing_overrides_error_by_default() {
        let mut pool_state = setup_pool_state().await;
        let poison = poison_snapshot(FailurePolicy::Error);
        let (_tx, rx) = watch::channel(poison);
        pool_state.set_live_overrides(rx);

        assert!(pool_state
            .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &dai(), &bal())
            .is_err());
    }

    /// With [`FailurePolicy::FallbackToIndexedState`], a snapshot that breaks the simulation is
    /// dropped and the operation is retried on the plain indexed state, matching the result the
    /// pool produces without any live overrides.
    #[tokio::test]
    async fn test_failing_overrides_fall_back_to_indexed_state() {
        let pool_state = setup_pool_state().await;
        let expected = pool_state
            .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &dai(), &bal())
            .unwrap();
        let expected_limits = pool_state
            .get_limits(dai().address.clone(), bal().address.clone())
            .unwrap();

        let mut pool_state = pool_state;
        let poison = poison_snapshot(FailurePolicy::FallbackToIndexedState);
        let (_tx, rx) = watch::channel(poison);
        pool_state.set_live_overrides(rx);

        let result = pool_state
            .get_amount_out(BigUint::from_str("1000000000000000000").unwrap(), &dai(), &bal())
            .expect("must fall back to indexed state");
        assert_eq!(result.amount, expected.amount);

        let limits = pool_state
            .get_limits(dai().address.clone(), bal().address.clone())
            .expect("limits must fall back to indexed state");
        assert_eq!(limits, expected_limits);

        let tokens =
            HashMap::from([(dai().address.clone(), dai()), (bal().address.clone(), bal())]);
        pool_state
            .set_spot_prices(&tokens)
            .expect("spot prices must fall back to indexed state");
    }
}
