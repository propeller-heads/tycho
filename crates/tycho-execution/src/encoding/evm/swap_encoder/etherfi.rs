use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on an Etherfi pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct EtherfiSwapEncoder {
    executor_address: Bytes,
    eeth_address: Bytes,
    weeth_address: Bytes,
    eth_address: Bytes,
}

#[repr(u8)]
enum EtherfiDirection {
    EethToEth = 0,
    EthToEeth = 1,
    EethToWeeth = 2,
    WeethToEeth = 3,
}

impl SwapEncoder for EtherfiSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let config = config
            .ok_or_else(|| EncodingError::FatalError("Etherfi config is empty".to_string()))?;

        let eeth_address = config
            .get("eeth_address")
            .map(|a| Bytes::from(a.as_str()))
            .ok_or_else(|| {
                EncodingError::FatalError("Missing eeth_address in etherfi config".to_string())
            })?;

        let weeth_address = config
            .get("weeth_address")
            .map(|a| Bytes::from(a.as_str()))
            .ok_or_else(|| {
                EncodingError::FatalError("Missing weeth_address in etherfi config".to_string())
            })?;

        Ok(Self {
            executor_address,
            eeth_address,
            weeth_address,
            eth_address: chain.native_token().address,
        })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let direction = if *swap.token_in().address == self.eeth_address &&
            *swap.token_out().address == self.eth_address
        {
            EtherfiDirection::EethToEth
        } else if *swap.token_in().address == self.eth_address &&
            *swap.token_out().address == self.eeth_address
        {
            EtherfiDirection::EthToEeth
        } else if *swap.token_in().address == self.eeth_address &&
            *swap.token_out().address == self.weeth_address
        {
            EtherfiDirection::EethToWeeth
        } else if *swap.token_in().address == self.weeth_address &&
            *swap.token_out().address == self.eeth_address
        {
            EtherfiDirection::WeethToEeth
        } else {
            return Err(EncodingError::InvalidInput("Combination not allowed".to_owned()));
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
    use crate::encoding::models::default_token;

    const EETH_ADDRESS: &str = "0x35fA164735182de50811E8e2E824cFb9B6118ac2";
    const WEETH_ADDRESS: &str = "0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee";
    const REDEMPTION_MANAGER_ADDRESS: &str = "0xDadEf1fFBFeaAB4f68A9fD181395F68b4e4E7Ae0";

    fn etherfi_config() -> HashMap<String, String> {
        HashMap::from([
            ("eeth_address".to_string(), EETH_ADDRESS.to_string()),
            ("weeth_address".to_string(), WEETH_ADDRESS.to_string()),
            ("redemption_manager_address".to_string(), REDEMPTION_MANAGER_ADDRESS.to_string()),
        ])
    }

    fn encoding_context(token_in: &Bytes, token_out: &Bytes) -> EncodingContext {
        EncodingContext {
            router_address: None,
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        }
    }

    fn encoder() -> EtherfiSwapEncoder {
        EtherfiSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            Some(etherfi_config()),
        )
        .unwrap()
    }

    #[test]
    fn test_encode_etherfi_eeth_to_eth() {
        let component = ProtocolComponent {
            id: String::from("0x308861a430be4cce5502d0a12724771fc6daf216"),
            ..Default::default()
        };
        let token_in = Bytes::from(EETH_ADDRESS);
        let token_out = Bytes::from("0x0000000000000000000000000000000000000000");
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
        // direction EethToEth
        assert_eq!(hex_swap, String::from("00"));
    }

    #[test]
    fn test_encode_etherfi_eth_to_eeth() {
        let component = ProtocolComponent {
            id: String::from("0x308861a430be4cce5502d0a12724771fc6daf216"),
            ..Default::default()
        };
        let token_in = Bytes::from("0x0000000000000000000000000000000000000000");
        let token_out = Bytes::from(EETH_ADDRESS);
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
        // direction EthToEeth
        assert_eq!(hex_swap, String::from("01",));
    }

    #[test]
    fn test_encode_etherfi_eeth_to_weeth() {
        let component = ProtocolComponent {
            id: String::from("0xcd5fe23c85820f7b72d0926fc9b05b43e359b7ee"),
            ..Default::default()
        };
        let token_in = Bytes::from(EETH_ADDRESS);
        let token_out = Bytes::from(WEETH_ADDRESS);
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
        // direction EethToWeeth
        assert_eq!(hex_swap, String::from("02",));
    }

    #[test]
    fn test_encode_etherfi_weeth_to_eeth() {
        let component = ProtocolComponent {
            id: String::from("0xcd5fe23c85820f7b72d0926fc9b05b43e359b7ee"),
            ..Default::default()
        };
        let token_in = Bytes::from(WEETH_ADDRESS);
        let token_out = Bytes::from(EETH_ADDRESS);
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
        // direction WeethToEeth
        assert_eq!(hex_swap, String::from("03"));
    }

    #[test]
    fn test_encode_etherfi_invalid_pair() {
        let component = ProtocolComponent {
            id: String::from("0x308861a430be4cce5502d0a12724771fc6daf216"),
            ..Default::default()
        };
        let token_in = Bytes::from(WEETH_ADDRESS);
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
