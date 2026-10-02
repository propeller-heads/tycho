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

/// The venue's component for one poll. Its `tokens` are every token a direction names.
#[allow(clippy::too_many_arguments)]
pub fn venue_component<B: Serialize>(
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

/// What a venue component carries.
pub struct DecodedVenue<B> {
    pub books: Vec<B>,
    pub tokens: HashMap<Bytes, Token>,
    pub quote_rule: Option<QuoteRule>,
}

/// A missing `books` attribute is a venue with no books. Every token the component names must
/// be in `all_tokens`.
pub fn decode_venue<B: DeserializeOwned>(
    snapshot: &ComponentWithState,
    all_tokens: &HashMap<Bytes, Token>,
) -> Result<DecodedVenue<B>, InvalidSnapshotError> {
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
    Ok(DecodedVenue { books, tokens, quote_rule })
}

/// The stream message for one poll: `component` when the venue has books, else the removal of
/// the component the stream emitted before, if any.
pub fn venue_message(
    current: &mut Option<ProtocolComponent>,
    component: Option<ComponentWithState>,
) -> StateSyncMessage<TimestampHeader> {
    let mut states = HashMap::new();
    let mut removed_components = HashMap::new();
    match component {
        Some(component) => {
            *current = Some(component.component.clone());
            states.insert(component.component.id.clone(), component);
        }
        None => {
            if let Some(component) = current.take() {
                removed_components.insert(component.id.clone(), component);
            }
        }
    }
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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
    use crate::rfq::protocols::test_utils::{usdc, weth};

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
    fn swap_directions_partial_entry() {
        assert!(decode_swap_directions(&[0u8; 41]).is_err());
    }

    #[test]
    fn component_tokens_are_the_directions_tokens() {
        let directions = BTreeSet::from([(weth().address, usdc().address)]);
        let component = venue_component(
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
    fn venue_message_removes_the_component_once() {
        let component = venue_component(
            "rfq:test",
            "test_pool",
            Chain::Ethereum,
            &BTreeSet::from([(weth().address, usdc().address)]),
            &["book"],
            100.0,
            QuoteRule::OncePerVenue,
        )
        .unwrap();
        let id = component.component.id.clone();
        let mut current = None;

        let first = venue_message(&mut current, Some(component));
        assert!(first.snapshots.states.contains_key(&id));
        assert!(first.removed_components.is_empty());

        let second = venue_message(&mut current, None);
        assert!(second.snapshots.states.is_empty());
        assert!(second
            .removed_components
            .contains_key(&id));

        let third = venue_message(&mut current, None);
        assert!(third.snapshots.states.is_empty());
        assert!(third.removed_components.is_empty());
    }
}
