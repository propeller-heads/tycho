use std::{collections::HashMap, fmt::Debug};

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
        },
    },
    simulation::{SimulationEngine, SimulationParameters},
};

sol! {
    function convertToShares(uint256 assets) public returns (uint256);
    function convertToAssets(uint256 shares) public returns (uint256);
    function maxDeposit(address caller) external returns (uint256);
    function maxWithdraw(address caller) external returns (uint256);
    function totalSupply() external view returns (uint256);
}

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
        totalSupplyCall {},
        None,
        "totalSupply",
    )?;

    let share_price = simulate_and_decode_call(
        &vm_engine,
        pool,
        AlloyAddress::ZERO,
        convertToAssetsCall { shares: U256::from(10).pow(U256::from(share_token.decimals)) },
        None,
        "convertToAssets",
    )?;

    let asset_price = simulate_and_decode_call(
        &vm_engine,
        pool,
        AlloyAddress::ZERO,
        convertToSharesCall { assets: U256::from(10).pow(U256::from(asset_token.decimals)) },
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
        maxDepositCall { caller },
        Some(token_overwrites.clone()),
        "maxDeposit",
    )?;

    // Use the vault's totalSupply as the upper bound for maxRedeem.
    // This represents the maximum amount of shares that can be burned, since
    // a user cannot redeem more shares than the total supply of the vault.
    Ok(ERC4626State::new(
        pool,
        asset_token,
        share_token,
        asset_price,
        share_price,
        max_deposit,
        total_supply,
    ))
}

fn simulate_and_decode_call<D, Call, Ret>(
    vm_engine: &SimulationEngine<D>,
    pool: &Address,
    caller: AlloyAddress,
    call: Call,
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

    let params = SimulationParameters {
        caller,
        to,
        data,
        value: U256::ZERO,
        overrides,
        gas_limit: None,
        transient_storage: None,
        block_overrides: None,
    };

    let res = vm_engine
        .simulate(&params)
        .map_err(|e| SimulationError::FatalError(format!("{method} simulate failed: {e}")))?;

    Call::abi_decode_returns(res.result.as_ref())
        .map_err(|e| SimulationError::FatalError(format!("{method} decode failed: {e}")))
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
        protocol::erc4626::vm::decode_from_vm,
        simulation::SimulationEngine,
    };

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn test_decode_simulation_db() {
        let usdc = Token::new(
            &Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            "usdc",
            6,
            0,
            &[Some(20000)],
            Chain::Ethereum,
            100,
        );
        let sp_usdc = Token::new(
            &Bytes::from_str("0x28B3a8fb53B741A8Fd78c0fb9A6B2393d896a43d").unwrap(),
            "sp_usdc",
            6,
            0,
            &[Some(2000)],
            Chain::Ethereum,
            100,
        );

        let block = BlockHeader {
            number: 23881700,
            hash: Bytes::from_str(
                "0xb11cb57ba2620d0f31da3a3c531977707569b796003ba65c44eaca990e6f2957",
            )
            .unwrap(),
            timestamp: 1764145355,
            ..Default::default()
        };
        let mut db = SimulationDB::new(get_client(None).unwrap(), get_runtime().unwrap(), None);
        db.set_block(Some(block));
        let vm = SimulationEngine::new(db, false);

        decode_from_vm(
            &Bytes::from("0x28B3a8fb53B741A8Fd78c0fb9A6B2393d896a43d"),
            &usdc,
            &sp_usdc,
            vm,
        )
        .expect("decoding failed");
    }
}
