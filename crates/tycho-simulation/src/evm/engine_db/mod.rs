use std::{collections::HashMap, fmt::Debug};

use alloy::primitives::{Address, U160};
use revm::{primitives::KECCAK_EMPTY, state::AccountInfo, DatabaseRef};
use tycho_client::feed::BlockHeader;
use tycho_common::simulation::errors::SimulationError;

use crate::evm::{
    engine_db::{
        engine_db_interface::EngineDatabaseInterface,
        tycho_db::{PreCachedDB, PreCachedDBError},
    },
    simulation::SimulationEngine,
    tycho_models::{AccountUpdate, ChangeType, ResponseAccount},
};

pub mod engine_db_interface;
pub mod simulation_db;
pub mod tycho_db;
pub mod utils;

pub static SHARED_TYCHO_DB: std::sync::LazyLock<PreCachedDB> = std::sync::LazyLock::new(|| {
    PreCachedDB::new().unwrap_or_else(|err| panic!("Failed to create PreCachedDB: {err}"))
});

/// Creates a simulation engine.
///
/// # Parameters
///
/// - `trace`: Whether to trace calls. Only meant for debugging purposes, might print a lot of data
///   to stdout.
pub fn create_engine<D: EngineDatabaseInterface + Clone + Debug>(
    db: D,
    trace: bool,
) -> Result<SimulationEngine<D>, SimulationError>
where
    <D as EngineDatabaseInterface>::Error: Debug,
    <D as DatabaseRef>::Error: Debug,
{
    let engine = SimulationEngine::new(db.clone(), trace);

    let zero_account_info =
        AccountInfo { balance: Default::default(), nonce: 0, code_hash: KECCAK_EMPTY, code: None };

    // Accounts necessary for enabling pre-compilation are initialized by default.
    engine
        .state
        .init_account(Address::ZERO, zero_account_info.clone(), None, false)
        .map_err(|e| {
            SimulationError::FatalError(format!("Failed to init zero address: {:?}", e))
        })?;

    engine
        .state
        .init_account(Address::from(U160::from(4)), zero_account_info.clone(), None, false)
        .map_err(|e| {
            SimulationError::FatalError(format!(
                "Failed to init ecrecover precompile address: {:?}",
                e
            ))
        })?;

    Ok(engine)
}

pub fn update_engine(
    db: PreCachedDB,
    block: Option<BlockHeader>,
    vm_storage: Option<HashMap<Address, ResponseAccount>>,
    account_updates: HashMap<Address, AccountUpdate>,
) -> Result<Vec<AccountUpdate>, PreCachedDBError> {
    if let Some(block) = block {
        let mut vm_updates: Vec<AccountUpdate> = Vec::new();

        for account_update in account_updates.values() {
            vm_updates.push(account_update.clone());
        }

        if let Some(vm_storage_values) = vm_storage {
            for vm_storage_values in vm_storage_values.values() {
                // ResponseAccount objects to AccountUpdate objects as required by the update method
                vm_updates.push(AccountUpdate {
                    address: vm_storage_values.address,
                    chain: vm_storage_values.chain,
                    slots: vm_storage_values.slots.clone(),
                    balance: Some(vm_storage_values.native_balance),
                    code: Some(vm_storage_values.code.clone()),
                    change: ChangeType::Creation,
                });
            }
        }

        if !vm_updates.is_empty() {
            db.update(vm_updates.clone(), Some(block))?;
        }

        Ok(vm_updates)
    } else {
        Ok(vec![])
    }
}
