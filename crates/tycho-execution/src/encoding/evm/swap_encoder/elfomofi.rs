use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{primitives::Address, sol_types::SolValue};
use num_bigint::BigUint;
use tokio::runtime::Handle;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{
        bytes_to_address, check_signed_quote, create_encoding_runtime, on_blocking_thread,
        SafeRuntime,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

const ROUTER: &str = "0xf0f0f0f0fb0d738452efd03a28e8be14c76d5f73";
const PROTOCOL_SYSTEM: &str = "elfomofi";

/// Encodes a pre-funded ERC20 swap against the immutable ElFomoFi venue.
///
/// The snapshot feed carries indicative books; the venue does not sign or reserve a fill. The
/// solution's minimum output protects execution when its state changes after quoting.
#[derive(Clone)]
pub struct ElfomofiSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

fn validate_quote(quote: &SignedQuote, params: &GetAmountOutParams) -> Result<u64, EncodingError> {
    if quote.base_token != params.token_in ||
        quote.quote_token != params.token_out ||
        quote.amount_in != params.amount_in ||
        quote.amount_out == BigUint::ZERO ||
        quote.amount_out.bits() > 256
    {
        return Err(EncodingError::InvalidInput(
            "ElFomoFi refreshed quote must match the requested pair and input with positive output"
                .into(),
        ));
    }
    let expiry = quote
        .quote_attributes
        .get("valid_until")
        .ok_or_else(|| EncodingError::FatalError("ElFomoFi quote requires valid_until".into()))?;
    let valid_until = u64::from_be_bytes(
        expiry
            .as_ref()
            .try_into()
            .map_err(|_| {
                EncodingError::InvalidInput("ElFomoFi valid_until must be 8 bytes".into())
            })?,
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| EncodingError::FatalError("SystemTime before UNIX EPOCH".into()))?
        .as_secs();
    if valid_until <= now {
        return Err(EncodingError::RecoverableError("ElFomoFi refreshed quote has expired".into()));
    }
    Ok(valid_until)
}

impl SwapEncoder for ElfomofiSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if !matches!(chain, Chain::Base | Chain::Bsc | Chain::Robinhood) {
            return Err(EncodingError::FatalError(
                "ElFomoFi is configured for Base, BSC and Robinhood".into(),
            ));
        }
        let (runtime_handle, runtime) = create_encoding_runtime()?;
        Ok(Self { executor_address, chain, runtime_handle, runtime })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let sell = bytes_to_address(&swap.token_in().address)?;
        let buy = bytes_to_address(&swap.token_out().address)?;
        if sell == buy ||
            [sell, buy]
                .iter()
                .any(|token| token.is_zero() || *token == Address::repeat_byte(0xee))
        {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi requires two distinct ERC20 tokens".into(),
            ));
        }

        let component = swap.component();
        if component.chain != self.chain || component.protocol_system != PROTOCOL_SYSTEM {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi component must use elfomofi on the encoder's chain".into(),
            ));
        }
        if component.tokens.len() != 2 ||
            !component
                .tokens
                .contains(&swap.token_in().address) ||
            !component
                .tokens
                .contains(&swap.token_out().address)
        {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi component tokens do not match the swap pair".into(),
            ));
        }
        let (first, second) = if sell < buy { (sell, buy) } else { (buy, sell) };
        let expected_id = format!("elfomofi:{ROUTER}:{first:#x}:{second:#x}");
        // Simulation components store text IDs as bytes; conversion back to the common
        // component represents those bytes as hex. Both forms identify the same pinned pair.
        if component.id != expected_id && component.id != hex::encode(expected_id.as_bytes()) {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi component does not match the canonical router/token pair".into(),
            ));
        }
        let state = swap
            .protocol_state()
            .as_ref()
            .ok_or_else(|| {
                EncodingError::FatalError(
                    "ElFomoFi requires protocol_state for a fresh quote".into(),
                )
            })?;
        let amount_in = swap
            .estimated_amount_in()
            .as_ref()
            .ok_or_else(|| {
                EncodingError::FatalError("ElFomoFi requires estimated_amount_in".into())
            })?;
        if amount_in == &BigUint::ZERO || amount_in.bits() > 256 {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi input must be positive and fit uint256".into(),
            ));
        }
        let sender = context
            .router_address
            .clone()
            .ok_or_else(|| {
                EncodingError::FatalError(
                    "ElFomoFi requires the Tycho router address for quoting".into(),
                )
            })?;
        if bytes_to_address(&sender)?.is_zero() {
            return Err(EncodingError::InvalidInput("ElFomoFi quote sender cannot be zero".into()));
        }
        let params = GetAmountOutParams {
            amount_in: amount_in.clone(),
            token_in: swap.token_in().address.clone(),
            token_out: swap.token_out().address.clone(),
            sender: sender.clone(),
            receiver: sender,
            origin: context.origin.clone(),
        };
        let quote = on_blocking_thread(|| {
            self.runtime_handle.block_on(async {
                state
                    .as_indicatively_priced()?
                    .request_signed_quote(GetAmountOutParams {
                        amount_in: params.amount_in.clone(),
                        token_in: params.token_in.clone(),
                        token_out: params.token_out.clone(),
                        sender: params.sender.clone(),
                        receiver: params.receiver.clone(),
                        origin: params.origin.clone(),
                    })
                    .await
            })
        })??;
        let valid_until = validate_quote(&quote, &params)?;
        check_signed_quote(swap, state.as_ref(), &quote)?;
        let mut encoded = (sell, buy).abi_encode_packed();
        encoded.extend_from_slice(&valid_until.to_be_bytes());
        Ok(encoded)
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }

    fn blocks_on_quote(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{
            swap_encoder::swap_encoder_registry::SwapEncoderRegistry, testing_utils::MockRFQState,
            tycho_encoders::TychoRouterEncoder, utils::write_calldata_to_file,
        },
        models::{default_token, Solution},
        tycho_encoder::TychoEncoder,
    };

    fn tokens() -> (Bytes, Bytes) {
        (
            Bytes::from("0x4200000000000000000000000000000000000006"),
            Bytes::from("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
        )
    }

    fn component() -> ProtocolComponent {
        let (a, b) = tokens();
        ProtocolComponent {
            id: format!("elfomofi:{ROUTER}:{a}:{b}"),
            protocol_system: PROTOCOL_SYSTEM.into(),
            chain: Chain::Base,
            tokens: vec![a, b],
            ..Default::default()
        }
    }

    fn encode(
        component: ProtocolComponent,
        sell: Bytes,
        buy: Bytes,
    ) -> Result<Vec<u8>, EncodingError> {
        encode_amount(component, sell, buy, 100u64.into())
    }

    fn encode_amount(
        component: ProtocolComponent,
        sell: Bytes,
        buy: Bytes,
        amount_in: BigUint,
    ) -> Result<Vec<u8>, EncodingError> {
        encode_amount_on(Chain::Base, component, sell, buy, amount_in)
    }

    fn encode_amount_on(
        chain: Chain,
        component: ProtocolComponent,
        sell: Bytes,
        buy: Bytes,
        amount_in: BigUint,
    ) -> Result<Vec<u8>, EncodingError> {
        let context = EncodingContext {
            router_address: Some(Bytes::from(vec![0x44; 20])),
            group_token_in: sell.clone(),
            group_token_out: buy.clone(),
            origin: None,
        };
        let swap = Swap::new(component, default_token(sell), default_token(buy), BigUint::ZERO)
            .with_estimated_amount_in(amount_in)
            .with_protocol_state(Arc::new(MockRFQState {
                quote_amount_out: 200u64.into(),
                quote_data: HashMap::from([(
                    "valid_until".into(),
                    Bytes::from(u64::MAX.to_be_bytes().to_vec()),
                )]),
                ..Default::default()
            }));
        ElfomofiSwapEncoder::new(Bytes::zero(20), chain, None)?.encode_swap(&swap, &context)
    }

    #[test]
    fn test_elfomofi_chain_selection_and_cross_chain_rejection() {
        let (sell, buy) = tokens();
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            let component = ProtocolComponent { chain, ..component() };
            let encoded = encode_amount_on(
                chain,
                component.clone(),
                sell.clone(),
                buy.clone(),
                100u64.into(),
            )
            .unwrap();
            assert_eq!(encoded, [sell.as_ref(), buy.as_ref(), &u64::MAX.to_be_bytes()].concat());
            for wrong_chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
                if chain != wrong_chain {
                    assert!(matches!(
                        encode_amount_on(
                            wrong_chain,
                            component.clone(),
                            sell.clone(),
                            buy.clone(),
                            100u64.into(),
                        ),
                        Err(EncodingError::InvalidInput(_))
                    ));
                }
            }
        }
    }

    #[test]
    fn test_encode_elfomofi_both_directions() {
        let (a, b) = tokens();
        for (name, sell, buy) in
            [("test_encode_elfomofi_weth_usdc", &a, &b), ("test_encode_elfomofi_usdc_weth", &b, &a)]
        {
            let encoded = encode(component(), sell.clone(), buy.clone()).unwrap();
            assert_eq!(encoded, [sell.as_ref(), buy.as_ref(), &u64::MAX.to_be_bytes()].concat());
            write_calldata_to_file(name, &hex::encode(encoded));
        }
    }

    #[test]
    fn test_encode_elfomofi_simulation_component_id() {
        let (a, b) = tokens();
        let raw = component();
        let converted = ProtocolComponent { id: hex::encode(raw.id.as_bytes()), ..raw.clone() };
        assert_eq!(encode(raw, a.clone(), b.clone()).unwrap(), encode(converted, a, b).unwrap());
    }

    #[test]
    fn test_encode_elfomofi_sequential_routes() {
        let (a, b) = tokens();
        let executor = Bytes::from(vec![0x33; 20]);
        let router = Bytes::from(vec![0x44; 20]);
        let registry = SwapEncoderRegistry::new(Chain::Base).register_encoder(
            PROTOCOL_SYSTEM,
            Box::new(ElfomofiSwapEncoder::new(executor.clone(), Chain::Base, None).unwrap()),
        );
        let encoder = TychoRouterEncoder::new(registry, router.clone()).unwrap();
        let state = Arc::new(MockRFQState {
            quote_amount_out: 200u64.into(),
            quote_data: HashMap::from([(
                "valid_until".into(),
                Bytes::from(u64::MAX.to_be_bytes().to_vec()),
            )]),
            ..Default::default()
        });
        for legs in [2, 4, 8] {
            let swaps = (0..legs)
                .map(|index| {
                    let (sell, buy) = if index % 2 == 0 { (&a, &b) } else { (&b, &a) };
                    Swap::new(
                        component(),
                        default_token(sell.clone()),
                        default_token(buy.clone()),
                        200_000u64.into(),
                    )
                    .with_estimated_amount_in(if index == 0 {
                        100u64.into()
                    } else {
                        200u64.into()
                    })
                    .with_protocol_state(state.clone())
                })
                .collect();
            let solution = Solution::new(
                Bytes::from(vec![0x55; 20]),
                Bytes::from(vec![0x55; 20]),
                a.clone(),
                a.clone(),
                100u64.into(),
                200u64.into(),
                199u64.into(),
                swaps,
            );
            let encoded = encoder
                .encode_solutions(vec![solution])
                .unwrap();
            assert_eq!(encoded.len(), 1);
            assert_eq!(encoded[0].interacting_with(), &router);
            assert!(encoded[0]
                .function_signature()
                .starts_with("sequentialSwap("));
            assert_eq!(encoded[0].swaps().len(), legs * 70);
            for (index, leg) in encoded[0]
                .swaps()
                .chunks_exact(70)
                .enumerate()
            {
                let (sell, buy) = if index % 2 == 0 { (&a, &b) } else { (&b, &a) };
                assert_eq!(&leg[..2], &68u16.to_be_bytes());
                assert_eq!(&leg[2..22], executor.as_ref());
                assert_eq!(&leg[22..42], sell.as_ref());
                assert_eq!(&leg[42..62], buy.as_ref());
                assert_eq!(&leg[62..], &u64::MAX.to_be_bytes());
            }
        }
    }

    #[test]
    fn test_elfomofi_input_accepts_unsigned_range_and_rejects_overflow() {
        let (a, b) = tokens();
        for amount in [BigUint::from(1u8) << 255, (BigUint::from(1u8) << 256) - 1u8] {
            assert!(encode_amount(component(), a.clone(), b.clone(), amount).is_ok());
        }
        for amount in [BigUint::ZERO, BigUint::from(1u8) << 256] {
            assert!(matches!(
                encode_amount(component(), a.clone(), b.clone(), amount),
                Err(EncodingError::InvalidInput(_)),
            ));
        }
    }

    #[test]
    fn test_elfomofi_forwards_origin_and_applies_optional_quote_shortfall() {
        let (sell, buy) = tokens();
        let origin = Some(Bytes::from(vec![0x55; 20]));
        let context = EncodingContext {
            router_address: Some(Bytes::from(vec![0x44; 20])),
            group_token_in: sell.clone(),
            group_token_out: buy.clone(),
            origin: origin.clone(),
        };
        let state = Arc::new(MockRFQState {
            quote_amount_out: 198u64.into(),
            level_amount_out: Some(200u64.into()),
            quote_data: HashMap::from([(
                "valid_until".into(),
                Bytes::from(u64::MAX.to_be_bytes().to_vec()),
            )]),
            ..Default::default()
        });
        let swap = Swap::new(component(), default_token(sell), default_token(buy), BigUint::ZERO)
            .with_estimated_amount_in(100u64.into())
            .with_protocol_state(state.clone());
        let encoder = ElfomofiSwapEncoder::new(Bytes::zero(20), Chain::Base, None).unwrap();
        assert!(encoder
            .encode_swap(&swap, &context)
            .is_ok());
        assert!(encoder
            .encode_swap(
                &swap
                    .clone()
                    .with_max_signed_quote_shortfall_bps(100),
                &context
            )
            .is_ok());
        assert!(matches!(
            encoder.encode_swap(&swap.with_max_signed_quote_shortfall_bps(99), &context),
            Err(EncodingError::RecoverableError(_))
        ));
        assert_eq!(*state.received_origins.lock().unwrap(), vec![origin; 3]);
    }

    #[test]
    fn test_elfomofi_rejects_wrong_component() {
        let (a, b) = tokens();
        let original = component();
        for invalid in [
            ProtocolComponent { id: "wrong".into(), ..original.clone() },
            ProtocolComponent { chain: Chain::Ethereum, ..original.clone() },
            ProtocolComponent { protocol_system: "other_protocol".into(), ..original.clone() },
            ProtocolComponent { tokens: vec![a.clone()], ..original.clone() },
            ProtocolComponent { tokens: vec![a.clone(), Bytes::zero(20)], ..original.clone() },
            ProtocolComponent { id: format!("{ROUTER}:{b}:{a}"), ..original },
        ] {
            assert!(encode(invalid, a.clone(), b.clone()).is_err());
        }
    }

    #[test]
    fn test_elfomofi_rejects_native_identical_or_malformed_tokens() {
        let (a, b) = tokens();
        for invalid in [Bytes::zero(20), Bytes::from(vec![0xee; 20]), Bytes::zero(19), b.clone()] {
            assert!(encode(component(), invalid, b.clone()).is_err());
        }
        assert!(encode(component(), a.clone(), a).is_err());
        assert!(ElfomofiSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).is_err());
    }

    #[test]
    fn test_elfomofi_requires_fresh_quote_context() {
        let (a, b) = tokens();
        let context = EncodingContext {
            router_address: Some(Bytes::from(vec![0x44; 20])),
            group_token_in: a.clone(),
            group_token_out: b.clone(),
            origin: None,
        };
        let raw = Swap::new(component(), default_token(a), default_token(b), BigUint::ZERO);
        let encoder = ElfomofiSwapEncoder::new(Bytes::zero(20), Chain::Base, None).unwrap();
        assert!(encoder.blocks_on_quote());
        assert!(matches!(encoder.encode_swap(&raw, &context), Err(EncodingError::FatalError(_))));
        let state = Arc::new(MockRFQState {
            quote_amount_out: 200u64.into(),
            quote_data: HashMap::from([(
                "valid_until".into(),
                Bytes::from(u64::MAX.to_be_bytes().to_vec()),
            )]),
            ..Default::default()
        });
        let without_amount = raw.with_protocol_state(state);
        assert!(matches!(
            encoder.encode_swap(&without_amount, &context),
            Err(EncodingError::FatalError(_))
        ));
        let complete = without_amount.with_estimated_amount_in(100u64.into());
        let without_router = EncodingContext { router_address: None, ..context };
        assert!(matches!(
            encoder.encode_swap(&complete, &without_router),
            Err(EncodingError::FatalError(_))
        ));
    }

    #[test]
    fn test_elfomofi_rejects_mismatched_or_empty_quote() {
        let (a, b) = tokens();
        let params = GetAmountOutParams {
            amount_in: 100u64.into(),
            token_in: a.clone(),
            token_out: b.clone(),
            sender: Bytes::from(vec![0x44; 20]),
            receiver: Bytes::from(vec![0x44; 20]),
            origin: None,
        };
        for (base_token, quote_token, amount_in, amount_out) in [
            (b.clone(), a.clone(), 100u64, 200u64),
            (a.clone(), b.clone(), 101u64, 200u64),
            (a, b, 100u64, 0u64),
        ] {
            let quote = SignedQuote {
                base_token,
                quote_token,
                amount_in: amount_in.into(),
                amount_out: amount_out.into(),
                quote_attributes: HashMap::new(),
            };
            assert!(matches!(validate_quote(&quote, &params), Err(EncodingError::InvalidInput(_))));
        }
    }

    #[test]
    fn test_elfomofi_rejects_missing_malformed_or_stale_expiry() {
        let (a, b) = tokens();
        let context = EncodingContext {
            router_address: Some(Bytes::from(vec![0x44; 20])),
            group_token_in: a.clone(),
            group_token_out: b.clone(),
            origin: None,
        };
        let encoder = ElfomofiSwapEncoder::new(Bytes::zero(20), Chain::Base, None).unwrap();
        for expiry in [None, Some(Bytes::zero(7)), Some(Bytes::from(0u64.to_be_bytes().to_vec()))] {
            let quote_data = expiry
                .map(|value| HashMap::from([("valid_until".into(), value)]))
                .unwrap_or_default();
            let state = Arc::new(MockRFQState {
                quote_amount_out: 200u64.into(),
                quote_data,
                ..Default::default()
            });
            let swap = Swap::new(
                component(),
                default_token(a.clone()),
                default_token(b.clone()),
                BigUint::ZERO,
            )
            .with_estimated_amount_in(100u64.into())
            .with_protocol_state(state);
            assert!(encoder
                .encode_swap(&swap, &context)
                .is_err());
        }
    }
}
