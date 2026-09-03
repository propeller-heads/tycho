use std::{collections::HashMap, fmt::Debug};

use alloy::{core::sol, dyn_abi::SolType, primitives::U256, sol_types::SolCall};
use revm::DatabaseRef;
use tycho_common::{
    models::{token::Token, Address},
    simulation::errors::SimulationError,
    Bytes,
};

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::fluid::FluidV1,
    simulation::{BlockEnvOverrides, SimulationEngine, SimulationParameters},
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

/// Attribute carrying the resolver's ABI-encoded `PoolWithReserves` output.
pub const POOL_RESERVES_ADJUSTED_ATTRIBUTE: &str = "pool_reserves_adjusted";
/// Attribute carrying the target block's timestamp as an eight-byte big-endian `u64`.
pub const BLOCK_TIMESTAMP_ATTRIBUTE: &str = "block_timestamp";

/// Encodes Fluid resolver output and its block timestamp as pending-state attributes.
pub fn pending_state_attributes(reserves: Vec<u8>, block_timestamp: u64) -> [(String, Bytes); 2] {
    [
        (POOL_RESERVES_ADJUSTED_ATTRIBUTE.to_string(), Bytes::from(reserves)),
        (
            BLOCK_TIMESTAMP_ATTRIBUTE.to_string(),
            Bytes::from(block_timestamp.to_be_bytes().to_vec()),
        ),
    ]
}

/// The pool state decoded from the resolver's `getPoolReservesAdjusted` return bytes.
#[derive(Debug, PartialEq)]
pub(super) struct FluidPoolState {
    pub(super) collateral_reserves: super::v1::CollateralReserves,
    pub(super) debt_reserves: super::v1::DebtReserves,
    pub(super) dex_limits: super::v1::DexLimits,
    pub(super) center_price: U256,
    pub(super) fee: U256,
    pub(super) sync_time: u64,
}

pub fn decode_from_vm<D: EngineDatabaseInterface + Clone + Debug>(
    pool: &Address,
    token0: &Token,
    token1: &Token,
    resolver_address: &[u8],
    vm: &SimulationEngine<D>,
) -> Result<FluidV1, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let state = fetch_pool_state(pool, resolver_address, vm)?;
    Ok(FluidV1::new(
        pool,
        token0,
        token1,
        state.collateral_reserves,
        state.debt_reserves,
        state.dex_limits,
        state.center_price,
        state.fee,
        state.sync_time,
    ))
}

/// State the resolver call runs against, overriding what the engine's database holds.
/// `Default` overrides nothing and reads confirmed state.
#[derive(Debug, Clone, Default)]
pub struct ResolverOverrides {
    pub storage: Option<HashMap<alloy::primitives::Address, HashMap<U256, U256>>>,
    pub native_balances: Option<HashMap<alloy::primitives::Address, U256>>,
    pub block: Option<BlockEnvOverrides>,
}

/// Calls the reserves resolver for `pool` and returns the raw ABI-encoded return bytes.
///
/// Takes `overrides` by reference so one override set can be built once and reused across
/// every pool priced against the same pending block.
pub fn call_resolver<D: EngineDatabaseInterface + Clone + Debug>(
    pool: &Address,
    resolver_address: &[u8],
    engine: &SimulationEngine<D>,
    overrides: &ResolverOverrides,
) -> Result<Vec<u8>, SimulationError>
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
        overrides: overrides.storage.clone(),
        block_overrides: overrides.block.clone(),
        native_balance_overrides: overrides.native_balances.clone(),
        ..Default::default()
    };

    let res = engine
        .simulate(&params)
        .map_err(|e| SimulationError::FatalError(format!("{e}")))?;
    Ok(res.result.to_vec())
}

/// Calls the reserves resolver for `pool` against confirmed state and decodes the result,
/// stamping `sync_time` from the engine's current block.
pub(super) fn fetch_pool_state<D: EngineDatabaseInterface + Clone + Debug>(
    pool: &Address,
    resolver_address: &[u8],
    engine: &SimulationEngine<D>,
) -> Result<FluidPoolState, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let bytes = call_resolver(pool, resolver_address, engine, &ResolverOverrides::default())?;
    let sync_time = engine
        .state
        .get_current_block()
        .ok_or_else(|| {
            SimulationError::FatalError(format!(
                "VM block not set while decoding state for FluidV1: 0x{:x}",
                pool
            ))
        })?
        .timestamp;
    decode_reserves(&bytes, sync_time)
}

