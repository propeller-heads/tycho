use std::{collections::HashMap, str::FromStr, sync::LazyLock};

use alloy::sol_types::SolValue;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use strum_macros::{EnumIter, EnumString, IntoStaticStr};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::{SLIPSTREAMS_FORKS, UNISWAP_V2_FORKS, UNISWAP_V3_FORKS},
        utils::bytes_to_address,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Static attribute holding a fallback component's pAMM address.
const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// The highest Uniswap V2 fee `TychoFallbackRouter` accepts, in bps.
const MAX_UNISWAP_V2_FEE_BPS: u8 = 30;

/// A protocol `TychoFallbackRouter` can fall back on. Mirrors the contract's `FallbackProtocol`
/// enum: the discriminant is the protocol byte, and the snake-case variant name is the
/// `fallback_protocol` tag in `user_data` and the [`FallbackSwapData`] variant name.
///
/// [`FallbackSwapData`] is the public half of that contract: a solver builds the variant for the
/// pool it chose and serializes it into the swap's `user_data`.
///
/// To add a protocol: add the variant last, matching the contract enum; a [`forks`](Self::forks)
/// arm if it has forks; the [`FallbackSwapData`] variant of the same name; its
/// [`FallbackSwapData::encode`] arm; and its chains in [`SUPPORTED_PROTOCOLS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, EnumString, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum FallbackProtocol {
    UniswapV2,
    UniswapV3,
    UniswapV4,
    Curve,
    FluidV1,
    AerodromeV1,
}

/// The fallback protocols each chain's `TychoFallbackRouter` runs. A chain lists a protocol when
/// its router has what the protocol needs and Tycho has an executor for it there. A chain missing
/// here has no router.
static SUPPORTED_PROTOCOLS: LazyLock<HashMap<Chain, &'static [FallbackProtocol]>> =
    LazyLock::new(|| {
        HashMap::from([
            (
                Chain::Ethereum,
                &[
                    FallbackProtocol::UniswapV2,
                    FallbackProtocol::UniswapV3,
                    FallbackProtocol::UniswapV4,
                    FallbackProtocol::Curve,
                    FallbackProtocol::FluidV1,
                ][..],
            ),
            (
                Chain::Base,
                &[
                    FallbackProtocol::UniswapV2,
                    FallbackProtocol::UniswapV3,
                    FallbackProtocol::UniswapV4,
                    FallbackProtocol::AerodromeV1,
                ][..],
            ),
        ])
    });

impl FallbackProtocol {
    /// The protocol byte: the ordinal of the contract's `FallbackProtocol` variant.
    fn protocol_byte(self) -> u8 {
        self as u8
    }

    /// The `fallback_protocol` tag naming this protocol in `user_data`.
    pub fn user_data_name(self) -> &'static str {
        self.into()
    }

    /// Forks that map to this protocol.
    fn forks(self) -> &'static [&'static [&'static str]] {
        match self {
            FallbackProtocol::UniswapV2 => &[UNISWAP_V2_FORKS],
            // Slipstream pools use Uniswap V3's `swap` and callback.
            FallbackProtocol::UniswapV3 => &[UNISWAP_V3_FORKS, SLIPSTREAMS_FORKS],
            _ => &[],
        }
    }

    /// The protocol a Tycho `protocol_system` or `user_data` tag maps to. A `vm:` prefix is
    /// ignored: `vm:curve` is Curve.
    pub fn from_protocol_system(protocol_system: &str) -> Option<Self> {
        let name = protocol_system
            .strip_prefix("vm:")
            .unwrap_or(protocol_system);
        if let Ok(protocol) = Self::from_str(name) {
            return Some(protocol);
        }
        Self::iter().find(|protocol| {
            protocol
                .forks()
                .iter()
                .any(|forks| forks.contains(&name))
        })
    }

    /// Whether `chain`'s `TychoFallbackRouter` runs this protocol, per [`SUPPORTED_PROTOCOLS`].
    pub fn supported_on(self, chain: Chain) -> bool {
        SUPPORTED_PROTOCOLS
            .get(&chain)
            .is_some_and(|protocols| protocols.contains(&self))
    }
}

/// The fallback protocol and its pool, from the swap's `user_data` JSON, e.g.
/// `{"fallback_protocol":"uniswap_v3","pool":"0x…"}`.
struct FallbackSwap {
    protocol: FallbackProtocol,
    data: FallbackSwapData,
}

