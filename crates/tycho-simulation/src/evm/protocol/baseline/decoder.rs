use std::{collections::HashMap, str::FromStr};

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::{BaselineCurve, BaselineQuoteState, BaselineState};
use crate::{
    evm::protocol::u256_num::bytes_to_u256,
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

const RELAY_ATTRIBUTE: &str = "relay";
const RESERVE_ATTRIBUTE: &str = "reserve";

const SNAPSHOT_CURVE_BLV: &str = "snapshot_curve_blv";
const SNAPSHOT_CURVE_CIRC: &str = "snapshot_curve_circ";
const SNAPSHOT_CURVE_SUPPLY: &str = "snapshot_curve_supply";
const SNAPSHOT_CURVE_SWAP_FEE: &str = "snapshot_curve_swap_fee";
const SNAPSHOT_CURVE_RESERVES: &str = "snapshot_curve_reserves";
const SNAPSHOT_CURVE_TOTAL_SUPPLY: &str = "snapshot_curve_total_supply";
const SNAPSHOT_CURVE_CONVEXITY_EXP: &str = "snapshot_curve_convexity_exp";
const SNAPSHOT_CURVE_LAST_INVARIANT: &str = "snapshot_curve_last_invariant";
const QUOTE_BLOCK_BUY_DELTA_CIRC: &str = "quote_block_buy_delta_circ";
const QUOTE_BLOCK_SELL_DELTA_CIRC: &str = "quote_block_sell_delta_circ";
const TOTAL_SUPPLY: &str = "total_supply";
const TOTAL_B_TOKENS: &str = "total_b_tokens";
const TOTAL_RESERVES: &str = "total_reserves";
const RESERVE_DECIMALS: &str = "reserve_decimals";
const LIQUIDITY_FEE_PCT: &str = "liquidity_fee_pct";
const PENDING_SURPLUS: &str = "pending_surplus";
const SHOULD_SETTLE_PENDING_SURPLUS: &str = "should_settle_pending_surplus";
const MAX_SELL_DELTA: &str = "max_sell_delta";
const SNAPSHOT_ACTIVE_PRICE: &str = "snapshot_active_price";

#[cfg(test)]
const REQUIRED_QUOTE_STATE_ATTRIBUTES: &[&str] = &[
    SNAPSHOT_CURVE_BLV,
    SNAPSHOT_CURVE_CIRC,
    SNAPSHOT_CURVE_SUPPLY,
    SNAPSHOT_CURVE_SWAP_FEE,
    SNAPSHOT_CURVE_RESERVES,
    SNAPSHOT_CURVE_TOTAL_SUPPLY,
    SNAPSHOT_CURVE_CONVEXITY_EXP,
    SNAPSHOT_CURVE_LAST_INVARIANT,
    QUOTE_BLOCK_BUY_DELTA_CIRC,
    QUOTE_BLOCK_SELL_DELTA_CIRC,
    TOTAL_SUPPLY,
    TOTAL_B_TOKENS,
    TOTAL_RESERVES,
    RESERVE_DECIMALS,
    LIQUIDITY_FEE_PCT,
    PENDING_SURPLUS,
    SHOULD_SETTLE_PENDING_SURPLUS,
    MAX_SELL_DELTA,
    SNAPSHOT_ACTIVE_PRICE,
];

impl TryFromWithBlock<ComponentWithState, BlockHeader> for BaselineState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let component_id = Bytes::from_str(snapshot.component.id.as_str()).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "Expected Baseline component id to be bToken address: {e}"
            ))
        })?;

        if snapshot.state.component_id != snapshot.component.id {
            return Err(InvalidSnapshotError::ValueError(format!(
                "Baseline component id mismatch: state={} component={}",
                snapshot.state.component_id, snapshot.component.id
            )));
        }

        let relay = get_static_attr(&snapshot, RELAY_ATTRIBUTE)?.clone();
        let reserve_address = get_static_attr(&snapshot, RESERVE_ATTRIBUTE)?.clone();
        let b_token = all_tokens
            .get(&component_id)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Missing Baseline bToken metadata: {component_id}"
                ))
            })?
            .clone();
        let reserve = all_tokens
            .get(&reserve_address)
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!(
                    "Missing Baseline reserve token metadata: {reserve_address}"
                ))
            })?
            .clone();

        if !snapshot
            .component
            .tokens
            .contains(&component_id)
        {
            return Err(InvalidSnapshotError::ValueError(format!(
                "Baseline component tokens missing bToken {component_id}"
            )));
        }

        if !snapshot
            .component
            .tokens
            .contains(&reserve_address)
        {
            return Err(InvalidSnapshotError::ValueError(format!(
                "Baseline component tokens missing reserve {reserve_address}"
            )));
        }

        let quote_state = decode_quote_state(&snapshot.state.attributes)?;
        Ok(BaselineState::new(component_id, relay, b_token, reserve, quote_state))
    }
}

