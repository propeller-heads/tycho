use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
};

use alloy::{
    primitives::{Address, Bytes, Keccak256, U256},
    sol_types::SolValue,
};
use itertools::Itertools;
use revm::{
    primitives::KECCAK_EMPTY,
    state::{AccountInfo, Bytecode},
    DatabaseRef,
};
use tracing::warn;
use tycho_common::{simulation::errors::SimulationError, Bytes as TychoBytes};

use super::{
    constants::{EXTERNAL_ACCOUNT, MAX_BALANCE},
    models::Capability,
    state::EVMPoolState,
    tycho_simulation_contract::TychoSimulationContract,
    utils::get_code_for_contract,
};
use crate::evm::{
    engine_db::{create_engine, engine_db_interface::EngineDatabaseInterface},
    protocol::utils::bytes_to_address,
    simulation::{BlockEnvOverrides, SimulationEngine, SimulationParameters},
};

#[derive(Debug)]
/// `EVMPoolStateBuilder` is a builder pattern implementation for creating instances of
/// `EVMPoolState`.
///
/// This struct provides a flexible way to construct `EVMPoolState` objects with
/// multiple optional parameters. It handles the validation of required fields and applies default
/// values for optional parameters where necessary.
/// # Example
/// Constructing a `EVMPoolState` with only the required parameters:
/// ```rust
/// use alloy::primitives::Address;
/// use std::path::PathBuf;
/// use tycho_common::Bytes;
/// use tycho_simulation::evm::engine_db::SHARED_TYCHO_DB;
/// use tycho_simulation::evm::protocol::vm::state_builder::EVMPoolStateBuilder;
/// use tycho_simulation::evm::protocol::vm::constants::BALANCER_V2;
/// /// use tycho_common::simulation::errors::SimulationError;
/// use revm::state::Bytecode;
///
/// #[tokio::main]
/// async fn main() -> Result<(), tycho_common::simulation::errors::SimulationError> {
///     use tycho_client::feed::BlockHeader;
///
///     let pool_id: String = "0x4626d81b3a1711beb79f4cecff2413886d461677000200000000000000000011".into();
///
///     let tokens = vec![
///         Bytes::from("0x6b175474e89094c44da98b954eedeac495271d0f"),
///         Bytes::from("0xba100000625a3754423978a60c9317c58a424e3d"),
///     ];
///
///     // Set up the block for the database
///     let block = BlockHeader {
///         number: 1,
///         hash: Default::default(),
///         timestamp: 1632456789,
///         ..Default::default()
///     };
///     SHARED_TYCHO_DB.update(vec![], Some(block)).unwrap();
///
///     // Build the EVMPoolState
///     let pool_state = EVMPoolStateBuilder::new(pool_id, tokens, Address::random())
///         .adapter_contract_bytecode(Bytecode::new_raw(BALANCER_V2.into()))
///         .build(SHARED_TYCHO_DB.clone())
///         .await?;
///     Ok(())
/// }
/// ```
pub struct EVMPoolStateBuilder<D: EngineDatabaseInterface + Clone + Debug>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    id: String,
    tokens: Vec<TychoBytes>,
    balances: HashMap<Address, U256>,
    adapter_address: Address,
    balance_owner: Option<Address>,
    capabilities: Option<HashSet<Capability>>,
    involved_contracts: Option<HashSet<Address>>,
    contract_balances: HashMap<Address, HashMap<Address, U256>>,
    stateless_contracts: Option<HashMap<String, Option<Vec<u8>>>>,
    manual_updates: Option<bool>,
    trace: Option<bool>,
    engine: Option<SimulationEngine<D>>,
    adapter_contract: Option<TychoSimulationContract<D>>,
    adapter_contract_bytecode: Option<Bytecode>,
    disable_overwrite_tokens: HashSet<Address>,
    block_overrides: Option<BlockEnvOverrides>,
}

