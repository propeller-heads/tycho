use std::collections::{BTreeMap, HashMap};

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{
    attributes::{decode_bin_id, decode_reserves, decode_u16, decode_u32, parse_bin_id},
    state::PancakeswapInfinityBinState,
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for PancakeswapInfinityBinState {
    type Error = InvalidSnapshotError;

    /// Decodes a snapshot of a `pancakeswap_infinity_bin` pool.
    ///
    /// | field | source | attribute |
    /// |---|---|---|
    /// | `bin_step` | static | `bin_step` |
    /// | `active_id` | state | `active_id` |
    /// | `lp_fee` | state, else static | `fee`, else `key_lp_fee` |
    /// | `protocol_fee_*` | state, else 0 | `protocol_fees/zero2one`, `protocol_fees/one2zero` |
    /// | `bins` | state | every `bins/{id}` key |
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let static_attrs = snapshot.component.static_attributes;
        let state_attrs = snapshot.state.attributes;

        let bin_step = decode_u16("bin_step", attribute(&static_attrs, "bin_step")?)?;
        let active_id = decode_bin_id("active_id", attribute(&state_attrs, "active_id")?)?;
        // Either source satisfies the LP fee; the substreams mirrors the static one into the
        // dynamic one, which a governance update then moves.
        let lp_fee = match (state_attrs.get("fee"), static_attrs.get("key_lp_fee")) {
            (Some(fee), _) => decode_u32("fee", fee)?,
            (None, Some(fee)) => decode_u32("key_lp_fee", fee)?,
            (None, None) => return Err(InvalidSnapshotError::MissingAttribute("fee".to_string())),
        };
        // A pool carries no protocol fee until governance sets one.
        let protocol_fee = |key: &str| match state_attrs.get(key) {
            Some(value) => decode_u16(key, value).map_err(InvalidSnapshotError::from),
            None => Ok(0),
        };
        let protocol_fee_zero_for_one = protocol_fee("protocol_fees/zero2one")?;
        let protocol_fee_one_for_zero = protocol_fee("protocol_fees/one2zero")?;
        let bins = state_attrs
            .iter()
            .filter_map(|(key, value)| {
                if !key.starts_with("bins/") {
                    return None;
                }
                let entry = parse_bin_id(key)
                    .and_then(|id| decode_reserves(key, value).map(|reserves| (id, reserves)));

                match entry {
                    // A zeroed bin holds nothing; the swap walk would pay a crossing for it.
                    Ok((_, (0, 0))) => None,
                    Ok(entry) => Some(Ok(entry)),
                    Err(err) => Some(Err(InvalidSnapshotError::from(err))),
                }
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        Ok(Self {
            active_id,
            bin_step,
            lp_fee,
            protocol_fee_zero_for_one,
            protocol_fee_one_for_zero,
            bins,
        })
    }
}

/// Reads a required attribute.
fn attribute<'a>(
    attrs: &'a HashMap<String, Bytes>,
    key: &str,
) -> Result<&'a Bytes, InvalidSnapshotError> {
    attrs
        .get(key)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(key.to_string()))
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use rstest::rstest;
    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ChangeType,
    };

    use super::{super::attributes::reserve_word, *};
    use crate::evm::protocol::test_utils::try_decode_snapshot_with_defaults;

    const ACTIVE_ID: u32 = 8_388_608;

    fn component(static_attrs: &[(&str, Bytes)]) -> ProtocolComponent {
        ProtocolComponent {
            id: "0xbin".to_string(),
            protocol_system: "pancakeswap_infinity_bin".to_string(),
            protocol_type_name: "pancakeswap_infinity_bin_pool".to_string(),
            chain: Chain::Base,
            tokens: Vec::new(),
            contract_addresses: Vec::new(),
            static_attributes: static_attrs
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
            change: ChangeType::Creation,
            creation_tx: Bytes::from(vec![0u8; 32]),
            created_at: DateTime::from_timestamp(1622526000, 0)
                .unwrap()
                .naive_utc(),
        }
    }

    /// A snapshot of a hookless static-fee pool, shaped like the one the harness indexes.
    fn snapshot(
        static_attrs: &[(&str, Bytes)],
        state_attrs: &[(&str, Bytes)],
    ) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState::new(
                "0xbin",
                state_attrs
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.clone()))
                    .collect(),
                HashMap::new(),
            ),
            component: component(static_attrs),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    /// Every field lands where it belongs, and the bin word is read y first.
    #[tokio::test]
    async fn test_decodes_full_snapshot() {
        let snapshot = snapshot(
            &[
                ("bin_step", Bytes::from(10u16.to_be_bytes().to_vec())),
                ("key_lp_fee", Bytes::from(vec![0x64])),
            ],
            &[
                ("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00])),
                (&format!("bins/{ACTIVE_ID}"), reserve_word(7, 11)),
            ],
        );

        let state = try_decode_snapshot_with_defaults::<PancakeswapInfinityBinState>(snapshot)
            .await
            .unwrap();

        assert_eq!(state.bin_step, 10);
        assert_eq!(state.active_id, ACTIVE_ID);
        assert_eq!(state.lp_fee, 100);
        assert_eq!(state.protocol_fee_zero_for_one, 0);
        assert_eq!(state.protocol_fee_one_for_zero, 0);
        assert_eq!(state.bins[&ACTIVE_ID], (7, 11));
    }

    /// The dynamic `fee` wins over the static `key_lp_fee` it mirrors, so a governance update is
    /// not ignored.
    #[tokio::test]
    async fn test_dynamic_fee_overrides_static_lp_fee() {
        let snapshot = snapshot(
            &[
                ("bin_step", Bytes::from(10u16.to_be_bytes().to_vec())),
                ("key_lp_fee", Bytes::from(vec![0x64])),
            ],
            &[
                ("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00])),
                ("fee", Bytes::from(vec![0x01, 0xf4])),
            ],
        );

        let state = try_decode_snapshot_with_defaults::<PancakeswapInfinityBinState>(snapshot)
            .await
            .unwrap();

        assert_eq!(state.lp_fee, 500);
    }

    /// Required attributes, and the name each absence is reported under. Dropping `key_lp_fee`
    /// reports as `fee`, since either source satisfies the LP fee and only losing both is an error.
    #[tokio::test]
    #[rstest]
    #[case::missing_bin_step("bin_step", "bin_step")]
    #[case::missing_active_id("active_id", "active_id")]
    #[case::missing_lp_fee("key_lp_fee", "fee")]
    async fn test_missing_required_attribute_errors(#[case] removed: &str, #[case] reported: &str) {
        let statics: Vec<_> = [
            ("bin_step", Bytes::from(10u16.to_be_bytes().to_vec())),
            ("key_lp_fee", Bytes::from(vec![0x64])),
        ]
        .into_iter()
        .filter(|(key, _)| *key != removed)
        .collect();
        let states: Vec<_> = [("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00]))]
            .into_iter()
            .filter(|(key, _)| *key != removed)
            .collect();

        let result = try_decode_snapshot_with_defaults::<PancakeswapInfinityBinState>(snapshot(
            &statics, &states,
        ))
        .await;

        assert!(
            matches!(
                result,
                Err(InvalidSnapshotError::MissingAttribute(ref attr)) if attr == reported
            ),
            "expected MissingAttribute({reported}), got {result:?}"
        );
    }

    /// Bins with no reserves never enter the map; the swap walk would pay a crossing for them.
    #[tokio::test]
    async fn test_skips_empty_bins() {
        let snapshot = snapshot(
            &[
                ("bin_step", Bytes::from(10u16.to_be_bytes().to_vec())),
                ("key_lp_fee", Bytes::from(vec![0x64])),
            ],
            &[
                ("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00])),
                (&format!("bins/{ACTIVE_ID}"), reserve_word(0, 1_000)),
                (&format!("bins/{}", ACTIVE_ID + 1), reserve_word(0, 0)),
            ],
        );

        let state = try_decode_snapshot_with_defaults::<PancakeswapInfinityBinState>(snapshot)
            .await
            .unwrap();

        assert_eq!(state.bins.len(), 1);
        assert_eq!(state.bins[&ACTIVE_ID], (0, 1_000));
    }
}
