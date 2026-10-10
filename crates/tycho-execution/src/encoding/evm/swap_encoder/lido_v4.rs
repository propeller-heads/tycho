use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

#[derive(Clone)]
pub struct LidoV4SwapEncoder {
    executor_address: Bytes,
    steth_address: Bytes,
    wsteth_address: Bytes,
    native_token_address: Bytes,
}

/// Mirrors `LidoV4Direction` in LidoV4Executor.sol; the index is the whole calldata.
#[repr(u8)]
enum LidoV4Direction {
    Submit = 0,
    Wrap = 1,
    Unwrap = 2,
    SubmitAndWrap = 3,
}

impl SwapEncoder for LidoV4SwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let config = config
            .ok_or_else(|| EncodingError::FatalError("Lido V4 config is empty".to_string()))?;

        let address = |name: &str| -> Result<Bytes, EncodingError> {
            let value = config.get(name).ok_or_else(|| {
                EncodingError::FatalError(format!("Missing {name} in lido_v4 config"))
            })?;
            let address = value
                .parse::<alloy::primitives::Address>()
                .map_err(|error| {
                    EncodingError::FatalError(format!("Invalid {name} in lido_v4 config: {error}"))
                })?;
            Ok(Bytes::from(address.as_slice()))
        };
        let steth_address = address("steth_address")?;
        let wsteth_address = address("wsteth_address")?;

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
            LidoV4Direction::Submit
        } else if *swap.token_in().address == self.steth_address &&
            *swap.token_out().address == self.wsteth_address
        {
            LidoV4Direction::Wrap
        } else if *swap.token_in().address == self.wsteth_address &&
            *swap.token_out().address == self.steth_address
        {
            LidoV4Direction::Unwrap
        } else if *swap.token_in().address == self.native_token_address &&
            *swap.token_out().address == self.wsteth_address
        {
            // wstETH's receive() stakes and wraps in one call.
            LidoV4Direction::SubmitAndWrap
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

    fn lido_v4_config() -> HashMap<String, String> {
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

    fn encoder() -> LidoV4SwapEncoder {
        LidoV4SwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            Some(lido_v4_config()),
        )
        .unwrap()
    }

    #[test]
    fn test_lido_config_rejects_malformed_addresses() {
        for field in ["steth_address", "wsteth_address"] {
            for invalid in ["0xzz", "0x01", "0x", "0x11111111111111111111111111111111111111111111"]
            {
                let mut config = lido_v4_config();
                config.insert(field.to_string(), invalid.to_string());
                assert!(
                    LidoV4SwapEncoder::new(Bytes::zero(20), Chain::Ethereum, Some(config)).is_err()
                );
            }
            let mut config = lido_v4_config();
            config.remove(field);
            assert!(LidoV4SwapEncoder::new(Bytes::zero(20), Chain::Ethereum, Some(config)).is_err());
        }
        assert!(LidoV4SwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).is_err());
    }

    #[test]
    fn test_encode_lido_v4_submit() {
        let component = ProtocolComponent {
            id: STETH_ADDRESS.to_string(),
            protocol_system: "lido_v4".to_string(),
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
        write_calldata_to_file("test_encode_lido_v4_submit", hex_swap.as_str());
    }

    #[test]
    fn test_encode_lido_v4_wrap() {
        let component = ProtocolComponent {
            id: STETH_ADDRESS.to_string(),
            protocol_system: "lido_v4".to_string(),
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
        write_calldata_to_file("test_encode_lido_v4_wrap", hex_swap.as_str());
    }

    #[test]
    fn test_encode_lido_v4_unwrap() {
        let component = ProtocolComponent {
            id: STETH_ADDRESS.to_string(),
            protocol_system: "lido_v4".to_string(),
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
        write_calldata_to_file("test_encode_lido_v4_unwrap", hex_swap.as_str());
    }

    #[test]
    fn test_encode_lido_v4_submit_and_wrap() {
        let component = ProtocolComponent {
            id: STETH_ADDRESS.to_string(),
            protocol_system: "lido_v4".to_string(),
            ..Default::default()
        };
        let token_in = Bytes::from("0x0000000000000000000000000000000000000000");
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

        assert_eq!(hex_swap, "03");
        write_calldata_to_file("test_encode_lido_v4_submit_and_wrap", hex_swap.as_str());
    }

    #[test]
    fn test_encode_lido_v4_invalid_pair() {
        let component = ProtocolComponent {
            id: STETH_ADDRESS.to_string(),
            protocol_system: "lido_v4".to_string(),
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
