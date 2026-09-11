use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

#[derive(Clone)]
pub struct LidoV3SwapEncoder {
    executor_address: Bytes,
    steth_address: Bytes,
    wsteth_address: Bytes,
    native_token_address: Bytes,
}

#[repr(u8)]
enum LidoV3Direction {
    Submit = 0,
    Wrap = 1,
    Unwrap = 2,
}

impl SwapEncoder for LidoV3SwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let config = config
            .ok_or_else(|| EncodingError::FatalError("Lido V3 config is empty".to_string()))?;

        let steth_address = config
            .get("steth_address")
            .map(|a| Bytes::from(a.as_str()))
            .ok_or_else(|| {
                EncodingError::FatalError("Missing steth_address in lido_v3 config".to_string())
            })?;

        let wsteth_address = config
            .get("wsteth_address")
            .map(|a| Bytes::from(a.as_str()))
            .ok_or_else(|| {
                EncodingError::FatalError("Missing wsteth_address in lido_v3 config".to_string())
            })?;

        Ok(Self {
            executor_address,
            steth_address,
            wsteth_address,
            native_token_address: chain.native_token().address,
        })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let direction = if *swap.token_in().address == self.native_token_address &&
            *swap.token_out().address == self.steth_address
        {
            LidoV3Direction::Submit
        } else if *swap.token_in().address == self.steth_address &&
            *swap.token_out().address == self.wsteth_address
        {
            LidoV3Direction::Wrap
        } else if *swap.token_in().address == self.wsteth_address &&
            *swap.token_out().address == self.steth_address
        {
            LidoV3Direction::Unwrap
        } else {
            return Err(EncodingError::InvalidInput("Combination not allowed".to_string()))
        };

        let args = (direction as u8).to_be_bytes();

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
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    const STETH_ADDRESS: &str = "0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84";
    const WSTETH_ADDRESS: &str = "0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0";

    fn lido_v3_config() -> HashMap<String, String> {
        HashMap::from([
            ("steth_address".to_string(), STETH_ADDRESS.to_string()),
            ("wsteth_address".to_string(), WSTETH_ADDRESS.to_string()),
        ])
    }

    fn encoding_context(token_in: &Bytes, token_out: &Bytes) -> EncodingContext {
        EncodingContext {
            router_address: Some(Bytes::default()),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        }
    }

    fn encoder() -> LidoV3SwapEncoder {
        LidoV3SwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            Some(lido_v3_config()),
        )
        .unwrap()
    }

    #[test]
    fn test_encode_lido_v3_submit() {
        let component = ProtocolComponent {
            id: STETH_ADDRESS.to_string(),
            protocol_system: "lido_v3".to_string(),
            ..Default::default()
        };
        let token_in = Bytes::from("0x0000000000000000000000000000000000000000");
        let token_out = Bytes::from(STETH_ADDRESS);
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );

        let encoded_swap = encoder()
            .encode_swap(&swap, &encoding_context(&token_in, &token_out))
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(hex_swap, "00");
        write_calldata_to_file("test_encode_lido_v3_submit", hex_swap.as_str());
    }

    #[test]
    fn test_encode_lido_v3_wrap() {
        let component = ProtocolComponent {
            id: WSTETH_ADDRESS.to_string(),
            protocol_system: "lido_v3".to_string(),
            ..Default::default()
        };
        let token_in = Bytes::from(STETH_ADDRESS);
        let token_out = Bytes::from(WSTETH_ADDRESS);
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );

        let encoded_swap = encoder()
            .encode_swap(&swap, &encoding_context(&token_in, &token_out))
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(hex_swap, "01");
        write_calldata_to_file("test_encode_lido_v3_wrap", hex_swap.as_str());
    }

    #[test]
    fn test_encode_lido_v3_unwrap() {
        let component = ProtocolComponent {
            id: WSTETH_ADDRESS.to_string(),
            protocol_system: "lido_v3".to_string(),
            ..Default::default()
        };
        let token_in = Bytes::from(WSTETH_ADDRESS);
        let token_out = Bytes::from(STETH_ADDRESS);
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );

        let encoded_swap = encoder()
            .encode_swap(&swap, &encoding_context(&token_in, &token_out))
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(hex_swap, "02");
        write_calldata_to_file("test_encode_lido_v3_unwrap", hex_swap.as_str());
    }

    #[test]
    fn test_encode_lido_v3_invalid_pair() {
        let component = ProtocolComponent {
            id: STETH_ADDRESS.to_string(),
            protocol_system: "lido_v3".to_string(),
            ..Default::default()
        };
        let token_in = Bytes::from(WSTETH_ADDRESS);
        let token_out = Bytes::from("0x0000000000000000000000000000000000000000");
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );

        let encoded_swap = encoder().encode_swap(&swap, &encoding_context(&token_in, &token_out));

        assert!(encoded_swap.is_err());
    }
}
