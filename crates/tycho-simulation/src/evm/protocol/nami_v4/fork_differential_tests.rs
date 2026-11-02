//! Fork variant of the differential test: `NamiV4State` quotes against Nami's live Base WETH/USDC
//! pool and its production plugin, forked over RPC.
//!
//! Every swap runs through Nami's deployed `SwapRouter` at simulated blocks after the fork block,
//! spread over several hours so the plugin's volatility oracle moves the fee. Before each swap the
//! test dry-runs it once, which loads every account and storage slot the swap reads (the plugin's
//! oracle included) from the RPC into the local cache, then loads that cache into
//! `SHARED_TYCHO_DB`, quotes with `NamiV4State`, executes the swap and requires the quote and the
//! post-swap pool state to match it exactly.
use std::{collections::HashSet, time::Instant};

use alloy::{
    eips::BlockId,
    primitives::{
        aliases::{I24, U160},
        Address, Bytes as AlloyBytes, Log, U256,
    },
    providers::{DynProvider, Provider, ProviderBuilder},
    sol,
    sol_types::{SolCall, SolEvent},
};
use revm::{
    context::{
        result::{ExecutionResult, Output},
        BlockEnv, CfgEnv, Context, TxEnv,
    },
    database::{AlloyDB, CacheDB, WrapDatabaseAsync},
    primitives::{hardfork::SpecId, TxKind},
    state::AccountInfo,
    Database, ExecuteCommitEvm, ExecuteEvm, MainBuilder, MainContext,
};
use tycho_client::feed::BlockHeader;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::{BlockContext, ProtocolSim},
    Bytes,
};

use super::NamiV4State;
use crate::evm::{
    engine_db::SHARED_TYCHO_DB,
    protocol::{
        u256_num::u256_to_biguint,
        utils::uniswap::tick_math::{MAX_TICK, MIN_TICK},
    },
    tycho_models::{AccountUpdate, ChangeType},
};

sol! {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        address deployer;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 limitSqrtPrice;
    }

    interface ISwapRouter {
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256 amountOut);
    }

    interface IWeth {
        function deposit() external payable;
        function approve(address spender, uint256 amount) external returns (bool);
    }

    interface IPool {
        function plugin() external view returns (address);
        function globalState() external view returns (
            uint160 price,
            int24 tick,
            uint16 lastFee,
            uint8 pluginConfig,
            uint16 communityFee,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function ticks(int24 tick) external view returns (
            uint256 liquidityTotal,
            int128 liquidityDelta,
            int24 prevTick,
            int24 nextTick,
            uint256 outerFeeGrowth0Token,
            uint256 outerFeeGrowth1Token
        );
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 price,
            uint128 liquidity,
            int24 tick
        );
        event SwapFee(address indexed sender, uint24 overrideFee, uint24 pluginFee);
    }
}

const BASE_CHAIN_ID: u64 = 8453;
const FORK_BLOCK: u64 = 52_562_900;
const POOL: Address = alloy::primitives::address!("0E8AF367DbA0c9CB4674D38539eF3465A25a7237");
const SWAP_ROUTER: Address =
    alloy::primitives::address!("DB5F3B86f2CE75a8b2aD0120C01193CaDA30F4c8");
const WETH: Address = alloy::primitives::address!("4200000000000000000000000000000000000006");
const USDC: Address = alloy::primitives::address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");

type ForkDb = CacheDB<WrapDatabaseAsync<AlloyDB<alloy::network::Ethereum, DynProvider>>>;

/// Base forked at `FORK_BLOCK`, advanced locally: every transaction is sent by `owner`.
struct ForkedBase {
    db: ForkDb,
    block: BlockEnv,
    owner: Address,
}

impl ForkedBase {
    fn new(rpc_url: &str) -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let provider = ProviderBuilder::new()
            .connect_http(
                rpc_url
                    .parse()
                    .expect("valid BASE_RPC_URL"),
            )
            .erased();
        let fork_timestamp = runtime
            .block_on(async {
                provider
                    .get_block_by_number(FORK_BLOCK.into())
                    .await
            })
            .expect("fork block request")
            .expect("fork block exists")
            .header
            .timestamp;
        let alloy_db = AlloyDB::new(provider, BlockId::number(FORK_BLOCK));
        let mut db = CacheDB::new(WrapDatabaseAsync::with_runtime(alloy_db, runtime));

