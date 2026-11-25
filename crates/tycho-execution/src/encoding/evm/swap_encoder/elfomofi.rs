use std::collections::HashMap;

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::{ELFOMOFI_PROTOCOL_SYSTEM, ROUTER_ETH_ADDRESS},
        utils::biguint_to_u256,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// The chains ElFomoFi's router is deployed on, all at one address.
pub const ELFOMOFI_CHAINS: [Chain; 3] = [Chain::Base, Chain::Bsc, Chain::Robinhood];

/// Slippage applied to the re-quoted output when neither the swap nor the encoder config names
/// another floor rule.
const DEFAULT_SLIPPAGE_BPS: u32 = 50;

/// Encoder config key overriding [`DEFAULT_SLIPPAGE_BPS`].
const SLIPPAGE_BPS_CONFIG_KEY: &str = "slippage_bps";

const BPS_DENOMINATOR: u32 = 10_000;

/// How the minimum output of an ElFomoFi swap is set, passed as the swap's `user_data` JSON,
/// e.g. `{"slippage_bps":30}` or `{"min_amount_out":"1250000000"}`. Without `user_data` the
/// encoder uses `slippage_bps` from its config (50 by default).
///
/// The venue fills an input past its book at the book's capped output and keeps the whole input,
/// so every swap carries a floor, priced for the swap's `estimated_amount_in` and scaled by the
/// executor to the input that actually arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ElfomofiFloor {
    /// Re-quote `estimated_amount_in` on the swap's `protocol_state` and allow this many basis
    /// points below it. Needs the state, which a serialized swap does not carry.
    SlippageBps(u32),
    /// This output, as a decimal string. Must be positive and, when the swap carries its
    /// `protocol_state`, no more than the state quotes.
    MinAmountOut(String),
}

/// Encodes an ElFomoFi swap as tokenIn (20 bytes), tokenOut (20), the reference input (32) and
/// the minimum output for it (32), matching `ElfomofiExecutor`.
#[derive(Clone)]
pub struct ElfomofiSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    slippage_bps: u32,
}

impl SwapEncoder for ElfomofiSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if !ELFOMOFI_CHAINS.contains(&chain) {
            return Err(EncodingError::FatalError(format!(
                "ElFomoFi swaps are not supported on {chain}"
            )));
        }
        let slippage_bps = match config
            .as_ref()
            .and_then(|config| config.get(SLIPPAGE_BPS_CONFIG_KEY))
        {
            Some(value) => value.parse().map_err(|e| {
                EncodingError::FatalError(format!(
                    "ElFomoFi {SLIPPAGE_BPS_CONFIG_KEY} {value}: {e}"
                ))
            })?,
            None => DEFAULT_SLIPPAGE_BPS,
        };
        check_slippage(slippage_bps).map_err(EncodingError::FatalError)?;
        Ok(Self { executor_address, chain, slippage_bps })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        self.check_swap(swap)?;
        let reference_in = swap
            .estimated_amount_in()
            .clone()
            .filter(|amount| *amount > BigUint::ZERO)
            .ok_or_else(|| {
                invalid(swap, "needs a positive estimated_amount_in to price its floor")
            })?;
        let floor = self.floor(swap, &reference_in)?;

        let mut data = Vec::with_capacity(104);
        data.extend_from_slice(&swap.token_in().address);
        data.extend_from_slice(&swap.token_out().address);
        data.extend_from_slice(&biguint_to_u256(&reference_in).to_be_bytes::<32>());
        data.extend_from_slice(&biguint_to_u256(&floor).to_be_bytes::<32>());
        Ok(data)
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