impl<D> EVMPoolStateBuilder<D>
where
    D: EngineDatabaseInterface + Clone + Debug + 'static,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    pub fn new(id: String, tokens: Vec<TychoBytes>, adapter_address: Address) -> Self {
        Self {
            id,
            tokens,
            balances: HashMap::new(),
            adapter_address,
            balance_owner: None,
            capabilities: None,
            involved_contracts: None,
            contract_balances: HashMap::new(),
            stateless_contracts: None,
            manual_updates: None,
            trace: None,
            engine: None,
            adapter_contract: None,
            adapter_contract_bytecode: None,
            disable_overwrite_tokens: HashSet::new(),
            block_overrides: None,
        }
    }

    #[deprecated(note = "Use account balances instead")]
    pub fn balance_owner(mut self, balance_owner: Address) -> Self {
        self.balance_owner = Some(balance_owner);
        self
    }

    /// Set component balances. This balance belongs to the 'balance_owner' if one is set,
    /// otherwise it belongs to the pool itself.
    pub fn balances(mut self, balances: HashMap<Address, U256>) -> Self {
        self.balances = balances;
        self
    }

    /// Set contract balances
    pub fn account_balances(
        mut self,
        account_balances: HashMap<Address, HashMap<Address, U256>>,
    ) -> Self {
        self.contract_balances = account_balances;
        self
    }

    pub fn capabilities(mut self, capabilities: HashSet<Capability>) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    pub fn involved_contracts(mut self, involved_contracts: HashSet<Address>) -> Self {
        self.involved_contracts = Some(involved_contracts);
        self
    }

    pub fn stateless_contracts(
        mut self,
        stateless_contracts: HashMap<String, Option<Vec<u8>>>,
    ) -> Self {
        self.stateless_contracts = Some(stateless_contracts);
        self
    }
    pub fn manual_updates(mut self, manual_updates: bool) -> Self {
        self.manual_updates = Some(manual_updates);
        self
    }

    pub fn trace(mut self, trace: bool) -> Self {
        self.trace = Some(trace);
        self
    }

    pub fn engine(mut self, engine: SimulationEngine<D>) -> Self {
        self.engine = Some(engine);
        self
    }

    pub fn adapter_contract(mut self, adapter_contract: TychoSimulationContract<D>) -> Self {
        self.adapter_contract = Some(adapter_contract);
        self
    }

    pub fn adapter_contract_bytecode(mut self, adapter_contract_bytecode: Bytecode) -> Self {
        self.adapter_contract_bytecode = Some(adapter_contract_bytecode);
        self
    }

    pub fn disable_overwrite_tokens(mut self, disable_overwrite_tokens: HashSet<Address>) -> Self {
        self.disable_overwrite_tokens = disable_overwrite_tokens;
        self
    }

    pub fn block_overrides(mut self, block_overrides: Option<BlockEnvOverrides>) -> Self {
        self.block_overrides = block_overrides;
        self
    }

    /// Build the final EVMPoolState object
    pub async fn build(mut self, db: D) -> Result<EVMPoolState<D>, SimulationError> {
        let engine = if let Some(engine) = &self.engine {
            engine.clone()
        } else {
            self.engine = Some(self.get_default_engine(db).await?);
            self.engine.clone().ok_or_else(|| {
                SimulationError::FatalError(
                    "Failed to get build engine: Engine not initialized".to_string(),
                )
            })?
        };

        if self.adapter_contract.is_none() {
            self.adapter_contract = Some(TychoSimulationContract::new_contract(
                self.adapter_address,
                self.adapter_contract_bytecode
                    .clone()
                    .ok_or_else(|| {
                        SimulationError::FatalError("Adapter contract bytecode not set".to_string())
                    })?,
                engine.clone(),
            )?)
        };

        let capabilities = if let Some(capabilities) = &self.capabilities {
            capabilities.clone()
        } else {
            self.get_default_capabilities()?
        };

        let adapter_contract = self.adapter_contract.ok_or_else(|| {
            SimulationError::FatalError(
                "Failed to get build engine: Adapter contract not initialized".to_string(),
            )
        })?;

        Ok(EVMPoolState::new(
            self.id,
            self.tokens,
            self.balances,
            self.balance_owner,
            self.contract_balances,
            HashMap::new(),
            capabilities,
            HashMap::new(),
            self.involved_contracts
                .unwrap_or_default(),
            self.manual_updates.unwrap_or(false),
            adapter_contract,
            self.disable_overwrite_tokens,
            self.block_overrides,
        ))
    }

    async fn get_default_engine(&self, db: D) -> Result<SimulationEngine<D>, SimulationError> {
        let engine = create_engine(db, self.trace.unwrap_or(false))?;

        engine
            .state
            .init_account(
                *EXTERNAL_ACCOUNT,
                AccountInfo {
                    balance: *MAX_BALANCE,
                    nonce: 0,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                },
                None,
                false,
            )
            .map_err(|err| {
                SimulationError::FatalError(format!(
                    "Failed to get default engine: Failed to init external account: {err:?}"
                ))
            })?;

        if let Some(stateless_contracts) = &self.stateless_contracts {
            for (address, bytecode) in stateless_contracts.iter() {
                let mut addr_str = address.clone();
                let (code, code_hash) = if bytecode.is_none() {
                    if addr_str.starts_with("call") {
                        addr_str = self
                            .get_address_from_call(&engine, &addr_str)?
                            .to_string();
                    }
                    let code = get_code_for_contract(&addr_str, None).await?;
                    (Some(code.clone()), code.hash_slow())
                } else {
                    let code =
                        Bytecode::new_raw(Bytes::from(bytecode.clone().ok_or_else(|| {
                            SimulationError::FatalError(
                                "Failed to get default engine: Byte code from stateless contracts is None".into(),
                            )
                        })?));
                    (Some(code.clone()), code.hash_slow())
                };
                let account_address: Address = addr_str.parse().map_err(|_| {
                    SimulationError::FatalError(format!(
                        "Failed to get default engine: Couldn't parse address string {address}"
                    ))
                })?;
                engine.state.init_account(
                    Address(*account_address),
                    AccountInfo { balance: Default::default(), nonce: 0, code_hash, code },
                    None,
                    false,
                ).map_err(|err| {
                    SimulationError::FatalError(format!(
                        "Failed to get default engine: Failed to init stateless contract account: {err:?}"
                    ))
                })?;
            }
        }
        Ok(engine)
    }

    fn get_default_capabilities(&mut self) -> Result<HashSet<Capability>, SimulationError> {
        let mut capabilities = Vec::new();

        // Generate all permutations of tokens and retrieve capabilities
        for tokens_pair in self.tokens.iter().permutations(2) {
            // Manually unpack the inner vector
            if let [t0, t1] = tokens_pair[..] {
                let caps = self
                    .adapter_contract
                    .clone()
                    .ok_or_else(|| {
                        SimulationError::FatalError(
                            "Failed to get default capabilities: Adapter contract not initialized"
                                .to_string(),
                        )
                    })?
                    .get_capabilities(&self.id, bytes_to_address(t0)?, bytes_to_address(t1)?)?;
                capabilities.push(caps);
            }
        }

        // Find the maximum capabilities length
        let max_capabilities = capabilities
            .iter()
            .map(|c| c.len())
            .max()
            .unwrap_or(0);

        // Intersect all capability sets
        let common_capabilities: HashSet<_> = capabilities
            .iter()
            .fold(capabilities[0].clone(), |acc, cap| acc.intersection(cap).cloned().collect());

        // Check for mismatches in capabilities
        if common_capabilities.len() < max_capabilities {
            warn!(
                "Warning: Pool {} has different capabilities depending on the token pair!",
                self.id
            );
        }
        Ok(common_capabilities)
    }

    /// Gets the address of the code - mostly used for dynamic proxy implementations. For example,
    /// some protocols have some dynamic math implementation that is given by the factory. When
    /// we swap on the pools for such protocols, it will call the factory to get the implementation
    /// and use it for the swap.
    /// This method simulates the call to the pool, which gives us the address of the
    /// implementation.
    ///
    /// # See Also
    /// [Dynamic Address Resolution Example](https://github.com/propeller-heads/propeller-protocol-lib/blob/main/docs/indexing/reserved-attributes.md#description-2)
    fn get_address_from_call(
        &self,
        engine: &SimulationEngine<D>,
        decoded: &str,
    ) -> Result<Address, SimulationError> {
        let method_name = decoded
            .split(':')
            .next_back()
            .ok_or_else(|| {
                SimulationError::FatalError(
                    "Failed to get address from call: Could not decode method name from call"
                        .into(),
                )
            })?;

        let selector = {
            let mut hasher = Keccak256::new();
            hasher.update(method_name.as_bytes());
            let result = hasher.finalize();
            result[..4].to_vec()
        };

        let to_address = decoded
            .split(':')
            .nth(1)
            .ok_or_else(|| {
                SimulationError::FatalError(
                    "Failed to get address from call: Could not decode to_address from call".into(),
                )
            })?;

        let parsed_address: Address = to_address.parse().map_err(|_| {
            SimulationError::FatalError(format!(
                "Failed to get address from call: Invalid address format: {to_address}"
            ))
        })?;

        let sim_params = SimulationParameters {
            data: selector.to_vec(),
            to: parsed_address,
            overrides: Some(HashMap::new()),
            caller: *EXTERNAL_ACCOUNT,
            value: U256::from(0u64),
            gas_limit: None,
            transient_storage: None,
            block_overrides: None,
        };

        let sim_result = engine
            .simulate(&sim_params)
            .map_err(|err| SimulationError::FatalError(err.to_string()))?;

        let address: Address = Address::abi_decode(&sim_result.result).map_err(|e| {
            SimulationError::FatalError(format!("Failed to get address from call: Failed to decode address list from simulation result {e:?}"))
        })?;

        Ok(address)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::evm::engine_db::{tycho_db::PreCachedDB, SHARED_TYCHO_DB};

    #[test]
    fn test_build_without_required_fields() {
        let id = "pool_1".to_string();
        let tokens =
            vec![TychoBytes::from_str("0000000000000000000000000000000000000000").unwrap()];
        let balances = HashMap::new();
        let adapter_address =
            Address::from_str("0xA2C5C98A892fD6656a7F39A2f63228C0Bc846270").unwrap();
        let result = tokio_test::block_on(
            EVMPoolStateBuilder::<PreCachedDB>::new(id, tokens, adapter_address)
                .balances(balances)
                .build(SHARED_TYCHO_DB.clone()),
        );

        assert!(result.is_err());
        match result.unwrap_err() {
            SimulationError::FatalError(field) => {
                assert_eq!(field, "Adapter contract bytecode not set")
            }
            _ => panic!("Unexpected error type"),
        }
    }

    #[test]
    fn test_engine_setup() {
        let id = "pool_1".to_string();
        let token2 = TychoBytes::from_str("0000000000000000000000000000000000000002").unwrap();
        let token3 = TychoBytes::from_str("0000000000000000000000000000000000000003").unwrap();
        let tokens = vec![token2.clone(), token3.clone()];
        let balances = HashMap::new();
        let adapter_address =
            Address::from_str("0xA2C5C98A892fD6656a7F39A2f63228C0Bc846270").unwrap();
        let builder =
            EVMPoolStateBuilder::<PreCachedDB>::new(id, tokens, adapter_address).balances(balances);

        let engine = tokio_test::block_on(builder.get_default_engine(SHARED_TYCHO_DB.clone()));

        assert!(engine.is_ok());
        let engine = engine.unwrap();
        assert!(engine
            .state
            .get_account_storage()
            .expect("Failed to get account storage")
            .account_present(&EXTERNAL_ACCOUNT));
    }
}
