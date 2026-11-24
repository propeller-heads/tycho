use std::{any::Any, collections::HashMap, fmt::Debug, str::FromStr, sync::Arc};

use alloy::{
    primitives::{keccak256, Address, Signed, Uint, B256, I128, I256, U256},
    sol_types::SolType,
};
use revm::{
    state::{AccountInfo, Bytecode},
    DatabaseRef,
};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::Balances,
    },
    Bytes,
};

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::{
        uniswap_v4::{
            hooks::{
                hook_handler::HookHandler,
                models::{
                    AfterSwapDelta, AfterSwapParameters, AfterSwapSolReturn, AmountRanges,
                    BeforeSwapOutput, BeforeSwapParameters, BeforeSwapSolOutput,
                    GetLimitsSolReturn, SwapFeeAccrual, SwapParams, WithGasEstimate,
                },
            },
            state::UniswapV4State,
        },
        vm::{
            erc20_token::TokenProxyOverwriteFactory,
            tycho_simulation_contract::TychoSimulationContract,
        },
    },
    simulation::{PendingOverrides, SimulationEngine},
};

const EULER_LENS_BYTECODE_BYTES: &[u8] = include_bytes!("assets/EulerLimitsLens.evm.runtime");

#[derive(Debug, Clone)]
pub(crate) struct GenericVMHookHandler<D: EngineDatabaseInterface + Clone + Debug>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    contract: TychoSimulationContract<D>,
    address: Address,
    pool_manager: Address,
    limits_entrypoint: Option<String>,
    is_euler: bool,
    /// `None` for confirmed state.
    pending_overrides: Option<Arc<PendingOverrides>>,
}

impl<D: EngineDatabaseInterface + Clone + Debug> PartialEq for GenericVMHookHandler<D>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address &&
            self.pool_manager == other.pool_manager &&
            self.limits_entrypoint == other.limits_entrypoint
    }
}

impl<D: EngineDatabaseInterface + Clone + Debug> GenericVMHookHandler<D>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    pub fn new(
        address: Address,
        engine: SimulationEngine<D>,
        pool_manager: Address,
        _all_tokens: HashMap<Bytes, Token>,
        _account_balances: HashMap<Bytes, HashMap<Bytes, Bytes>>,
        limits_entrypoint: Option<String>,
        is_euler: bool,
    ) -> Result<Self, SimulationError> {
        Ok(GenericVMHookHandler {
            contract: TychoSimulationContract::new(address, engine)?,
            address,
            pool_manager,
            limits_entrypoint,
            is_euler,
            pending_overrides: None,
        })
    }

    pub fn unlock_pool_manager(&self) -> HashMap<Address, HashMap<U256, U256>> {
        let is_unlocked_slot = U256::from_be_bytes(keccak256("Unlocked").0) - U256::from(1);
        HashMap::from([(self.pool_manager, HashMap::from([(is_unlocked_slot, U256::from(1u64))]))])
        // the slot is here https://github.com/Uniswap/v4-core/blob/main/src/libraries/Lock.sol#L8C5-L8C117
    }

    /// Writes the pool state `Pool.swap` stores before the PoolManager calls `afterSwap` into the
    /// PoolManager storage the call runs on: the price and tick, the liquidity and, with
    /// [`PoolAfterSwap::fees`](super::models::PoolAfterSwap::fees), the fee accounting. A hook that
    /// reads the pool there then sees the swap's result, and the pool stays consistent for a
    /// hook that modifies a position in `afterSwap`. Bit layout of the first slot: v4-core
    /// `Slot0`.
    ///
    /// The native swap starts from the indexed pool, not from changes that `beforeSwap` made to
    /// it. So the price and liquidity written here replace any that `beforeSwap` wrote, and
    /// `afterSwap` sees the pool that the quote comes from.
    fn store_pool_state_after_swap(
        &self,
        overwrites: &mut HashMap<Address, HashMap<U256, U256>>,
        pool_id: B256,
        params: &AfterSwapParameters,
    ) -> Result<(), SimulationError> {
        let pool = &params.pool_after_swap;
        let state_slot = mapping_slot(pool_id, POOLS_SLOT);
        let storage = overwrites
            .entry(self.pool_manager)
            .or_default();
        // Keep the protocol and LP fees (bits 184..232); replace the tick and the price below them.
        let fees = self.stored_value(storage, state_slot)? >> 184 << 184;
        let tick = U256::from(pool.tick.cast_unsigned() & 0xFF_FFFF) << 160;
        storage.insert(state_slot, fees | tick | pool.sqrt_price);
        storage.insert(state_slot + LIQUIDITY_OFFSET, U256::from(pool.liquidity));
        if let Some(accrual) = &pool.fees {
            let zero_for_one = params.swap_params.zero_for_one;
            let input =
                if zero_for_one { params.context.currency_0 } else { params.context.currency_1 };
            self.store_swap_fees(storage, state_slot, zero_for_one, input, accrual)?;
        }
        Ok(())
    }

    /// Writes a swap's fee accrual as `Pool.swap` and `PoolManager._swap` do: the input token's
    /// fee growth, every crossed tick's fee growth outside flipped against the growth at its
    /// crossing, and the protocol's fees on the input token (`protocolFeesAccrued`).
    fn store_swap_fees(
        &self,
        storage: &mut HashMap<U256, U256>,
        state_slot: U256,
        zero_for_one: bool,
        input: Address,
        accrual: &SwapFeeAccrual,
    ) -> Result<(), SimulationError> {
        let (input_growth_slot, other_growth_slot) = if zero_for_one {
            (state_slot + FEE_GROWTH_GLOBAL0_OFFSET, state_slot + FEE_GROWTH_GLOBAL1_OFFSET)
        } else {
            (state_slot + FEE_GROWTH_GLOBAL1_OFFSET, state_slot + FEE_GROWTH_GLOBAL0_OFFSET)
        };
        let input_growth = self.stored_value(storage, input_growth_slot)?;
        let other_growth = self.stored_value(storage, other_growth_slot)?;
        for (tick, growth_at_cross) in &accrual.crossed_ticks {
            let input_global = input_growth.wrapping_add(*growth_at_cross);
            let globals = if zero_for_one {
                [input_global, other_growth]
            } else {
                [other_growth, input_global]
            };
            let tick = I256::try_from(*tick).map_err(|e| {
                SimulationError::FatalError(format!("Failed to convert tick {tick}: {e:?}"))
            })?;
            let tick_slot = mapping_slot(tick, state_slot + TICKS_OFFSET);
            for (offset, global) in [
                (TICK_FEE_GROWTH_OUTSIDE0_OFFSET, globals[0]),
                (TICK_FEE_GROWTH_OUTSIDE1_OFFSET, globals[1]),
            ] {
                let slot = tick_slot + offset;
                let outside = self.stored_value(storage, slot)?;
                storage.insert(slot, global.wrapping_sub(outside));
            }
        }
        storage.insert(input_growth_slot, input_growth.wrapping_add(accrual.fee_growth));
        if !accrual.protocol_fee.is_zero() {
            let slot = mapping_slot(input, PROTOCOL_FEES_ACCRUED_SLOT);
            let accrued = self.stored_value(storage, slot)?;
            storage.insert(slot, accrued.wrapping_add(accrual.protocol_fee));
        }
        Ok(())
    }

    /// The value of a PoolManager storage slot: from `storage` when the call already overrides it,
    /// otherwise from the engine's database.
    fn stored_value(
        &self,
        storage: &HashMap<U256, U256>,
        slot: U256,
    ) -> Result<U256, SimulationError> {
        if let Some(value) = storage.get(&slot) {
            return Ok(*value);
        }
        self.contract
            .engine
            .state
            .storage_ref(self.pool_manager, slot)
            .map_err(|e| {
                SimulationError::RecoverableError(format!(
                    "Failed to read PoolManager storage slot {slot:#x}: {e:?}"
                ))
            })
    }
}

