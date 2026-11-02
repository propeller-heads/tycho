use std::{collections::HashMap, str::FromStr};

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::swap_encoder::{fallback::FallbackSwap, native::NativeSwapEncoder},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Protocol config key of the chain's `NativeFallbackRouter` address.
const FALLBACK_ROUTER_CONFIG_KEY: &str = "fallback_router";

/// `NativeSwapEncoder` returns `[tokenIn: 20][tokenOut: 20][target: 20][signedAmountIn: 32]
/// [calldata]`. This encoder keeps the tokens, drops `target` and length-prefixes the rest.
/// `NATIVE_HEAD_LENGTH` is the length of the kept tokens.
const NATIVE_HEAD_LENGTH: usize = 40;

/// Length of the dropped `target`, the address the Native executor calls. `NativeFallbackRouter`
/// always calls its own `nativeRouterV6`, so it takes no target.
const NATIVE_TARGET_LENGTH: usize = 20;

/// Encodes a Native swap for `NativeFallbackRouter`. The quote is requested with that router as
/// recipient.
#[derive(Clone)]
pub struct NativeFallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    fallback_router: Bytes,
    native: NativeSwapEncoder,
}

impl NativeFallbackSwapEncoder {
    fn quote_context(&self, encoding_context: &EncodingContext) -> EncodingContext {
        EncodingContext {
            router_address: Some(self.fallback_router.clone()),
            ..encoding_context.clone()
        }
    }
}

