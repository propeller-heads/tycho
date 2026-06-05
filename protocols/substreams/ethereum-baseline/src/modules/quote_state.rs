use crate::abi::b_controller::events::{CreatorFeePctSet, DeployerSet, LiquidityFeePctSet};
use ethabi::{ParamType, Token};
use substreams::scalar::BigInt;
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::models::{Attribute, ChangeType};

const GET_QUOTE_STATE_SELECTOR: [u8; 4] = [0x97, 0xa6, 0x11, 0xc1];
const SNAPSHOT_CURVE_FIELDS: [&str; 8] = [
    "snapshot_curve_blv",
    "snapshot_curve_circ",
    "snapshot_curve_supply",
    "snapshot_curve_swap_fee",
    "snapshot_curve_reserves",
    "snapshot_curve_total_supply",
    "snapshot_curve_convexity_exp",
    "snapshot_curve_last_invariant",
];
const QUOTE_STATE_FIELDS: [&str; 11] = [
    "quote_block_buy_delta_circ",
    "quote_block_sell_delta_circ",
    "total_supply",
    "total_b_tokens",
    "total_reserves",
    "reserve_decimals",
    "liquidity_fee_pct",
    "pending_surplus",
    "should_settle_pending_surplus",
    "max_sell_delta",
    "snapshot_active_price",
];

pub(crate) fn maybe_update_component_id(log: &eth::v2::Log) -> Option<String> {
    CreatorFeePctSet::match_and_decode(log)
        .map(|event| event.b_token)
        .or_else(|| LiquidityFeePctSet::match_and_decode(log).map(|event| event.b_token))
        .or_else(|| DeployerSet::match_and_decode(log).map(|event| event.b_token))
        .map(|b_token| format!("0x{}", hex::encode(b_token)))
}

pub(crate) fn attributes_for_component(
    relay_address: &[u8],
    component_id: &str,
    change: ChangeType,
) -> Vec<Attribute> {
    component_id
        .strip_prefix("0x")
        .and_then(|hex| hex::decode(hex).ok())
        .filter(|b_token| b_token.len() == 20)
        .and_then(|b_token| call_quote_state(relay_address, &b_token))
        .map(|tokens| attributes_from_tokens(tokens, change))
        .unwrap_or_default()
}

fn call_quote_state(relay_address: &[u8], b_token: &[u8]) -> Option<Vec<Token>> {
    use substreams_ethereum::pb::eth::rpc;

    let rpc_calls = rpc::RpcCalls {
        calls: vec![rpc::RpcCall {
            to_addr: relay_address.to_vec(),
            data: get_quote_state_calldata(b_token),
        }],
    };
    let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
    let response = responses.first()?;
    if response.failed {
        substreams::log::debug!(
            "Baseline getQuoteState eth_call failed for bToken 0x{}",
            hex::encode(b_token)
        );
        return None;
    }

    decode_quote_state(response.raw.as_ref()).or_else(|| {
        substreams::log::info!(
            "Baseline getQuoteState response failed to decode for bToken 0x{}",
            hex::encode(b_token)
        );
        None
    })
}

fn get_quote_state_calldata(b_token: &[u8]) -> Vec<u8> {
    let mut calldata = Vec::with_capacity(36);
    calldata.extend(GET_QUOTE_STATE_SELECTOR);
    calldata.extend(ethabi::encode(&[Token::Address(ethabi::Address::from_slice(b_token))]));
    calldata
}

fn decode_quote_state(data: &[u8]) -> Option<Vec<Token>> {
    let mut decoded = ethabi::decode(&[quote_state_param_type()], data).ok()?;
    decoded.pop()?.into_tuple()
}

fn quote_state_param_type() -> ParamType {
    ParamType::Tuple(vec![
        ParamType::Tuple(vec![
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Uint(256),
        ]),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(8),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Bool,
        ParamType::Uint(256),
        ParamType::Uint(256),
    ])
}

