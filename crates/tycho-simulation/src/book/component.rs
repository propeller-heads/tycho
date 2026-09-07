//! The `ProtocolComponent` every book feed emits for a pair, and the ids that identify it.
//!
//! Consumers key their state maps by component id across all protocols, so the id carries the
//! venue's name: two venues' books for the same pair are two components.

use alloy::primitives::utils::keccak256;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use crate::protocol::models::ProtocolComponent;

/// The id of a two-sided venue's book for an unordered pair: the same whichever orientation the
/// venue publishes it in, so a pair that flips orientation between updates keeps its component.
pub fn unordered_pair_component_id(venue: &str, a: &Bytes, b: &Bytes) -> String {
    let (token0, token1) = if a.as_ref() <= b.as_ref() { (a, b) } else { (b, a) };
    directed_pair_component_id(venue, token0, token1)
}

/// The id of a one-directional venue's book from `base` into `quote`; the reverse direction is a
/// different book with a different id.
pub fn directed_pair_component_id(venue: &str, base: &Bytes, quote: &Bytes) -> String {
    let pair = format!("{venue}_{}/{}", hex::encode(base), hex::encode(quote));
    keccak256(pair.as_bytes()).to_string()
}

/// A book's component: no contracts, no static attributes, the two tokens in the book's
/// orientation. `id` must be a hex string (see [`unordered_pair_component_id`],
/// [`directed_pair_component_id`], or a pool address).
pub fn pair_component(
    id: &str,
    protocol_system: impl Into<String>,
    protocol_type: impl Into<String>,
    chain: Chain,
    base: Token,
    quote: Token,
) -> ProtocolComponent {
    ProtocolComponent::new(
        Bytes::from(id),
        protocol_system.into(),
        protocol_type.into(),
        chain,
        vec![base, quote],
        vec![],
        Default::default(),
        Default::default(),
        Default::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(last: u8) -> Bytes {
        Bytes::from(vec![last; 20])
    }

    #[test]
    fn unordered_id_ignores_orientation_and_directed_id_does_not() {
        let (a, b) = (addr(1), addr(2));

        assert_eq!(
            unordered_pair_component_id("v", &a, &b),
            unordered_pair_component_id("v", &b, &a)
        );
        assert_ne!(
            directed_pair_component_id("v", &a, &b),
            directed_pair_component_id("v", &b, &a)
        );
        assert_eq!(
            unordered_pair_component_id("v", &b, &a),
            directed_pair_component_id("v", &a, &b)
        );
        // Two venues' books for the same pair are two components.
        assert_ne!(
            directed_pair_component_id("v", &a, &b),
            directed_pair_component_id("w", &a, &b)
        );
    }
}