/// The PoolManager's `pools` mapping, and the offsets of a pool's fields from the slot of its state
/// (v4-core `StateLibrary`).
const POOLS_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);
const FEE_GROWTH_GLOBAL0_OFFSET: U256 = U256::from_limbs([1, 0, 0, 0]);
const FEE_GROWTH_GLOBAL1_OFFSET: U256 = U256::from_limbs([2, 0, 0, 0]);
const LIQUIDITY_OFFSET: U256 = U256::from_limbs([3, 0, 0, 0]);
const TICKS_OFFSET: U256 = U256::from_limbs([4, 0, 0, 0]);
/// The offsets of a tick's fee growth outside of token 0 and of token 1 from the slot of its info.
const TICK_FEE_GROWTH_OUTSIDE0_OFFSET: U256 = U256::from_limbs([1, 0, 0, 0]);
const TICK_FEE_GROWTH_OUTSIDE1_OFFSET: U256 = U256::from_limbs([2, 0, 0, 0]);
/// The PoolManager's `protocolFeesAccrued` mapping (v4-core `ProtocolFees`).
const PROTOCOL_FEES_ACCRUED_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

/// The storage slot of `key` in a Solidity mapping at `slot`: `keccak256(abi.encode(key, slot))`.
fn mapping_slot(key: impl alloy::sol_types::SolValue, slot: U256) -> U256 {
    U256::from_be_bytes(keccak256(alloy::sol_types::SolValue::abi_encode(&(key, slot))).0)
}

