use std::{
    collections::{BTreeSet, HashMap},
    time::SystemTime,
};

use alloy::primitives::utils::keccak256;
use serde::{de::DeserializeOwned, Serialize};
use tycho_client::feed::synchronizer::{ComponentWithState, Snapshot, StateSyncMessage};
use tycho_common::{
    models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        token::Token,
        Chain,
    },
    Bytes,
};

use crate::{
    protocol::errors::InvalidSnapshotError,
    rfq::{
        errors::RFQError,
        models::{QuoteRule, TimestampHeader},
    },
};

/// State attribute: the venue's books as JSON.
pub const BOOKS_ATTRIBUTE: &str = "books";
/// Static attribute: the directions the venue quotes, 40 bytes each (token in, then token out).
pub const SWAP_DIRECTIONS_ATTRIBUTE: &str = "swap_directions";

/// The id of a venue's component on `chain`. One per chain, the same across polls.
pub fn component_id(protocol_system: &str, chain: Chain) -> String {
    keccak256(format!("{protocol_system}_{}", chain.id()).as_bytes()).to_string()
}

pub fn encode_swap_directions(directions: &BTreeSet<(Bytes, Bytes)>) -> Bytes {
    let mut encoded = Vec::with_capacity(directions.len() * 40);
    for (token_in, token_out) in directions {
        encoded.extend_from_slice(token_in);
        encoded.extend_from_slice(token_out);
    }
    encoded.into()
}

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

/// The all-pairs component for one poll. Its `tokens` are every token a direction names.
#[allow(clippy::too_many_arguments)]
pub fn all_pairs_component<B: Serialize>(
    protocol_system: &str,
    protocol_type_name: &str,
    chain: Chain,
    swap_directions: &BTreeSet<(Bytes, Bytes)>,
    books: &[B],
    tvl: f64,
    quote_rule: QuoteRule,
) -> Result<ComponentWithState, RFQError> {
    let mut tokens = BTreeSet::new();
    for (token_in, token_out) in swap_directions {
        tokens.insert(token_in.clone());
        tokens.insert(token_out.clone());
    }
    let books = serde_json::to_vec(books)
        .map_err(|e| RFQError::ParsingError(format!("Failed to serialize books: {e}")))?;
    let id = component_id(protocol_system, chain);
    let component = ProtocolComponent {
        id: id.clone(),
        protocol_system: protocol_system.to_string(),
        protocol_type_name: protocol_type_name.to_string(),
        chain,
        tokens: tokens.into_iter().collect(),
        contract_addresses: vec![],
        static_attributes: HashMap::from([
            (SWAP_DIRECTIONS_ATTRIBUTE.to_string(), encode_swap_directions(swap_directions)),
            (QuoteRule::ATTRIBUTE.to_string(), quote_rule.as_str().as_bytes().into()),
        ]),
        ..Default::default()
    };
    let attributes = HashMap::from([(BOOKS_ATTRIBUTE.to_string(), books.into())]);
    Ok(ComponentWithState {
        state: ProtocolComponentState::new(&id, attributes, HashMap::new()),
        component,
        component_tvl: Some(tvl),
        entrypoints: vec![],
    })
}

/// What an all-pairs component carries.
pub struct DecodedAllPairs<B> {
    pub books: Vec<B>,
    pub tokens: HashMap<Bytes, Token>,
    /// `None` when the component carries no `quote_rule` attribute; the builder default applies.
    pub quote_rule: Option<QuoteRule>,
}

/// A missing `books` attribute is a venue with no books. Every token the component names must
/// be in `all_tokens`. Fails on a per-pair component, which has no `swap_directions` attribute.
pub fn decode_all_pairs_component<B: DeserializeOwned>(
    snapshot: &ComponentWithState,
    all_tokens: &HashMap<Bytes, Token>,
) -> Result<DecodedAllPairs<B>, InvalidSnapshotError> {
    if !snapshot
        .component
        .static_attributes
        .contains_key(SWAP_DIRECTIONS_ATTRIBUTE)
    {
        return Err(InvalidSnapshotError::MissingAttribute(format!(
            "Component {} of {} has no {SWAP_DIRECTIONS_ATTRIBUTE} attribute, so its client \
             streams per pair. Build the client with ComponentLayout::AllPairs, or register the \
             per-pair state for it.",
            snapshot.component.id, snapshot.component.protocol_system
        )));
    }
    let mut tokens = HashMap::new();
    for address in &snapshot.component.tokens {
        let token = all_tokens.get(address).ok_or_else(|| {
            InvalidSnapshotError::ValueError(format!("Token not found: {address}"))
        })?;
        tokens.insert(address.clone(), token.clone());
    }
    let books = match snapshot
        .state
        .attributes
        .get(BOOKS_ATTRIBUTE)
    {
        Some(books) => serde_json::from_slice(books)
            .map_err(|e| InvalidSnapshotError::ValueError(format!("Invalid books JSON: {e}")))?,
        None => Vec::new(),
    };
    let quote_rule = QuoteRule::from_attributes(&snapshot.component.static_attributes)
        .map_err(InvalidSnapshotError::ValueError)?;
    Ok(DecodedAllPairs { books, tokens, quote_rule })
}

/// Seconds since the UNIX epoch.
pub fn unix_timestamp() -> Result<u64, RFQError> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| RFQError::ParsingError("SystemTime before UNIX EPOCH!".into()))
}

