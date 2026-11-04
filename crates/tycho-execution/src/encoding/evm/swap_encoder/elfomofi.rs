use std::collections::HashMap;

use alloy::primitives::Address;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    serde_primitives::biguint_string,
    swap_encoder::SwapEncoder,
};

const PROTOCOL_SYSTEM: &str = "propamm:elfomofi";
const DEFAULT_SLIPPAGE_BPS: u32 = 50;

pub fn validate_swaps(swaps: &[Swap]) -> Result<(), EncodingError> {
    if swaps
        .iter()
        .filter(|swap| swap.component().protocol_system == PROTOCOL_SYSTEM)
        .nth(1)
        .is_some()
    {
        return Err(EncodingError::InvalidInput(
            "Only one ElFomoFi swap is supported in a solution".into(),
        ));
    }
    Ok(())
}

/// Per-swap execution floor, serialized into `Swap::user_data`.
/// An absolute floor applies at the estimated input; smaller fills scale it down.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ElfomofiSwapPolicy {
    SlippageBps(u32),
    MinAmountOut(#[serde(with = "biguint_string")] BigUint),
}

impl ElfomofiSwapPolicy {
    /// Encodes a validated policy for `Swap::with_user_data`.
    pub fn to_user_data(&self) -> Result<Bytes, EncodingError> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?.into())
    }

    fn validate(&self) -> Result<(), EncodingError> {
        match self {
            Self::SlippageBps(bps) if *bps >= 10_000 => {
                Err(EncodingError::InvalidInput("ElFomoFi slippage_bps must be below 10000".into()))
            }
            Self::MinAmountOut(amount) if amount == &BigUint::ZERO || amount.bits() > 256 => {
                Err(EncodingError::InvalidInput(
                    "ElFomoFi min_amount_out must be positive and fit uint256".into(),
                ))
            }
            Self::SlippageBps(_) | Self::MinAmountOut(_) => Ok(()),
        }
    }
}

/// Encodes a local book quote and minimum output for a directly funded ElFomoFi swap.
#[derive(Clone)]
pub struct ElfomofiSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    slippage_bps: u32,
}

impl ElfomofiSwapEncoder {
    fn encode(&self, swap: &Swap) -> Result<Vec<u8>, EncodingError> {
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
                "ElFomoFi component must use propamm:elfomofi on the encoder's chain".into(),
            ));
        }
        if component.tokens.len() < 2 ||
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
        let pair_id =
            hex::encode([PROTOCOL_SYSTEM.as_bytes(), first.as_slice(), second.as_slice()].concat());
        let component_id = component
            .id
            .strip_prefix("0x")
            .unwrap_or(&component.id);
        if component_id != hex::encode(PROTOCOL_SYSTEM) &&
            (component_id != pair_id || component.tokens.len() != 2)
        {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi component does not match the canonical venue or token pair".into(),
            ));
        }
        let state = swap
            .protocol_state()
            .as_ref()
            .ok_or_else(|| {
                EncodingError::FatalError("ElFomoFi requires its book protocol_state".into())
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
        let quote = state.get_amount_out(amount_in.clone(), swap.token_in(), swap.token_out())?;
        if quote.amount == BigUint::ZERO || quote.amount.bits() > 256 {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi book output must be positive and fit uint256".into(),
            ));
        }
        let policy = swap
            .user_data()
            .as_ref()
            .map(|data| {
                serde_json::from_slice::<ElfomofiSwapPolicy>(data.as_ref()).map_err(|error| {
                    EncodingError::InvalidInput(format!("Invalid ElFomoFi swap policy: {error}"))
                })
            })
            .transpose()?
            .unwrap_or(ElfomofiSwapPolicy::SlippageBps(self.slippage_bps));
        policy.validate()?;
        let minimum = match policy {
            ElfomofiSwapPolicy::SlippageBps(bps) => {
                // Round up so a positive quote retains a positive execution floor.
                (&quote.amount * BigUint::from(10_000 - bps) + BigUint::from(9_999u32)) /
                    BigUint::from(10_000u32)
            }
            ElfomofiSwapPolicy::MinAmountOut(minimum) => minimum,
        };
        let mut encoded = Vec::with_capacity(104);
        encoded.extend_from_slice(sell.as_slice());
        encoded.extend_from_slice(buy.as_slice());
        for amount in [amount_in, &minimum] {
            let bytes = amount.to_bytes_be();
            encoded.resize(encoded.len() + 32 - bytes.len(), 0);
            encoded.extend_from_slice(&bytes);
        }
        Ok(encoded)
    }
}

