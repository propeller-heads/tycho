use std::{collections::HashMap, env, str::FromStr, time::Duration};

use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{models::token::Token, Bytes};

use super::{
    client::Aqua0Client,
    models::{Aqua0Market, Aqua0StateResponse},
    state::Aqua0State,
};
use crate::{
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
    rfq::models::TimestampHeader,
};

fn text_attribute(
    attributes: &HashMap<String, Bytes>,
    name: &str,
) -> Result<String, InvalidSnapshotError> {
    let value = attributes
        .get(name)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.into()))?;
    String::from_utf8(value.to_vec())
        .map_err(|error| InvalidSnapshotError::ValueError(format!("Invalid {name}: {error}")))
}

impl TryFromWithBlock<ComponentWithState, TimestampHeader> for Aqua0State {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _header: TimestampHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        if snapshot.component.tokens.len() != 2 {
            return Err(InvalidSnapshotError::ValueError(
                "Aqua0 component must contain two tokens".into(),
            ));
        }
        let token0 = all_tokens
            .get(&snapshot.component.tokens[0])
            .cloned()
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError("Aqua0 token0 metadata missing".into())
            })?;
        let token1 = all_tokens
            .get(&snapshot.component.tokens[1])
            .cloned()
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError("Aqua0 token1 metadata missing".into())
            })?;

        let raw_state = snapshot
            .state
            .attributes
            .get("state")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("state".into()))?;
        let state: Aqua0StateResponse = serde_json::from_slice(raw_state).map_err(|error| {
            InvalidSnapshotError::ValueError(format!("Invalid Aqua0 state JSON: {error}"))
        })?;
        if state.component_id != snapshot.component.id {
            return Err(InvalidSnapshotError::ValueError(
                "Aqua0 component identity mismatch".into(),
            ));
        }

        let static_attributes = &snapshot.component.static_attributes;
        let market = Aqua0Market {
            pool_id: state.pool_id.clone(),
            class_id: state.class_id.clone(),
            amount0_samples: text_attribute(static_attributes, "amount0_samples")?
                .split(',')
                .map(str::to_string)
                .collect(),
            amount1_samples: text_attribute(static_attributes, "amount1_samples")?
                .split(',')
                .map(str::to_string)
                .collect(),
        };
        let client = Aqua0Client::new(
            snapshot.component.chain,
            text_attribute(static_attributes, "api_url")?,
            market,
            env::var("AQUA0_RFQ_API_KEY").unwrap_or_default(),
            env::var("AQUA0_RFQ_OPERATOR_KEY").unwrap_or_default(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .map_err(|error| InvalidSnapshotError::ValueError(error.to_string()))?;

        // Validate that all on-chain identity fields are valid hex before accepting the snapshot.
        Bytes::from_str(&state.pool_id).map_err(|error| {
            InvalidSnapshotError::ValueError(format!("Invalid poolId: {error}"))
        })?;
        Bytes::from_str(&state.hooks)
            .map_err(|error| InvalidSnapshotError::ValueError(format!("Invalid hooks: {error}")))?;

        Ok(Aqua0State {
            token0,
            token1,
            fee_units: state.fee,
            state_version: state.state_version,
            expires_at: state.expires_at,
            zero_for_one: state.directions.zero_for_one,
            one_for_zero: state.directions.one_for_zero,
            client,
        })
    }
}
