use std::{any::Any, collections::HashMap, fmt::Debug};

use alloy::primitives::U256;
use num_bigint::{BigUint, ToBigUint};
use serde::{Deserialize, Serialize};
use tracing::trace;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        erc4626::vm,
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
    },
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ERC4626State {
    pool_address: Bytes,
    asset_token: Token,
    share_token: Token,
    asset_price: U256,
    share_price: U256,
    max_deposit: U256,
    max_redeem: U256,
}

impl ERC4626State {
    pub fn new(
        pool_address: &Bytes,
        asset_token: &Token,
        share_token: &Token,
        asset_price: U256,
        share_price: U256,
        max_deposit: U256,
        max_redeem: U256,
    ) -> Self {
        Self {
            pool_address: pool_address.clone(),
            asset_token: asset_token.clone(),
            share_token: share_token.clone(),
            asset_price,
            share_price,
            max_deposit,
            max_redeem,
        }
    }
}

#[typetag::serde]
impl ProtocolSim for ERC4626State {
    fn fee(&self) -> f64 {
        0f64
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let share_unit = U256::from(10).pow(U256::from(self.share_token.decimals));
        let asset_unit = U256::from(10).pow(U256::from(self.asset_token.decimals));

        let one_share_in_asset = u256_to_f64(self.share_price)? / u256_to_f64(asset_unit)?;
        let one_asset_in_share = u256_to_f64(self.asset_price)? / u256_to_f64(share_unit)?;

        if base.address == self.share_token.address && quote.address == self.asset_token.address {
            return Ok(one_share_in_asset); // 1 share → asset
        }

        if base.address == self.asset_token.address && quote.address == self.share_token.address {
            return Ok(one_asset_in_share); // 1 asset → share
        }

        Err(SimulationError::FatalError("invalid pair".into()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = biguint_to_u256(&amount_in);
        if token_in.address == self.asset_token.address &&
            token_out.address == self.share_token.address
        {
            // asset → share: this corresponds to an ERC4626.deposit operation.
            // The user deposits underlying assets and receives vault shares.
            Ok(GetAmountOutResult {
                amount: u256_to_biguint(
                    amount_in * self.asset_price /
                        U256::from(10).pow(U256::from(self.asset_token.decimals)),
                ),
                gas: 297000.to_biguint().expect("infallible"),
                new_state: self.clone_box(),
            })
        } else if token_in.address == self.share_token.address &&
            token_out.address == self.asset_token.address
        {
            // share → asset: this corresponds to an ERC4626.redeem operation.
            // The user burns vault shares and receives underlying assets.
            Ok(GetAmountOutResult {
                amount: u256_to_biguint(
                    amount_in * self.share_price /
                        U256::from(10).pow(U256::from(self.share_token.decimals)),
                ),
                gas: 287000.to_biguint().expect("infallible"),
                new_state: self.clone_box(),
            })
        } else {
            Err(SimulationError::FatalError(format!(
                "Invalid token pair: {}, {}",
                token_in.address, token_out.address
            )))
        }
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let share_scale = BigUint::from(10u32).pow(self.share_token.decimals);
        let asset_scale = BigUint::from(10u32).pow(self.asset_token.decimals);

        if sell_token == self.share_token.address && buy_token == self.asset_token.address {
            // asset_out_raw = shares_raw * share_price_raw / 10^share_decimals
            let buy_raw = (&u256_to_biguint(self.max_redeem) * &u256_to_biguint(self.share_price)) /
                &share_scale;
            return Ok((u256_to_biguint(self.max_redeem), buy_raw));
        }

        if sell_token == self.asset_token.address && buy_token == self.share_token.address {
            // share_out_raw = asset_raw * asset_price_raw / 10^asset_decimals
            let buy_raw = (&u256_to_biguint(self.max_deposit) * &u256_to_biguint(self.asset_price)) /
                &asset_scale;

            return Ok((u256_to_biguint(self.max_deposit), buy_raw));
        }

        Err(SimulationError::FatalError(format!(
            "Invalid token pair: {}, {}",
            sell_token, buy_token
        )))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let engine =
            create_engine(SHARED_TYCHO_DB.clone(), false).expect("Failed to create engine");

        let state =
            vm::decode_from_vm(&self.pool_address, &self.asset_token, &self.share_token, engine)?;
        trace!(?state, "Calling delta transition for {}", &self.pool_address);

        self.asset_price = state.asset_price;
        self.share_price = state.share_price;
        self.max_deposit = state.max_deposit;
        self.max_redeem = state.max_redeem;
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
        if let Some(other_state) = other
            .as_any()
            .downcast_ref::<ERC4626State>()
        {
            self.pool_address == other_state.pool_address &&
                self.asset_token == other_state.asset_token &&
                self.share_token == other_state.share_token &&
                self.asset_price == other_state.asset_price &&
                self.share_price == other_state.share_price &&
                self.max_deposit == other_state.max_deposit &&
                self.max_redeem == other_state.max_redeem
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
