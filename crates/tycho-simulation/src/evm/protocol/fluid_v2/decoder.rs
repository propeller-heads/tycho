use std::{collections::HashMap, str::FromStr};

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::protocol::{
        fluid_v2::state::{DexType, DexVariables, DexVariables2, FluidV2State},
        utils::uniswap::{i24_be_bytes_to_i32, tick_list::TickInfo},
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for FluidV2State {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let component_id = Bytes::from_str(snapshot.component.id.as_str()).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "Expected component id to be hex encoded dex_type and dex_id: {e}"
            ))
        })?;
        if component_id.len() != 33 {
            return Err(InvalidSnapshotError::ValueError(format!(
                "Expected component id to be 33 bytes, got {}",
                component_id.len()
            )));
        }
        let component_id_dex_type = component_id[0];
        let dex_id = Bytes::from(&component_id[1..]);
        let token0_address = snapshot
            .component
            .tokens
            .first()
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError("Missing token0 in component".to_string())
            })?;
        let token0 = all_tokens
            .get(token0_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Missing token0 in state: {token0_address}"
                ))
            })?;
        let token1_address = snapshot
            .component
            .tokens
            .get(1)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError("Missing token1 in component".to_string())
            })?;
        let token1 = all_tokens
            .get(token1_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Missing token1 in state: {token1_address}"
                ))
            })?;
        let dex_type = snapshot
            .component
            .static_attributes
            .get("dex_type")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("dex_type".to_string()))?
            .clone();

        let fee = snapshot
            .component
            .static_attributes
            .get("fee")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("fee".to_string()))?
            .clone();

        let tick_spacing = snapshot
            .component
            .static_attributes
            .get("tick_spacing")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("tick_spacing".to_string()))?
            .clone();

        let controller_bytes = snapshot
            .component
            .static_attributes
            .get("controller")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("controller".to_string()))?
            .clone();

        let dex_variables_bytes = snapshot
            .state
            .attributes
            .get("dex_variables")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("dex_variables".to_string()))?
            .clone();

        let dex_variables2_bytes = snapshot
            .state
            .attributes
            .get("dex_variables2")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("dex_variables2".to_string()))?
            .clone();

        let token0_reserve_bytes = snapshot
            .state
            .attributes
            .get("token0/token_reserves")
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute("token0/token_reserves".to_string())
            })?
            .clone();

        let token1_reserve_bytes = snapshot
            .state
            .attributes
            .get("token1/token_reserves")
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute("token1/token_reserves".to_string())
            })?
            .clone();

        let token0_borrow_exchange_price_bytes = snapshot
            .state
            .attributes
            .get("token0/borrow_exchange_price")
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute("token0/borrow_exchange_price".to_string())
            })?
            .clone();

        let token0_supply_exchange_price_bytes = snapshot
            .state
            .attributes
            .get("token0/supply_exchange_price")
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute("token0/supply_exchange_price".to_string())
            })?
            .clone();

        let token1_borrow_exchange_price_bytes = snapshot
            .state
            .attributes
            .get("token1/borrow_exchange_price")
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute("token1/borrow_exchange_price".to_string())
            })?
            .clone();

        let token1_supply_exchange_price_bytes = snapshot
            .state
            .attributes
            .get("token1/supply_exchange_price")
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute("token1/supply_exchange_price".to_string())
            })?
            .clone();

        let ticks: Result<Vec<_>, _> = snapshot
            .state
            .attributes
            .iter()
            .filter_map(|(key, value)| {
                if key.starts_with("ticks/") {
                    Some(
                        key.split('/')
                            .nth(1)?
                            .parse::<i32>()
                            .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
                            .and_then(|tick_index| {
                                TickInfo::new(tick_index, i128::from(value.clone())).map_err(
                                    |err| InvalidSnapshotError::ValueError(err.to_string()),
                                )
                            }),
                    )
                } else {
                    None
                }
            })
            .collect();

        let mut ticks = match ticks {
            Ok(ticks) if !ticks.is_empty() => ticks
                .into_iter()
                .filter(|t| t.net_liquidity != 0)
                .collect::<Vec<_>>(),
            _ => return Err(InvalidSnapshotError::MissingAttribute("tick_liquidities".to_string())),
        };

        ticks.sort_by_key(|tick| tick.index);

        let dex_type_value = U256::from_be_slice(&dex_type).to::<u32>();
        let dex_type = match dex_type_value {
            3 => DexType::D3,
            4 => DexType::D4,
            _ => {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "Unsupported dex_type {dex_type_value}",
                )));
            }
        };
        if component_id_dex_type != dex_type_value as u8 {
            return Err(InvalidSnapshotError::ValueError(format!(
                "Component id dex_type {component_id_dex_type} does not match static dex_type {dex_type_value}",
            )));
        }

        let fee_value = U256::from_be_slice(&fee).to::<u32>();
        let fee_value = fee_value & 0x00FF_FFFF;
        let dynamic_fee = fee_value == 0x00FF_FFFF;

        let tick_spacing_4_bytes = if tick_spacing.len() == 32 {
            if tick_spacing == Bytes::zero(32) {
                Bytes::from([0; 4])
            } else {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "Tick Spacing bytes too long for {tick_spacing}, expected 4"
                )));
            }
        } else {
            tick_spacing
        };

        let tick_spacing = i24_be_bytes_to_i32(&tick_spacing_4_bytes);

        let dex_variables = DexVariables::from_packed(U256::from_be_slice(&dex_variables_bytes));
        let dex_variables2 = DexVariables2::from_packed(U256::from_be_slice(&dex_variables2_bytes));

        let token0_reserve = U256::from_be_slice(&token0_reserve_bytes);
        let token1_reserve = U256::from_be_slice(&token1_reserve_bytes);
        let token0_borrow_exchange_price = U256::from_be_slice(&token0_borrow_exchange_price_bytes);
        let token0_supply_exchange_price = U256::from_be_slice(&token0_supply_exchange_price_bytes);
        let token1_borrow_exchange_price = U256::from_be_slice(&token1_borrow_exchange_price_bytes);
        let token1_supply_exchange_price = U256::from_be_slice(&token1_supply_exchange_price_bytes);

        let state = FluidV2State::new(
            dex_id,
            token0.clone(),
            token1.clone(),
            dex_type,
            fee_value,
            dynamic_fee,
            tick_spacing,
            controller_bytes,
            dex_variables,
            dex_variables2,
            token0_reserve,
            token1_reserve,
            token0_borrow_exchange_price,
            token0_supply_exchange_price,
            token1_borrow_exchange_price,
            token1_supply_exchange_price,
            ticks,
        );

        Ok(state)
    }
}
