//! Revert data decoding against the known TychoRouter / FeeCalculator error ABIs.
use std::{collections::HashMap, sync::OnceLock};

use ethabi::{param_type::Reader, ParamType, Token};

use super::error_table::ERRORS;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Revert {
    /// 4-byte selector as `0x`-prefixed hex; empty when there is no revert data.
    pub selector: String,
    /// Human readable reason, e.g. `TychoRouter__NegativeSlippage(5, 10)` or the `Error(string)`
    /// message. Empty when the selector is unknown.
    pub reason: String,
}

struct ErrorAbi {
    name: &'static str,
    params: Vec<ParamType>,
}

fn table() -> &'static HashMap<[u8; 4], ErrorAbi> {
    static TABLE: OnceLock<HashMap<[u8; 4], ErrorAbi>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map = HashMap::new();
        for (name, param_names) in ERRORS {
            let params: Vec<ParamType> = param_names
                .iter()
                .map(|p| Reader::read(p).unwrap_or_else(|e| panic!("bad param type {p}: {e}")))
                .collect();
            let selector = ethabi::short_signature(name, &params);
            map.insert(selector, ErrorAbi { name, params });
        }
        map
    })
}

pub fn decode_revert(data: &[u8]) -> Revert {
    let Some(selector) = data.get(..4) else {
        return Revert {
            selector: String::new(),
            reason: if data.is_empty() {
                String::new()
            } else {
                format!("0x{}", hex::encode(data))
            },
        };
    };
    let selector_hex = format!("0x{}", hex::encode(selector));
    let Some(abi) = table().get(selector) else {
        return Revert { selector: selector_hex, reason: String::new() };
    };
    let reason = match ethabi::decode(&abi.params, &data[4..]) {
        Ok(tokens) if abi.name == "Error" => match tokens.first() {
            Some(Token::String(s)) => s.clone(),
            _ => String::new(),
        },
        Ok(tokens) => format!(
            "{}({})",
            abi.name,
            tokens
                .iter()
                .map(fmt_token)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(_) => format!("{}(<undecodable args>)", abi.name),
    };
    Revert { selector: selector_hex, reason }
}

fn fmt_token(token: &Token) -> String {
    match token {
        Token::Address(a) => format!("0x{}", hex::encode(a.as_bytes())),
        Token::Uint(u) => u.to_string(),
        Token::Int(i) => {
            // ethabi stores int256 as two's complement U256.
            let mut bytes = [0u8; 32];
            i.to_big_endian(&mut bytes);
            num_bigint::BigInt::from_signed_bytes_be(&bytes).to_string()
        }
        Token::String(s) => format!("{s:?}"),
        Token::Bool(b) => b.to_string(),
        Token::Bytes(b) | Token::FixedBytes(b) => format!("0x{}", hex::encode(b)),
        Token::Array(items) | Token::FixedArray(items) | Token::Tuple(items) => {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(fmt_token)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_error(name: &str, params: &[ParamType], tokens: &[Token]) -> Vec<u8> {
        let mut out = ethabi::short_signature(name, params).to_vec();
        out.extend(ethabi::encode(tokens));
        out
    }

    #[test]
    fn decodes_error_string() {
        let data = encode_error("Error", &[ParamType::String], &[Token::String("nope".into())]);
        let r = decode_revert(&data);
        assert_eq!(r.selector, "0x08c379a0");
        assert_eq!(r.reason, "nope");
    }

    #[test]
    fn decodes_custom_error_with_args() {
        let data = encode_error(
            "TychoRouter__NegativeSlippage",
            &[ParamType::Uint(256), ParamType::Uint(256)],
            &[Token::Uint(5.into()), Token::Uint(10.into())],
        );
        assert_eq!(decode_revert(&data).reason, "TychoRouter__NegativeSlippage(5, 10)");
    }

    #[test]
    fn decodes_negative_int_argument() {
        let minus_one = ethabi::Uint::MAX;
        let data = encode_error(
            "TychoRouter__NegativeOutputDelta",
            &[ParamType::Int(256)],
            &[Token::Int(minus_one)],
        );
        assert_eq!(decode_revert(&data).reason, "TychoRouter__NegativeOutputDelta(-1)");
    }

    #[test]
    fn decodes_executor_revert_wrapper() {
        let exec = ethabi::Address::from([0xab; 20]);
        let data = encode_error(
            "Dispatcher__SwapReverted",
            &[ParamType::Address],
            &[Token::Address(exec)],
        );
        assert_eq!(
            decode_revert(&data).reason,
            "Dispatcher__SwapReverted(0xabababababababababababababababababababab)"
        );
    }

    #[test]
    fn unknown_selector_keeps_selector_only() {
        let r = decode_revert(&[0xde, 0xad, 0xbe, 0xef, 0, 0]);
        assert_eq!(r.selector, "0xdeadbeef");
        assert_eq!(r.reason, "");
    }

    #[test]
    fn empty_and_short_data() {
        assert_eq!(decode_revert(&[]), Revert::default());
        let r = decode_revert(&[1, 2]);
        assert_eq!(r.selector, "");
        assert_eq!(r.reason, "0x0102");
    }

    #[test]
    fn truncated_args_are_reported() {
        let mut data =
            ethabi::short_signature("TychoRouter__NotAContract", &[ParamType::Address]).to_vec();
        data.extend([0u8; 3]);
        assert_eq!(decode_revert(&data).reason, "TychoRouter__NotAContract(<undecodable args>)");
    }
}