/// Decodes the resolver's ABI-encoded `PoolWithReserves` return bytes into pool state fields,
/// stamping `sync_time` as the moment the reserves were observed.
pub(super) fn decode_reserves(
    bytes: &[u8],
    sync_time: u64,
) -> Result<FluidPoolState, SimulationError> {
    let pool_w_reserves = PoolWithReserves::abi_decode(bytes).map_err(|e| {
        SimulationError::FatalError(format!(
            "Failed to decode pool reserves: {e} 0x{encoded}",
            encoded = hex::encode(bytes)
        ))
    })?;
    Ok(FluidPoolState {
        collateral_reserves: super::v1::CollateralReserves {
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
        debt_reserves: super::v1::DebtReserves {
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
        dex_limits: super::v1::DexLimits {
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
        center_price: pool_w_reserves.centerPrice,
        fee: pool_w_reserves.fee,
        sync_time,
    })
}

/// A `PoolWithReserves` sample with distinct values per field, so decode tests can detect
/// any field-mapping mixup.
#[cfg(test)]
pub(super) fn sample_pool_with_reserves() -> PoolWithReserves {
    PoolWithReserves {
        pool: alloy::primitives::Address::ZERO,
        token0: alloy::primitives::Address::ZERO,
        token1: alloy::primitives::Address::ZERO,
        fee: U256::from(41u64),
        centerPrice: U256::from(42u64),
        collateralReserves: CollateralReserves {
            token0RealReserves: U256::from(1u64),
            token1RealReserves: U256::from(2u64),
            token0ImaginaryReserves: U256::from(3u64),
            token1ImaginaryReserves: U256::from(4u64),
        },
        debtReserves: DebtReserves {
            token0Debt: U256::from(11u64),
            token1Debt: U256::from(12u64),
            token0RealReserves: U256::from(13u64),
            token1RealReserves: U256::from(14u64),
            token0ImaginaryReserves: U256::from(15u64),
            token1ImaginaryReserves: U256::from(16u64),
        },
        limits: DexLimits {
            withdrawableToken0: TokenLimit {
                available: U256::from(21u64),
                expandsTo: U256::from(22u64),
                expandDuration: U256::from(23u64),
            },
            withdrawableToken1: TokenLimit {
                available: U256::from(24u64),
                expandsTo: U256::from(25u64),
                expandDuration: U256::from(26u64),
            },
            borrowableToken0: TokenLimit {
                available: U256::from(27u64),
                expandsTo: U256::from(28u64),
                expandDuration: U256::from(29u64),
            },
            borrowableToken1: TokenLimit {
                available: U256::from(30u64),
                expandsTo: U256::from(31u64),
                expandDuration: U256::from(32u64),
            },
        },
    }
}

/// The `FluidPoolState` that decoding [`sample_pool_with_reserves`] must produce.
#[cfg(test)]
pub(super) fn sample_pool_state(sync_time: u64) -> FluidPoolState {
    FluidPoolState {
        collateral_reserves: super::v1::CollateralReserves {
            token0_real_reserves: U256::from(1u64),
            token1_real_reserves: U256::from(2u64),
            token0_imaginary_reserves: U256::from(3u64),
            token1_imaginary_reserves: U256::from(4u64),
        },
        debt_reserves: super::v1::DebtReserves {
            token0_real_reserves: U256::from(13u64),
            token1_real_reserves: U256::from(14u64),
            token0_imaginary_reserves: U256::from(15u64),
            token1_imaginary_reserves: U256::from(16u64),
        },
        dex_limits: super::v1::DexLimits {
            borrowable_token0: super::v1::TokenLimit {
                available: U256::from(27u64),
                expands_to: U256::from(28u64),
                expand_duration: U256::from(29u64),
            },
            borrowable_token1: super::v1::TokenLimit {
                available: U256::from(30u64),
                expands_to: U256::from(31u64),
                expand_duration: U256::from(32u64),
            },
            withdrawable_token0: super::v1::TokenLimit {
                available: U256::from(21u64),
                expands_to: U256::from(22u64),
                expand_duration: U256::from(23u64),
            },
            withdrawable_token1: super::v1::TokenLimit {
                available: U256::from(24u64),
                expands_to: U256::from(25u64),
                expand_duration: U256::from(26u64),
            },
        },
        center_price: U256::from(42u64),
        fee: U256::from(41u64),
        sync_time,
    }
}

#[cfg(test)]
mod test {
    use std::{collections::HashMap, str::FromStr};

    use alloy::{primitives::U256, sol_types::SolValue};
    use revm::state::{AccountInfo, Bytecode};
    use tycho_client::feed::BlockHeader;
    use tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    };

    use crate::evm::{
        engine_db::{
            engine_db_interface::EngineDatabaseInterface,
            simulation_db::SimulationDB,
            utils::{get_client, get_runtime},
        },
        protocol::fluid::vm::{
            call_resolver, decode_from_vm, decode_reserves, pending_state_attributes,
            ResolverOverrides, BLOCK_TIMESTAMP_ATTRIBUTE, POOL_RESERVES_ADJUSTED_ATTRIBUTE,
        },
        simulation::{BlockEnvOverrides, SimulationEngine},
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
            &vm,
        )
        .expect("decoding failed");
    }

    #[test]
    fn test_decode_reserves() {
        let encoded = SolValue::abi_encode(&super::sample_pool_with_reserves());

        let fields = decode_reserves(&encoded, 1_700_000_000).expect("decoding failed");

        assert_eq!(fields, super::sample_pool_state(1_700_000_000));
    }

    const SELFBALANCE: u8 = 0x47;
    const NUMBER: u8 = 0x43;
    const TIMESTAMP: u8 = 0x42;

    fn bytecode_reporting_engine(
        resolver: alloy::primitives::Address,
        bytecode: Vec<u8>,
    ) -> SimulationEngine<SimulationDB<crate::evm::engine_db::simulation_db::EVMProvider>> {
        let bytecode = Bytecode::new_raw(alloy::primitives::Bytes::from(bytecode));
        let mut db = SimulationDB::new(get_client(None).unwrap(), get_runtime().unwrap(), None);
        db.init_account(
            resolver,
            AccountInfo::new(U256::ZERO, 0, bytecode.hash_slow(), bytecode),
            None,
            true,
        )
        .expect("failed to init resolver account");
        db.init_account(alloy::primitives::Address::ZERO, AccountInfo::default(), None, true)
            .expect("failed to init caller account");
        db.set_block(Some(BlockHeader { number: 1, timestamp: 2, ..Default::default() }));
        SimulationEngine::new(db, false)
    }

    /// Returns the value pushed by `opcode` from the resolver execution environment.
    fn opcode_reporting_engine(
        resolver: alloy::primitives::Address,
        opcode: u8,
    ) -> SimulationEngine<SimulationDB<crate::evm::engine_db::simulation_db::EVMProvider>> {
        bytecode_reporting_engine(
            resolver,
            vec![opcode, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3],
        )
    }

    #[test]
    fn test_pending_state_attributes() {
        let attributes = HashMap::from(pending_state_attributes(vec![1, 2, 3], 42));

        assert_eq!(
            attributes.get(POOL_RESERVES_ADJUSTED_ATTRIBUTE),
            Some(&Bytes::from(vec![1, 2, 3]))
        );
        assert_eq!(
            attributes.get(BLOCK_TIMESTAMP_ATTRIBUTE),
            Some(&Bytes::from(42_u64.to_be_bytes().to_vec()))
        );
    }

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn test_call_resolver_applies_native_balance_override() {
        let resolver = Bytes::from("0xC93876C0EEd99645DD53937b25433e311881A27C");
        let resolver_address = alloy::primitives::Address::from_slice(resolver.as_ref());
        let engine = opcode_reporting_engine(resolver_address, SELFBALANCE);
        let pool = Bytes::from("0x0B1a513ee24972DAEf112bC777a5610d4325C9e7");
        let pending_balance = U256::from(4_200_000_000_000_000_000u64);

        let confirmed = call_resolver(&pool, &resolver, &engine, &ResolverOverrides::default())
            .expect("resolver call failed");
        let pending = call_resolver(
            &pool,
            &resolver,
            &engine,
            &ResolverOverrides {
                native_balances: Some(HashMap::from([(resolver_address, pending_balance)])),
                ..Default::default()
            },
        )
        .expect("resolver call failed");

        assert_eq!(
            U256::from_be_slice(&confirmed),
            U256::ZERO,
            "Without an override the resolver must see the confirmed balance."
        );
        assert_eq!(
            U256::from_be_slice(&pending),
            pending_balance,
            "The overridden balance must reach the resolver's execution."
        );
    }

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn test_call_resolver_applies_block_env_overrides() {
        let resolver = Bytes::from("0xC93876C0EEd99645DD53937b25433e311881A27C");
        let resolver_address = alloy::primitives::Address::from_slice(resolver.as_ref());
        let pool = Bytes::from("0x0B1a513ee24972DAEf112bC777a5610d4325C9e7");
        let overrides = ResolverOverrides {
            block: Some(BlockEnvOverrides {
                number: Some(23_526_115),
                timestamp: Some(1_759_842_947),
            }),
            ..Default::default()
        };

        let number = call_resolver(
            &pool,
            &resolver,
            &opcode_reporting_engine(resolver_address, NUMBER),
            &overrides,
        )
        .expect("resolver call failed");
        let timestamp = call_resolver(
            &pool,
            &resolver,
            &opcode_reporting_engine(resolver_address, TIMESTAMP),
            &overrides,
        )
        .expect("resolver call failed");

        assert_eq!(
            U256::from_be_slice(&number),
            U256::from(23_526_115),
            "The overridden block number must reach the resolver's execution."
        );
        assert_eq!(
            U256::from_be_slice(&timestamp),
            U256::from(1_759_842_947),
            "The overridden block timestamp must reach the resolver's execution."
        );
    }

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn test_call_resolver_without_overrides_reads_confirmed_block() {
        let resolver = Bytes::from("0xC93876C0EEd99645DD53937b25433e311881A27C");
        let resolver_address = alloy::primitives::Address::from_slice(resolver.as_ref());
        let pool = Bytes::from("0x0B1a513ee24972DAEf112bC777a5610d4325C9e7");

        let number = call_resolver(
            &pool,
            &resolver,
            &opcode_reporting_engine(resolver_address, NUMBER),
            &ResolverOverrides::default(),
        )
        .expect("resolver call failed");

        assert_eq!(
            U256::from_be_slice(&number),
            U256::ONE,
            "Default overrides must leave the engine's confirmed block in place."
        );
    }

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn test_call_resolver_applies_storage_override() {
        let resolver = Bytes::from("0xC93876C0EEd99645DD53937b25433e311881A27C");
        let resolver_address = alloy::primitives::Address::from_slice(resolver.as_ref());
        let pool = Bytes::from("0x0B1a513ee24972DAEf112bC777a5610d4325C9e7");
        let expected = U256::from(123);
        let engine = bytecode_reporting_engine(
            resolver_address,
            vec![
                0x60, 0x00, // PUSH1 0
                0x54, // SLOAD
                0x60, 0x00, // PUSH1 0
                0x52, // MSTORE
                0x60, 0x20, // PUSH1 32
                0x60, 0x00, // PUSH1 0
                0xf3, // RETURN
            ],
        );
        let overrides = ResolverOverrides {
            storage: Some(HashMap::from([(
                resolver_address,
                HashMap::from([(U256::ZERO, expected)]),
            )])),
            ..Default::default()
        };

        let result =
            call_resolver(&pool, &resolver, &engine, &overrides).expect("resolver call failed");

        assert_eq!(U256::from_be_slice(&result), expected);
    }

    #[test]
    fn test_decode_reserves_truncated_bytes() {
        let encoded = SolValue::abi_encode(&super::sample_pool_with_reserves());

        let result = decode_reserves(&encoded[..encoded.len() - 32], 1_700_000_000);

        assert!(result.is_err(), "truncated resolver bytes must fail to decode");
    }
}
