//! ElFomoFi on Base, BSC and Robinhood: per-pair books quoted by an operator and filled from one
//! shared vault.
//!
//! The feed polls one view per chain over JSON-RPC, the helper's `getAllOrderbooks()`, which
//! returns every pair's cumulative ask and bid ladders (net of fees, cut at the vault's balance
//! of the output token) and the block it read them at. The states price swaps by interpolating
//! those ladders; they do not model the venue's pricing contracts, whose quoter and engine are
//! unverified and replaced without events, so the helper's answer is the whole model.
//!
//! # What this integration depends on
//!
//! The same three contracts sit at the same addresses on every chain:
//!
//! | Contract | Address                                      | Notes                              |
//! |----------|----------------------------------------------|------------------------------------|
//! | Router   | `0xf0f0F0F0FB0d738452EfD03A28e8be14C76d5f73` | not upgradeable; the fill entry    |
//! | Pricing  | `0xffffffbb2d432b8acb4c57d556c0c721a431d038` | transparent proxy, router-only     |
//! | Vault    | `0xbb1b19f138db3925883a96ff7a304277460e0c99` | Safe; every pair's inventory       |
//!
//! The helper is per chain (`models::ElfomofiSupportedChain::helper`). The pricing proxy was last
//! upgraded on 2026-11-20 on all three chains (Base 59,985,486, BSC 134,716,965, Robinhood
//! 119,145,799); the recordings in `testdata/` postdate it.
//!
//! # Known limits
//!
//! - Quotes worsen in steps from 4-5 s after the block a poll reads and empty after 10-16 s
//!   (`models::ElfomofiSupportedChain::max_read_lag` has the per-chain figures), and a pair
//!   following a reference AMM pool drops to zero when that pool moves about 1%. The feed withdraws
//!   books that are 2 s old, and the executor's floor turns a quote that went stale in flight into
//!   a revert.
//! - Every pair's ladder is cut at the vault's whole balance of its output token, so pairs that
//!   share a token each claim all of it. When two fills in one block together exceed it, the second
//!   pays only what is left of the balance and reverts on its floor; with the vault empty, the
//!   venue reverts it.
//! - A fill moves the rest of the same-direction ladder about 0.1% worse at the next levels
//!   (measured on a Base fork); a continued fill priced on the pre-fill ladder does not include
//!   that, and the floor absorbs it. How the opposite direction moves after a fill is not known, so
//!   a state that has filled one way refuses the other.

mod client;
pub mod feed;
mod models;
mod source;
pub mod state;
#[cfg(test)]
mod test_support;

pub use models::ElfomofiSupportedChain;

/// Protocol system stamped on every component this integration emits.
pub const PROTOCOL_SYSTEM: &str = "pamm:elfomofi";

/// Component type stamped on every pair this integration emits.
pub(crate) const PROTOCOL_TYPE: &str = "elfomofi_pair";

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use num_bigint::BigUint;
    use tycho_common::{models::protocol::ProtocolComponent, Bytes};
    use tycho_execution::encoding::{
        evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        models::{EncodingContext, Swap},
    };

    use super::*;
    use crate::pamm::protocols::elfomofi::test_support::{usdc, weth, weth_usdc_state};

    /// tycho-execution's encoder registry keys the encoder on its own copy of this string.
    #[test]
    fn the_protocol_system_is_the_key_the_encoder_registry_expects() {
        assert_eq!(PROTOCOL_SYSTEM, tycho_execution::encoding::evm::ELFOMOFI_PROTOCOL_SYSTEM);
    }

    /// A swap priced on a state this module emits encodes with the floor re-quoted from that
    /// state: one WETH buys 3,000 USDC on the test book, less the default 50 bps is 2,985.
    #[test]
    fn the_encoder_prices_its_floor_on_the_emitted_state() {
        let component = ProtocolComponent {
            id: "elfomofi".to_string(),
            protocol_system: PROTOCOL_SYSTEM.to_string(),
            chain: tycho_common::models::Chain::Base,
            tokens: vec![weth().address, usdc().address],
            ..Default::default()
        };
        let swap = Swap::new(component, weth(), usdc(), BigUint::ZERO)
            .with_estimated_amount_in(BigUint::from(1_000_000_000_000_000_000u64))
            .with_protocol_state(Arc::new(weth_usdc_state()));
        let executors = format!(r#"{{"base": {{"{PROTOCOL_SYSTEM}": "{}"}}}}"#, Bytes::zero(20));
        let registry = SwapEncoderRegistry::new(tycho_common::models::Chain::Base)
            .add_default_encoders(Some(executors))
            .unwrap();
        let encoder = registry
            .get_encoder(PROTOCOL_SYSTEM)
            .unwrap();
        let context = EncodingContext {
            router_address: None,
            group_token_in: weth().address,
            group_token_out: usdc().address,
            origin: None,
        };

        let data = encoder
            .encode_swap(&swap, &context)
            .unwrap();

        assert_eq!(BigUint::from_bytes_be(&data[72..]), BigUint::from(2_985_000_000u64));
    }
}
