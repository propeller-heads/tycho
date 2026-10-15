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
    /// | `protocol_fee_*` | state | `protocol_fees/zero2one`, `protocol_fees/one2zero` |
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

        // The substreams package already drops these pools. Checked again because a quote for
        // one would be silently wrong, not an error.
        let parameters = attribute(&static_attrs, "parameters")?;
        if has_swap_hooks(parameters)? {
            return Err(InvalidSnapshotError::ValueError(format!(
                "pool registers a swap hook (parameters {parameters}); its quotes depend on the hook"
            )));
        }
        if let Some(fee) = static_attrs.get("key_lp_fee") {
            if decode_u32("key_lp_fee", fee)? == DYNAMIC_FEE_FLAG {
                return Err(InvalidSnapshotError::ValueError(
                    "pool has a dynamic LP fee, set by its hook per swap".to_string(),
                ));
            }
        }

        let bin_step = decode_u16("bin_step", attribute(&static_attrs, "bin_step")?)?;
        let active_id = decode_bin_id("active_id", attribute(&state_attrs, "active_id")?)?;
        // Either source satisfies the LP fee; the substreams mirrors the static one into `fee`.
        // Only a dynamic-fee pool can change its LP fee, and those are rejected above.
        let lp_fee = match (state_attrs.get("fee"), static_attrs.get("key_lp_fee")) {
            (Some(fee), _) => decode_u32("fee", fee)?,
            (None, Some(fee)) => decode_u32("key_lp_fee", fee)?,
            (None, None) => return Err(InvalidSnapshotError::MissingAttribute("fee".to_string())),
        };
        // `initialize` sets the protocol fee from the fee controller and the substreams emits both
        // halves at creation, so a missing half is a broken snapshot, not a zero fee: defaulting
        // would overstate every quote.
        let protocol_fee = |key: &str| {
            decode_u16(key, attribute(&state_attrs, key)?).map_err(InvalidSnapshotError::from)
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

/// Swap-related bits of the hooks registration bitmap (`IBinHooks.sol:10-23`): `beforeSwap`,
/// `afterSwap` and their returns-delta variants.
const SWAP_HOOK_BITS: u16 = (1 << 6) | (1 << 7) | (1 << 10) | (1 << 11);

/// `LPFeeLibrary.DYNAMIC_FEE_FLAG`: the LP fee comes from the hook.
const DYNAMIC_FEE_FLAG: u32 = 0x80_0000;

/// Whether the PoolKey `parameters` word registers a swap hook. The bitmap is its low 16 bits.
fn has_swap_hooks(parameters: &Bytes) -> Result<bool, InvalidSnapshotError> {
    let word: &[u8; 32] = parameters
        .as_ref()
        .try_into()
        .map_err(|_| {
            InvalidSnapshotError::ValueError(format!(
                "parameters is {} bytes, expected 32",
                parameters.len()
            ))
        })?;
    Ok(u16::from_be_bytes([word[30], word[31]]) & SWAP_HOOK_BITS != 0)
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

    /// PoolKey `parameters` with bin step 10 above the given hooks bitmap.
    fn parameters(hooks_bitmap: u16) -> Bytes {
        let mut word = vec![0u8; 32];
        word[28..30].copy_from_slice(&10u16.to_be_bytes());
        word[30..].copy_from_slice(&hooks_bitmap.to_be_bytes());
        Bytes::from(word)
    }

    /// Static attributes of a hookless static-fee pool with a 100-pip LP fee.
    fn statics() -> Vec<(&'static str, Bytes)> {
        vec![
            ("bin_step", Bytes::from(10u16.to_be_bytes().to_vec())),
            ("key_lp_fee", Bytes::from(vec![0x64])),
            ("parameters", parameters(0)),
        ]
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
            &statics(),
            &[
                ("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00])),
                ("protocol_fees/zero2one", Bytes::from(vec![0x03])),
                ("protocol_fees/one2zero", Bytes::from(vec![0x05])),
                (&format!("bins/{ACTIVE_ID}"), reserve_word(7, 11)),
            ],
        );

        let state = try_decode_snapshot_with_defaults::<PancakeswapInfinityBinState>(snapshot)
            .await
            .unwrap();

        assert_eq!(state.bin_step, 10);
        assert_eq!(state.active_id, ACTIVE_ID);
        assert_eq!(state.lp_fee, 100);
        assert_eq!(state.protocol_fee_zero_for_one, 3);
        assert_eq!(state.protocol_fee_one_for_zero, 5);
        assert_eq!(state.bins[&ACTIVE_ID], (7, 11));
    }

    /// The state `fee` is read before the static `key_lp_fee` it mirrors.
    #[tokio::test]
    async fn test_state_fee_takes_precedence_over_key_lp_fee() {
        let snapshot = snapshot(
            &statics(),
            &[
                ("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00])),
                ("protocol_fees/zero2one", Bytes::from(vec![0x03])),
                ("protocol_fees/one2zero", Bytes::from(vec![0x05])),
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
    #[case::missing_parameters("parameters", "parameters")]
    #[case::missing_protocol_fee_zero2one("protocol_fees/zero2one", "protocol_fees/zero2one")]
    #[case::missing_protocol_fee_one2zero("protocol_fees/one2zero", "protocol_fees/one2zero")]
    async fn test_missing_required_attribute_errors(#[case] removed: &str, #[case] reported: &str) {
        let statics: Vec<_> = statics()
            .into_iter()
            .filter(|(key, _)| *key != removed)
            .collect();
        let states: Vec<_> = [
            ("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00])),
            ("protocol_fees/zero2one", Bytes::from(vec![0x03])),
            ("protocol_fees/one2zero", Bytes::from(vec![0x03])),
        ]
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

    /// Pools the simulation would misquote, should one get past the substreams filter: any swap
    /// hook bit, a dynamic LP fee, or a `parameters` word that cannot be read.
    #[tokio::test]
    #[rstest]
    #[case::before_swap("parameters", parameters(1 << 6))]
    #[case::after_swap("parameters", parameters(1 << 7))]
    #[case::before_swap_returns_delta("parameters", parameters(1 << 10))]
    #[case::after_swap_returns_delta("parameters", parameters(1 << 11))]
    #[case::dynamic_lp_fee("key_lp_fee", Bytes::from(vec![0x80, 0x00, 0x00]))]
    #[case::short_parameters("parameters", Bytes::from(vec![0u8; 31]))]
    async fn test_rejects_pools_it_cannot_quote(#[case] key: &str, #[case] value: Bytes) {
        let mut statics = statics();
        for (name, existing) in &mut statics {
            if *name == key {
                *existing = value.clone();
            }
        }

        let result = try_decode_snapshot_with_defaults::<PancakeswapInfinityBinState>(snapshot(
            &statics,
            &[
                ("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00])),
                ("protocol_fees/zero2one", Bytes::from(vec![0x03])),
                ("protocol_fees/one2zero", Bytes::from(vec![0x05])),
            ],
        ))
        .await;

        assert!(
            matches!(result, Err(InvalidSnapshotError::ValueError(_))),
            "{key} = {value} decoded: {result:?}"
        );
    }

    /// Every other hook (initialize, mint, burn, donate, and the mint/burn returns-delta bits 12
    /// and 13) leaves swaps alone, so those pools still decode.
    #[tokio::test]
    async fn test_accepts_non_swap_hooks() {
        let mut statics = statics();
        statics[2].1 = parameters(0b11_0011_0011_1111);

        let result = try_decode_snapshot_with_defaults::<PancakeswapInfinityBinState>(snapshot(
            &statics,
            &[
                ("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00])),
                ("protocol_fees/zero2one", Bytes::from(vec![0x03])),
                ("protocol_fees/one2zero", Bytes::from(vec![0x05])),
            ],
        ))
        .await;

        assert!(result.is_ok(), "non-swap hooks rejected: {result:?}");
    }

    /// Bins with no reserves never enter the map; the swap walk would pay a crossing for them.
    #[tokio::test]
    async fn test_skips_empty_bins() {
        let snapshot = snapshot(
            &statics(),
            &[
                ("active_id", Bytes::from(vec![0x00, 0x80, 0x00, 0x00])),
                ("protocol_fees/zero2one", Bytes::from(vec![0x03])),
                ("protocol_fees/one2zero", Bytes::from(vec![0x05])),
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
