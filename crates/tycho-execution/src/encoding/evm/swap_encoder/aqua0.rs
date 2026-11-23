use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use num_bigint::BigUint;
use tokio::runtime::Handle;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    Bytes,
};

use super::uniswap_v4::{UniswapV4SwapEncoder, UniswapV4UserData};
use crate::encoding::{
    errors::EncodingError,
    evm::utils::{
        check_signed_quote, create_encoding_runtime, get_static_attribute, on_blocking_thread,
        SafeRuntime,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Fetches Aqua0's binding JIT authorization, then delegates byte packing to Tycho's existing
/// Uniswap V4 encoder and executor. The only custom payload is V4 `hookData`.
///
/// The hook binds the exact input amount, so an Aqua0 swap fills only as the whole route's first
/// hop. This encoder checks what it can see: the swap is the first of its group, it has no split
/// of its own, and its `estimated_amount_in` is set and non-zero. The rest is the solver's to keep,
/// because the encoder sees one swap and not the route:
///
/// - the Aqua0 swap is the first hop of the whole route,
/// - the route has a single Aqua0 swap,
/// - the route has no splits,
/// - its `estimated_amount_in` equals the solution's `amount_in`.
///
/// A route that breaks one of these is still encoded, and its binding quote reserves Aqua0 backing
/// until the deadline, but the transaction reverts on chain instead of filling at another amount.
#[derive(Clone)]
pub struct Aqua0SwapEncoder {
    executor_address: Bytes,
    v4: UniswapV4SwapEncoder,
    runtime_handle: Handle,
    #[expect(
        dead_code,
        reason = "owns the runtime behind `runtime_handle`; dropping it would shut the runtime down"
    )]
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
        // Only what this swap shows is checked here; the route-level rules are in the doc comment.
        if encoding_context.group_token_in != swap.token_in().address {
            return Err(EncodingError::FatalError(
                "Aqua0 must be the first swap because its JIT authorization binds exact input"
                    .into(),
            ));
        }
        if swap.split() != 0.0 {
            return Err(EncodingError::InvalidInput(
                "Aqua0 cannot be split because its JIT authorization binds exact input".into(),
            ));
        }

        // The V4 encoder reads the hook with `Address::from_slice`, which panics on any other
        // length.
        if get_static_attribute(swap, "hooks")?.len() != 20 {
            return Err(EncodingError::FatalError("Aqua0 hooks attribute must be 20 bytes".into()));
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
            .filter(|amount| *amount != BigUint::ZERO)
            .ok_or_else(|| {
                EncodingError::InvalidInput(
                    "a non-zero estimated amount in is required for Aqua0 RFQ".into(),
                )
            })?;
        let router = encoding_context
            .router_address
            .clone()
            .ok_or_else(|| {
                EncodingError::FatalError(
                    "Tycho Router V3 address is required for Aqua0 RFQ".into(),
                )
            })?;

        // The caller's V4 options are kept; the hook data is the binding quote's.
        let mut user_data = UniswapV4UserData::from_swap_user_data(swap.user_data().as_ref())?;
        if !user_data.hook_data.is_empty() {
            return Err(EncodingError::InvalidInput(
                "Aqua0 sets hook_data from its binding quote; leave it out of user_data".into(),
            ));
        }

        // The binding quote reserves backing, so everything that can be checked without it is
        // checked first: a positive amount within the backed sizes, then every static check of
        // the V4 encoding.
        protocol_state.get_amount_out(amount_in.clone(), swap.token_in(), swap.token_out())?;
        self.v4.encode_swap(
            &swap
                .clone()
                .with_user_data(v4_user_data(&user_data)?),
            encoding_context,
        )?;

        let params = GetAmountOutParams {
            amount_in: amount_in.clone(),
            token_in: swap.token_in().address.clone(),
            token_out: swap.token_out().address.clone(),
            sender: router.clone(),
            receiver: router,
            origin: None,
        };

        let signed_quote = on_blocking_thread(|| {
            self.runtime_handle.block_on(async {
                protocol_state
                    .as_indicatively_priced()?
                    .request_signed_quote(params)
                    .await
            })
        })??;
        check_signed_quote(swap, protocol_state.as_ref(), &signed_quote)?;
        if signed_quote.base_token != swap.token_in().address ||
            signed_quote.quote_token != swap.token_out().address ||
            signed_quote.amount_in != amount_in
        {
            return Err(EncodingError::FatalError(
                "Aqua0 binding quote does not match the encoded swap".into(),
            ));
        }
        check_deadline(&signed_quote.quote_attributes)?;
        user_data.hook_data = signed_quote
            .quote_attributes
            .get("hook_data")
            .filter(|data| !data.is_empty())
            .ok_or_else(|| {
                EncodingError::FatalError(
                    "Aqua0 binding quote must contain non-empty hook_data".into(),
                )
            })?
            .clone();

        self.v4.encode_swap(
            &swap
                .clone()
                .with_user_data(v4_user_data(&user_data)?),
            encoding_context,
        )
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

fn v4_user_data(user_data: &UniswapV4UserData) -> Result<Bytes, EncodingError> {
    serde_json::to_vec(user_data)
        .map(Bytes::from)
        .map_err(|error| EncodingError::FatalError(error.to_string()))
}

/// The binding quote's signed deadline, which the hook enforces, must not have passed.
fn check_deadline(attributes: &HashMap<String, Bytes>) -> Result<(), EncodingError> {
    let deadline = attributes
        .get("deadline_timestamp")
        .ok_or_else(|| {
            EncodingError::FatalError("Aqua0 quote must have a deadline_timestamp attribute".into())
        })?;
    let deadline = u64::from_be_bytes(
        deadline
            .as_ref()
            .try_into()
            .map_err(|_wrong_length| {
                EncodingError::InvalidInput(format!(
                    "Aqua0 deadline_timestamp must be 8 bytes, got {}",
                    deadline.len()
                ))
            })?,
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_before_epoch| EncodingError::FatalError("SystemTime before UNIX EPOCH".into()))?
        .as_secs();
    if deadline <= now {
        return Err(EncodingError::RecoverableError(format!("Aqua0 quote expired at {deadline}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use num_bigint::{BigInt, BigUint};
    use tycho_common::{models::protocol::ProtocolComponent, Bytes};

    use super::*;
    use crate::encoding::{evm::testing_utils::MockRFQState, models::default_token};

    fn quote_data(hook_data: Bytes, deadline: u64) -> HashMap<String, Bytes> {
        HashMap::from([
            ("hook_data".into(), hook_data),
            ("deadline_timestamp".into(), Bytes::from(deadline.to_be_bytes().to_vec())),
        ])
    }

    /// The quote requests a mock state received, one per request.
    type Quotes = Arc<std::sync::Mutex<Vec<Option<Bytes>>>>;

    /// A first-hop WETH -> USDC swap on a V4 pool with a hook, and the encoder for it.
    fn fixture(
        static_attributes: HashMap<String, Bytes>,
        level_amount_out: Option<BigUint>,
        deadline: u64,
        user_data: Option<&str>,
    ) -> (Swap, EncodingContext, Aqua0SwapEncoder, Quotes) {
        let token_in = Bytes::from("0x4200000000000000000000000000000000000006");
        let token_out = Bytes::from("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let component = ProtocolComponent {
            protocol_system: "rfq:aqua0".into(),
            static_attributes,
            ..Default::default()
        };
        let state = MockRFQState {
            quote_amount_in: Some(BigUint::from(1_000u32)),
            quote_amount_out: BigUint::from(2_000u32),
            quote_data: quote_data(Bytes::from("0xdeadbeef"), deadline),
            level_amount_out,
            delay: Default::default(),
            received_origins: Default::default(),
        };
        let quotes = Arc::clone(&state.received_origins);
        let mut swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_protocol_state(Arc::new(state))
        .with_estimated_amount_in(BigUint::from(1_000u32));
        if let Some(user_data) = user_data {
            swap = swap.with_user_data(Bytes::from(user_data.as_bytes().to_vec()));
        }
        let context = EncodingContext {
            router_address: Some(Bytes::from("0x9bA632d83e9eF57571256Cf4cc951b8aF1158e9C")),
            group_token_in: token_in,
            group_token_out: token_out,
            origin: None,
        };
        let encoder = Aqua0SwapEncoder::new(
            Bytes::from("0x78db9684220541601E9215bB16b219e5DF6cF0fb"),
            Chain::Base,
            None,
        )
        .unwrap();
        (swap, context, encoder, quotes)
    }

    fn pool_attributes() -> HashMap<String, Bytes> {
        HashMap::from([
            ("key_lp_fee".into(), Bytes::from(BigInt::from(3000).to_signed_bytes_be())),
            ("tick_spacing".into(), Bytes::from(BigInt::from(60).to_signed_bytes_be())),
            ("hooks".into(), Bytes::from("0xACaF2945890AB6caea62bDa459d1922532A500C8")),
        ])
    }

    /// The caller's V4 options survive; only the hook data comes from the quote.
    #[test]
    fn keeps_the_callers_v4_user_data() {
        let (swap, context, encoder, _) = fixture(
            pool_attributes(),
            Some(BigUint::from(2_000u32)),
            u64::MAX,
            Some(r#"{"skip_unlock":true}"#),
        );
        let encoded = encoder
            .encode_swap(&swap, &context)
            .unwrap();
        // group token in | group token out | zero_for_one | skip_unlock | pool params
        assert_eq!(encoded[41], 1, "skip_unlock was dropped");
        assert!(encoded.ends_with(&Bytes::from("0xdeadbeef")));
    }

    #[test]
    fn refuses_caller_hook_data_before_requesting_a_quote() {
        let (swap, context, encoder, quotes) = fixture(
            pool_attributes(),
            Some(BigUint::from(2_000u32)),
            u64::MAX,
            Some(r#"{"hook_data":"0x01"}"#),
        );
        assert!(encoder
            .encode_swap(&swap, &context)
            .unwrap_err()
            .to_string()
            .contains("hook_data"));
        assert!(quotes.lock().unwrap().is_empty(), "no quote was requested");
    }

    /// An amount the state does not price (zero, under the minimum, over the backed depth) and a
    /// pool key the V4 encoder cannot pack are refused before backing is reserved.
    #[test]
    fn refuses_unpriced_amounts_and_bad_pool_keys_before_requesting_a_quote() {
        let mut missing_fee = pool_attributes();
        missing_fee.remove("key_lp_fee");
        for (attributes, level) in
            [(pool_attributes(), None), (missing_fee, Some(BigUint::from(2_000u32)))]
        {
            let (swap, context, encoder, quotes) = fixture(attributes, level, u64::MAX, None);
            assert!(encoder
                .encode_swap(&swap, &context)
                .is_err());
            assert!(quotes.lock().unwrap().is_empty(), "no quote was requested");
        }
    }

    /// A split swap and a zero input are refused before backing is reserved.
    #[test]
    fn refuses_a_split_and_a_zero_amount_before_requesting_a_quote() {
        let refused = |change: fn(Swap) -> Swap, reason: &str| {
            let (swap, context, encoder, quotes) =
                fixture(pool_attributes(), Some(BigUint::from(2_000u32)), u64::MAX, None);
            let error = encoder
                .encode_swap(&change(swap), &context)
                .unwrap_err();
            assert!(
                matches!(error, EncodingError::InvalidInput(ref message) if message.contains(reason)),
                "{error}"
            );
            assert!(quotes.lock().unwrap().is_empty(), "no quote was requested");
        };
        refused(|swap| swap.with_split(0.5), "cannot be split");
        refused(|swap| swap.with_estimated_amount_in(BigUint::ZERO), "non-zero");
    }

    #[test]
    fn refuses_an_expired_quote() {
        let (swap, context, encoder, _) =
            fixture(pool_attributes(), Some(BigUint::from(2_000u32)), 1, None);
        assert!(matches!(
            encoder.encode_swap(&swap, &context),
            Err(EncodingError::RecoverableError(message)) if message.contains("expired")
        ));
    }

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
            quote_data: quote_data(hook_data.clone(), u64::MAX),
            level_amount_out: Some(BigUint::from(2_000u32)),
            delay: Default::default(),
            received_origins: Default::default(),
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
            origin: None,
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
            &[
                hook.as_ref(),
                &u16::try_from(hook_data.len())
                    .unwrap()
                    .to_be_bytes(),
                hook_data.as_ref(),
            ]
            .concat()
        ));
    }

    #[test]
    fn refuses_a_hook_that_is_not_an_address_before_requesting_a_quote() {
        let token_in = Bytes::from("0x4200000000000000000000000000000000000006");
        let token_out = Bytes::from("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let mut static_attributes = HashMap::new();
        static_attributes.insert("hooks".into(), Bytes::from("0x1234"));
        let component = ProtocolComponent {
            protocol_system: "rfq:aqua0".into(),
            static_attributes,
            ..Default::default()
        };
        let state = MockRFQState {
            quote_amount_in: Some(BigUint::from(1_000u32)),
            quote_amount_out: BigUint::from(2_000u32),
            quote_data: quote_data(Bytes::from("0xdeadbeef"), u64::MAX),
            level_amount_out: Some(BigUint::from(2_000u32)),
            delay: Default::default(),
            received_origins: Default::default(),
        };
        let quotes = Arc::clone(&state.received_origins);
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_protocol_state(Arc::new(state))
        .with_estimated_amount_in(BigUint::from(1_000u32));
        let context = EncodingContext {
            router_address: Some(Bytes::from("0x9bA632d83e9eF57571256Cf4cc951b8aF1158e9C")),
            group_token_in: token_in,
            group_token_out: token_out,
            origin: None,
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
            .contains("20 bytes"));
        assert!(quotes.lock().unwrap().is_empty(), "no quote was requested");
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
            origin: None,
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
