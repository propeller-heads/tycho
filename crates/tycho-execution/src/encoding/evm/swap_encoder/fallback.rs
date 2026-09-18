use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::LazyLock,
};

use alloy::sol_types::SolValue;
use serde::Deserialize;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::{DEFAULT_EXECUTORS_JSON, UNISWAP_V2_FORKS, UNISWAP_V3_FORKS},
        utils::bytes_to_address,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Static attribute under which fallback components carry their pAMM address — the same
/// attribute the price-level-stream family uses.
const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// The highest Uniswap V2 fee `TychoFallbackRouter` accepts (`feeBps <= 30`).
const MAX_UNISWAP_V2_FEE_BPS: u8 = 30;

/// Slipstream deployments and their forks. The registry encodes them through
/// `SlipstreamsSwapEncoder` because their executor data differs from Uniswap V3's, but the pool
/// itself keeps V3's `swap` ABI and callback, which is all `TychoFallbackRouter` uses.
const SLIPSTREAMS_FORKS: &[&str] =
    &["aerodrome_slipstreams", "velodrome_slipstreams", "up_v3", "ramses_v3"];

/// The protocols a chain's executor config names, keyed by chain. A chain has a protocol's
/// per-chain singleton exactly when it has that protocol's executor, which is also how
/// `deploy-fallback-router.js` decides which singletons the chain's `TychoFallbackRouter` gets.
static EXECUTOR_PROTOCOLS: LazyLock<HashMap<Chain, HashSet<String>>> = LazyLock::new(|| {
    let config: HashMap<Chain, HashMap<String, String>> =
        serde_json::from_str(DEFAULT_EXECUTORS_JSON)
            // Embedded at compile time and parsed by every registry test, so a failure here
            // is a broken build, not a runtime condition.
            .expect("config/executor_addresses.json is valid");
    config
        .into_iter()
        .map(|(chain, executors)| (chain, executors.into_keys().collect()))
        .collect()
});

/// A protocol `TychoFallbackRouter` can fall back on, one per variant of the contract's
/// `FallbackProtocol` enum.
///
/// The solver names one per swap in `user_data`; [`FallbackProtocol::from_protocol_system`]
/// says which one a Tycho component encodes as, and [`FallbackProtocol::supported_on`] whether
/// a chain's deployment can run it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FallbackProtocol {
    UniswapV2,
    UniswapV3,
    UniswapV4,
    Curve,
    FluidV1,
    AerodromeV1,
}

impl FallbackProtocol {
    /// Every protocol, in protocol-byte order.
    pub const ALL: [FallbackProtocol; 6] = [
        FallbackProtocol::UniswapV2,
        FallbackProtocol::UniswapV3,
        FallbackProtocol::UniswapV4,
        FallbackProtocol::Curve,
        FallbackProtocol::FluidV1,
        FallbackProtocol::AerodromeV1,
    ];

    /// The ordinal of the matching `TychoFallbackRouter.FallbackProtocol` variant — the wire
    /// format's protocol byte.
    pub fn protocol_byte(self) -> u8 {
        match self {
            FallbackProtocol::UniswapV2 => 0,
            FallbackProtocol::UniswapV3 => 1,
            FallbackProtocol::UniswapV4 => 2,
            FallbackProtocol::Curve => 3,
            FallbackProtocol::FluidV1 => 4,
            FallbackProtocol::AerodromeV1 => 5,
        }
    }

