use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::primitives::Address;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

const PROTOCOL_SYSTEM: &str = "propamm:elfomofi";
const DEFAULT_SLIPPAGE_BPS: u32 = 50;

/// Encodes a local book quote and a minimum rate for a directly funded ElFomoFi swap.
#[derive(Clone)]
pub struct ElfomofiSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    slippage_bps: u32,
}

// Only the shared serialized snapshot timing is needed; pricing stays in ProtocolSim.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct SnapshotTiming {
    chain: Chain,
    block_timestamp: u64,
    observed_at: u64,
    max_age_seconds: u64,
}

#[derive(Deserialize)]
struct SnapshotState {
    snapshot: SnapshotTiming,
}
#[derive(Deserialize)]
struct StateEnvelope {
    state: SnapshotState,
}

impl ElfomofiSwapEncoder {
    fn encode_at(&self, swap: &Swap, now: u64) -> Result<Vec<u8>, EncodingError> {
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
        let expected_id =
            hex::encode([PROTOCOL_SYSTEM.as_bytes(), first.as_slice(), second.as_slice()].concat());
        if component
            .id
            .strip_prefix("0x")
            .unwrap_or(&component.id) !=
            expected_id
        {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi component does not match the canonical token pair".into(),
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
        let timing =
            serde_json::from_value::<StateEnvelope>(serde_json::to_value(state.as_ref())?)?
                .state
                .snapshot;
        if timing.chain != self.chain || timing.max_age_seconds == 0 {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi snapshot has an invalid chain or lifetime".into(),
            ));
        }
        let valid_until = timing
            .block_timestamp
            .min(timing.observed_at)
            .checked_add(timing.max_age_seconds)
            .ok_or_else(|| {
                EncodingError::InvalidInput("ElFomoFi snapshot expiry overflows".into())
            })?;
        if now >= valid_until {
            return Err(EncodingError::RecoverableError(
                "ElFomoFi book expired; wait for a fresh snapshot".into(),
            ));
        }
        let quote = state.get_amount_out(amount_in.clone(), swap.token_in(), swap.token_out())?;
        if quote.amount == BigUint::ZERO || quote.amount.bits() > 256 {
            return Err(EncodingError::InvalidInput(
                "ElFomoFi book output must be positive and fit uint256".into(),
            ));
        }
        // Round up so a positive quote always retains a positive execution floor.
        let minimum = (&quote.amount * BigUint::from(10_000 - self.slippage_bps) +
            BigUint::from(9_999u32)) /
            BigUint::from(10_000u32);
        let mut encoded = Vec::with_capacity(112);
        encoded.extend_from_slice(sell.as_slice());
        encoded.extend_from_slice(buy.as_slice());
        encoded.extend_from_slice(&valid_until.to_be_bytes());
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
                value.parse::<u32>().map_err(|_| {
                    EncodingError::InvalidInput(
                        "ElFomoFi slippage_bps must be an integer below 10000".into(),
                    )
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
        _context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| EncodingError::FatalError("SystemTime before UNIX EPOCH".into()))?
            .as_secs();
        self.encode_at(swap, now)
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
            swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
            tycho_encoders::TychoRouterEncoder, utils::write_calldata_to_file,
        },
        models::{default_token, Solution},
        tycho_encoder::TychoEncoder,
    };

    const NOW: u64 = 1_792_586_309;
    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct MockElfomofiBook {
        snapshot: SnapshotTiming,
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
    fn state(chain: Chain, timestamp: u64, amount_out: BigUint) -> Arc<MockElfomofiBook> {
        Arc::new(MockElfomofiBook {
            snapshot: SnapshotTiming {
                chain,
                block_timestamp: timestamp,
                observed_at: timestamp + 1,
                max_age_seconds: 10,
            },
            amount_out,
        })
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
            .with_protocol_state(state(chain, NOW, 200u64.into()))
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
                .encode_at(&swap, NOW)
                .unwrap();
            assert_eq!(encoded.len(), 112);
            assert_eq!(&encoded[..20], swap.token_in().address.as_ref());
            assert_eq!(&encoded[20..40], swap.token_out().address.as_ref());
            assert_eq!(&encoded[40..48], &(NOW + 10).to_be_bytes());
            assert_eq!(BigUint::from_bytes_be(&encoded[48..80]), 100u64.into());
            assert_eq!(BigUint::from_bytes_be(&encoded[80..]), 199u64.into());
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
                .with_protocol_state(state(Chain::Base, NOW, amount_out.into()));
            let encoded = encoder(Chain::Base)
                .encode_at(&swap, NOW)
                .unwrap();
            write_calldata_to_file(name, &hex::encode(encoded));
        }
    }
    #[test]
    fn test_elfomofi_chains_and_canonical_ids() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            let swap = swap(chain, false);
            assert!(encoder(chain)
                .encode_at(&swap, NOW)
                .is_ok());
            for wrong in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
                if wrong != chain {
                    assert!(encoder(wrong)
                        .encode_at(&swap, NOW)
                        .is_err());
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
        .with_protocol_state(state(Chain::Base, NOW, 200u64.into()));
        assert_eq!(
            encoder(Chain::Base)
                .encode_at(&original, NOW)
                .unwrap(),
            encoder(Chain::Base)
                .encode_at(&prefixed, NOW)
                .unwrap()
        );
    }
    #[test]
    fn test_elfomofi_snapshot_expiry_and_chain() {
        let swap = swap(Chain::Base, false);
        assert!(encoder(Chain::Base)
            .encode_at(&swap, NOW + 9)
            .is_ok());
        assert!(matches!(
            encoder(Chain::Base).encode_at(&swap, NOW + 10),
            Err(EncodingError::RecoverableError(_))
        ));
        let wrong = swap.with_protocol_state(state(Chain::Bsc, NOW, 200u64.into()));
        assert!(encoder(Chain::Base)
            .encode_at(&wrong, NOW)
            .is_err());
    }
    #[test]
    fn test_elfomofi_deadline_uses_oldest_timestamp_and_rejects_overflow() {
        let swap = swap(Chain::Base, false);
        for timing in [
            SnapshotTiming {
                chain: Chain::Base,
                block_timestamp: NOW,
                observed_at: NOW - 10,
                max_age_seconds: 10,
            },
            SnapshotTiming {
                chain: Chain::Base,
                block_timestamp: NOW,
                observed_at: NOW,
                max_age_seconds: 0,
            },
            SnapshotTiming {
                chain: Chain::Base,
                block_timestamp: u64::MAX,
                observed_at: u64::MAX,
                max_age_seconds: 10,
            },
        ] {
            let state = Arc::new(MockElfomofiBook { snapshot: timing, amount_out: 200u64.into() });
            assert!(encoder(Chain::Base)
                .encode_at(&swap.clone().with_protocol_state(state), NOW)
                .is_err());
        }
    }
    #[test]
    fn test_elfomofi_requires_snapshot_and_reference_input() {
        let (a, b) = tokens();
        let swap =
            Swap::new(component(Chain::Base), default_token(a), default_token(b), BigUint::ZERO);
        assert!(encoder(Chain::Base)
            .encode_at(&swap, NOW)
            .is_err());
        let swap = swap.with_protocol_state(state(Chain::Base, NOW, 200u64.into()));
        assert!(encoder(Chain::Base)
            .encode_at(&swap, NOW)
            .is_err());
        for amount in [BigUint::ZERO, BigUint::from(1u8) << 256] {
            assert!(encoder(Chain::Base)
                .encode_at(
                    &swap
                        .clone()
                        .with_estimated_amount_in(amount),
                    NOW
                )
                .is_err());
        }
    }
    #[test]
    fn test_elfomofi_unsigned_amounts_and_positive_floor() {
        let mut swap = swap(Chain::Base, false);
        swap = swap
            .with_estimated_amount_in((BigUint::from(1u8) << 256) - 1u8)
            .with_protocol_state(state(Chain::Base, NOW, 1u8.into()));
        let encoded = encoder(Chain::Base)
            .encode_at(&swap, NOW)
            .unwrap();
        assert_eq!(BigUint::from_bytes_be(&encoded[80..]), 1u8.into());
        for output in [BigUint::ZERO, BigUint::from(1u8) << 256] {
            assert!(encoder(Chain::Base)
                .encode_at(
                    &swap
                        .clone()
                        .with_protocol_state(state(Chain::Base, NOW, output)),
                    NOW
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
            .encode_at(&swap(Chain::Base, false), NOW)
            .unwrap();
        assert_eq!(BigUint::from_bytes_be(&encoded[80..]), 200u64.into());
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
            .with_protocol_state(state(Chain::Base, NOW, 200u64.into()));
            assert!(encoder(Chain::Base)
                .encode_at(&invalid, NOW)
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
            .with_protocol_state(state(Chain::Base, NOW, 200u64.into()));
            assert!(encoder(Chain::Base)
                .encode_at(&invalid, NOW)
                .is_err());
        }
        assert!(ElfomofiSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).is_err());
    }
    #[test]
    fn test_elfomofi_encoding_needs_no_rpc_runtime_or_router() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let swap =
            swap(Chain::Base, false).with_protocol_state(state(Chain::Base, now, 200u64.into()));
        assert!(!encoder(Chain::Base).blocks_on_quote());
        assert!(encoder(Chain::Base)
            .encode_swap(&swap, &context())
            .is_ok());
    }
    #[test]
    fn test_elfomofi_sequential_routes_allow_any_leg_count() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (a, _) = tokens();
        let executor = Bytes::from(vec![0x33; 20]);
        let router = Bytes::from(vec![0x44; 20]);
        let registry = SwapEncoderRegistry::new(Chain::Base)
            .register_encoder(PROTOCOL_SYSTEM, Box::new(encoder(Chain::Base)));
        let encoder = TychoRouterEncoder::new(registry, router.clone()).unwrap();
        for legs in [2, 4, 8] {
            let swaps = (0..legs)
                .map(|index| {
                    swap(Chain::Base, index % 2 != 0).with_protocol_state(state(
                        Chain::Base,
                        now,
                        200u64.into(),
                    ))
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
            assert_eq!(encoded[0].interacting_with(), &router);
            assert_eq!(encoded[0].swaps().len(), legs * 134);
            for leg in encoded[0].swaps().chunks_exact(134) {
                assert_eq!(&leg[..2], &132u16.to_be_bytes());
                assert_eq!(&leg[2..22], executor.as_ref());
            }
        }
    }
}
