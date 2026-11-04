//! One-shot reader for the TychoRouterV3's on-chain router fee on output.
//!
//! The integration test backs the router fee out of the simulated amount out before comparing it
//! against the on-chain executed amount. Rather than hard-coding that fee, this module reads it
//! from the deployed FeeCalculator at start-up so the test tracks fee changes automatically.

use alloy::{
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes, TxKind},
    providers::{Provider, RootProvider},
    rpc::types::{BlockId, TransactionRequest},
    sol,
    sol_types::SolCall,
};
use miette::miette;
use tycho_common::Bytes;

sol! {
    interface ITychoRouter {
        function getFeeCalculator() external view returns (address);
    }

    interface IFeeCalculator {
        function MAX_BPS() external view returns (uint32);
        function getRouterFeeOnOutput() external view returns (uint32);
    }
}

/// Router fee on output, expressed as a fraction (`numerator / denominator`) in the
/// FeeCalculator's fee-unit scale (100% = `denominator` units).
#[derive(Clone, Copy, Debug)]
pub struct RouterFeeOnOutput {
    numerator: u64,
    denominator: u64,
}

impl RouterFeeOnOutput {
    /// The fee numerator in the FeeCalculator's fee-unit scale.
    pub fn numerator(&self) -> u64 {
        self.numerator
    }

    /// The fee denominator (the FeeCalculator's precision scale, where this value represents 100%).
    pub fn denominator(&self) -> u64 {
        self.denominator
    }
}

/// Reads the default router fee on output from the on-chain FeeCalculator.
///
/// Resolves the FeeCalculator address from `router_address` (`getFeeCalculator`), then reads its
/// precision scale (`MAX_BPS`) and the default fee on output (`getRouterFeeOnOutput`). The
/// integration test simulates swaps with no client fee and no per-client override, so the default
/// fee is the rate the router applies on-chain.
///
/// # Errors
///
/// Returns an error if `router_address` is not 20 bytes, any `eth_call` fails or returns
/// undecodable data, or the FeeCalculator reports a zero precision scale.
pub async fn fetch_router_fee_on_output(
    provider: &RootProvider<Ethereum>,
    router_address: &Bytes,
) -> miette::Result<RouterFeeOnOutput> {
    if router_address.len() != 20 {
        return Err(miette!("router address {router_address:?} is not 20 bytes"));
    }
    let router = Address::from_slice(router_address.as_ref());

    let fee_calculator = eth_call::<ITychoRouter::getFeeCalculatorCall>(
        provider,
        router,
        "getFeeCalculator",
        ITychoRouter::getFeeCalculatorCall {}.abi_encode(),
    )
    .await?;

    let denominator = u64::from(
        eth_call::<IFeeCalculator::MAX_BPSCall>(
            provider,
            fee_calculator,
            "MAX_BPS",
            IFeeCalculator::MAX_BPSCall {}.abi_encode(),
        )
        .await?,
    );
    if denominator == 0 {
        return Err(miette!(
            "FeeCalculator {fee_calculator} reported a zero MAX_BPS precision scale"
        ));
    }

    let numerator = u64::from(
        eth_call::<IFeeCalculator::getRouterFeeOnOutputCall>(
            provider,
            fee_calculator,
            "getRouterFeeOnOutput",
            IFeeCalculator::getRouterFeeOnOutputCall {}.abi_encode(),
        )
        .await?,
    );

    Ok(RouterFeeOnOutput { numerator, denominator })
}

/// Performs an `eth_call` of `calldata` against `contract` at the latest block and decodes the
/// return value.
///
/// The block is set explicitly: without one, the provider sends `pending`, which nodes that keep
/// no pending state reject.
async fn eth_call<C: SolCall>(
    provider: &RootProvider<Ethereum>,
    contract: Address,
    method: &'static str,
    calldata: Vec<u8>,
) -> miette::Result<C::Return> {
    let response = provider
        .call(TransactionRequest {
            to: Some(TxKind::Call(contract)),
            input: AlloyBytes::from(calldata).into(),
            ..Default::default()
        })
        .block(BlockId::latest())
        .await
        .map_err(|e| miette!("{method} call to {contract} failed: {e}"))?;
    C::abi_decode_returns(response.as_ref())
        .map_err(|e| miette!("failed to decode {method} response from {contract}: {e}"))
}

#[cfg(test)]
mod tests {
    use alloy::{primitives::address, sol_types::SolValue};
    use serde_json::{json, Value};

    use super::*;

    const ROUTER: Address = address!("1644d2477f809cc2c71bccfd6dc9497e3f83210d");
    const FEE_CALCULATOR: Address = address!("2930cb02f8293a684981ef7fb63c160329c410f1");

    /// Answers the three router fee calls like a node that keeps no pending state: a call at
    /// `pending` gets the error such a node returns, a call at any other block gets the result.
    async fn rpc_without_pending_state(server: &mut mockito::Server) -> mockito::Mock {
        server
            .mock("POST", "/")
            .with_header("content-type", "application/json")
            .with_body_from_request(|request| {
                let request: Value = serde_json::from_slice(request.body().expect("request body"))
                    .expect("JSON-RPC request");
                let id = request["id"].clone();
                let block = request["params"][1]
                    .as_str()
                    .unwrap_or("pending");
                if block == "pending" {
                    return json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32000, "message": "pending state is not supported"},
                    })
                    .to_string()
                    .into();
                }
                let call = &request["params"][0];
                let to: Address = call["to"]
                    .as_str()
                    .expect("call target")
                    .parse()
                    .expect("address");
                let input = call["input"]
                    .as_str()
                    .expect("call input");
                let selector = &input[..10];
                let result = match (to, selector) {
                    (ROUTER, "0x46f177ec") => FEE_CALCULATOR.abi_encode(),
                    (FEE_CALCULATOR, "0xfd967f47") => 100_000_000u32.abi_encode(),
                    (FEE_CALCULATOR, "0x53b897aa") => 5_000u32.abi_encode(),
                    (to, selector) => panic!("unexpected call {selector} to {to}"),
                };
                json!({"jsonrpc": "2.0", "id": id, "result": AlloyBytes::from(result)})
                    .to_string()
                    .into()
            })
            .expect(3)
            .create_async()
            .await
    }

    #[tokio::test]
    async fn reads_the_fee_from_a_node_without_pending_state() {
        let mut server = mockito::Server::new_async().await;
        let rpc = rpc_without_pending_state(&mut server).await;
        let provider = RootProvider::<Ethereum>::new_http(server.url().parse().expect("url"));

        let fee = fetch_router_fee_on_output(&provider, &Bytes::from(ROUTER.as_slice()))
            .await
            .expect("every call names a block the node serves");

        assert_eq!(fee.numerator(), 5_000);
        assert_eq!(fee.denominator(), 100_000_000);
        rpc.assert_async().await;
    }
}
