//! One-shot reader for the TychoRouterV3's on-chain router fee on output.
//!
//! The integration test backs the router fee out of the simulated amount out before comparing it
//! against the on-chain executed amount. Rather than hard-coding that fee, this module reads it
//! from the deployed FeeCalculator at start-up so the test tracks fee changes automatically.

use alloy::{
    network::Ethereum,
    primitives::{Address, Bytes as AlloyBytes, TxKind},
    providers::{Provider, RootProvider},
    rpc::types::TransactionRequest,
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
        function MAX_FEE_BPS() external view returns (uint32);
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
/// precision scale (`MAX_FEE_BPS`) and the default fee on output (`getRouterFeeOnOutput`). The
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
        eth_call::<IFeeCalculator::MAX_FEE_BPSCall>(
            provider,
            fee_calculator,
            "MAX_FEE_BPS",
            IFeeCalculator::MAX_FEE_BPSCall {}.abi_encode(),
        )
        .await?,
    );
    if denominator == 0 {
        return Err(miette!(
            "FeeCalculator {fee_calculator} reported a zero MAX_FEE_BPS precision scale"
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

/// Performs an `eth_call` of `calldata` against `contract` and decodes the return value.
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
        .await
        .map_err(|e| miette!("{method} call to {contract} failed: {e}"))?;
    C::abi_decode_returns(response.as_ref())
        .map_err(|e| miette!("failed to decode {method} response from {contract}: {e}"))
}
