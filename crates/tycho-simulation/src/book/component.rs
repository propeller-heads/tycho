//! The `ProtocolComponent` every book feed emits for a pair, and the ids that identify it.
//!
//! Consumers key their state maps by component id across all protocols, so the id carries the
//! protocol system: two integrations' books for the same pair are two components, and so are the
//! same venue's book feed and a future native integration of it.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use crate::{book::Book, protocol::models::ProtocolComponent};

/// The id of a book from `base` into `quote`: the protocol system and the two addresses,
/// concatenated. The order of the two addresses is part of the id, so a venue whose reverse book
/// is a separate book passes the orientation it published, and a venue that emits one book per
/// pair passes the two addresses in a fixed order, keeping the component when the venue flips the
/// pair around.
pub fn pair_component_id(protocol_system: &str, base: &Bytes, quote: &Bytes) -> Bytes {
    Bytes::from([protocol_system.as_bytes(), base, quote].concat())
}

/// The id of `market_maker`'s book on that pair: its [`pair_component_id`] followed by the
/// maker's name, for a venue whose quote comes from the one maker it is asked of.
pub(crate) fn maker_component_id(
    protocol_system: &str,
    base: &Bytes,
    quote: &Bytes,
    market_maker: &str,
) -> Bytes {
    let pair = pair_component_id(protocol_system, base, quote);
    Bytes::from([pair.as_ref(), market_maker.as_bytes()].concat())
}

/// Static attribute: the directions a component's books quote, 40 bytes each, the token sold
/// first. A component carries it where its token list does not already say which way round it
/// trades, and a consumer that finds none reads every permutation of the tokens instead.
pub const SWAP_DIRECTIONS_ATTRIBUTE: &str = "swap_directions";

pub(crate) fn encode_swap_directions<'a>(
    directions: impl IntoIterator<Item = &'a (Bytes, Bytes)>,
) -> Bytes {
    let mut encoded = Vec::new();
    for (token_in, token_out) in directions {
        encoded.extend_from_slice(token_in);
        encoded.extend_from_slice(token_out);
    }
    encoded.into()
}

/// The directions a component advertises in its [`SWAP_DIRECTIONS_ATTRIBUTE`].
pub fn decode_swap_directions(attribute: &[u8]) -> Result<Vec<(Bytes, Bytes)>, String> {
    let (directions, rest) = attribute.as_chunks::<40>();
    if !rest.is_empty() {
        return Err(format!(
            "Swap directions attribute holds {} bytes, not a multiple of 40",
            attribute.len()
        ));
    }
    Ok(directions
        .iter()
        .map(|direction| (Bytes::from(&direction[..20]), Bytes::from(&direction[20..])))
        .collect())
}

/// A book's component: no contracts, the two tokens in the book's orientation, and whatever
/// static attributes the caller stamps on it. `id` is the pair's identity (see
/// [`pair_component_id`]) or the pool's address.
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

/// A state that prices one pair.
pub trait PairState: ProtocolSim {
    fn base_token(&self) -> &Token;

    fn quote_token(&self) -> &Token;

    /// The directions this book prices, as `(token sold, token bought)`. Only the state knows
    /// which sides of it have levels, so only the state can say: a book nobody bids on buys
    /// nothing, and one with no asks sells nothing, whatever pair it was published as.
    fn directions(&self) -> Vec<(Bytes, Bytes)>;
}

