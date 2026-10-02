use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{enums::FeeAmount, fee_tier::FeeTier, state::UniswapV3State};
use crate::{
    evm::protocol::{
        swap_quoter::AttachedComponent,
        utils::uniswap::{i24_be_bytes_to_i32, tick_list::TickInfo},
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for UniswapV3State {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `UniswapV3State`. Errors with a `InvalidSnapshotError`
    /// if the snapshot is missing any required attributes, if its fee and tick spacing are out of
    /// range, or if it has no `tick_spacing` attribute and its fee has no `FeeAmount` variant.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
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

        let fee = decode_fee_tier(&snapshot.component.static_attributes)?;

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

        let component = AttachedComponent::from_snapshot(&snapshot.component, all_tokens);
        UniswapV3State::new(liquidity, sqrt_price, fee, tick, ticks)
            .map(|state| state.with_component(component))
            .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
    }
}

/// Reads the pool's fee and tick spacing from its static attributes.
///
/// The `tick_spacing` attribute is authoritative when present, so any fee the factory enabled is
/// accepted. Without it the spacing is implied by the fee, which only works for the fee amounts
/// [`FeeAmount`] knows about.
fn decode_fee_tier(
    static_attributes: &HashMap<String, Bytes>,
) -> Result<FeeTier, InvalidSnapshotError> {
    let fee = static_attributes
        .get("fee")
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute("fee".to_string()))?;
    let fee = decode_i32("fee", fee)?;

    let Some(tick_spacing) = static_attributes.get("tick_spacing") else {
        return FeeAmount::try_from(fee)
            .map(FeeTier::from)
            .map_err(|_| InvalidSnapshotError::ValueError("Unsupported fee amount".to_string()));
    };
    let tick_spacing = decode_i32("tick_spacing", tick_spacing)?;

    let fee = u32::try_from(fee)
        .map_err(|_| InvalidSnapshotError::ValueError(format!("Negative fee {fee}")))?;
    let tick_spacing = u16::try_from(tick_spacing).map_err(|_| {
        InvalidSnapshotError::ValueError(format!("Tick spacing {tick_spacing} out of range"))
    })?;
    FeeTier::new(fee, tick_spacing).map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
}

