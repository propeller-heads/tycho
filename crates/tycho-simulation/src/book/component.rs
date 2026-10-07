//! The `ProtocolComponent` every book feed emits for a pair, and the ids that identify it.
//!
//! Consumers key their state maps by component id across all protocols, so the id carries the
//! protocol system: two integrations' books for the same pair are two components, and so are the
//! same venue's book feed and a future native integration of it.

use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use crate::protocol::models::ProtocolComponent;

/// The id of a book from `base` into `quote`: the protocol system and the two addresses,
/// concatenated. The order of the two addresses is part of the id, so a venue whose reverse book
/// is a separate book passes the orientation it published, and a venue that emits one book per
/// pair passes the two addresses in a fixed order, keeping the component when the venue flips the
/// pair around.
pub fn pair_component_id(protocol_system: &str, base: &Bytes, quote: &Bytes) -> Bytes {
    Bytes::from([protocol_system.as_bytes(), base, quote].concat())
}

/// A book's component: no contracts, no static attributes, the two tokens in the book's
/// orientation. `id` is the pair's identity (see [`pair_component_id`]) or the pool's address.
pub fn pair_component(
    id: Bytes,
    protocol_system: impl Into<String>,
    protocol_type: impl Into<String>,
    chain: Chain,
    base: Token,
    quote: Token,
) -> ProtocolComponent {
    ProtocolComponent::new(
        id,
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

    /// A 20-byte address of repeated `byte`.
    fn addr(byte: u8) -> Bytes {
        Bytes::from(vec![byte; 20])
    }

    #[test]
    fn the_id_is_the_protocol_system_followed_by_the_two_addresses() {
        let id = pair_component_id("book:venue", &addr(0x11), &addr(0x22));

        // "book:venue" in ASCII, then the twenty bytes of each address.
        assert_eq!(
            id.to_string(),
            "0x626f6f6b3a76656e7565\
             1111111111111111111111111111111111111111\
             2222222222222222222222222222222222222222"
        );
    }

    #[test]
    fn the_id_distinguishes_orientation_and_venue() {
        let (a, b) = (addr(1), addr(2));

        assert_ne!(pair_component_id("book:v", &a, &b), pair_component_id("book:v", &b, &a));
        // Two protocol systems' books for the same pair are two components.
        assert_ne!(pair_component_id("book:v", &a, &b), pair_component_id("v", &a, &b));
    }
}
