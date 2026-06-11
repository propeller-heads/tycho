//! Execution setup utilities module.
//!
//! This module provides helper functions for setting up execution environments,
//! including loading router and executor bytecode for various protocols.
//! The actual execution logic is from the tycho-test library.

use std::{collections::HashMap, sync::LazyLock};

use miette::{miette, IntoDiagnostic, WrapErr};
use tycho_test::execution::models::RouterOverwritesData;
pub const ROUTER_BYTECODE_JSON: &str = include_str!("../fixtures/TychoRouter.runtime.json");

// Include all executor bytecode files at compile time
const UNISWAP_V2_BYTECODE_JSON: &str = include_str!("../fixtures/UniswapV2.runtime.json");
const UNISWAP_V3_BYTECODE_JSON: &str = include_str!("../fixtures/UniswapV3.runtime.json");
const UNISWAP_V4_BYTECODE_JSON: &str = include_str!("../fixtures/UniswapV4.runtime.json");
const UNISWAP_V4_ANGSTROM_BYTECODE_JSON: &str =
    include_str!("../fixtures/UniswapV4Angstrom.runtime.json");
const BALANCER_V2_BYTECODE_JSON: &str = include_str!("../fixtures/BalancerV2.runtime.json");
const BALANCER_V3_BYTECODE_JSON: &str = include_str!("../fixtures/BalancerV3.runtime.json");
const CURVE_BYTECODE_JSON: &str = include_str!("../fixtures/Curve.runtime.json");
const MAVERICK_V2_BYTECODE_JSON: &str = include_str!("../fixtures/MaverickV2.runtime.json");
const EKUBO_V3_BYTECODE_JSON: &str = include_str!("../fixtures/EkuboV3.runtime.json");
const FLUIDV1_BYTECODE_JSON: &str = include_str!("../fixtures/FluidV1.runtime.json");
const LIQUIDITYPARTY_BYTECODE_JSON: &str = include_str!("../fixtures/LiquidityParty.runtime.json");

/// Mapping from protocol component patterns to executor bytecode JSON strings
static EXECUTOR_MAPPING: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert("uniswap_v2", UNISWAP_V2_BYTECODE_JSON);
    map.insert("sushiswap", UNISWAP_V2_BYTECODE_JSON);
    map.insert("pancakeswap_v2", UNISWAP_V2_BYTECODE_JSON);
    map.insert("uniswap_v3", UNISWAP_V3_BYTECODE_JSON);
    map.insert("pancakeswap_v3", UNISWAP_V3_BYTECODE_JSON);
    map.insert("uniswap_v4", UNISWAP_V4_BYTECODE_JSON);
    // If you would like to test any other hook, replace this bytecode with the
    // desired hook bytecode
    map.insert("uniswap_v4_hooks", UNISWAP_V4_ANGSTROM_BYTECODE_JSON);
    map.insert("vm:balancer_v2", BALANCER_V2_BYTECODE_JSON);
    map.insert("vm:balancer_v3", BALANCER_V3_BYTECODE_JSON);
    map.insert("vm:curve", CURVE_BYTECODE_JSON);
    map.insert("vm:maverick_v2", MAVERICK_V2_BYTECODE_JSON);
    map.insert("ekubo_v3", EKUBO_V3_BYTECODE_JSON);
    map.insert("fluid_v1", FLUIDV1_BYTECODE_JSON);
    map.insert("vm:liquidityparty", LIQUIDITYPARTY_BYTECODE_JSON);
    map
});

/// Get executor bytecode JSON based on protocol system
fn get_executor_bytecode_json(protocol_system: &str) -> miette::Result<&'static str> {
    for (pattern, executor_json) in EXECUTOR_MAPPING.iter() {
        if protocol_system == *pattern {
            return Ok(executor_json);
        }
    }
    Err(miette!("Unknown protocol system '{}' - no matching executor found", protocol_system))
}

/// Load executor bytecode from embedded constants based on the protocol system
pub fn load_executor_bytecode(protocol_system: &str) -> miette::Result<Vec<u8>> {
    let executor_json = get_executor_bytecode_json(protocol_system)?;

    let json_value: serde_json::Value = serde_json::from_str(executor_json)
        .into_diagnostic()
        .wrap_err("Failed to parse executor JSON")?;

    let bytecode_str = json_value["runtimeBytecode"]
        .as_str()
        .ok_or_else(|| miette!("No bytecode field found in executor JSON"))?;

    // Remove 0x prefix if present
    let bytecode_hex =
        if let Some(stripped) = bytecode_str.strip_prefix("0x") { stripped } else { bytecode_str };

    hex::decode(bytecode_hex)
        .into_diagnostic()
        .wrap_err("Failed to decode executor bytecode from hex")
}

/// Creates router overwrites data for execution simulation.
///
/// This function loads both the router bytecode and the appropriate executor bytecode
/// for the given protocol system, packaging them into a RouterOverwritesData struct
/// that can be used with tycho-test's execution functions.
///
/// # Arguments
/// * `protocol_system` - The protocol system identifier (e.g., "uniswap_v2", "balancer_v2")
///
/// # Returns
/// A `RouterOverwritesData` struct containing both router and executor bytecode.
///
/// # Errors
/// Returns an error if:
/// - Router bytecode JSON parsing fails
/// - Executor bytecode loading fails for the protocol system
/// - Bytecode hex decoding fails
pub fn create_router_overwrites_data(
    protocol_system: &str,
) -> miette::Result<RouterOverwritesData> {
    let router_bytecode = {
        let json_value: serde_json::Value = serde_json::from_str(ROUTER_BYTECODE_JSON)
            .into_diagnostic()
            .wrap_err("Failed to parse router JSON")?;
        let bytecode_str = json_value["runtimeBytecode"]
            .as_str()
            .ok_or_else(|| miette::miette!("No runtimeBytecode field found in router JSON"))?;
        let bytecode_hex = if let Some(stripped) = bytecode_str.strip_prefix("0x") {
            stripped
        } else {
            bytecode_str
        };
        hex::decode(bytecode_hex)
            .into_diagnostic()
            .wrap_err("Failed to decode router bytecode from hex")?
    };

    let executor_bytecode = load_executor_bytecode(protocol_system)?;

    Ok(RouterOverwritesData { router_bytecode, executor_bytecode, fee_calculator_bytecode: vec![] })
}
