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
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
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

        let dynamic_fee_config = DynamicFeeConfig::from_attributes(&snapshot.state.attributes)
            .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))?;

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

        // An empty tick list decodes to a state that quotes no liquidity; rejecting it would
        // leave no state for later deltas to repopulate.
        let mut ticks: Vec<_> = ticks?
            .into_iter()
            .filter(|tick| tick.net_liquidity != 0)
            .collect();

        ticks.sort_by_key(|tick| tick.index);

        if liquidity != 0 && ticks.is_empty() {
            tracing::warn!(
                pool_id = %snapshot.component.id,
                "snapshot carries nonzero liquidity but no initialized ticks"
            );
        }

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
            dynamic_fee_config,
        )
        .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use rstest::rstest;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::{
        models::protocol::{ProtocolComponent, ProtocolComponentState},
        Bytes,
    };

    use super::*;
    use crate::evm::protocol::{
        test_utils::try_decode_snapshot_with_defaults,
        utils::{
            slipstreams::{dynamic_fee_module::DynamicFeeConfig, observations::Observation},
            uniswap::{tick_list::TickInfo, tick_math::get_sqrt_ratio_at_tick},
        },
    };

    const STALE_DYNAMIC_FEE_MODULE: [u8; 20] =
        hex_literal::hex!("DB45818A6db280ecfeB33cbeBd445423d0216b5D");
    fn snapshot(dynamic_fee_module: Option<Bytes>) -> ComponentWithState {
        let sqrt_price = get_sqrt_ratio_at_tick(0).expect("tick zero should have a sqrt price");
        let initialized_observation = (U256::from(1) << 248_u32).to_be_bytes::<32>();
        let mut attributes = HashMap::from([
            ("liquidity".to_string(), Bytes::from(100_u128.to_be_bytes())),
            ("sqrt_price_x96".to_string(), Bytes::from(sqrt_price.to_be_bytes::<32>())),
            ("observationIndex".to_string(), Bytes::from(0_u16.to_be_bytes())),
            ("observationCardinality".to_string(), Bytes::from(1_u16.to_be_bytes())),
            ("dfc_baseFee".to_string(), Bytes::from(500_u32.to_be_bytes())),
            ("dfc_scalingFactor".to_string(), Bytes::from(6_000_000_u64.to_be_bytes())),
            ("dfc_feeCap".to_string(), Bytes::from(700_u32.to_be_bytes())),
            ("dfc_initialFeeEnabled".to_string(), Bytes::from([1_u8])),
            ("dfc_initialFee".to_string(), Bytes::from(30_u32.to_be_bytes())),
            ("tick".to_string(), Bytes::from(0_i32.to_be_bytes())),
            ("ticks/-1".to_string(), Bytes::from(1_i128.to_be_bytes())),
            ("ticks/1".to_string(), Bytes::from((-1_i128).to_be_bytes())),
            ("observations/0".to_string(), Bytes::from(initialized_observation)),
        ]);
        if let Some(dynamic_fee_module) = dynamic_fee_module {
            attributes.insert("dynamic_fee_module".to_string(), dynamic_fee_module);
        }

        ComponentWithState {
            state: ProtocolComponentState::new("test-pool", attributes, HashMap::new()),
            component: ProtocolComponent {
                id: "test-pool".to_string(),
                static_attributes: HashMap::from([
                    ("default_fee".to_string(), Bytes::from(100_u32.to_be_bytes())),
                    ("tick_spacing".to_string(), Bytes::from(1_i32.to_be_bytes())),
                ]),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn expected_state(dfc: DynamicFeeConfig) -> AerodromeSlipstreamsState {
        AerodromeSlipstreamsState::new(
            "test-pool".to_string(),
            0,
            100,
            get_sqrt_ratio_at_tick(0).expect("tick zero should have a sqrt price"),
            0,
            1,
            100,
            1,
            0,
            vec![TickInfo::new(-1, 1).unwrap(), TickInfo::new(1, -1).unwrap()],
            vec![Observation { initialized: true, index: 0, ..Default::default() }],
            dfc,
        )
        .expect("test state should be valid")
    }

    #[rstest]
    #[case::missing_module(None)]
    #[case::stale_module(Some(Bytes::from(STALE_DYNAMIC_FEE_MODULE)))]
    #[tokio::test]
    async fn missing_or_unsupported_module_falls_back_to_default_config(
        #[case] dynamic_fee_module: Option<Bytes>,
    ) {
        // Still decodable with a default config, not dropped: these pools quote their default_fee.
        let decoded = try_decode_snapshot_with_defaults::<AerodromeSlipstreamsState>(snapshot(
            dynamic_fee_module,
        ))
        .await
        .expect("pools without a supported marker should remain decodable");

        assert_eq!(decoded, expected_state(DynamicFeeConfig::default()));
    }

    #[tokio::test]
    async fn no_tick_attributes_decode_to_an_empty_tick_list() {
        let mut snapshot = snapshot(None);
        snapshot
            .state
            .attributes
            .retain(|key, _| !key.starts_with("ticks/"));

        let decoded = try_decode_snapshot_with_defaults::<AerodromeSlipstreamsState>(snapshot)
            .await
            .expect("a snapshot without tick attributes is a decodable, empty pool");

        let expected = AerodromeSlipstreamsState::new(
            "test-pool".to_string(),
            0,
            100,
            get_sqrt_ratio_at_tick(0).expect("tick zero should have a sqrt price"),
            0,
            1,
            100,
            1,
            0,
            vec![],
            vec![Observation { initialized: true, index: 0, ..Default::default() }],
            DynamicFeeConfig::default(),
        )
        .expect("an empty tick list is a valid pool state");
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn all_zero_ticks_decode_to_an_empty_tick_list() {
        let mut snapshot = snapshot(None);
        snapshot
            .state
            .attributes
            .insert("ticks/-1".to_string(), Bytes::from(0_i128.to_be_bytes()));
        snapshot
            .state
            .attributes
            .insert("ticks/1".to_string(), Bytes::from(0_i128.to_be_bytes()));

        let decoded = try_decode_snapshot_with_defaults::<AerodromeSlipstreamsState>(snapshot)
            .await
            .expect("an all-zero tick set is a decodable, empty pool");

        let expected = AerodromeSlipstreamsState::new(
            "test-pool".to_string(),
            0,
            100,
            get_sqrt_ratio_at_tick(0).expect("tick zero should have a sqrt price"),
            0,
            1,
            100,
            1,
            0,
            vec![],
            vec![Observation { initialized: true, index: 0, ..Default::default() }],
            DynamicFeeConfig::default(),
        )
        .expect("an empty tick list is a valid pool state");
        assert_eq!(decoded, expected);
    }

    #[rstest]
    #[case::factory_5e7b(hex_literal::hex!("090b2A6bb475c00e2256e2095A60887cD710803b"))]
    #[case::factory_ade6(hex_literal::hex!("F4Ecd78EBEB6d36CF7f80B5B6B41453515fe2785"))]
    #[case::factory_f8f2(hex_literal::hex!("87D8f999BBa9343E8099552426775B51C338E8CB"))]
    #[tokio::test]
    async fn supported_module_defaults_missing_initial_fee_attributes(
        #[case] dynamic_fee_module: [u8; 20],
    ) {
        let mut snapshot = snapshot(Some(Bytes::from(dynamic_fee_module)));
        snapshot
            .state
            .attributes
            .remove("dfc_initialFeeEnabled");
        snapshot
            .state
            .attributes
            .remove("dfc_initialFee");

        let decoded = try_decode_snapshot_with_defaults::<AerodromeSlipstreamsState>(snapshot)
            .await
            .expect("new module updates should work with pre-upgrade pool state");

        assert_eq!(decoded, expected_state(DynamicFeeConfig::new(500, 700, 6_000_000, false, 0)));
    }
}