impl FallbackSwap {
    fn from_user_data(user_data: &Option<Bytes>) -> Result<Self, EncodingError> {
        let Some(bytes) = user_data
            .as_ref()
            .filter(|bytes| !bytes.is_empty())
        else {
            return Err(EncodingError::InvalidInput(
                "Fallback swaps require user_data naming the fallback protocol \
                 (e.g. {\"fallback_protocol\":\"uniswap_v3\",\"pool\":\"0x…\"})"
                    .to_string(),
            ));
        };
        let invalid_json = |e| {
            EncodingError::InvalidInput(format!("Invalid fallback protocol user_data JSON: {e}"))
        };

        let mut value: serde_json::Value = serde_json::from_slice(bytes).map_err(invalid_json)?;
        let protocol = {
            let name = value
                .get("fallback_protocol")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    EncodingError::InvalidInput(
                        "Fallback protocol user_data JSON names no fallback_protocol".to_string(),
                    )
                })?;
            FallbackProtocol::from_protocol_system(name).ok_or_else(|| {
                EncodingError::InvalidInput(format!(
                    "Fallback protocol {name} is not one TychoFallbackRouter can run"
                ))
            })?
        };
        // Replace a fork name with the tag serde expects.
        value["fallback_protocol"] = protocol.user_data_name().into();
        let data = serde_json::from_value(value).map_err(invalid_json)?;

        Ok(Self { protocol, data })
    }

    /// The protocol byte followed by the protocol data.
    fn encode(&self) -> Result<Vec<u8>, EncodingError> {
        let mut encoded = vec![self.protocol.protocol_byte()];
        encoded.extend(self.data.encode()?);
        Ok(encoded)
    }
}

/// The fallback pool a `fallback:` swap names in its `user_data`, one variant per
/// [`FallbackProtocol`].
///
/// Serializes to the JSON `FallbackSwapEncoder` reads, tagged `fallback_protocol` with the
/// protocol's [`user_data_name`](FallbackProtocol::user_data_name), e.g.
/// `{"fallback_protocol":"uniswap_v3","pool":"0x…"}`. A solver builds the variant for the pool it
/// picked and puts `serde_json::to_vec(&data)` on the swap's `user_data`. The tag also accepts a
/// fork's protocol system (`sushiswap_v2`, `vm:curve`) on the way in, which maps to the base
/// variant.
///
/// Swap direction for the Uniswap family and Aerodrome follows from the swap's own tokens, so it
/// is not carried here. Curve's coin indices and Fluid's `zero2one` are the pool's own ordering,
/// which the encoder cannot recover from the tokens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fallback_protocol", rename_all = "snake_case")]
pub enum FallbackSwapData {
    /// A Uniswap V2 pair or fork. `fee_bps` is the pair's swap fee; the router accepts at most
    /// 30.
    UniswapV2 { pair: Bytes, fee_bps: u8 },
    /// A Uniswap V3 pool, or a fork on Uniswap V3's `swap` and callback (Slipstreams included).
    UniswapV3 { pool: Bytes },
    /// A Uniswap V4 pool, identified by its key rather than an address. Hooked pools are not
    /// supported yet: `hook` must be absent or the zero address and `hook_data` absent or empty.
    UniswapV4 {
        fee: u32,
        tick_spacing: i32,
        #[serde(default)]
        hook: Bytes,
        #[serde(default)]
        hook_data: Bytes,
    },
    /// A Curve pool. `pool_type` picks the `exchange` signature the router calls; `i` and `j` are
    /// the input and output coins' positions in the pool's own coin list.
    Curve { pool: Bytes, pool_type: u8, i: u8, j: u8 },
    /// A Fluid V1 dex. `zero2one` is `true` when the swap's input is the dex's token0.
    FluidV1 { dex: Bytes, zero2one: bool },
    /// An Aerodrome V1 pool.
    AerodromeV1 { pool: Bytes },
}

