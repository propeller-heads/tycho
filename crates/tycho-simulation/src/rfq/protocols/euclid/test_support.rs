//! Fixtures shared by the tests of the Euclid client, models, source and feed.

use std::str::FromStr;

use alloy::{primitives::U256, sol_types::SolCall};
use num_bigint::BigUint;
use tycho_common::{models::protocol::GetAmountOutParams, Bytes};

use crate::rfq::protocols::euclid::models::{
    fillOrderRFQToCall, EuclidFirmQuote, EuclidFirmResponse,
};

/// A levels frame recorded from Euclid's gateway on Ethereum: WETH/USDC.
pub(super) const LEVELS_FRAME: &str = include_str!("test_responses/levels_frame.json");
/// A binding quote recorded from Euclid's gateway on Ethereum: 0.003 WETH for USDC.
pub(super) const FIRM_QUOTE: &str = include_str!("test_responses/firm_quote.json");

pub(super) const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
pub(super) const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
/// The TychoRouter the recorded quote was requested for, as sender and receiver: the one the
/// router tests deploy, so the same recording fills through it on a fork.
pub(super) const ROUTER: &str = "0x6bc529dc7b81a031828ddce2bc419d01ff268c66";
/// What the recorded order pays for its 0.003 WETH, in USDC.
pub(super) const RECORDED_AMOUNT_OUT: &str = "6241692";

pub(super) fn bytes(value: &str) -> Bytes {
    Bytes::from_str(value).unwrap()
}

/// The request the recorded quote answers: 0.003 WETH for USDC, to the router.
pub(super) fn recorded_params() -> GetAmountOutParams {
    GetAmountOutParams {
        amount_in: BigUint::from_str("3000000000000000").unwrap(),
        token_in: bytes(WETH),
        token_out: bytes(USDC),
        sender: bytes(ROUTER),
        receiver: bytes(ROUTER),
        origin: None,
    }
}

fn recorded_quote() -> EuclidFirmQuote {
    match serde_json::from_str::<EuclidFirmResponse>(FIRM_QUOTE).unwrap() {
        EuclidFirmResponse::Quote(quote) => quote,
        EuclidFirmResponse::Declined(declined) => panic!("recorded a decline: {declined:?}"),
    }
}

/// The recorded quote with its order changed by `change` and re-encoded. The original's expiry
/// is moved far ahead first, since the recording lasted thirty seconds; the expiry sits in the
/// 64 bits above the nonce of the order's `info`.
pub(super) fn quote_with(change: impl FnOnce(&mut fillOrderRFQToCall)) -> EuclidFirmQuote {
    let quote = recorded_quote();
    let mut call = fillOrderRFQToCall::abi_decode(quote.calldata.as_ref()).unwrap();
    call.order.info = (call.order.info & U256::from(u64::MAX)) | (U256::from(u64::MAX) << 64usize);
    change(&mut call);
    EuclidFirmQuote { calldata: call.abi_encode().into(), ..quote }
}

/// The recorded response with its order's expiry moved far ahead: what the gateway would answer
/// now.
pub(super) fn live_firm_quote() -> String {
    let mut response: serde_json::Value = serde_json::from_str(FIRM_QUOTE).unwrap();
    response["calldata"] = serde_json::json!(quote_with(|_| {}).calldata);
    response.to_string()
}