    /// The `fallback_protocol` value naming this protocol in a swap's `user_data`.
    pub fn user_data_name(self) -> &'static str {
        match self {
            FallbackProtocol::UniswapV2 => "uniswap_v2",
            FallbackProtocol::UniswapV3 => "uniswap_v3",
            FallbackProtocol::UniswapV4 => "uniswap_v4",
            FallbackProtocol::Curve => "curve",
            FallbackProtocol::FluidV1 => "fluid_v1",
            FallbackProtocol::AerodromeV1 => "aerodrome_v1",
        }
    }

    /// The protocol a Tycho `protocol_system` (or a [`user_data_name`](Self::user_data_name))
    /// encodes as, or `None` for one `TychoFallbackRouter` cannot run.
    ///
    /// Uniswap V2 forks share the constant-fee `swap(amount0Out, amount1Out, to, data)` pool;
    /// Uniswap V3 forks and the Slipstream deployments share V3's `swap` and callback, which
    /// `TychoFallbackRouter` answers whatever selector the fork renamed it to.
    pub fn from_protocol_system(protocol_system: &str) -> Option<Self> {
        if UNISWAP_V2_FORKS.contains(&protocol_system) {
            return Some(FallbackProtocol::UniswapV2);
        }
        if UNISWAP_V3_FORKS.contains(&protocol_system) ||
            SLIPSTREAMS_FORKS.contains(&protocol_system)
        {
            return Some(FallbackProtocol::UniswapV3);
        }
        match protocol_system {
            "uniswap_v4" | "uniswap_v4_hooks" => Some(FallbackProtocol::UniswapV4),
            "curve" | "vm:curve" => Some(FallbackProtocol::Curve),
            "fluid_v1" => Some(FallbackProtocol::FluidV1),
            "aerodrome_v1" => Some(FallbackProtocol::AerodromeV1),
            _ => None,
        }
    }

    /// Whether `chain`'s `TychoFallbackRouter` can run this protocol.
    ///
    /// Uniswap V4 and Fluid V1 call a per-chain singleton the router takes as a constructor
    /// immutable, and a chain without it deploys the router with `address(0)` there, which makes
    /// the protocol revert `TychoFallbackRouter__ProtocolUnavailable`. The chain has the
    /// singleton exactly when it has the protocol's executor. Every other protocol is addressed
    /// per swap and needs nothing from the deployment.
    pub fn supported_on(self, chain: Chain) -> bool {
        let executor = match self {
            FallbackProtocol::UniswapV4 => "uniswap_v4",
            FallbackProtocol::FluidV1 => "fluid_v1",
            FallbackProtocol::UniswapV2 |
            FallbackProtocol::UniswapV3 |
            FallbackProtocol::Curve |
            FallbackProtocol::AerodromeV1 => return true,
        };
        EXECUTOR_PROTOCOLS
            .get(&chain)
            .is_some_and(|executors| executors.contains(executor))
    }

    /// The protocols `chain`'s `TychoFallbackRouter` can run, in protocol-byte order.
    pub fn supported(chain: Chain) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|protocol| protocol.supported_on(chain))
            .collect()
    }
}

/// The fallback protocol and pool that fill a pAMM swap when the pAMM fails. The solver picks
/// them, JSON-encoded into `Swap::user_data` (e.g.
/// `{"fallback_protocol":"uniswap_v3","pool":"0x…"}`); a swap without one is rejected. Each
/// protocol's JSON fields are the variant fields below; the tag is any name
/// [`FallbackProtocol::from_protocol_system`] resolves.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "fallback_protocol", rename_all = "snake_case")]
enum FallbackSwapData {
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
    AerodromeV1 {
        pool: Bytes,
    },
}

impl FallbackSwapData {
    fn from_swap_user_data(user_data: &Option<Bytes>) -> Result<Self, EncodingError> {
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
        let canonical = value
            .get("fallback_protocol")
            .and_then(serde_json::Value::as_str)
            .and_then(FallbackProtocol::from_protocol_system)
            .map(FallbackProtocol::user_data_name);
        if let Some(canonical) = canonical {
            value["fallback_protocol"] = canonical.into();
        }
        serde_json::from_value(value).map_err(invalid_json)
    }

    fn protocol(&self) -> FallbackProtocol {
        match self {
            FallbackSwapData::UniswapV2 { .. } => FallbackProtocol::UniswapV2,
            FallbackSwapData::UniswapV3 { .. } => FallbackProtocol::UniswapV3,
            FallbackSwapData::UniswapV4 { .. } => FallbackProtocol::UniswapV4,
            FallbackSwapData::Curve { .. } => FallbackProtocol::Curve,
            FallbackSwapData::FluidV1 { .. } => FallbackProtocol::FluidV1,
            FallbackSwapData::AerodromeV1 { .. } => FallbackProtocol::AerodromeV1,
        }
    }

