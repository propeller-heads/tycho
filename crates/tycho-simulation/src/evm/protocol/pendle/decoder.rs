//! Decoding a Tycho snapshot into `PendleState`.
//!
//! Tycho binds one state type per protocol system, so this dispatches on `protocol_type_name` —
//! the two component types the Substreams package emits carry entirely different attributes.
//!
//! Every attribute read here is one the indexing PR emits. A missing one is an error rather than a
//! default: quoting a market whose `py_index` silently defaulted would be wrong in exactly the way
//! that is hardest to notice.

use std::{collections::HashMap, str::FromStr};

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{
    math::market::MarketState,
    state::{
        attribute_i256, attribute_u256, PendleMarketState, PendleState, PendleSyState, TokenClass,
    },
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

const MARKET_TYPE: &str = "pendle_market";
const SY_TYPE: &str = "pendle_sy";

fn missing(name: &str) -> InvalidSnapshotError {
    InvalidSnapshotError::MissingAttribute(name.to_string())
}

fn address(raw: &Bytes, name: &str) -> Result<Bytes, InvalidSnapshotError> {
    if raw.len() != 20 {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{name} is {} bytes, expected a 20-byte address",
            raw.len()
        )));
    }
    Ok(raw.clone())
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for PendleState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        match snapshot
            .component
            .protocol_type_name
            .as_str()
        {
            MARKET_TYPE => decode_market(snapshot, block).map(PendleState::Market),
            SY_TYPE => decode_sy(snapshot).map(PendleState::Sy),
            other => Err(InvalidSnapshotError::ValueError(format!(
                "unknown Pendle protocol type {other}"
            ))),
        }
    }
}

fn decode_market(
    snapshot: ComponentWithState,
    block: BlockHeader,
) -> Result<PendleMarketState, InvalidSnapshotError> {
    let statics = &snapshot.component.static_attributes;
    let state = &snapshot.state.attributes;

    let scalar_root =
        attribute_i256(statics, "scalar_root").ok_or_else(|| missing("scalar_root"))?;
    let expiry = attribute_u256(statics, "expiry")
        .ok_or_else(|| missing("expiry"))?
        .saturating_to::<u64>();

    let total_pt = attribute_i256(state, "total_pt").ok_or_else(|| missing("total_pt"))?;
    let total_sy = attribute_i256(state, "total_sy").ok_or_else(|| missing("total_sy"))?;
    let last_ln_implied_rate = attribute_u256(state, "last_ln_implied_rate")
        .ok_or_else(|| missing("last_ln_implied_rate"))?;
    let ln_fee_rate_root =
        attribute_u256(state, "ln_fee_rate_root").ok_or_else(|| missing("ln_fee_rate_root"))?;
    let reserve_fee_percent = attribute_u256(state, "reserve_fee_percent")
        .ok_or_else(|| missing("reserve_fee_percent"))?;

    // The live index, not the stored one. `py_index_stored` is a floor that drifts below the rate
    // the contract will actually use, so quoting off it is quietly wrong between interactions.
    let py_index = attribute_u256(state, "py_index_current").ok_or_else(|| {
        InvalidSnapshotError::MissingAttribute(
            "py_index_current (py_index_stored alone is a stale floor and must not be substituted)"
                .to_string(),
        )
    })?;

    // Prefer the indexer's own clock over the block header: it is emitted for every live market on
    // every refresh block, whereas the header only reflects blocks where this component changed.
    let block_timestamp = attribute_u256(state, "block_timestamp")
        .map(|value| value.saturating_to::<u64>())
        .unwrap_or(block.timestamp);

    Ok(PendleMarketState {
        market: MarketState {
            total_pt,
            total_sy,
            scalar_root,
            expiry,
            ln_fee_rate_root,
            reserve_fee_percent,
            last_ln_implied_rate,
        },
        py_index,
        block_timestamp,
        sy_address: address(
            statics
                .get("sy_address")
                .ok_or_else(|| missing("sy_address"))?,
            "sy_address",
        )?,
        pt_address: address(
            statics
                .get("pt_address")
                .ok_or_else(|| missing("pt_address"))?,
            "pt_address",
        )?,
        yt_address: address(
            statics
                .get("yt_address")
                .ok_or_else(|| missing("yt_address"))?,
            "yt_address",
        )?,
    })
}

fn decode_sy(snapshot: ComponentWithState) -> Result<PendleSyState, InvalidSnapshotError> {
    let statics = &snapshot.component.static_attributes;
    let state = &snapshot.state.attributes;

    let sy_address = Bytes::from_str(snapshot.component.id.as_str()).map_err(|e| {
        InvalidSnapshotError::ValueError(format!("SY component id is not an address: {e}"))
    })?;

    let sy_decimals = attribute_u256(statics, "sy_decimals")
        .ok_or_else(|| missing("sy_decimals"))?
        .saturating_to::<u32>();
    let asset_decimals = attribute_u256(statics, "asset_decimals")
        .ok_or_else(|| missing("asset_decimals"))?
        .saturating_to::<u32>();

    let exchange_rate =
        attribute_u256(state, "sy_exchange_rate").ok_or_else(|| missing("sy_exchange_rate"))?;
    if exchange_rate.is_zero() {
        return Err(InvalidSnapshotError::ValueError(
            "sy_exchange_rate is zero, which no live SY reports".to_string(),
        ));
    }
    let rate_stale = state
        .get("sy_rate_stale")
        .is_some_and(|raw| raw.iter().any(|byte| *byte != 0));

    // One attribute per quotable token, named for the direction and the token. A token the indexer
    // could not classify has no attribute here and is simply not quotable.
    let tokens_in = token_classes(statics, "token_in_class_0x")?;
    let tokens_out = token_classes(statics, "token_out_class_0x")?;
    if tokens_in.is_empty() && tokens_out.is_empty() {
        return Err(InvalidSnapshotError::ValueError(format!(
            "SY {sy_address} declares no quotable tokens in either direction"
        )));
    }

    // What the SY holds is what it can pay out when redeeming, so the balances are state the
    // quote depends on, not bookkeeping.
    let token_balances = snapshot
        .state
        .balances
        .iter()
        .map(|(token, balance)| (token.clone(), U256::from_be_slice(balance)))
        .collect();

    Ok(PendleSyState {
        sy_address,
        token_balances,
        component_id: snapshot.component.id.clone(),
        exchange_rate,
        rate_stale,
        sy_decimals,
        asset_decimals,
        tokens_in,
        tokens_out,
    })
}

fn token_classes(
    statics: &HashMap<String, Bytes>,
    prefix: &str,
) -> Result<HashMap<Bytes, TokenClass>, InvalidSnapshotError> {
    let mut classes = HashMap::new();
    for (name, value) in statics {
        let Some(suffix) = name.strip_prefix(prefix) else { continue };
        let token = Bytes::from_str(suffix).map_err(|e| {
            InvalidSnapshotError::ValueError(format!("{name} does not name an address: {e}"))
        })?;
        let class = TokenClass::parse(value).ok_or_else(|| {
            InvalidSnapshotError::ValueError(format!(
                "{name} carries an unknown token class {:?}",
                String::from_utf8_lossy(value)
            ))
        })?;
        classes.insert(token, class);
    }
    Ok(classes)
}
