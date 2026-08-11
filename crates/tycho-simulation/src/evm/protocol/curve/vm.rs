//! Reads Curve pool state from the locally indexed VM storage via view-getter calls and
//! assembles a vendored `math::Pool` through `adapter::build_pool`.
//!
//! The getter set per variant mirrors the reference RPC consumer in
//! `curve-adapter/tests/fuzz_registry.rs`, executed against the `SimulationEngine` instead of a
//! live RPC node so values are read from the same indexed storage Tycho already tracks.
use std::{collections::HashMap, fmt::Debug};

use alloy::{
    core::sol,
    primitives::{Address as AlloyAddress, Keccak256, U256},
    sol_types::{SolCall, SolValue},
};
use revm::{
    state::{AccountInfo, Bytecode},
    DatabaseRef,
};
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::{
        curve::{
            adapter::{build_pool, detect_eth_variant, CurveVariant, ProbingResults, RawPoolState},
            math::Pool,
        },
        vm::utils::get_code_for_contract,
    },
    simulation::{SimulationEngine, SimulationParameters},
};

sol! {
    #[allow(missing_docs)]
    interface ICurve {
        function balances(uint256 i) external view returns (uint256);
        function A() external view returns (uint256);
        function fee() external view returns (uint256);
        function initial_A() external view returns (uint256);
        function future_A() external view returns (uint256);
        function future_A_gamma_time() external view returns (uint256);
        function offpeg_fee_multiplier() external view returns (uint256);
        function gamma() external view returns (uint256);
        function D() external view returns (uint256);
        function mid_fee() external view returns (uint256);
        function out_fee() external view returns (uint256);
        function fee_gamma() external view returns (uint256);
        function price_scale() external view returns (uint256);
        function MATH() external view returns (address);
        function base_pool() external view returns (address);
        function version() external view returns (string);
    }

    #[allow(missing_docs)]
    interface ICurveOld {
        function balances(int128 i) external view returns (uint256);
    }

    #[allow(missing_docs)]
    interface ICurveTri {
        function price_scale(uint256 i) external view returns (uint256);
        function precisions() external view returns (uint256[3]);
    }

    #[allow(missing_docs)]
    interface ICurveTwo {
        function precisions() external view returns (uint256[2]);
    }

    #[allow(missing_docs)]
    interface IBasePool {
        function get_virtual_price() external view returns (uint256);
    }
}

/// `A_PRECISION` for StableSwap V2 / NG / Meta / ALend pools.
const A_PRECISION: u64 = 100;
/// `stored_rates()` selector — read via raw call because the return encoding (fixed vs dynamic
/// array) varies across NG pool versions.
const STORED_RATES_SELECTOR: [u8; 4] = [0xfd, 0x06, 0x84, 0xb1];

