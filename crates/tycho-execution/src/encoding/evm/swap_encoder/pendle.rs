use std::{collections::HashMap, str::FromStr};

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{constants::ROUTER_ETH_ADDRESS, utils::bytes_to_address},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// The six Pendle edges: four through a market, two through its SY. The discriminants are
/// calldata — they must stay in step with the `PendleEdge` enum in `PendleExecutor.sol`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum PendleEdge {
    SyForPt = 0,
    PtForSy = 1,
    SyForYt = 2,
    YtForSy = 3,
    TokenForSy = 4,
    SyForToken = 5,
}

/// Encodes a swap on a Pendle market or on the SY behind it.
///
/// The protocol contributes two component types under one protocol system: a market, whose
/// static attributes name its SY, PT and YT, and an SY, whose id is the SY token itself. Which
/// of the six edges a swap takes follows from the component and the token pair, so nothing
/// about the edge has to be carried in the solution.
#[derive(Clone)]
pub struct PendleSwapEncoder {
    executor_address: Bytes,
    native_token_address: Bytes,
}

impl PendleSwapEncoder {
    fn market_edge(
        swap: &Swap,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<PendleEdge, EncodingError> {
        let attribute = |name: &str| -> Result<Bytes, EncodingError> {
            swap.component()
                .static_attributes
                .get(name)
                .cloned()
                .ok_or_else(|| {
                    EncodingError::FatalError(format!(
                        "Pendle market {} is missing the {} static attribute",
                        swap.component().id,
                        name
                    ))
                })
        };
        let sy = attribute("sy_address")?;
        let pt = attribute("pt_address")?;
        let yt = attribute("yt_address")?;

        if token_in == &sy && token_out == &pt {
            Ok(PendleEdge::SyForPt)
        } else if token_in == &pt && token_out == &sy {
            Ok(PendleEdge::PtForSy)
        } else if token_in == &sy && token_out == &yt {
            Ok(PendleEdge::SyForYt)
        } else if token_in == &yt && token_out == &sy {
            Ok(PendleEdge::YtForSy)
        } else {
            Err(EncodingError::InvalidInput(format!(
                "Pendle market {} does not trade {} for {}",
                swap.component().id,
                token_in,
                token_out
            )))
        }
    }

    fn sy_edge(
        sy: &Bytes,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<PendleEdge, EncodingError> {
        if token_out == sy {
            Ok(PendleEdge::TokenForSy)
        } else if token_in == sy {
            Ok(PendleEdge::SyForToken)
        } else {
            Err(EncodingError::InvalidInput(format!(
                "Pendle SY {sy} trades neither {token_in} nor {token_out}"
            )))
        }
    }
}

impl SwapEncoder for PendleSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Ethereum {
            return Err(EncodingError::FatalError(
                "Pendle swaps are only supported on Ethereum".to_string(),
            ));
        }
        let native_token_address = chain.native_token().address;
        Ok(Self { executor_address, native_token_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let token_in = &swap.token_in().address;
        let token_out = &swap.token_out().address;
        // Some SYs mint from native ETH, which the indexer drops because it has no `decimals()`
        // and so never classifies. The executor refuses a native leg by either name it can
        // carry, the chain's native address and the router's `0xEeee…` sentinel, so it is refused
        // here too rather than encoded into a revert.
        for token in [token_in, token_out] {
            if token == &self.native_token_address || token == &*ROUTER_ETH_ADDRESS {
                return Err(EncodingError::InvalidInput(
                    "Pendle native ETH legs are not supported; wrap first".to_string(),
                ));
            }
        }
        // The market edges are called on the market and the SY edges on the SY, and in both
        // cases that contract is the component itself.
        let target = Bytes::from_str(&swap.component().id).map_err(|_| {
            EncodingError::FatalError(format!(
                "Invalid Pendle component id {}",
                swap.component().id
            ))
        })?;

        // A market carries its SY, PT and YT as static attributes; an SY carries none of them and
        // lists itself among its tokens. The attributes decide first: a market is also an ERC-20,
        // its own LP token, so a pair naming the market would otherwise read as an SY edge and
        // revert on chain. A component that is neither is taken as a market, whose edge names
        // the attribute it is missing.
        let component = swap.component();
        let has_market_layout = ["sy_address", "pt_address", "yt_address"]
            .iter()
            .any(|name| {
                component
                    .static_attributes
                    .contains_key(*name)
            });
        let is_sy =
            token_in == &target || token_out == &target || component.tokens.contains(&target);
        let edge = if !has_market_layout && is_sy {
            Self::sy_edge(&target, token_in, token_out)?
        } else {
            Self::market_edge(swap, token_in, token_out)?
        };

        let args = (
            [edge as u8],
            bytes_to_address(token_in)?,
            bytes_to_address(token_out)?,
            bytes_to_address(&target)?,
        );
        Ok(args.abi_encode_packed())
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::models::default_token;

    // The wstETH market expiring 30 Dec 2027, its SY, PT and YT.
    const MARKET: &str = "0x34280882267ffa6383b363e278b027be083bbe3b";
    const SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
    const PT: &str = "0xb253eff1104802b97ac7e3ac9fdd73aece295a2c";
    const YT: &str = "0x04b7fa1e727d7290d6e24fa9b426d0c940283a95";
    const WSTETH: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";

    fn market_component() -> ProtocolComponent {
        ProtocolComponent {
            id: String::from(MARKET),
            protocol_system: String::from("pendle"),
            static_attributes: HashMap::from([
                ("sy_address".to_string(), Bytes::from_str(SY).unwrap()),
                ("pt_address".to_string(), Bytes::from_str(PT).unwrap()),
                ("yt_address".to_string(), Bytes::from_str(YT).unwrap()),
            ]),
            ..Default::default()
        }
    }

    fn sy_component() -> ProtocolComponent {
        ProtocolComponent {
            id: String::from(SY),
            protocol_system: String::from("pendle"),
            tokens: vec![Bytes::from_str(SY).unwrap(), Bytes::from_str(WSTETH).unwrap()],
            ..Default::default()
        }
    }

    fn encode(component: ProtocolComponent, token_in: &str, token_out: &str) -> String {
        let token_in = Bytes::from_str(token_in).unwrap();
        let token_out = Bytes::from_str(token_out).unwrap();
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
            origin: None,
        };
        let encoder = PendleSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            None,
        )
        .unwrap();
        alloy::hex::encode(
            encoder
                .encode_swap(&swap, &encoding_context)
                .unwrap(),
        )
    }

    #[test]
    fn test_encode_market_edges() {
        assert_eq!(
            encode(market_component(), SY, PT),
            format!("00{}{}{}", &SY[2..], &PT[2..], &MARKET[2..])
        );
        assert_eq!(
            encode(market_component(), PT, SY),
            format!("01{}{}{}", &PT[2..], &SY[2..], &MARKET[2..])
        );
        assert_eq!(
            encode(market_component(), SY, YT),
            format!("02{}{}{}", &SY[2..], &YT[2..], &MARKET[2..])
        );
        assert_eq!(
            encode(market_component(), YT, SY),
            format!("03{}{}{}", &YT[2..], &SY[2..], &MARKET[2..])
        );
    }

    #[test]
    fn test_encode_sy_edges() {
        assert_eq!(
            encode(sy_component(), WSTETH, SY),
            format!("04{}{}{}", &WSTETH[2..], &SY[2..], &SY[2..])
        );
        assert_eq!(
            encode(sy_component(), SY, WSTETH),
            format!("05{}{}{}", &SY[2..], &WSTETH[2..], &SY[2..])
        );
    }

    #[test]
    fn test_encode_rejects_pt_for_yt() {
        // PT and YT only trade against SY; there is no direct pair between them.
        let swap = Swap::new(
            market_component(),
            default_token(Bytes::from_str(PT).unwrap()),
            default_token(Bytes::from_str(YT).unwrap()),
            BigUint::ZERO,
        );
        let encoder = PendleSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();
        let error = encoder
            .encode_swap(
                &swap,
                &EncodingContext {
                    router_address: Some(Bytes::zero(20)),
                    group_token_in: Bytes::from_str(PT).unwrap(),
                    group_token_out: Bytes::from_str(YT).unwrap(),
                    origin: None,
                },
            )
            .unwrap_err();
        assert!(matches!(error, EncodingError::InvalidInput(_)));
    }

    #[test]
    fn test_encode_rejects_market_without_token_layout() {
        // A market whose static attributes never arrived must name what is missing rather
        // than fail somewhere downstream.
        let mut component = market_component();
        component.static_attributes.clear();
        let swap = Swap::new(
            component,
            default_token(Bytes::from_str(SY).unwrap()),
            default_token(Bytes::from_str(PT).unwrap()),
            BigUint::ZERO,
        );
        let encoder = PendleSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();
        let error = encoder
            .encode_swap(
                &swap,
                &EncodingContext {
                    router_address: Some(Bytes::zero(20)),
                    group_token_in: Bytes::from_str(SY).unwrap(),
                    group_token_out: Bytes::from_str(PT).unwrap(),
                    origin: None,
                },
            )
            .unwrap_err();
        assert!(matches!(error, EncodingError::FatalError(_)));
    }

    /// A market is also its own LP token. A pair naming it on a market component is not an SY
    /// edge, which would encode and then revert on chain; it is refused as no edge of the market.
    #[test]
    fn test_encode_rejects_the_market_lp_token_on_a_market() {
        let swap = Swap::new(
            market_component(),
            default_token(Bytes::from_str(MARKET).unwrap()),
            default_token(Bytes::from_str(SY).unwrap()),
            BigUint::ZERO,
        );
        let encoder = PendleSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();
        let error = encoder
            .encode_swap(
                &swap,
                &EncodingContext {
                    router_address: Some(Bytes::zero(20)),
                    group_token_in: Bytes::from_str(MARKET).unwrap(),
                    group_token_out: Bytes::from_str(SY).unwrap(),
                    origin: None,
                },
            )
            .unwrap_err();
        assert!(matches!(error, EncodingError::InvalidInput(_)), "{error:?}");
    }

    /// An SY trades only against itself, so a pair naming neither side as the SY is bad input,
    /// not a market missing its layout.
    #[test]
    fn test_encode_rejects_a_pair_the_sy_does_not_trade() {
        let swap = Swap::new(
            sy_component(),
            default_token(Bytes::from_str(WSTETH).unwrap()),
            default_token(Bytes::from_str(PT).unwrap()),
            BigUint::ZERO,
        );
        let encoder = PendleSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();
        let error = encoder
            .encode_swap(
                &swap,
                &EncodingContext {
                    router_address: Some(Bytes::zero(20)),
                    group_token_in: Bytes::from_str(WSTETH).unwrap(),
                    group_token_out: Bytes::from_str(PT).unwrap(),
                    origin: None,
                },
            )
            .unwrap_err();
        let EncodingError::InvalidInput(message) = error else {
            panic!("expected InvalidInput, got {error:?}");
        };
        assert!(message.contains("trades neither"), "{message}");
    }

    // SY-wstETH lists native ETH among its `tokensIn`; the integration quotes the wrapped legs
    // only, so a native leg must be refused rather than encoded into a revert, by either name.
    #[rstest::rstest]
    #[case::chain_native_address(Chain::Ethereum.native_token().address)]
    #[case::router_sentinel(ROUTER_ETH_ADDRESS.clone())]
    fn test_encode_rejects_native_eth(#[case] native: Bytes) {
        let swap = Swap::new(
            sy_component(),
            default_token(native.clone()),
            default_token(Bytes::from_str(SY).unwrap()),
            BigUint::ZERO,
        );
        let encoder = PendleSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();
        let error = encoder
            .encode_swap(
                &swap,
                &EncodingContext {
                    router_address: Some(Bytes::zero(20)),
                    group_token_in: native,
                    group_token_out: Bytes::from_str(SY).unwrap(),
                    origin: None,
                },
            )
            .unwrap_err();
        assert!(matches!(error, EncodingError::InvalidInput(_)));
    }
}
