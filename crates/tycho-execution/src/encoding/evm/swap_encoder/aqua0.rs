use std::collections::HashMap;

use tokio::runtime::Handle;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    Bytes,
};

use super::uniswap_v4::UniswapV4SwapEncoder;
use crate::encoding::{
    errors::EncodingError,
    evm::utils::{create_encoding_runtime, on_blocking_thread, SafeRuntime},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Fetches Aqua0's binding JIT authorization, then delegates byte packing to Tycho's existing
/// Uniswap V4 encoder and executor. The only custom payload is V4 `hookData`.
#[derive(Clone)]
pub struct Aqua0SwapEncoder {
    executor_address: Bytes,
    v4: UniswapV4SwapEncoder,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

impl SwapEncoder for Aqua0SwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let v4 = UniswapV4SwapEncoder::new(executor_address.clone(), chain, config)?;
        let (runtime_handle, runtime) = create_encoding_runtime()?;
        Ok(Self { executor_address, v4, runtime_handle, runtime })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        // A later sequential hop receives the previous venue's dynamic output. Aqua0 signs an exact
        // amountSpecified, so that amount is unknowable at encoding time and must be refused.
        if encoding_context.group_token_in != swap.token_in().address {
            return Err(EncodingError::FatalError(
                "Aqua0 must be the first swap because its JIT authorization binds exact input"
                    .into(),
            ));
        }

        let protocol_state = swap
            .protocol_state()
            .as_ref()
            .ok_or_else(|| {
                EncodingError::FatalError("protocol_state is required for Aqua0 RFQ".into())
            })?;
        let amount_in = swap
            .estimated_amount_in()
            .clone()
            .ok_or_else(|| {
                EncodingError::FatalError("estimated amount in is required for Aqua0 RFQ".into())
            })?;
        let router = encoding_context
            .router_address
            .clone()
            .ok_or_else(|| {
                EncodingError::FatalError(
                    "Tycho Router V3 address is required for Aqua0 RFQ".into(),
                )
            })?;
        let params = GetAmountOutParams {
            amount_in: amount_in.clone(),
            token_in: swap.token_in().address.clone(),
            token_out: swap.token_out().address.clone(),
            sender: router.clone(),
            receiver: router,
        };

        let signed_quote = on_blocking_thread(|| {
            self.runtime_handle.block_on(async {
                protocol_state
                    .as_indicatively_priced()
                    .map_err(|error| EncodingError::FatalError(error.to_string()))?
                    .request_signed_quote(params)
                    .await
                    .map_err(|error| EncodingError::FatalError(error.to_string()))
            })
        })??;
        if signed_quote.base_token != swap.token_in().address
            || signed_quote.quote_token != swap.token_out().address
            || signed_quote.amount_in != amount_in
        {
            return Err(EncodingError::FatalError(
                "Aqua0 binding quote does not match the encoded swap".into(),
            ));
        }
        let hook_data = signed_quote
            .quote_attributes
            .get("hook_data")
            .filter(|data| !data.is_empty())
            .ok_or_else(|| {
                EncodingError::FatalError(
                    "Aqua0 binding quote must contain non-empty hook_data".into(),
                )
            })?;

        let user_data = serde_json::to_vec(&serde_json::json!({
            "hook_data": hook_data.to_string(),
        }))
        .map_err(|error| EncodingError::FatalError(error.to_string()))?;
        self.v4.encode_swap(
            &swap
                .clone()
                .with_user_data(user_data.into()),
            encoding_context,
        )
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
    use std::sync::Arc;

    use num_bigint::{BigInt, BigUint};
    use tycho_common::{models::protocol::ProtocolComponent, Bytes};

    use super::*;
    use crate::encoding::{evm::testing_utils::MockRFQState, models::default_token};

    #[test]
    fn forwards_backend_hook_data_through_the_existing_v4_encoding() {
        let token_in = Bytes::from("0x4200000000000000000000000000000000000006");
        let token_out = Bytes::from("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let hook = Bytes::from("0xACaF2945890AB6caea62bDa459d1922532A500C8");
        let hook_data = Bytes::from("0xdeadbeef");
        let mut static_attributes = HashMap::new();
        static_attributes
            .insert("key_lp_fee".into(), Bytes::from(BigInt::from(3000).to_signed_bytes_be()));
        static_attributes
            .insert("tick_spacing".into(), Bytes::from(BigInt::from(60).to_signed_bytes_be()));
        static_attributes.insert("hooks".into(), hook.clone());
        let component = ProtocolComponent {
            protocol_system: "rfq:aqua0".into(),
            static_attributes,
            ..Default::default()
        };
        let amount_in = BigUint::from(1_000u32);
        let state = MockRFQState {
            quote_amount_in: Some(amount_in.clone()),
            quote_amount_out: BigUint::from(2_000u32),
            quote_data: HashMap::from([("hook_data".into(), hook_data.clone())]),
        };
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_protocol_state(Arc::new(state))
        .with_estimated_amount_in(amount_in);
        let context = EncodingContext {
            router_address: Some(Bytes::from("0x9bA632d83e9eF57571256Cf4cc951b8aF1158e9C")),
            group_token_in: token_in,
            group_token_out: token_out,
        };
        let encoder = Aqua0SwapEncoder::new(
            Bytes::from("0x78db9684220541601E9215bB16b219e5DF6cF0fb"),
            Chain::Base,
            None,
        )
        .unwrap();
        let encoded = encoder
            .encode_swap(&swap, &context)
            .unwrap();

        // The packed V4 pool tail is hook address | uint16 hookData length | hookData.
        assert!(encoded.ends_with(
            &[hook.as_ref(), &(hook_data.len() as u16).to_be_bytes(), hook_data.as_ref(),].concat()
        ));
    }

    #[test]
    fn rejects_a_dynamic_later_hop() {
        let token_in = Bytes::from("0x4200000000000000000000000000000000000006");
        let token_out = Bytes::from("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let swap = Swap::new(
            ProtocolComponent::default(),
            default_token(token_in),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from("0x0000000000000000000000000000000000000001"),
            group_token_out: token_out,
        };
        let encoder = Aqua0SwapEncoder::new(
            Bytes::from("0x78db9684220541601E9215bB16b219e5DF6cF0fb"),
            Chain::Base,
            None,
        )
        .unwrap();
        assert!(encoder
            .encode_swap(&swap, &context)
            .unwrap_err()
            .to_string()
            .contains("first swap"));
    }
}