impl ElfomofiSwapEncoder {
    /// Refuses a swap the executor or the venue would revert, or one priced on another venue.
    fn check_swap(&self, swap: &Swap) -> Result<(), EncodingError> {
        let component = swap.component();
        if component.protocol_system != ELFOMOFI_PROTOCOL_SYSTEM {
            return Err(invalid(
                swap,
                format!("belongs to {}, not {ELFOMOFI_PROTOCOL_SYSTEM}", component.protocol_system),
            ));
        }
        if component.chain != self.chain {
            return Err(invalid(swap, format!("is on {}, not {}", component.chain, self.chain)));
        }
        let (token_in, token_out) = (&swap.token_in().address, &swap.token_out().address);
        if token_in == token_out {
            return Err(invalid(swap, format!("swaps {token_in} for itself")));
        }
        for token in [token_in, token_out] {
            if token.len() != 20 ||
                token.iter().all(|byte| *byte == 0) ||
                *token == *ROUTER_ETH_ADDRESS
            {
                return Err(invalid(swap, format!("trades {token}, which is not an ERC20")));
            }
            if !component.tokens.contains(token) {
                return Err(invalid(swap, format!("does not trade {token}")));
            }
        }
        Ok(())
    }

    /// The minimum output for `reference_in`, by the rule the swap names.
    fn floor(&self, swap: &Swap, reference_in: &BigUint) -> Result<BigUint, EncodingError> {
        let rule = match swap
            .user_data()
            .as_ref()
            .filter(|bytes| !bytes.is_empty())
        {
            Some(bytes) => serde_json::from_slice(bytes).map_err(|e| {
                invalid(swap, format!("user_data is not an ElFomoFi floor rule: {e}"))
            })?,
            None => ElfomofiFloor::SlippageBps(self.slippage_bps),
        };
        match rule {
            ElfomofiFloor::SlippageBps(bps) => {
                check_slippage(bps).map_err(|e| invalid(swap, e))?;
                let quote = quote(swap, reference_in)?.ok_or_else(|| {
                    invalid(
                        swap,
                        "carries no protocol_state to re-quote for slippage_bps; pass \
                         {\"min_amount_out\":\"…\"} in user_data instead",
                    )
                })?;
                let kept = BigUint::from(BPS_DENOMINATOR - bps);
                let denominator = BigUint::from(BPS_DENOMINATOR);
                Ok((quote * kept + &denominator - 1u8) / denominator)
            }
            ElfomofiFloor::MinAmountOut(amount) => {
                let floor: BigUint = amount
                    .parse()
                    .map_err(|e| invalid(swap, format!("min_amount_out {amount}: {e}")))?;
                if floor == BigUint::ZERO || floor.bits() > 256 {
                    return Err(invalid(
                        swap,
                        format!("min_amount_out {floor} must be positive and fit in 256 bits"),
                    ));
                }
                if let Some(quote) = quote(swap, reference_in)? {
                    if floor > quote {
                        return Err(invalid(
                            swap,
                            format!("min_amount_out {floor} is above the book's quote {quote}"),
                        ));
                    }
                }
                Ok(floor)
            }
        }
    }
}

/// What the swap's `protocol_state` pays for `amount_in`; `None` without a state.
fn quote(swap: &Swap, amount_in: &BigUint) -> Result<Option<BigUint>, EncodingError> {
    let Some(state) = swap.protocol_state() else {
        return Ok(None);
    };
    let result = state
        .get_amount_out(amount_in.clone(), swap.token_in(), swap.token_out())
        .map_err(|e| invalid(swap, format!("cannot quote {amount_in}: {e}")))?;
    Ok(Some(result.amount))
}

fn check_slippage(bps: u32) -> Result<(), String> {
    if bps >= BPS_DENOMINATOR {
        return Err(format!("ElFomoFi slippage_bps {bps} must be below {BPS_DENOMINATOR}"));
    }
    Ok(())
}

