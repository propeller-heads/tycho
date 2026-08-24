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

const ATTR_SCALAR_ROOT: &str = "scalar_root";
const ATTR_EXPIRY: &str = "expiry";
const ATTR_SY_ADDRESS: &str = "sy_address";
const ATTR_PT_ADDRESS: &str = "pt_address";
const ATTR_YT_ADDRESS: &str = "yt_address";
const ATTR_TOTAL_PT: &str = "total_pt";
const ATTR_TOTAL_SY: &str = "total_sy";
const ATTR_LAST_LN_IMPLIED_RATE: &str = "last_ln_implied_rate";
const ATTR_LN_FEE_RATE_ROOT: &str = "ln_fee_rate_root";
const ATTR_RESERVE_FEE_PERCENT: &str = "reserve_fee_percent";
const ATTR_PY_INDEX_CURRENT: &str = "py_index_current";
const ATTR_RATE_SAMPLED_AT: &str = "rate_sampled_at";
const ATTR_SY_DECIMALS: &str = "sy_decimals";
const ATTR_ASSET_DECIMALS: &str = "asset_decimals";
const ATTR_SY_EXCHANGE_RATE: &str = "sy_exchange_rate";
const ATTR_TOKEN_IN_CLASS_PREFIX: &str = "token_in_class_0x";
const ATTR_TOKEN_OUT_CLASS_PREFIX: &str = "token_out_class_0x";

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
            SY_TYPE => decode_sy(snapshot, block).map(PendleState::Sy),
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
        attribute_i256(statics, ATTR_SCALAR_ROOT).ok_or_else(|| missing(ATTR_SCALAR_ROOT))?;
    let expiry = attribute_u256(statics, ATTR_EXPIRY)
        .ok_or_else(|| missing(ATTR_EXPIRY))?
        .saturating_to::<u64>();

    let total_pt = attribute_i256(state, ATTR_TOTAL_PT).ok_or_else(|| missing(ATTR_TOTAL_PT))?;
    let total_sy = attribute_i256(state, ATTR_TOTAL_SY).ok_or_else(|| missing(ATTR_TOTAL_SY))?;
    let last_ln_implied_rate = attribute_u256(state, ATTR_LAST_LN_IMPLIED_RATE)
        .ok_or_else(|| missing(ATTR_LAST_LN_IMPLIED_RATE))?;
    let ln_fee_rate_root = attribute_u256(state, ATTR_LN_FEE_RATE_ROOT)
        .ok_or_else(|| missing(ATTR_LN_FEE_RATE_ROOT))?;
    let reserve_fee_percent = attribute_u256(state, ATTR_RESERVE_FEE_PERCENT)
        .ok_or_else(|| missing(ATTR_RESERVE_FEE_PERCENT))?;

    // The live index, not the stored one. `py_index_stored` is a floor that drifts below the rate
    // the contract will actually use, so quoting off it is quietly wrong between interactions.
    let py_index = attribute_u256(state, ATTR_PY_INDEX_CURRENT).ok_or_else(|| {
        InvalidSnapshotError::MissingAttribute(format!(
            "{ATTR_PY_INDEX_CURRENT} (py_index_stored alone is a stale floor and must not be \
                 substituted)"
        ))
    })?;

    // When `py_index` above was read. Required rather than defaulting to the header: the refresh
    // emits the two together, and a header fallback would date a rate by a block it was not read
    // at, which is precisely the claim the exactness guard exists to check.
    let rate_sampled_at = attribute_u256(state, ATTR_RATE_SAMPLED_AT)
        .ok_or_else(|| missing(ATTR_RATE_SAMPLED_AT))?
        .saturating_to::<u64>();

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
        rate_sampled_at,
        head_timestamp: block.timestamp,
        sy_address: address(
            statics
                .get(ATTR_SY_ADDRESS)
                .ok_or_else(|| missing(ATTR_SY_ADDRESS))?,
            ATTR_SY_ADDRESS,
        )?,
        pt_address: address(
            statics
                .get(ATTR_PT_ADDRESS)
                .ok_or_else(|| missing(ATTR_PT_ADDRESS))?,
            ATTR_PT_ADDRESS,
        )?,
        yt_address: address(
            statics
                .get(ATTR_YT_ADDRESS)
                .ok_or_else(|| missing(ATTR_YT_ADDRESS))?,
            ATTR_YT_ADDRESS,
        )?,
    })
}

fn decode_sy(
    snapshot: ComponentWithState,
    block: BlockHeader,
) -> Result<PendleSyState, InvalidSnapshotError> {
    let statics = &snapshot.component.static_attributes;
    let state = &snapshot.state.attributes;

    let sy_address = Bytes::from_str(snapshot.component.id.as_str()).map_err(|e| {
        InvalidSnapshotError::ValueError(format!("SY component id is not an address: {e}"))
    })?;

    let sy_decimals = attribute_u256(statics, ATTR_SY_DECIMALS)
        .ok_or_else(|| missing(ATTR_SY_DECIMALS))?
        .saturating_to::<u32>();
    let asset_decimals = attribute_u256(statics, ATTR_ASSET_DECIMALS)
        .ok_or_else(|| missing(ATTR_ASSET_DECIMALS))?
        .saturating_to::<u32>();

    let exchange_rate = attribute_u256(state, ATTR_SY_EXCHANGE_RATE)
        .ok_or_else(|| missing(ATTR_SY_EXCHANGE_RATE))?;
    if exchange_rate.is_zero() {
        return Err(InvalidSnapshotError::ValueError(format!(
            "{ATTR_SY_EXCHANGE_RATE} is zero, which no live SY reports"
        )));
    }
    // Dates the rate above, and is emitted with it. Required for the same reason it is on a
    // market: a wrapper quote is only exact at the block its rate was read at.
    let rate_sampled_at = attribute_u256(state, ATTR_RATE_SAMPLED_AT)
        .ok_or_else(|| missing(ATTR_RATE_SAMPLED_AT))?
        .saturating_to::<u64>();

    // One attribute per quotable token, named for the direction and the token. A token the indexer
    // could not classify has no attribute here and is simply not quotable.
    let tokens_in = token_classes(statics, ATTR_TOKEN_IN_CLASS_PREFIX)?;
    let tokens_out = token_classes(statics, ATTR_TOKEN_OUT_CLASS_PREFIX)?;
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
        rate_sampled_at,
        head_timestamp: block.timestamp,
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
