use std::{env, sync::Arc};

use alloy::providers::ProviderBuilder;
use dotenvy::dotenv;
use tokio::runtime::Runtime;
use tycho_common::simulation::errors::SimulationError;

use crate::evm::engine_db::simulation_db::EVMProvider;

pub fn get_runtime() -> Result<Option<Arc<Runtime>>, SimulationError> {
    if tokio::runtime::Handle::try_current().is_ok() {
        Err(SimulationError::FatalError(
            "A Tokio runtime is already running in this context".to_string(),
        ))?;
    }

    Runtime::new()
        .map(|runtime| Some(Arc::new(runtime)))
        .map_err(|err| {
            SimulationError::FatalError(format!("Failed to create Tokio runtime: {err}"))
        })
}

pub fn get_client(rpc_url: Option<String>) -> Result<Arc<EVMProvider>, SimulationError> {
    let runtime = get_runtime()?.ok_or(SimulationError::FatalError(
        "A Tokio runtime is required to create the EVM client".to_string(),
    ))?;

    let url = rpc_url
        .or_else(|| env::var("RPC_URL").ok())
        .or_else(|| {
            dotenv().ok()?;
            env::var("RPC_URL").ok()
        })
        .ok_or_else(|| {
            SimulationError::FatalError(
                "Please provide RPC_URL environment variable or add it to .env file.".to_string(),
            )
        })?;

    let connect_future = async {
        ProviderBuilder::new()
            .connect(&url)
            .await
            .map_err(|err| {
                SimulationError::RecoverableError(format!(
                    "Failed to connect to RPC `{url}`: {err}"
                ))
            })
    };

    let client = runtime.block_on(connect_future)?;

    Ok(Arc::new(client))
}