fn invalid(swap: &Swap, reason: impl std::fmt::Display) -> EncodingError {
    EncodingError::InvalidInput(format!(
        "ElFomoFi swap on component {} {reason}",
        swap.component().id
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rstest::rstest;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{testing_utils::MockRFQState, utils::write_calldata_to_file},
        models::default_token,
    };

    const WETH: &str = "0x4200000000000000000000000000000000000006";
    const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    const OTHER: &str = "0x3333333333333333333333333333333333333333";
    const ONE_WETH: u64 = 1_000_000_000_000_000_000;
    /// What the mock book pays for one WETH: 3,000 USDC.
    const QUOTE: u64 = 3_000_000_000;

    fn component() -> ProtocolComponent {
        ProtocolComponent {
            id: "elfomofi-weth-usdc".to_string(),
            protocol_system: ELFOMOFI_PROTOCOL_SYSTEM.to_string(),
            chain: Chain::Base,
            tokens: vec![Bytes::from(WETH), Bytes::from(USDC)],
            ..Default::default()
        }
    }

    fn swap(component: ProtocolComponent, token_in: &str, token_out: &str) -> Swap {
        Swap::new(
            component,
            default_token(Bytes::from(token_in)),
            default_token(Bytes::from(token_out)),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(ONE_WETH))
        .with_protocol_state(Arc::new(MockRFQState {
            level_amount_out: Some(BigUint::from(QUOTE)),
            ..Default::default()
        }))
    }

    fn weth_for_usdc() -> Swap {
        swap(component(), WETH, USDC)
    }

    fn with_rule(swap: Swap, rule: &ElfomofiFloor) -> Swap {
        swap.with_user_data(Bytes::from(serde_json::to_vec(rule).unwrap()))
    }

    fn encode(swap: &Swap) -> Result<Vec<u8>, EncodingError> {
        encode_with(swap, None)
    }

    fn encode_with(
        swap: &Swap,
        config: Option<HashMap<String, String>>,
    ) -> Result<Vec<u8>, EncodingError> {
        let context = EncodingContext {
            router_address: None,
            group_token_in: swap.token_in().address.clone(),
            group_token_out: swap.token_out().address.clone(),
            origin: None,
        };
        ElfomofiSwapEncoder::new(Bytes::zero(20), Chain::Base, config)?.encode_swap(swap, &context)
    }

    fn reason(result: Result<Vec<u8>, EncodingError>) -> String {
        let error = result.unwrap_err();
        assert!(matches!(error, EncodingError::InvalidInput(_)), "{error}");
        error.to_string()
    }

    /// The default floor is the quote less 50 bps, rounded up: 3,000 USDC less 15 is 2,985
    /// (0xb1eb7c40). One WETH is 0x0de0b6b3a7640000.
    #[test]
    fn test_encode_elfomofi_default_slippage() {
        let encoded = encode(&weth_for_usdc()).unwrap();
        write_calldata_to_file("test_encode_elfomofi", &hex::encode(&encoded));

        assert_eq!(
            hex::encode(&encoded),
            String::from("4200000000000000000000000000000000000006") +
                "833589fcd6edb6e08f4c7c32d4f71b54bda02913" +
                "0000000000000000000000000000000000000000000000000de0b6b3a7640000" +
                "00000000000000000000000000000000000000000000000000000000b1eb7c40"
        );
    }

    /// 100 bps off 3,000 USDC is 2,970 (0xb1069a80).
    #[test]
    fn test_encode_elfomofi_slippage_from_user_data() {
        let swap = with_rule(weth_for_usdc(), &ElfomofiFloor::SlippageBps(100));

        let encoded = encode(&swap).unwrap();

        assert_eq!(hex::encode(&encoded[72..]), format!("{:064x}", 2_970_000_000u64));
    }

    #[test]
    fn test_encode_elfomofi_slippage_from_config() {
        let config = HashMap::from([(SLIPPAGE_BPS_CONFIG_KEY.to_string(), "100".to_string())]);

        let encoded = encode_with(&weth_for_usdc(), Some(config)).unwrap();

        assert_eq!(hex::encode(&encoded[72..]), format!("{:064x}", 2_970_000_000u64));
    }

    /// A quote of 1 wei less 1 bp rounds up to 1, never to a floor of 0.
    #[test]
    fn test_encode_elfomofi_slippage_rounds_up() {
        let swap = with_rule(weth_for_usdc(), &ElfomofiFloor::SlippageBps(1)).with_protocol_state(
            Arc::new(MockRFQState {
                level_amount_out: Some(BigUint::from(1u8)),
                ..Default::default()
            }),
        );

        let encoded = encode(&swap).unwrap();

        assert_eq!(hex::encode(&encoded[72..]), format!("{:064x}", 1));
    }

    #[test]
    fn test_encode_elfomofi_min_amount_out() {
        let swap = with_rule(weth_for_usdc(), &ElfomofiFloor::MinAmountOut("2900000000".into()));

        let encoded = encode(&swap).unwrap();

        assert_eq!(hex::encode(&encoded[72..]), format!("{:064x}", 2_900_000_000u64));
    }

    /// A serialized swap loses its state, so `min_amount_out` is its only rule.
    #[test]
    fn test_encode_elfomofi_min_amount_out_without_state() {
        let swap = Swap::new(
            component(),
            default_token(Bytes::from(WETH)),
            default_token(Bytes::from(USDC)),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(ONE_WETH));
        let ruled = with_rule(swap.clone(), &ElfomofiFloor::MinAmountOut("2900000000".into()));

        assert!(encode(&ruled).is_ok());
        assert!(reason(encode(&swap)).contains("min_amount_out"));
    }

    #[rstest]
    #[case::above_the_quote(ElfomofiFloor::MinAmountOut("3000000001".into()), "above the book's quote")]
    #[case::zero(ElfomofiFloor::MinAmountOut("0".into()), "must be positive")]
    #[case::not_a_number(ElfomofiFloor::MinAmountOut("lots".into()), "min_amount_out lots")]
    #[case::past_256_bits(
        ElfomofiFloor::MinAmountOut(format!("{}", BigUint::from(1u8) << 256u32)),
        "fit in 256 bits"
    )]
    #[case::whole_slippage(ElfomofiFloor::SlippageBps(10_000), "must be below")]
    fn test_encode_elfomofi_refuses_floor(#[case] rule: ElfomofiFloor, #[case] expected: &str) {
        let swap = with_rule(weth_for_usdc(), &rule);

        let reason = reason(encode(&swap));

        assert!(reason.contains(expected), "{reason}");
        assert!(reason.contains("elfomofi-weth-usdc"), "{reason}");
    }

    #[test]
    fn test_encode_elfomofi_refuses_unknown_user_data() {
        let swap = weth_for_usdc().with_user_data(Bytes::from(br#"{"floor":1}"#.to_vec()));

        assert!(reason(encode(&swap)).contains("not an ElFomoFi floor rule"));
    }

    #[test]
    fn test_encode_elfomofi_needs_estimated_amount_in() {
        let swap = Swap::new(
            component(),
            default_token(Bytes::from(WETH)),
            default_token(Bytes::from(USDC)),
            BigUint::ZERO,
        );

        assert!(reason(encode(&swap)).contains("estimated_amount_in"));
    }

    #[rstest]
    #[case::other_protocol(ProtocolComponent { protocol_system: "baibai".into(), ..component() }, WETH, USDC, "belongs to baibai")]
    #[case::other_chain(ProtocolComponent { chain: Chain::Bsc, ..component() }, WETH, USDC, "is on bsc")]
    #[case::token_outside_the_pair(component(), WETH, OTHER, "does not trade")]
    #[case::same_token(component(), WETH, WETH, "for itself")]
    #[case::native_token(
        component(),
        "0x0000000000000000000000000000000000000000",
        USDC,
        "not an ERC20"
    )]
    fn test_encode_elfomofi_refuses_swap(
        #[case] component: ProtocolComponent,
        #[case] token_in: &str,
        #[case] token_out: &str,
        #[case] expected: &str,
    ) {
        let reason = reason(encode(&swap(component, token_in, token_out)));

        assert!(reason.contains(expected), "{reason}");
    }

    #[rstest]
    #[case::ethereum(Chain::Ethereum, None)]
    #[case::slippage_past_the_whole(Chain::Base, Some("10000"))]
    #[case::slippage_not_a_number(Chain::Base, Some("half"))]
    fn test_elfomofi_encoder_refuses_config(#[case] chain: Chain, #[case] slippage: Option<&str>) {
        let config = slippage
            .map(|bps| HashMap::from([(SLIPPAGE_BPS_CONFIG_KEY.to_string(), bps.to_string())]));

        assert!(ElfomofiSwapEncoder::new(Bytes::zero(20), chain, config).is_err());
    }

    #[test]
    fn test_elfomofi_floor_json_shape() {
        assert_eq!(
            serde_json::to_string(&ElfomofiFloor::SlippageBps(30)).unwrap(),
            r#"{"slippage_bps":30}"#
        );
        assert_eq!(
            serde_json::to_string(&ElfomofiFloor::MinAmountOut("5".into())).unwrap(),
            r#"{"min_amount_out":"5"}"#
        );
    }
}
