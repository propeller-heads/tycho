use std::collections::HashMap;

use alloy::sol_types::SolValue;
use serde::Deserialize;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Static attribute under which fallback components carry their pAMM venue address — the same
/// attribute the price-level-stream family uses.
const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// The highest Uniswap V2 fee `TychoFallbackRouter` accepts (`feeBps <= 30`).
const MAX_UNISWAP_V2_FEE_BPS: u8 = 30;

/// The fallback venue that fills a pAMM swap when the pAMM fails, JSON-encoded into
/// `Swap::user_data`. Required: `TychoFallbackRouter` rejects a swap without one, so the
/// solver must pick the venue and its pool when it builds the solution.
///
/// The variants mirror `TychoFallbackRouter.Venue`; the wire format is the venue byte followed
/// by the venue data the contract decodes:
///
/// | Venue        | JSON                                                       | Venue data |
/// |--------------|------------------------------------------------------------|------------|
/// | `uniswap_v2` | `{"protocol":"uniswap_v2","pair":"0x…","fee_bps":30}`         | `pair(20) ++ fee_bps(1)` |
/// | `uniswap_v3` | `{"protocol":"uniswap_v3","pool":"0x…"}`                      | `pool(20)` |
/// | `uniswap_v4` | `{"protocol":"uniswap_v4","fee":3000,"tick_spacing":60,"hook":"0x…","hook_data":"0x…"}` | `fee(3) ++ tick_spacing(3) ++ hook(20) ++ hook_data` |
/// | `curve`      | `{"protocol":"curve","pool":"0x…","pool_type":1,"i":0,"j":1}` | `pool(20) ++ pool_type(1) ++ i(1) ++ j(1)` |
/// | `fluid_v1`   | `{"protocol":"fluid_v1","dex":"0x…","zero2one":true}`         | `dex(20) ++ zero2one(1)` |
///
/// Swap direction on Uniswap V2/V3/V4 comes from the sort order of the swap's tokens, so it is
/// not part of the JSON. Fluid's `zero2one` is the dex's own token order, which cannot be
/// derived, and Curve's `i`/`j` are the pool's coin indices.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
enum FallbackProtocol {
    UniswapV2 {
        pair: Bytes,
        fee_bps: u8,
    },
    UniswapV3 {
        pool: Bytes,
    },
    UniswapV4 {
        fee: u32,
        tick_spacing: i32,
        hook: Bytes,
        #[serde(default)]
        hook_data: Bytes,
    },
    Curve {
        pool: Bytes,
        pool_type: u8,
        i: u8,
        j: u8,
    },
    FluidV1 {
        dex: Bytes,
        zero2one: bool,
    },
}

impl FallbackProtocol {
    fn from_swap_user_data(user_data: &Option<Bytes>) -> Result<Self, EncodingError> {
        match user_data.as_ref() {
            Some(bytes) if !bytes.is_empty() => serde_json::from_slice(bytes).map_err(|e| {
                EncodingError::FatalError(format!("Invalid fallback venue user_data JSON: {e}"))
            }),
            _ => Err(EncodingError::FatalError(
                "Fallback swaps require user_data naming the fallback venue \
                 (e.g. {\"protocol\":\"uniswap_v3\",\"pool\":\"0x…\"})"
                    .to_string(),
            )),
        }
    }

    /// The ordinal of the matching `TychoFallbackRouter.Venue` variant — the wire format's
    /// venue byte.
    fn protocol_byte(&self) -> u8 {
        match self {
            FallbackProtocol::UniswapV2 { .. } => 0,
            FallbackProtocol::UniswapV3 { .. } => 1,
            FallbackProtocol::UniswapV4 { .. } => 2,
            FallbackProtocol::Curve { .. } => 3,
            FallbackProtocol::FluidV1 { .. } => 4,
        }
    }

