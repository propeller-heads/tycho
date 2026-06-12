use std::sync::LazyLock;

use alloy::primitives::{Address, U256};
use tycho_common::simulation::errors::SimulationError;

pub static EXTERNAL_ACCOUNT: LazyLock<Address> = LazyLock::new(|| {
    Address::from_slice(
        &hex::decode("f847a638E44186F3287ee9F8cAF73FF4d4B80784")
            .expect("Invalid string for external account address"),
    )
});
pub static MAX_BALANCE: LazyLock<U256> = LazyLock::new(|| U256::MAX / U256::from(2));

pub const ERC20_BYTECODE: &[u8] = include_bytes!("assets/ERC20.bin");
pub const ERC20_PROXY_BYTECODE: &[u8] = include_bytes!("assets/TokenProxy.bin");
pub const BALANCER_V2: &[u8] = include_bytes!("assets/BalancerV2SwapAdapter.evm.runtime");
pub const BALANCER_V3: &[u8] = include_bytes!("assets/BalancerV3SwapAdapter.evm.runtime");
pub const CURVE: &[u8] = include_bytes!("assets/CurveSwapAdapter.evm.runtime");
pub const FERMISWAP: &[u8] = include_bytes!("assets/FermiSwapAdapter.evm.runtime");
pub const MAVERICK_V2: &[u8] = include_bytes!("assets/MaverickV2SwapAdapter.evm.runtime");
pub const LIQUIDITY_PARTY: &[u8] = include_bytes!("assets/LiquidityPartySwapAdapter.evm.runtime");
pub fn get_adapter_file(protocol: &str) -> Result<&'static [u8], SimulationError> {
    match protocol {
        "balancer_v2" => Ok(BALANCER_V2),
        "balancer_v3" => Ok(BALANCER_V3),
        "curve" => Ok(CURVE),
        "fermiswap" => Ok(FERMISWAP),
        "maverick_v2" => Ok(MAVERICK_V2),
        "liquidityparty" => Ok(LIQUIDITY_PARTY),
        _ => Err(SimulationError::FatalError(format!("Adapter for protocol {protocol} not found"))),
    }
}