/// Read Curve pool state for `variant` from the engine and build the matching [`Pool`].
///
/// `token_decimals` must be ordered to match the pool's coin indices. Returns a fully
/// constructed [`Pool`] ready for quoting, or a [`SimulationError`] if a required getter
/// reverts or `build_pool` rejects the assembled state.
pub fn decode_from_vm<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    variant: CurveVariant,
    token_decimals: &[u8],
) -> Result<Pool, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let n_coins = token_decimals.len();
    let state = match variant {
        CurveVariant::StableSwapV0 => RawPoolState {
            variant,
            balances: read_balances_int128(engine, pool, n_coins)?,
            token_decimals: token_decimals.to_vec(),
            amp: call(engine, pool, ICurve::ACall {})?,
            fee: Some(call(engine, pool, ICurve::feeCall {})?),
            ..Default::default()
        },
        CurveVariant::StableSwapV1 => RawPoolState {
            variant,
            balances: read_balances(engine, pool, n_coins)?,
            token_decimals: token_decimals.to_vec(),
            amp: call(engine, pool, ICurve::ACall {})?,
            fee: Some(call(engine, pool, ICurve::feeCall {})?),
            ..Default::default()
        },
        CurveVariant::StableSwapV2 | CurveVariant::StableSwapSTETH => RawPoolState {
            variant,
            balances: read_balances(engine, pool, n_coins)?,
            token_decimals: token_decimals.to_vec(),
            amp: read_ramped_amp(engine, pool)?,
            fee: Some(call(engine, pool, ICurve::feeCall {})?),
            ..Default::default()
        },
        CurveVariant::StableSwapALend => RawPoolState {
            variant,
            balances: read_balances(engine, pool, n_coins)?,
            token_decimals: token_decimals.to_vec(),
            amp: read_ramped_amp(engine, pool)?,
            fee: Some(call(engine, pool, ICurve::feeCall {})?),
            offpeg_fee_multiplier: Some(call(engine, pool, ICurve::offpeg_fee_multiplierCall {})?),
            ..Default::default()
        },
        CurveVariant::StableSwapNG => RawPoolState {
            variant,
            balances: read_balances(engine, pool, n_coins)?,
            token_decimals: token_decimals.to_vec(),
            amp: read_ramped_amp(engine, pool)?,
            fee: Some(call(engine, pool, ICurve::feeCall {})?),
            // v5+ crvUSD factory pools lack offpeg_fee_multiplier; build_pool defaults it.
            offpeg_fee_multiplier: call_opt(engine, pool, ICurve::offpeg_fee_multiplierCall {}),
            dynamic_rates: read_stored_rates(engine, pool, n_coins),
            ..Default::default()
        },
        CurveVariant::StableSwapMeta => {
            let mut dynamic_rates = vec![None; n_coins];
            if let Some(last) = dynamic_rates.last_mut() {
                *last = Some(read_base_virtual_price(engine, pool)?);
            }
            RawPoolState {
                variant,
                balances: read_balances(engine, pool, n_coins)?,
                token_decimals: token_decimals.to_vec(),
                amp: read_ramped_amp(engine, pool)?,
                fee: Some(call(engine, pool, ICurve::feeCall {})?),
                dynamic_rates: Some(dynamic_rates),
                ..Default::default()
            }
        }
        CurveVariant::TwoCryptoV1 | CurveVariant::TwoCryptoNG | CurveVariant::TwoCryptoStable => {
            read_twocrypto(engine, pool, variant, token_decimals)?
        }
        CurveVariant::TriCryptoV1 | CurveVariant::TriCryptoNG => {
            read_tricrypto(engine, pool, variant, token_decimals)?
        }
    };

    build_pool(&state)
        .map_err(|e| SimulationError::FatalError(format!("curve build_pool failed: {e}")))
}

fn read_twocrypto<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    variant: CurveVariant,
    token_decimals: &[u8],
) -> Result<RawPoolState, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    ensure_no_active_a_gamma_ramp(engine, pool)?;
    let balances = read_balances(engine, pool, 2)?;
    let price_scale = call(engine, pool, ICurve::price_scaleCall {})?;
    let precisions = call_opt(engine, pool, ICurveTwo::precisionsCall {}).map(|p| p.to_vec());
    let gamma = if variant == CurveVariant::TwoCryptoStable {
        None
    } else {
        Some(call(engine, pool, ICurve::gammaCall {})?)
    };
    let eth_variant =
        if variant == CurveVariant::TwoCryptoV1 { Some(detect_eth_variant(*pool)) } else { None };
    Ok(RawPoolState {
        variant,
        balances,
        token_decimals: token_decimals.to_vec(),
        amp: call(engine, pool, ICurve::ACall {})?,
        mid_fee: Some(call(engine, pool, ICurve::mid_feeCall {})?),
        out_fee: Some(call(engine, pool, ICurve::out_feeCall {})?),
        fee_gamma: Some(call(engine, pool, ICurve::fee_gammaCall {})?),
        d: Some(call(engine, pool, ICurve::DCall {})?),
        gamma,
        price_scale: Some(vec![price_scale]),
        precisions,
        eth_variant,
        ..Default::default()
    })
}

fn read_tricrypto<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    variant: CurveVariant,
    token_decimals: &[u8],
) -> Result<RawPoolState, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    ensure_no_active_a_gamma_ramp(engine, pool)?;
    let balances = read_balances(engine, pool, 3)?;
    let ps0 = call(engine, pool, ICurveTri::price_scaleCall { i: U256::from(0) })?;
    let ps1 = call(engine, pool, ICurveTri::price_scaleCall { i: U256::from(1) })?;
    let precisions = call_opt(engine, pool, ICurveTri::precisionsCall {}).map(|p| p.to_vec());
    Ok(RawPoolState {
        variant,
        balances,
        token_decimals: token_decimals.to_vec(),
        amp: call(engine, pool, ICurve::ACall {})?,
        mid_fee: Some(call(engine, pool, ICurve::mid_feeCall {})?),
        out_fee: Some(call(engine, pool, ICurve::out_feeCall {})?),
        fee_gamma: Some(call(engine, pool, ICurve::fee_gammaCall {})?),
        d: Some(call(engine, pool, ICurve::DCall {})?),
        gamma: Some(call(engine, pool, ICurve::gammaCall {})?),
        price_scale: Some(vec![ps0, ps1]),
        precisions,
        ..Default::default()
    })
}