    /// Packs the venue byte of `TychoFallbackRouter.Venue` followed by the venue data its
    /// `_executeFallback` decodes, validating what the contract would revert on.
    fn encode(&self) -> Result<Vec<u8>, EncodingError> {
        let mut data = vec![self.protocol_byte()];
        match self {
            FallbackProtocol::UniswapV2 { pair, fee_bps } => {
                if *fee_bps > MAX_UNISWAP_V2_FEE_BPS {
                    return Err(EncodingError::InvalidInput(format!(
                        "Uniswap V2 fallback fee is {fee_bps} bps, the fallback router accepts \
                         at most {MAX_UNISWAP_V2_FEE_BPS}"
                    )));
                }
                data.extend_from_slice(bytes_to_address(pair)?.as_slice());
                data.push(*fee_bps);
            }
            FallbackProtocol::UniswapV3 { pool } => {
                data.extend_from_slice(bytes_to_address(pool)?.as_slice());
            }
            FallbackProtocol::UniswapV4 { fee, tick_spacing, hook, hook_data } => {
                if *fee >= 1 << 24 {
                    return Err(EncodingError::InvalidInput(format!(
                        "Uniswap V4 fallback fee {fee} does not fit uint24"
                    )));
                }
                if *tick_spacing < -(1 << 23) || *tick_spacing >= 1 << 23 {
                    return Err(EncodingError::InvalidInput(format!(
                        "Uniswap V4 fallback tick spacing {tick_spacing} does not fit int24"
                    )));
                }
                data.extend_from_slice(&fee.to_be_bytes()[1..]);
                data.extend_from_slice(&tick_spacing.to_be_bytes()[1..]);
                data.extend_from_slice(bytes_to_address(hook)?.as_slice());
                data.extend_from_slice(hook_data.as_ref());
            }
            FallbackProtocol::Curve { pool, pool_type, i, j } => {
                data.extend_from_slice(bytes_to_address(pool)?.as_slice());
                data.extend_from_slice(&[*pool_type, *i, *j]);
            }
            FallbackProtocol::FluidV1 { dex, zero2one } => {
                data.extend_from_slice(bytes_to_address(dex)?.as_slice());
                data.push(u8::from(*zero2one));
            }
        }
        Ok(data)
    }
}

/// Encodes a swap that runs a pAMM through `TychoFallbackRouter` so a failing pAMM retries on
/// the fallback venue named in the swap's `user_data` instead of reverting the route.
///
/// The pAMM address comes from the component's `pamm_address` static attribute, which every
/// fallback component carries. Swap data for `FallbackExecutor` is packed
/// `token_in ++ token_out ++ pamm ++ protocol_byte ++ venue_data` (see [`FallbackProtocol`]).
#[derive(Clone)]
pub struct FallbackSwapEncoder {
    executor_address: Bytes,
}

impl FallbackSwapEncoder {
    fn pamm_address(swap: &Swap) -> Result<Bytes, EncodingError> {
        let component = swap.component();
        component
            .static_attributes
            .get(PAMM_ADDRESS_ATTRIBUTE)
            .cloned()
            .ok_or_else(|| {
                EncodingError::FatalError(format!(
                    "Fallback component {} is missing the {PAMM_ADDRESS_ATTRIBUTE} static \
                     attribute",
                    component.id
                ))
            })
    }
}

