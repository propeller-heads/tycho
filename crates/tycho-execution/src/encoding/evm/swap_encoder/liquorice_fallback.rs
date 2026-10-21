use std::{collections::HashMap, str::FromStr};

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::swap_encoder::{fallback::FallbackSwap, liquorice::LiquoriceSwapEncoder},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Protocol config key of the chain's `LiquoriceFallbackRouter` address.
const FALLBACK_ROUTER_CONFIG_KEY: &str = "fallback_router";

/// `LiquoriceSwapEncoder` returns `[tokenIn: 20][tokenOut: 20][partialFillOffset: 4]
/// [originalBaseTokenAmount: 32][minBaseTokenAmount: 32][settlement calldata]`. This encoder keeps
/// the tokens, drops the partial-fill fields and length-prefixes the calldata.
/// `LIQUORICE_HEAD_LENGTH` is the length of the kept tokens.
const LIQUORICE_HEAD_LENGTH: usize = 40;

/// Length of the dropped partial-fill fields. `LiquoriceFallbackRouter` does not partially fill,
/// so it takes the settlement calldata alone.
const LIQUORICE_FILL_FIELDS_LENGTH: usize = 68;

/// Encodes a Liquorice swap for `LiquoriceFallbackRouter`. The quote is requested with that router
/// as trader and receiver, and fills its signed amount.
#[derive(Clone)]
pub struct LiquoriceFallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    fallback_router: Bytes,
    liquorice: LiquoriceSwapEncoder,
}

impl LiquoriceFallbackSwapEncoder {
    fn quote_context(&self, encoding_context: &EncodingContext) -> EncodingContext {
        EncodingContext {
            router_address: Some(self.fallback_router.clone()),
            ..encoding_context.clone()
        }
    }
}

