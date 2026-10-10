use std::{collections::HashMap, str::FromStr};

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::swap_encoder::{bebop::BebopSwapEncoder, fallback::FallbackSwap},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Protocol config key of the chain's `BebopFallbackRouter` address.
const FALLBACK_ROUTER_CONFIG_KEY: &str = "fallback_router";

const BEBOP_HEAD_LENGTH: usize = 60;

/// Encodes a Bebop swap for `BebopFallbackRouter`. The quote is requested with that router as
/// taker.
#[derive(Clone)]
pub struct BebopFallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    fallback_router: Bytes,
    bebop: BebopSwapEncoder,
}

impl BebopFallbackSwapEncoder {
    fn quote_context(&self, encoding_context: &EncodingContext) -> EncodingContext {
        EncodingContext {
            router_address: Some(self.fallback_router.clone()),
            ..encoding_context.clone()
        }
    }
}

impl SwapEncoder for BebopFallbackSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let fallback_router = config
            .as_ref()
            .and_then(|config| config.get(FALLBACK_ROUTER_CONFIG_KEY))
            .ok_or_else(|| {
                EncodingError::FatalError(format!(
                    "Missing {FALLBACK_ROUTER_CONFIG_KEY} in the fallback:rfq:bebop config for \
                     {chain}: add the chain's BebopFallbackRouter address to \
                     protocol_specific_addresses.json"
                ))
            })?;
        let fallback_router = Bytes::from_str(fallback_router).map_err(|_| {
            EncodingError::FatalError(format!(
                "Invalid {FALLBACK_ROUTER_CONFIG_KEY} address for {chain}: {fallback_router}"
            ))
        })?;
        let bebop = BebopSwapEncoder::new(executor_address.clone(), chain, None)?;
        Ok(Self { executor_address, chain, fallback_router, bebop })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let fallback = FallbackSwap::from_user_data(swap.user_data())?;
        if !fallback
            .protocol
            .supported_on(self.chain)
        {
            return Err(EncodingError::InvalidInput(format!(
                "Fallback protocol {} is not supported on {}: the chain's BebopFallbackRouter \
                 would revert the swap",
                fallback.protocol.user_data_name(),
                self.chain
            )));
        }

        let bebop = self
            .bebop
            .encode_swap(swap, &self.quote_context(encoding_context))?;
        let (head, bebop_data) = bebop.split_at(BEBOP_HEAD_LENGTH);
        let bebop_data_length = u32::try_from(bebop_data.len()).map_err(|_| {
            EncodingError::FatalError(format!(
                "Bebop swap data of {} bytes does not fit the uint32 length prefix",
                bebop_data.len()
            ))
        })?;

        let mut data = head.to_vec();
        data.extend_from_slice(&bebop_data_length.to_be_bytes());
        data.extend_from_slice(bebop_data);
        data.extend(fallback.encode()?);
        Ok(data)
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn blocks_on_quote(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{testing_utils::MockRFQState, utils::write_calldata_to_file},
        models::default_token,
    };

    const WETH: &str = "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const WBTC: &str = "2260fac5e5542a773aa44fbcfedf7c193bc2c599";
    const SETTLEMENT: &str = "bbbbbbb520d69a9775e85b458c58c648259fad5f";
    const FALLBACK_ROUTER: &str = "1111111111111111111111111111111111111111";
    const WBTC_WETH_UNIV2: &str = "bb2b8038a1640196fbe3e38816f3e67cba72d940";

    fn encoder() -> BebopFallbackSwapEncoder {
        BebopFallbackSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([(
                FALLBACK_ROUTER_CONFIG_KEY.to_string(),
                format!("0x{FALLBACK_ROUTER}"),
            )])),
        )
        .unwrap()
    }

    fn context() -> EncodingContext {
        EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(format!("0x{WETH}").as_str()),
            group_token_out: Bytes::from(format!("0x{WBTC}").as_str()),
        }
    }

    fn weth_wbtc_swap(user_data: &str) -> Swap {
        let state = MockRFQState {
            quote_amount_in: Some(BigUint::from(1_000u64)),
            quote_amount_out: BigUint::from(3u64),
            quote_data: HashMap::from([
                ("calldata".to_string(), Bytes::from("0x4dcebcba12")),
                ("partial_fill_offset".to_string(), Bytes::from(12u64.to_be_bytes().to_vec())),
                ("tx_to".to_string(), Bytes::from(format!("0x{SETTLEMENT}").as_str())),
            ]),
            ..Default::default()
        };
        let component = ProtocolComponent {
            id: String::from("bebop-rfq"),
            protocol_system: String::from("fallback:rfq:bebop"),
            ..Default::default()
        };
        Swap::new(
            component,
            default_token(Bytes::from(format!("0x{WETH}").as_str())),
            default_token(Bytes::from(format!("0x{WBTC}").as_str())),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(1_000u64))
        .with_protocol_state(Arc::new(state))
        .with_user_data(Bytes::from(user_data.as_bytes()))
    }

    #[test]
    fn test_encode_bebop_with_uniswap_v2_fallback() {
        let swap = weth_wbtc_swap(&format!(
            r#"{{"fallback_protocol":"uniswap_v2","pair":"0x{WBTC_WETH_UNIV2}","fee_bps":30}}"#
        ));

        let hex_swap = encode(
            encoder()
                .encode_swap(&swap, &context())
                .unwrap(),
        );

        // bebopData: offset 0c, originalFilledTakerAmount 1000 (0x3e8), calldata 4dcebcba12.
        let bebop_data = format!("0c{:064x}4dcebcba12", 1_000);
        assert_eq!(
            hex_swap,
            format!(
                "{WETH}{WBTC}{SETTLEMENT}{:08x}{bebop_data}00{WBTC_WETH_UNIV2}1e",
                bebop_data.len() / 2
            )
        );
    }

    /// `BebopFallbackRouterTest` runs this swap data against the order on a mainnet fork. The
    /// order's taker is `TAKER` there, so the fallback router config names it.
    #[test]
    fn test_encode_bebop_fallback_for_solidity() {
        let one_weth = BigUint::from(1_000_000_000_000_000_000u64);
        let state = MockRFQState {
            quote_amount_in: Some(one_weth.clone()),
            quote_amount_out: BigUint::from(3_617_660u64),
            quote_data: HashMap::from([
                ("calldata".to_string(), Bytes::from_str("0x4dcebcba00000000000000000000000000000000000000000000000000000000689b137a0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000bee3211ab312a8d065c4fef0247448e17a8da000000000000000000000000000000000000000000000000000279ead5d9683d8a5000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000002260fac5e5542a773aa44fbcfedf7c193bc2c5990000000000000000000000000000000000000000000000000de0b6b3a7640000000000000000000000000000000000000000000000000000000000000037337c0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000000000000000000000000000000000000000000000f71248bc6c123bbf12adc837470f75640000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000418e9b0fb72ed9b86f7a7345026269c02b9056efcdfb67a377c7ff6c4a62a4807a7671ae759edf29aea1b2cb8efc8659e3aedac72943cd3607985a1849256358641c00000000000000000000000000000000000000000000000000000000000000").unwrap()),
                ("partial_fill_offset".to_string(), Bytes::from(12u64.to_be_bytes().to_vec())),
                ("tx_to".to_string(), Bytes::from(format!("0x{SETTLEMENT}").as_str())),
            ]),
            ..Default::default()
        };
        let swap = Swap::new(
            ProtocolComponent {
                id: String::from("bebop-rfq"),
                protocol_system: String::from("fallback:rfq:bebop"),
                ..Default::default()
            },
            default_token(Bytes::from(format!("0x{WETH}").as_str())),
            default_token(Bytes::from(format!("0x{WBTC}").as_str())),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(one_weth)
        .with_protocol_state(Arc::new(state))
        .with_user_data(Bytes::from(
            format!(
                r#"{{"fallback_protocol":"uniswap_v2","pair":"0x{WBTC_WETH_UNIV2}","fee_bps":30}}"#
            )
            .as_bytes(),
        ));
        let encoder = BebopFallbackSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([(
                FALLBACK_ROUTER_CONFIG_KEY.to_string(),
                "0x5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f".to_string(),
            )])),
        )
        .unwrap();

        let encoded = encoder
            .encode_swap(&swap, &context())
            .unwrap();

        write_calldata_to_file("test_encode_bebop_fallback_for_solidity", &encode(encoded));
    }

    #[test]
    fn test_quote_is_requested_for_the_fallback_router() {
        let quote_context = encoder().quote_context(&context());

        assert_eq!(
            quote_context.router_address,
            Some(Bytes::from(format!("0x{FALLBACK_ROUTER}").as_str()))
        );
        assert_eq!(quote_context.group_token_in, context().group_token_in);
    }

    #[test]
    fn test_new_requires_the_fallback_router_config() {
        let err = BebopFallbackSwapEncoder::new(Bytes::default(), Chain::Ethereum, None)
            .err()
            .unwrap();

        assert!(
            matches!(err, EncodingError::FatalError(message) if message.contains("fallback_router"))
        );
    }

    #[test]
    fn test_rejects_fallback_the_chain_does_not_run() {
        let swap = weth_wbtc_swap(&format!(
            r#"{{"fallback_protocol":"aerodrome_v1","pool":"0x{WBTC_WETH_UNIV2}"}}"#
        ));

        let err = encoder()
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(
            matches!(err, EncodingError::InvalidInput(message) if message.contains("ethereum"))
        );
    }
}