impl SwapEncoder for FallbackSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Ethereum {
            return Err(EncodingError::FatalError(
                "Fallback swaps are only supported on Ethereum".to_string(),
            ));
        }

        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let venue = FallbackProtocol::from_swap_user_data(swap.user_data())?;
        let pamm = bytes_to_address(&Self::pamm_address(swap)?)?;
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let mut data = (token_in, token_out, pamm).abi_encode_packed();
        data.extend(venue.encode()?);
        Ok(data)
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
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::models::default_token;

    // The addresses below match the Fallback.t.sol fixtures so that test can reuse them.
    const PAMM: &str = "1111111111111111111111111111111111111111";
    const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const WETH: &str = "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC_WETH_USV3: &str = "88e6a0c2ddd26feeb64f039a2c41296fcb3f5640";

    fn usdc_weth_component() -> ProtocolComponent {
        ProtocolComponent {
            // The id the price level stream produces: pamm ++ token0 ++ token1.
            id: format!("0x{PAMM}{USDC}{WETH}"),
            protocol_system: String::from("fallback:kipseli"),
            static_attributes: HashMap::from([(
                PAMM_ADDRESS_ATTRIBUTE.to_string(),
                Bytes::from(format!("0x{PAMM}").as_str()),
            )]),
            ..Default::default()
        }
    }

    fn encoder() -> FallbackSwapEncoder {
        FallbackSwapEncoder::new(Bytes::default(), Chain::Ethereum, None).unwrap()
    }

    fn encode_usdc_weth(user_data: Option<&str>) -> Result<String, EncodingError> {
        let token_in = Bytes::from(format!("0x{USDC}").as_str());
        let token_out = Bytes::from(format!("0x{WETH}").as_str());
        let mut swap = Swap::new(
            usdc_weth_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        if let Some(data) = user_data {
            swap = swap.with_user_data(Bytes::from(data.as_bytes()));
        }
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };

        encoder()
            .encode_swap(&swap, &encoding_context)
            .map(|encoded| encode(&encoded))
    }

    #[test]
    fn test_encode_uniswap_v3_fallback() {
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}01{USDC_WETH_USV3}"));
    }

    #[test]
    fn test_encode_uniswap_v2_fallback() {
        let pair = "b4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"protocol":"uniswap_v2","pair":"0x{pair}","fee_bps":30}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}00{pair}1e"));
    }

    #[test]
    fn test_encode_uniswap_v4_fallback() {
        let hex_swap = encode_usdc_weth(Some(
            r#"{"protocol":"uniswap_v4","fee":3000,"tick_spacing":-60,
                "hook":"0x2222222222222222222222222222222222222222","hook_data":"0xdeadbeef"}"#,
        ))
        .unwrap();

        // fee 3000 = 0x000bb8; tick spacing -60 = 0xffffc4 in int24 two's complement.
        assert_eq!(
            hex_swap,
            format!(
                "{USDC}{WETH}{PAMM}02000bb8ffffc42222222222222222222222222222222222222222deadbeef"
            )
        );
    }

    #[test]
    fn test_encode_uniswap_v4_fallback_without_hook_data() {
        let hex_swap = encode_usdc_weth(Some(
            r#"{"protocol":"uniswap_v4","fee":500,"tick_spacing":10,
                "hook":"0x0000000000000000000000000000000000000000"}"#,
        ))
        .unwrap();

        assert_eq!(
            hex_swap,
            format!("{USDC}{WETH}{PAMM}020001f400000a0000000000000000000000000000000000000000")
        );
    }

    #[test]
    fn test_encode_curve_fallback() {
        let pool = "3333333333333333333333333333333333333333";
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"protocol":"curve","pool":"0x{pool}","pool_type":1,"i":0,"j":2}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}03{pool}010002"));
    }

    #[test]
    fn test_encode_fluid_v1_fallback() {
        let dex = "4444444444444444444444444444444444444444";
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"protocol":"fluid_v1","dex":"0x{dex}","zero2one":true}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}04{dex}01"));
    }

    #[test]
    fn test_rejects_missing_user_data() {
        let err = encode_usdc_weth(None).unwrap_err();
        assert!(matches!(err, EncodingError::FatalError(msg) if msg.contains("user_data")));
    }

    #[test]
    fn test_rejects_unknown_venue() {
        let err =
            encode_usdc_weth(Some(r#"{"protocol":"balancer_v2","pool":"0x11"}"#)).unwrap_err();
        assert!(matches!(err, EncodingError::FatalError(msg) if msg.contains("JSON")));
    }

    #[test]
    fn test_rejects_uniswap_v2_fee_above_cap() {
        let pair = "b4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let err = encode_usdc_weth(Some(&format!(
            r#"{{"protocol":"uniswap_v2","pair":"0x{pair}","fee_bps":31}}"#
        )))
        .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("31")));
    }

    #[test]
    fn test_rejects_uniswap_v4_fee_overflowing_uint24() {
        let err = encode_usdc_weth(Some(
            r#"{"protocol":"uniswap_v4","fee":16777216,"tick_spacing":60,
                "hook":"0x0000000000000000000000000000000000000000"}"#,
        ))
        .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("uint24")));
    }

    #[test]
    fn test_rejects_uniswap_v4_tick_spacing_overflowing_int24() {
        let err = encode_usdc_weth(Some(
            r#"{"protocol":"uniswap_v4","fee":500,"tick_spacing":8388608,
                "hook":"0x0000000000000000000000000000000000000000"}"#,
        ))
        .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("int24")));
    }

    #[test]
    fn test_rejects_component_without_pamm_address() {
        let mut component = usdc_weth_component();
        component.static_attributes.clear();
        let swap = Swap::new(
            component,
            default_token(Bytes::from(format!("0x{USDC}").as_str())),
            default_token(Bytes::from(format!("0x{WETH}").as_str())),
            BigUint::ZERO,
        )
        .with_user_data(Bytes::from(
            format!(r#"{{"protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#).into_bytes(),
        ));
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(format!("0x{USDC}").as_str()),
            group_token_out: Bytes::from(format!("0x{WETH}").as_str()),
        };

        let result = encoder().encode_swap(&swap, &encoding_context);
        assert!(
            matches!(result, Err(EncodingError::FatalError(msg)) if msg.contains(PAMM_ADDRESS_ATTRIBUTE))
        );
    }

    #[test]
    fn test_encoder_rejects_non_ethereum_chain() {
        let result = FallbackSwapEncoder::new(Bytes::zero(20), Chain::Base, None);
        assert!(matches!(result, Err(EncodingError::FatalError(msg)) if msg.contains("Ethereum")));
    }
}