/// Refuse to read a CryptoSwap pool while its A/gamma ramp is active.
///
/// While `future_A_gamma_time > block.timestamp` the pool contract stops using its stored `D`:
/// `get_dy`/`exchange` recompute the invariant from live balances via `newton_D` on every call.
/// `A()` and `gamma()` are computed getters that interpolate to the read block, but `D()` is a
/// plain storage getter, so reading all three uniformly pairs a stale invariant with ramped
/// coefficients — a state no real pool has, which misquotes without bound. Recomputing `D` in
/// Rust (a `newton_D` next to `core::tricrypto_ng::newton_y_3`) is the eventual fix; until then
/// decoding fails for the duration of the ramp. Pools without the getter predate A/gamma ramping
/// and are read as before.
fn ensure_no_active_a_gamma_ramp<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> Result<(), SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let Some(future_a_gamma_time) = call_opt(engine, pool, ICurve::future_A_gamma_timeCall {})
    else {
        return Ok(());
    };
    let block = engine
        .state
        .get_current_block()
        .ok_or_else(|| {
            // Unreachable in practice: the getter call above only succeeds when the engine has a
            // current block to simulate against.
            SimulationError::FatalError("curve: current block not set in engine".to_string())
        })?;
    if future_a_gamma_time > U256::from(block.timestamp) {
        return Err(SimulationError::RecoverableError(format!(
            "curve pool {pool} is ramping A/gamma until timestamp {future_a_gamma_time}: its \
             stored D is not used by the contract during the ramp, so quotes would be wrong"
        )));
    }
    Ok(())
}

/// Read the amplification coefficient, preferring `initial_A` when the pool is not ramping
/// (avoids the integer-division precision loss of `A()` for `A_PRECISION=100` variants).
fn read_ramped_amp<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> Result<U256, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let initial_a = call_opt(engine, pool, ICurve::initial_ACall {});
    let future_a = call_opt(engine, pool, ICurve::future_ACall {});
    match (initial_a, future_a) {
        (Some(ia), Some(fa)) if ia == fa => Ok(ia),
        // While ramping we read `A()`, which the pool interpolates to the read block. The adapter
        // docs suggest instead interpolating `initial_A`/`future_A` and calling `Pool::set_amp`
        // per quote — but the pool has no access to the current block timestamp outside
        // `delta_transition`, so per-quote interpolation isn't feasible yet. `A()` is refreshed on
        // every `delta_transition` (i.e. on every swap), and ramps span days, so intra-interval
        // drift is negligible. Revisit if the pool gains access to the block timestamp.
        _ => Ok(call(engine, pool, ICurve::ACall {})? * U256::from(A_PRECISION)),
    }
}

fn read_balances<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    n_coins: usize,
) -> Result<Vec<U256>, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let mut balances = Vec::with_capacity(n_coins);
    for i in 0..n_coins {
        balances.push(call(engine, pool, ICurve::balancesCall { i: U256::from(i) })?);
    }
    Ok(balances)
}

fn read_balances_int128<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    n_coins: usize,
) -> Result<Vec<U256>, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let mut balances = Vec::with_capacity(n_coins);
    for i in 0..n_coins {
        balances.push(call(engine, pool, ICurveOld::balancesCall { i: i as i128 })?);
    }
    Ok(balances)
}

/// Resolve a StableSwapMeta pool's base LP token rate via the base pool's `get_virtual_price()`.
fn read_base_virtual_price<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> Result<U256, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let base_pool = call(engine, pool, ICurve::base_poolCall {})?;
    call(engine, &base_pool, IBasePool::get_virtual_priceCall {})
}