impl FallbackSwapData {
    /// Packs the fields in the contract's order. Rejects values the contract would revert on.
    fn encode(&self) -> Result<Vec<u8>, EncodingError> {
        let mut data = Vec::new();
        match self {
            FallbackSwapData::UniswapV2 { pair, fee_bps } => {
                if *fee_bps > MAX_UNISWAP_V2_FEE_BPS {
                    return Err(EncodingError::InvalidInput(format!(
                        "Uniswap V2 fallback fee is {fee_bps} bps, the fallback router accepts \
                         at most {MAX_UNISWAP_V2_FEE_BPS}"
                    )));
                }
                data.extend_from_slice(bytes_to_address(pair)?.as_slice());
                data.push(*fee_bps);
            }
            FallbackSwapData::UniswapV3 { pool } => {
                data.extend_from_slice(bytes_to_address(pool)?.as_slice());
            }
            FallbackSwapData::UniswapV4 { fee, tick_spacing, hook, hook_data } => {
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
                if hook.iter().any(|byte| *byte != 0) || !hook_data.is_empty() {
                    return Err(EncodingError::InvalidInput(
                        "Uniswap V4 hooks are not supported as a fallback yet: hook must be the \
                         zero address and hook_data empty"
                            .to_string(),
                    ));
                }
                data.extend_from_slice(&fee.to_be_bytes()[1..]);
                data.extend_from_slice(&tick_spacing.to_be_bytes()[1..]);
                data.extend_from_slice(&[0u8; 20]);
            }
            FallbackSwapData::Curve { pool, pool_type, i, j } => {
                data.extend_from_slice(bytes_to_address(pool)?.as_slice());
                data.extend_from_slice(&[*pool_type, *i, *j]);
            }
            FallbackSwapData::FluidV1 { dex, zero2one } => {
                data.extend_from_slice(bytes_to_address(dex)?.as_slice());
                data.push(u8::from(*zero2one));
            }
            FallbackSwapData::AerodromeV1 { pool } => {
                data.extend_from_slice(bytes_to_address(pool)?.as_slice());
            }
        }
        Ok(data)
    }
}

/// Encodes a pAMM swap for `TychoFallbackRouter`, which retries a failing pAMM on the fallback
/// protocol named in the swap's `user_data`.
///
/// # Fields
/// * `executor_address` - The executor that performs the swap.
/// * `chain` - The chain whose router runs the swap. Protocols it does not run are rejected.
#[derive(Clone)]
pub struct FallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
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
                    "pAMM component {} is missing the {PAMM_ADDRESS_ATTRIBUTE} static \
                     attribute",
                    component.id
                ))
            })
    }

    /// Rejects a protocol the chain's `TychoFallbackRouter` does not run.
    fn reject_unsupported(&self, protocol: FallbackProtocol) -> Result<(), EncodingError> {
        if !protocol.supported_on(self.chain) {
            return Err(EncodingError::InvalidInput(format!(
                "Fallback protocol {} is not supported on {}: the chain's TychoFallbackRouter \
                 would revert the swap",
                protocol.user_data_name(),
                self.chain
            )));
        }
        Ok(())
    }
}

