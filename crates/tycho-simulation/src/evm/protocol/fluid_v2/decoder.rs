use std::collections::{BTreeMap, HashMap};

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::protocol::fluid_v2::state::{
        decode_net_liquidity, parse_tick_key, DexType, DexVariables, DexVariables2, FluidV2State,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for FluidV2State {
    type Error = InvalidSnapshotError;

    /// Decodes a pool from its indexed attributes, then reads both tokens' Liquidity state through
    /// the DEX v2 resolver in the shared VM database.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let component = &snapshot.component;
        let attributes = &snapshot.state.attributes;
        let token = |index: usize| -> Result<Token, InvalidSnapshotError> {
            let address = component
                .tokens
                .get(index)
                .ok_or_else(|| {
                    InvalidSnapshotError::ValueError(format!("Missing token{index} in component"))
                })?;
            all_tokens
                .get(address)
                .cloned()
                .ok_or_else(|| {
                    InvalidSnapshotError::ValueError(format!("Missing token{index} {address}"))
                })
        };
        let static_attribute = |name: &str| {
            component
                .static_attributes
                .get(name)
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))
        };
        let attribute = |name: &str| {
            attributes
                .get(name)
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))
        };

        let dex_type = match U256::from_be_slice(static_attribute("dex_type")?).to::<u64>() {
            3 => DexType::D3,
            4 => DexType::D4,
            other => {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "Unsupported Fluid v2 dex type {other}"
                )))
            }
        };
        let tick_spacing = i32::try_from(U256::from_be_slice(static_attribute("tick_spacing")?))
            .ok()
            .filter(|tick_spacing| *tick_spacing > 0)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(
                    "Fluid v2 tick spacing must be positive".to_string(),
                )
            })?;
        let resolver = static_attribute("resolver")?.clone();

        let mut ticks = BTreeMap::new();
        for (key, value) in attributes {
            let Some(tick) = parse_tick_key(key)
                .map_err(|e| InvalidSnapshotError::ValueError(format!("{e:?}")))?
            else {
                continue;
            };
            let net_liquidity =
                decode_net_liquidity(value).map_err(InvalidSnapshotError::ValueError)?;
            if net_liquidity != 0 {
                ticks.insert(tick, net_liquidity);
            }
        }

        let token0 = token(0)?;
        let token1 = token(1)?;
        let (token0_liquidity, token1_liquidity) =
            FluidV2State::fetch_token_liquidity(&resolver, &token0.address, &token1.address)
                .map_err(InvalidSnapshotError::VMError)?;

        Ok(FluidV2State::new(
            dex_type,
            token0,
            token1,
            resolver,
            tick_spacing,
            DexVariables::from_packed(U256::from_be_slice(attribute("dex_variables")?)),
            DexVariables2::from_packed(U256::from_be_slice(attribute("dex_variables2")?)),
            U256::from_be_slice(attribute("token0/token_reserves")?),
            U256::from_be_slice(attribute("token1/token_reserves")?),
            ticks,
            token0_liquidity,
            token1_liquidity,
        ))
    }
}