fn attributes_from_tokens(tokens: Vec<Token>, change: ChangeType) -> Vec<Attribute> {
    let mut attributes = Vec::with_capacity(SNAPSHOT_CURVE_FIELDS.len() + QUOTE_STATE_FIELDS.len());
    let Some(Token::Tuple(snapshot_curve)) = tokens.first() else {
        return attributes;
    };

    attributes.extend(
        SNAPSHOT_CURVE_FIELDS
            .iter()
            .zip(snapshot_curve.iter())
            .filter_map(|(name, token)| uint_attribute(name, token, change)),
    );
    attributes.extend(
        QUOTE_STATE_FIELDS
            .iter()
            .zip(tokens.iter().skip(1))
            .filter_map(|(name, token)| match token {
                Token::Bool(value) => Some(Attribute {
                    name: (*name).to_string(),
                    value: vec![u8::from(*value)],
                    change: change.into(),
                }),
                _ => uint_attribute(name, token, change),
            }),
    );

    attributes
}

fn uint_attribute(name: &str, token: &Token, change: ChangeType) -> Option<Attribute> {
    let mut value = [0u8; 32];
    token
        .clone()
        .into_uint()?
        .to_big_endian(value.as_mut_slice());

    Some(Attribute {
        name: name.to_string(),
        value: BigInt::from_unsigned_bytes_be(&value).to_signed_bytes_be(),
        change: change.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTOKEN: &str = "9fdbde76236998dc2836fe67a9954ede456a1d63";
    const QUOTE_STATE_RETURN: &str = "\
        00000000000000000000000000000000000000000000000000045fe2077d45cc\
        00000000000000000000000000000000000000000009b4025b3ce9cc0ce068d8\
        00000000000000000000000000000000000000000007aae9ecb9e5b2281f9728\
        000000000000000000000000000000000000000000000000002386f26fc10000\
        0000000000000000000000000000000000000000031038c47b0fdcb0d1fc0000\
        000000000000000000000000000000000000000000115eec47f6cf7e35000000\
        000000000000000000000000000000000000000001ff92b9e15ad389e7000000\
        000000000000000000000000000000000000000000000000000d8addff4411bc\
        0000000000000000000000000000000000000000000000000000000000000000\
        0000000000000000000000000000000000000000000000000000000000000000\
        000000000000000000000000000000000000000000115eec47f6cf7e35000000\
        00000000000000000000000000000000000000000007aae9ecb9e5b2281f9728\
        000000000000000000000000000000000000000000031038c46ba46727c2b400\
        0000000000000000000000000000000000000000000000000000000000000012\
        00000000000000000000000000000000000000000000000006f05b59d3b20000\
        00000000000000000000000000000000000000000000000000000f6b75890f48\
        0000000000000000000000000000000000000000000000000000000000000001\
        00000000000000000000000000000000000000000009b4025b3ce9cc0ce068d8\
        0000000000000000000000000000000000000000000000000004f0fac48ee005";

    #[test]
    fn encodes_get_quote_state_calldata() {
        let b_token = hex::decode(BTOKEN).unwrap();

        assert_eq!(
            hex::encode(get_quote_state_calldata(&b_token)),
            format!("97a611c1000000000000000000000000{BTOKEN}")
        );
    }

    #[test]
    fn decodes_quote_state_attributes() {
        let data = hex::decode(QUOTE_STATE_RETURN.replace(char::is_whitespace, "")).unwrap();
        let tokens = decode_quote_state(&data).unwrap();
        let attributes = attributes_from_tokens(tokens, ChangeType::Update);

        assert_eq!(attributes.len(), 19);
        assert!(attributes
            .iter()
            .any(|attr| attr.name == "snapshot_curve_blv"));
        assert_eq!(
            attributes
                .iter()
                .find(|attr| attr.name == "reserve_decimals")
                .unwrap()
                .value,
            BigInt::from(18).to_signed_bytes_be()
        );
        assert_eq!(
            attributes
                .iter()
                .find(|attr| attr.name == "should_settle_pending_surplus")
                .unwrap()
                .value,
            vec![1]
        );
    }
}
