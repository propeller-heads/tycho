use std::collections::{BTreeMap, HashMap};

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::protocol::{
        fluid_v2::{
            state::{
                decode_net_liquidity, decode_paused, decode_word, parse_tick_key, DexType,
                DexVariables, DexVariables2, FluidV2State,
            },
            vm::TokenLiquidity,
        },
        utils::bytes_to_address,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for FluidV2State {
    type Error = InvalidSnapshotError;

    /// Decodes a pool from its indexed attributes, then reads both tokens' Liquidity state through
    /// the DEX v2 resolver, and whether Liquidity is paused, in the shared VM database.
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

        let word = |name: &str, value: &Bytes| {
            decode_word(name, value).map_err(InvalidSnapshotError::ValueError)
        };
        let address = |name: &str| {
            bytes_to_address(static_attribute(name)?)
                .map_err(|e| InvalidSnapshotError::ValueError(format!("Fluid v2 {name}: {e}")))
        };

        let dex_type = match u64::try_from(word("dex_type", static_attribute("dex_type")?)?) {
            Ok(3) => DexType::D3,
            Ok(4) => DexType::D4,
            _ => {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "Unsupported Fluid v2 dex type {}",
                    static_attribute("dex_type")?
                )))
            }
        };
        let tick_spacing = i32::try_from(word("tick_spacing", static_attribute("tick_spacing")?)?)
            .ok()
            .filter(|tick_spacing| *tick_spacing > 0)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(
                    "Fluid v2 tick spacing must be positive".to_string(),
                )
            })?;

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

        let mut state = FluidV2State {
            dex_type,
            token0: token(0)?,
            token1: token(1)?,
            resolver: address("resolver")?,
            liquidity: address("liquidity")?,
            tick_spacing,
            dex_variables: DexVariables::from_packed(word(
                "dex_variables",
                attribute("dex_variables")?,
            )?),
            dex_variables2: DexVariables2::from_packed(word(
                "dex_variables2",
                attribute("dex_variables2")?,
            )?),
            token0_reserve: word("token0/token_reserves", attribute("token0/token_reserves")?)?,
            token1_reserve: word("token1/token_reserves", attribute("token1/token_reserves")?)?,
            ticks,
            token0_liquidity: TokenLiquidity::default(),
            token1_liquidity: TokenLiquidity::default(),
            liquidity_paused: false,
            paused: attributes
                .get("paused")
                .is_some_and(decode_paused),
        };
        state
            .refresh_liquidity_state()
            .map_err(InvalidSnapshotError::VMError)?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain,
    };

    use super::*;

    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const USDT: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";

    fn tokens() -> HashMap<Bytes, Token> {
        [USDC, USDT]
            .into_iter()
            .map(|address| {
                let address = Bytes::from_str(address).unwrap();
                let token = Token::new(&address, "T", 6, 0, &[Some(20_000)], Chain::Ethereum, 100);
                (address, token)
            })
            .collect()
    }

    /// A snapshot as the package emits it, with `name` replaced by `value`, or removed.
    fn snapshot(name: &str, value: Option<Bytes>) -> ComponentWithState {
        let mut static_attributes = HashMap::from([
            ("dex_type".to_string(), Bytes::from(vec![3])),
            ("fee".to_string(), Bytes::from(vec![0x64])),
            ("tick_spacing".to_string(), Bytes::from(vec![1])),
            ("controller".to_string(), Bytes::from(vec![0u8; 20])),
            (
                "resolver".to_string(),
                Bytes::from_str("0x143c3927EaF0e50a59b1651C2Eb1e37FB62Aa2a9").unwrap(),
            ),
            (
                "liquidity".to_string(),
                Bytes::from_str("0x52Aa899454998Be5b000Ad077a46Bbe360F4e497").unwrap(),
            ),
        ]);
        let mut attributes = HashMap::from([
            ("dex_variables".to_string(), Bytes::from(vec![0u8; 32])),
            ("dex_variables2".to_string(), Bytes::from(vec![0u8; 32])),
            ("token0/token_reserves".to_string(), Bytes::from(vec![0u8; 16])),
            ("token1/token_reserves".to_string(), Bytes::from(vec![0u8; 16])),
        ]);
        let map = if static_attributes.contains_key(name) {
            &mut static_attributes
        } else {
            &mut attributes
        };
        match value {
            Some(value) => map.insert(name.to_string(), value),
            None => map.remove(name),
        };
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: "0x03".to_string(),
                attributes,
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                tokens: vec![Bytes::from_str(USDC).unwrap(), Bytes::from_str(USDT).unwrap()],
                static_attributes,
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    async fn decode(snapshot: ComponentWithState) -> Result<FluidV2State, InvalidSnapshotError> {
        FluidV2State::try_from_with_header(
            snapshot,
            BlockHeader::default(),
            &HashMap::new(),
            &tokens(),
            &DecoderContext::new(),
        )
        .await
    }

    #[rstest]
    #[case::liquidity("liquidity")]
    #[case::resolver("resolver")]
    #[case::dex_variables("dex_variables")]
    #[case::token1_reserves("token1/token_reserves")]
    #[tokio::test]
    async fn missing_attribute_is_an_error(#[case] name: &str) {
        let err = decode(snapshot(name, None))
            .await
            .unwrap_err();

        assert!(matches!(err, InvalidSnapshotError::MissingAttribute(missing) if missing == name));
    }

    #[rstest]
    #[case::dex_type_wider_than_a_word("dex_type", vec![3u8; 33])]
    #[case::dex_type_not_d3_or_d4("dex_type", vec![1])]
    #[case::dex_type_above_u64("dex_type", vec![1u8; 9])]
    #[case::zero_tick_spacing("tick_spacing", vec![0])]
    #[case::short_resolver("resolver", vec![1u8; 19])]
    #[case::long_liquidity("liquidity", vec![1u8; 21])]
    #[case::dex_variables_wider_than_a_word("dex_variables", vec![1u8; 33])]
    #[tokio::test]
    async fn malformed_attribute_is_an_error(#[case] name: &str, #[case] value: Vec<u8>) {
        let err = decode(snapshot(name, Some(Bytes::from(value))))
            .await
            .unwrap_err();

        assert!(matches!(err, InvalidSnapshotError::ValueError(_)), "{err:?}");
    }
}