    /// Encodes the protocol byte followed by its data, rejecting values `TychoFallbackRouter`
    /// would revert on.
    fn encode(&self) -> Result<Vec<u8>, EncodingError> {
        let mut data = vec![self.protocol().protocol_byte()];
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
                data.extend_from_slice(&fee.to_be_bytes()[1..]);
                data.extend_from_slice(&tick_spacing.to_be_bytes()[1..]);
                data.extend_from_slice(bytes_to_address(hook)?.as_slice());
                data.extend_from_slice(hook_data.as_ref());
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

/// Encodes a swap that runs a pAMM through `TychoFallbackRouter` so a failing pAMM retries on
/// the fallback protocol named in the swap's `user_data` instead of reverting the route.
///
/// The pAMM address comes from the `pamm_address` static attribute of the component.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
/// * `chain` - The chain whose `TychoFallbackRouter` runs the swap; a fallback protocol it cannot
///   run ([`FallbackProtocol::supported_on`]) is rejected at encoding time.
/// * `angstrom_hook_address` - The chain's Angstrom hook, from the `fallback` section of
///   `protocol_specific_addresses.json`. Uniswap V4 fallbacks naming this hook are rejected. `None`
///   on a chain without Angstrom, where there is nothing to reject.
#[derive(Clone)]
pub struct FallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    angstrom_hook_address: Option<Bytes>,
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

    /// Rejects a protocol the chain's `TychoFallbackRouter` deploys without.
    fn reject_unsupported(&self, protocol: FallbackProtocol) -> Result<(), EncodingError> {
        if !protocol.supported_on(self.chain) {
            return Err(EncodingError::InvalidInput(format!(
                "Fallback protocol {} is unavailable on {}: the chain's TychoFallbackRouter \
                 deploys without its singleton, so the swap would revert on chain",
                protocol.user_data_name(),
                self.chain
            )));
        }
        Ok(())
    }

    /// Rejects a Uniswap V4 fallback whose hook is the chain's Angstrom hook.
    fn reject_angstrom_hook(&self, protocol: &FallbackSwapData) -> Result<(), EncodingError> {
        if let FallbackSwapData::UniswapV4 { hook, .. } = protocol {
            if Some(hook) == self.angstrom_hook_address.as_ref() {
                return Err(EncodingError::InvalidInput(
                    "Angstrom pools are unsupported as a fallback protocol".to_string(),
                ));
            }
        }
        Ok(())
    }
}

impl SwapEncoder for FallbackSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let angstrom_hook_address = config
            .as_ref()
            .and_then(|config| config.get("angstrom_hook_address"))
            .map(|address| {
                Bytes::from_str(address).map_err(|_| {
                    EncodingError::FatalError(format!("Invalid Angstrom hook address {address}"))
                })
            })
            .transpose()?;

