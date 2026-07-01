use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::AerodromeSlipstreamsState;
use crate::{
    evm::protocol::utils::{
        slipstreams::{dynamic_fee_module::DynamicFeeConfig, observations::Observation},
        uniswap::{i24_be_bytes_to_i32, tick_list::TickInfo},
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for AerodromeSlipstreamsState {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `AerodromeSlipstreamsState`. Errors with a
    /// `InvalidSnapshotError` if the snapshot is missing any required attributes or if the fee
    /// amount is not supported.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let component =
            crate::evm::protocol::build_swap_quoter_component(&snapshot.component, all_tokens)?;

        let liq = snapshot
            .state
            .attributes
            .get("liquidity")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("liquidity".to_string()))?
            .clone();

        // This is a hotfix because if the liquidity has never been updated after creation, it's
        // currently encoded as H256::zero(), therefore, we can't decode this as u128.
        // We can remove this once it has been fixed on the tycho side.
        let liq_16_bytes = if liq.len() == 32 {
            // Make sure it only happens for 0 values, otherwise error.
            if liq == Bytes::zero(32) {
                Bytes::from([0; 16])
            } else {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "Liquidity bytes too long for {liq}, expected 16"
                )));
            }
        } else {
            liq
        };

        let liquidity = u128::from(liq_16_bytes);

        let sqrt_price = U256::from_be_slice(
            snapshot
                .state
                .attributes
                .get("sqrt_price_x96")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("sqrt_price".to_string()))?,
        );

        let observation_index = u16::from(
            snapshot
                .state
                .attributes
                .get("observationIndex")
                .ok_or_else(|| {
                    InvalidSnapshotError::MissingAttribute("observationIndex".to_string())
                })?
                .clone(),
        );

        let observation_cardinality = u16::from(
            snapshot
                .state
                .attributes
                .get("observationCardinality")
                .ok_or_else(|| {
                    InvalidSnapshotError::MissingAttribute("observationCardinality".to_string())
                })?
                .clone(),
        );

        let dfc_base_fee = u32::from(
            snapshot
                .state
                .attributes
                .get("dfc_baseFee")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("dfc_baseFee".to_string()))?
                .clone(),
        );

        let dfc_scaling_factor = u64::from(
            snapshot
                .state
                .attributes
                .get("dfc_scalingFactor")
                .ok_or_else(|| {
                    InvalidSnapshotError::MissingAttribute("dfc_scalingFactor".to_string())
                })?
                .clone(),
        );

        let dfc_fee_cap = u32::from(
            snapshot
                .state
                .attributes
                .get("dfc_feeCap")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("dfc_feeCap".to_string()))?
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

        let tick = snapshot
            .state
            .attributes
            .get("tick")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("tick".to_string()))?
            .clone();

        // This is a hotfix because if the tick has never been updated after creation, it's
        // currently encoded as H256::zero(), therefore, we can't decode this as i32. We can
        // remove this this will be fixed on the tycho side.
        let ticks_4_bytes = if tick.len() == 32 {
            // Make sure it only happens for 0 values, otherwise error.
            if tick == Bytes::zero(32) {
                Bytes::from([0; 4])
            } else {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "Tick bytes too long for {tick}, expected 4"
                )));
            }
        } else {
            tick
        };
        let tick = i24_be_bytes_to_i32(&ticks_4_bytes);

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

        let observations: Vec<Observation> = snapshot
            .state
            .attributes
            .iter()
            .filter_map(|(key, value)| {
                key.strip_prefix("observations/")?
                    .parse::<i32>()
                    .ok()
                    .and_then(|idx| Observation::from_attribute(idx, value).ok())
            })
            .collect();

        let mut observations: Vec<_> = observations
            .into_iter()
            .filter(|t| t.initialized)
            .collect();

        if observations.is_empty() {
            return Err(InvalidSnapshotError::MissingAttribute("observations".to_string()));
        }

        observations.sort_by_key(|observation| observation.index);

        AerodromeSlipstreamsState::new(
            snapshot.component.id.clone(),
            block.timestamp,
            liquidity,
            sqrt_price,
            observation_index,
            observation_cardinality,
            default_fee,
            tick_spacing,
            tick,
            ticks,
            observations,
            DynamicFeeConfig::new(dfc_base_fee, dfc_fee_cap, dfc_scaling_factor),
        )
        .map(|state| state.with_component(component))
        .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
    }
}
