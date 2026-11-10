use std::{collections::HashMap, str::FromStr};

use alloy::primitives::Address;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a deposit into fxSAVE for the FxSaveExecutor: USDC or fxUSD into fxSP, then the fxSP
/// into fxSAVE.
///
/// The executor holds fxSP and fxSAVE as immutables, so the calldata is the input token alone
/// (20 bytes). The component only trades into fxSAVE, its id.
#[derive(Clone)]
pub struct FxSaveSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for FxSaveSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Ethereum {
            return Err(EncodingError::FatalError(format!(
                "fxSAVE deposits are only supported on Ethereum, not on {chain}"
            )));
        }
        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let vault = Address::from_str(&swap.component().id).map_err(|e| {
            EncodingError::FatalError(format!(
                "Invalid fxSAVE component id {}: {e}",
                swap.component().id
            ))
        })?;
        if swap.token_out().address.as_ref() != vault.as_slice() {
            return Err(EncodingError::InvalidInput(format!(
                "fxSAVE component {} only deposits into fxSAVE, not into {}",
                swap.component().id,
                swap.token_out().address
            )));
        }
        Ok(swap.token_in().address.to_vec())
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
    use rstest::rstest;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    const FXSAVE: &str = "0x7743e50f534a7f9f1791dde7dcd89f7783eefc39";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const FXUSD: &str = "0x085780639cc2cacd35e474e71f4d000e2405d8f6";
    const EXECUTOR: &str = "0x5c2f5a71f67c01775180adc06909288b4c329308";

    fn encode_deposit(
        component_id: &str,
        token_in: &str,
        token_out: &str,
    ) -> Result<Vec<u8>, EncodingError> {
        let component = ProtocolComponent {
            id: component_id.to_string(),
            protocol_system: "fxsave".to_string(),
            ..Default::default()
        };
        let swap = Swap::new(
            component,
            default_token(Bytes::from(token_in)),
            default_token(Bytes::from(token_out)),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(token_in),
            group_token_out: Bytes::from(token_out),
            origin: None,
        };
        FxSaveSwapEncoder::new(Bytes::from(EXECUTOR), Chain::Ethereum, None)?
            .encode_swap(&swap, &encoding_context)
    }

    #[rstest]
    #[case::usdc(USDC, "fxsave_usdc_deposit")]
    #[case::fxusd(FXUSD, "fxsave_fxusd_deposit")]
    fn test_encode_deposit(#[case] token_in: &str, #[case] test_name: &str) {
        let encoded = encode_deposit(FXSAVE, token_in, FXSAVE).unwrap();

        assert_eq!(encoded, Bytes::from(token_in).to_vec());
        write_calldata_to_file(test_name, encode(&encoded).as_str());
    }

    #[test]
    fn test_redeem_is_rejected() {
        assert!(matches!(
            encode_deposit(FXSAVE, FXSAVE, USDC),
            Err(EncodingError::InvalidInput(_))
        ));
    }

    #[rstest]
    #[case::not_hex("not-hex")]
    #[case::odd_length("0xabc")]
    #[case::too_short("0x7743e50f534a7f9f1791dde7dcd89f7783eefc")]
    fn test_invalid_component_id_is_rejected(#[case] component_id: &str) {
        let Err(EncodingError::FatalError(message)) = encode_deposit(component_id, USDC, FXSAVE)
        else {
            panic!("component id {component_id} was not rejected as a fatal error");
        };

        assert!(message.contains(component_id), "error does not name the id: {message}");
    }

    #[test]
    fn test_rejects_chains_other_than_ethereum() {
        let result = FxSaveSwapEncoder::new(Bytes::from(EXECUTOR), Chain::Base, None);

        assert_eq!(
            result.err(),
            Some(EncodingError::FatalError(
                "fxSAVE deposits are only supported on Ethereum, not on base".to_string()
            ))
        );
    }
}
