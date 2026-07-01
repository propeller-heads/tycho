use std::{any::Any, collections::HashMap, fmt::Debug, sync::Arc};

use alloy::primitives::U256;
use num_bigint::{BigUint, ToBigUint};
use serde::{Deserialize, Serialize};
use tracing::trace;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::ProtocolComponent, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
        swap::{
            LimitsParams, MarginalPrice, MarginalPriceParams, QuerySwapParams, Quote, QuoteAmount,
            QuoteParams, Range, SimulationResult, Swap, SwapFee, SwapLimits, SwapQuoter,
            Transition, TransitionParams,
        },
    },
    Bytes,
};

use crate::evm::{
    engine_db::{create_engine, SHARED_TYCHO_DB},
    protocol::{
        erc4626::vm,
        u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64},
        utils::solidity_math::mul_div,
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ERC4626State {
    pool_address: Bytes,
    asset_token: Token,
    share_token: Token,
    asset_price: U256,
    share_price: U256,
    max_deposit: U256,
    max_redeem: U256,
    #[serde(skip)]
    component: Option<Arc<ProtocolComponent<Arc<Token>>>>,
}

impl PartialEq for ERC4626State {
    fn eq(&self, other: &Self) -> bool {
        self.pool_address == other.pool_address &&
            self.asset_token == other.asset_token &&
            self.share_token == other.share_token &&
            self.asset_price == other.asset_price &&
            self.share_price == other.share_price &&
            self.max_deposit == other.max_deposit &&
            self.max_redeem == other.max_redeem
    }
}

impl Eq for ERC4626State {}

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
            component: None,
        }
    }

    /// Attaches the `SwapQuoter` component (carrying the pool's `Arc<Token>`s) to this state.
    pub fn with_component(mut self, component: Arc<ProtocolComponent<Arc<Token>>>) -> Self {
        self.component = Some(component);
        self
    }

    /// `U256`-native swap core shared by `get_amount_out` and `SwapQuoter::quote`. Returns
    /// `(amount_out, gas)`. The vault price is unaffected by a swap, so no new state is produced.
    fn amount_out_u256(
        &self,
        amount_in: U256,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(U256, u64), SimulationError> {
        if token_in == &self.asset_token.address && token_out == &self.share_token.address {
            // asset → share: ERC4626.deposit.
            let amount_out = amount_in * self.asset_price /
                U256::from(10).pow(U256::from(self.asset_token.decimals));
            Ok((amount_out, 297_000))
        } else if token_in == &self.share_token.address && token_out == &self.asset_token.address {
            // share → asset: ERC4626.redeem.
            let amount_out = amount_in * self.share_price /
                U256::from(10).pow(U256::from(self.share_token.decimals));
            Ok((amount_out, 287_000))
        } else {
            Err(SimulationError::FatalError(format!(
                "Invalid token pair: {}, {}",
                token_in, token_out
            )))
        }
    }

    /// `U256`-native limits core shared by `get_limits` and `SwapQuoter::swap_limits`.
    fn limits_u256(
        &self,
        sell_token: &Bytes,
        buy_token: &Bytes,
    ) -> Result<(U256, U256), SimulationError> {
        if sell_token == &self.share_token.address && buy_token == &self.asset_token.address {
            let buy_raw = mul_div(
                self.max_redeem,
                self.share_price,
                U256::from(10).pow(U256::from(self.share_token.decimals)),
            )?;
            Ok((self.max_redeem, buy_raw))
        } else if sell_token == &self.asset_token.address && buy_token == &self.share_token.address
        {
            let buy_raw = mul_div(
                self.max_deposit,
                self.asset_price,
                U256::from(10).pow(U256::from(self.asset_token.decimals)),
            )?;
            Ok((self.max_deposit, buy_raw))
        } else {
            Err(SimulationError::FatalError(format!(
                "Invalid token pair: {}, {}",
                sell_token, buy_token
            )))
        }
    }

    /// Refreshes the cached vault prices and limits from the shared VM database. Shared by the
    /// `ProtocolSim` and `SwapQuoter` transition methods (the delta is read from the VM, not the
    /// event payload).
    fn refresh_from_vm(&mut self) -> Result<(), TransitionError> {
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
        let (amount_out, gas) =
            self.amount_out_u256(amount_in, &token_in.address, &token_out.address)?;
        Ok(GetAmountOutResult {
            amount: u256_to_biguint(amount_out),
            gas: gas.to_biguint().expect("infallible"),
            new_state: ProtocolSim::clone_box(self),
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let (max_in, max_out) = self.limits_u256(&sell_token, &buy_token)?;
        Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.refresh_from_vm()
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

#[typetag::serde]
impl SwapQuoter for ERC4626State {
    fn component(&self) -> SimulationResult<Arc<ProtocolComponent<Arc<Token>>>> {
        self.component.clone().ok_or_else(|| {
            SimulationError::FatalError(
                "ERC4626State: component not set (decode did not populate it)".to_string(),
            )
        })
    }

    fn fee(&self, _params: QuoteParams) -> SimulationResult<SwapFee> {
        Ok(SwapFee::new(0f64))
    }

    fn marginal_price(&self, params: MarginalPriceParams) -> SimulationResult<MarginalPrice> {
        let component = self.component.as_ref().ok_or_else(|| {
            SimulationError::FatalError("ERC4626State: component not set".to_string())
        })?;
        let base = component
            .get_token(params.token_in())
            .ok_or_else(|| SimulationError::FatalError("token_in not in component".to_string()))?;
        let quote = component
            .get_token(params.token_out())
            .ok_or_else(|| SimulationError::FatalError("token_out not in component".to_string()))?;

        let share_unit = U256::from(10).pow(U256::from(self.share_token.decimals));
        let asset_unit = U256::from(10).pow(U256::from(self.asset_token.decimals));

        let one_share_in_asset = u256_to_f64(self.share_price)? / u256_to_f64(asset_unit)?;
        let one_asset_in_share = u256_to_f64(self.asset_price)? / u256_to_f64(share_unit)?;

        if base.address == self.share_token.address && quote.address == self.asset_token.address {
            return Ok(MarginalPrice::new(one_share_in_asset));
        }

        if base.address == self.asset_token.address && quote.address == self.share_token.address {
            return Ok(MarginalPrice::new(one_asset_in_share));
        }

        Err(SimulationError::FatalError("invalid pair".into()))
    }

    fn quote(&self, params: QuoteParams) -> SimulationResult<Quote> {
        let amount_in = match params.amount() {
            QuoteAmount::FixedIn(amount) => *amount,
            QuoteAmount::FixedOut(_) => {
                return Err(SimulationError::RecoverableError(
                    "ERC4626State does not yet support exact-out (FixedOut) quoting".to_string(),
                ))
            }
        };

        let (amount_out, gas) =
            self.amount_out_u256(amount_in, params.token_in(), params.token_out())?;
        let new_state = if params.should_return_new_state() {
            Some(Arc::new(self.clone()) as Arc<dyn SwapQuoter>)
        } else {
            None
        };
        Ok(Quote::new(amount_out, gas, new_state))
    }

    fn swap_limits(&self, params: LimitsParams) -> SimulationResult<SwapLimits> {
        let (max_in, max_out) = self.limits_u256(params.token_in(), params.token_out())?;
        Ok(SwapLimits::new(Range::new(U256::ZERO, max_in)?, Range::new(U256::ZERO, max_out)?))
    }

    fn query_swap(&self, _params: QuerySwapParams) -> SimulationResult<Swap> {
        Err(SimulationError::FatalError(
            "ERC4626State::query_swap is not yet wired (pending token plumbing)".to_string(),
        ))
    }

    fn delta_transition(
        &mut self,
        _params: TransitionParams,
    ) -> Result<Transition, TransitionError> {
        self.refresh_from_vm()?;
        Ok(Transition::default())
    }

    fn clone_box(&self) -> Box<dyn SwapQuoter> {
        Box::new(self.clone())
    }

    #[allow(deprecated)]
    fn to_protocol_sim(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;
    use tycho_common::models::Chain;

    use super::*;

    fn asset_token() -> Token {
        Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "ASSET",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn share_token() -> Token {
        Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap(),
            "SHARE",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn sample_state() -> ERC4626State {
        ERC4626State::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000003").unwrap(),
            &asset_token(),
            &share_token(),
            U256::from_str("1050000000000000000").unwrap(),
            U256::from_str("952380952380952380").unwrap(),
            U256::from_str("1000000000000000000000000").unwrap(),
            U256::from_str("500000000000000000000000").unwrap(),
        )
    }

    #[rstest]
    #[case::deposit(true)]
    #[case::redeem(false)]
    fn test_swap_quoter_matches_protocol_sim(#[case] deposit: bool) {
        let state = sample_state();
        let (token_in, token_out) =
            if deposit { (asset_token(), share_token()) } else { (share_token(), asset_token()) };

        for amount in ["1000000000000000000", "777000000000000000000"] {
            let amount = BigUint::from_str(amount).unwrap();

            let legacy = state
                .get_amount_out(amount.clone(), &token_in, &token_out)
                .unwrap();
            let quote = state
                .quote(
                    QuoteParams::fixed_in(
                        &token_in.address,
                        &token_out.address,
                        biguint_to_u256(&amount),
                    )
                    .unwrap()
                    .with_new_state(),
                )
                .unwrap();

            assert_eq!(u256_to_biguint(quote.amount_out()), legacy.amount);
            assert_eq!(BigUint::from(quote.gas()), legacy.gas);
            assert!(quote.new_state().is_some());
        }

        let (legacy_in, legacy_out) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .unwrap();
        let limits = state
            .swap_limits(LimitsParams::new(&token_in.address, &token_out.address))
            .unwrap();
        assert_eq!(u256_to_biguint(limits.range_in().upper()), legacy_in);
        assert_eq!(u256_to_biguint(limits.range_out().upper()), legacy_out);
    }

    #[test]
    fn test_marginal_price_matches_spot_price() {
        let asset = asset_token();
        let share = share_token();

        let mut dto = tycho_common::models::protocol::ProtocolComponent::default();
        dto.tokens = vec![asset.address.clone(), share.address.clone()];
        let all_tokens = std::collections::HashMap::from([
            (asset.address.clone(), asset.clone()),
            (share.address.clone(), share.clone()),
        ]);
        let component =
            crate::evm::protocol::build_swap_quoter_component(&dto, &all_tokens).unwrap();
        let state = sample_state().with_component(component);

        for (base, quote) in [(&asset, &share), (&share, &asset)] {
            let spot = state.spot_price(base, quote).unwrap();
            let marginal = state
                .marginal_price(MarginalPriceParams::new(&base.address, &quote.address))
                .unwrap();
            approx::assert_ulps_eq!(marginal.price(), spot);
        }
    }
}
