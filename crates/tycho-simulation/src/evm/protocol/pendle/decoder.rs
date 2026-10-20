//! Decoding a Tycho snapshot into `PendleState`.
//!
//! Tycho binds one state type per protocol system, so this dispatches on `protocol_type_name` —
//! the two component types the Substreams package emits carry entirely different attributes.
//!
//! Every attribute read here is one the indexing PR emits. A missing one is an error rather than a
//! default, with two exceptions that both describe a real on-chain state rather than a guess:
//!
//! - A market's reserves and implied rate are absent until its first liquidity event, because the
//!   indexer only writes them from events. A market's storage starts zeroed, so absent means zero.
//! - A rate (`py_index_current`, `sy_exchange_rate`) is absent until a refresh first reads it. The
//!   state then decodes as never sampled, which no head counts as current: it refuses every quote
//!   and reports no depth until a delta brings the rate in.
//!
//! Failing either case instead would lose the component for the life of the stream, since a
//! component that fails to decode is never decoded again.

use std::{collections::HashMap, str::FromStr};

use alloy::primitives::{I256, U256};
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{
    clock::NEVER_SAMPLED,
    math::market::MarketState,
    state::{
        attribute_i256, attribute_reserve, attribute_u256, class_attribute_token,
        lifted_cap_as_none, step_attribute_token, u256_from_be, Direction, PendleMarketState,
        PendleState, PendleSyState, TokenClass, ValueTooWide, ENTRY_FEE_BPS, ENTRY_FEE_FROM,
        NO_CLASS, PAUSED, SY_PAUSED, SY_SUPPLY_CAP, SY_TOTAL_SUPPLY, YIELD_TOKEN,
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
const ATTR_FACTORY: &str = "factory";
/// The original market factory. Its markets lack the `MarketZeroNetLPFee` check.
const ORIGINAL_FACTORY: [u8; 20] = hex_literal::hex!("27b1dacd74688af24a64bd3c9c1b143118740784");
const ATTR_TOTAL_SY: &str = "total_sy";
const ATTR_LAST_LN_IMPLIED_RATE: &str = "last_ln_implied_rate";
const ATTR_LN_FEE_RATE_ROOT: &str = "ln_fee_rate_root";
const ATTR_RESERVE_FEE_PERCENT: &str = "reserve_fee_percent";
const ATTR_PY_INDEX_CURRENT: &str = "py_index_current";
const ATTR_RATE_SAMPLED_AT: &str = "rate_sampled_at";
const ATTR_SY_DECIMALS: &str = "sy_decimals";
const ATTR_ASSET_DECIMALS: &str = "asset_decimals";
const ATTR_SY_EXCHANGE_RATE: &str = "sy_exchange_rate";

impl From<ValueTooWide> for InvalidSnapshotError {
    fn from(error: ValueTooWide) -> Self {
        InvalidSnapshotError::ValueError(error.to_string())
    }
}

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
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        match snapshot
            .component
            .protocol_type_name
            .as_str()
        {
            MARKET_TYPE => decode_market(snapshot, block).map(PendleState::Market),
            SY_TYPE => decode_sy(snapshot, block, all_tokens).map(PendleState::Sy),
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
        attribute_i256(statics, ATTR_SCALAR_ROOT)?.ok_or_else(|| missing(ATTR_SCALAR_ROOT))?;
    let expiry = attribute_u256(statics, ATTR_EXPIRY)?
        .ok_or_else(|| missing(ATTR_EXPIRY))?
        .saturating_to::<u64>();

    // Written only from Mint, Burn, Swap and UpdateImpliedRate, so a market that has not seen its
    // first liquidity yet carries none of them. The contract's storage is zero at that point too.
    let total_pt = attribute_reserve(state, ATTR_TOTAL_PT)?.unwrap_or(I256::ZERO);
    let total_sy = attribute_reserve(state, ATTR_TOTAL_SY)?.unwrap_or(I256::ZERO);
    let last_ln_implied_rate =
        attribute_u256(state, ATTR_LAST_LN_IMPLIED_RATE)?.unwrap_or(U256::ZERO);
    // Read from the factory at creation, which can fail. A market without them decodes, and
    // refuses to quote until a fee update arrives: failing the decode would retire it for good, and
    // by default stop the stream.
    let ln_fee_rate_root = attribute_u256(state, ATTR_LN_FEE_RATE_ROOT)?;
    let reserve_fee_percent = attribute_u256(state, ATTR_RESERVE_FEE_PERCENT)?;
    let fee_unread = ln_fee_rate_root.is_none() || reserve_fee_percent.is_none();

    // The live index, not the stored one. `py_index_stored` is a floor that drifts below the rate
    // the contract will actually use, so quoting off it is quietly wrong between interactions.
    let (py_index, rate_sampled_at) = rate_reading(state, ATTR_PY_INDEX_CURRENT)?;

    // Emitted with the market at creation, from the SY's `paused()` as read then.
    let sy_paused = !attribute_u256(state, SY_PAUSED)?
        .ok_or_else(|| missing(SY_PAUSED))?
        .is_zero();

    Ok(PendleMarketState {
        market: MarketState {
            total_pt,
            total_sy,
            scalar_root,
            expiry,
            ln_fee_rate_root: ln_fee_rate_root.unwrap_or_default(),
            reserve_fee_percent: reserve_fee_percent.unwrap_or_default(),
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
        sy_paused,
        execution_timestamp: 0,
        fee_unread,
        from_original_factory: statics
            .get(ATTR_FACTORY)
            .is_some_and(|factory| factory.as_ref() == ORIGINAL_FACTORY),
        memo: Default::default(),
    })
}

fn decode_sy(
    snapshot: ComponentWithState,
    block: BlockHeader,
    all_tokens: &HashMap<Bytes, Token>,
) -> Result<PendleSyState, InvalidSnapshotError> {
    let statics = &snapshot.component.static_attributes;
    let state = &snapshot.state.attributes;

    let sy_address = Bytes::from_str(snapshot.component.id.as_str()).map_err(|e| {
        InvalidSnapshotError::ValueError(format!("SY component id is not an address: {e}"))
    })?;

    let sy_decimals = attribute_u256(statics, ATTR_SY_DECIMALS)?
        .ok_or_else(|| missing(ATTR_SY_DECIMALS))?
        .saturating_to::<u32>();
    let asset_decimals = attribute_u256(statics, ATTR_ASSET_DECIMALS)?
        .ok_or_else(|| missing(ATTR_ASSET_DECIMALS))?
        .saturating_to::<u32>();

    let (exchange_rate, rate_sampled_at) = rate_reading(state, ATTR_SY_EXCHANGE_RATE)?;

    // One state attribute per quotable token, named for the direction and the token. A token the
    // indexer could not classify has no attribute here, or `NO_CLASS` once an upgrade took its
    // class away, and is simply not quotable. They are state because an upgrade can change them,
    // so an SY with none still decodes: a later delta can classify its tokens.
    let (tokens_in, tokens_out) = token_classes(state)?;

    // Supply bounds every redemption, and is emitted with the SY at creation.
    let total_supply =
        attribute_u256(state, SY_TOTAL_SUPPLY)?.ok_or_else(|| missing(SY_TOTAL_SUPPLY))?;
    let paused = !attribute_u256(state, PAUSED)?
        .ok_or_else(|| missing(PAUSED))?
        .is_zero();

    // Every bound on a wrap edge converts between a token amount and an SY amount, so the token's
    // own decimals are state the quote depends on. Taken from the stream's token set rather than
    // from an attribute: the indexer does not publish them, and a token it does not carry is not
    // routable anyway — such a token is simply absent here and its edges report no depth. Every
    // listed token is looked up, not only today's classified ones, since an upgrade can classify
    // another.
    let token_decimals = snapshot
        .component
        .tokens
        .iter()
        .filter_map(|token| {
            all_tokens
                .get(token)
                .map(|known| (token.clone(), known.decimals))
        })
        .collect();

    // An SY that caps its own supply carries the cap; one without a cap carries nothing, and one
    // whose cap was lifted carries `type(uint256).max`.
    let supply_cap = attribute_u256(state, SY_SUPPLY_CAP)?.and_then(lifted_cap_as_none);
    // What the SY holds of its yield token bounds a redemption to it. Both are optional: without
    // them a redemption is bounded by supply alone.
    let yield_token = statics
        .get(YIELD_TOKEN)
        .map(|raw| address(raw, YIELD_TOKEN))
        .transpose()?;
    let yield_token_held = yield_token.as_ref().and_then(|token| {
        snapshot
            .state
            .balances
            .get(token)
            .and_then(|balance| u256_from_be(balance, YIELD_TOKEN).ok())
    });

    let mut sy = PendleSyState {
        sy_address,
        total_supply,
        paused,
        token_decimals,
        supply_cap,
        yield_token,
        yield_token_held,
        exchange_rate,
        rate_sampled_at,
        head_timestamp: block.timestamp,
        sy_decimals,
        asset_decimals,
        tokens_in,
        tokens_out,
        holding_backs_supply: false,
        deposit_steps: deposit_steps(state)?,
        entry_fee_bps: attribute_u256(state, ENTRY_FEE_BPS)?
            .unwrap_or_default()
            .saturating_to(),
        entry_fee_from: attribute_u256(state, ENTRY_FEE_FROM)?
            .unwrap_or_default()
            .saturating_to(),
        execution_timestamp: 0,
    };
    sy.judge_holding();
    Ok(sy)
}

/// Reads a rate and the block timestamp it was read at, which the refresh emits together.
///
/// A rate that was never read decodes as zero at [`NEVER_SAMPLED`], a state that refuses to quote
/// until a delta brings the rate in. So does a rate of zero. A rate without its timestamp is an
/// error: dating it by the header instead would claim it was read at a block it was not, which is
/// exactly what the exactness guard checks.
fn rate_reading(
    state: &HashMap<String, Bytes>,
    rate_name: &str,
) -> Result<(U256, u64), InvalidSnapshotError> {
    let Some(rate) = attribute_u256(state, rate_name)? else {
        return Ok((U256::ZERO, NEVER_SAMPLED));
    };
    let rate_sampled_at = attribute_u256(state, ATTR_RATE_SAMPLED_AT)?
        .ok_or_else(|| missing(ATTR_RATE_SAMPLED_AT))?
        .saturating_to::<u64>();
    // A drained or deprecated SY can report a zero rate; one has since block 25,835,592. Nothing
    // can be priced at zero, so it reads as never sampled: the state decodes and refuses to quote
    // rather than failing, which would stop every protocol in the consumer's stream.
    if rate.is_zero() {
        return Ok((rate, NEVER_SAMPLED));
    }
    Ok((rate, rate_sampled_at))
}

/// The entry and exit token classes the state carries.
fn token_classes(state: &HashMap<String, Bytes>) -> Result<TokenClasses, InvalidSnapshotError> {
    let mut tokens_in = HashMap::new();
    let mut tokens_out = HashMap::new();
    for (name, value) in state {
        let Some((direction, token)) =
            class_attribute_token(name).map_err(InvalidSnapshotError::ValueError)?
        else {
            continue;
        };
        if value.as_ref() == NO_CLASS {
            continue;
        }
        let class = TokenClass::parse(value).ok_or_else(|| {
            InvalidSnapshotError::ValueError(format!(
                "{name} carries an unknown token class {:?}",
                String::from_utf8_lossy(value)
            ))
        })?;
        match direction {
            Direction::Deposit => tokens_in.insert(token, class),
            Direction::Redeem => tokens_out.insert(token, class),
        };
    }
    Ok((tokens_in, tokens_out))
}

type TokenClasses = (HashMap<Bytes, TokenClass>, HashMap<Bytes, TokenClass>);

/// The deposit step of each entry token the state names one for.
fn deposit_steps(
    state: &HashMap<String, Bytes>,
) -> Result<HashMap<Bytes, U256>, InvalidSnapshotError> {
    let mut steps = HashMap::new();
    for (name, value) in state {
        let Some(token) = step_attribute_token(name).map_err(InvalidSnapshotError::ValueError)?
        else {
            continue;
        };
        steps.insert(token, u256_from_be(value, name)?);
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use tycho_common::{
        dto::ProtocolStateDelta,
        models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            Chain,
        },
        simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{Balances, ProtocolSim},
        },
    };

    use super::*;
    use crate::evm::protocol::pendle::state::{TOKEN_IN_CLASS_PREFIX, TOKEN_OUT_CLASS_PREFIX};

    const MARKET: &str = "0x34280882267ffa6383b363e278b027be083bbe3b";
    const SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
    const PT: &str = "0xb253eff1104802b97ac7e3ac9fdd73aece295a2c";
    const YT: &str = "0x04b7fa1e727d7290d6e24fa9b426d0c940283a95";
    const WSTETH: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";
    const STETH: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";
    const HEAD: u64 = 1_800_000_000;

    fn word(value: u128) -> Bytes {
        Bytes::from(
            U256::from(value)
                .to_be_bytes::<32>()
                .to_vec(),
        )
    }

    fn address_bytes(value: &str) -> Bytes {
        Bytes::from_str(value).unwrap()
    }

    fn token(value: &str) -> Token {
        Token::new(&address_bytes(value), "TKN", 18, 0, &[Some(10_000)], Chain::Ethereum, 100)
    }

    fn header() -> BlockHeader {
        BlockHeader { timestamp: HEAD, ..Default::default() }
    }

    fn snapshot(
        id: &str,
        protocol_type_name: &str,
        static_attributes: HashMap<String, Bytes>,
        attributes: HashMap<String, Bytes>,
    ) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: id.to_string(),
                attributes,
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: id.to_string(),
                protocol_type_name: protocol_type_name.to_string(),
                static_attributes,
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    /// A market as the indexer emits it on the block the factory created it: statics and fees,
    /// but no reserves and no rate yet.
    fn created_market() -> ComponentWithState {
        snapshot(
            MARKET,
            MARKET_TYPE,
            HashMap::from([
                (ATTR_SCALAR_ROOT.to_string(), word(86_364_560_000_000_000_000)),
                (ATTR_EXPIRY.to_string(), word(1_830_124_800)),
                (ATTR_SY_ADDRESS.to_string(), address_bytes(SY)),
                (ATTR_PT_ADDRESS.to_string(), address_bytes(PT)),
                (ATTR_YT_ADDRESS.to_string(), address_bytes(YT)),
            ]),
            HashMap::from([
                (ATTR_LN_FEE_RATE_ROOT.to_string(), word(499_875_041_000_000)),
                (ATTR_RESERVE_FEE_PERCENT.to_string(), word(80)),
                (SY_PAUSED.to_string(), Bytes::from(vec![0])),
            ]),
        )
    }

    /// An SY as the indexer emits it at creation: its classes, supply and pause flag are state.
    fn created_sy() -> ComponentWithState {
        let mut sy = snapshot(
            SY,
            SY_TYPE,
            HashMap::from([
                (ATTR_SY_DECIMALS.to_string(), word(18)),
                (ATTR_ASSET_DECIMALS.to_string(), word(18)),
            ]),
            HashMap::from([
                (
                    format!("{TOKEN_IN_CLASS_PREFIX}{}", &WSTETH[2..]),
                    Bytes::from(b"one_to_one".to_vec()),
                ),
                (
                    format!("{TOKEN_OUT_CLASS_PREFIX}{}", &WSTETH[2..]),
                    Bytes::from(b"one_to_one".to_vec()),
                ),
                (SY_TOTAL_SUPPLY.to_string(), word(1_000_000_000_000_000_000_000)),
                (PAUSED.to_string(), Bytes::from(vec![0])),
            ]),
        );
        sy.component.tokens = vec![address_bytes(SY), address_bytes(WSTETH)];
        sy
    }

    /// A market cannot tell whether it can trade without its SY's pause flag, which it is
    /// created with.
    #[tokio::test]
    async fn a_market_without_its_sy_pause_flag_is_rejected() {
        let mut market = created_market();
        market
            .state
            .attributes
            .remove(SY_PAUSED);
        assert!(matches!(
            decode(market).await,
            Err(InvalidSnapshotError::MissingAttribute(name)) if name == SY_PAUSED
        ));
    }

    /// The cap, the yield token and what the SY holds of it all decode; an SY without them still
    /// decodes, bounded by supply alone.
    #[tokio::test]
    async fn an_sy_carries_its_cap_yield_token_and_holding() {
        let mut sy = created_sy();
        sy.component
            .static_attributes
            .insert(YIELD_TOKEN.to_string(), address_bytes(WSTETH));
        sy.state
            .attributes
            .insert(SY_SUPPLY_CAP.to_string(), word(5_000));
        sy.state
            .balances
            .insert(address_bytes(WSTETH), Bytes::from(vec![0x01, 0x00]));
        let PendleState::Sy(state) = decode(sy).await.unwrap() else { panic!("not an SY") };
        assert_eq!(state.supply_cap, Some(U256::from(5_000u32)));
        assert_eq!(state.yield_token, Some(address_bytes(WSTETH)));
        assert_eq!(state.yield_token_held, Some(U256::from(256u32)));

        let PendleState::Sy(bare) = decode(created_sy()).await.unwrap() else {
            panic!("not an SY")
        };
        assert_eq!((bare.supply_cap, bare.yield_token, bare.yield_token_held), (None, None, None));
    }

    /// An SY with no class in either direction, such as one whose every probe failed at creation,
    /// decodes and reports no depth, and a later class makes it quotable.
    #[tokio::test]
    async fn an_sy_without_a_class_decodes_and_waits_for_one() {
        let mut sy = created_sy();
        sy.state
            .attributes
            .retain(|name, _| !name.contains("_class_"));
        let mut state = decode(sy).await.unwrap();
        let (max_in, _) = state
            .get_limits(address_bytes(WSTETH), address_bytes(SY))
            .unwrap();
        assert_eq!(max_in, BigUint::ZERO);

        let class = format!("{TOKEN_IN_CLASS_PREFIX}{}", &WSTETH[2..]);
        state
            .delta_transition(
                delta(SY, vec![(&class, Bytes::from(b"one_to_one".to_vec()))]),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        let quote = state
            .get_amount_out(one(), &token(WSTETH), &token(SY))
            .unwrap();
        assert_eq!(quote.amount, one());
    }

    /// The indexer writes a lifted cap as the `type(uint256).max` the SY reports, in 33 bytes of
    /// unsigned big-endian, rather than deleting it: a snapshot and a delta both read it as no cap.
    #[tokio::test]
    async fn a_lifted_cap_is_no_cap() {
        let lifted = Bytes::from([vec![0u8], vec![0xff; 32]].concat());
        let mut sy = created_sy();
        sy.state
            .attributes
            .insert(SY_SUPPLY_CAP.to_string(), lifted.clone());
        let PendleState::Sy(state) = decode(sy).await.unwrap() else { panic!("not an SY") };
        assert_eq!(state.supply_cap, None);

        let mut state = decode(created_sy()).await.unwrap();
        for (cap, expected) in [(word(5_000), Some(U256::from(5_000u32))), (lifted, None)] {
            state
                .delta_transition(
                    delta(SY, vec![(SY_SUPPLY_CAP, cap)]),
                    &HashMap::new(),
                    &Balances::default(),
                )
                .unwrap();
            let PendleState::Sy(sy) = &state else { panic!("not an SY") };
            assert_eq!(sy.supply_cap, expected);
        }
    }

    /// A token that matches neither closed form after an upgrade arrives as `NO_CLASS` instead of
    /// a deletion, and is unquotable in a snapshot and after a delta alike.
    #[tokio::test]
    async fn a_token_whose_class_was_taken_away_is_not_quotable() {
        let no_class = Bytes::from(NO_CLASS.to_vec());
        let mut sy = created_sy();
        sy.state
            .attributes
            .insert(format!("{TOKEN_IN_CLASS_PREFIX}{}", &STETH[2..]), no_class.clone());
        sy.component
            .tokens
            .push(address_bytes(STETH));
        let mut state = decode(sy).await.unwrap();
        let (max_in, _) = state
            .get_limits(address_bytes(STETH), address_bytes(SY))
            .unwrap();
        assert_eq!(max_in, BigUint::ZERO);

        let taken_away = format!("{TOKEN_OUT_CLASS_PREFIX}{}", &WSTETH[2..]);
        state
            .delta_transition(
                delta(SY, vec![(&taken_away, no_class)]),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        let PendleState::Sy(sy) = &state else { panic!("not an SY") };
        assert!(sy
            .tokens_in
            .contains_key(&address_bytes(WSTETH)));
        assert!(!sy
            .tokens_in
            .contains_key(&address_bytes(STETH)));
        assert!(sy.tokens_out.is_empty());
        let (max_in, _) = state
            .get_limits(address_bytes(SY), address_bytes(WSTETH))
            .unwrap();
        assert_eq!(max_in, BigUint::ZERO);
    }

    /// A snapshot balance past 256 bits is malformed: the SY still decodes, with its holding
    /// unknown.
    #[tokio::test]
    async fn a_malformed_snapshot_balance_leaves_the_holding_unknown() {
        let mut sy = created_sy();
        sy.component
            .static_attributes
            .insert(YIELD_TOKEN.to_string(), address_bytes(WSTETH));
        sy.state
            .balances
            .insert(address_bytes(WSTETH), Bytes::from(vec![0x01; 33]));
        let PendleState::Sy(state) = decode(sy).await.unwrap() else { panic!("not an SY") };
        assert_eq!(state.yield_token_held, None);
    }

    /// A reserve is never negative. The indexer's balance aggregation hands back an unsigned
    /// magnitude, and 10e18 in that form, `0x8ac7…`, has its top bit set: it must decode positive,
    /// as the signed form `0x008ac7…` does, in a snapshot and in a delta.
    #[tokio::test]
    async fn a_reserve_with_its_top_bit_set_decodes_positive() {
        let ten = U256::from(10_000_000_000_000_000_000u64);
        for raw in ["8ac7230489e80000", "008ac7230489e80000"] {
            let bytes = Bytes::from(hex::decode(raw).unwrap());
            let mut market = created_market();
            market
                .state
                .attributes
                .insert(ATTR_TOTAL_SY.to_string(), bytes.clone());
            let PendleState::Market(decoded) = decode(market).await.unwrap() else {
                panic!("not a market")
            };
            assert_eq!(decoded.market.total_sy, I256::try_from(ten).unwrap(), "{raw}");

            let mut state = decode(created_market()).await.unwrap();
            state
                .delta_transition(
                    delta(MARKET, vec![(ATTR_TOTAL_PT, bytes)]),
                    &HashMap::new(),
                    &Balances::default(),
                )
                .unwrap();
            let PendleState::Market(moved) = state else { panic!("not a market") };
            assert_eq!(moved.market.total_pt, I256::try_from(ten).unwrap(), "{raw}");
        }
    }

    /// Supply bounds every redemption, so an SY without it cannot be decoded.
    #[tokio::test]
    async fn an_sy_without_its_supply_is_rejected() {
        let mut sy = created_sy();
        sy.state
            .attributes
            .remove(SY_TOTAL_SUPPLY);
        assert!(matches!(
            decode(sy).await,
            Err(InvalidSnapshotError::MissingAttribute(name)) if name == SY_TOTAL_SUPPLY
        ));
    }

    async fn decode(snapshot: ComponentWithState) -> Result<PendleState, InvalidSnapshotError> {
        let tokens = HashMap::from([(address_bytes(WSTETH), token(WSTETH))]);
        PendleState::try_from_with_header(
            snapshot,
            header(),
            &HashMap::new(),
            &tokens,
            &DecoderContext::default(),
        )
        .await
    }

    fn delta(id: &str, attributes: Vec<(&str, Bytes)>) -> ProtocolStateDelta {
        ProtocolStateDelta {
            component_id: id.to_string(),
            updated_attributes: attributes
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
            deleted_attributes: Default::default(),
        }
    }

    fn one() -> BigUint {
        BigUint::from(10u64.pow(18))
    }

    #[tokio::test]
    async fn a_market_created_this_block_decodes_and_waits_for_its_rate() {
        let state = decode(created_market()).await.unwrap();

        let (max_in, max_out) = state
            .get_limits(address_bytes(SY), address_bytes(PT))
            .unwrap();
        assert_eq!((max_in, max_out), (BigUint::ZERO, BigUint::ZERO));
        assert!(matches!(
            state.get_amount_out(one(), &token(SY), &token(PT)),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    /// The rate arrives before the first liquidity does. The market is current but empty.
    #[tokio::test]
    async fn a_market_with_a_rate_but_no_liquidity_reports_no_depth() {
        let mut state = decode(created_market()).await.unwrap();
        state
            .delta_transition(
                delta(
                    MARKET,
                    vec![
                        (ATTR_PY_INDEX_CURRENT, word(1_241_884_000_000_000_000)),
                        (ATTR_RATE_SAMPLED_AT, word(HEAD.into())),
                        ("block_timestamp", word(HEAD.into())),
                    ],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        let (max_in, max_out) = state
            .get_limits(address_bytes(SY), address_bytes(PT))
            .unwrap();
        assert_eq!((max_in, max_out), (BigUint::ZERO, BigUint::ZERO));
        assert!(matches!(
            state.get_amount_out(one(), &token(SY), &token(PT)),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    #[tokio::test]
    async fn a_created_market_quotes_once_liquidity_and_a_rate_arrive() {
        let mut state = decode(created_market()).await.unwrap();
        state
            .delta_transition(
                delta(
                    MARKET,
                    vec![
                        (ATTR_TOTAL_PT, word(83_658_000_000_000_000_000)),
                        (ATTR_TOTAL_SY, word(1_429_719_000_000_000_000_000)),
                        (ATTR_LAST_LN_IMPLIED_RATE, word(20_211_000_000_000_000)),
                        (ATTR_PY_INDEX_CURRENT, word(1_241_884_000_000_000_000)),
                        (ATTR_RATE_SAMPLED_AT, word(HEAD.into())),
                        ("block_timestamp", word(HEAD.into())),
                    ],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        let (max_in, _) = state
            .get_limits(address_bytes(SY), address_bytes(PT))
            .unwrap();
        assert!(max_in > BigUint::ZERO);
        let quote = state
            .get_amount_out(one(), &token(SY), &token(PT))
            .unwrap();
        assert!(quote.amount > BigUint::ZERO);
    }

    /// The indexer reads a market's fees at creation and can fail to. Such a market decodes, has
    /// no depth and refuses to quote, instead of failing the decode, until a fee update arrives.
    #[tokio::test]
    async fn a_market_without_its_fees_waits_for_them() {
        let mut market = created_market();
        for name in [ATTR_LN_FEE_RATE_ROOT, ATTR_RESERVE_FEE_PERCENT] {
            market.state.attributes.remove(name);
        }
        let mut state = decode(market).await.unwrap();
        let live = delta(
            MARKET,
            vec![
                (ATTR_TOTAL_PT, word(83_658_000_000_000_000_000)),
                (ATTR_TOTAL_SY, word(1_429_719_000_000_000_000_000)),
                (ATTR_LAST_LN_IMPLIED_RATE, word(20_211_000_000_000_000)),
                (ATTR_PY_INDEX_CURRENT, word(1_241_884_000_000_000_000)),
                (ATTR_RATE_SAMPLED_AT, word(HEAD.into())),
                ("block_timestamp", word(HEAD.into())),
            ],
        );
        state
            .delta_transition(live, &HashMap::new(), &Balances::default())
            .unwrap();
        let (max_in, _) = state
            .get_limits(address_bytes(SY), address_bytes(PT))
            .unwrap();
        assert_eq!(max_in, BigUint::ZERO);
        assert!(matches!(
            state.get_amount_out(one(), &token(SY), &token(PT)),
            Err(SimulationError::RecoverableError(_))
        ));

        let fees = delta(
            MARKET,
            vec![
                (ATTR_LN_FEE_RATE_ROOT, word(499_875_041_000_000)),
                (ATTR_RESERVE_FEE_PERCENT, word(80)),
            ],
        );
        state
            .delta_transition(fees, &HashMap::new(), &Balances::default())
            .unwrap();
        assert!(state
            .get_amount_out(one(), &token(SY), &token(PT))
            .is_ok());
    }

    #[tokio::test]
    async fn an_sy_whose_rate_was_never_read_quotes_only_its_one_to_one_edges() {
        let mut sy = created_sy();
        sy.state.attributes.insert(
            format!("{TOKEN_IN_CLASS_PREFIX}{}", &STETH[2..]),
            Bytes::from(b"index_rate".to_vec()),
        );
        sy.component
            .tokens
            .push(address_bytes(STETH));
        let mut state = decode(sy).await.unwrap();

        // A `one_to_one` wrap is a decimal rescale and never reads the rate.
        let quote = state
            .get_amount_out(one(), &token(WSTETH), &token(SY))
            .unwrap();
        assert_eq!(quote.amount, one(), "wstETH wraps one to one");

        let (max_in, _) = state
            .get_limits(address_bytes(STETH), address_bytes(SY))
            .unwrap();
        assert_eq!(max_in, BigUint::ZERO);
        assert!(matches!(
            state.get_amount_out(one(), &token(STETH), &token(SY)),
            Err(SimulationError::RecoverableError(_))
        ));

        state
            .delta_transition(
                delta(
                    SY,
                    vec![
                        (ATTR_SY_EXCHANGE_RATE, word(1_241_884_000_000_000_000)),
                        (ATTR_RATE_SAMPLED_AT, word(HEAD.into())),
                        ("block_timestamp", word(HEAD.into())),
                    ],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        assert!(state
            .get_amount_out(one(), &token(STETH), &token(SY))
            .is_ok());
    }

    /// The refresh emits the two together, so one without the other is a broken stream rather
    /// than a rate that has not been read yet.
    #[tokio::test]
    async fn a_rate_without_its_timestamp_is_rejected() {
        let mut market = created_market();
        market
            .state
            .attributes
            .insert(ATTR_PY_INDEX_CURRENT.to_string(), word(1_241_884_000_000_000_000));

        let error = decode(market).await.unwrap_err();

        assert!(
            matches!(&error, InvalidSnapshotError::MissingAttribute(name) if name == ATTR_RATE_SAMPLED_AT),
            "{error:?}"
        );
    }

    /// Suffixes a class attribute name can carry that do not name a 20-byte address.
    const NOT_AN_ADDRESS: [&str; 3] = ["zz", "7f7f", "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f"];

    /// A class attribute that names no address fails a snapshot and a delta alike, as an update
    /// or a deletion. Skipping it on either path would leave a class the indexer meant to set or
    /// delete unapplied.
    #[tokio::test]
    async fn a_class_attribute_that_names_no_address_is_rejected() {
        for suffix in NOT_AN_ADDRESS {
            let name = format!("{TOKEN_IN_CLASS_PREFIX}{suffix}");

            let mut sy = created_sy();
            sy.state
                .attributes
                .insert(name.clone(), Bytes::from(b"one_to_one".to_vec()));
            let error = decode(sy).await.unwrap_err();
            assert!(matches!(error, InvalidSnapshotError::ValueError(_)), "{suffix}: {error:?}");

            let mut state = decode(created_sy()).await.unwrap();
            let update = delta(SY, vec![(&name, Bytes::from(b"one_to_one".to_vec()))]);
            let error = state
                .delta_transition(update, &HashMap::new(), &Balances::default())
                .unwrap_err();
            assert!(matches!(error, TransitionError::DecodeError(_)), "{suffix}: {error:?}");

            let mut deletion = delta(SY, vec![]);
            deletion.deleted_attributes = [name.clone()].into();
            let error = state
                .delta_transition(deletion, &HashMap::new(), &Balances::default())
                .unwrap_err();
            assert!(matches!(error, TransitionError::DecodeError(_)), "{suffix}: {error:?}");
        }
    }

    /// A signed value never needs more than 32 bytes in minimal two's complement, so a wider one
    /// is malformed and rejected rather than truncated to its low bytes.
    #[tokio::test]
    async fn a_signed_value_past_256_bits_is_rejected() {
        let too_wide = Bytes::from([vec![0x01], vec![0u8; 32]].concat());

        let mut market = created_market();
        market
            .state
            .attributes
            .insert(ATTR_TOTAL_PT.to_string(), too_wide.clone());
        let error = decode(market).await.unwrap_err();
        assert!(matches!(error, InvalidSnapshotError::ValueError(_)), "{error:?}");

        let mut state = decode(created_market()).await.unwrap();
        let error = state
            .delta_transition(
                delta(MARKET, vec![(ATTR_TOTAL_SY, too_wide)]),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap_err();
        assert!(matches!(error, TransitionError::DecodeError(_)), "{error:?}");
    }

    /// A drained SY can report a zero rate; one has since block 25,835,592. It decodes as never
    /// sampled, so it refuses to quote, rather than failing the decode, which by default stops
    /// every protocol in the consumer's stream.
    #[tokio::test]
    async fn a_read_exchange_rate_of_zero_decodes_as_never_sampled() {
        let mut sy = created_sy();
        sy.state
            .attributes
            .insert(ATTR_SY_EXCHANGE_RATE.to_string(), word(0));
        sy.state
            .attributes
            .insert(ATTR_RATE_SAMPLED_AT.to_string(), word(HEAD.into()));

        let PendleState::Sy(state) = decode(sy)
            .await
            .expect("a zero rate decodes")
        else {
            panic!("an SY decodes as an SY");
        };
        assert_eq!(state.rate_sampled_at, NEVER_SAMPLED);
        assert!(!state.is_current());
    }
}
