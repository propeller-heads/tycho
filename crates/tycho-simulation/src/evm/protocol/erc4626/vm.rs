use std::{collections::HashMap, fmt::Debug, sync::LazyLock};

use alloy::{
    core::sol,
    primitives::{Address as AlloyAddress, U256},
    sol_types::SolCall,
};
use revm::{primitives::KECCAK_EMPTY, state::AccountInfo, DatabaseRef};
use tycho_common::{
    models::{token::Token, Address},
    simulation::errors::SimulationError,
};
use tycho_ethereum::BytesCodec;

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::{
        erc4626::state::ERC4626State,
        vm::{
            constants::{EXTERNAL_ACCOUNT, MAX_BALANCE},
            erc20_token::{Overwrites, TokenProxyOverwriteFactory},
            utils::get_storage_slot_index_at_key,
        },
    },
    simulation::{SimulationEngine, SimulationParameters},
    ContractCompiler, SlotId,
};

sol! {
    function convertToShares(uint256 assets) public returns (uint256);
    function convertToAssets(uint256 shares) public returns (uint256);
    function maxDeposit(address caller) external returns (uint256);
    function maxWithdraw(address caller) external returns (uint256);
    function maxRedeem(address owner) external returns (uint256);
    function balanceOf(address owner) external view returns (uint256);
    function totalSupply() external view returns (uint256);
}

/// Base slots of the share-balance mapping in the vault layouts in use: a plain Solidity mapping
/// declared among the first ten slots (sDAI at 1, sUSDS, sUSDC and stUSDS at 2, the Spark vaults
/// at 9).
static SHARE_BALANCE_MAPPING_SLOTS: LazyLock<Vec<SlotId>> =
    LazyLock::new(|| (0u64..10).map(U256::from).collect());

/// Marker written into the candidate balance slots; far above any real share balance.
static BALANCE_SLOT_SENTINEL: LazyLock<U256> = LazyLock::new(|| U256::from(0x5e17_1e10_u64) << 160);

#[expect(
    clippy::needless_pass_by_value,
    reason = "public API; taking the engine by reference is a breaking change"
)]
pub fn decode_from_vm<D: EngineDatabaseInterface + Clone + Debug>(
    pool: &Address,
    asset_token: &Token,
    share_token: &Token,
    vm_engine: SimulationEngine<D>,
) -> Result<ERC4626State, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let total_supply = simulate_and_decode_call(
        &vm_engine,
        pool,
        AlloyAddress::ZERO,
        &totalSupplyCall {},
        None,
        "totalSupply",
    )?;

    let share_price = simulate_and_decode_call(
        &vm_engine,
        pool,
        AlloyAddress::ZERO,
        &convertToAssetsCall { shares: U256::from(10).pow(U256::from(share_token.decimals)) },
        None,
        "convertToAssets",
    )?;

    let asset_price = simulate_and_decode_call(
        &vm_engine,
        pool,
        AlloyAddress::ZERO,
        &convertToSharesCall { assets: U256::from(10).pow(U256::from(asset_token.decimals)) },
        None,
        "convertToShares",
    )?;

    vm_engine
        .state
        .init_account(
            *EXTERNAL_ACCOUNT,
            AccountInfo { balance: *MAX_BALANCE, nonce: 0, code_hash: KECCAK_EMPTY, code: None },
            None,
            false,
        )
        .map_err(|err| {
            SimulationError::FatalError(format!(
                "Failed to decode from vm: Failed to init external account: {err:?}"
            ))
        })?;

    let mut factory = TokenProxyOverwriteFactory::new(
        AlloyAddress::from_slice(asset_token.address.as_ref()),
        None,
    );
    factory.set_balance(*MAX_BALANCE, *EXTERNAL_ACCOUNT);
    let token_overwrites = factory.get_overwrites();

    let caller = AlloyAddress::from_slice(&*EXTERNAL_ACCOUNT.0);

    // Assume the caller has sufficient tokens. In simulation we only care about
    // the protocol limits, not the caller’s actual token balance.
    let max_deposit = simulate_and_decode_call(
        &vm_engine,
        pool,
        caller,
        &maxDepositCall { caller },
        Some(token_overwrites),
        "maxDeposit",
    )?;

    let max_redeem = read_max_redeem(&vm_engine, pool, caller, total_supply)?;

    Ok(ERC4626State::new(
        pool,
        asset_token,
        share_token,
        asset_price,
        share_price,
        max_deposit,
        max_redeem,
    ))
}

/// Returns the most shares the vault lets one owner redeem: idle liquidity, a utilization cap or a
/// pause, whichever binds.
///
/// `maxRedeem(owner)` is also capped by the owner's share balance, so it is read for an `owner`
/// holding the whole `total_supply`.
fn read_max_redeem<D: EngineDatabaseInterface + Clone + Debug>(
    vm_engine: &SimulationEngine<D>,
    pool: &Address,
    owner: AlloyAddress,
    total_supply: U256,
) -> Result<U256, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let balance_slot = find_share_balance_slot(vm_engine, pool, owner)?;
    let whole_supply_balance = HashMap::from([(
        AlloyAddress::from_bytes(pool),
        HashMap::from([(balance_slot, total_supply)]),
    )]);
    simulate_and_decode_call(
        vm_engine,
        pool,
        owner,
        &maxRedeemCall { owner },
        Some(whole_supply_balance),
        "maxRedeem",
    )
}

