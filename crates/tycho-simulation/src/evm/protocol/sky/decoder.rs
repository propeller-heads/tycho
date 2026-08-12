use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::{JoinEscrows, SkyComponentKind, SkyState};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for SkyState {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `SkyState`, dispatching on the
    /// `component_type` static attribute (`psm`, `psm_wrapper` or `converter`).
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let kind = match snapshot
            .component
            .static_attributes
            .get("component_type")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("component_type".to_string()))?
            .as_ref()
        {
            b"psm" => SkyComponentKind::Psm,
            b"psm_wrapper" => SkyComponentKind::PsmWrapper,
            b"converter" => SkyComponentKind::Converter,
            other => {
                return Err(InvalidSnapshotError::ValueError(format!(
                    "unknown sky component_type: {}",
                    String::from_utf8_lossy(other)
                )))
            }
        };

        let tokens = snapshot
            .component
            .tokens
            .iter()
            .map(|address| {
                all_tokens
                    .get(address)
                    .cloned()
                    .ok_or_else(|| {
                        InvalidSnapshotError::ValueError(format!("unknown token {address}"))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let [a, b] = tokens.try_into().map_err(|_| {
            InvalidSnapshotError::ValueError("sky components have exactly two tokens".to_string())
        })?;

        // Token roles come from the `gem` static attribute (also the swap encoder's
        // direction source), so the token list order does not matter.
        let gem_address = snapshot
            .component
            .static_attributes
            .get("gem")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("gem".to_string()))?;
        let (stable, gem) = if b.address == *gem_address {
            (a, b)
        } else if a.address == *gem_address {
            (b, a)
        } else {
            return Err(InvalidSnapshotError::ValueError(format!(
                "gem attribute {gem_address} is not among the component tokens"
            )));
        };

        let get_fee = |name: &str| -> Result<U256, InvalidSnapshotError> {
            match kind {
                // The converter is immutable and feeless; no attributes are indexed.
                SkyComponentKind::Converter => Ok(U256::ZERO),
                SkyComponentKind::Psm | SkyComponentKind::PsmWrapper => snapshot
                    .state
                    .attributes
                    .get(name)
                    .map(|value| U256::from_be_slice(value))
                    .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string())),
            }
        };
        let get_balance = |token: &Token| -> Result<U256, InvalidSnapshotError> {
            snapshot
                .state
                .balances
                .get(&token.address)
                .map(|balance| U256::from_be_slice(balance))
                .ok_or_else(|| {
                    // Every sky component carries both balances from creation (the
                    // substreams package seeds them explicitly), so absence means a
                    // corrupt or stale snapshot, not a zero balance.
                    InvalidSnapshotError::ValueError(format!(
                        "missing balance for token {} on component {}",
                        token.address, snapshot.component.id
                    ))
                })
        };

        let (stable_balance, gem_balance) = (get_balance(&stable)?, get_balance(&gem)?);

        let get_escrow = |name: &str| -> Result<U256, InvalidSnapshotError> {
            snapshot
                .state
                .attributes
                .get(name)
                .map(|value| U256::from_be_slice(value))
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_string()))
        };
        // The wrapper's in-flight DAI <-> USDS conversion is bounded by the join
        // escrows, seeded at creation like the fees.
        let escrows = match kind {
            SkyComponentKind::PsmWrapper => Some(JoinEscrows {
                dai: get_escrow("dai_escrow")?,
                usds: get_escrow("usds_escrow")?,
            }),
            SkyComponentKind::Psm | SkyComponentKind::Converter => None,
        };

        Ok(SkyState::new(
            snapshot.component.id.to_string(),
            kind,
            stable,
            gem,
            get_fee("tin")?,
            get_fee("tout")?,
            stable_balance,
            gem_balance,
            escrows,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;
    use tycho_common::{
        models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            Chain,
        },
        Bytes,
    };

    use super::*;

    async fn decode(
        snapshot: ComponentWithState,
        all_tokens: &HashMap<Bytes, Token>,
    ) -> Result<SkyState, InvalidSnapshotError> {
        SkyState::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            all_tokens,
            &Default::default(),
        )
        .await
    }

    const PSM_ID: &str = "0xf6e72db5454dd049d0788e411b06cfaf16853042";
    const DAI: &str = "0x6b175474e89094c44da98b954eedeac495271d0f";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const USDS: &str = "0xdc035d45d973e3ec169d2276ddab16f1e407384f";

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(50_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn all_tokens() -> HashMap<Bytes, Token> {
        [token(DAI, "DAI", 18), token(USDC, "USDC", 6), token(USDS, "USDS", 18)]
            .into_iter()
            .map(|t| (t.address.clone(), t))
            .collect()
    }

    fn snapshot(
        id: &str,
        component_type: &str,
        gem: &str,
        tokens: Vec<&str>,
        with_fees: bool,
    ) -> ComponentWithState {
        let attributes = if with_fees {
            HashMap::from([
                ("tin".to_string(), Bytes::from(U256::ZERO.to_be_bytes_vec())),
                ("tout".to_string(), Bytes::from(U256::from(2u8).to_be_bytes_vec())),
            ])
        } else {
            HashMap::new()
        };
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: id.to_string(),
                attributes,
                balances: HashMap::from([
                    (
                        Bytes::from_str(tokens[0]).unwrap(),
                        Bytes::from(U256::from(1000u64).to_be_bytes_vec()),
                    ),
                    (
                        Bytes::from_str(tokens[1]).unwrap(),
                        Bytes::from(U256::from(2000u64).to_be_bytes_vec()),
                    ),
                ]),
            },
            component: ProtocolComponent {
                id: id.to_string(),
                tokens: tokens
                    .iter()
                    .map(|t| Bytes::from_str(t).unwrap())
                    .collect(),
                static_attributes: HashMap::from([
                    ("component_type".to_string(), Bytes::from(component_type.as_bytes().to_vec())),
                    ("gem".to_string(), Bytes::from_str(gem).unwrap()),
                ]),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[rstest]
    #[case::stable_first(vec![DAI, USDC])]
    #[case::gem_first(vec![USDC, DAI])]
    #[tokio::test]
    async fn decodes_psm_regardless_of_token_order(#[case] tokens: Vec<&str>) {
        let snap = snapshot(PSM_ID, "psm", USDC, tokens, true);
        let state = decode(snap, &all_tokens())
            .await
            .unwrap();
        assert_eq!(state.kind, SkyComponentKind::Psm);
    }

    #[tokio::test]
    async fn decodes_converter_without_fee_attributes() {
        let snap = snapshot(
            "0x3225737a9bbb6473cb4a45b7244aca2befdb276a",
            "converter",
            USDS,
            vec![DAI, USDS],
            false,
        );
        let state = decode(snap, &all_tokens())
            .await
            .unwrap();
        assert_eq!(state.kind, SkyComponentKind::Converter);
    }

    fn wrapper_snapshot() -> ComponentWithState {
        let mut snap = snapshot(
            "0xa188eec8f81263234da3622a406892f3d630f98c",
            "psm_wrapper",
            USDC,
            vec![USDS, USDC],
            true,
        );
        snap.state.attributes.extend([
            ("dai_escrow".to_string(), Bytes::from(U256::from(7u8).to_be_bytes_vec())),
            ("usds_escrow".to_string(), Bytes::from(U256::from(9u8).to_be_bytes_vec())),
        ]);
        snap
    }

    #[tokio::test]
    async fn decodes_wrapper_with_join_escrows() {
        let state = decode(wrapper_snapshot(), &all_tokens())
            .await
            .unwrap();
        assert_eq!(state.kind, SkyComponentKind::PsmWrapper);
    }

    #[rstest]
    #[case::dai_escrow("dai_escrow")]
    #[case::usds_escrow("usds_escrow")]
    #[tokio::test]
    async fn missing_escrow_attribute_errors_for_wrapper(#[case] name: &str) {
        let mut snap = wrapper_snapshot();
        snap.state.attributes.remove(name);
        let result = decode(snap, &all_tokens()).await;
        assert!(matches!(result, Err(InvalidSnapshotError::MissingAttribute(_))));
    }

    #[tokio::test]
    async fn missing_fee_attribute_errors_for_psm() {
        let snap = snapshot(PSM_ID, "psm", USDC, vec![DAI, USDC], false);
        let result = decode(snap, &all_tokens()).await;
        assert!(matches!(result, Err(InvalidSnapshotError::MissingAttribute(_))));
    }

    #[tokio::test]
    async fn unknown_component_type_errors() {
        let snap = snapshot(PSM_ID, "mystery", USDC, vec![DAI, USDC], true);
        let result = decode(snap, &all_tokens()).await;
        assert!(matches!(result, Err(InvalidSnapshotError::ValueError(_))));
    }

    #[tokio::test]
    async fn missing_gem_attribute_errors() {
        let mut snap = snapshot(PSM_ID, "psm", USDC, vec![DAI, USDC], true);
        snap.component
            .static_attributes
            .remove("gem");
        let result = decode(snap, &all_tokens()).await;
        assert!(matches!(result, Err(InvalidSnapshotError::MissingAttribute(_))));
    }

    #[tokio::test]
    async fn missing_balance_errors() {
        let mut snap = snapshot(PSM_ID, "psm", USDC, vec![DAI, USDC], true);
        snap.state
            .balances
            .remove(&Bytes::from_str(DAI).unwrap());
        let result = decode(snap, &all_tokens()).await;
        assert!(matches!(result, Err(InvalidSnapshotError::ValueError(_))));
    }
}