fn decode_i32(name: &str, value: &Bytes) -> Result<i32, InvalidSnapshotError> {
    if value.len() > 4 {
        return Err(InvalidSnapshotError::ValueError(format!(
            "Attribute {name} is {} bytes, expected at most 4",
            value.len()
        )));
    }
    Ok(i32::from(value.clone()))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use chrono::DateTime;
    use rstest::rstest;
    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ChangeType,
    };

    use super::*;
    use crate::evm::protocol::test_utils::try_decode_snapshot_with_defaults;

    fn usv3_component() -> ProtocolComponent {
        let creation_time = DateTime::from_timestamp(1622526000, 0)
            .unwrap()
            .naive_utc(); //Sample timestamp

        // Add a static attribute "fee"
        let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
        static_attributes.insert("fee".to_string(), Bytes::from(3000_i32.to_be_bytes().to_vec()));

        ProtocolComponent {
            id: "State1".to_string(),
            protocol_system: "system1".to_string(),
            protocol_type_name: "typename1".to_string(),
            chain: Chain::Ethereum,
            tokens: Vec::new(),
            contract_addresses: Vec::new(),
            static_attributes,
            change: ChangeType::Creation,
            creation_tx: Bytes::from_str("0x0000").unwrap(),
            created_at: creation_time,
        }
    }

    fn usv3_attributes() -> HashMap<String, Bytes> {
        vec![
            ("liquidity".to_string(), Bytes::from(100_u64.to_be_bytes().to_vec())),
            ("sqrt_price_x96".to_string(), Bytes::from(200_u64.to_be_bytes().to_vec())),
            ("tick".to_string(), Bytes::from(300_i32.to_be_bytes().to_vec())),
            ("ticks/60/net_liquidity".to_string(), Bytes::from(400_i128.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect::<HashMap<String, Bytes>>()
    }

    #[tokio::test]
    async fn test_usv3_try_from() {
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: usv3_attributes(),
                balances: HashMap::new(),
            },
            component: usv3_component(),
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<UniswapV3State>(snapshot).await;

        assert!(result.is_ok());
        let expected = UniswapV3State::new(
            100,
            U256::from(200),
            FeeAmount::Medium,
            300,
            vec![TickInfo::new(60, 400).unwrap()],
        )
        .unwrap();
        assert_eq!(result.unwrap(), expected);
    }

    #[tokio::test]
    #[rstest]
    #[case::missing_liquidity("liquidity")]
    #[case::missing_sqrt_price("sqrt_price")]
    #[case::missing_tick("tick")]
    #[case::missing_tick_liquidity("tick_liquidities")]
    #[case::missing_fee("fee")]
    async fn test_usv3_try_from_invalid(#[case] missing_attribute: String) {
        // remove missing attribute
        let mut attributes = usv3_attributes();
        attributes.remove(&missing_attribute);

        if missing_attribute == "tick_liquidities" {
            attributes.remove("ticks/60/net_liquidity");
        }

        if missing_attribute == "sqrt_price" {
            attributes.remove("sqrt_price_x96");
        }

        let mut component = usv3_component();
        if missing_attribute == "fee" {
            component
                .static_attributes
                .remove("fee");
        }

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<UniswapV3State>(snapshot).await;

        assert!(result.is_err());
        assert!(matches!(
            result.err().unwrap(),
            InvalidSnapshotError::MissingAttribute(attr) if attr == missing_attribute
        ));
    }

    fn usv3_snapshot(static_attributes: &[(&str, Vec<u8>)], tick: i32) -> ComponentWithState {
        let mut component = usv3_component();
        for (name, value) in static_attributes {
            component
                .static_attributes
                .insert(name.to_string(), Bytes::from(value.clone()));
        }
        let mut attributes = usv3_attributes();
        attributes.remove("ticks/60/net_liquidity");
        attributes.insert(
            format!("ticks/{tick}/net_liquidity"),
            Bytes::from(400_i128.to_be_bytes().to_vec()),
        );
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[tokio::test]
    #[rstest]
    // Substreams emit `fee` and `tick_spacing` as minimal big-endian signed bytes.
    #[case::fee_outside_fee_amount(vec![0x32], vec![0x0a], 50, 10, 20)]
    #[case::spacing_differs_from_fee_amount(vec![0x00, 0xc8], vec![0x04], 200, 4, 8)]
    #[case::large_fee_small_spacing(vec![0x75, 0x30], vec![0x01], 30_000, 1, 7)]
    async fn test_usv3_try_from_uses_tick_spacing_attribute(
        #[case] fee_bytes: Vec<u8>,
        #[case] tick_spacing_bytes: Vec<u8>,
        #[case] fee: u32,
        #[case] tick_spacing: u16,
        #[case] tick: i32,
    ) {
        let snapshot =
            usv3_snapshot(&[("fee", fee_bytes), ("tick_spacing", tick_spacing_bytes)], tick);

        let result = try_decode_snapshot_with_defaults::<UniswapV3State>(snapshot).await;

        let expected = UniswapV3State::new(
            100,
            U256::from(200),
            FeeTier::new(fee, tick_spacing).unwrap(),
            300,
            vec![TickInfo::new(tick, 400).unwrap()],
        )
        .unwrap();
        assert_eq!(result.unwrap(), expected);
    }

    #[tokio::test]
    async fn test_usv3_try_from_rejects_tick_off_attribute_spacing() {
        // Fee 200 implies spacing 2, so tick 2 is only invalid if the attribute's spacing 4 wins.
        let snapshot = usv3_snapshot(&[("fee", vec![0x00, 0xc8]), ("tick_spacing", vec![0x04])], 2);

        let result = try_decode_snapshot_with_defaults::<UniswapV3State>(snapshot).await;

        assert!(matches!(
            result,
            Err(InvalidSnapshotError::ValueError(err)) if err.contains("not aligned")
        ));
    }

    #[tokio::test]
    #[rstest]
    #[case::negative_fee(vec![0xff], vec![0x01], "Negative fee")]
    #[case::fee_at_denominator(vec![0x0f, 0x42, 0x40], vec![0x01], "must be below")]
    #[case::zero_spacing(vec![0x32], vec![0x00], "must be positive")]
    #[case::negative_spacing(vec![0x32], vec![0xff], "out of range")]
    #[case::spacing_above_u16(vec![0x32], vec![0x01, 0x00, 0x00], "out of range")]
    #[case::spacing_wider_than_i32(vec![0x32], vec![0x00; 5], "expected at most 4")]
    async fn test_usv3_try_from_invalid_tick_spacing_attribute(
        #[case] fee_bytes: Vec<u8>,
        #[case] tick_spacing_bytes: Vec<u8>,
        #[case] expected_error: &str,
    ) {
        let snapshot =
            usv3_snapshot(&[("fee", fee_bytes), ("tick_spacing", tick_spacing_bytes)], 0);

        let result = try_decode_snapshot_with_defaults::<UniswapV3State>(snapshot).await;

        assert!(matches!(
            result,
            Err(InvalidSnapshotError::ValueError(err)) if err.contains(expected_error)
        ));
    }

    #[tokio::test]
    async fn test_usv3_try_from_invalid_fee() {
        // Without a `tick_spacing` attribute only `FeeAmount` fees can be decoded.
        let mut component = usv3_component();
        component
            .static_attributes
            .insert("fee".to_string(), Bytes::from(4000_i32.to_be_bytes().to_vec()));

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: usv3_attributes(),
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<UniswapV3State>(snapshot).await;

        assert!(result.is_err());
        assert!(matches!(
            result.err().unwrap(),
            InvalidSnapshotError::ValueError(err) if err == *"Unsupported fee amount"
        ));
    }
}