pub(super) fn decode_quote_state(
    attributes: &HashMap<String, Bytes>,
) -> Result<BaselineQuoteState, InvalidSnapshotError> {
    Ok(BaselineQuoteState {
        snapshot_curve: BaselineCurve {
            blv: get_u256(attributes, SNAPSHOT_CURVE_BLV)?,
            circ: get_u256(attributes, SNAPSHOT_CURVE_CIRC)?,
            supply: get_u256(attributes, SNAPSHOT_CURVE_SUPPLY)?,
            swap_fee: get_u256(attributes, SNAPSHOT_CURVE_SWAP_FEE)?,
            reserves: get_u256(attributes, SNAPSHOT_CURVE_RESERVES)?,
            total_supply: get_u256(attributes, SNAPSHOT_CURVE_TOTAL_SUPPLY)?,
            convexity_exp: get_u256(attributes, SNAPSHOT_CURVE_CONVEXITY_EXP)?,
            last_invariant: get_u256(attributes, SNAPSHOT_CURVE_LAST_INVARIANT)?,
        },
        quote_block_buy_delta_circ: get_u256(attributes, QUOTE_BLOCK_BUY_DELTA_CIRC)?,
        quote_block_sell_delta_circ: get_u256(attributes, QUOTE_BLOCK_SELL_DELTA_CIRC)?,
        total_supply: get_u256(attributes, TOTAL_SUPPLY)?,
        total_b_tokens: get_u256(attributes, TOTAL_B_TOKENS)?,
        total_reserves: get_u256(attributes, TOTAL_RESERVES)?,
        reserve_decimals: get_u256(attributes, RESERVE_DECIMALS)?,
        liquidity_fee_pct: get_u256(attributes, LIQUIDITY_FEE_PCT)?,
        pending_surplus: get_u256(attributes, PENDING_SURPLUS)?,
        should_settle_pending_surplus: get_bool(attributes, SHOULD_SETTLE_PENDING_SURPLUS)?,
        max_sell_delta: get_u256(attributes, MAX_SELL_DELTA)?,
        snapshot_active_price: get_u256(attributes, SNAPSHOT_ACTIVE_PRICE)?,
    })
}

fn get_static_attr<'a>(
    snapshot: &'a ComponentWithState,
    name: &str,
) -> Result<&'a Bytes, InvalidSnapshotError> {
    snapshot
        .component
        .static_attributes
        .get(name)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_owned()))
}

fn get_u256(attributes: &HashMap<String, Bytes>, name: &str) -> Result<U256, InvalidSnapshotError> {
    let bytes = attributes
        .get(name)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_owned()))?;
    decode_u256_attr(name, bytes)
}

fn get_bool(attributes: &HashMap<String, Bytes>, name: &str) -> Result<bool, InvalidSnapshotError> {
    let bytes = attributes
        .get(name)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(name.to_owned()))?;

    match bytes.as_ref() {
        [0] => Ok(false),
        [1] => Ok(true),
        value => Err(InvalidSnapshotError::ValueError(format!(
            "Invalid Baseline bool attribute {name}: expected 0 or 1, got 0x{}",
            hex::encode(value)
        ))),
    }
}

