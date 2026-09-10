//! Decoding of TychoRouter swap entry-point calls across ABI generations.
mod error_table;
pub mod revert;
pub mod swaps;

use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::Call;

use crate::{
    abi::{tycho_router_v2 as v2, tycho_router_v3_0 as v3_0, tycho_router_v3_1 as v3_1},
    params::RouterVersion,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Single,
    Sequential,
    Split,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Single => "single",
            Method::Sequential => "sequential",
            Method::Split => "split",
        }
    }
}

/// How the input funds reach the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Funding {
    TransferFrom,
    Permit2,
    Vault,
    /// Funds were already in the router (V2 with `isTransferFromAllowed = false`).
    None,
}

impl Funding {
    pub fn as_str(self) -> &'static str {
        match self {
            Funding::TransferFrom => "transfer_from",
            Funding::Permit2 => "permit2",
            Funding::Vault => "vault",
            Funding::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFeeParams {
    pub fee_bps: BigInt,
    pub receiver: Vec<u8>,
    pub max_client_contribution: BigInt,
    pub deadline: BigInt,
    pub signature: Vec<u8>,
}

impl From<(BigInt, Vec<u8>, BigInt, BigInt, Vec<u8>)> for ClientFeeParams {
    fn from(t: (BigInt, Vec<u8>, BigInt, BigInt, Vec<u8>)) -> Self {
        ClientFeeParams {
            fee_bps: t.0,
            receiver: t.1,
            max_client_contribution: t.2,
            deadline: t.3,
            signature: t.4,
        }
    }
}

/// Version-independent view of a decoded swap entry-point call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapCall {
    pub method: Method,
    pub funding: Funding,
    pub amount_in: BigInt,
    pub token_in: Vec<u8>,
    pub token_out: Vec<u8>,
    pub expected_amount_out: Option<BigInt>,
    pub min_amount_out: BigInt,
    pub receiver: Vec<u8>,
    /// Size of the token array; only encoded for split swaps.
    pub n_tokens: Option<u32>,
    pub client_fee: Option<ClientFeeParams>,
    pub wrap_eth: bool,
    pub unwrap_eth: bool,
    pub swaps: Vec<u8>,
    /// Bytes appended after the canonical ABI encoding of the arguments.
    pub watermark: Vec<u8>,
}

/// Decodes `call` if its selector is one of the swap entry points of `version`.
///
/// Returns `None` for non-swap calls (admin functions, callbacks, executor delegatecalls) and
/// `Some(Err)` when the selector matched but the arguments could not be decoded.
pub fn decode_swap_call(version: RouterVersion, call: &Call) -> Option<Result<SwapCall, String>> {
    match version {
        RouterVersion::V2 => decode_v2(call),
        RouterVersion::V3_0 => decode_v3_0(call),
        RouterVersion::V3_1 => decode_v3_1(call),
    }
}

/// Decodes the `uint256 amountOut` return value shared by every swap entry point.
pub fn decode_amount_out(return_data: &[u8]) -> Result<BigInt, String> {
    v3_1::functions::SingleSwap::output(return_data)
}

fn watermark(input: &[u8], canonical: &[u8]) -> Vec<u8> {
    if input.len() > canonical.len() && input.starts_with(canonical) {
        input[canonical.len()..].to_vec()
    } else {
        Vec::new()
    }
}

/// Clamps `nTokens` to u32; the router indexes tokens with a single byte anyway.
fn n_tokens(value: &BigInt) -> Option<u32> {
    Some(
        value
            .to_string()
            .parse::<u32>()
            .unwrap_or(u32::MAX),
    )
}

macro_rules! try_decode {
    ($call:expr, $ty:ty, |$d:ident| $build:expr) => {
        if <$ty>::match_call($call) {
            return Some(<$ty>::decode($call).map(|$d| {
                let canonical = $d.encode();
                let mut swap: SwapCall = $build;
                swap.watermark = watermark(&$call.input, &canonical);
                swap
            }));
        }
    };
}

fn decode_v2(call: &Call) -> Option<Result<SwapCall, String>> {
    macro_rules! v2_common {
        ($d:ident, $method:expr, $funding:expr, $n_tokens:expr, $swaps:expr) => {
            SwapCall {
                method: $method,
                funding: $funding,
                amount_in: $d.amount_in,
                token_in: $d.token_in,
                token_out: $d.token_out,
                expected_amount_out: None,
                min_amount_out: $d.min_amount_out,
                receiver: $d.receiver,
                n_tokens: $n_tokens,
                client_fee: None,
                wrap_eth: $d.wrap_eth,
                unwrap_eth: $d.unwrap_eth,
                swaps: $swaps,
                watermark: Vec::new(),
            }
        };
    }
    fn funding(transfer_from: bool) -> Funding {
        if transfer_from {
            Funding::TransferFrom
        } else {
            Funding::None
        }
    }
    try_decode!(call, v2::functions::SingleSwap, |d| v2_common!(
        d,
        Method::Single,
        funding(d.is_transfer_from_allowed),
        None,
        d.swap_data
    ));
    try_decode!(call, v2::functions::SingleSwapPermit2, |d| v2_common!(
        d,
        Method::Single,
        Funding::Permit2,
        None,
        d.swap_data
    ));
    try_decode!(call, v2::functions::SequentialSwap, |d| v2_common!(
        d,
        Method::Sequential,
        funding(d.is_transfer_from_allowed),
        None,
        d.swaps
    ));
    try_decode!(call, v2::functions::SequentialSwapPermit2, |d| v2_common!(
        d,
        Method::Sequential,
        Funding::Permit2,
        None,
        d.swaps
    ));
    try_decode!(call, v2::functions::SplitSwap, |d| v2_common!(
        d,
        Method::Split,
        funding(d.is_transfer_from_allowed),
        n_tokens(&d.n_tokens),
        d.swaps
    ));
    try_decode!(call, v2::functions::SplitSwapPermit2, |d| v2_common!(
        d,
        Method::Split,
        Funding::Permit2,
        n_tokens(&d.n_tokens),
        d.swaps
    ));
    None
}

macro_rules! v3_common {
    ($d:ident, $method:expr, $funding:expr, $n_tokens:expr, $expected:expr, $swaps:expr) => {
        SwapCall {
            method: $method,
            funding: $funding,
            amount_in: $d.amount_in,
            token_in: $d.token_in,
            token_out: $d.token_out,
            expected_amount_out: $expected,
            min_amount_out: $d.min_amount_out,
            receiver: $d.receiver,
            n_tokens: $n_tokens,
            client_fee: Some(ClientFeeParams::from($d.client_fee_params)),
            wrap_eth: false,
            unwrap_eth: false,
            swaps: $swaps,
            watermark: Vec::new(),
        }
    };
}

fn decode_v3_0(call: &Call) -> Option<Result<SwapCall, String>> {
    use v3_0::functions as f;
    try_decode!(call, f::SingleSwap, |d| v3_common!(
        d,
        Method::Single,
        Funding::TransferFrom,
        None,
        None,
        d.swap_data
    ));
    try_decode!(call, f::SingleSwapPermit2, |d| v3_common!(
        d,
        Method::Single,
        Funding::Permit2,
        None,
        None,
        d.swap_data
    ));
    try_decode!(call, f::SingleSwapUsingVault, |d| v3_common!(
        d,
        Method::Single,
        Funding::Vault,
        None,
        None,
        d.swap_data
    ));
    try_decode!(call, f::SequentialSwap, |d| v3_common!(
        d,
        Method::Sequential,
        Funding::TransferFrom,
        None,
        None,
        d.swaps
    ));
    try_decode!(call, f::SequentialSwapPermit2, |d| v3_common!(
        d,
        Method::Sequential,
        Funding::Permit2,
        None,
        None,
        d.swaps
    ));
    try_decode!(call, f::SequentialSwapUsingVault, |d| v3_common!(
        d,
        Method::Sequential,
        Funding::Vault,
        None,
        None,
        d.swaps
    ));
    try_decode!(call, f::SplitSwap, |d| v3_common!(
        d,
        Method::Split,
        Funding::TransferFrom,
        n_tokens(&d.n_tokens),
        None,
        d.swaps
    ));
    try_decode!(call, f::SplitSwapPermit2, |d| v3_common!(
        d,
        Method::Split,
        Funding::Permit2,
        n_tokens(&d.n_tokens),
        None,
        d.swaps
    ));
    try_decode!(call, f::SplitSwapUsingVault, |d| v3_common!(
        d,
        Method::Split,
        Funding::Vault,
        n_tokens(&d.n_tokens),
        None,
        d.swaps
    ));
    None
}

fn decode_v3_1(call: &Call) -> Option<Result<SwapCall, String>> {
    use v3_1::functions as f;
    try_decode!(call, f::SingleSwap, |d| v3_common!(
        d,
        Method::Single,
        Funding::TransferFrom,
        None,
        Some(d.expected_amount_out),
        d.swap_data
    ));
    try_decode!(call, f::SingleSwapPermit2, |d| v3_common!(
        d,
        Method::Single,
        Funding::Permit2,
        None,
        Some(d.expected_amount_out),
        d.swap_data
    ));
    try_decode!(call, f::SingleSwapUsingVault, |d| v3_common!(
        d,
        Method::Single,
        Funding::Vault,
        None,
        Some(d.expected_amount_out),
        d.swap_data
    ));
    try_decode!(call, f::SequentialSwap, |d| v3_common!(
        d,
        Method::Sequential,
        Funding::TransferFrom,
        None,
        Some(d.expected_amount_out),
        d.swaps
    ));
    try_decode!(call, f::SequentialSwapPermit2, |d| v3_common!(
        d,
        Method::Sequential,
        Funding::Permit2,
        None,
        Some(d.expected_amount_out),
        d.swaps
    ));
    try_decode!(call, f::SequentialSwapUsingVault, |d| v3_common!(
        d,
        Method::Sequential,
        Funding::Vault,
        None,
        Some(d.expected_amount_out),
        d.swaps
    ));
    try_decode!(call, f::SplitSwap, |d| v3_common!(
        d,
        Method::Split,
        Funding::TransferFrom,
        n_tokens(&d.n_tokens),
        Some(d.expected_amount_out),
        d.swaps
    ));
    try_decode!(call, f::SplitSwapPermit2, |d| v3_common!(
        d,
        Method::Split,
        Funding::Permit2,
        n_tokens(&d.n_tokens),
        Some(d.expected_amount_out),
        d.swaps
    ));
    try_decode!(call, f::SplitSwapUsingVault, |d| v3_common!(
        d,
        Method::Split,
        Funding::Vault,
        n_tokens(&d.n_tokens),
        Some(d.expected_amount_out),
        d.swaps
    ));
    None
}

#[cfg(test)]
mod tests;