impl SwapEncoder for LiquoriceFallbackSwapEncoder {
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
                    "Missing {FALLBACK_ROUTER_CONFIG_KEY} in the fallback:rfq:liquorice config \
                     for {chain}: add the chain's LiquoriceFallbackRouter address to \
                     protocol_specific_addresses.json"
                ))
            })?;
        let fallback_router = Bytes::from_str(fallback_router).map_err(|_| {
            EncodingError::FatalError(format!(
                "Invalid {FALLBACK_ROUTER_CONFIG_KEY} address for {chain}: {fallback_router}"
            ))
        })?;
        let liquorice = LiquoriceSwapEncoder::new(executor_address.clone(), chain, None)?;
        Ok(Self { executor_address, chain, fallback_router, liquorice })
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
                "Fallback protocol {} is not supported on {}: the chain's \
                 LiquoriceFallbackRouter would revert the swap",
                fallback.protocol.user_data_name(),
                self.chain
            )));
        }
        let native_token = self.chain.native_token().address;
        if swap.token_in().address == native_token || swap.token_out().address == native_token {
            return Err(EncodingError::InvalidInput(format!(
                "The native token is not supported on fallback:rfq:liquorice: the chain's \
                 LiquoriceFallbackRouter only trades ERC20s and would revert the swap. Use the \
                 wrapped token, or rfq:liquorice, for {}",
                self.chain
            )));
        }

        let liquorice = self
            .liquorice
            .encode_swap(swap, &self.quote_context(encoding_context))?;
        let (head, rest) = liquorice.split_at(LIQUORICE_HEAD_LENGTH);
        let settlement_calldata = &rest[LIQUORICE_FILL_FIELDS_LENGTH..];
        let settlement_calldata_length =
            u32::try_from(settlement_calldata.len()).map_err(|_| {
                EncodingError::FatalError(format!(
                    "Liquorice calldata of {} bytes does not fit the uint32 length prefix",
                    settlement_calldata.len()
                ))
            })?;

        let mut data = head.to_vec();
        data.extend_from_slice(&settlement_calldata_length.to_be_bytes());
        data.extend_from_slice(settlement_calldata);
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

    use alloy::hex::{decode, encode};
    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{
            testing_utils::MockRFQState,
            utils::{biguint_to_u256, write_calldata_to_file},
        },
        models::default_token,
    };

    const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const WETH: &str = "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC_WETH_USV3: &str = "88e6a0c2ddd26feeb64f039a2c41296fcb3f5640";
    /// The trader and receiver of `LiquoriceFallbackRouterTest`'s signed quote.
    const TRADER: &str = "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f";
    /// `LiquoriceFallbackRouterTest`'s signed `settleSingle` calldata for 3000 USDC.
    const SETTLE_SINGLE_CALLDATA: &str = "9935c86800000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f68705800000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000024000000000000000000000000000000000000000000000000000000000b2d05e000000000000000000000000000000000000000000000000000000000000000320000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000000010000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc200000000000000000000000000000000000000000000000000000000b2d05e000000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000006985036700000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f687058000000000000000000000000000000000000000000000000000000000000000131000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000041883a6506193307eebda0f3adf2cb81f84a073e030749055ebb18cbf98704eef100a03c307266527d706f9a5c3e08ed0988f5b130bc5327e0ad62dde6f3709d251b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000";
    const SIGNED_USDC_IN: u64 = 3_000_000_000;

    fn uint(value: u64) -> Bytes {
        Bytes::from(
            biguint_to_u256(&BigUint::from(value))
                .to_be_bytes::<32>()
                .to_vec(),
        )
    }

    fn encoder() -> LiquoriceFallbackSwapEncoder {
        LiquoriceFallbackSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([(FALLBACK_ROUTER_CONFIG_KEY.to_string(), format!("0x{TRADER}"))])),
        )
        .unwrap()
    }

    fn context() -> EncodingContext {
        EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(format!("0x{USDC}").as_str()),
            group_token_out: Bytes::from(format!("0x{WETH}").as_str()),
            origin: None,
        }
    }

    fn usdc_weth_swap(user_data: &str) -> Swap {
        let state = MockRFQState {
            quote_amount_in: None,
            quote_amount_out: BigUint::from(1_000_000_000_000_000_000u64),
            quote_data: HashMap::from([
                ("calldata".to_string(), Bytes::from(decode(SETTLE_SINGLE_CALLDATA).unwrap())),
                ("base_token_amount".to_string(), uint(SIGNED_USDC_IN)),
            ]),
            ..Default::default()
        };
        Swap::new(
            ProtocolComponent {
                id: String::from("liquorice-rfq"),
                protocol_system: String::from("fallback:rfq:liquorice"),
                ..Default::default()
            },
            default_token(Bytes::from(format!("0x{USDC}").as_str())),
            default_token(Bytes::from(format!("0x{WETH}").as_str())),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(SIGNED_USDC_IN))
        .with_protocol_state(Arc::new(state))
        .with_user_data(Bytes::from(user_data.as_bytes()))
    }

    /// `LiquoriceFallbackRouterTest` runs this swap data against the quote on a mainnet fork.
    #[test]
    fn test_encode_liquorice_fallback_for_solidity() {
        let swap = usdc_weth_swap(&format!(
            r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#
        ));

        let hex_swap = encode(
            encoder()
                .encode_swap(&swap, &context())
                .unwrap(),
        );

        assert_eq!(
            hex_swap,
            format!(
                "{USDC}{WETH}{:08x}{SETTLE_SINGLE_CALLDATA}01{USDC_WETH_USV3}",
                SETTLE_SINGLE_CALLDATA.len() / 2
            )
        );
        write_calldata_to_file("test_encode_liquorice_fallback_for_solidity", &hex_swap);
    }

    #[test]
    fn test_quote_context() {
        let quote_context = encoder().quote_context(&context());

        assert_eq!(quote_context.router_address, Some(Bytes::from(format!("0x{TRADER}").as_str())));
        assert_eq!(quote_context.group_token_in, context().group_token_in);
    }

    #[test]
    fn test_new_without_fallback_router_config() {
        let err = LiquoriceFallbackSwapEncoder::new(Bytes::default(), Chain::Ethereum, None)
            .err()
            .unwrap();

        assert!(
            matches!(err, EncodingError::FatalError(message) if message.contains("fallback_router"))
        );
    }

    #[test]
    fn test_fallback_the_chain_does_not_run() {
        let swap = usdc_weth_swap(&format!(
            r#"{{"fallback_protocol":"aerodrome_v1","pool":"0x{USDC_WETH_USV3}"}}"#
        ));

        let err = encoder()
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(
            matches!(err, EncodingError::InvalidInput(message) if message.contains("ethereum"))
        );
    }

    #[rstest]
    #[case::native_token_in(Chain::Ethereum.native_token().address, Bytes::from(format!("0x{WETH}").as_str()))]
    #[case::native_token_out(Bytes::from(format!("0x{USDC}").as_str()), Chain::Ethereum.native_token().address)]
    fn test_native_token_leg(#[case] token_in: Bytes, #[case] token_out: Bytes) {
        let swap = Swap::new(
            ProtocolComponent {
                id: String::from("liquorice-rfq"),
                protocol_system: String::from("fallback:rfq:liquorice"),
                ..Default::default()
            },
            default_token(token_in),
            default_token(token_out),
            BigUint::ZERO,
        )
        .with_user_data(Bytes::from(
            format!(r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#)
                .as_bytes(),
        ));

        let err = encoder()
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(
            matches!(err, EncodingError::InvalidInput(message) if message.contains("native token"))
        );
    }
}