fn decode_u256_attr(name: &str, bytes: &Bytes) -> Result<U256, InvalidSnapshotError> {
    let value = bytes.as_ref();
    if value.is_empty() {
        return Err(InvalidSnapshotError::ValueError(format!(
            "Invalid Baseline uint attribute {name}: empty value"
        )));
    }

    if value.len() > 33 || (value.len() == 33 && value[0] != 0) {
        return Err(InvalidSnapshotError::ValueError(format!(
            "Invalid Baseline uint attribute {name}: expected unsigned integer bytes, got 0x{}",
            hex::encode(value)
        )));
    }

    let value = if value.len() == 33 { &value[1..] } else { value };
    Ok(bytes_to_u256(Bytes::from(value.to_vec()).into()))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr};

    use alloy::primitives::U256;
    use rstest::rstest;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::{
        dto::ProtocolStateDelta,
        models::{
            protocol::{ProtocolComponent, ProtocolComponentState},
            token::Token,
            Chain, ChangeType,
        },
        simulation::protocol_sim::{Balances, ProtocolSim},
        Bytes,
    };

    use super::{
        BaselineState, InvalidSnapshotError, LIQUIDITY_FEE_PCT, RELAY_ATTRIBUTE,
        REQUIRED_QUOTE_STATE_ATTRIBUTES, RESERVE_ATTRIBUTE, SHOULD_SETTLE_PENDING_SURPLUS,
        SNAPSHOT_CURVE_BLV,
    };
    use crate::protocol::models::TryFromWithBlock;

    const BTOKEN: &str = "0x9fdbde76236998dc2836fe67a9954ede456a1d63";
    const RESERVE: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const RELAY: &str = "0xc81fd894c0ace037d133af4886550ac8133568e8";

    fn create_test_snapshot() -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: BTOKEN.to_owned(),
                attributes: quote_state_attributes(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent::new(
                BTOKEN,
                "baseline",
                "baseline",
                Chain::Ethereum,
                vec![hex_bytes(BTOKEN), hex_bytes(RESERVE)],
                vec![hex_bytes(RELAY)],
                HashMap::from([
                    (RELAY_ATTRIBUTE.to_owned(), hex_bytes(RELAY)),
                    (RESERVE_ATTRIBUTE.to_owned(), hex_bytes(RESERVE)),
                ]),
                ChangeType::Creation,
                Bytes::default(),
                Default::default(),
            ),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn quote_state_attributes() -> HashMap<String, Bytes> {
        HashMap::from([
            (SNAPSHOT_CURVE_BLV.to_owned(), u256_attr(1)),
            ("snapshot_curve_circ".to_owned(), u256_attr(2)),
            ("snapshot_curve_supply".to_owned(), u256_attr(3)),
            ("snapshot_curve_swap_fee".to_owned(), u256_attr(4)),
            ("snapshot_curve_reserves".to_owned(), u256_attr(5)),
            ("snapshot_curve_total_supply".to_owned(), u256_attr(6)),
            ("snapshot_curve_convexity_exp".to_owned(), u256_attr(7)),
            ("snapshot_curve_last_invariant".to_owned(), u256_attr(8)),
            ("quote_block_buy_delta_circ".to_owned(), u256_attr(9)),
            ("quote_block_sell_delta_circ".to_owned(), u256_attr(10)),
            ("total_supply".to_owned(), u256_attr(11)),
            ("total_b_tokens".to_owned(), u256_attr(12)),
            ("total_reserves".to_owned(), u256_attr(13)),
            ("reserve_decimals".to_owned(), u256_attr(18)),
            (LIQUIDITY_FEE_PCT.to_owned(), u256_attr(15)),
            ("pending_surplus".to_owned(), u256_attr(16)),
            (SHOULD_SETTLE_PENDING_SURPLUS.to_owned(), Bytes::from(vec![1])),
            ("max_sell_delta".to_owned(), u256_attr(17)),
            ("snapshot_active_price".to_owned(), u256_attr(18)),
        ])
    }

    fn u256_attr(value: u64) -> Bytes {
        Bytes::from(U256::from(value).to_be_bytes_vec())
    }

    fn hex_bytes(value: &str) -> Bytes {
        Bytes::from_str(value).unwrap()
    }

    fn all_tokens() -> HashMap<Bytes, Token> {
        HashMap::from([
            (
                hex_bytes(BTOKEN),
                Token::new(
                    &hex_bytes(BTOKEN),
                    "bToken",
                    18,
                    0,
                    &[Some(100_000)],
                    Chain::Ethereum,
                    100,
                ),
            ),
            (
                hex_bytes(RESERVE),
                Token::new(
                    &hex_bytes(RESERVE),
                    "WETH",
                    18,
                    0,
                    &[Some(100_000)],
                    Chain::Ethereum,
                    100,
                ),
            ),
        ])
    }

    async fn decode(snapshot: ComponentWithState) -> Result<BaselineState, InvalidSnapshotError> {
        BaselineState::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            &all_tokens(),
            &Default::default(),
        )
        .await
    }

    #[tokio::test]
    async fn decodes_baseline_quote_state() {
        let snapshot = create_test_snapshot();
        let result = decode(snapshot).await;

        assert!(result.is_ok());
        let state = result.unwrap();
        assert_eq!(state.component_id, hex_bytes(BTOKEN));
        assert_eq!(state.relay, hex_bytes(RELAY));
        assert_eq!(state.reserve.address, hex_bytes(RESERVE));
        assert_eq!(state.quote_state.snapshot_curve.blv, U256::from(1));
        assert_eq!(state.quote_state.liquidity_fee_pct, U256::from(15));
        assert!(
            state
                .quote_state
                .should_settle_pending_surplus
        );
    }

    #[tokio::test]
    #[rstest]
    #[case::relay(RELAY_ATTRIBUTE)]
    #[case::reserve(RESERVE_ATTRIBUTE)]
    async fn rejects_missing_static_attribute(#[case] missing_attribute: &str) {
        let mut snapshot = create_test_snapshot();
        snapshot
            .component
            .static_attributes
            .remove(missing_attribute);

        let result = decode(snapshot).await;

        assert!(matches!(
            result.unwrap_err(),
            InvalidSnapshotError::MissingAttribute(ref name) if name == missing_attribute
        ));
    }

    #[tokio::test]
    async fn rejects_mismatched_component_id() {
        let mut snapshot = create_test_snapshot();
        snapshot.state.component_id = RESERVE.to_owned();

        let result = decode(snapshot).await;

        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn rejects_invalid_component_id() {
        let mut snapshot = create_test_snapshot();
        snapshot.component.id = "not-an-address".to_owned();

        let result = decode(snapshot).await;

        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn rejects_missing_btoken_metadata() {
        let snapshot = create_test_snapshot();
        let mut tokens = all_tokens();
        tokens.remove(&hex_bytes(BTOKEN));

        let result = BaselineState::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            &tokens,
            &Default::default(),
        )
        .await;

        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn rejects_missing_reserve_from_component_tokens() {
        let mut snapshot = create_test_snapshot();
        snapshot.component.tokens = vec![hex_bytes(BTOKEN)];

        let result = decode(snapshot).await;

        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn rejects_malformed_u256_attribute() {
        let mut snapshot = create_test_snapshot();
        snapshot
            .state
            .attributes
            .insert(SNAPSHOT_CURVE_BLV.to_owned(), Bytes::from(vec![1; 33]));

        let result = decode(snapshot).await;

        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn decodes_positive_u256_with_sign_padding() {
        let mut snapshot = create_test_snapshot();
        let mut padded = vec![0];
        padded.extend([0xff; 32]);
        snapshot
            .state
            .attributes
            .insert(SNAPSHOT_CURVE_BLV.to_owned(), Bytes::from(padded));

        let result = decode(snapshot).await;

        assert!(result.is_ok());
        assert_eq!(
            result
                .unwrap()
                .quote_state
                .snapshot_curve
                .blv,
            U256::MAX
        );
    }

    #[tokio::test]
    async fn rejects_malformed_bool_attribute() {
        let mut snapshot = create_test_snapshot();
        snapshot
            .state
            .attributes
            .insert(SHOULD_SETTLE_PENDING_SURPLUS.to_owned(), Bytes::from(vec![2]));

        let result = decode(snapshot).await;

        assert!(matches!(result.unwrap_err(), InvalidSnapshotError::ValueError(_)));
    }

    #[tokio::test]
    async fn applies_full_quote_state_delta_transition() {
        let mut state = decode(create_test_snapshot())
            .await
            .unwrap();
        let mut updated_attributes = quote_state_attributes();
        updated_attributes.insert(SNAPSHOT_CURVE_BLV.to_owned(), u256_attr(42));

        let result = state.delta_transition(
            ProtocolStateDelta {
                component_id: BTOKEN.to_owned(),
                updated_attributes,
                deleted_attributes: Default::default(),
            },
            &HashMap::default(),
            &Balances::default(),
        );

        assert!(result.is_ok());
        assert_eq!(state.quote_state.snapshot_curve.blv, U256::from(42));
    }

    #[tokio::test]
    #[rstest]
    #[case::snapshot_curve_blv(REQUIRED_QUOTE_STATE_ATTRIBUTES[0])]
    #[case::snapshot_curve_circ(REQUIRED_QUOTE_STATE_ATTRIBUTES[1])]
    #[case::snapshot_curve_supply(REQUIRED_QUOTE_STATE_ATTRIBUTES[2])]
    #[case::snapshot_curve_swap_fee(REQUIRED_QUOTE_STATE_ATTRIBUTES[3])]
    #[case::snapshot_curve_reserves(REQUIRED_QUOTE_STATE_ATTRIBUTES[4])]
    #[case::snapshot_curve_total_supply(REQUIRED_QUOTE_STATE_ATTRIBUTES[5])]
    #[case::snapshot_curve_convexity_exp(REQUIRED_QUOTE_STATE_ATTRIBUTES[6])]
    #[case::snapshot_curve_last_invariant(REQUIRED_QUOTE_STATE_ATTRIBUTES[7])]
    #[case::quote_block_buy_delta_circ(REQUIRED_QUOTE_STATE_ATTRIBUTES[8])]
    #[case::quote_block_sell_delta_circ(REQUIRED_QUOTE_STATE_ATTRIBUTES[9])]
    #[case::total_supply(REQUIRED_QUOTE_STATE_ATTRIBUTES[10])]
    #[case::total_b_tokens(REQUIRED_QUOTE_STATE_ATTRIBUTES[11])]
    #[case::total_reserves(REQUIRED_QUOTE_STATE_ATTRIBUTES[12])]
    #[case::reserve_decimals(REQUIRED_QUOTE_STATE_ATTRIBUTES[13])]
    #[case::liquidity_fee_pct(REQUIRED_QUOTE_STATE_ATTRIBUTES[14])]
    #[case::pending_surplus(REQUIRED_QUOTE_STATE_ATTRIBUTES[15])]
    #[case::should_settle_pending_surplus(REQUIRED_QUOTE_STATE_ATTRIBUTES[16])]
    #[case::max_sell_delta(REQUIRED_QUOTE_STATE_ATTRIBUTES[17])]
    #[case::snapshot_active_price(REQUIRED_QUOTE_STATE_ATTRIBUTES[18])]
    async fn rejects_missing_quote_state_attribute(#[case] missing_attribute: &str) {
        let mut snapshot = create_test_snapshot();
        snapshot
            .state
            .attributes
            .remove(missing_attribute);

        let result = decode(snapshot).await;

        assert!(matches!(
            result.unwrap_err(),
            InvalidSnapshotError::MissingAttribute(ref name) if name == missing_attribute
        ));
    }
}
