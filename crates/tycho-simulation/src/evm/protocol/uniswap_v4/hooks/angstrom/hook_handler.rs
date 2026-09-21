use std::{any::Any, collections::HashMap, fmt::Debug};

use alloy::primitives::{aliases::U24, Address, I128, I256, U256};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::Balances,
    },
    Bytes,
};

use crate::evm::protocol::uniswap_v4::hooks::{
    hook_handler::HookHandler,
    models::{
        AfterSwapDelta, AfterSwapParameters, AmountRanges, BeforeSwapDelta, BeforeSwapOutput,
        BeforeSwapParameters, SwapParams, WithGasEstimate,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub struct AngstromFees {
    pub unlock: U24,
    pub protocol_unlock: U24,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AngstromHookHandler {
    address: Address,
    pool_manager: Address,
    fees: AngstromFees,
    pool_removed: bool,
}

impl AngstromHookHandler {
    pub fn new(
        address: Address,
        pool_manager: Address,
        fees: AngstromFees,
        pool_removed: bool,
    ) -> Self {
        Self { address, pool_manager, fees, pool_removed }
    }
}
impl HookHandler for AngstromHookHandler {
    fn address(&self) -> Address {
        self.address
    }
    fn fee(
        &self,
        _: &crate::evm::protocol::uniswap_v4::state::UniswapV4State,
        _: SwapParams,
    ) -> Result<f64, SimulationError> {
        // For Angstrom naive swaps, the fee is handled by the pool manager
        // and not overridden by the hook
        Err(SimulationError::RecoverableError(
            "fee is not implemented for AngstromHook".to_string(),
        ))
    }

    fn before_swap(
        &self,
        params: BeforeSwapParameters,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError> {
        if self.pool_removed {
            return Err(SimulationError::FatalError(format!(
                "angstrom pool {} has been removed",
                self.address
            )));
        }
        // Base gas for the hook call
        let mut gas_estimate = 400;
        if !params.hook_data.is_empty() {
            // Another call + sstore + ec-recover
            gas_estimate += 10_000;
        }

        // Gas for internal asset/pool lookup
        gas_estimate += 1000;

        // Apply the unlock fee as an override.
        // In Uniswap V4, bit 22 is the OVERRIDE_FEE_FLAG (0x400000) - see LPFeeLibrary.sol
        //
        // Replicate the behaviour of the Angstrom hook:
        // swapFee = _unlockedFees[storeKey].unlockedFee | LPFeeLibrary.OVERRIDE_FEE_FLAG;
        const OVERRIDE_FEE_FLAG: u32 = 1 << 22;
        let swap_fee_override = U24::from(self.fees.unlock.to::<u32>() | OVERRIDE_FEE_FLAG);

        Ok(WithGasEstimate {
            gas_estimate,
            result: BeforeSwapOutput {
                amount_delta: BeforeSwapDelta(I256::ZERO),
                fee: swap_fee_override,
                overwrites: HashMap::new(),
                transient_storage: HashMap::new(),
            },
        })
    }

    fn after_swap(
        &self,
        params: AfterSwapParameters,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
        _: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<AfterSwapDelta>, SimulationError> {
        let fee_rate_e6 = I128::unchecked_from(self.fees.protocol_unlock.to::<u32>());
        let one_e6 = I128::unchecked_from(1_000_000);

        let exact_input = params.swap_params.amount_specified < I256::ZERO;
        let target_amount = if exact_input != params.swap_params.zero_for_one {
            params.delta.amount0()
        } else {
            params.delta.amount1()
        };

        // Take absolute value of target amount
        let p_target_amount =
            if target_amount < I128::ZERO { target_amount.wrapping_neg() } else { target_amount };

        let protocol_fee_amount = if exact_input {
            p_target_amount * fee_rate_e6 / one_e6
        } else {
            p_target_amount * one_e6 / (one_e6 - fee_rate_e6) - p_target_amount
        };

        // Note: On-chain, this mints the fee to the fee collector and returns just the int128 fee:
        // UNI_V4.mint(address(FEE_COLLECTOR), currencyId, uint128(fee));
        // return (IAfterSwapHook.afterSwap.selector, fee);
        // Minting is irrelevant for our simulation.

        Ok(WithGasEstimate {
            // lookup + mint + tick updates post swap.
            gas_estimate: 15_000,
            result: protocol_fee_amount,
        })
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        // Handle fee updates
        if let Some(unlocked_fee) = delta
            .updated_attributes
            .get("angstrom_unlocked_fee")
        {
            self.fees.unlock = U24::from_be_slice(unlocked_fee);
        }
        if let Some(protocol_unlocked_fee) = delta
            .updated_attributes
            .get("angstrom_protocol_unlocked_fee")
        {
            self.fees.protocol_unlock = U24::from_be_slice(protocol_unlocked_fee);
        }

        if let Some(angstrom_removed_pool) = delta
            .updated_attributes
            .get("angstrom_removed_pool")
        {
            self.pool_removed = !angstrom_removed_pool.is_zero();
        }

        Ok(())
    }

    fn transitions_from_delta_alone(&self, _delta: &ProtocolStateDelta) -> bool {
        true
    }

    fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
        Err(SimulationError::RecoverableError(
            "spot_price is not implemented for AngstromHook".to_string(),
        ))
    }

    fn get_amount_ranges(
        &self,
        _token_in: Bytes,
        _token_out: Bytes,
    ) -> Result<AmountRanges, SimulationError> {
        Err(SimulationError::RecoverableError(
            "get_amount_ranges is not implemented for AngstromHook".to_string(),
        ))
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

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use alloy::primitives::I256;
    use tycho_common::Bytes;

    use super::*;
    use crate::evm::protocol::uniswap_v4::{
        hooks::models::{BalanceDelta, StateContext},
        state::UniswapV4Fees,
    };

    #[test]
    fn test_before_swap() {
        // Case taken from tx 0xa9a8c90060e4d5e8bac9ad06eaf706d87d36cb4d11fd2d705f6a37b84918765d
        let token_0 = Address::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let token_1 = Address::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let fees = AngstromFees {
            // To get these values, enable storage access logs on tenderly,
            // and look at the hex value retrieved right after calling afterSwap
            //
            // The value 1879048430 (hex: 0x700000EE) contains two packed uint24 values:
            // Lower 24 bits (unlockedFee):      0x0000EE   = 238
            // Upper 24 bits (protocolUnlockedFee): 0x70    = 112
            unlock: U24::from(238),
            protocol_unlock: U24::from(112),
        };
        let handler = AngstromHookHandler::new(
            Address::from_str("0x0000000aa232009084bd71a5797d089aa4edfad4").unwrap(),
            Address::from_str("0x000000000004444c5dc75cb358380d2e3de08a90").unwrap(),
            fees,
            false,
        );

        let params = BeforeSwapParameters {
            context: StateContext {
                currency_0: token_0,
                currency_1: token_1,
                // Fees are irrelevant to the beforeSwap method.
                fees: UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 3000 },
                tick_spacing: 10,
            },
            sender: Address::from_str("0xb535aeb27335b91e1b5bccbd64888ba7574efbf8").unwrap(),
            // Swap params and hook data taken from tenderly simulation: input to innermost
            // beforeSwap call (directly to hook, not to UniswapV4's Hook library wrapper)
            swap_params: SwapParams {
                zero_for_one: true,
                amount_specified: I256::try_from(-401348115).unwrap(),
                sqrt_price_limit: U256::from_str("4295128740").unwrap(),
            },
            // This hook data contains the attestation - used solely for access control and not for
            // simulation. For this reason, although it's present in the tenderly simulation,
            // we do not include this in our test.
            hook_data: Bytes::new(),
        };

        let result = handler
            .before_swap(params, None, None)
            .unwrap();

        // Tenderly response:
        // "output":{
        //      "1":"0" -> amount delta
        //      "response":"0x575e24b4" -> selector (not used in our case)
        //      "swapFee":"4194542" -> fee
        // }
        assert_eq!(result.result.amount_delta, BeforeSwapDelta(I256::ZERO));
        assert_eq!(result.result.fee, U24::from(4194542));
    }

    #[test]
    fn test_after_swap() {
        // Case taken from tx 0xa9a8c90060e4d5e8bac9ad06eaf706d87d36cb4d11fd2d705f6a37b84918765d
        let token_0 = Address::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let token_1 = Address::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();

        let fees = AngstromFees {
            // To get these values, enable storage access logs on tenderly,
            // and look at the hex value retrieved right after calling afterSwap
            //
            // The value 1879048430 (hex: 0x700000EE) contains two packed uint24 values:
            // Lower 24 bits (unlockedFee):      0x0000EE   = 238
            // Upper 24 bits (protocolUnlockedFee): 0x70    = 112
            unlock: U24::from(238),
            protocol_unlock: U24::from(112),
        };
        let handler = AngstromHookHandler::new(
            Address::from_str("0x0000000aa232009084bd71a5797d089aa4edfad4").unwrap(),
            Address::from_str("0x000000000004444c5dc75cb358380d2e3de08a90").unwrap(),
            fees,
            false,
        );

        let params = AfterSwapParameters {
            context: StateContext {
                currency_0: token_0,
                currency_1: token_1,
                // Irrelevant: Fee is not used in afterSwap calculations.
                fees: UniswapV4Fees { zero_for_one: 0, one_for_zero: 0, lp_fee: 0 },
                tick_spacing: 10,
            },
            sender: Address::from_str("0xb535aeb27335b91e1b5bccbd64888ba7574efbf8").unwrap(),
            // Taken from tenderly simulation: input to innermost afterSwap call (directly to hook,
            // not to UniswapV4's Hook library wrapper)
            swap_params: SwapParams {
                zero_for_one: true,
                amount_specified: I256::try_from(-401348115i128).unwrap(),
                sqrt_price_limit: U256::from_str("4295128740").unwrap(),
            },
            // Taken from tenderly simulation: Swap event (since input to the hook method is not
            // parsed properly on tenderly)
            delta: BalanceDelta::new(
                I128::try_from(-401348115i128).unwrap(),
                I128::try_from(128499868287523768i128).unwrap(),
            ),
            hook_data: Bytes::new(),
        };

        let result = handler
            .after_swap(params, None, None)
            .unwrap();

        // Taken from tenderly simulation: output of innermost afterSwap call (directly to hook,
        // not to UniswapV4's Hook library wrapper)
        assert_eq!(result.result, I128::unchecked_from(14391985248202i128));
        assert_eq!(result.gas_estimate, 15_000);
    }

    #[test]
    fn test_delta_transition() {
        let fees = AngstromFees {
            // To get these values, enable storage access logs on tenderly,
            // and look at the hex value retrieved right after calling afterSwap
            //
            // The value 1879048430 (hex: 0x700000EE) contains two packed uint24 values:
            // Lower 24 bits (unlockedFee):      0x0000EE   = 238
            // Upper 24 bits (protocolUnlockedFee): 0x70    = 112
            unlock: U24::from(238),
            protocol_unlock: U24::from(112),
        };
        let mut handler = AngstromHookHandler::new(
            Address::from_str("0x0000000aa232009084bd71a5797d089aa4edfad4").unwrap(),
            Address::from_str("0x000000000004444c5dc75cb358380d2e3de08a90").unwrap(),
            fees,
            false,
        );

        // Change fees
        let new_unlock_fee = &U24::from(4000).to_be_bytes::<3>();
        let new_protocol_unlock_fee = &U24::from(3000).to_be_bytes::<3>();

        let mut updated_attributes = HashMap::new();
        updated_attributes.insert("angstrom_unlocked_fee".to_string(), new_unlock_fee.into());
        updated_attributes
            .insert("angstrom_protocol_unlocked_fee".to_string(), new_protocol_unlock_fee.into());

        let delta = ProtocolStateDelta {
            component_id: "test".to_string(),
            updated_attributes,
            deleted_attributes: HashSet::new(),
        };

        let result = handler.delta_transition(delta, &HashMap::new(), &Default::default());
        assert!(result.is_ok());

        // Verify new pool was added
        assert!(handler.fees.unlock == U24::from(4000));
    }
}