        let owner = Address::repeat_byte(0x4c);
        db.insert_account_info(
            owner,
            AccountInfo { balance: U256::from(10u128.pow(21)), ..Default::default() },
        );
        let block = BlockEnv {
            number: U256::from(FORK_BLOCK),
            timestamp: U256::from(fork_timestamp),
            gas_limit: 30_000_000,
            ..Default::default()
        };
        ForkedBase { db, block, owner }
    }

    fn tx(&mut self, to: Address, data: Vec<u8>, value: U256) -> TxEnv {
        let nonce = self
            .db
            .basic(self.owner)
            .expect("owner account")
            .map_or(0, |account| account.nonce);
        TxEnv {
            caller: self.owner,
            kind: TxKind::Call(to),
            data: AlloyBytes::from(data),
            value,
            nonce,
            gas_limit: 30_000_000,
            gas_price: 0,
            chain_id: Some(BASE_CHAIN_ID),
            ..Default::default()
        }
    }

    fn cfg() -> CfgEnv {
        let mut cfg = CfgEnv::new_with_spec(SpecId::PRAGUE);
        cfg.chain_id = BASE_CHAIN_ID;
        cfg
    }

    /// Executes a call and commits it.
    fn call(&mut self, to: Address, data: Vec<u8>, value: U256) -> (AlloyBytes, Vec<Log>) {
        let tx = self.tx(to, data, value);
        let mut evm = Context::mainnet()
            .with_db(&mut self.db)
            .with_cfg(Self::cfg())
            .with_block(self.block.clone())
            .build_mainnet();
        match evm
            .transact_commit(tx)
            .expect("EVM execution error")
        {
            ExecutionResult::Success { output: Output::Call(output), logs, .. } => (output, logs),
            other => panic!("call to {to} failed: {other:?}"),
        }
    }

    /// Executes a call without committing it. Everything it reads stays in the local cache.
    fn dry_run(&mut self, to: Address, data: Vec<u8>) -> AlloyBytes {
        let tx = self.tx(to, data, U256::ZERO);
        let mut evm = Context::mainnet()
            .with_db(&mut self.db)
            .with_cfg(Self::cfg())
            .with_block(self.block.clone())
            .build_mainnet();
        match evm
            .transact(tx)
            .expect("EVM execution error")
            .result
        {
            ExecutionResult::Success { output: Output::Call(output), .. } => output,
            other => panic!("dry run to {to} failed: {other:?}"),
        }
    }

    /// Loads every contract the local cache holds into `SHARED_TYCHO_DB`.
    fn sync_to_shared_db(&self) {
        let updates = self
            .db
            .cache
            .accounts
            .iter()
            .filter_map(|(address, account)| {
                let code = account.info.code.clone().or_else(|| {
                    self.db
                        .cache
                        .contracts
                        .get(&account.info.code_hash)
                        .cloned()
                })?;
                if code.is_empty() {
                    return None;
                }
                Some(AccountUpdate::new(
                    *address,
                    Chain::Base,
                    account
                        .storage
                        .iter()
                        .map(|(slot, value)| (*slot, *value))
                        .collect(),
                    Some(account.info.balance),
                    Some(code.original_bytes().to_vec()),
                    ChangeType::Creation,
                ))
            })
            .collect();
        let header = BlockHeader {
            number: self.block.number.to::<u64>(),
            timestamp: self.block.timestamp.to::<u64>(),
            ..Default::default()
        };
        SHARED_TYCHO_DB
            .update(updates, Some(header))
            .expect("shared DB update");
    }

    fn advance(&mut self, seconds: u64) {
        self.block.number += U256::ONE;
        self.block.timestamp += U256::from(seconds);
    }

    fn swap_calldata(&self, zero_for_one: bool, amount_in: U256) -> Vec<u8> {
        let (token_in, token_out) = if zero_for_one { (WETH, USDC) } else { (USDC, WETH) };
        ISwapRouter::exactInputSingleCall {
            params: ExactInputSingleParams {
                tokenIn: token_in,
                tokenOut: token_out,
                deployer: Address::ZERO,
                recipient: self.owner,
                deadline: self.block.timestamp + U256::from(600),
                amountIn: amount_in,
                amountOutMinimum: U256::ZERO,
                // Zero lets the router use the loosest limit, as the simulation does.
                limitSqrtPrice: U160::ZERO,
            },
        }
        .abi_encode()
    }

    /// Reads the live pool into a `NamiV4State` quoting for the current block, walking its linked
    /// list of initialized ticks.
    fn pool_state(&mut self) -> NamiV4State {
        let output = self.dry_run(POOL, IPool::globalStateCall {}.abi_encode());
        let global_state = IPool::globalStateCall::abi_decode_returns(&output).unwrap();
        let output = self.dry_run(POOL, IPool::liquidityCall {}.abi_encode());
        let liquidity = IPool::liquidityCall::abi_decode_returns(&output).unwrap();
        let output = self.dry_run(POOL, IPool::pluginCall {}.abi_encode());
        let plugin = IPool::pluginCall::abi_decode_returns(&output).unwrap();

        let mut ticks = Vec::new();
        let mut tick = MIN_TICK;
        loop {
            let call = IPool::ticksCall { tick: I24::try_from(tick).unwrap() };
            let output = self.dry_run(POOL, call.abi_encode());
            let info = IPool::ticksCall::abi_decode_returns(&output).unwrap();
            if !info.liquidityTotal.is_zero() {
                ticks.push((tick, info.liquidityDelta));
            }
            let next = info.nextTick.as_i32();
            if tick == MAX_TICK || next <= tick {
                break;
            }
            tick = next;
        }

        NamiV4State::new(
            POOL,
            liquidity,
            U256::from(global_state.price),
            global_state.tick.as_i32(),
            ticks,
            u32::from(global_state.lastFee),
            Some(plugin),
            global_state.pluginConfig,
            BlockContext::new(self.block.number.to::<u64>(), self.block.timestamp.to::<u64>()),
        )
    }
}

