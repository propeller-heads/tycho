use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::VelodromeSlipstreamsState;
use crate::{
    evm::protocol::utils::uniswap::{i24_be_bytes_to_i32, tick_list::TickInfo},
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for VelodromeSlipstreamsState {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `AerodromeSlipstreamsState`. Errors with a
    /// `InvalidSnapshotError` if the snapshot is missing any required attributes or if the fee
    /// amount is not supported.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let component =
            crate::evm::protocol::build_swap_quoter_component(&snapshot.component, all_tokens)?;

        let liquidity = u128::from(
            snapshot
                .state
                .attributes
                .get("liquidity")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("liquidity".to_string()))?
                .clone(),
        );

        let sqrt_price = U256::from_be_slice(
            snapshot
                .state
                .attributes
                .get("sqrt_price_x96")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("sqrt_price".to_string()))?,
        );

        let custom_fee = u32::from(
            snapshot
                .state
                .attributes
                .get("custom_fee")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("custom_fee".to_string()))?
                .clone(),
        );

        let tick_spacing = snapshot
            .component
            .static_attributes
            .get("tick_spacing")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("tick_spacing".to_string()))?
            .clone();

        let tick_spacing_4_bytes = if tick_spacing.len() == 32 {
            // Make sure it only happens for 0 values, otherwise error.
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

        let default_fee = u32::from(
            snapshot
                .component
                .static_attributes
                .get("default_fee")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("default_fee".to_string()))?
                .clone(),
        );

        let tick = i32::from(
            snapshot
                .state
                .attributes
                .get("tick")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("tick".to_string()))?
                .clone(),
        );

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

        VelodromeSlipstreamsState::new(
            liquidity,
            sqrt_price,
            default_fee,
            custom_fee,
            tick_spacing,
            tick,
            ticks,
        )
        .map(|state| state.with_component(component))
        .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
    }
}