impl SwapEncoder for NativeFallbackSwapEncoder {
    /// Reads `fallback_router` and Native's `router_v6` from `config`.
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
                    "Missing {FALLBACK_ROUTER_CONFIG_KEY} in the fallback:rfq:native config for \
                     {chain}: add the chain's NativeFallbackRouter address to \
                     protocol_specific_addresses.json"
                ))
            })?;
        let fallback_router = Bytes::from_str(fallback_router).map_err(|_| {
            EncodingError::FatalError(format!(
                "Invalid {FALLBACK_ROUTER_CONFIG_KEY} address for {chain}: {fallback_router}"
            ))
        })?;
        let native = NativeSwapEncoder::new(executor_address.clone(), chain, config)?;
        Ok(Self { executor_address, chain, fallback_router, native })
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
                "Fallback protocol {} is not supported on {}: the chain's NativeFallbackRouter \
                 would revert the swap",
                fallback.protocol.user_data_name(),
                self.chain
            )));
        }
        let native_token = self.chain.native_token().address;
        if swap.token_in().address == native_token || swap.token_out().address == native_token {
            return Err(EncodingError::InvalidInput(format!(
                "The native token is not supported on fallback:rfq:native: the chain's \
                 NativeFallbackRouter only trades ERC20s and would revert the swap. Use the \
                 wrapped token's book, or rfq:native, for {}",
                self.chain
            )));
        }

        let native = self
            .native
            .encode_swap(swap, &self.quote_context(encoding_context))?;
        let (head, rest) = native.split_at(NATIVE_HEAD_LENGTH);
        let native_data = &rest[NATIVE_TARGET_LENGTH..];
        let native_data_length = u32::try_from(native_data.len()).map_err(|_| {
            EncodingError::FatalError(format!(
                "Native swap data of {} bytes does not fit the uint32 length prefix",
                native_data.len()
            ))
        })?;

        let mut data = head.to_vec();
        data.extend_from_slice(&native_data_length.to_be_bytes());
        data.extend_from_slice(native_data);
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
        evm::{testing_utils::MockRFQState, utils::write_calldata_to_file},
        models::default_token,
    };

    const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const WETH: &str = "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const ROUTER_V6: &str = "4777a6b3a9a889abfd4c7666bdd2a7ab633293be";
    const USDC_WETH_USV3: &str = "88e6a0c2ddd26feeb64f039a2c41296fcb3f5640";
    /// The recipient of `NativeFallbackRouterTest`'s recorded quote.
    const RECIPIENT: &str = "cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2";
    /// `NativeFallbackRouterTest`'s recorded tradeRFQT calldata for 1 USDC.
    const TRADE_RFQT_CALLDATA: &str = "7083527c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ef435b99c2108d0309211d0cc05b70d202c0b5df000000000000000000000000129b3d9a0a6e4beab88f5cb1e57995d72a6e24f1000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc200000000000000000000000000000000000000000000000000000000000f424000000000000000000000000000000000000000000000000000016dfd1ab39d5c00000000000000000000000000000000000000000000000000016dfd1ab39d5c000000000000000000000000000000000000000000000000000000006a9f8d3c00000000000000000000000000000000000000000000000035a49220053ede33000000000000000000000000000000000000000000000000000000006a9f8cf5000000000000000000000000000000000000000000000000000000006a9f8d1d00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ec6e372d2326451e8711e06e0d8b78f9000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003600000000000000000000000006044eef7179034319e2c8636ea885b37cbfa9aba000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003e00000000000000000000000000000000000000000000000000000000000000041dc6662535476055fdc70cbf26235a36548260584b63670d587fff026324eacff3a91e0b6cb791bd8059d8701ad7065a8944385d96b4c6dd6910d74f9d06364a91c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041fbb4628ced3cdc79524c223e42adae7c302475dbac6ac3eb119293a41c3f3cf5482d18a6fa8c2bcdc07ae6551068dfa1dba9cff189cc56f8bc6c26f8783c8d261c00000000000000000000000000000000000000000000000000000000000000";

    fn encoder() -> NativeFallbackSwapEncoder {
        NativeFallbackSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([
                (FALLBACK_ROUTER_CONFIG_KEY.to_string(), format!("0x{RECIPIENT}")),
                ("router_v6".to_string(), format!("0x{ROUTER_V6}")),
            ])),
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
        swap(
            Bytes::from(format!("0x{USDC}").as_str()),
            Bytes::from(format!("0x{WETH}").as_str()),
            user_data,
        )
    }

    fn swap(token_in: Bytes, token_out: Bytes, user_data: &str) -> Swap {
        let state = MockRFQState {
            quote_amount_in: None,
            quote_amount_out: BigUint::from(402_408_818_842_972u64),
            quote_data: HashMap::from([
                ("target".to_string(), Bytes::from(format!("0x{ROUTER_V6}").as_str())),
                ("calldata".to_string(), Bytes::from(decode(TRADE_RFQT_CALLDATA).unwrap())),
                ("deadline_timestamp".to_string(), Bytes::from(u64::MAX.to_be_bytes().to_vec())),
            ]),
            ..Default::default()
        };
        Swap::new(
            ProtocolComponent {
                id: String::from("native-rfq"),
                protocol_system: String::from("fallback:rfq:native"),
                ..Default::default()
            },
            default_token(token_in),
            default_token(token_out),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(1_000_000u64))
        .with_protocol_state(Arc::new(state))
        .with_user_data(Bytes::from(user_data.as_bytes()))
    }

    /// `NativeFallbackRouterTest` runs this swap data against the recorded quote on a mainnet
    /// fork.
    #[test]
    fn test_encode_native_fallback_for_solidity() {
        let swap = usdc_weth_swap(&format!(
            r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#
        ));

        let hex_swap = encode(
            encoder()
                .encode_swap(&swap, &context())
                .unwrap(),
        );

        let native_data = format!("{:064x}{TRADE_RFQT_CALLDATA}", 1_000_000);
        assert_eq!(
            hex_swap,
            format!("{USDC}{WETH}{:08x}{native_data}01{USDC_WETH_USV3}", native_data.len() / 2)
        );
        write_calldata_to_file("test_encode_native_fallback_for_solidity", &hex_swap);
    }

    #[test]
    fn test_quote_context() {
        let quote_context = encoder().quote_context(&context());

        assert_eq!(
            quote_context.router_address,
            Some(Bytes::from(format!("0x{RECIPIENT}").as_str()))
        );
        assert_eq!(quote_context.group_token_in, context().group_token_in);
    }

    #[test]
    fn test_new_without_fallback_router_config() {
        let err = NativeFallbackSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([("router_v6".to_string(), format!("0x{ROUTER_V6}"))])),
        )
        .err()
        .unwrap();

        assert!(
            matches!(err, EncodingError::FatalError(message) if message.contains("fallback_router"))
        );
    }

    #[test]
    fn test_new_without_router_v6_config() {
        let err = NativeFallbackSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([(
                FALLBACK_ROUTER_CONFIG_KEY.to_string(),
                format!("0x{RECIPIENT}"),
            )])),
        )
        .err()
        .unwrap();

        assert!(matches!(err, EncodingError::FatalError(message) if message.contains("router_v6")));
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
        let swap = swap(
            token_in,
            token_out,
            &format!(r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#),
        );

        let err = encoder()
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(
            matches!(err, EncodingError::InvalidInput(message) if message.contains("native token"))
        );
    }
}
