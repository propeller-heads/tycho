use std::collections::HashMap;

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{
    adaptive_fee::FeeConfiguration, attributes, state::CamelotV3State, timepoints::Timepoint,
};
use crate::{
    evm::protocol::utils::uniswap::tick_list::TickInfo,
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

fn required<'a>(
    snapshot: &'a ComponentWithState,
    name: &str,
) -> Result<&'a Bytes, InvalidSnapshotError> {
    snapshot
        .state
        .attributes
        .get(name)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))
}

fn value_error(err: String) -> InvalidSnapshotError {
    InvalidSnapshotError::ValueError(err)
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for CamelotV3State {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `CamelotV3State`.
    ///
    /// Errors when a pool-level attribute is missing or malformed, or when the ring does not
    /// contain the timepoint `timepoint_index` points at. A pool without positions has no
    /// `ticks/*` attributes and decodes to a state that quotes nothing.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let liquidity = attributes::u128_attr(
            attributes::LIQUIDITY,
            required(&snapshot, attributes::LIQUIDITY)?,
        )
        .map_err(value_error)?;
        let sqrt_price = attributes::u160_attr(
            attributes::SQRT_PRICE_X96,
            required(&snapshot, attributes::SQRT_PRICE_X96)?,
        )
        .map_err(value_error)?;
        let tick = attributes::i24_attr(attributes::TICK, required(&snapshot, attributes::TICK)?)
            .map_err(value_error)?;
        let fee_zto =
            attributes::u16_attr(attributes::FEE_ZTO, required(&snapshot, attributes::FEE_ZTO)?)
                .map_err(value_error)?;
        let fee_otz =
            attributes::u16_attr(attributes::FEE_OTZ, required(&snapshot, attributes::FEE_OTZ)?)
                .map_err(value_error)?;
        let timepoint_index = attributes::u16_attr(
            attributes::TIMEPOINT_INDEX,
            required(&snapshot, attributes::TIMEPOINT_INDEX)?,
        )
        .map_err(value_error)?;
        let volume_per_liquidity_in_block = attributes::u128_attr(
            attributes::VOLUME_PER_LIQUIDITY_IN_BLOCK,
            required(&snapshot, attributes::VOLUME_PER_LIQUIDITY_IN_BLOCK)?,
        )
        .map_err(value_error)?;
        let fee_config_zto =
            FeeConfiguration::from_slot(required(&snapshot, attributes::FEE_CONFIG_ZTO)?)
                .map_err(|err| value_error(err.to_string()))?;
        let fee_config_otz =
            FeeConfiguration::from_slot(required(&snapshot, attributes::FEE_CONFIG_OTZ)?)
                .map_err(|err| value_error(err.to_string()))?;

        let mut ticks = Vec::new();
        let mut timepoints = Vec::new();
        for (key, value) in snapshot.state.attributes.iter() {
            if let Some(tick) = attributes::tick_of_key(key) {
                let net_liquidity = attributes::i128_attr(key, value).map_err(value_error)?;
                ticks.push(
                    TickInfo::new(tick.map_err(value_error)?, net_liquidity)
                        .map_err(|err| value_error(err.to_string()))?,
                );
            } else if let Some(index) = attributes::timepoint_of_key(key) {
                timepoints.push(
                    Timepoint::from_attribute(index.map_err(value_error)?, value)
                        .map_err(|err| value_error(err.to_string()))?,
                );
            }
        }
        if !sqrt_price.is_zero() &&
            !timepoints
                .iter()
                .any(|t| t.index == timepoint_index)
        {
            return Err(InvalidSnapshotError::MissingAttribute(format!(
                "{}{timepoint_index}",
                attributes::TIMEPOINTS_PREFIX
            )));
        }

        CamelotV3State::new(
            snapshot.component.id.clone(),
            // Seed value only: the stream decoder points every block-sensitive state at the
            // execution block before the snapshot reaches a consumer.
            block.timestamp,
            liquidity,
            sqrt_price,
            tick,
            fee_zto,
            fee_otz,
            timepoint_index,
            volume_per_liquidity_in_block,
            ticks,
            timepoints,
            fee_config_zto,
            fee_config_otz,
        )
        .map_err(|err| value_error(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use rstest::rstest;
    use tycho_common::{
        models::protocol::{ProtocolComponent, ProtocolComponentState},
        simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::evm::protocol::{
        camelot_v3::timepoints::Timepoint, test_utils::try_decode_snapshot_with_defaults,
    };

    /// `feeConfigZto`/`feeConfigOtz` slot of the WETH/USDC pool's operator.
    const FEE_CONFIG_SLOT: [u8; 32] =
        hex_literal::hex!("00000000000000000064000a000000002134003b0000ea60000002d002580000");

    fn timepoint_attribute(index: u16, block_timestamp: u32) -> (String, Bytes) {
        let timepoint = Timepoint {
            index,
            initialized: true,
            block_timestamp,
            average_tick: -197_230,
            ..Default::default()
        };
        (format!("timepoints/{index}"), Bytes::from(timepoint.to_attribute().to_vec()))
    }

    fn attributes() -> HashMap<String, Bytes> {
        HashMap::from([
            ("liquidity".to_string(), Bytes::from(52_937_052_414_055_576u128.to_be_bytes())),
            (
                "sqrt_price_x96".to_string(),
                Bytes::from(
                    U256::from(4_133_423_575_270_025_211_670_931u128).to_be_bytes::<32>()[12..]
                        .to_vec(),
                ),
            ),
            ("tick".to_string(), Bytes::from((-197_230i32).to_be_bytes()[1..].to_vec())),
            ("fee_zto".to_string(), Bytes::from(100u16.to_be_bytes())),
            ("fee_otz".to_string(), Bytes::from(100u16.to_be_bytes())),
            ("timepoint_index".to_string(), Bytes::from(48_648u16.to_be_bytes())),
            (
                "volume_per_liquidity_in_block".to_string(),
                Bytes::from(715_374_378_433_376u128.to_be_bytes()),
            ),
            ("fee_config_zto".to_string(), Bytes::from(FEE_CONFIG_SLOT)),
            ("fee_config_otz".to_string(), Bytes::from(FEE_CONFIG_SLOT)),
            ("ticks/-197240".to_string(), Bytes::from(1_000i128.to_be_bytes())),
            ("ticks/-197220".to_string(), Bytes::from((-1_000i128).to_be_bytes())),
            timepoint_attribute(48_647, 1_790_273_000),
            timepoint_attribute(48_648, 1_790_273_118),
        ])
    }

    fn snapshot(attributes: HashMap<String, Bytes>) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState::new("pool", attributes, HashMap::new()),
            component: ProtocolComponent { id: "pool".to_string(), ..Default::default() },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[tokio::test]
    async fn decodes_a_full_snapshot() {
        let decoded = try_decode_snapshot_with_defaults::<CamelotV3State>(snapshot(attributes()))
            .await
            .expect("snapshot should decode");

        let expected = CamelotV3State::new(
            "pool".to_string(),
            0,
            52_937_052_414_055_576,
            U256::from(4_133_423_575_270_025_211_670_931u128),
            -197_230,
            100,
            100,
            48_648,
            715_374_378_433_376,
            vec![TickInfo::new(-197_240, 1_000).unwrap(), TickInfo::new(-197_220, -1_000).unwrap()],
            vec![
                Timepoint::from_attribute(48_647, &timepoint_attribute(48_647, 1_790_273_000).1)
                    .unwrap(),
                Timepoint::from_attribute(48_648, &timepoint_attribute(48_648, 1_790_273_118).1)
                    .unwrap(),
            ],
            FeeConfiguration::from_slot(&FEE_CONFIG_SLOT).unwrap(),
            FeeConfiguration::from_slot(&FEE_CONFIG_SLOT).unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn a_pool_without_positions_decodes() {
        let mut attributes = attributes();
        attributes.retain(|key, _| !key.starts_with("ticks/"));
        attributes.insert("liquidity".to_string(), Bytes::from(0u128.to_be_bytes()));

        let decoded = try_decode_snapshot_with_defaults::<CamelotV3State>(snapshot(attributes))
            .await
            .expect("a pool without ticks should decode");
        assert_eq!(
            decoded
                .get_limits(Bytes::from([1u8; 20]), Bytes::from([2u8; 20]))
                .unwrap(),
            (Default::default(), Default::default())
        );
    }

    #[tokio::test]
    async fn an_uninitialized_pool_decodes_without_a_ring() {
        let mut attributes = attributes();
        attributes.retain(|key, _| !key.starts_with("timepoints/") && !key.starts_with("ticks/"));
        attributes.insert("sqrt_price_x96".to_string(), Bytes::from([0u8; 20]));
        attributes.insert("liquidity".to_string(), Bytes::from(0u128.to_be_bytes()));

        let decoded = try_decode_snapshot_with_defaults::<CamelotV3State>(snapshot(attributes))
            .await
            .expect("a pool before initialize should decode");
        assert_eq!(decoded.fee(), 100.0 / 1_000_000.0, "stored fees, no recomputation");
        assert!(decoded
            .spot_price(
                &Token::new(
                    &Bytes::from([1u8; 20]),
                    "A",
                    18,
                    0,
                    &[Some(10_000)],
                    tycho_common::models::Chain::Arbitrum,
                    100
                ),
                &Token::new(
                    &Bytes::from([2u8; 20]),
                    "B",
                    18,
                    0,
                    &[Some(10_000)],
                    tycho_common::models::Chain::Arbitrum,
                    100
                ),
            )
            .is_err());
    }

    #[rstest]
    #[case::liquidity("liquidity")]
    #[case::sqrt_price("sqrt_price_x96")]
    #[case::tick("tick")]
    #[case::fee_zto("fee_zto")]
    #[case::fee_otz("fee_otz")]
    #[case::timepoint_index("timepoint_index")]
    #[case::volume_per_liquidity_in_block("volume_per_liquidity_in_block")]
    #[case::fee_config_zto("fee_config_zto")]
    #[case::fee_config_otz("fee_config_otz")]
    #[case::last_timepoint("timepoints/48648")]
    #[tokio::test]
    async fn a_missing_attribute_is_reported(#[case] missing: &str) {
        let mut attributes = attributes();
        attributes.remove(missing);

        let err = try_decode_snapshot_with_defaults::<CamelotV3State>(snapshot(attributes))
            .await
            .expect_err("decoding must fail");
        assert!(
            matches!(err, InvalidSnapshotError::MissingAttribute(ref name) if name == missing),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_malformed_attribute_is_a_value_error() {
        let mut attributes = attributes();
        attributes.insert("fee_zto".to_string(), Bytes::from([1u8, 0, 0]));

        let err = try_decode_snapshot_with_defaults::<CamelotV3State>(snapshot(attributes))
            .await
            .expect_err("decoding must fail");
        assert!(matches!(err, InvalidSnapshotError::ValueError(_)), "{err:?}");
    }
}
