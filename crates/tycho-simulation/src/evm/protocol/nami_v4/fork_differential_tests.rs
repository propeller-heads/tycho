//! Fork variant of the differential test: `NamiV4State` quotes against Nami's live Base WETH/USDC
//! pool and its production plugin, forked over RPC.
//!
//! Every swap runs through Nami's deployed `SwapRouter` at simulated blocks after the fork block,
//! spread over several hours so the plugin's volatility oracle moves the fee. Before each swap the
//! test dry-runs it once, which loads every account and storage slot the swap reads (the plugin's
//! oracle included) from the RPC into the local cache, then loads that cache into
//! `SHARED_TYCHO_DB`, quotes with `NamiV4State`, executes the swap and requires the quote and the
//! post-swap pool state to match it exactly. Each swap is also quoted from the state the previous
//! quote returned, in the same block or the next, and the quoted gas is held against the gas the
//! pool's `swap` frame spent.
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
    interpreter::{CallInputs, CallOutcome},
    primitives::{hardfork::SpecId, TxKind},
    state::AccountInfo,
    Database, ExecuteEvm, InspectCommitEvm, Inspector, MainBuilder, MainContext,
};
use tycho_client::feed::BlockHeader;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::{
        errors::SimulationError,
        protocol_sim::{BlockContext, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use super::NamiV4State;
use crate::evm::{
    engine_db::SHARED_TYCHO_DB,
    protocol::{
        nami_v4::swap_math::get_token0_delta,
        u256_num::u256_to_biguint,
        utils::uniswap::tick_math::{get_sqrt_ratio_at_tick, MAX_TICK, MIN_TICK},
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

/// Gas the pool's `swap` frame spent in a transaction, less the callback it made into the swap
/// router, which pays the input and is the caller's cost rather than the pool's.
#[derive(Default)]
struct PoolSwapGas {
    swap_frame: u64,
    callback: u64,
}

impl<CTX> Inspector<CTX> for PoolSwapGas {
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let spent = outcome.result.gas.spent();
        // `swap` is the pool's largest frame in the transaction; its other frames are reads.
        if inputs.target_address == POOL {
            self.swap_frame = self.swap_frame.max(spent);
        }
        if inputs.caller == POOL && inputs.target_address == SWAP_ROUTER {
            self.callback = spent;
        }
    }
}

/// A committed call: its output, its logs and the gas the pool's `swap` spent on its own account.
struct Executed {
    logs: Vec<Log>,
    pool_swap_gas: u64,
}

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
    fn call(&mut self, to: Address, data: Vec<u8>, value: U256) -> Executed {
        let tx = self.tx(to, data, value);
        let mut gas = PoolSwapGas::default();
        let result = {
            let mut evm = Context::mainnet()
                .with_db(&mut self.db)
                .with_cfg(Self::cfg())
                .with_block(self.block.clone())
                .build_mainnet_with_inspector(&mut gas);
            evm.inspect_tx_commit(tx)
                .expect("EVM execution error")
        };
        match result {
            ExecutionResult::Success { output: Output::Call(_), logs, .. } => Executed {
                logs,
                pool_swap_gas: gas
                    .swap_frame
                    .saturating_sub(gas.callback),
            },
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

    fn block_context(&self) -> BlockContext {
        BlockContext::new(self.block.number.to::<u64>(), self.block.timestamp.to::<u64>())
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
            self.block_context(),
        )
    }
}

/// Quotes `state` for `amount_in` and fails the test on an error.
fn quote(
    state: &dyn ProtocolSim,
    amount_in: U256,
    token_in: &Token,
    token_out: &Token,
    what: &str,
) -> GetAmountOutResult {
    state
        .get_amount_out(u256_to_biguint(amount_in), token_in, token_out)
        .unwrap_or_else(|err| panic!("{what}: quote failed: {err:?}"))
}

/// Requires the quoted output and post-swap pool state to match the chain's.
fn assert_quote_matches(
    quote: &GetAmountOutResult,
    amount_out: U256,
    real: &NamiV4State,
    what: &str,
) {
    assert_eq!(quote.amount, u256_to_biguint(amount_out), "{what}: amount out");
    let simulated = quote
        .new_state
        .as_any()
        .downcast_ref::<NamiV4State>()
        .unwrap();
    assert_eq!(simulated.sqrt_price, real.sqrt_price, "{what}: sqrt price");
    assert_eq!(simulated.tick, real.tick, "{what}: tick");
    assert_eq!(simulated.liquidity, real.liquidity, "{what}: liquidity");
}

/// Requires the quoted gas to sit within the band around what the pool's `swap` frame spent, and
/// returns their ratio. A swap that follows another in the same block finds warm storage, so it
/// spends less than the model, which prices a cold swap.
fn assert_gas_quoted(quote: &GetAmountOutResult, measured: u64, what: &str) -> f64 {
    let quoted: u64 = quote.gas.to_string().parse().unwrap();
    let ratio = quoted as f64 / measured as f64;
    assert!(
        (0.85..=1.25).contains(&ratio),
        "{what}: quoted {quoted} gas against {measured} spent by the pool's swap"
    );
    ratio
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
fn test_quotes_match_the_live_base_pool() {
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

    // (zero_for_one, amount in, seconds since the previous swap; zero keeps the same block). The
    // pool's liquidity sits in a narrow range, so amounts stay small enough not to cross out of
    // it; the first swap sells WETH, which funds the USDC side.
    let milli_eth = 10u128.pow(15);
    let schedule: [(bool, u128, u64); 12] = [
        (true, 2 * milli_eth, 12),
        (false, 1_000_000, 0),
        (false, 3_000_000, 600),
        (true, 3 * milli_eth, 1_800),
        (true, milli_eth, 0),
        (false, 5_000_000, 3_600),
        (true, milli_eth, 900),
        (true, 2 * milli_eth, 2_400),
        (false, 6_000_000, 3_600),
        (false, 1_000_000, 300),
        (true, milli_eth / 2, 5_400),
        (false, 2_000_000, 120),
    ];

    let mut fees = HashSet::new();
    let mut gas_ratios = Vec::new();
    let mut chained: Option<Box<dyn ProtocolSim>> = None;
    for (step, (zero_for_one, amount_in, seconds)) in schedule.into_iter().enumerate() {
        let same_block = seconds == 0;
        if !same_block {
            chain.advance(seconds);
        }
        let amount_in = U256::from(amount_in);
        let calldata = chain.swap_calldata(zero_for_one, amount_in);
        let (token_in, token_out) = if zero_for_one { (&weth, &usdc) } else { (&usdc, &weth) };
        let what = format!("step {step}");

        // A quote chained from the previous quote's state. In the same block it runs before the
        // previous swap's writes reach the shared DB, so the plugin storage the quote carried
        // must stand in for them. In a new block it reads them like any other quote.
        let mut chained_quote = None;
        if same_block {
            chained_quote = chained.as_deref().map(|state| {
                quote(state, amount_in, token_in, token_out, &format!("{what} chained"))
            });
        }
        let state = chain.pool_state();
        chain.dry_run(SWAP_ROUTER, calldata.clone());
        chain.sync_to_shared_db();
        if !same_block {
            if let Some(state) = chained.as_deref_mut() {
                state.apply_block(&chain.block_context());
                chained_quote =
                    Some(quote(state, amount_in, token_in, token_out, &format!("{what} chained")));
            }
        }
        let fresh_quote = quote(&state, amount_in, token_in, token_out, &what);

        let executed = chain.call(SWAP_ROUTER, calldata, U256::ZERO);
        let (amount_out, fee) = swap_result(&executed.logs, zero_for_one);
        let real = chain.pool_state();

        assert_quote_matches(&fresh_quote, amount_out, &real, &format!("{what}, fee {fee}"));
        if let Some(chained_quote) = &chained_quote {
            assert_quote_matches(chained_quote, amount_out, &real, &format!("{what} chained"));
        }
        gas_ratios.push(assert_gas_quoted(&fresh_quote, executed.pool_swap_gas, &what));
        chained = Some(chained_quote.map_or(fresh_quote.new_state, |chained| chained.new_state));
        fees.insert(fee);
    }

    // One more swap, funded well past the first initialized tick below the price, so it crosses
    // it. Past the pool's last tick it walks empty ranges to the price limit and leaves input
    // unswapped, which the quote reports as a partial result.
    chain.advance(60);
    let state = chain.pool_state();
    let tick_below = state.prev_initialized_tick(state.tick);
    let max_funding = U256::from(100u128 * 10u128.pow(18));
    let amount_to_tick = get_token0_delta(
        get_sqrt_ratio_at_tick(tick_below).unwrap(),
        state.sqrt_price,
        state.liquidity,
        true,
    )
    .unwrap();
    let amount_in = amount_to_tick * U256::from(111) / U256::from(100);
    if tick_below > MIN_TICK && amount_in <= max_funding {
        chain.call(WETH, IWeth::depositCall {}.abi_encode(), amount_in);
        let calldata = chain.swap_calldata(true, amount_in);
        chain.dry_run(SWAP_ROUTER, calldata.clone());
        chain.sync_to_shared_db();
        let crossing = match state.get_amount_out(u256_to_biguint(amount_in), &weth, &usdc) {
            Ok(quote) => quote,
            Err(SimulationError::InvalidInput(_, Some(partial))) => partial,
            Err(err) => panic!("tick crossing: quote failed: {err:?}"),
        };

        let executed = chain.call(SWAP_ROUTER, calldata, U256::ZERO);
        let (amount_out, fee) = swap_result(&executed.logs, true);
        let real = chain.pool_state();

        assert!(real.tick < tick_below, "the crossing swap must pass tick {tick_below}");
        assert_quote_matches(&crossing, amount_out, &real, &format!("tick crossing, fee {fee}"));
        gas_ratios.push(assert_gas_quoted(&crossing, executed.pool_swap_gas, "tick crossing"));
        fees.insert(fee);
    } else {
        eprintln!(
            "skipping the tick crossing swap: tick below {tick_below}, {amount_in} wei of WETH \
             needed"
        );
    }

    eprintln!(
        "live-pool differential test: {} swaps, fees seen {fees:?}, quoted/spent gas {:?}, took \
         {:.1?}",
        gas_ratios.len(),
        gas_ratios
            .iter()
            .map(|ratio| format!("{ratio:.2}"))
            .collect::<Vec<_>>(),
        started.elapsed()
    );
    assert!(!fees.contains(&0), "the plugin must set every fee, saw {fees:?}");
    assert!(fees.len() > 1, "the plugin's fee must move over the schedule, saw {fees:?}");
}
