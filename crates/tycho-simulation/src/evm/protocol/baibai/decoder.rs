use std::collections::HashMap;

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::BaibaiState;
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

pub(super) fn word(bytes: &Bytes) -> Result<U256, InvalidSnapshotError> {
    if bytes.len() > 32 {
        return Err(InvalidSnapshotError::ValueError("BaiBai word exceeds 32 bytes".into()));
    }
    Ok(U256::from_be_slice(bytes))
}

pub(super) fn apply_words(
    words: &mut [U256; 32],
    attrs: &HashMap<String, Bytes>,
    required: bool,
) -> Result<(), InvalidSnapshotError> {
    for (i, output) in words.iter_mut().enumerate() {
        let name = format!("word_{i}");
        match attrs.get(&name) {
            Some(value) => *output = word(value)?,
            None if required => return Err(InvalidSnapshotError::MissingAttribute(name)),
            None => (),
        }
    }
    Ok(())
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for BaibaiState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let base = snapshot
            .component
            .static_attributes
            .get("base")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("base".into()))?
            .clone();
        let quote = snapshot
            .component
            .static_attributes
            .get("quote")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("quote".into()))?
            .clone();
        if base.len() != 20 ||
            quote.len() != 20 ||
            base == quote ||
            base == Bytes::zero(20) ||
            quote == Bytes::zero(20) ||
            snapshot.component.tokens.len() != 2 ||
            !snapshot
                .component
                .tokens
                .contains(&base) ||
            !snapshot
                .component
                .tokens
                .contains(&quote)
        {
            return Err(InvalidSnapshotError::ValueError("invalid BaiBai tokens".into()));
        }
        let decimals = all_tokens
            .get(&quote)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError("missing BaiBai quote token metadata".into())
            })?
            .decimals;
        let c_unit = U256::from(10)
            .checked_pow(U256::from(decimals.saturating_sub(8)))
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError("quote decimals exceed uint256".into())
            })?;
        let mut state = Self {
            id: snapshot.component.id.clone(),
            tokens: [base, quote],
            words: [U256::ZERO; 32],
            balances: [U256::ZERO; 2],
            c_unit,
            timestamp: block.timestamp,
        };
        apply_words(&mut state.words, &snapshot.state.attributes, true)?;
        for (i, token) in state.tokens.iter().enumerate() {
            state.balances[i] = word(
                snapshot
                    .state
                    .balances
                    .get(token)
                    .ok_or_else(|| {
                        InvalidSnapshotError::MissingAttribute(format!("balance for {token}"))
                    })?,
            )?;
        }
        state
            .side(true)
            .and_then(|_| state.side(false))
            .map_err(|e| InvalidSnapshotError::ValueError(e.to_string()))?;
        Ok(state)
    }
}
