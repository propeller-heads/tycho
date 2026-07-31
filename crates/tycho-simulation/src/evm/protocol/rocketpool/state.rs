use std::{any::Any, collections::HashMap};

use alloy::primitives::U256;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};
use tycho_ethereum::BytesCodec;

use crate::evm::protocol::{
    rocketpool::ETH_ADDRESS,
    safe_math::{safe_add_u256, safe_sub_u256},
    u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    utils::solidity_math::mul_div,
};

const DEPOSIT_FEE_BASE: u128 = 1_000_000_000_000_000_000; // 1e18

/// 32 ETH — the hardcoded amount every megapool queue entry requests.
const FULL_DEPOSIT_VALUE: u128 = 32_000_000_000_000_000_000u128;

// The names of the constants reflect the exact method from the tenderly log.
const BASE_DEPOSIT_GAS: u64 = 209_000;
const RETH_MINT_GAS: u64 = 50_000;
const VAULT_DEPOSIT_OR_EXCESS_GAS: u64 = 20_000;
const ASSIGN_DEPOSITS_BASE_GAS: u64 = 20_000;
// peekItem + withdrawEther + assignFunds + dequeueItem
const GAS_PER_MEGAPOOL_ASSIGNMENT: u64 = 120_000;
const BASE_WITHDRAWAL_GAS: u64 = 134_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RocketpoolState {
    pub reth_supply: U256,
    pub total_eth: U256,
    /// ETH available in the deposit pool contract
    pub deposit_contract_balance: U256,
    /// ETH available in the rETH contract
    pub reth_contract_liquidity: U256,
    /// Deposit fee as %, scaled by DEPOSIT_FEE_BASE, such as 500_000_000_000_000 represents 0.05%
    /// fee.
    pub deposit_fee: U256,
    pub deposits_enabled: bool,
    pub min_deposit_amount: U256,
    pub max_deposit_pool_size: U256,
    /// Whether assigning deposits is enabled (allows using queue capacity)
    pub deposit_assigning_enabled: bool,
    /// Maximum number of assignments per deposit
    pub deposit_assign_maximum: U256,
    /// The base number of assignments to try per deposit
    pub deposit_assign_socialised_maximum: U256,
    /// Total ETH requested across express + standard megapool queues
    pub megapool_queue_requested_total: U256,
    /// Target rETH collateral rate (scaled by 1e18, e.g. 0.01e18 = 1%).
    /// On-chain: RocketDAOProtocolSettingsNetwork.getTargetRethCollateralRate()
    pub target_reth_collateral_rate: U256,
}