/// Read `stored_rates()` as `dynamic_rates`, handling both fixed-size and dynamic ABI encodings.
fn read_stored_rates<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    n_coins: usize,
) -> Option<Vec<Option<U256>>>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let res = engine
        .simulate(&params(*pool, STORED_RATES_SELECTOR.to_vec()))
        .ok()?;
    let out = res.result.as_ref();
    if out.len() < n_coins * 32 {
        return None;
    }
    // If the first word is a small ABI offset (dynamic encoding) rather than a rate
    // (rates are >= 10^18), skip the offset + length words.
    let first_word = U256::from_be_slice(&out[..32]);
    let data_offset =
        if first_word <= U256::from(256u64) && out.len() >= (n_coins + 2) * 32 { 64 } else { 0 };
    let rates = (0..n_coins)
        .map(|i| {
            let start = data_offset + i * 32;
            Some(U256::from_be_slice(&out[start..start + 32]))
        })
        .collect();
    Some(rates)
}

/// Read the pool's `MATH()` contract address, if it exposes one (TwoCrypto-NG era pools).
pub fn read_math_address<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> Option<AlloyAddress>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    call_opt(engine, pool, ICurve::MATHCall {})
}

/// Ensure the code of the pool's actual `MATH()` contract is loaded into the engine.
///
/// TwoCrypto-NG pools delegate math to a `MATH()` contract, and the substreams may index a
/// stale/hardcoded math address (a different version than the pool actually uses), so the real one
/// read from `MATH()` is loaded here by fetching its code via RPC. A no-op for pools without a
/// `MATH()` getter. Returns an error when a pool exposes `MATH()` but its code cannot be fetched or
/// loaded — the caller must fail decoding rather than build a pool with unresolved math.
pub async fn load_math_contract<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> Result<(), SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let Some(math) = read_math_address(engine, pool) else {
        return Ok(());
    };
    let code = get_code_for_contract(&math.to_string(), None)
        .await
        .map_err(|e| {
            SimulationError::RecoverableError(format!(
                "curve: failed to fetch MATH() code for {math}: {e}"
            ))
        })?;
    engine
        .state
        .init_account(
            math,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: code.hash_slow(),
                code: Some(code),
            },
            None,
            false,
        )
        .map_err(|e| {
            SimulationError::FatalError(format!(
                "curve: failed to load MATH() code for {math}: {e:?}"
            ))
        })?;
    Ok(())
}

/// Read the `version()` string from a contract (e.g. a TwoCrypto MATH implementation), if present.
pub fn read_version<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    address: &AlloyAddress,
) -> Option<String>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    call_opt(engine, address, ICurve::versionCall {})
}

/// Probe the pool's on-chain interface to populate [`ProbingResults`] for variant detection.
///
/// Each field records whether the corresponding getter succeeded; only used as a fallback when
/// the variant cannot be resolved from static attributes.
pub fn probe<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    n_coins: usize,
) -> ProbingResults
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    ProbingResults {
        has_gamma: call_opt(engine, pool, ICurve::gammaCall {}).is_some(),
        n_coins,
        has_math: read_math_address(engine, pool).is_some(),
        // Read `MATH().version()` so `detect_variant` can split TwoCrypto NG (v2.x) from
        // TwoCryptoStable (v0.x) on the probe fallback path, matching `resolve_twocrypto`.
        math_version: read_math_address(engine, pool).and_then(|math| read_version(engine, &math)),
        has_offpeg_fee_multiplier: call_opt(engine, pool, ICurve::offpeg_fee_multiplierCall {})
            .is_some(),
        has_stored_rates: read_stored_rates(engine, pool, n_coins).is_some(),
        has_version: call_opt(engine, pool, ICurve::versionCall {}).is_some(),
        has_base_pool: call_opt(engine, pool, ICurve::base_poolCall {}).is_some(),
        has_int128_balances: call_opt(engine, pool, ICurveOld::balancesCall { i: 0 }).is_some(),
        pool_address: *pool,
    }
}