/// The book `state` prices, under `id`: the state, the component it belongs to, and when the
/// venue last updated it.
///
/// A book with levels on both sides trades every way its two tokens can be read, so its
/// component says no more than its token list does. One with levels on a single side, or on
/// neither, names what it quotes in its [`SWAP_DIRECTIONS_ATTRIBUTE`].
pub fn pair_book<S: PairState + 'static>(
    protocol_system: &str,
    protocol_type: &str,
    chain: Chain,
    id: Bytes,
    state: S,
    updated_at: Option<DateTime<Utc>>,
) -> Book {
    let base = state.base_token().clone();
    let quote = state.quote_token().clone();
    let both_ways = [
        (base.address.clone(), quote.address.clone()),
        (quote.address.clone(), base.address.clone()),
    ];
    let mut component = pair_component(id, protocol_system, protocol_type, chain, base, quote);
    let directions = state.directions();
    if !both_ways
        .iter()
        .all(|direction| directions.contains(direction))
    {
        component
            .static_attributes
            .insert(SWAP_DIRECTIONS_ATTRIBUTE.to_string(), encode_swap_directions(&directions));
    }
    Book { component, state: Arc::new(state), updated_at }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_common::models::Chain;

    use super::*;
    use crate::evm::decoder::MockProtocolSim;

    /// A 20-byte address of repeated `byte`.
    fn addr(byte: u8) -> Bytes {
        Bytes::from(vec![byte; 20])
    }

    #[test]
    fn a_makers_id_is_the_pair_id_followed_by_the_maker_name() {
        let id = maker_component_id("rfq:venue", &addr(0x11), &addr(0x22), "mm1");

        // "rfq:venue", the twenty bytes of each address, then "mm1", all in ASCII.
        assert_eq!(
            id.to_string(),
            "0x7266713a76656e7565\
             1111111111111111111111111111111111111111\
             2222222222222222222222222222222222222222\
             6d6d31"
        );
    }

    #[test]
    fn the_id_is_the_protocol_system_followed_by_the_two_addresses() {
        let id = pair_component_id("rfq:venue", &addr(0x11), &addr(0x22));

        // "rfq:venue" in ASCII, then the twenty bytes of each address.
        assert_eq!(
            id.to_string(),
            "0x7266713a76656e7565\
             1111111111111111111111111111111111111111\
             2222222222222222222222222222222222222222"
        );
    }

    #[test]
    fn the_id_distinguishes_orientation_and_venue() {
        let (a, b) = (addr(1), addr(2));

        assert_ne!(pair_component_id("rfq:v", &a, &b), pair_component_id("rfq:v", &b, &a));
        // Two protocol systems' books for the same pair are two components.
        assert_ne!(pair_component_id("rfq:v", &a, &b), pair_component_id("v", &a, &b));
    }

    /// The twenty bytes of each address, the token sold first, and no attribute at all where
    /// the component's own token list reads as the directions.
    #[rstest]
    #[case::buys_base_only(
        vec![(addr(0x11), addr(0x22))],
        Some("0x1111111111111111111111111111111111111111\
                2222222222222222222222222222222222222222")
    )]
    #[case::sells_base_only(
        vec![(addr(0x22), addr(0x11))],
        Some("0x2222222222222222222222222222222222222222\
                1111111111111111111111111111111111111111")
    )]
    #[case::no_levels(vec![], Some("0x"))]
    #[case::both_ways(vec![(addr(0x11), addr(0x22)), (addr(0x22), addr(0x11))], None)]
    fn a_pair_component_advertises_the_directions_its_tokens_do_not(
        #[case] directions: Vec<(Bytes, Bytes)>,
        #[case] expected: Option<&str>,
    ) {
        let token = |address: Bytes, symbol: &str| {
            Token::new(&address, symbol, 18, 0, &[Some(10_000)], Chain::Ethereum, 100)
        };
        let mut state = MockProtocolSim::new();
        state
            .expect_base_token()
            .return_const(token(addr(0x11), "BASE"));
        state
            .expect_quote_token()
            .return_const(token(addr(0x22), "QUOTE"));
        state
            .expect_directions()
            .return_const(directions);

        let book = pair_book("rfq:venue", "venue_pool", Chain::Ethereum, addr(0x33), state, None);

        assert_eq!(
            book.component
                .static_attributes
                .get(SWAP_DIRECTIONS_ATTRIBUTE)
                .map(|attribute| attribute.to_string()),
            expected.map(str::to_string)
        );
    }
}