/// Returns the storage slot of the vault holding `owner`'s share balance.
///
/// Writes a distinct sentinel into `owner`'s entry of every mapping in
/// `SHARE_BALANCE_MAPPING_SLOTS` and reads it back through `balanceOf`, in a single call. Fails
/// when the vault keeps balances anywhere else.
fn find_share_balance_slot<D: EngineDatabaseInterface + Clone + Debug>(
    vm_engine: &SimulationEngine<D>,
    pool: &Address,
    owner: AlloyAddress,
) -> Result<SlotId, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let mut candidates: Overwrites = HashMap::new();
    for (index, mapping_slot) in SHARE_BALANCE_MAPPING_SLOTS
        .iter()
        .enumerate()
    {
        let slot = get_storage_slot_index_at_key(owner, *mapping_slot, ContractCompiler::Solidity);
        candidates.insert(slot, *BALANCE_SLOT_SENTINEL + U256::from(index));
    }
    let balance = simulate_and_decode_call(
        vm_engine,
        pool,
        owner,
        &balanceOfCall { owner },
        Some(HashMap::from([(AlloyAddress::from_bytes(pool), candidates.clone())])),
        "balanceOf",
    )?;

    for (slot, sentinel) in candidates {
        if sentinel == balance {
            return Ok(slot);
        }
    }
    Err(SimulationError::FatalError(format!(
        "Vault {pool} keeps share balances outside the known mapping slots (balanceOf returned \
         {balance}); add its layout to SHARE_BALANCE_MAPPING_SLOTS"
    )))
}