        Ok(Self { executor_address, chain, angstrom_hook_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let protocol = FallbackSwapData::from_swap_user_data(swap.user_data())?;
        self.reject_unsupported(protocol.protocol())?;
        self.reject_angstrom_hook(&protocol)?;
        let pamm = bytes_to_address(&Self::pamm_address(swap)?)?;
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let mut data = (token_in, token_out, pamm).abi_encode_packed();
        data.extend(protocol.encode()?);
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

    // The mainnet address from the `fallback` section of
    // `config/protocol_specific_addresses.json`.
    const ANGSTROM_HOOK: &str = "0000000aa232009084Bd71A5797d089AA4Edfad4";

    fn encoder() -> FallbackSwapEncoder {
        FallbackSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([(
                "angstrom_hook_address".to_string(),
                format!("0x{ANGSTROM_HOOK}"),
            )])),
        )
        .unwrap()
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
    fn test_protocol_bytes_follow_all_order() {
        for (byte, protocol) in FallbackProtocol::ALL
            .into_iter()
            .enumerate()
        {
            assert_eq!(usize::from(protocol.protocol_byte()), byte, "{protocol:?}");
        }
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
            ("uniswap_v4_hooks", Some(FallbackProtocol::UniswapV4)),
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

    /// Every protocol's own name round-trips, so a solver can hand a `FallbackProtocol` back as
    /// the `user_data` tag.
    #[test]
    fn test_user_data_name_round_trips() {
        for protocol in FallbackProtocol::ALL {
            assert_eq!(
                FallbackProtocol::from_protocol_system(protocol.user_data_name()),
                Some(protocol)
            );
        }
    }

    /// Ethereum has every singleton; Base has no Fluid; Plasma has no Uniswap V4.
    #[test]
    fn test_supported_follows_executor_config() {
        assert_eq!(FallbackProtocol::supported(Chain::Ethereum), FallbackProtocol::ALL.to_vec());
        assert!(!FallbackProtocol::FluidV1.supported_on(Chain::Base));
        assert!(FallbackProtocol::UniswapV4.supported_on(Chain::Base));
        assert!(!FallbackProtocol::UniswapV4.supported_on(Chain::Plasma));
        assert!(FallbackProtocol::FluidV1.supported_on(Chain::Plasma));
        // Per-swap protocols need nothing from the deployment.
        for chain in [Chain::Base, Chain::Plasma, Chain::Unichain] {
            assert!(FallbackProtocol::UniswapV3.supported_on(chain), "{chain}");
            assert!(FallbackProtocol::AerodromeV1.supported_on(chain), "{chain}");
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
        let hex_swap = encode_usdc_weth(Some(&format!(
            r#"{{"fallback_protocol":"aerodrome_v1","pool":"0x{pool}"}}"#
        )))
        .unwrap();

        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}05{pool}"));
    }

    #[test]
    fn test_encode_uniswap_v4_fallback() {
        let hex_swap = encode_usdc_weth(Some(
            r#"{"fallback_protocol":"uniswap_v4","fee":3000,"tick_spacing":-60,
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
            r#"{"fallback_protocol":"uniswap_v4","fee":500,"tick_spacing":10,
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

    #[test]
    fn test_rejects_missing_user_data() {
        let err = encode_usdc_weth(None).unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("user_data")));
    }

    #[test]
    fn test_rejects_unknown_protocol() {
        let err = encode_usdc_weth(Some(r#"{"fallback_protocol":"balancer_v2","pool":"0x11"}"#))
            .unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("JSON")));
    }

    #[test]
    fn test_rejects_pool_shorter_than_an_address() {
        // `Bytes` deserializes any length, so a short pool passes serde and must fail at the
        // address conversion instead.
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

        let result = encoder().encode_swap(&swap, &encoding_context);
        assert!(
            matches!(result, Err(EncodingError::FatalError(msg)) if msg.contains(PAMM_ADDRESS_ATTRIBUTE))
        );
    }

    #[test]
    fn test_encoder_builds_on_any_chain_without_config() {
        FallbackSwapEncoder::new(Bytes::zero(20), Chain::Base, None).unwrap();
    }

    #[test]
    fn test_encoder_rejects_malformed_angstrom_hook() {
        let config = HashMap::from([("angstrom_hook_address".to_string(), "0xzz".to_string())]);
        let result = FallbackSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, Some(config));
        assert!(matches!(result, Err(EncodingError::FatalError(msg)) if msg.contains("0xzz")));
    }

    fn encode_v4_with_hook(
        encoder: &FallbackSwapEncoder,
        hook: &str,
    ) -> Result<String, EncodingError> {
        let token_in = Bytes::from(format!("0x{USDC}").as_str());
        let token_out = Bytes::from(format!("0x{WETH}").as_str());
        let swap = Swap::new(
            usdc_weth_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_user_data(Bytes::from(
            format!(
                r#"{{"fallback_protocol":"uniswap_v4","fee":3000,"tick_spacing":60,"hook":"0x{hook}"}}"#
            )
            .into_bytes(),
        ));
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };
        encoder
            .encode_swap(&swap, &encoding_context)
            .map(|encoded| encode(&encoded))
    }

    #[test]
    fn test_angstrom_hook() {
        let err = encode_v4_with_hook(&encoder(), ANGSTROM_HOOK).unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(msg) if msg.contains("Angstrom")));
    }

    #[test]
    fn test_non_angstrom_hook() {
        let hook = "2222222222222222222222222222222222222222";
        let hex_swap = encode_v4_with_hook(&encoder(), hook).unwrap();
        // fee 3000 = 0x000bb8; tick spacing 60 = 0x00003c.
        assert_eq!(hex_swap, format!("{USDC}{WETH}{PAMM}02000bb800003c{hook}"));
    }

    /// A chain without Angstrom configures no hook, so no hook is rejected.
    #[test]
    fn test_no_angstrom_hook_configured_accepts_any_hook() {
        let encoder = FallbackSwapEncoder::new(Bytes::default(), Chain::Base, None).unwrap();
        let hex_swap = encode_v4_with_hook(&encoder, ANGSTROM_HOOK).unwrap();
        assert_eq!(
            hex_swap,
            format!("{USDC}{WETH}{PAMM}02000bb800003c{}", ANGSTROM_HOOK.to_lowercase())
        );
    }
}