impl<D: EngineDatabaseInterface + Clone + Debug + 'static> HookHandler for GenericVMHookHandler<D>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    fn address(&self) -> Address {
        self.address
    }

    fn needs_swap_fee_accrual(&self) -> bool {
        true
    }

    fn before_swap(
        &self,
        params: BeforeSwapParameters,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError> {
        let pending_overrides = self.pending_overrides.as_deref();
        let mut transient_storage_params = self.unlock_pool_manager();
        if let Some(input_params) = transient_storage {
            transient_storage_params.extend(input_params);
        }

        let token_in = if params.swap_params.zero_for_one {
            params.context.currency_0
        } else {
            params.context.currency_1
        };
        let mut token_overwrites = TokenProxyOverwriteFactory::new(token_in, None);
        // Overwrite pool manager's balance of token in. This is relevant when the pool manager does
        // not have a lot of these tokens and the hook assumes that the token in is transferred
        // before before_swap
        token_overwrites.set_balance(U256::MAX, self.pool_manager);
        // This is only to set the custom allowance flag to true, so we can fall in this condition
        // https://github.com/propeller-heads/tycho-simulation/blob/23c3b1fbacdf4cccec62b633c109e668c6d5f12a/token-proxy/src/TokenProxy.sol#L342
        token_overwrites.set_allowance(U256::MAX, Address::ZERO, self.address);
        let mut final_overwrites = token_overwrites.get_overwrites();
        if let Some(input_overwrites) = overwrites {
            final_overwrites.extend(input_overwrites)
        }
        let final_overwrites = over_pending_storage(pending_overrides, final_overwrites);

        let args = (
            params.sender,
            (
                params.context.currency_0,
                params.context.currency_1,
                Uint::<24, 1>::from(params.context.fees.lp_fee),
                Signed::<24, 1>::try_from(params.context.tick_spacing).map_err(|e| {
                    SimulationError::FatalError(format!("Failed to convert tick: {e:?}"))
                })?,
                self.address,
            ),
            (
                params.swap_params.zero_for_one,
                params.swap_params.amount_specified,
                params.swap_params.sqrt_price_limit,
            ),
            params.hook_data.to_vec(),
        );
        let selector = "beforeSwap(address,(address,address,uint24,int24,address),(bool,int256,uint160),bytes)";

        let res = self.contract.call(
            selector,
            args,
            Some(final_overwrites),
            Some(self.pool_manager),
            U256::from(0u64),
            Some(transient_storage_params),
            pending_overrides.and_then(|p| p.block.clone()),
            pending_overrides.and_then(|p| p.native_balances.clone()),
        )?;

        let decoded = BeforeSwapSolOutput::abi_decode(&res.return_value).map_err(|e| {
            SimulationError::FatalError(format!("Failed to decode before swap return value: {e:?}"))
        })?;
        let state_updates = res.simulation_result.state_updates;
        let overwrites: HashMap<Address, HashMap<U256, U256>> = state_updates
            .into_iter()
            .filter_map(|(address, update)| {
                update
                    .storage
                    .map(|storage| (address, storage))
            })
            .collect();
        Ok(WithGasEstimate {
            gas_estimate: res.simulation_result.gas_used,
            result: BeforeSwapOutput::new(
                decoded,
                overwrites,
                res.simulation_result.transient_storage,
            ),
        })
    }

    fn after_swap(
        &self,
        params: AfterSwapParameters,
        overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<AfterSwapDelta>, SimulationError> {
        let pending_overrides = self.pending_overrides.as_deref();
        let mut transient_storage_params = self.unlock_pool_manager();
        // What beforeSwap left holds the unlock flag too, so replacing the PoolManager map keeps
        // it.
        if let Some(input_params) = transient_storage {
            transient_storage_params.extend(input_params);
        }
        let pool_key = (
            params.context.currency_0,
            params.context.currency_1,
            Uint::<24, 1>::from(params.context.fees.lp_fee),
            Signed::<24, 1>::try_from(params.context.tick_spacing).map_err(|e| {
                SimulationError::FatalError(format!("Failed to convert tick: {e:?}"))
            })?,
            self.address,
        );
        let mut overwrites =
            over_pending_storage(pending_overrides, overwrites.unwrap_or_default());
        self.store_pool_state_after_swap(
            &mut overwrites,
            keccak256(alloy::sol_types::SolValue::abi_encode(&pool_key)),
            &params,
        )?;
        let args = (
            params.sender,
            pool_key,
            (
                params.swap_params.zero_for_one,
                params.swap_params.amount_specified,
                params.swap_params.sqrt_price_limit,
            ),
            params.delta.as_i256(),
            params.hook_data.to_vec(),
        );
        let selector = "afterSwap(address,(address,address,uint24,int24,address),(bool,int256,uint160),int256,bytes)";

        let res = self.contract.call(
            selector,
            args,
            Some(overwrites),
            Some(self.pool_manager),
            U256::from(0u64),
            Some(transient_storage_params),
            pending_overrides.and_then(|p| p.block.clone()),
            pending_overrides.and_then(|p| p.native_balances.clone()),
        )?;

        let decoded = AfterSwapSolReturn::abi_decode(&res.return_value).map_err(|e| {
            SimulationError::FatalError(format!("Failed to decode before swap return value: {e:?}"))
        })?;

        Ok(WithGasEstimate {
            gas_estimate: res.simulation_result.gas_used,
            result: I128::try_from(decoded.delta).map_err(|e| {
                SimulationError::FatalError(format!("Failed to convert delta: {e:?}"))
            })?,
        })
    }

    fn fee(&self, _context: &UniswapV4State, _params: SwapParams) -> Result<f64, SimulationError> {
        Err(SimulationError::RecoverableError(
            "fee is not implemented for GenericVMHookHandler".to_string(),
        ))
    }

    fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
        Err(SimulationError::RecoverableError(
            "spot_price is not implemented for GenericVMHookHandler".to_string(),
        ))
    }

    fn get_amount_ranges(
        &self,
        token_in: Bytes,
        token_out: Bytes,
    ) -> Result<AmountRanges, SimulationError> {
        if let Some(entrypoint) = &self.limits_entrypoint {
            let parts: Vec<&str> = entrypoint.split(':').collect();
            if parts.len() != 2 {
                return Err(SimulationError::FatalError(
                    "Invalid limits_entrypoint format. Expected 'address:signature'".to_string(),
                ));
            }
            let function_signature = parts[1];
            let token_in_addr = Address::from_slice(&token_in.0);
            let token_out_addr = Address::from_slice(&token_out.0);
            let contract_address = Address::from_str(parts[0]).map_err(|e| {
                SimulationError::FatalError(format!("Failed to parse contract address: {e:?}"))
            })?;
            let args = (token_in_addr, token_out_addr);

            // Use a deterministic lens contract address for state override
            // The lens contract bytecode will be deployed via state override during the eth_call.
            // This maintains the same interface as the original getLimits.
            let mut overwrites = None;
            let limits_contract = if self.is_euler {
                let lens_address = Address::from_str("0x0000000000000000000000000000000000001337")
                    .map_err(|e| {
                        SimulationError::FatalError(format!(
                            "Failed to parse contract address: {e:?}"
                        ))
                    })?;
                let info: AccountInfo = AccountInfo {
                    nonce: Default::default(),
                    balance: U256::from(0),
                    code: Some(Bytecode::new_raw(EULER_LENS_BYTECODE_BYTES.into())),
                    code_hash: keccak256(EULER_LENS_BYTECODE_BYTES),
                };
                let mut storage_overwrites = HashMap::new();

                // We set the hooks address to the slot 0 to allow identifying the desired hook
                // address
                let original_contract = format!("0x000000000000000000000000{:x}", contract_address);
                let original_contract_u256 = U256::from_str(&original_contract).map_err(|e| {
                    SimulationError::FatalError(format!(
                        "Failed to convert hook contract to U256: {e:?}"
                    ))
                })?;
                storage_overwrites.insert(U256::from(0), original_contract_u256);
                overwrites = Some(HashMap::from([(lens_address, storage_overwrites)]));

                self.contract
                    .engine
                    .state
                    .init_account(
                        lens_address,
                        info,
                        None,
                        true, // mocked
                    )
                    .map_err(|e| {
                        SimulationError::FatalError(format!("Failed to initialize contract: {e:?}"))
                    })?;
                TychoSimulationContract::new(lens_address, self.contract.engine.clone())?
            } else {
                TychoSimulationContract::new(contract_address, self.contract.engine.clone())?
            };

            let pending_overrides = self.pending_overrides.as_deref();
            let res = limits_contract.call(
                function_signature,
                args,
                Some(over_pending_storage(pending_overrides, overwrites.unwrap_or_default())),
                None,
                U256::from(0u64),
                None,
                pending_overrides.and_then(|p| p.block.clone()),
                pending_overrides.and_then(|p| p.native_balances.clone()),
            )?;

            let decoded = GetLimitsSolReturn::abi_decode(&res.return_value).map_err(|e| {
                SimulationError::FatalError(format!(
                    "Failed to decode getLimits return value: {:?} with error {e:?}",
                    res.return_value
                ))
            })?;

            // For now, we only care about upper limits
            Ok(AmountRanges {
                amount_in_range: (U256::ZERO, decoded.amount_in_upper_limit),
                amount_out_range: (U256::ZERO, decoded.amount_out_upper_limit),
            })
        } else {
            Err(SimulationError::RecoverableError(
                "limits_entrypoint is not set for this GenericVMHookHandler".to_string(),
            ))
        }
    }

    fn set_pending_overrides(&mut self, overrides: Arc<PendingOverrides>) {
        self.pending_overrides = Some(overrides);
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(limits_entrypoint_bytes) = delta
            .updated_attributes
            .get("limits_entrypoint")
        {
            let limits_entrypoint =
                String::from_utf8(limits_entrypoint_bytes.0.to_vec()).map_err(|e| {
                    TransitionError::SimulationError(SimulationError::FatalError(format!(
                        "Failed to parse limits_entrypoint from delta: {e:?}"
                    )))
                })?;

            self.limits_entrypoint = Some(limits_entrypoint);
        }

        Ok(())
    }

    fn clone_box(&self) -> Box<dyn HookHandler> {
        Box::new((*self).clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_equal(&self, other: &dyn HookHandler) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

/// `overwrites` slot by slot over the pending block's storage, so a hook call sees the pending
/// block's writes except where the caller overrides them.
fn over_pending_storage(
    pending_overrides: Option<&PendingOverrides>,
    overwrites: HashMap<Address, HashMap<U256, U256>>,
) -> HashMap<Address, HashMap<U256, U256>> {
    let Some(mut layered) = pending_overrides.and_then(|p| p.storage.clone()) else {
        return overwrites;
    };
    for (address, slots) in overwrites {
        layered
            .entry(address)
            .or_default()
            .extend(slots);
    }
    layered
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        default::Default,
        str::FromStr,
    };

    use alloy::primitives::{aliases::U24, B256, I256, U256};
    use revm::state::{AccountInfo, Bytecode};
    use tycho_client::feed::BlockHeader;

    use super::*;
    use crate::evm::{
        engine_db::{
            create_engine,
            simulation_db::{EVMProvider, SimulationDB},
            utils::{get_client, get_runtime},
        },
        protocol::{
            uniswap_v4::{
                hooks::models::{BalanceDelta, BeforeSwapDelta, PoolAfterSwap, StateContext},
                state::UniswapV4Fees,
            },
            vm::constants::MAX_BALANCE,
        },
        simulation::BlockEnvOverrides,
    };

    #[test]
    fn test_before_swap() {
        let block = BlockHeader {
            number: 22578103,
            hash: Bytes::from_str(
                "0x035c0e674c3bf3384a74b766908ab41c1968e989360aa26bea1dd64b1626f5f0",
            )
            .unwrap(),
            timestamp: 1748397011,
            ..Default::default()
        };

        let db = SimulationDB::new(
            get_client(None).expect("Failed to create client"),
            get_runtime().expect("Failed to get runtime"),
            Some(block),
        );
        let engine = create_engine(db, true).expect("Failed to create simulation engine");

        let hook_address = Address::from_str("0x0010d0d5db05933fa0d9f7038d365e1541a41888")
            .expect("Invalid hook address");

        let pool_manager = Address::from_str("0x000000000004444c5dc75cB358380D2e3dE08A90")
            .expect("Invalid pool manager address");

        let hook_handler = GenericVMHookHandler::new(
            hook_address,
            engine,
            pool_manager,
            HashMap::new(),
            HashMap::new(),
            None,
            false, // Not Euler - this is a Bunni hook contract
        )
        .expect("Failed to create GenericVMHookHandler");

        // simulating this tx: 0x6eef1c491d72edf73efd007b152b18d5f7814c5f3bd1c7d9be465fb9b4920f17
        let params = BeforeSwapParameters {
            context: StateContext {
                currency_0: Address::from_str("0x0000000000000000000000000000000000000000")
                    .unwrap(),
                currency_1: Address::from_str("0x000000c396558ffbab5ea628f39658bdf61345b3")
                    .unwrap(), // BUNNI
                fees: UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 1 },
                tick_spacing: 60,
            },
            sender: Address::from_str("0x66a9893cc07d91d95644aedd05d03f95e1dba8af").unwrap(),
            swap_params: SwapParams {
                zero_for_one: true,
                amount_specified: I256::try_from(-200000000000000000i128).unwrap(),
                sqrt_price_limit: U256::from(4295128740u64),
            },
            hook_data: Default::default(),
        };

        let result = hook_handler.before_swap(params, None, None);

        let res = result.unwrap().result;
        assert_eq!(
            res.amount_delta,
            BeforeSwapDelta(I256::from_raw(
                U256::from_str("68056473384187693032957288407292048885944984507633924869").unwrap()
            ))
        );
        assert_eq!(res.fee, U24::from(0));

        let expected_pool_manager_overwrites =            HashMap::from([
            (U256::from_str("79713336399215462747684527778665522702705876308670869673494720303103901230619").unwrap(), U256::from_str("78844352340497850335").unwrap()),
            (U256::from_str("108879414307233709404675130083871591294970341372768375199199972515051197806561").unwrap(), U256::from_str("21221083610867514702322193").unwrap()),
            (U256::from_str("66040010302484169318109846669699282294419705743047365478864291677462571000427").unwrap(), U256::from_str("6979725958049348898386").unwrap()),
            (U256::from_str("102130606322871718988206911149349113780831816677661791942959449920066266765719").unwrap(), U256::from_str("8016423715570101954005").unwrap()),
        ]);
        assert_eq!(
            *res.overwrites
                .get(&pool_manager)
                .unwrap(),
            expected_pool_manager_overwrites
        );

        let expected_pool_manager_transient_storage = HashMap::from([
            // CurrencyDelta(hook, BUNNI)
            (
                U256::from_str("55705082733434384960622358509877205174921948415007105780397939750626106833531").unwrap(),
                U256::from_str("56868629622924134286587").unwrap(),
            ),
            // CurrencyDelta(hook, ETH): -0.2 ETH
            (
                U256::from_str("72349358219047000942299849320276948455843849691036087799430587987856838543874").unwrap(),
                U256::from_str("115792089237316195423570985008687907853269984665640564039457384007913129639936").unwrap(),
            ),
            // Unlocked
            (
                U256::from_str("87100234046427240614499661373387320107015461065347489303548037305558901893923").unwrap(),
                U256::from_str("1").unwrap(),
            ),
            // NonzeroDeltaCount
            (
                U256::from_str("56671960505278111519104690822132496699113179860588238901689140059013086026251").unwrap(),
                U256::from_str("2").unwrap(),
            ),
        ]);
        assert_eq!(
            *res.transient_storage
                .get(&pool_manager)
                .unwrap(),
            expected_pool_manager_transient_storage
        );
    }

    #[test]
    fn test_after_swap() {
        let block = BlockHeader {
            number: 15797251,
            hash: Bytes::from_str(
                "0x7032b93c5b0d419f2001f7c77c19ade6da92d2df147712eac1a27c7ffedfe410",
            )
            .unwrap(),
            timestamp: 1748397011,
            ..Default::default()
        };
        let db = SimulationDB::new(
            get_client(None).expect("Failed to create client"),
            get_runtime().expect("Failed to get runtime"),
            Some(block),
        );
        let engine = create_engine(db, true).expect("Failed to create simulation engine");

        // pool manager on ethereum
        let pool_manager = Address::from_str("0x000000000004444c5dc75cB358380D2e3dE08A90")
            .expect("Invalid pool manager address");

        let pool_manager_bytecode =
            Bytecode::new_raw(include_bytes!("assets/pool_manager_bytecode.bin").into());

        engine
            .state
            .init_account(
                pool_manager,
                AccountInfo {
                    balance: *MAX_BALANCE,
                    nonce: 0,
                    code_hash: B256::from(keccak256(pool_manager_bytecode.clone().bytes())),
                    code: Some(pool_manager_bytecode),
                },
                None,
                false,
            )
            .expect("Failed to initialize account");

        let hook_address = Address::from_str("0x0010d0d5db05933fa0d9f7038d365e1541a41888")
            .expect("Invalid hook address");

        // This bytecode corresponds to LimitOrder example hook https://github.com/Uniswap/v4-periphery/blob/example-contracts/contracts/hooks/examples/LimitOrder.sol
        // To get the bytecode:
        //  - we had to change the `IPoolManager public immutable manager` from an immutable to a
        //    constant and hardcode the pool manager address. This is necessary because the
        //    immutable variables are only filled in at (real) deployment time, if we just want to
        //    inspect the bytecode they will be set to zero.
        //  - `forge inspect LimitOrder deployedBytecode` to get the bytecode.
        let bytecode =
            Bytecode::new_raw(include_bytes!("assets/after_swap_test_hook_bytecode.bin").into());

        engine
            .state
            .init_account(
                hook_address,
                AccountInfo {
                    balance: *MAX_BALANCE,
                    nonce: 0,
                    code_hash: B256::from(keccak256(bytecode.clone().bytes())),
                    code: Some(bytecode),
                },
                None,
                true,
            )
            .expect("Failed to initialize account");

        let hook_handler = GenericVMHookHandler::new(
            hook_address,
            engine,
            pool_manager,
            HashMap::new(),
            HashMap::new(),
            None,
            false, // Not euler contract
        )
        .expect("Failed to create GenericVMHookHandler");

        let context = StateContext {
            currency_0: Address::from_str("0x0000000000000000000000000000000000000000").unwrap(),
            currency_1: Address::from_str("0x000000c396558ffbab5ea628f39658bdf61345b3").unwrap(),
            fees: UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 1 },
            tick_spacing: 60,
        };
        let swap_params = SwapParams {
            zero_for_one: true,
            amount_specified: I256::try_from(-200000000000000000i128).unwrap(),
            sqrt_price_limit: U256::from(4295128740u64),
        };

        let after_swap_params = AfterSwapParameters {
            context,
            sender: Address::from_str("0x66a9893cc07d91d95644aedd05d03f95e1dba8af").unwrap(),
            swap_params,
            delta: BalanceDelta(
                I256::from_dec_str("-3777134272822416944443458142492627143113384069767150805")
                    .unwrap(),
            ),
            hook_data: Bytes::new(),
            pool_after_swap: PoolAfterSwap::default(),
        };

        let result = hook_handler.after_swap(after_swap_params, None, None);

        let res = result.unwrap().result;
        // This hook does not return any delta, so we expect it to be zero.
        assert_eq!(res, I128::ZERO);
    }

    #[test]
    #[ignore = "requires network access to the public Unichain RPC, which is rate limited"]
    fn test_before_and_after_swap() {
        let block = BlockHeader {
            number: 15797251,
            hash: Bytes::from_str(
                "0x7032b93c5b0d419f2001f7c77c19ade6da92d2df147712eac1a27c7ffedfe410",
            )
            .unwrap(),
            timestamp: 1746562410,
            ..Default::default()
        };
        let db = SimulationDB::new(
            get_client(Some("https://unichain.drpc.org".into())).expect("Failed to create client"),
            get_runtime().expect("Failed to get runtime"),
            Some(block),
        );
        let engine = create_engine(db, true).expect("Failed to create simulation engine");

        let hook_address = Address::from_str("0x7f7d7e4a9d4da8997730997983c5ca64846868c0")
            .expect("Invalid hook address");

        let pool_manager = Address::from_str("0x1F98400000000000000000000000000000000004")
            .expect("Invalid pool manager address");

        let hook_handler = GenericVMHookHandler::new(
            hook_address,
            engine,
            pool_manager,
            HashMap::new(),
            HashMap::new(),
            None,
            false, // Not euler
        )
        .expect("Failed to create GenericVMHookHandler");

        let universal_router =
            Address::from_str("0xef740bf23acae26f6492b10de645d6b98dc8eaf3").unwrap();

        // simulating this tx: 0x6f471e490570c89482e44edd286db35b2dd93c52307bfe6f28dbe8ed3326470d on
        // unichain
        let context = StateContext {
            currency_0: Address::from_str("0x0000000000000000000000000000000000000000").unwrap(),
            currency_1: Address::from_str("0x7edc481366a345d7f9fcecb207408b5f2887ff99").unwrap(),
            fees: UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 100 },
            tick_spacing: 1,
        };
        let swap_params = SwapParams {
            zero_for_one: true,
            amount_specified: I256::from_dec_str("-11100000000000000").unwrap(),
            sqrt_price_limit: U256::from_str("4295128740").unwrap(),
        };
        let params = BeforeSwapParameters {
            context: context.clone(),
            sender: universal_router,
            swap_params: swap_params.clone(),
            hook_data: Default::default(),
        };
        // Setting the sender in UniversalRouter transient storage
        let sender = Address::from_str("0xceeb96f4733ba07ca56d0052fb132ffa1e0d7b16").unwrap();
        let is_unlocked_slot = U256::from_be_bytes(keccak256("Locker").0) - U256::from(1);
        let transient_storage = HashMap::from([(
            universal_router,
            HashMap::from([(is_unlocked_slot, U256::from_be_slice(sender.as_slice()))]),
        )]);

        let result = hook_handler
            .before_swap(params, None, Some(transient_storage))
            .unwrap();

        assert_eq!(result.result.amount_delta, BeforeSwapDelta(I256::from_dec_str("0").unwrap()));

        let after_swap_params = AfterSwapParameters {
            context,
            sender: universal_router,
            swap_params,
            delta: BalanceDelta(
                I256::from_dec_str("-3777134272822416944443458142492627143113384069767150805")
                    .unwrap(),
            ),
            hook_data: Bytes::new(),
            pool_after_swap: PoolAfterSwap::default(),
        };

        let result = hook_handler.after_swap(
            after_swap_params,
            Some(result.result.overwrites),
            Some(result.result.transient_storage),
        );

        let res = result.unwrap().result;
        // This hook does not return any delta, so we expect it to be zero.
        assert_eq!(res, I128::ZERO);
    }

    #[test]
    fn test_get_amount_ranges() {
        let block = BlockHeader {
            number: 23072527,
            hash: Bytes::from_str(
                "0x639d7e454339ba43da3b2288b45078405330afcc3cd7f10e6e852be9c70ac164",
            )
            .unwrap(),
            timestamp: 1754368727,
            ..Default::default()
        };

        let db = SimulationDB::new(
            get_client(None).expect("Failed to create client"),
            get_runtime().expect("Failed to get runtime"),
            Some(block),
        );
        let engine = create_engine(db, true).expect("Failed to create simulation engine");

        let hook_address = Address::from_str("0xC88b618C2c670c2e2a42e06B466B6F0e82A6E8A8")
            .expect("Invalid hook address");

        let pool_manager = Address::from_str("0x000000000004444c5dc75cB358380D2e3dE08A90")
            .expect("Invalid pool manager address");

        // Encode function signature as expected in TychoSimulationContract.encode_input
        let limits_entrypoint =
            "0xC88b618C2c670c2e2a42e06B466B6F0e82A6E8A8:getLimits(address,address)";
        let hook_handler = GenericVMHookHandler::new(
            hook_address,
            engine,
            pool_manager,
            HashMap::new(),
            HashMap::new(),
            Some(limits_entrypoint.to_string()),
            true, // Euler contract
        )
        .expect("Failed to create GenericVMHookHandler");

        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(); // USDC
        let token_out = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(); // WETH

        let result = hook_handler.get_amount_ranges(token_in, token_out);

        match result {
            Ok(ranges) => {
                assert_eq!(ranges.amount_in_range.0, U256::ZERO);
                assert_eq!(ranges.amount_out_range.0, U256::ZERO);

                // This varies depending on block, so we just use a safe min amount
                let min_expected_limit_1 = U256::from_str("1000000").unwrap();
                let min_expected_limit_2 = U256::from_str("1000").unwrap();

                assert!(ranges.amount_in_range.1 >= min_expected_limit_1);
                assert!(ranges.amount_out_range.1 >= min_expected_limit_2);
            }
            Err(e) => {
                panic!("get_amount_out ranges failed {e}");
            }
        }
    }

    #[test]
    fn test_delta_transition_limits_entrypoint_decoding() {
        let block = BlockHeader {
            number: 1,
            hash: Bytes::from_str(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap(),
            timestamp: 1748397011,
            ..Default::default()
        };
        let db = SimulationDB::new(
            get_client(None).expect("Failed to create client"),
            get_runtime().expect("Failed to get runtime"),
            Some(block),
        );
        let engine = create_engine(db, true).expect("Failed to create simulation engine");

        let mut hook_handler = GenericVMHookHandler {
            contract: TychoSimulationContract::new(Address::ZERO, engine).unwrap(),
            address: Address::ZERO,
            pool_manager: Address::ZERO,
            limits_entrypoint: None,
            is_euler: true,
            pending_overrides: None,
        };

        assert!(hook_handler.limits_entrypoint.is_none());
        let limits_entrypoint_value =
            "0xC88b618C2c670c2e2a42e06B466B6F0e82A6E8A8:getLimits(address,address)";
        let mut updated_attributes = HashMap::new();

        // Imitate the way that the limits_entrypoint is inserted in the indexer
        updated_attributes.insert(
            "limits_entrypoint".to_string(),
            Bytes::from(
                limits_entrypoint_value
                    .as_bytes()
                    .to_vec(),
            ),
        );

        let delta = ProtocolStateDelta {
            component_id: "hook_component".to_string(),
            updated_attributes,
            deleted_attributes: HashSet::new(),
        };

        let result = hook_handler.delta_transition(delta, &HashMap::new(), &Default::default());
        assert!(result.is_ok());

        // Verify the limits_entrypoint was updated correctly
        assert_eq!(hook_handler.limits_entrypoint.unwrap(), limits_entrypoint_value);
    }

    #[test]
    fn test_caller_overwrites_win_over_pending_storage_slot_by_slot() {
        let hook = Address::repeat_byte(1);
        let other = Address::repeat_byte(2);
        let pending = PendingOverrides {
            storage: Some(HashMap::from([
                (
                    hook,
                    HashMap::from([
                        (U256::from(1), U256::from(10)),
                        (U256::from(2), U256::from(20)),
                    ]),
                ),
                (other, HashMap::from([(U256::from(1), U256::from(30))])),
            ])),
            native_balances: None,
            block: Some(BlockEnvOverrides { number: Some(1), timestamp: Some(2) }),
        };
        let overwrites = HashMap::from([(hook, HashMap::from([(U256::from(1), U256::from(11))]))]);

        let layered = over_pending_storage(Some(&pending), overwrites);

        assert_eq!(layered[&hook][&U256::from(1)], U256::from(11), "the caller's slot wins");
        assert_eq!(
            layered[&hook][&U256::from(2)],
            U256::from(20),
            "the pending block's other slot of the same account survives"
        );
        assert_eq!(layered[&other][&U256::from(1)], U256::from(30));
    }

    #[test]
    fn test_no_pending_storage_leaves_overwrites_untouched() {
        let hook = Address::repeat_byte(1);
        let overwrites = HashMap::from([(hook, HashMap::from([(U256::from(1), U256::from(11))]))]);

        assert_eq!(over_pending_storage(None, overwrites.clone()), overwrites);
        assert_eq!(
            over_pending_storage(Some(&PendingOverrides::default()), overwrites.clone()),
            overwrites
        );
    }

    fn offline_handler(pool_manager: Address) -> GenericVMHookHandler<SimulationDB<EVMProvider>> {
        let db = SimulationDB::new(get_client(None).unwrap(), get_runtime().unwrap(), None);
        GenericVMHookHandler {
            contract: TychoSimulationContract::new(Address::ZERO, create_engine(db, true).unwrap())
                .unwrap(),
            address: Address::ZERO,
            pool_manager,
            limits_entrypoint: None,
            is_euler: false,
            pending_overrides: None,
        }
    }

    fn after_swap_params(
        zero_for_one: bool,
        currencies: [Address; 2],
        pool: PoolAfterSwap,
    ) -> AfterSwapParameters {
        AfterSwapParameters {
            context: StateContext {
                currency_0: currencies[0],
                currency_1: currencies[1],
                fees: UniswapV4Fees::new(0, 0, 0),
                tick_spacing: 10,
            },
            sender: Address::ZERO,
            swap_params: SwapParams {
                zero_for_one,
                amount_specified: I256::ZERO,
                sqrt_price_limit: U256::ZERO,
            },
            delta: BalanceDelta(I256::ZERO),
            hook_data: Bytes::new(),
            pool_after_swap: pool,
        }
    }

    /// The 0.265 ETH sell on ETH/cbBTC pool
    /// 0x8514479827d1884c2957e8015b2aed79c4422e89e44476a170a70101b5422203 in block 26399072, which
    /// crossed tick -266140 and paid no protocol fee: writing its post-swap state over the
    /// PoolManager storage of block 26399071 gives the storage of block 26399072. The accrual is
    /// the one this swap records (`state::tests::eth_cbbtc_sell`).
    #[test]
    fn test_pool_state_after_swap_with_fees() {
        let pool_manager = Address::from_str("0x000000000004444c5dc75cb358380d2e3de08a90").unwrap();
        let pool_id =
            B256::from_str("0x8514479827d1884c2957e8015b2aed79c4422e89e44476a170a70101b5422203")
                .unwrap();
        let state =
            U256::from_str("0x3411256eb99e84d9e9170aadf4ae5a0f3537fcb3510407c58776c92b67930323")
                .unwrap();
        let tick =
            U256::from_str("0xa69b839a2e94a8ce248b8d37a9dbea05fd833ff4923a7eb36c1cee1958f3162f")
                .unwrap();
        let slots = [
            state,
            state + U256::from(1),
            state + U256::from(2),
            state + U256::from(3),
            tick + U256::from(1),
            tick + U256::from(2),
        ];
        let before = [
            "0x000000000000000000fbf06e000000000000000000001bee48016527bc99c6c9",
            "0x19629d1724f23b6d2c61c11e60a272b85e5",
            "0x3a52f6d24ef7e520358c6c315",
            "0x105b562238d6d",
            "0x18efb0a967816426f502e0fcb9d9a5fb752",
            "0x39290b807e5895d1952750e10",
        ];
        let after = [
            "0x000000000000000000fbf04b000000000000000000001be1a4ed81b08a14a878",
            "0x196f62884d7dd5cf55b66cc34f053fd5a90",
            "0x3a52f6d24ef7e520358c6c315",
            "0xd32ae2075d2d",
            "0x76c2e71c314d924abdaf230d852dc02f6",
            "0x129eb51d09f4f4ea0651b505",
        ];
        let mut overwrites = HashMap::from([(
            pool_manager,
            slots
                .iter()
                .zip(before)
                .map(|(slot, value)| (*slot, U256::from_str(value).unwrap()))
                .collect::<HashMap<_, _>>(),
        )]);
        let params = after_swap_params(
            true,
            [
                Address::ZERO,
                Address::from_str("0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf").unwrap(),
            ],
            PoolAfterSwap {
                sqrt_price: U256::from_str("131666296751652001327224").unwrap(),
                tick: -266165,
                liquidity: 232181134220589,
                fees: Some(SwapFeeAccrual {
                    fee_growth: U256::from_str("0xcc571288b9a622954aba4ee62cd1d4ab").unwrap(),
                    protocol_fee: U256::ZERO,
                    crossed_ticks: vec![(
                        -266140,
                        U256::from_str("0x3d6795ec0764c135ecf0166bc6103463").unwrap(),
                    )],
                }),
            },
        );

        offline_handler(pool_manager)
            .store_pool_state_after_swap(&mut overwrites, pool_id, &params)
            .unwrap();

        let written: Vec<U256> = slots
            .iter()
            .map(|slot| overwrites[&pool_manager][slot])
            .collect();
        let expected: Vec<U256> = after
            .iter()
            .map(|value| U256::from_str(value).unwrap())
            .collect();
        assert_eq!(written, expected);
        assert_eq!(overwrites[&pool_manager].len(), slots.len(), "no protocol fee to write");
    }

    /// A swap's protocol fee is added to `protocolFeesAccrued` of its input token. The slot is the
    /// one the mainnet PoolManager's `protocolFeesAccrued(USDC)` getter reads.
    #[test]
    fn test_pool_state_after_swap_protocol_fee() {
        let pool_manager = Address::from_str("0x000000000004444c5dc75cb358380d2e3de08a90").unwrap();
        let usdc = Address::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let accrued_usdc =
            U256::from_str("0x84893e0f271e5f8233d24aa85ba38e0d2ed8f0fc8f608c286ccee51e6c35dd6e")
                .unwrap();
        // Every other slot the write reads is set, so the database is never asked.
        let state = mapping_slot(B256::ZERO, U256::from(6));
        let mut storage: HashMap<U256, U256> = (0..3u8)
            .map(|offset| (state + U256::from(offset), U256::ZERO))
            .collect();
        storage.insert(accrued_usdc, U256::from(321081233u64));
        let mut overwrites = HashMap::from([(pool_manager, storage)]);
        let fees = SwapFeeAccrual { protocol_fee: U256::from(345509u64), ..Default::default() };
        let params = after_swap_params(
            false,
            [Address::ZERO, usdc],
            PoolAfterSwap { fees: Some(fees), ..Default::default() },
        );

        offline_handler(pool_manager)
            .store_pool_state_after_swap(&mut overwrites, B256::ZERO, &params)
            .unwrap();

        assert_eq!(overwrites[&pool_manager][&accrued_usdc], U256::from(321081233u64 + 345509));
    }

    /// PoolManager storage of mainnet ETH/USDC pool
    /// 0x21c67e77068de97969ba93d4aab21826d33ca12bb9f565d8496e8fda8a82ca27 (protocol fee 512125,
    /// LP fee 500) at blocks 26400000 and 26400200: writing the later price, tick and liquidity
    /// over the earlier slot0 gives the later slot0 value.
    #[test]
    fn test_pool_state_after_swap() {
        let pool_manager = Address::from_str("0x000000000004444c5dc75cb358380d2e3de08a90").unwrap();
        let pool_id =
            B256::from_str("0x21c67e77068de97969ba93d4aab21826d33ca12bb9f565d8496e8fda8a82ca27")
                .unwrap();
        let slot0 =
            U256::from_str("0xda8cac368d67cd2f2d8aaa5cc531768e0fa3b1d205c5c5de60da078e1f59bdfc")
                .unwrap();
        let db = SimulationDB::new(get_client(None).unwrap(), get_runtime().unwrap(), None);
        let hook_handler = GenericVMHookHandler {
            contract: TychoSimulationContract::new(Address::ZERO, create_engine(db, true).unwrap())
                .unwrap(),
            address: Address::ZERO,
            pool_manager,
            limits_entrypoint: None,
            is_euler: false,
            pending_overrides: None,
        };
        let mut overwrites = HashMap::from([(
            pool_manager,
            HashMap::from([(
                slot0,
                U256::from_str(
                    "0x0000000001f407d07dfcf1bf00000000000000000002f05eabb55f9c49d605f0",
                )
                .unwrap(),
            )]),
        )]);
        let params = AfterSwapParameters {
            context: StateContext {
                currency_0: Address::ZERO,
                currency_1: Address::ZERO,
                fees: UniswapV4Fees::new(0, 0, 0),
                tick_spacing: 10,
            },
            sender: Address::ZERO,
            swap_params: SwapParams {
                zero_for_one: true,
                amount_specified: I256::ZERO,
                sqrt_price_limit: U256::ZERO,
            },
            delta: BalanceDelta(I256::ZERO),
            hook_data: Bytes::new(),
            pool_after_swap: PoolAfterSwap {
                sqrt_price: U256::from_str("3549800748464954447918354").unwrap(),
                tick: -200274,
                liquidity: 128237474780958120,
                fees: None,
            },
        };

        hook_handler
            .store_pool_state_after_swap(&mut overwrites, pool_id, &params)
            .unwrap();

        assert_eq!(
            overwrites[&pool_manager][&slot0],
            U256::from_str("0x0000000001f407d07dfcf1ae00000000000000000002efb3158c167381527512")
                .unwrap()
        );
        assert_eq!(
            overwrites[&pool_manager][&(slot0 + U256::from(3))],
            U256::from(128237474780958120u64)
        );
    }
}