impl SwapEncoder for ElfomofiSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if !matches!(chain, Chain::Base | Chain::Bsc | Chain::Robinhood) {
            return Err(EncodingError::FatalError(
                "ElFomoFi is configured for Base, BSC and Robinhood".into(),
            ));
        }
        let slippage_bps = config
            .as_ref()
            .and_then(|config| config.get("slippage_bps"))
            .map(|value| {
                value.parse::<u32>().map_err(|error| {
                    EncodingError::InvalidInput(format!(
                        "ElFomoFi slippage_bps must be an integer below 10000: {error}"
                    ))
                })
            })
            .transpose()?
            .unwrap_or(DEFAULT_SLIPPAGE_BPS);
        if slippage_bps >= 10_000 {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi slippage_bps must be below 10000".into(),
            ));
        }
        Ok(Self { executor_address, chain, slippage_bps })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        self.encode(swap)
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
    use std::{any::Any, sync::Arc};

    use tycho_common::{
        dto::ProtocolStateDelta,
        models::{protocol::ProtocolComponent, token::Token},
        simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
        },
    };

    use super::*;
    use crate::encoding::{
        evm::{
            swap_encoder::{
                swap_encoder_registry::SwapEncoderRegistry, uniswap_v2::UniswapV2SwapEncoder,
            },
            tycho_encoders::TychoRouterEncoder,
            utils::write_calldata_to_file,
        },
        models::{default_token, Solution},
        tycho_encoder::TychoEncoder,
    };

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct MockElfomofiBook {
        amount_out: BigUint,
    }
    #[typetag::serde]
    impl ProtocolSim for MockElfomofiBook {
        fn fee(&self) -> f64 {
            0.0
        }
        fn spot_price(&self, _: &Token, _: &Token) -> Result<f64, SimulationError> {
            Ok(2.0)
        }
        fn get_amount_out(
            &self,
            _: BigUint,
            _: &Token,
            _: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            Ok(GetAmountOutResult::new(self.amount_out.clone(), BigUint::ZERO, self.clone_box()))
        }
        fn get_limits(&self, _: Bytes, _: Bytes) -> Result<(BigUint, BigUint), SimulationError> {
            Ok((BigUint::from(1u8) << 256, BigUint::from(1u8) << 256))
        }
        fn delta_transition(
            &mut self,
            _: ProtocolStateDelta,
            _: &HashMap<Bytes, Token>,
            _: &Balances,
        ) -> Result<(), TransitionError> {
            Ok(())
        }
        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn eq(&self, other: &dyn ProtocolSim) -> bool {
            other.as_any().is::<Self>()
        }
    }
    fn tokens() -> (Bytes, Bytes) {
        (
            Bytes::from("0x4200000000000000000000000000000000000006"),
            Bytes::from("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"),
        )
    }
    fn component(chain: Chain) -> ProtocolComponent {
        let (a, b) = tokens();
        ProtocolComponent {
            id: hex::encode([PROTOCOL_SYSTEM.as_bytes(), a.as_ref(), b.as_ref()].concat()),
            protocol_system: PROTOCOL_SYSTEM.into(),
            chain,
            tokens: vec![a, b],
            ..Default::default()
        }
    }
    fn state(amount_out: BigUint) -> Arc<MockElfomofiBook> {
        Arc::new(MockElfomofiBook { amount_out })
    }
    fn swap(chain: Chain, reverse: bool) -> Swap {
        let (mut a, mut b) = tokens();
        if reverse {
            std::mem::swap(&mut a, &mut b);
        }
        let mut a = default_token(a);
        let mut b = default_token(b);
        a.chain = chain;
        b.chain = chain;
        Swap::new(component(chain), a, b, 250_000u64.into())
            .with_estimated_amount_in(100u64.into())
            .with_protocol_state(state(200u64.into()))
    }
    fn pair_swap(chain: Chain, sell: Bytes, buy: Bytes) -> Swap {
        let (first, second) = if sell < buy { (&sell, &buy) } else { (&buy, &sell) };
        let component = ProtocolComponent {
            id: hex::encode([PROTOCOL_SYSTEM.as_bytes(), first.as_ref(), second.as_ref()].concat()),
            protocol_system: PROTOCOL_SYSTEM.into(),
            chain,
            tokens: vec![sell.clone(), buy.clone()],
            ..Default::default()
        };
        let mut sell = default_token(sell);
        let mut buy = default_token(buy);
        sell.chain = chain;
        buy.chain = chain;
        Swap::new(component, sell, buy, BigUint::ZERO)
            .with_estimated_amount_in(100u64.into())
            .with_protocol_state(state(200u64.into()))
    }
    fn venue_swap(chain: Chain, reverse: bool) -> Swap {
        let pair = swap(chain, reverse);
        let mut component = pair.component().clone();
        component.id = hex::encode(PROTOCOL_SYSTEM);
        component
            .tokens
            .push(Bytes::from(vec![0x77; 20]));
        Swap::new(component, pair.token_in().clone(), pair.token_out().clone(), BigUint::ZERO)
            .with_estimated_amount_in(100u64.into())
            .with_protocol_state(state(200u64.into()))
    }
    fn solution_encoder(chain: Chain) -> TychoRouterEncoder {
        let registry = SwapEncoderRegistry::new(chain)
            .register_encoder(PROTOCOL_SYSTEM, Box::new(encoder(chain)));
        TychoRouterEncoder::new(registry, Bytes::from(vec![0x44; 20]))
    }
    fn solution(swaps: Vec<Swap>) -> Solution {
        Solution::new(
            Bytes::from(vec![0x55; 20]),
            Bytes::from(vec![0x55; 20]),
            swaps[0].token_in().address.clone(),
            swaps
                .last()
                .unwrap()
                .token_out()
                .address
                .clone(),
            100u64.into(),
            200u64.into(),
            199u64.into(),
            swaps,
        )
    }
    fn encoder(chain: Chain) -> ElfomofiSwapEncoder {
        ElfomofiSwapEncoder::new(Bytes::from(vec![0x33; 20]), chain, None).unwrap()
    }
    fn context() -> EncodingContext {
        EncodingContext {
            router_address: None,
            group_token_in: Bytes::zero(20),
            group_token_out: Bytes::zero(20),
            origin: None,
        }
    }

    #[test]
    fn test_encode_elfomofi_local_book_both_directions() {
        for reverse in [false, true] {
            let swap = swap(Chain::Base, reverse);
            let encoded = encoder(Chain::Base)
                .encode(&swap)
                .unwrap();
            assert_eq!(encoded.len(), 104);
            assert_eq!(&encoded[..20], swap.token_in().address.as_ref());
            assert_eq!(&encoded[20..40], swap.token_out().address.as_ref());
            assert_eq!(BigUint::from_bytes_be(&encoded[40..72]), 100u64.into());
            assert_eq!(BigUint::from_bytes_be(&encoded[72..]), 199u64.into());
        }
    }
    #[test]
    fn test_encode_elfomofi_fork_calldata() {
        for (reverse, amount_in, amount_out, name) in [
            (false, 600_000_000_000_000_000u64, 1_480_650_386u64, "test_encode_elfomofi_weth_usdc"),
            (true, 1_480_709_614u64, 600_000_000_000_000_000u64, "test_encode_elfomofi_usdc_weth"),
        ] {
            let swap = swap(Chain::Base, reverse)
                .with_estimated_amount_in(amount_in.into())
                .with_protocol_state(state(amount_out.into()));
            let encoded = encoder(Chain::Base)
                .encode(&swap)
                .unwrap();
            write_calldata_to_file(name, &hex::encode(encoded));
        }
    }
    #[test]
    fn test_elfomofi_chains_and_canonical_ids() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            let swap = swap(chain, false);
            assert!(encoder(chain).encode(&swap).is_ok());
            for wrong in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
                if wrong != chain {
                    assert!(encoder(wrong).encode(&swap).is_err());
                }
            }
        }
    }
    #[test]
    fn test_elfomofi_accepts_common_component_hex_ids() {
        let original = swap(Chain::Base, false);
        assert!(!original
            .component()
            .id
            .starts_with("0x"));
        let mut component = original.component().clone();
        component.id = format!("0x{}", component.id);
        let prefixed = Swap::new(
            component,
            original.token_in().clone(),
            original.token_out().clone(),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(100u64.into())
        .with_protocol_state(state(200u64.into()));
        assert_eq!(
            encoder(Chain::Base)
                .encode(&original)
                .unwrap(),
            encoder(Chain::Base)
                .encode(&prefixed)
                .unwrap()
        );
    }
    #[test]
    fn test_elfomofi_accepts_venue_components_with_all_pair_tokens() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            for reverse in [false, true] {
                let venue = venue_swap(chain, reverse);
                let pair = swap(chain, reverse);
                assert_eq!(
                    encoder(chain).encode(&venue).unwrap(),
                    encoder(chain).encode(&pair).unwrap()
                );
                let mut component = venue.component().clone();
                component.id = format!("0x{}", component.id);
                let prefixed = Swap::new(
                    component,
                    venue.token_in().clone(),
                    venue.token_out().clone(),
                    BigUint::ZERO,
                )
                .with_estimated_amount_in(100u64.into())
                .with_protocol_state(state(200u64.into()));
                assert!(encoder(chain).encode(&prefixed).is_ok());
            }
        }
    }
    #[test]
    fn test_elfomofi_rejects_ambiguous_or_mismatched_venue_tokens() {
        let venue = venue_swap(Chain::Base, false);
        let mut legacy = venue.component().clone();
        legacy.id = component(Chain::Base).id;
        let mut missing = venue.component().clone();
        missing.tokens.remove(0);
        for component in [legacy, missing] {
            let invalid = Swap::new(
                component,
                venue.token_in().clone(),
                venue.token_out().clone(),
                BigUint::ZERO,
            )
            .with_estimated_amount_in(100u64.into())
            .with_protocol_state(state(200u64.into()));
            assert!(encoder(Chain::Base)
                .encode(&invalid)
                .is_err());
        }
    }
    #[test]
    fn test_elfomofi_requires_state_and_reference_input() {
        let (a, b) = tokens();
        let swap =
            Swap::new(component(Chain::Base), default_token(a), default_token(b), BigUint::ZERO);
        assert!(encoder(Chain::Base)
            .encode(&swap)
            .is_err());
        let swap = swap.with_protocol_state(state(200u64.into()));
        assert!(encoder(Chain::Base)
            .encode(&swap)
            .is_err());
        for amount in [BigUint::ZERO, BigUint::from(1u8) << 256] {
            assert!(encoder(Chain::Base)
                .encode(
                    &swap
                        .clone()
                        .with_estimated_amount_in(amount)
                )
                .is_err());
        }
    }
    #[test]
    fn test_elfomofi_unsigned_amounts_and_positive_floor() {
        let mut swap = swap(Chain::Base, false);
        swap = swap
            .with_estimated_amount_in((BigUint::from(1u8) << 256) - 1u8)
            .with_protocol_state(state(1u8.into()));
        let encoded = encoder(Chain::Base)
            .encode(&swap)
            .unwrap();
        assert_eq!(BigUint::from_bytes_be(&encoded[72..]), 1u8.into());
        for output in [BigUint::ZERO, BigUint::from(1u8) << 256] {
            assert!(encoder(Chain::Base)
                .encode(
                    &swap
                        .clone()
                        .with_protocol_state(state(output))
                )
                .is_err());
        }
    }
    #[test]
    fn test_elfomofi_slippage_config() {
        for value in ["10000", "-1", "invalid"] {
            assert!(ElfomofiSwapEncoder::new(
                Bytes::zero(20),
                Chain::Base,
                Some(HashMap::from([("slippage_bps".into(), value.into())]))
            )
            .is_err());
        }
        let encoder = ElfomofiSwapEncoder::new(
            Bytes::zero(20),
            Chain::Base,
            Some(HashMap::from([("slippage_bps".into(), "0".into())])),
        )
        .unwrap();
        let encoded = encoder
            .encode(&swap(Chain::Base, false))
            .unwrap();
        assert_eq!(BigUint::from_bytes_be(&encoded[72..]), 200u64.into());
    }
    #[test]
    fn test_elfomofi_per_swap_execution_policy() {
        for (policy, minimum) in [
            (ElfomofiSwapPolicy::SlippageBps(100), 198u64),
            (ElfomofiSwapPolicy::SlippageBps(0), 200u64),
            (ElfomofiSwapPolicy::MinAmountOut(190u64.into()), 190u64),
        ] {
            let swap = swap(Chain::Base, false).with_user_data(policy.to_user_data().unwrap());
            let encoded = encoder(Chain::Base)
                .encode(&swap)
                .unwrap();
            assert_eq!(BigUint::from_bytes_be(&encoded[72..]), minimum.into());
        }
    }

    #[test]
    fn test_elfomofi_rejects_invalid_execution_policy() {
        for policy in [
            ElfomofiSwapPolicy::SlippageBps(10_000),
            ElfomofiSwapPolicy::MinAmountOut(BigUint::ZERO),
            ElfomofiSwapPolicy::MinAmountOut(BigUint::from(1u8) << 256),
        ] {
            assert!(policy.to_user_data().is_err());
        }
        for data in [
            r#"{"slippage_bps":100,"min_amount_out":"190"}"#,
            r#"{"slippage_bps":100,"unknown":1}"#,
            r#"{"slippage_bps":10000}"#,
            r#"{"slippage_bps":-1}"#,
            r#"{"min_amount_out":"0"}"#,
            r#"{"unknown":1}"#,
            "{}",
            "invalid",
        ] {
            let swap = swap(Chain::Base, false).with_user_data(data.as_bytes().to_vec().into());
            assert!(
                encoder(Chain::Base)
                    .encode(&swap)
                    .is_err(),
                "{data}"
            );
        }
    }

    #[test]
    fn test_elfomofi_rejects_invalid_components_and_tokens() {
        let original = swap(Chain::Base, false);
        for component in [
            ProtocolComponent { id: "wrong".into(), ..component(Chain::Base) },
            ProtocolComponent { protocol_system: "elfomofi".into(), ..component(Chain::Base) },
            ProtocolComponent { tokens: vec![], ..component(Chain::Base) },
        ] {
            let invalid = Swap::new(
                component,
                original.token_in().clone(),
                original.token_out().clone(),
                BigUint::ZERO,
            )
            .with_estimated_amount_in(100u64.into())
            .with_protocol_state(state(200u64.into()));
            assert!(encoder(Chain::Base)
                .encode(&invalid)
                .is_err());
        }
        for token in [
            Bytes::zero(20),
            Bytes::from(vec![0xee; 20]),
            Bytes::zero(19),
            original.token_out().address.clone(),
        ] {
            let invalid = Swap::new(
                component(Chain::Base),
                default_token(token),
                original.token_out().clone(),
                BigUint::ZERO,
            )
            .with_estimated_amount_in(100u64.into())
            .with_protocol_state(state(200u64.into()));
            assert!(encoder(Chain::Base)
                .encode(&invalid)
                .is_err());
        }
        assert!(ElfomofiSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).is_err());
    }
    #[test]
    fn test_elfomofi_encoding_needs_no_rpc_runtime_or_router() {
        let swap = swap(Chain::Base, false).with_protocol_state(state(200u64.into()));
        assert!(!encoder(Chain::Base).blocks_on_quote());
        assert!(encoder(Chain::Base)
            .encode_swap(&swap, &context())
            .is_ok());
    }
    #[test]
    fn test_elfomofi_multiple_swaps_are_rejected_across_distinct_pairs() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            let encoder = solution_encoder(chain);
            for legs in [2u8, 4, 8] {
                let swaps = (0..legs)
                    .map(|index| {
                        pair_swap(
                            chain,
                            Bytes::from(vec![index + 1; 20]),
                            Bytes::from(vec![index + 2; 20]),
                        )
                    })
                    .collect();
                assert!(matches!(
                    encoder.encode_solutions(vec![solution(swaps)]),
                    Err(EncodingError::InvalidInput(message))
                        if message.contains("Only one ElFomoFi swap is supported")
                ));
            }
        }
    }
    #[test]
    fn test_elfomofi_multiple_swaps_are_rejected_across_directions_and_splits() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            for reverse in [false, true] {
                for split in [0.0, 0.5] {
                    let first = venue_swap(chain, false).with_split(split);
                    let second = swap(chain, reverse);
                    let mut component = second.component().clone();
                    component.id = format!("0x{}", component.id);
                    let alias = Swap::new(
                        component,
                        second.token_in().clone(),
                        second.token_out().clone(),
                        BigUint::ZERO,
                    )
                    .with_estimated_amount_in(100u64.into())
                    .with_protocol_state(state(200u64.into()));
                    assert!(matches!(
                        solution_encoder(chain).encode_solutions(vec![solution(vec![first, alias])]),
                        Err(EncodingError::InvalidInput(message))
                            if message.contains("Only one ElFomoFi swap is supported")
                    ));
                }
            }
        }
    }
    #[test]
    fn test_elfomofi_usage_validation_only_counts_elfomofi() {
        let first = swap(Chain::Base, false);
        let mut component = first.component().clone();
        component.id = "another-component-id".into();
        let alias = Swap::new(
            component.clone(),
            first.token_out().clone(),
            first.token_in().clone(),
            BigUint::ZERO,
        );
        assert!(validate_swaps(&[first.clone(), alias]).is_err());
        component.protocol_system = "uniswap_v2".into();
        let other = Swap::new(
            component,
            first.token_in().clone(),
            first.token_out().clone(),
            BigUint::ZERO,
        );
        assert!(validate_swaps(&[first, other.clone(), other]).is_ok());
    }
    #[test]
    fn test_elfomofi_can_be_encoded_in_separate_solutions() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            let encoded = solution_encoder(chain)
                .encode_solutions(vec![
                    solution(vec![venue_swap(chain, false)]),
                    solution(vec![venue_swap(chain, true)]),
                ])
                .unwrap();
            assert_eq!(encoded.len(), 2);
        }
    }
    #[test]
    fn test_elfomofi_can_chain_with_other_dexes() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            for first in [false, true] {
                let elfomofi = venue_swap(chain, false);
                let mut third = default_token(Bytes::from(vec![0x66; 20]));
                third.chain = chain;
                let (sell, buy) = if first {
                    (elfomofi.token_out().clone(), third)
                } else {
                    (third, elfomofi.token_in().clone())
                };
                let other = Swap::new(
                    ProtocolComponent {
                        id: Bytes::from(vec![0x88; 20]).to_string(),
                        protocol_system: "uniswap_v2".into(),
                        chain,
                        tokens: vec![sell.address.clone(), buy.address.clone()],
                        ..Default::default()
                    },
                    sell,
                    buy,
                    BigUint::ZERO,
                );
                let swaps = if first { vec![elfomofi, other] } else { vec![other, elfomofi] };
                let registry = SwapEncoderRegistry::new(chain)
                    .register_encoder(PROTOCOL_SYSTEM, Box::new(encoder(chain)))
                    .register_encoder(
                        "uniswap_v2",
                        Box::new(
                            UniswapV2SwapEncoder::new(Bytes::from(vec![0x99; 20]), chain, None)
                                .unwrap(),
                        ),
                    );
                let encoder = TychoRouterEncoder::new(registry, Bytes::from(vec![0x44; 20]));
                assert!(encoder
                    .encode_solutions(vec![solution(swaps)])
                    .is_ok());
            }
        }
    }
}