/// The stream message for one poll: every component in `components`, and the removal of every
/// component the stream emitted before that `components` lacks.
pub fn poll_message(
    current: &mut HashMap<String, ComponentWithState>,
    components: HashMap<String, ComponentWithState>,
    timestamp: u64,
) -> StateSyncMessage<TimestampHeader> {
    let mut removed_components = HashMap::new();
    for (id, component) in current.iter() {
        if !components.contains_key(id) {
            removed_components.insert(id.clone(), component.component.clone());
        }
    }
    *current = components.clone();
    sync_message(components, removed_components, timestamp)
}

fn sync_message(
    states: HashMap<String, ComponentWithState>,
    removed_components: HashMap<String, ProtocolComponent>,
    timestamp: u64,
) -> StateSyncMessage<TimestampHeader> {
    StateSyncMessage {
        header: TimestampHeader { timestamp },
        snapshots: Snapshot { states, vm_storage: HashMap::new() },
        deltas: None,
        removed_components,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rfq::protocols::test_utils::{all_pairs_snapshot, usdc, weth};

    #[test]
    fn swap_directions_round_trip() {
        let directions =
            BTreeSet::from([(weth().address, usdc().address), (usdc().address, weth().address)]);
        let encoded = encode_swap_directions(&directions);
        assert_eq!(encoded.len(), 80);
        let decoded: BTreeSet<_> = decode_swap_directions(&encoded)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(decoded, directions);
    }

    #[test]
    fn swap_directions_length_not_a_multiple_of_40() {
        let mut attribute =
            encode_swap_directions(&BTreeSet::from([(weth().address, usdc().address)])).to_vec();
        attribute.push(0);
        let result = decode_swap_directions(&attribute);
        assert!(matches!(result, Err(message) if message.contains("not a multiple of 40")));
    }

    #[test]
    fn component_tokens_are_the_directions_tokens() {
        let directions = BTreeSet::from([(weth().address, usdc().address)]);
        let component = all_pairs_component(
            "rfq:test",
            "test_pool",
            Chain::Ethereum,
            &directions,
            &["book"],
            100.0,
            QuoteRule::OncePerVenue,
        )
        .unwrap();

        let mut expected_tokens = vec![weth().address, usdc().address];
        expected_tokens.sort();
        assert_eq!(component.component.tokens, expected_tokens);
        assert_eq!(component.component.id, component_id("rfq:test", Chain::Ethereum));
        assert_eq!(component.state.component_id, component.component.id);
        assert_eq!(component.component_tvl, Some(100.0));
        assert_eq!(
            component.component.static_attributes[QuoteRule::ATTRIBUTE].as_ref(),
            b"once_per_venue"
        );
        assert_eq!(
            component.component.static_attributes[SWAP_DIRECTIONS_ATTRIBUTE],
            encode_swap_directions(&directions)
        );
        let books: Vec<String> =
            serde_json::from_slice(&component.state.attributes[BOOKS_ATTRIBUTE]).unwrap();
        assert_eq!(books, ["book"]);
    }

    #[test]
    fn decode_all_pairs_component_missing_token() {
        let (snapshot, mut tokens) = all_pairs_snapshot("rfq:test", &[weth(), usdc()], &["book"]);
        tokens.remove(&weth().address);
        let result = decode_all_pairs_component::<String>(&snapshot, &tokens);
        assert!(
            matches!(result, Err(InvalidSnapshotError::ValueError(msg)) if msg.contains("Token not found"))
        );
    }

    #[test]
    fn decode_all_pairs_component_invalid_books_json() {
        let (mut snapshot, tokens) = all_pairs_snapshot("rfq:test", &[weth(), usdc()], &["book"]);
        snapshot
            .state
            .attributes
            .insert(BOOKS_ATTRIBUTE.to_string(), b"invalid json".into());
        let result = decode_all_pairs_component::<String>(&snapshot, &tokens);
        assert!(
            matches!(result, Err(InvalidSnapshotError::ValueError(msg)) if msg.contains("Invalid books JSON"))
        );
    }

    #[test]
    fn decode_all_pairs_component_rejects_per_pair_component() {
        let (mut snapshot, tokens) = all_pairs_snapshot("rfq:test", &[weth(), usdc()], &["book"]);
        snapshot
            .component
            .static_attributes
            .remove(SWAP_DIRECTIONS_ATTRIBUTE);
        let result = decode_all_pairs_component::<String>(&snapshot, &tokens);
        assert!(
            matches!(result, Err(InvalidSnapshotError::MissingAttribute(msg)) if msg.contains("AllPairs"))
        );
    }

    #[test]
    fn poll_message_removes_dropped_components() {
        let pair = |id: &str| {
            let mut component = all_pairs_component(
                "rfq:test",
                "test_pool",
                Chain::Ethereum,
                &BTreeSet::from([(weth().address, usdc().address)]),
                &["book"],
                100.0,
                QuoteRule::OncePerVenue,
            )
            .unwrap();
            component.component.id = id.to_string();
            (id.to_string(), component)
        };
        let mut current = HashMap::new();

        let first = poll_message(&mut current, HashMap::from([pair("a"), pair("b")]), 1);
        assert_eq!(first.snapshots.states.len(), 2);
        assert!(first.removed_components.is_empty());

        let second = poll_message(&mut current, HashMap::from([pair("b")]), 2);
        assert_eq!(
            second
                .snapshots
                .states
                .keys()
                .collect::<Vec<_>>(),
            ["b"]
        );
        assert_eq!(
            second
                .removed_components
                .keys()
                .collect::<Vec<_>>(),
            ["a"]
        );
    }
}