impl SwapEncoder for FallbackSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        Ok(Self { executor_address, chain })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let fallback = FallbackSwap::from_user_data(swap.user_data())?;
        self.reject_unsupported(fallback.protocol)?;
        let pamm = bytes_to_address(&Self::pamm_address(swap)?)?;
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let mut data = (token_in, token_out, pamm).abi_encode_packed();
        data.extend(fallback.encode()?);
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
    use crate::encoding::{evm::constants::DEFAULT_EXECUTORS_JSON, models::default_token};

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

    fn encoder(chain: Chain) -> FallbackSwapEncoder {
        FallbackSwapEncoder::new(Bytes::default(), chain, None).unwrap()
    }

    fn encode_usdc_weth(user_data: Option<&str>) -> Result<String, EncodingError> {
        encode_usdc_weth_on(Chain::Ethereum, user_data)
    }

    fn encode_usdc_weth_on(chain: Chain, user_data: Option<&str>) -> Result<String, EncodingError> {
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

        encoder(chain)
            .encode_swap(&swap, &encoding_context)
            .map(|encoded| encode(&encoded))
    }

    #[test]
    fn test_encode_uniswap_v3_fallback() {
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}01{USDC_WETH_USV3}"));
    }

    #[test]
    fn test_encode_uniswap_v2_fallback() {
        let pair = "b4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"fallback_protocol":"uniswap_v2","pair":"0x{pair}","fee_bps":30}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}00{pair}1e"));
    }

    #[test]
    fn test_encode_sushiswap_v2_alias() {
        let pair = "b4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"fallback_protocol":"sushiswap_v2","pair":"0x{pair}","fee_bps":30}}"#
        )))
        .unwrap();

        // Byte 00 = UniswapV2: the fork name resolves to the base variant.
        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}00{pair}1e"));
    }

    #[test]
    fn test_encode_pancakeswap_v3_alias() {
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"fallback_protocol":"pancakeswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#
        )))
        .unwrap();

        // Byte 01 = UniswapV3.
        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}01{USDC_WETH_USV3}"));
    }

    #[test]
    fn test_encode_slipstreams_alias() {
        for fork in SLIPSTREAMS_FORKS {
            let hex_swap = encode_usdc_weth(Some(&format!(
                r#"{{"fallback_protocol":"{fork}","pool":"0x{USDC_WETH_USV3}"}}"#
            )))
            .unwrap();

            assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}01{USDC_WETH_USV3}"), "{fork}");
        }
    }

    #[test]
    fn test_encode_curve_by_protocol_system_name() {
        let pool = "3333333333333333333333333333333333333333";
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"fallback_protocol":"vm:curve","pool":"0x{pool}","pool_type":1,"i":0,"j":2}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}03{pool}010002"));
    }

    #[test]
    fn test_from_protocol_system() {
        let cases = [
            ("uniswap_v2", Some(FallbackProtocol::UniswapV2)),
            ("quickswap_v2", Some(FallbackProtocol::UniswapV2)),
            ("uniswap_v3", Some(FallbackProtocol::UniswapV3)),
            ("pancakeswap_v3", Some(FallbackProtocol::UniswapV3)),
            ("velodrome_slipstreams", Some(FallbackProtocol::UniswapV3)),
            ("uniswap_v4", Some(FallbackProtocol::UniswapV4)),
            ("uniswap_v4_hooks", None),
            ("curve", Some(FallbackProtocol::Curve)),
            ("vm:curve", Some(FallbackProtocol::Curve)),
            ("fluid_v1", Some(FallbackProtocol::FluidV1)),
            ("aerodrome_v1", Some(FallbackProtocol::AerodromeV1)),
            ("vm:balancer_v2", None),
            ("pricelevelstream:kipseli", None),
        ];
        for (name, expected) in cases {
            assert_eq!(FallbackProtocol::from_protocol_system(name), expected, "{name}");
        }
    }

    /// Every `user_data_name` resolves back to its protocol.
    #[test]
    fn test_user_data_name_round_trips() {
        for protocol in FallbackProtocol::iter() {
            assert_eq!(
                FallbackProtocol::from_protocol_system(protocol.user_data_name()),
                Some(protocol)
            );
        }
    }

    /// Every `user_data_name` is a `FallbackSwapData` serde tag.
    #[test]
    fn test_every_user_data_name_is_a_serde_tag() {
        for protocol in FallbackProtocol::iter() {
            let tag_only = format!(r#"{{"fallback_protocol":"{}"}}"#, protocol.user_data_name());
            // Fails on the missing fields, never on the tag.
            if let Err(error) = serde_json::from_str::<FallbackSwapData>(&tag_only) {
                assert!(
                    !error
                        .to_string()
                        .contains("unknown variant"),
                    "{protocol:?}: {error}"
                );
            }
        }
    }

    #[test]
    fn test_supported_on() {
        assert!(FallbackProtocol::FluidV1.supported_on(Chain::Ethereum));
        assert!(!FallbackProtocol::AerodromeV1.supported_on(Chain::Ethereum));
        assert!(FallbackProtocol::AerodromeV1.supported_on(Chain::Base));
        assert!(!FallbackProtocol::Curve.supported_on(Chain::Base));
        // No router.
        assert!(!FallbackProtocol::UniswapV3.supported_on(Chain::Plasma));
    }

    /// A listed protocol has an executor on that chain, so Tycho indexes pools for it there.
    #[test]
    fn test_supported_protocols_have_executors() {
        let executors: HashMap<Chain, HashMap<String, String>> =
            serde_json::from_str(DEFAULT_EXECUTORS_JSON).unwrap();
        for (chain, protocols) in SUPPORTED_PROTOCOLS.iter() {
            let executors = executors
                .get(chain)
                .unwrap_or_else(|| panic!("{chain} has no executors"));
            for protocol in protocols.iter() {
                assert!(
                    executors
                        .keys()
                        .any(|system| FallbackProtocol::from_protocol_system(system) ==
                            Some(*protocol)),
                    "{chain} supports {} but has no executor for it",
                    protocol.user_data_name()
                );
            }
        }
    }

    /// Uniswap V4 and Fluid V1 are supported exactly where the router deploys with their
    /// singleton, which `deploy-fallback-router.js` reads from `executor_deployments.json`.
    #[test]
    fn test_supported_singletons_match_executor_deployments() {
        let deployments: serde_json::Value =
            serde_json::from_str(include_str!("../../../../config/executor_deployments.json"))
                .unwrap();
        let singletons =
            [(FallbackProtocol::UniswapV4, "uniswap_v4"), (FallbackProtocol::FluidV1, "fluid_v1")];
        for (chain, protocols) in SUPPORTED_PROTOCOLS.iter() {
            for (protocol, executor) in singletons {
                let deployed = deployments[chain.to_string()][executor]["args"][0].is_string();
                assert_eq!(protocols.contains(&protocol), deployed, "{chain}: {executor}");
            }
        }
    }

    #[test]
    fn test_rejects_protocol_unavailable_on_chain() {
        let encoder = FallbackSwapEncoder::new(Bytes::default(), Chain::Base, None).unwrap();
        let dex = "4444444444444444444444444444444444444444";
        let token_in = Bytes::from(format!("0x{USDC}").as_str());
        let token_out = Bytes::from(format!("0x{WETH}").as_str());
        let swap = Swap::new(
            usdc_weth_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_user_data(Bytes::from(
            format!(r#"{{"fallback_protocol":"fluid_v1","dex":"0x{dex}","zero2one":true}}"#)
                .into_bytes(),
        ));
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };

        let err = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap_err();
        assert!(
            matches!(err, EncodingError::InvalidInput(msg) if msg.contains("fluid_v1") && msg.contains("base"))
        );
    }

    #[test]
    fn test_encode_aerodrome_v1_fallback() {
        let pool = "5555555555555555555555555555555555555555";
        let hex_swap = encode_usdc_weth_on(
            Chain::Base,
            Some(&format!(r#"{{"fallback_protocol":"aerodrome_v1","pool":"0x{pool}"}}"#)),
        )
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}05{pool}"));
    }

    #[test]
    fn test_encode_uniswap_v4_fallback() {
        let hex_swap = encode_usdc_weth(Some(
            r#"{"fallback_protocol":"uniswap_v4","fee":3000,"tick_spacing":-60,
                "hook":"0x0000000000000000000000000000000000000000","hook_data":"0x"}"#,
        ))
        .unwrap();

        // fee 3000 = 0x000bb8; tick spacing -60 = 0xffffc4 in int24 two's complement.
        assert_eq!(
            hex_swap,
            format!("{USDC}{WETH}{PAMM}02000bb8ffffc40000000000000000000000000000000000000000")
        );
    }

    #[test]
    fn test_encode_uniswap_v4_fallback_without_hook_fields() {
        let hex_swap = encode_usdc_weth(Some(
            r#"{"fallback_protocol":"uniswap_v4","fee":500,"tick_spacing":10}"#,
        ))
        .unwrap();

        assert_eq!(
            hex_swap,
            format!("{USDC}{WETH}{PAMM}020001f400000a0000000000000000000000000000000000000000")
        );
    }

    #[test]
    fn test_rejects_uniswap_v4_hook() {
        let err = encode_usdc_weth(Some(
            r#"{"fallback_protocol":"uniswap_v4","fee":3000,"tick_spacing":60,
                "hook":"0x2222222222222222222222222222222222222222"}"#,
        ))
        .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("hooks")));
    }

    #[test]
    fn test_rejects_uniswap_v4_hook_data() {
        let err = encode_usdc_weth(Some(
            r#"{"fallback_protocol":"uniswap_v4","fee":3000,"tick_spacing":60,
                "hook":"0x0000000000000000000000000000000000000000","hook_data":"0xdeadbeef"}"#,
        ))
        .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("hooks")));
    }

    #[test]
    fn test_encode_curve_fallback() {
        let pool = "3333333333333333333333333333333333333333";
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"fallback_protocol":"curve","pool":"0x{pool}","pool_type":1,"i":0,"j":2}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}03{pool}010002"));
    }

    #[test]
    fn test_encode_fluid_v1_fallback() {
        let dex = "4444444444444444444444444444444444444444";
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"fallback_protocol":"fluid_v1","dex":"0x{dex}","zero2one":true}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}04{dex}01"));
    }

    /// A solver builds `FallbackSwapData` and serializes it; the encoder reads that JSON back into
    /// the same value and packs the bytes the contract expects. Optional V4 hook fields serialize
    /// as empty and are accepted as the zero hook.
    #[test]
    fn test_serialized_fallback_swap_data_round_trips_through_the_encoder() {
        let pool = Bytes::from(format!("0x{USDC_WETH_USV3}").as_str());
        let cases = [
            (FallbackSwapData::UniswapV3 { pool: pool.clone() }, format!("01{USDC_WETH_USV3}")),
            (
                FallbackSwapData::UniswapV4 {
                    fee: 500,
                    tick_spacing: 10,
                    hook: Bytes::default(),
                    hook_data: Bytes::default(),
                },
                "020001f400000a0000000000000000000000000000000000000000".to_string(),
            ),
            (
                FallbackSwapData::Curve { pool: pool.clone(), pool_type: 1, i: 0, j: 2 },
                format!("03{USDC_WETH_USV3}010002"),
            ),
        ];
        for (data, expected_tail) in cases {
            let json = serde_json::to_string(&data).unwrap();

            let hex_swap = encode_usdc_weth(Some(&json)).unwrap();

            assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}{expected_tail}"), "{json}");
            let decoded =
                FallbackSwap::from_user_data(&Some(Bytes::from(json.as_bytes()))).unwrap();
            assert_eq!(decoded.data, data);
        }
    }

    #[test]
    fn test_rejects_missing_user_data() {
        let err = encode_usdc_weth(None).unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("user_data")));
    }

    #[test]
    fn test_rejects_unknown_protocol() {
        let err = encode_usdc_weth(Some(r#"{"fallback_protocol":"balancer_v2","pool":"0x11"}"#))
            .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("balancer_v2")));
    }

    #[test]
    fn test_rejects_user_data_without_a_protocol_name() {
        let err = encode_usdc_weth(Some(r#"{"pool":"0x11"}"#)).unwrap_err();
        assert!(
            matches!(err, EncodingError::InvalidInput(msg) if msg.contains("fallback_protocol"))
        );
    }

    #[test]
    fn test_rejects_pool_shorter_than_an_address() {
        // `Bytes` accepts any length; the address conversion rejects it.
        let err = encode_usdc_weth(Some(r#"{"fallback_protocol":"uniswap_v3","pool":"0x11"}"#))
            .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("Invalid address")));
    }

    #[test]
    fn test_rejects_uniswap_v2_fee_above_cap() {
        let pair = "b4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let err = encode_usdc_weth(Some(&format!(
            r#"{{"fallback_protocol":"uniswap_v2","pair":"0x{pair}","fee_bps":31}}"#
        )))
        .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("31")));
    }

    #[test]
    fn test_rejects_uniswap_v4_fee_overflowing_uint24() {
        let err = encode_usdc_weth(Some(
            r#"{"fallback_protocol":"uniswap_v4","fee":16777216,"tick_spacing":60,
                "hook":"0x0000000000000000000000000000000000000000"}"#,
        ))
        .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("uint24")));
    }

    #[test]
    fn test_rejects_uniswap_v4_tick_spacing_overflowing_int24() {
        let err = encode_usdc_weth(Some(
            r#"{"fallback_protocol":"uniswap_v4","fee":500,"tick_spacing":8388608,
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
            format!(r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#)
                .into_bytes(),
        ));
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(format!("0x{USDC}").as_str()),
            group_token_out: Bytes::from(format!("0x{WETH}").as_str()),
        };

        let result = encoder(Chain::Ethereum).encode_swap(&swap, &encoding_context);
        assert!(
            matches!(result, Err(EncodingError::FatalError(msg)) if msg.contains(PAMM_ADDRESS_ATTRIBUTE))
        );
    }

    #[test]
    fn test_encoder_builds_on_any_chain_without_config() {
        FallbackSwapEncoder::new(Bytes::zero(20), Chain::Base, None).unwrap();
    }
}