/// Load the pool's stateless/implementation contracts (the `stateless_contract_addr_{i}` state
/// attributes) into the engine's DB so getter calls on proxy pools resolve their delegatecall
/// targets. Curve pools are commonly EIP-1167 proxies whose implementation code is not part of the
/// indexed pool storage; without this, getters delegatecall into empty code and revert.
///
/// Addresses may be static (`0x…`) or dynamic (`call:0x<factory>:method()`), and code is fetched
/// via RPC unless provided inline as `stateless_contract_code_{i}`. The engine shares its DB with
/// `SHARED_TYCHO_DB`, so accounts loaded here persist for later `delta_transition` rebuilds.
pub async fn load_stateless_contracts<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    attributes: &HashMap<String, Bytes>,
) -> Result<(), SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let mut index = 0;
    while let Some(encoded) = attributes.get(&format!("stateless_contract_addr_{index}")) {
        let address = String::from_utf8(encoded.to_vec()).map_err(|e| {
            SimulationError::FatalError(format!("curve stateless address not UTF-8: {e}"))
        })?;
        let inline_code = attributes
            .get(&format!("stateless_contract_code_{index}"))
            .map(|value| value.to_vec());
        index += 1;

        let (account, code) = match inline_code {
            Some(bytecode) => (address, Bytecode::new_raw(bytecode.into())),
            None => {
                let resolved = if address.starts_with("call") {
                    resolve_call_address(engine, &address)?
                } else {
                    address
                };
                let code = get_code_for_contract(&resolved, None).await?;
                (resolved, code)
            }
        };
        let account: AlloyAddress = account.parse().map_err(|_| {
            SimulationError::FatalError(format!("curve stateless: invalid address {account}"))
        })?;
        engine
            .state
            .init_account(
                account,
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 0,
                    code_hash: code.hash_slow(),
                    code: Some(code),
                },
                None,
                false,
            )
            .map_err(|e| {
                SimulationError::FatalError(format!("curve stateless init_account failed: {e:?}"))
            })?;
    }
    Ok(())
}

/// Resolve a dynamic `call:0x<address>:method()` directive to a concrete implementation address by
/// simulating the parameterless `method()` view and decoding its returned address.
fn resolve_call_address<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    directive: &str,
) -> Result<String, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let method = directive
        .split(':')
        .next_back()
        .ok_or_else(|| {
            SimulationError::FatalError(format!(
                "curve stateless: malformed call directive {directive}"
            ))
        })?;
    let to: AlloyAddress = directive
        .split(':')
        .nth(1)
        .ok_or_else(|| {
            SimulationError::FatalError(format!(
                "curve stateless: missing target in call directive {directive}"
            ))
        })?
        .parse()
        .map_err(|_| {
            SimulationError::FatalError(format!(
                "curve stateless: invalid target in call directive {directive}"
            ))
        })?;
    let mut hasher = Keccak256::new();
    hasher.update(method.as_bytes());
    let selector = hasher.finalize()[..4].to_vec();
    let res = engine
        .simulate(&params(to, selector))
        .map_err(|e| SimulationError::FatalError(format!("curve stateless call failed: {e}")))?;
    let address = AlloyAddress::abi_decode(res.result.as_ref()).map_err(|e| {
        SimulationError::FatalError(format!("curve stateless call decode failed: {e}"))
    })?;
    Ok(address.to_string())
}

fn params(to: AlloyAddress, data: Vec<u8>) -> SimulationParameters {
    SimulationParameters {
        caller: AlloyAddress::ZERO,
        to,
        data,
        value: U256::ZERO,
        overrides: None,
        gas_limit: None,
        transient_storage: None,
        block_overrides: None,
    }
}

fn call<D, C, R>(
    engine: &SimulationEngine<D>,
    to: &AlloyAddress,
    sol_call: C,
) -> Result<R, SimulationError>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
    C: SolCall<Return = R>,
{
    let res = engine
        .simulate(&params(*to, sol_call.abi_encode()))
        .map_err(|e| SimulationError::RecoverableError(format!("curve getter call failed: {e}")))?;
    C::abi_decode_returns(res.result.as_ref())
        .map_err(|e| SimulationError::FatalError(format!("curve getter decode failed: {e}")))
}

