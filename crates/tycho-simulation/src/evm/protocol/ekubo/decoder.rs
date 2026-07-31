use std::collections::HashMap;

use evm_ekubo_sdk::{
    math::uint::U256,
    quoting::{
        full_range_pool::FullRangePoolState,
        oracle_pool::OraclePoolState,
        twamm_pool::TwammPoolState,
        types::{Config, NodeKey},
    },
};
use itertools::Itertools;
use num_traits::Zero;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{
    attributes::{sale_rate_deltas_from_attributes, ticks_from_attributes},
    pool::{base::BasePool, full_range::FullRangePool, oracle::OraclePool, twamm::TwammPool},
    state::EkuboState,
};
use crate::{
    evm::protocol::ekubo::pool::mev_resist::MevResistPool,
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

enum EkuboExtension {
    Base,
    Oracle,
    Twamm,
    MevResist,
}

impl TryFrom<Bytes> for EkuboExtension {
    type Error = InvalidSnapshotError;

    fn try_from(value: Bytes) -> Result<Self, Self::Error> {
        // See extension ID encoding in tycho-protocol-sdk
        match i32::from(value) {
            0 => Err(InvalidSnapshotError::ValueError("Unknown Ekubo extension".to_string())),
            1 => Ok(Self::Base),
            2 => Ok(Self::Oracle),
            3 => Ok(Self::Twamm),
            4 => Ok(Self::MevResist),
            discriminant => Err(InvalidSnapshotError::ValueError(format!(
                "Unknown Ekubo extension discriminant {discriminant}"
            ))),
        }
    }
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for EkuboState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let static_attrs = snapshot.component.static_attributes;
        let state_attrs = snapshot.state.attributes;

        let extension_id = attribute(&static_attrs, "extension_id")?
            .clone()
            .try_into()?;

        let (token0, token1) = (
            U256::from_big_endian(attribute(&static_attrs, "token0")?),
            U256::from_big_endian(attribute(&static_attrs, "token1")?),
        );

        let fee = u64::from_be_bytes(
            attribute(&static_attrs, "fee")?
                .as_ref()
                .try_into()
                .map_err(|err| {
                    InvalidSnapshotError::ValueError(format!("fee length mismatch: {err:?}"))
                })?,
        );

        let tick_spacing = u32::from_be_bytes(
            attribute(&static_attrs, "tick_spacing")?
                .as_ref()
                .try_into()
                .map_err(|err| {
                    InvalidSnapshotError::ValueError(format!(
                        "tick_spacing length mismatch: {err:?}"
                    ))
                })?,
        );

        let extension = U256::from_big_endian(attribute(&static_attrs, "extension")?);

        let config = Config { fee, tick_spacing, extension };

        let liquidity = attribute(&state_attrs, "liquidity")?
            .clone()
            .into();

        let sqrt_ratio = U256::from_big_endian(attribute(&state_attrs, "sqrt_ratio")?);

        let key = NodeKey { token0, token1, config };

        Ok(match extension_id {
            EkuboExtension::Base => {
                if tick_spacing.is_zero() {
                    Self::FullRange(FullRangePool::new(
                        key,
                        FullRangePoolState { sqrt_ratio, liquidity },
                    )?)
                } else {
                    let tick = attribute(&state_attrs, "tick")?
                        .clone()
                        .into();

                    let mut ticks = ticks_from_attributes(state_attrs)
                        .map_err(InvalidSnapshotError::ValueError)?;

                    ticks.sort_unstable_by_key(|tick| tick.index);

                    Self::Base(BasePool::new(key, ticks, sqrt_ratio, liquidity, tick)?)
                }
            }
            EkuboExtension::Oracle => Self::Oracle(OraclePool::new(
                &key,
                OraclePoolState {
                    full_range_pool_state: FullRangePoolState { sqrt_ratio, liquidity },
                    last_snapshot_time: 0, /* For the purpose of quote computation it isn't
                                            * required to track actual timestamps */
                },
            )?),
            EkuboExtension::Twamm => {
                let (token0_sale_rate, token1_sale_rate) = (
                    attribute(&state_attrs, "token0_sale_rate")?
                        .clone()
                        .into(),
                    attribute(&state_attrs, "token1_sale_rate")?
                        .clone()
                        .into(),
                );

                let last_execution_time: u64 = attribute(&state_attrs, "last_execution_time")?
                    .clone()
                    .into();

                let mut virtual_order_deltas =
                    sale_rate_deltas_from_attributes(state_attrs, last_execution_time)
                        .map_err(InvalidSnapshotError::ValueError)?
                        .collect_vec();

                virtual_order_deltas.sort_unstable_by_key(|delta| delta.time);

                Self::Twamm(TwammPool::new(
                    &key,
                    TwammPoolState {
                        full_range_pool_state: FullRangePoolState { sqrt_ratio, liquidity },
                        token0_sale_rate,
                        token1_sale_rate,
                        last_execution_time,
                    },
                    virtual_order_deltas,
                )?)
            }
            EkuboExtension::MevResist => {
                let tick = attribute(&state_attrs, "tick")?
                    .clone()
                    .into();

                let mut ticks =
                    ticks_from_attributes(state_attrs).map_err(InvalidSnapshotError::ValueError)?;

                ticks.sort_unstable_by_key(|tick| tick.index);

                Self::MevResist(MevResistPool::new(key, ticks, sqrt_ratio, liquidity, tick)?)
            }
        })
    }
}

fn attribute<'a>(
    map: &'a HashMap<String, Bytes>,
    key: &str,
) -> Result<&'a Bytes, InvalidSnapshotError> {
    map.get(key)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(key.to_string()))
}

#[cfg(test)]
mod tests {
    use rstest::*;
    use rstest_reuse::apply;
    use tycho_common::models::protocol::ProtocolComponentState;

    use super::*;
    use crate::evm::protocol::{
        ekubo::test_cases::*, test_utils::try_decode_snapshot_with_defaults,
    };

    #[apply(all_cases)]
    #[tokio::test]
    async fn test_try_from_with_header(case: TestCase) {
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: String::new(),
                attributes: case.state_attributes,
                balances: HashMap::new(),
            },
            component: case.component,
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<EkuboState>(snapshot)
            .await
            .expect("reconstructing state");

        assert_eq!(result, case.state_before_transition);
    }

    #[apply(all_cases)]
    #[tokio::test]
    async fn test_try_from_invalid(case: TestCase) {
        for missing_attribute in case.required_attributes {
            let mut component = case.component.clone();
            let mut attributes = case.state_attributes.clone();

            component
                .static_attributes
                .remove(&missing_attribute);
            attributes.remove(&missing_attribute);

            let snapshot = ComponentWithState {
                state: ProtocolComponentState {
                    attributes,
                    component_id: String::new(),
                    balances: HashMap::new(),
                },
                component,
                component_tvl: None,
                entrypoints: Vec::new(),
            };

            let result = EkuboState::try_from_with_header(
                snapshot,
                BlockHeader::default(),
                &HashMap::default(),
                &HashMap::default(),
                &DecoderContext::new(),
            )
            .await;

            assert!(result.is_err());
        }
    }
}