fn simulate_and_decode_call<D, Call, Ret>(
    vm_engine: &SimulationEngine<D>,
    pool: &Address,
    caller: AlloyAddress,
    call: &Call,
    overrides: Option<HashMap<AlloyAddress, Overwrites>>,
    method: &str,
) -> Result<Ret, SimulationError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
    Call: SolCall<Return = Ret>,
{
    let data = call.abi_encode();
    let to = AlloyAddress::from_bytes(pool);

    let params = SimulationParameters { caller, to, data, overrides, ..Default::default() };

    let res = vm_engine
        .simulate(&params)
        .map_err(|e| SimulationError::FatalError(format!("{method} simulate failed: {e}")))?;

    Call::abi_decode_returns(res.result.as_ref())
        .map_err(|e| SimulationError::FatalError(format!("{method} decode failed: {e}")))
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use alloy::primitives::{Address as AlloyAddress, Bytes as AlloyBytes, U256};
    use num_bigint::BigUint;
    use revm::{bytecode::Bytecode, state::AccountInfo};
    use tycho_client::feed::BlockHeader;
    use tycho_common::{
        models::{token::Token, Chain},
        simulation::protocol_sim::ProtocolSim,
        Bytes,
    };

    use super::find_share_balance_slot;
    use crate::evm::{
        engine_db::{
            engine_db_interface::EngineDatabaseInterface,
            simulation_db::{EVMProvider, SimulationDB},
            tycho_db::PreCachedDB,
            utils::{get_client, get_runtime},
        },
        protocol::{erc4626::vm::decode_from_vm, vm::utils::get_storage_slot_index_at_key},
        simulation::SimulationEngine,
        ContractCompiler,
    };

    const VAULT: &str = "0x0000000000000000000000000000000000001234";
    const OWNER: AlloyAddress = AlloyAddress::repeat_byte(0xab);

    /// An engine holding `OWNER` and, at `VAULT`, a contract that runs `runtime` for any call.
    fn engine_with_vault(runtime: &'static [u8]) -> SimulationEngine<PreCachedDB> {
        let state = PreCachedDB::new().expect("PreCachedDB creation succeeds");
        let bytecode = Bytecode::new_raw(AlloyBytes::from_static(runtime));
        let account = AccountInfo::new(U256::ZERO, 0, bytecode.hash_slow(), bytecode);
        state
            .init_account(AlloyAddress::from_str(VAULT).unwrap(), account, None, true)
            .expect("vault account init succeeds");
        // The zero address is the block's coinbase, which revm loads on every call.
        for account in [OWNER, AlloyAddress::ZERO] {
            state
                .init_account(account, AccountInfo::default(), None, true)
                .expect("account init succeeds");
        }
        state
            .update(Vec::new(), Some(BlockHeader { number: 1, timestamp: 2, ..Default::default() }))
            .expect("block header update succeeds");
        SimulationEngine::new(state, false)
    }

    #[test]
    fn test_find_share_balance_slot_in_solidity_mapping() {
        // Returns `sload(keccak256(abi.encode(calldataload(4), 2)))`: a `balanceOf` reading a
        // mapping declared at slot 2.
        let engine = engine_with_vault(&[
            0x60, 0x04, 0x35, // CALLDATALOAD(4)
            0x60, 0x00, 0x52, // MSTORE(0, owner)
            0x60, 0x02, 0x60, 0x20, 0x52, // MSTORE(32, 2)
            0x60, 0x40, 0x60, 0x00, 0x20, // KECCAK256(0, 64)
            0x54, // SLOAD
            0x60, 0x00, 0x52, // MSTORE(0, balance)
            0x60, 0x20, 0x60, 0x00, 0xf3, // RETURN(0, 32)
        ]);
        let slot = find_share_balance_slot(&engine, &Bytes::from_str(VAULT).unwrap(), OWNER)
            .expect("slot 2 is a known layout");

        assert_eq!(
            slot,
            get_storage_slot_index_at_key(OWNER, U256::from(2), ContractCompiler::Solidity)
        );
    }

    #[test]
    fn test_find_share_balance_slot_fails_for_unknown_layout() {
        // `balanceOf` returns 7 whatever the storage holds.
        let engine = engine_with_vault(&[
            0x60, 0x07, 0x60, 0x00, 0x52, // MSTORE(0, 7)
            0x60, 0x20, 0x60, 0x00, 0xf3, // RETURN(0, 32)
        ]);

        let result = find_share_balance_slot(&engine, &Bytes::from_str(VAULT).unwrap(), OWNER);

        let Err(err) = result else { panic!("found a slot in a vault without balances") };
        assert!(
            err.to_string()
                .contains("outside the known mapping slots"),
            "{err}"
        );
    }

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(20000)],
            Chain::Ethereum,
            100,
        )
    }

    fn rpc_engine(block: BlockHeader) -> SimulationEngine<SimulationDB<EVMProvider>> {
        let mut db = SimulationDB::new(get_client(None).unwrap(), get_runtime().unwrap(), None);
        db.set_block(Some(block));
        SimulationEngine::new(db, false)
    }

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn test_decode_simulation_db() {
        let usdc = token("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", "usdc", 6);
        let sp_usdc = token("0x28B3a8fb53B741A8Fd78c0fb9A6B2393d896a43d", "sp_usdc", 6);

        let block = BlockHeader {
            number: 23881700,
            hash: Bytes::from_str(
                "0xb11cb57ba2620d0f31da3a3c531977707569b796003ba65c44eaca990e6f2957",
            )
            .unwrap(),
            timestamp: 1764145355,
            ..Default::default()
        };

        decode_from_vm(
            &Bytes::from("0x28B3a8fb53B741A8Fd78c0fb9A6B2393d896a43d"),
            &usdc,
            &sp_usdc,
            rpc_engine(block),
        )
        .expect("decoding failed");
    }

    fn test_block() -> BlockHeader {
        BlockHeader {
            number: 26332682,
            hash: Bytes::from_str(
                "0xaf14ba2c3bc28c67197fbf5b38f3e2f3aed42b174f86e612ea219308a670a2bd",
            )
            .unwrap(),
            timestamp: 1793689739,
            ..Default::default()
        }
    }

    /// Asserts the redeem limit decoded at block 26332682 equals the vault's `maxRedeem` for an
    /// owner of the whole supply, read from chain at that block.
    fn assert_redeem_limit(vault: &str, asset: &Token, share: &Token, expected: u128) {
        let state = decode_from_vm(
            &Bytes::from_str(vault).unwrap(),
            asset,
            share,
            rpc_engine(test_block()),
        )
        .expect("decoding failed");

        let (max_shares_in, _) = state
            .get_limits(share.address.clone(), asset.address.clone())
            .expect("limits");
        assert_eq!(max_shares_in, BigUint::from(expected));
    }

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn test_stusds_redeem_limit_is_unused_funds() {
        // 10.7% of the 166.9M stUSDS supply: the rest is lent to Lockstake borrowers.
        assert_redeem_limit(
            "0x99cd4ec3f88a45940936f469e4bb72a2a701eeb9",
            &token("0xdc035d45d973e3ec169d2276ddab16f1e407384f", "USDS", 18),
            &token("0x99cd4ec3f88a45940936f469e4bb72a2a701eeb9", "stUSDS", 18),
            17_863_966_528_431_831_941_791_025,
        );
    }

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn test_susds_redeem_limit_is_whole_supply() {
        assert_redeem_limit(
            "0xa3931d71877c0e7a3148cb7eb4463524fec27fbd",
            &token("0xdc035d45d973e3ec169d2276ddab16f1e407384f", "USDS", 18),
            &token("0xa3931d71877c0e7a3148cb7eb4463524fec27fbd", "sUSDS", 18),
            4_072_928_603_697_856_501_415_619_567,
        );
    }
}