fn call_opt<D, C, R>(engine: &SimulationEngine<D>, to: &AlloyAddress, sol_call: C) -> Option<R>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
    C: SolCall<Return = R>,
{
    call::<D, C, R>(engine, to, sol_call).ok()
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use alloy::sol;
    use tycho_client::feed::BlockHeader;

    use super::*;
    use crate::evm::{
        engine_db::{
            simulation_db::SimulationDB,
            utils::{get_client, get_runtime},
        },
        simulation::SimulationEngine,
    };

    sol! {
        function get_dy_stable(int128 i, int128 j, uint256 dx) external view returns (uint256);
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
        function coins(uint256 i) external view returns (address);
        function decimals() external view returns (uint8);
    }

    const ETH_PLACEHOLDER: AlloyAddress =
        alloy::primitives::address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

    /// Read coin decimals on-chain so the differential check is robust to any pool's token set.
    fn read_decimals<D: EngineDatabaseInterface + Clone + Debug>(
        engine: &SimulationEngine<D>,
        pool: &AlloyAddress,
        n: usize,
    ) -> Vec<u8>
    where
        <D as DatabaseRef>::Error: Debug,
        <D as EngineDatabaseInterface>::Error: Debug,
    {
        (0..n)
            .map(|i| {
                let coin: AlloyAddress =
                    call(engine, pool, coinsCall { i: U256::from(i) }).unwrap();
                if coin == ETH_PLACEHOLDER {
                    18
                } else {
                    call(engine, &coin, decimalsCall {}).unwrap()
                }
            })
            .collect()
    }

    /// Decode a pool from the VM and assert our `get_amount_out` matches on-chain `get_dy`
    /// (StableSwap `int128` signature) at the same block.
    fn assert_stable_matches_onchain(
        pool: &str,
        variant: CurveVariant,
        n_coins: usize,
        swap: (usize, usize, U256),
        block: (u64, u64),
    ) {
        let (i, j, dx) = swap;
        let (block_number, timestamp) = block;
        let header = BlockHeader { number: block_number, timestamp, ..Default::default() };
        let mut db = SimulationDB::new(get_client(None).unwrap(), get_runtime().unwrap(), None);
        db.set_block(Some(header));
        let engine = SimulationEngine::new(db, false);
        let pool = AlloyAddress::from_str(pool).unwrap();
        let decimals = read_decimals(&engine, &pool, n_coins);

        let decoded = decode_from_vm(&engine, &pool, variant, &decimals).expect("decode failed");
        let ours = decoded
            .get_amount_out(i, j, dx)
            .expect("get_amount_out returned None");

        let onchain: U256 =
            call(&engine, &pool, get_dy_stableCall { i: i as i128, j: j as i128, dx })
                .expect("on-chain get_dy failed");

        assert_eq!(ours, onchain, "curve quote diverged from on-chain get_dy");
    }

    /// Pure check (no RPC): assemble `RawPoolState` from on-chain getter values for the
    /// TriCryptoNG USDC/WBTC/WETH pool (0x7f86bf…) and verify `build_pool().get_amount_out`
    /// reproduces on-chain `get_dy`. Isolates field-mapping + curve-math from the VM engine.
    #[test]
    fn tricrypto_ng_build_pool_matches_onchain_get_dy() {
        let u = |s: &str| s.parse::<U256>().unwrap();
        let state = RawPoolState {
            variant: CurveVariant::TriCryptoNG,
            balances: vec![u("2466241139205"), u("4200057336"), u("1595469030050811720465")],
            token_decimals: vec![6, 8, 18],
            amp: u("1707629"),
            mid_fee: Some(u("3000000")),
            out_fee: Some(u("30000000")),
            fee_gamma: Some(u("500000000000000")),
            d: Some(u("7457948167729606869978625")),
            gamma: Some(u("11809167828997")),
            price_scale: Some(vec![u("59372627314351316239076"), u("1565715369034455123313")]),
            ..Default::default()
        };
        let pool = build_pool(&state).expect("build_pool failed");
        let dx = U256::from(1_000_000_000u64); // 1000 USDC
        assert_eq!(pool.get_amount_out(0, 1, dx), Some(u("1690920")), "USDC->WBTC");
        assert_eq!(pool.get_amount_out(0, 2, dx), Some(u("641654961086650131")), "USDC->WETH");
    }

    /// Pure check (no RPC): assemble `RawPoolState` from on-chain getter values for the legacy
    /// WETH-paired CRV/ETH TwoCryptoV1 pool (0x8301AE4f…) at block 25_401_368 and verify
    /// `build_pool().get_amount_out` reproduces on-chain `get_dy` wei-for-wei in both directions
    /// for several dx. This pool is WETH-paired → ETH solver flavour (`eth_variant = true`). The
    /// getters mirror `read_twocrypto`; `precisions()` reverts on this legacy pool, so precisions
    /// fall back to `10^(18-decimals) = [1, 1]` (both coins 18-dec).
    #[test]
    fn twocrypto_v1_eth_variant_build_pool_matches_onchain_get_dy() {
        let u = |s: &str| s.parse::<U256>().unwrap();
        let make = |eth_variant: bool| {
            build_pool(&RawPoolState {
                variant: CurveVariant::TwoCryptoV1,
                balances: vec![u("33389428640940997105"), u("1538654846140514380725767708")],
                token_decimals: vec![18, 18], // coin0 WETH, coin1 CRV
                amp: u("400000"),
                gamma: Some(u("145000000000000")),
                d: Some(u("3338917956508624293928")),
                price_scale: Some(vec![u("52805053500476")]),
                mid_fee: Some(U256::from(26_000_000u64)),
                out_fee: Some(U256::from(45_000_000u64)),
                fee_gamma: Some(u("230000000000000")),
                eth_variant: Some(eth_variant),
                ..Default::default()
            })
            .expect("build_pool")
        };
        let pool = make(true);
        // WETH(0) -> CRV(1)
        assert_eq!(
            pool.get_amount_out(0, 1, u("1000000000000000000")),
            Some(u("44131555012248406155621024")),
            "1 WETH -> CRV",
        );
        assert_eq!(
            pool.get_amount_out(0, 1, u("500000000000000000")),
            Some(u("22389351263618692591189738")),
            "0.5 WETH -> CRV",
        );
        // CRV(1) -> WETH(0)
        assert_eq!(
            pool.get_amount_out(1, 0, u("1000000000000000000000")),
            Some(u("21806963109202")),
            "1000 CRV -> WETH",
        );
        assert_eq!(
            pool.get_amount_out(1, 0, u("10000000000000000000000")),
            Some(u("218068351371449")),
            "10000 CRV -> WETH",
        );
    }

    /// A `twocrypto_factory` pool with MATH v0.1.1 is TwoCryptoStable (StableSwap math, gamma
    /// ignored), not TwoCryptoNG. Confirms which variant reproduces on-chain `get_dy`.
    #[test]
    fn twocrypto_v011_is_stable_not_ng() {
        let u = |s: &str| s.parse::<U256>().unwrap();
        let base = |variant: CurveVariant, gamma: Option<U256>| {
            build_pool(&RawPoolState {
                variant,
                balances: vec![u("250289528581622891700521"), u("179139571297")],
                token_decimals: vec![18, 6],
                amp: u("350000"),
                mid_fee: Some(u("1000000")),
                out_fee: Some(u("20000000")),
                fee_gamma: Some(u("63100000000000000")),
                d: Some(u("500000118136487176847089")),
                gamma,
                price_scale: Some(vec![u("1393944381226980604")]),
                precisions: Some(vec![u("1"), u("1000000000000")]),
                ..Default::default()
            })
            .expect("build_pool")
        };
        let dx = u("1000000000000000000"); // 1 coin0
        let stable = base(CurveVariant::TwoCryptoStable, None).get_amount_out(0, 1, dx);
        let ng =
            base(CurveVariant::TwoCryptoNG, Some(u("100000000000000"))).get_amount_out(0, 1, dx);
        eprintln!("on-chain get_dy=717271  stable={stable:?}  ng={ng:?}");
        assert_eq!(stable, Some(u("717271")), "TwoCryptoStable matches on-chain get_dy");
        assert_ne!(ng, Some(u("717271")), "TwoCryptoNG does NOT match (current misclassification)");
    }

    /// Decimals sensitivity: coin 2 is native ETH (zero address in the component). If the live env
    /// feeds wrong decimals for it, `precisions[2]` is corrupted, poisoning the invariant and all
    /// pairs. Reproduces the garbage quote pattern seen in the integration test.
    #[test]
    fn tricrypto_ng_wrong_coin2_decimals_breaks_quotes() {
        let u = |s: &str| s.parse::<U256>().unwrap();
        let make = |decimals: Vec<u8>| {
            build_pool(&RawPoolState {
                variant: CurveVariant::TriCryptoNG,
                balances: vec![u("2466241139205"), u("4200057336"), u("1595469030050811720465")],
                token_decimals: decimals,
                amp: u("1707629"),
                mid_fee: Some(u("3000000")),
                out_fee: Some(u("30000000")),
                fee_gamma: Some(u("500000000000000")),
                d: Some(u("7457948167729606869978625")),
                gamma: Some(u("11809167828997")),
                price_scale: Some(vec![u("59372627314351316239076"), u("1565715369034455123313")]),
                ..Default::default()
            })
            .expect("build_pool")
        };
        let dx_usdc = U256::from(1_000_000_000u64); // 1000 USDC
        let dx_wbtc = U256::from(13_656_795u64); // ~0.137 WBTC
        for dec2 in [18u8, 0, 6] {
            let p = make(vec![6, 8, dec2]);
            eprintln!(
                "decimals=[6,8,{dec2}]  USDC->WETH(0,2)={:?}  WBTC->USDC(1,0)={:?}",
                p.get_amount_out(0, 2, dx_usdc),
                p.get_amount_out(1, 0, dx_wbtc),
            );
        }
        // Correct decimals give a sane ~0.6 ETH out; wrong decimals corrupt the invariant and
        // blow the quote up by orders of magnitude (here ~1594 ETH for the same 1000 USDC).
        let correct_out = make(vec![6, 8, 18])
            .get_amount_out(0, 2, dx_usdc)
            .unwrap();
        let wrong_out = make(vec![6, 8, 0])
            .get_amount_out(0, 2, dx_usdc)
            .unwrap();
        assert!(correct_out > U256::from(10).pow(U256::from(17)), "correct ~0.6 ETH");
        assert!(wrong_out > correct_out * U256::from(100u64), "wrong decimals corrupt the quote");
    }

    /// While a CryptoSwap pool's A/gamma ramp is active the contract ignores its stored `D`
    /// (it recomputes the invariant from balances on every call), so decoding must refuse the
    /// pool; once the ramp is over the same pool must decode and match on-chain `get_dy`.
    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn differential_tricrypto_active_a_gamma_ramp_is_refused() {
        // TricryptoUSDT (USDT/WBTC/WETH), `future_A_gamma_time` = 1_786_178_519.
        let pool = AlloyAddress::from_str("0xf5f5b97624542d72a9e06f04804bf81baa15e2b4").unwrap();
        let decode_at = |number: u64, timestamp: u64| {
            let header = BlockHeader { number, timestamp, ..Default::default() };
            let mut db = SimulationDB::new(get_client(None).unwrap(), get_runtime().unwrap(), None);
            db.set_block(Some(header));
            let engine = SimulationEngine::new(db, false);
            let decimals = read_decimals(&engine, &pool, 3);
            (decode_from_vm(&engine, &pool, CurveVariant::TriCryptoNG, &decimals), engine)
        };

        // Mid-ramp block (ts 1_786_171_667 < ramp end): decoding must fail naming the ramp.
        let (ramping, _) = decode_at(25_708_548, 1_786_171_667);
        let error = ramping.expect_err("decode must fail while the A/gamma ramp is active");
        assert!(
            error
                .to_string()
                .contains("ramping A/gamma"),
            "unexpected error: {error}"
        );

        // Post-ramp block (ts 1_786_309_559 > ramp end): decode succeeds and matches `get_dy`.
        let (decoded, engine) = decode_at(25_720_000, 1_786_309_559);
        let decoded = decoded.expect("decode must succeed once the ramp is over");
        let dx = U256::from(1_982_505u64); // 1.982505 USDT
        let ours = decoded
            .get_amount_out(0, 1, dx)
            .expect("get_amount_out returned None");
        let onchain: U256 =
            call(&engine, &pool, get_dyCall { i: U256::ZERO, j: U256::from(1), dx })
                .expect("on-chain get_dy failed");
        assert_eq!(ours, onchain, "curve quote diverged from on-chain get_dy");
    }

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn differential_3pool_stableswap_v1() {
        // 3pool DAI(0)->USDC(1), 1 DAI.
        assert_stable_matches_onchain(
            "0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7",
            CurveVariant::StableSwapV1,
            3,
            (0, 1, U256::from(1_000_000_000_000_000_000u128)),
            (21_500_000, 1_736_000_000),
        );
    }

    #[test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    fn differential_stableswap_ng_plain() {
        // crypto_swap_ng_factory plain pool, coin 0 -> coin 1.
        assert_stable_matches_onchain(
            "0xf55b0f6f2da5ffddb104b58a60f2862745960442",
            CurveVariant::StableSwapNG,
            2,
            (0, 1, U256::from(1_000_000_000_000_000_000u128)),
            (21_500_000, 1_736_000_000),
        );
    }
}