impl RocketpoolState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reth_supply: U256,
        total_eth: U256,
        deposit_contract_balance: U256,
        reth_contract_liquidity: U256,
        deposit_fee: U256,
        deposits_enabled: bool,
        min_deposit_amount: U256,
        max_deposit_pool_size: U256,
        deposit_assigning_enabled: bool,
        deposit_assign_maximum: U256,
        deposit_assign_socialised_maximum: U256,
        megapool_queue_requested_total: U256,
        target_reth_collateral_rate: U256,
    ) -> Self {
        Self {
            reth_supply,
            total_eth,
            deposit_contract_balance,
            reth_contract_liquidity,
            deposit_fee,
            deposits_enabled,
            min_deposit_amount,
            max_deposit_pool_size,
            deposit_assigning_enabled,
            deposit_assign_maximum,
            deposit_assign_socialised_maximum,
            megapool_queue_requested_total,
            target_reth_collateral_rate,
        }
    }

    /// Calculates rETH amount out for a given ETH deposit amount.
    fn get_reth_value(&self, eth_amount: U256) -> Result<U256, SimulationError> {
        // fee = ethIn * deposit_fee / DEPOSIT_FEE_BASE
        let fee = mul_div(eth_amount, self.deposit_fee, U256::from(DEPOSIT_FEE_BASE))?;
        let net_eth = safe_sub_u256(eth_amount, fee)?;
        // rethOut = netEth * rethSupply / totalEth
        mul_div(net_eth, self.reth_supply, self.total_eth)
    }

    /// Calculates ETH amount out for a given rETH burn amount.
    fn get_eth_value(&self, reth_amount: U256) -> Result<U256, SimulationError> {
        // ethOut = rethIn * totalEth / rethSupply
        mul_div(reth_amount, self.total_eth, self.reth_supply)
    }

    fn is_depositing_eth(token_in: &Bytes) -> bool {
        token_in.as_ref() == ETH_ADDRESS
    }

    fn assert_deposits_enabled(&self) -> Result<(), SimulationError> {
        if !self.deposits_enabled {
            Err(SimulationError::RecoverableError(
                "Deposits are currently disabled in Rocketpool".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    /// Returns the maximum deposit capacity considering both the base pool size
    /// and the megapool queue capacity (if deposit_assigning_enabled).
    ///
    /// Note: we only model megapool queues here. The legacy minipool queues were
    /// fully drained before the Saturn (v1.4) upgrade activated, and deposits into
    /// them are disabled, so their capacity is always zero.
    fn get_max_deposit_capacity(&self) -> Result<U256, SimulationError> {
        if self.deposit_assigning_enabled {
            safe_add_u256(self.max_deposit_pool_size, self.megapool_queue_requested_total)
        } else {
            Ok(self.max_deposit_pool_size)
        }
    }

    /// Returns the excess balance available for withdrawals from the deposit pool.
    ///
    /// ETH requested for queued megapool entries is reserved and cannot be withdrawn.
    /// Only the surplus above queue requested is available.
    fn get_deposit_pool_excess_balance(&self) -> Result<U256, SimulationError> {
        if self.megapool_queue_requested_total >= self.deposit_contract_balance {
            Ok(U256::ZERO)
        } else {
            safe_sub_u256(self.deposit_contract_balance, self.megapool_queue_requested_total)
        }
    }

    /// Returns total available liquidity for withdrawals.
    /// This is the sum of reth_contract_liquidity and the deposit pool excess balance.
    fn get_total_available_for_withdrawal(&self) -> Result<U256, SimulationError> {
        let deposit_pool_excess = self.get_deposit_pool_excess_balance()?;
        safe_add_u256(self.reth_contract_liquidity, deposit_pool_excess)
    }

    /// Computes how `processDeposit()` routes deposited ETH between the rETH contract
    /// (collateral buffer) and the deposit pool vault.
    ///
    /// Returns `(to_reth_contract, to_deposit_vault)`.
    fn compute_deposit_routing(
        &self,
        deposit_amount: U256,
    ) -> Result<(U256, U256), SimulationError> {
        // target_collateral = total_eth * target_reth_collateral_rate / 1e18
        let target_collateral = mul_div(
            self.total_eth,
            self.target_reth_collateral_rate,
            U256::from(DEPOSIT_FEE_BASE),
        )?;
        // Fill the rETH contract up to its target, send the rest to the deposit vault.
        let shortfall = target_collateral.saturating_sub(self.reth_contract_liquidity);
        let to_reth = deposit_amount.min(shortfall);
        let to_vault = deposit_amount - to_reth;
        Ok((to_reth, to_vault))
    }

    /// Calculates ETH assigned from the deposit pool to megapool queue entries.
    ///
    /// Three constraints bound assignment:
    ///   1. Count cap:    floor(deposit / 32 ETH) + socialisedMax, ≤ deposit_assign_maximum
    ///   2. Vault cap:    floor(deposit_contract_balance / 32 ETH)
    ///   3. Queue depth:  floor(megapool_queue_requested_total / 32 ETH)
    ///
    /// The minimum of these three gives the number of entries assigned.
    /// Total ETH assigned = entries × 32 ETH.
    fn calculate_assign_deposits(&self, deposit_amount: U256) -> (U256, u64) {
        if !self.deposit_assigning_enabled ||
            self.megapool_queue_requested_total
                .is_zero()
        {
            return (U256::ZERO, 0);
        }

        let full_deposit_value = U256::from(FULL_DEPOSIT_VALUE);

        // Constraint 1: count cap
        let scaling_count = deposit_amount / full_deposit_value;
        let count_cap = (self.deposit_assign_socialised_maximum + scaling_count)
            .min(self.deposit_assign_maximum);

        // Constraint 2: vault balance
        let vault_cap = self.deposit_contract_balance / full_deposit_value;

        // Constraint 3: queue depth
        let queue_entries = self.megapool_queue_requested_total / full_deposit_value;

        // Entries assigned = min of all three constraints
        let entries = count_cap
            .min(vault_cap)
            .min(queue_entries);

        (entries * full_deposit_value, entries.try_into().unwrap_or(u64::MAX))
    }
}

#[typetag::serde]
impl ProtocolSim for RocketpoolState {
    fn fee(&self) -> f64 {
        unimplemented!("Rocketpool has asymmetric fees; use spot_price or get_amount_out instead")
    }

    fn spot_price(&self, _base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        // As we are computing the amount of quote needed to buy 1 base, we check the quote token.
        let is_depositing_eth = RocketpoolState::is_depositing_eth(&quote.address);

        // As the protocol has no slippage, we can use a fixed amount for spot price calculation.
        // We compute how much base we get for 1 quote, then invert to get quote needed for 1 base.
        let amount = U256::from(1e18);

        let base_per_quote = if is_depositing_eth {
            // base=rETH, quote=ETH: compute rETH for given ETH
            self.assert_deposits_enabled()?;
            self.get_reth_value(amount)?
        } else {
            // base=ETH, quote=rETH: compute ETH for given rETH
            self.get_eth_value(amount)?
        };

        let base_per_quote = u256_to_f64(base_per_quote)? / 1e18;
        // Invert to get how much quote needed to buy 1 base
        Ok(1.0 / base_per_quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        _token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = biguint_to_u256(&amount_in);
        let is_depositing_eth = RocketpoolState::is_depositing_eth(&token_in.address);

        let amount_out = if is_depositing_eth {
            self.assert_deposits_enabled()?;

            if amount_in < self.min_deposit_amount {
                return Err(SimulationError::InvalidInput(
                    format!(
                        "Deposit amount {} is less than the minimum deposit of {}",
                        amount_in, self.min_deposit_amount
                    ),
                    None,
                ));
            }

            let capacity_needed = safe_add_u256(self.deposit_contract_balance, amount_in)?;
            let max_capacity = self.get_max_deposit_capacity()?;
            if capacity_needed > max_capacity {
                return Err(SimulationError::InvalidInput(
                    format!(
                        "Deposit would exceed maximum pool size (capacity needed: {}, max: {})",
                        capacity_needed, max_capacity
                    ),
                    None,
                ));
            }

            self.get_reth_value(amount_in)?
        } else {
            let eth_out = self.get_eth_value(amount_in)?;

            let total_available = self.get_total_available_for_withdrawal()?;
            if eth_out > total_available {
                return Err(SimulationError::RecoverableError(format!(
                    "Withdrawal {} exceeds available liquidity {}",
                    eth_out, total_available
                )));
            }

            eth_out
        };

        let mut new_state = self.clone();
        let mut gas_used = 0;

        // Note: total_eth and reth_supply are not updated as they are managed by an oracle.
        if is_depositing_eth {
            gas_used += BASE_DEPOSIT_GAS;

            // route ETH between rETH collateral buffer and vault.
            let (to_reth, to_vault) = new_state.compute_deposit_routing(amount_in)?;
            new_state.reth_contract_liquidity =
                safe_add_u256(new_state.reth_contract_liquidity, to_reth)?;
            new_state.deposit_contract_balance =
                safe_add_u256(new_state.deposit_contract_balance, to_vault)?;
            if to_reth > U256::ZERO {
                gas_used += RETH_MINT_GAS;
            }

            if to_vault > U256::ZERO {
                gas_used += VAULT_DEPOSIT_OR_EXCESS_GAS;
            }

            // Assign deposits: dequeue megapool entries and send ETH from vault to validators.
            // Both the vault balance and the queue requested are reduced by the same amount.
            let (eth_assigned, entries) = new_state.calculate_assign_deposits(amount_in);

            if entries > 0 {
                gas_used += ASSIGN_DEPOSITS_BASE_GAS;
                gas_used += entries * GAS_PER_MEGAPOOL_ASSIGNMENT;
            }

            if eth_assigned > U256::ZERO {
                new_state.deposit_contract_balance =
                    safe_sub_u256(new_state.deposit_contract_balance, eth_assigned)?;
                new_state.megapool_queue_requested_total =
                    safe_sub_u256(new_state.megapool_queue_requested_total, eth_assigned)?;
            }
        } else {
            gas_used = BASE_WITHDRAWAL_GAS;
            // Withdraw from rETH contract first, spill remainder into deposit pool.
            let needed_from_deposit_pool =
                amount_out.saturating_sub(new_state.reth_contract_liquidity);
            new_state.reth_contract_liquidity = new_state
                .reth_contract_liquidity
                .saturating_sub(amount_out);
            new_state.deposit_contract_balance =
                safe_sub_u256(new_state.deposit_contract_balance, needed_from_deposit_pool)?;
        }

        // The ETH withdrawal gas estimation assumes a best case scenario when there is sufficient
        // liquidity in the rETH contract. Note that there has never been a situation during a
        // withdrawal when this was not the case, hence the simplified gas estimation.

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(gas_used),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        _buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let is_depositing_eth = Self::is_depositing_eth(&sell_token);

        if is_depositing_eth {
            // ETH -> rETH: max sell = max_deposit_capacity - deposit_contract_balance
            let max_capacity = self.get_max_deposit_capacity()?;
            let max_eth_sell = safe_sub_u256(max_capacity, self.deposit_contract_balance)?;
            let max_reth_buy = self.get_reth_value(max_eth_sell)?;
            Ok((u256_to_biguint(max_eth_sell), u256_to_biguint(max_reth_buy)))
        } else {
            // rETH -> ETH: max buy = total available for withdrawal
            let max_eth_buy = self.get_total_available_for_withdrawal()?;
            let max_reth_sell = mul_div(max_eth_buy, self.reth_supply, self.total_eth)?;
            Ok((u256_to_biguint(max_reth_sell), u256_to_biguint(max_eth_buy)))
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.total_eth = delta
            .updated_attributes
            .get("total_eth")
            .map_or(self.total_eth, U256::from_bytes);
        self.reth_supply = delta
            .updated_attributes
            .get("reth_supply")
            .map_or(self.reth_supply, U256::from_bytes);
        self.deposit_contract_balance = delta
            .updated_attributes
            .get("deposit_contract_balance")
            .map_or(self.deposit_contract_balance, U256::from_bytes);
        self.reth_contract_liquidity = delta
            .updated_attributes
            .get("reth_contract_liquidity")
            .map_or(self.reth_contract_liquidity, U256::from_bytes);
        self.deposits_enabled = delta
            .updated_attributes
            .get("deposits_enabled")
            .map_or(self.deposits_enabled, |val| !U256::from_bytes(val).is_zero());
        self.deposit_assigning_enabled = delta
            .updated_attributes
            .get("deposit_assigning_enabled")
            .map_or(self.deposit_assigning_enabled, |val| !U256::from_bytes(val).is_zero());
        self.deposit_fee = delta
            .updated_attributes
            .get("deposit_fee")
            .map_or(self.deposit_fee, U256::from_bytes);
        self.min_deposit_amount = delta
            .updated_attributes
            .get("min_deposit_amount")
            .map_or(self.min_deposit_amount, U256::from_bytes);
        self.max_deposit_pool_size = delta
            .updated_attributes
            .get("max_deposit_pool_size")
            .map_or(self.max_deposit_pool_size, U256::from_bytes);
        self.deposit_assign_maximum = delta
            .updated_attributes
            .get("deposit_assign_maximum")
            .map_or(self.deposit_assign_maximum, U256::from_bytes);
        self.deposit_assign_socialised_maximum = delta
            .updated_attributes
            .get("deposit_assign_socialised_maximum")
            .map_or(self.deposit_assign_socialised_maximum, U256::from_bytes);
        self.megapool_queue_requested_total = delta
            .updated_attributes
            .get("megapool_queue_requested_total")
            .map_or(self.megapool_queue_requested_total, U256::from_bytes);
        self.target_reth_collateral_rate = delta
            .updated_attributes
            .get("target_reth_collateral_rate")
            .map_or(self.target_reth_collateral_rate, U256::from_bytes);

        Ok(())
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        if let Some(other_state) = other.as_any().downcast_ref::<Self>() {
            self == other_state
        } else {
            false
        }
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    };

    use approx::assert_ulps_eq;
    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_common::{
        dto::ProtocolStateDelta,
        hex_bytes::Bytes,
        models::{token::Token, Chain},
        simulation::{
            errors::SimulationError,
            protocol_sim::{Balances, ProtocolSim},
        },
    };

    use super::*;

    /// Helper function to create a RocketpoolState with easy-to-compute defaults for testing.
    /// - Exchange rate: 1 rETH = 2 ETH (100 rETH backed by 200 ETH)
    /// - Deposit fee: 40%
    /// - Deposit contract balance: 50 ETH
    /// - rETH contract liquidity: 0 ETH
    /// - Max pool size: 1000 ETH
    /// - Assign deposits enabled: false
    /// - Target rETH collateral rate: 1% (0.01e18)
    fn create_state() -> RocketpoolState {
        RocketpoolState::new(
            U256::from(100e18), // reth_supply: 100 rETH
            U256::from(200e18), /* total_eth: 200 ETH (1 rETH =
                                 * 2 ETH) */
            U256::from(50e18), // deposit_contract_balance: 50 ETH
            U256::ZERO,        // reth_contract_liquidity: 0 ETH
            U256::from(400_000_000_000_000_000u64), // deposit_fee: 40% (0.4e18)
            true,              // deposits_enabled
            U256::ZERO,        // min_deposit_amount
            U256::from(1000e18), // max_deposit_pool_size: 1000 ETH
            false,             // deposit_assigning_enabled
            U256::ZERO,        // deposit_assign_maximum
            U256::ZERO,        // deposit_assign_socialised_maximum
            U256::ZERO,        // megapool_queue_requested_total
            U256::from(10_000_000_000_000_000u64), // target_reth_collateral_rate: 1%
        )
    }

    fn eth_token() -> Token {
        Token::new(&Bytes::from(ETH_ADDRESS), "ETH", 18, 0, &[Some(100_000)], Chain::Ethereum, 100)
    }

    fn reth_token() -> Token {
        Token::new(
            &Bytes::from_str("0xae78736Cd615f374D3085123A210448E74Fc6393").unwrap(),
            "rETH",
            18,
            0,
            &[Some(100_000)],
            Chain::Ethereum,
            100,
        )
    }

    // ============ Max Deposit Capacity Tests ============

    #[test]
    fn test_max_capacity_assign_disabled() {
        let state = create_state();
        assert_eq!(
            state
                .get_max_deposit_capacity()
                .unwrap(),
            U256::from(1000e18)
        );
    }

    #[test]
    fn test_max_capacity_assign_enabled_empty_queue() {
        let mut state = create_state();
        state.deposit_assigning_enabled = true;
        assert_eq!(
            state
                .get_max_deposit_capacity()
                .unwrap(),
            U256::from(1000e18)
        );
    }

    #[test]
    fn test_max_capacity_assign_enabled_with_queue() {
        let mut state = create_state();
        state.deposit_assigning_enabled = true;
        state.megapool_queue_requested_total = U256::from(500e18);
        // 1000 + 500 = 1500 ETH
        assert_eq!(
            state
                .get_max_deposit_capacity()
                .unwrap(),
            U256::from(1500e18)
        );
    }

    // ============ Deposit Routing Tests ============

    #[rstest]
    #[case::all_to_reth(
        U256::ZERO,  // reth_contract_liquidity
        U256::from(10_000_000_000_000_000u64),  // target_reth_collateral_rate: 1%
        1_000_000_000_000_000_000u128,  // deposit: 1 ETH
        1_000_000_000_000_000_000u128,  // expected to_reth
        0u128,  // expected to_vault
    )]
    // shortfall (2 ETH) < deposit (10 ETH) → split
    #[case::split(
        U256::ZERO,
        U256::from(10_000_000_000_000_000u64),
        10_000_000_000_000_000_000u128,  // deposit: 10 ETH
        2_000_000_000_000_000_000u128,   // to_reth: 2 ETH (shortfall)
        8_000_000_000_000_000_000u128,   // to_vault: 8 ETH
    )]
    // no shortfall (liquidity 10 ETH > target 2 ETH) → all to vault
    #[case::all_to_vault(
        U256::from(10_000_000_000_000_000_000u128),  // liquidity: 10 ETH > 2 ETH target
        U256::from(10_000_000_000_000_000u64),
        5_000_000_000_000_000_000u128,  // deposit: 5 ETH
        0u128,
        5_000_000_000_000_000_000u128,
    )]
    // zero collateral rate → all to vault
    #[case::zero_collateral_rate(
        U256::ZERO,
        U256::ZERO,  // target_reth_collateral_rate: 0%
        5_000_000_000_000_000_000u128,
        0u128,
        5_000_000_000_000_000_000u128,
    )]
    fn test_deposit_routing(
        #[case] reth_contract_liquidity: U256,
        #[case] target_reth_collateral_rate: U256,
        #[case] deposit: u128,
        #[case] expected_to_reth: u128,
        #[case] expected_to_vault: u128,
    ) {
        let mut state = create_state();
        state.reth_contract_liquidity = reth_contract_liquidity;
        state.target_reth_collateral_rate = target_reth_collateral_rate;

        let (to_reth, to_vault) = state
            .compute_deposit_routing(U256::from(deposit))
            .unwrap();

        assert_eq!(to_reth, U256::from(expected_to_reth));
        assert_eq!(to_vault, U256::from(expected_to_vault));
    }

    // ============ Delta Transition Tests ============

    #[test]
    fn test_delta_transition_basic() {
        let mut state = create_state();

        let attributes: HashMap<String, Bytes> = [
            ("total_eth", U256::from(300u64)),
            ("reth_supply", U256::from(150u64)),
            ("deposit_contract_balance", U256::from(100u64)),
            ("reth_contract_liquidity", U256::from(20u64)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Bytes::from(v.to_be_bytes_vec())))
        .collect();

        let delta = ProtocolStateDelta {
            component_id: "Rocketpool".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: HashSet::new(),
        };

        state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(state.total_eth, U256::from(300u64));
        assert_eq!(state.reth_supply, U256::from(150u64));
        assert_eq!(state.deposit_contract_balance, U256::from(100u64));
        assert_eq!(state.reth_contract_liquidity, U256::from(20u64));
    }

    #[test]
    fn test_delta_transition_megapool_fields() {
        let mut state = create_state();

        let attributes: HashMap<String, Bytes> = [
            ("megapool_queue_requested_total", U256::from(1000u64)),
            ("target_reth_collateral_rate", U256::from(20_000_000_000_000_000u64)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Bytes::from(v.to_be_bytes_vec())))
        .collect();

        let delta = ProtocolStateDelta {
            component_id: "Rocketpool".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: HashSet::new(),
        };

        state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(state.megapool_queue_requested_total, U256::from(1000u64));
        assert_eq!(state.target_reth_collateral_rate, U256::from(20_000_000_000_000_000u64));
    }

    // ============ Spot Price Tests ============

    #[test]
    fn test_spot_price_deposit() {
        let state = create_state();
        let price = state
            .spot_price(&eth_token(), &reth_token())
            .unwrap();
        assert_ulps_eq!(price, 0.5);
    }

    #[test]
    fn test_spot_price_withdraw() {
        let state = create_state();
        let price = state
            .spot_price(&reth_token(), &eth_token())
            .unwrap();
        assert_ulps_eq!(price, 1.0 / 0.3);
    }

    /// Validate withdrawal spot price against on-chain getEthValue(1e18) at block 24480104.
    /// On-chain: getEthValue(1e18) = 1157737589816937166 → 1 rETH = 1.1577... ETH.
    /// Our spot_price(ETH, rETH) should return 1/1.1577... = 0.8637... (rETH per ETH).
    #[test]
    fn test_live_spot_price_withdrawal() {
        let state = create_state_at_block_24480104();
        let price = state
            .spot_price(&eth_token(), &reth_token())
            .unwrap();

        // Expected from on-chain getEthValue(1e18) = 1157737589816937166
        let on_chain_eth_value = 1_157_737_589_816_937_166f64;
        let expected = 1e18 / on_chain_eth_value;
        assert_ulps_eq!(price, expected, max_ulps = 10);
    }

    /// Validate deposit spot price against on-chain getRethValue(1e18) at block 24480104.
    /// On-chain: getRethValue(1e18) = 863753590447141981 (no deposit fee).
    /// Our spot_price(rETH, ETH) applies the 0.05% deposit fee on top.
    #[test]
    fn test_live_spot_price_deposit() {
        use crate::evm::protocol::utils::add_fee_markup;

        let state = create_state_at_block_24480104();
        let price = state
            .spot_price(&reth_token(), &eth_token())
            .unwrap();

        // On-chain getRethValue(1e18) = 863753590447141981 (no fee)
        // → exchange rate without fee = 1e18 / 863753590447141981
        let on_chain_reth_value = 863_753_590_447_141_981f64;
        let rate_without_fee = 1e18 / on_chain_reth_value;
        let fee = 500_000_000_000_000f64 / DEPOSIT_FEE_BASE as f64; // 0.05%
        let expected = add_fee_markup(rate_without_fee, fee);
        assert_ulps_eq!(price, expected, max_ulps = 10);
    }

    #[test]
    fn test_fee_panics() {
        let state = create_state();
        let result = std::panic::catch_unwind(|| state.fee());
        assert!(result.is_err());
    }

    // ============ Get Limits Tests ============

    #[test]
    fn test_limits_deposit() {
        let state = create_state();
        let (max_sell, max_buy) = state
            .get_limits(eth_token().address, reth_token().address)
            .unwrap();
        // max_sell = 1000 - 50 = 950 ETH
        assert_eq!(max_sell, BigUint::from(950_000_000_000_000_000_000u128));
        // max_buy = 950 * 0.6 * 100/200 = 285 rETH
        assert_eq!(max_buy, BigUint::from(285_000_000_000_000_000_000u128));
    }

    #[test]
    fn test_limits_withdrawal() {
        let state = create_state();
        let (max_sell, max_buy) = state
            .get_limits(reth_token().address, eth_token().address)
            .unwrap();
        // max_buy = liquidity = 50 ETH
        assert_eq!(max_buy, BigUint::from(50_000_000_000_000_000_000u128));
        // max_sell = 50 * 100/200 = 25 rETH
        assert_eq!(max_sell, BigUint::from(25_000_000_000_000_000_000u128));
    }

    #[test]
    fn test_limits_with_megapool_queue() {
        let mut state = create_state();
        state.max_deposit_pool_size = U256::from(100e18);
        state.deposit_assigning_enabled = true;
        state.megapool_queue_requested_total = U256::from(62e18);

        let (max_sell, _) = state
            .get_limits(eth_token().address, reth_token().address)
            .unwrap();
        // max_capacity = 100 + 62 = 162 ETH; max_sell = 162 - 50 = 112 ETH
        assert_eq!(max_sell, BigUint::from(112_000_000_000_000_000_000u128));
    }

    // ============ Limits Boundary Consistency Tests ============

    #[test]
    fn test_limits_deposit_boundary_accepted() {
        let mut state = create_state();
        state.deposit_assigning_enabled = true;
        state.megapool_queue_requested_total = U256::from(64e18);

        let (max_sell, _) = state
            .get_limits(eth_token().address, reth_token().address)
            .unwrap();

        // Depositing exactly max_sell should succeed
        let res = state.get_amount_out(max_sell.clone(), &eth_token(), &reth_token());
        assert!(res.is_ok(), "max_sell should be accepted");

        // Depositing max_sell + 1 wei should fail
        let over = max_sell + BigUint::from(1u64);
        let res = state.get_amount_out(over, &eth_token(), &reth_token());
        assert!(matches!(res, Err(SimulationError::InvalidInput(_, _))));
    }

    #[test]
    fn test_limits_withdrawal_boundary_accepted() {
        let mut state = create_state();
        state.reth_contract_liquidity = U256::from(30e18);

        let (max_sell, _) = state
            .get_limits(reth_token().address, eth_token().address)
            .unwrap();

        // Withdrawing exactly max_sell should succeed
        let res = state.get_amount_out(max_sell.clone(), &reth_token(), &eth_token());
        assert!(res.is_ok(), "max_sell should be accepted");

        // Withdrawing max_sell + 1 wei should fail
        let over = max_sell + BigUint::from(1u64);
        let res = state.get_amount_out(over, &reth_token(), &eth_token());
        assert!(matches!(res, Err(SimulationError::RecoverableError(_))));
    }

    // ============ Get Amount Out - Happy Path Tests ============

    #[test]
    fn test_deposit_eth() {
        let state = create_state();
        // Deposit 10 ETH: fee=4, net=6 → 6*100/200 = 3 rETH
        let res = state
            .get_amount_out(
                BigUint::from(10_000_000_000_000_000_000u128),
                &eth_token(),
                &reth_token(),
            )
            .unwrap();

        assert_eq!(res.amount, BigUint::from(3_000_000_000_000_000_000u128));

        // Collateral routing: target = 200e18 * 1e16 / 1e18 = 2e18.
        // Shortfall = 2e18 - 0 = 2e18. to_reth = min(10, 2) = 2, to_vault = 8.
        // deposit_contract_balance = 50 + 8 = 58 ETH
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<RocketpoolState>()
            .unwrap();
        assert_eq!(new_state.deposit_contract_balance, U256::from(58e18));
        assert_eq!(new_state.reth_contract_liquidity, U256::from(2e18));
    }

    #[test]
    fn test_withdraw_reth() {
        let state = create_state();
        // Withdraw 10 rETH: 10*200/100 = 20 ETH
        let res = state
            .get_amount_out(
                BigUint::from(10_000_000_000_000_000_000u128),
                &reth_token(),
                &eth_token(),
            )
            .unwrap();

        assert_eq!(res.amount, BigUint::from(20_000_000_000_000_000_000u128));

        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<RocketpoolState>()
            .unwrap();
        assert_eq!(new_state.deposit_contract_balance, U256::from(30e18));
    }

    // ============ Get Amount Out - Error Cases Tests ============

    #[test]
    fn test_deposit_disabled() {
        let mut state = create_state();
        state.deposits_enabled = false;
        let res = state.get_amount_out(BigUint::from(10u64), &eth_token(), &reth_token());
        assert!(matches!(res, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn test_deposit_below_minimum() {
        let mut state = create_state();
        state.min_deposit_amount = U256::from(100u64);
        let res = state.get_amount_out(BigUint::from(50u64), &eth_token(), &reth_token());
        assert!(matches!(res, Err(SimulationError::InvalidInput(_, _))));
    }

    #[test]
    fn test_deposit_exceeds_max_pool() {
        let mut state = create_state();
        state.max_deposit_pool_size = U256::from(60e18);
        let res = state.get_amount_out(
            BigUint::from(20_000_000_000_000_000_000u128),
            &eth_token(),
            &reth_token(),
        );
        assert!(matches!(res, Err(SimulationError::InvalidInput(_, _))));
    }

    #[test]
    fn test_withdrawal_insufficient_liquidity() {
        let state = create_state();
        let res = state.get_amount_out(
            BigUint::from(30_000_000_000_000_000_000u128),
            &reth_token(),
            &eth_token(),
        );
        assert!(matches!(res, Err(SimulationError::RecoverableError(_))));
    }

    #[test]
    fn test_withdrawal_limited_by_queue() {
        let mut state = create_state();
        state.deposit_contract_balance = U256::from(100e18);
        state.megapool_queue_requested_total = U256::from(62e18);

        // excess = 100 - 62 = 38 ETH; try withdraw 20 rETH = 40 ETH > 38
        let res = state.get_amount_out(
            BigUint::from(20_000_000_000_000_000_000u128),
            &reth_token(),
            &eth_token(),
        );
        assert!(matches!(res, Err(SimulationError::RecoverableError(_))));

        // Withdraw 15 rETH = 30 ETH <= 38 should work
        let res = state
            .get_amount_out(
                BigUint::from(15_000_000_000_000_000_000u128),
                &reth_token(),
                &eth_token(),
            )
            .unwrap();
        assert_eq!(res.amount, BigUint::from(30_000_000_000_000_000_000u128));
    }

    #[test]
    fn test_withdrawal_uses_both_pools() {
        let mut state = create_state();
        state.reth_contract_liquidity = U256::from(10e18);
        state.deposit_contract_balance = U256::from(50e18);

        // Withdraw 15 rETH = 30 ETH (more than reth_contract_liquidity of 10 ETH)
        let res = state
            .get_amount_out(
                BigUint::from(15_000_000_000_000_000_000u128),
                &reth_token(),
                &eth_token(),
            )
            .unwrap();

        assert_eq!(res.amount, BigUint::from(30_000_000_000_000_000_000u128));

        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<RocketpoolState>()
            .unwrap();
        assert_eq!(new_state.reth_contract_liquidity, U256::ZERO);
        assert_eq!(new_state.deposit_contract_balance, U256::from(30e18));
    }

    // ============ Assign Deposits Tests ============

    #[test]
    fn test_assign_deposits_with_queue() {
        let mut state = create_state();
        state.deposit_contract_balance = U256::from(200e18);
        state.reth_contract_liquidity = U256::from(10e18); // above 1% target, no shortfall
        state.deposit_assigning_enabled = true;
        state.deposit_assign_maximum = U256::from(90u64);
        state.megapool_queue_requested_total = U256::from(128e18); // 4 entries of 32 ETH

        // Deposit 100 ETH
        let res = state
            .get_amount_out(
                BigUint::from(100_000_000_000_000_000_000u128),
                &eth_token(),
                &reth_token(),
            )
            .unwrap();

        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<RocketpoolState>()
            .unwrap();
        // Collateral routing: target = 200e18 * 0.01 = 2e18.
        // Shortfall = 2e18 - 10e18 = 0 (already above target). to_reth=0, to_vault=100.
        // vault after routing = 200 + 100 = 300 ETH.
        //
        // Assignment: scaling_count = 100/32 = 3.
        // count_cap = min(0 + 3, 90) = 3.
        // vault_cap = 300/32 = 9. queue_entries = 128/32 = 4.
        // entries = min(3, 9, 4) = 3. assigned = 3 * 32 = 96 ETH.
        // vault after = 300 - 96 = 204. queue after = 128 - 96 = 32.
        assert_eq!(new_state.deposit_contract_balance, U256::from(204e18));
        assert_eq!(new_state.megapool_queue_requested_total, U256::from(32e18));
    }

    #[test]
    fn test_assign_deposits_empty_queue() {
        let mut state = create_state();
        state.deposit_assigning_enabled = true;

        let res = state
            .get_amount_out(
                BigUint::from(10_000_000_000_000_000_000u128),
                &eth_token(),
                &reth_token(),
            )
            .unwrap();

        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<RocketpoolState>()
            .unwrap();
        // No queue, no assignment. Routing: to_reth=2, to_vault=8. Balance = 50+8 = 58.
        assert_eq!(new_state.deposit_contract_balance, U256::from(58e18));
    }

    #[test]
    fn test_assign_deposits_disabled() {
        let mut state = create_state();
        state.deposit_assigning_enabled = false;
        state.megapool_queue_requested_total = U256::from(100e18);

        let res = state
            .get_amount_out(
                BigUint::from(10_000_000_000_000_000_000u128),
                &eth_token(),
                &reth_token(),
            )
            .unwrap();

        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<RocketpoolState>()
            .unwrap();
        // Assign disabled, no assignment. Routing: to_reth=2, to_vault=8. Balance = 50+8 = 58.
        assert_eq!(new_state.deposit_contract_balance, U256::from(58e18));
    }

    /// Full flow: deposit splits between collateral buffer and vault, then assignment drains vault.
    /// Exercises the combined routing + assignment path that is unique to Saturn v1.4.
    ///
    /// Setup: shortfall = 20 ETH, deposit = 100 ETH, vault = 50 ETH, queue = 96 ETH (3 entries).
    /// Routing: to_reth = 20 (fills shortfall), to_vault = 80. Vault after = 50 + 80 = 130 ETH.
    /// Assignment: scaling_count = 100/32 = 3, count_cap = min(3, 90) = 3.
    ///   vault_cap = 130/32 = 4. queue = 96/32 = 3. entries = min(3, 4, 3) = 3. assigned = 96 ETH.
    /// Final: vault = 130 - 96 = 34, queue = 96 - 96 = 0, reth_liq = 0 + 20 = 20.
    #[test]
    fn test_deposit_split_routing_with_assignment() {
        let mut state = create_state();
        state.reth_contract_liquidity = U256::ZERO;
        state.deposit_contract_balance = U256::from(50e18);
        state.deposit_assigning_enabled = true;
        state.deposit_assign_maximum = U256::from(90u64);
        state.megapool_queue_requested_total = U256::from(96e18); // 3 entries
                                                                  // target = 200e18 * 10% / 1e18 = 20 ETH → shortfall = 20 - 0 = 20
        state.target_reth_collateral_rate = U256::from(100_000_000_000_000_000u64); // 10%

        let res = state
            .get_amount_out(
                BigUint::from(100_000_000_000_000_000_000u128),
                &eth_token(),
                &reth_token(),
            )
            .unwrap();

        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<RocketpoolState>()
            .unwrap();
        assert_eq!(new_state.reth_contract_liquidity, U256::from(20e18));
        assert_eq!(new_state.deposit_contract_balance, U256::from(34e18));
        assert_eq!(new_state.megapool_queue_requested_total, U256::ZERO);
    }

    // ============ Assign Deposits — Constraint Unit Tests ============

    /// Helper: create a state pre-configured for assignment tests.
    /// Deposits enabled, high max, collateral already met (no routing interference).
    fn create_assign_state(
        deposit_contract_balance: U256,
        megapool_queue_requested_total: U256,
        deposit_assign_maximum: U256,
        deposit_assign_socialised_maximum: U256,
    ) -> RocketpoolState {
        let mut state = create_state();
        state.deposit_assigning_enabled = true;
        state.deposit_contract_balance = deposit_contract_balance;
        state.megapool_queue_requested_total = megapool_queue_requested_total;
        state.deposit_assign_maximum = deposit_assign_maximum;
        state.deposit_assign_socialised_maximum = deposit_assign_socialised_maximum;
        // Put liquidity above target so routing sends everything to vault
        state.reth_contract_liquidity = U256::from(10_000_000_000_000_000_000u128);
        state
    }

    #[rstest]
    // Count cap limits: deposit=64 ETH (2 entries), vault=300, queue=128 (4), max=90
    // count_cap = min(0 + 2, 90) = 2; vault_cap = 9; queue = 4 → 2 entries
    #[case::count_cap_limits(
        64_000_000_000_000_000_000u128,   // deposit
        300_000_000_000_000_000_000u128,  // vault
        128_000_000_000_000_000_000u128,  // queue
        90u64,                            // max
        0u64,                             // socialised
        64_000_000_000_000_000_000u128,   // expected: 2 * 32 ETH
    )]
    // Vault cap limits: deposit=200 ETH (6 entries), vault=64 (2), queue=192 (6), max=90
    // count_cap = 6; vault_cap = 2; queue = 6 → 2 entries
    #[case::vault_cap_limits(
        200_000_000_000_000_000_000u128,
        64_000_000_000_000_000_000u128,
        192_000_000_000_000_000_000u128,
        90u64,
        0u64,
        64_000_000_000_000_000_000u128,   // 2 * 32 ETH
    )]
    // Queue depth limits: deposit=200 ETH (6), vault=300 (9), queue=64 (2), max=90
    // count_cap = 6; vault_cap = 9; queue = 2 → 2 entries
    #[case::queue_depth_limits(
        200_000_000_000_000_000_000u128,
        300_000_000_000_000_000_000u128,
        64_000_000_000_000_000_000u128,
        90u64,
        0u64,
        64_000_000_000_000_000_000u128,   // 2 * 32 ETH
    )]
    // Max cap limits: deposit=200 ETH (6), vault=300 (9), queue=192 (6), max=3
    // count_cap = min(6, 3) = 3; vault_cap = 9; queue = 6 → 3 entries
    #[case::max_cap_limits(
        200_000_000_000_000_000_000u128,
        300_000_000_000_000_000_000u128,
        192_000_000_000_000_000_000u128,
        3u64,
        0u64,
        96_000_000_000_000_000_000u128,   // 3 * 32 ETH
    )]
    // Socialised max: deposit=10 ETH (<32, scaling=0), socialised=2, vault=200, queue=128, max=90
    // count_cap = min(0 + 2, 90) = 2; vault_cap = 6; queue = 4 → 2 entries
    #[case::socialised_max(
        10_000_000_000_000_000_000u128,
        200_000_000_000_000_000_000u128,
        128_000_000_000_000_000_000u128,
        90u64,
        2u64,
        64_000_000_000_000_000_000u128,   // 2 * 32 ETH
    )]
    // Small deposit, no socialised: deposit=10 ETH, socialised=0 → scaling=0, count_cap=0
    #[case::small_deposit_no_assignment(
        10_000_000_000_000_000_000u128,
        200_000_000_000_000_000_000u128,
        128_000_000_000_000_000_000u128,
        90u64,
        0u64,
        0u128
    )]
    // Vault below 32 ETH: deposit=100 ETH, vault=20, queue=128, max=90
    // vault_cap = 20/32 = 0 → 0 entries
    #[case::vault_below_32(
        100_000_000_000_000_000_000u128,
        20_000_000_000_000_000_000u128,
        128_000_000_000_000_000_000u128,
        90u64,
        0u64,
        0u128
    )]
    fn test_assign_constraint(
        #[case] deposit: u128,
        #[case] vault: u128,
        #[case] queue: u128,
        #[case] max: u64,
        #[case] socialised: u64,
        #[case] expected_assigned: u128,
    ) {
        let state = create_assign_state(
            U256::from(vault),
            U256::from(queue),
            U256::from(max),
            U256::from(socialised),
        );
        let (assigned, _gas_used) = state.calculate_assign_deposits(U256::from(deposit));
        assert_eq!(assigned, U256::from(expected_assigned));
    }

    // ============ Live Post-Saturn Transaction Tests ============

    /// State at block 24480104 (just before first post-Saturn deposit).
    /// Verified against on-chain data via cast calls at this block.
    fn create_state_at_block_24480104() -> RocketpoolState {
        RocketpoolState::new(
            U256::from_str_radix("489a96a246a2e92bbbd1", 16).unwrap(), // reth_supply
            U256::from_str_radix("540e645ee4119f4d8b9e", 16).unwrap(), // total_eth
            U256::from_str_radix("8dcfa9d0071987bb", 16).unwrap(),     // deposit_contract_balance
            U256::from_str_radix("c28d2e1d64f99ea24", 16).unwrap(),    // reth_contract_liquidity
            U256::from_str_radix("1c6bf52634000", 16).unwrap(),        // deposit_fee (0.05%)
            true,                                                      // deposits_enabled
            U256::from_str_radix("2386f26fc10000", 16).unwrap(),       // min_deposit_amount
            U256::from_str_radix("4f68ca6d8cd91c6000000", 16).unwrap(), // max_deposit_pool_size
            true,                                                      // deposit_assigning_enabled
            U256::from(90u64),                                         // deposit_assign_maximum
            U256::ZERO, // deposit_assign_socialised_maximum
            U256::from_str_radix("4a60532ad51bf000000", 16).unwrap(), /* megapool_queue_requested_total */
            U256::from(10_000_000_000_000_000u64), // target_reth_collateral_rate: 1%
        )
    }

    /// Test against real v1.4 deposit transaction.
    /// Tx 0xe0f1db165b621cb1e50b629af9d47e064be464fbcc7f2bcba3df1d27dbb916be at block 24480105.
    /// User deposited 85 ETH and received 73382345660413064855 rETH (0.05% fee applied).
    ///
    /// On-chain, the 85 ETH went entirely to the rETH collateral buffer because
    /// the collateral shortfall (target ~3965 ETH vs ~224 ETH held) far exceeds 85 ETH.
    /// The vault balance (deposit_contract_balance) was unchanged at ~10.22 ETH,
    /// which is < 32 ETH so no queue entries were assigned.
    #[test]
    fn test_live_deposit_post_saturn() {
        let state = create_state_at_block_24480104();

        let deposit_amount = BigUint::from(85_000_000_000_000_000_000u128);
        let res = state
            .get_amount_out(deposit_amount, &eth_token(), &reth_token())
            .unwrap();

        // Output amount: exact match with on-chain result
        let expected_reth_out = BigUint::from(73_382_345_660_413_064_855u128);
        assert_eq!(res.amount, expected_reth_out);

        // Post-state: now matches on-chain behavior.
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<RocketpoolState>()
            .unwrap();
        assert_eq!(new_state.total_eth, state.total_eth);
        assert_eq!(new_state.reth_supply, state.reth_supply);
        // deposit_contract_balance unchanged — all 85 ETH went to rETH collateral buffer
        assert_eq!(new_state.deposit_contract_balance, state.deposit_contract_balance);
        // rETH contract liquidity increased by the full 85 ETH
        assert_eq!(
            new_state.reth_contract_liquidity,
            safe_add_u256(
                state.reth_contract_liquidity,
                U256::from(85_000_000_000_000_000_000u128)
            )
            .unwrap()
        );
        // Queue unchanged — vault had < 32 ETH, no assignment possible
        assert_eq!(new_state.megapool_queue_requested_total, state.megapool_queue_requested_total);
    }

    /// Test against real v1.4 burn transaction.
    /// Tx 0x6e70e11475c158ca5f88a6d370dfef90eee6d696dd569c3eaab7d244c7b4a20f at block 24481338.
    /// User burned 2515686112138065226 rETH and received 2912504376202664754 ETH.
    ///
    /// Reuses `create_state_at_block_24480104` (~1200 blocks before the burn) because:
    ///   - `total_eth` and `reth_supply` are oracle-reported values (from rocketNetworkBalances)
    ///     updated only by oracle submissions (daily). Verified identical at both blocks:
    ///     getTotalETHBalance() = 396944271446898073504670, getTotalRETHSupply() =
    ///     342862039669683153255377.
    ///   - The burn amount (2.91 ETH) is sourced entirely from `reth_contract_liquidity` (224 ETH),
    ///     so `deposit_contract_balance` and `megapool_queue_requested_total` are not involved.
    #[test]
    fn test_live_burn_post_saturn() {
        let state = create_state_at_block_24480104();

        let burn_amount = BigUint::from(2_515_686_112_138_065_226u128);
        let res = state
            .get_amount_out(burn_amount, &reth_token(), &eth_token())
            .unwrap();

        // Output amount: exact match with on-chain getEthValue(burnAmount) at block 24481337
        let expected_eth_out = BigUint::from(2_912_504_376_202_664_754u128);
        assert_eq!(res.amount, expected_eth_out);

        // Post-state: verify oracle values unchanged and liquidity decreased
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<RocketpoolState>()
            .unwrap();
        assert_eq!(new_state.total_eth, state.total_eth);
        assert_eq!(new_state.reth_supply, state.reth_supply);
        // 2.91 ETH withdrawn entirely from reth_contract_liquidity (224 ETH available)
        assert_eq!(
            new_state.reth_contract_liquidity,
            safe_sub_u256(state.reth_contract_liquidity, U256::from(2_912_504_376_202_664_754u128))
                .unwrap()
        );
        assert_eq!(new_state.deposit_contract_balance, state.deposit_contract_balance);
    }
}
