use std::fmt::Display;

use alloy::transports::{RpcError as AlloyRpcError, TransportErrorKind};
use thiserror::Error;

/// Redacts the path component of any URLs in `s`.
///
/// Prevents API keys embedded in URL paths (e.g. `/6da0db2d2bfcd644f87e6f75db0a0d3b`) from
/// leaking into logs or error messages.
fn redact_url_paths(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut pos = 0;

    while pos < s.len() {
        let Some(idx) = s[pos..].find("://") else {
            result.push_str(&s[pos..]);
            break;
        };

        let abs = pos + idx;
        result.push_str(&s[pos..abs + 3]); // include "://"
        pos = abs + 3;

        // Find end of URL token (whitespace or closing delimiter)
        let token_end = s[pos..]
            .find(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '"' | '\''))
            .map(|i| pos + i)
            .unwrap_or(s.len());

        // If there's a path component, replace it with /***
        if let Some(slash) = s[pos..token_end].find('/') {
            result.push_str(&s[pos..pos + slash]); // host only
            result.push_str("/***");
        } else {
            result.push_str(&s[pos..token_end]); // host with no path
        }
        pos = token_end;
    }

    result
}

#[derive(Error, Debug)]
pub struct ReqwestError {
    pub msg: String,
    #[source]
    pub source: AlloyRpcError<TransportErrorKind>,
}

impl Display for ReqwestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.msg, redact_url_paths(&self.source.to_string()))
    }
}

#[derive(Error, Debug)]
pub enum RequestError {
    Reqwest(ReqwestError),
    Other(String),
}

impl Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::Reqwest(e) => write!(f, "{e}"),
            RequestError::Other(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Error, Debug)]
pub enum RPCError {
    #[error("RPC setup error: {0}")]
    SetupError(String),
    #[error("Request error: {0}")]
    RequestError(RequestError),
    #[error("Tracing failure: {0}")]
    TracingFailure(String),
    #[error("Unknown error: {0}")]
    UnknownError(String),
}

impl RPCError {
    pub(super) fn from_alloy<S: ToString>(
        msg: S,
        error: AlloyRpcError<TransportErrorKind>,
    ) -> Self {
        RPCError::RequestError(RequestError::Reqwest(ReqwestError {
            msg: msg.to_string(),
            source: error,
        }))
    }

    /// Returns true when the error is a JSON-RPC "execution reverted" response, i.e. the EVM
    /// executed the call and reverted. Such failures are deterministic properties of the called
    /// contract, not transport or provider issues.
    pub fn is_execution_reverted(&self) -> bool {
        let RPCError::RequestError(RequestError::Reqwest(e)) = self else {
            return false;
        };
        let AlloyRpcError::ErrorResp(payload) = &e.source else {
            return false;
        };
        // EIP-1474 reserves code 3 for eth_call reverts; some providers surface reverts
        // under generic codes (e.g. -32000) with an "execution reverted" message instead.
        payload.code == 3 ||
            payload
                .message
                .to_lowercase()
                .contains("execution reverted")
    }
}

#[cfg(test)]
mod tests {
    use alloy::rpc::json_rpc::ErrorPayload;
    use rstest::rstest;

    use super::{redact_url_paths, AlloyRpcError, RPCError, TransportErrorKind};

    #[rstest]
    #[case::eip_1474_revert_code(3, "execution reverted", true)]
    #[case::revert_under_generic_code(-32000, "execution reverted: something", true)]
    #[case::header_not_found(-32000, "header not found", false)]
    #[case::invalid_request(-32600, "invalid request", false)]
    fn is_execution_reverted_classification(
        #[case] code: i64,
        #[case] message: &str,
        #[case] expected: bool,
    ) {
        let payload = ErrorPayload { code, message: message.to_string().into(), data: None };
        let err = RPCError::from_alloy(
            "eth_call failed",
            AlloyRpcError::<TransportErrorKind>::ErrorResp(payload),
        );
        assert_eq!(err.is_execution_reverted(), expected);
    }

    #[test]
    fn is_execution_reverted_false_for_non_request_errors() {
        assert!(!RPCError::SetupError("bad url".to_string()).is_execution_reverted());
        assert!(!RPCError::UnknownError("boom".to_string()).is_execution_reverted());
    }

    #[test]
    fn redacts_path_component() {
        let input =
            "error sending request for url (https://arbitrum.chainstack.com/supersecretkey)";
        let output = redact_url_paths(input);
        assert_eq!(output, "error sending request for url (https://arbitrum.chainstack.com/***)");
    }

    #[test]
    fn preserves_url_without_path() {
        let input = "error sending request for url (https://arbitrum.chainstack.com)";
        let output = redact_url_paths(input);
        assert_eq!(output, input);
    }

    #[test]
    fn passthrough_when_no_url() {
        let input = "connection refused";
        assert_eq!(redact_url_paths(input), input);
    }

    #[test]
    fn redacts_multiple_urls() {
        let input = "primary https://a.example.com/key1 fallback https://b.example.com/key2";
        let output = redact_url_paths(input);
        assert_eq!(output, "primary https://a.example.com/*** fallback https://b.example.com/***");
    }

    #[test]
    fn redacts_url_at_end_of_string() {
        let input = "failed: https://node.example.com/apikey";
        let output = redact_url_paths(input);
        assert_eq!(output, "failed: https://node.example.com/***");
    }
}