fn token(address: Address, decimals: u32) -> Token {
    Token::new(&Bytes::from(address.to_vec()), "T", decimals, 0, &[Some(10_000)], Chain::Base, 100)
}

/// The output amount and the fee the plugin set, from the pool's events.
fn swap_result(logs: &[Log], zero_for_one: bool) -> (U256, u32) {
    let mut amount_out = None;
    let mut fee = None;
    for log in logs
        .iter()
        .filter(|log| log.address == POOL)
    {
        if let Ok(event) = IPool::Swap::decode_log(log) {
            // A later Swap event in the same transaction would be an after-swap back-run, not
            // the quoted swap, so keep the first.
            if amount_out.is_none() {
                let out = if zero_for_one { event.data.amount1 } else { event.data.amount0 };
                amount_out = Some(out.unsigned_abs());
            }
        } else if let Ok(event) = IPool::SwapFee::decode_log(log) {
            fee.get_or_insert(event.data.overrideFee.to::<u32>());
        }
    }
    (amount_out.expect("pool emitted Swap"), fee.expect("pool emitted SwapFee"))
}

#[test]
#[ignore = "Requires BASE_RPC_URL (archive)"]
fn quotes_match_the_live_base_pool() {
    let started = Instant::now();
    let rpc_url = std::env::var("BASE_RPC_URL").expect("BASE_RPC_URL");
    let mut chain = ForkedBase::new(&rpc_url);

    chain.call(WETH, IWeth::depositCall {}.abi_encode(), U256::from(10u128.pow(18)));
    for token in [WETH, USDC] {
        chain.call(
            token,
            IWeth::approveCall { spender: SWAP_ROUTER, amount: U256::MAX }.abi_encode(),
            U256::ZERO,
        );
    }
    let weth = token(WETH, 18);
    let usdc = token(USDC, 6);

    // (zero_for_one, amount in, seconds since the previous swap). The pool's liquidity sits in a
    // narrow range, so amounts stay small enough not to cross out of it; the first swap sells
    // WETH, which funds the USDC side.
    let milli_eth = 10u128.pow(15);
    let schedule: [(bool, u128, u64); 10] = [
        (true, 2 * milli_eth, 12),
        (false, 3_000_000, 600),
        (true, 3 * milli_eth, 1_800),
        (false, 5_000_000, 3_600),
        (true, milli_eth, 900),
        (true, 2 * milli_eth, 2_400),
        (false, 6_000_000, 3_600),
        (false, 1_000_000, 300),
        (true, milli_eth / 2, 5_400),
        (false, 2_000_000, 120),
    ];

    let mut fees = HashSet::new();
    for (step, (zero_for_one, amount_in, seconds)) in schedule.into_iter().enumerate() {
        chain.advance(seconds);
        let amount_in = U256::from(amount_in);
        let calldata = chain.swap_calldata(zero_for_one, amount_in);

        let state = chain.pool_state();
        chain.dry_run(SWAP_ROUTER, calldata.clone());
        chain.sync_to_shared_db();

        let (token_in, token_out) = if zero_for_one { (&weth, &usdc) } else { (&usdc, &weth) };
        let quote = state
            .get_amount_out(u256_to_biguint(amount_in), token_in, token_out)
            .unwrap_or_else(|err| panic!("step {step}: quote failed: {err:?}"));

        let (_, logs) = chain.call(SWAP_ROUTER, calldata, U256::ZERO);
        let (amount_out, fee) = swap_result(&logs, zero_for_one);

        assert_eq!(quote.amount, u256_to_biguint(amount_out), "step {step}: amount out, fee {fee}");
        let simulated = quote
            .new_state
            .as_any()
            .downcast_ref::<NamiV4State>()
            .unwrap();
        let real = chain.pool_state();
        assert_eq!(simulated.sqrt_price, real.sqrt_price, "step {step}: sqrt price");
        assert_eq!(simulated.tick, real.tick, "step {step}: tick");
        assert_eq!(simulated.liquidity, real.liquidity, "step {step}: liquidity");
        fees.insert(fee);
    }

    eprintln!(
        "live-pool differential test: {} swaps, fees seen {fees:?}, took {:.1?}",
        schedule.len(),
        started.elapsed()
    );
    assert!(!fees.contains(&0), "the plugin must set every fee, saw {fees:?}");
}
