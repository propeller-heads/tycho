use std::fmt::Debug;

use alloy::{core::sol, dyn_abi::SolType, primitives::U256, sol_types::SolCall};
use revm::DatabaseRef;
use tycho_common::{
    models::{token::Token, Address},
    simulation::errors::SimulationError,
};

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::fluid::FluidV1,
    simulation::{SimulationEngine, SimulationParameters},
};

sol! {
    struct CollateralReserves {
        uint token0RealReserves;
        uint token1RealReserves;
        uint token0ImaginaryReserves;
        uint token1ImaginaryReserves;
    }

    struct DebtReserves {
        uint token0Debt;
        uint token1Debt;
        uint token0RealReserves;
        uint token1RealReserves;
        uint token0ImaginaryReserves;
        uint token1ImaginaryReserves;
    }

    struct TokenLimit {
        uint256 available; // maximum available swap amount
        uint256 expandsTo; // maximum amount the available swap amount expands to
        uint256 expandDuration; // duration for `available` to grow to `expandsTo`
    }

    struct DexLimits {
        TokenLimit withdrawableToken0;
        TokenLimit withdrawableToken1;
        TokenLimit borrowableToken0;
        TokenLimit borrowableToken1;
    }

    struct PoolWithReserves {
        address pool;
        address token0;
        address token1;
        uint256 fee;
        uint256 centerPrice;
        CollateralReserves collateralReserves;
        DebtReserves debtReserves;
        DexLimits limits;
    }

    function getPoolReservesAdjusted(address pool_) public returns (PoolWithReserves memory poolReserves_);
}

pub fn decode_from_vm<D: EngineDatabaseInterface + Clone + Debug>(
    pool: &Address,
    token0: &Token,
    token1: &Token,
    resolver_address: &[u8],
    vm: SimulationEngine<D>,
) -> Result<FluidV1, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let reserves_call = getPoolReservesAdjustedCall {
        pool_: alloy::primitives::Address::from_slice(pool.as_ref()),
    };
    let data = reserves_call.abi_encode();

    let to = alloy::primitives::Address::from_slice(resolver_address);
    let params = SimulationParameters {
        caller: alloy::primitives::Address::ZERO,
        to,
        data,
        value: U256::ZERO,
        overrides: None,
        gas_limit: None,
        transient_storage: None,
        block_overrides: None,
    };

    let res = vm
        .simulate(&params)
        .map_err(|e| SimulationError::FatalError(format!("{e}")))?;

    let pool_w_reserves = PoolWithReserves::abi_decode(res.result.as_ref()).map_err(|e| {
        SimulationError::FatalError(format!("Failed to decode pool reserves: {e} {}", res.result))
    })?;
    let state = FluidV1::new(
        pool,
        token0,
        token1,
        super::v1::CollateralReserves {
            token0_real_reserves: pool_w_reserves
                .collateralReserves
                .token0RealReserves,
            token1_real_reserves: pool_w_reserves
                .collateralReserves
                .token1RealReserves,
            token0_imaginary_reserves: pool_w_reserves
                .collateralReserves
                .token0ImaginaryReserves,
            token1_imaginary_reserves: pool_w_reserves
                .collateralReserves
                .token1ImaginaryReserves,
        },
        super::v1::DebtReserves {
            token0_real_reserves: pool_w_reserves
                .debtReserves
                .token0RealReserves,
            token1_real_reserves: pool_w_reserves
                .debtReserves
                .token1RealReserves,
            token0_imaginary_reserves: pool_w_reserves
                .debtReserves
                .token0ImaginaryReserves,
            token1_imaginary_reserves: pool_w_reserves
                .debtReserves
                .token1ImaginaryReserves,
        },
        super::v1::DexLimits {
            borrowable_token0: super::v1::TokenLimit {
                available: pool_w_reserves
                    .limits
                    .borrowableToken0
                    .available,
                expands_to: pool_w_reserves
                    .limits
                    .borrowableToken0
                    .expandsTo,
                expand_duration: pool_w_reserves
                    .limits
                    .borrowableToken0
                    .expandDuration,
            },
            borrowable_token1: super::v1::TokenLimit {
                available: pool_w_reserves
                    .limits
                    .borrowableToken1
                    .available,
                expands_to: pool_w_reserves
                    .limits
                    .borrowableToken1
                    .expandsTo,
                expand_duration: pool_w_reserves
                    .limits
                    .borrowableToken1
                    .expandDuration,
            },
            withdrawable_token0: super::v1::TokenLimit {
                available: pool_w_reserves
                    .limits
                    .withdrawableToken0
                    .available,
                expands_to: pool_w_reserves
                    .limits
                    .withdrawableToken0
                    .expandsTo,
                expand_duration: pool_w_reserves
                    .limits
                    .withdrawableToken0
                    .expandDuration,
            },
            withdrawable_token1: super::v1::TokenLimit {
                available: pool_w_reserves
                    .limits
                    .withdrawableToken1
                    .available,
                expands_to: pool_w_reserves
                    .limits
                    .withdrawableToken1
                    .expandsTo,
                expand_duration: pool_w_reserves
                    .limits
                    .withdrawableToken1
                    .expandDuration,
            },
        },
        pool_w_reserves.centerPrice,
        pool_w_reserves.fee,
        vm.state
            .get_current_block()
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "VM block not set while decoding state for FluidV1: 0x{:x}",
                    pool
                ))
            })?
            .timestamp,
    );
    Ok(state)
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use tycho_client::feed::BlockHeader;
    use tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    };

    use crate::evm::{
        engine_db::{
            simulation_db::SimulationDB,
            utils::{get_client, get_runtime},
        },
        protocol::fluid::vm::decode_from_vm,
        simulation::SimulationEngine,
    };

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn test_decode_simulation_db() {
        let wsteth = Token::new(
            &Bytes::from_str("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0").unwrap(),
            "wsteth",
            18,
            0,
            &[Some(20000)],
            Chain::Ethereum,
            100,
        );
        let eth = Token::new(
            &Bytes::from_str("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE").unwrap(),
            "ETH",
            18,
            0,
            &[Some(2000)],
            Chain::Ethereum,
            100,
        );

        let block = BlockHeader {
            number: 23526115,
            hash: Bytes::from_str(
                "0xfe5df4d77d2e4ce5660f2329084d5ef238b6671bdcf961ce0a510071af7a2275",
            )
            .unwrap(),
            timestamp: 1759842947,
            ..Default::default()
        };
        let mut db = SimulationDB::new(get_client(None).unwrap(), get_runtime().unwrap(), None);
        db.set_block(Some(block));
        let vm = SimulationEngine::new(db, false);

        decode_from_vm(
            &Bytes::from("0x0B1a513ee24972DAEf112bC777a5610d4325C9e7"),
            &wsteth,
            &eth,
            &Bytes::from("0xC93876C0EEd99645DD53937b25433e311881A27C"),
            vm,
        )
        .expect("decoding failed");
    }
}
