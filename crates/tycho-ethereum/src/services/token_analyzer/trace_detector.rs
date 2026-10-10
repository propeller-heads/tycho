use std::{cmp, sync::Arc};

use alloy::{
    primitives::{Address, U256},
    rpc::types::{
        trace::parity::{TraceOutput, TraceResults},
        TransactionRequest,
    },
    sol_types::SolCall,
};
use tycho_common::{
    models::{
        blockchain::BlockTag,
        token::{TokenQuality, TransferCost, TransferTax},
    },
    traits::{TokenAnalyzer, TokenOwnerFinding},
    Bytes,
};

use super::{
    arbitrary_recipient, calculate_fee_bps, call_request, map_block_tag, ObservedTransfer,
};
use crate::{
    erc20::{approveCall, balanceOfCall, transferCall},
    rpc::EthereumRpcClient,
    BytesCodec,
};

/// Detects whether a token is "bad" (works in unexpected ways that are
/// problematic for solving) by simulating several transfers of a token. To find
/// an initial address to transfer from we use the amm pair providers.
/// Tokens are bad if:
/// - we cannot find an amm pool of the token to one of the base tokens
/// - transfer into the settlement contract or back out fails
/// - a transfer loses total balance
///
/// # Deprecated
///
/// Superseded by [`crate::services::token_analyzer::EthCallDetector`], which uses a single
/// `eth_call` with bytecode state overrides instead of `trace_callMany`. Prefer
/// `EthCallDetector` for all new usage.
#[deprecated(
    since = "0.154.0",
    note = "Use EthCallDetector instead. TraceCallDetector requires trace_callMany which is \
            not available on all chains and was slow to execute."
)]
pub struct TraceCallDetector {
    pub rpc: EthereumRpcClient,
    pub finder: Arc<dyn TokenOwnerFinding>,
    pub settlement_contract: Address,
}

#[allow(deprecated)]
#[async_trait::async_trait]
impl TokenAnalyzer for TraceCallDetector {
    type Error = String;

    async fn analyze(
        &self,
        token: Bytes,
        block: BlockTag,
    ) -> Result<(TokenQuality, Option<TransferCost>, Option<TransferTax>), String> {
        let (quality, transfer_cost, tax) = self
            .detect_impl(Address::from_bytes(&token), block)
            .await
            .map_err(|e| e.to_string())?;
        tracing::debug!(?token, ?quality, "determined token quality");
        Ok((
            quality,
            transfer_cost.map(|cost| cost.try_into().unwrap_or(8_000_000)),
            tax.map(|cost| cost.try_into().unwrap_or(10_000)),
        ))
    }
}

enum TraceRequestType {
    SimpleTransfer,
    DoubleTransfer(U256),
}

#[allow(deprecated)]
impl TraceCallDetector {
    pub fn new(
        rpc: &EthereumRpcClient,
        finder: Arc<dyn TokenOwnerFinding>,
        settlement_contract: Address,
    ) -> Self {
        Self { rpc: rpc.clone(), finder, settlement_contract }
    }

    pub async fn detect_impl(
        &self,
        token: Address,
        block: BlockTag,
    ) -> Result<(TokenQuality, Option<U256>, Option<U256>), String> {
        let block_tag = map_block_tag(block);

        // Arbitrary amount that is large enough that small relative fees should be
        // visible.
        const MIN_AMOUNT: u64 = 100_000;
        let (take_from, amount) = match self
            .finder
            .find_owner(token.to_bytes(), MIN_AMOUNT.into())
            .await
            .map_err(|e| e.to_string())?
        {
            Some((address, balance)) => {
                // Don't use the full balance, but instead a portion of it. This
                // makes the trace call less racy and prone to the transfer
                // failing because of a balance change from one block to the
                // next. This can happen because of either:
                // - Block propagation - the trace_callMany is handled by a node that is 1 block in
                //   the past
                // - New block observed - the trace_callMany is executed on a block that came in
                //   since we read the balance
                let amount = cmp::max(
                    U256::from_be_bytes::<32>(
                        balance
                            .lpad(32, 0)
                            .as_ref()
                            .try_into()
                            .expect("balance should be 32 bytes"),
                    ) / U256::from(2),
                    U256::from(MIN_AMOUNT),
                );

                tracing::debug!(?token, ?address, ?amount, "found owner");
                (Address::from_bytes(&address), amount)
            }
            None => {
                return Ok((
                    TokenQuality::bad(format!(
                        "Could not find on chain source of the token with at least {MIN_AMOUNT} \
                     balance.",
                    )),
                    None,
                    None,
                ))
            }
        };

        // We transfer the full available amount of the token from the amm pool into the
        // settlement contract and then to an arbitrary address.
        // Note that gas use can depend on the recipient because for the standard
        // implementation sending to an address that does not have any balance
        // yet (implicitly 0) causes an allocation.
        let request =
            self.create_trace_request(token, amount, take_from, TraceRequestType::SimpleTransfer);
        let simple_transfer_traces = self
            .rpc
            .trace_call_many(request, block_tag)
            .await
            .map_err(|e| e.to_string())?;

        let message = "\
        Failed to decode the token's balanceOf response because it did not \
        return 32 bytes. A common cause of this is a bug in the Vyper \
        smart contract compiler. See \
        https://github.com/cowprotocol/services/pull/781 for more \
        information.\
        ";
        let bad = TokenQuality::Bad { reason: message.to_string() };
        let middle_balance = match decode_u256(&simple_transfer_traces[2]) {
            Some(balance) => balance,
            None => return Ok((bad, None, None)),
        };

        let request = self.create_trace_request(
            token,
            amount,
            take_from,
            TraceRequestType::DoubleTransfer(middle_balance),
        );
        let double_transfer_traces = self
            .rpc
            .trace_call_many(request, block_tag)
            .await
            .map_err(|e| e.to_string())?;
        Self::handle_response(&double_transfer_traces, amount, middle_balance, take_from)
            .map_err(|e| e.to_string())
    }

    fn create_trace_request(
        &self,
        token: Address,
        amount: U256,
        take_from: Address,
        request_type: TraceRequestType,
    ) -> Vec<TransactionRequest> {
        let mut requests = Vec::new();
        let recipient = arbitrary_recipient();

        // 0 Get balance of settlement_contract before
        let calldata = balanceOfCall { _owner: self.settlement_contract }.abi_encode();
        requests.push(call_request(None, token, calldata));

        // 1 Transfer from take_from to settlement_contract
        let calldata = transferCall { _to: self.settlement_contract, _value: amount }.abi_encode();
        requests.push(call_request(Some(take_from), token, calldata));

        // 2 Get balance of settlement_contract after
        let calldata = balanceOfCall { _owner: self.settlement_contract }.abi_encode();
        requests.push(call_request(None, token, calldata));

        // 3 Get balance of arbitrary_recipient before
        let calldata = balanceOfCall { _owner: recipient }.abi_encode();
        requests.push(call_request(None, token, calldata));

        if let TraceRequestType::DoubleTransfer(middle_amount) = request_type {
            // 4 Transfer from settlement_contract to arbitrary_recipient
            let calldata = transferCall { _to: recipient, _value: middle_amount }.abi_encode();
            requests.push(call_request(Some(self.settlement_contract), token, calldata));

            // 5 Get balance of settlement_contract after
            let calldata = balanceOfCall { _owner: self.settlement_contract }.abi_encode();
            requests.push(call_request(None, token, calldata));

            // 6 Get balance of arbitrary_recipient after
            let calldata = balanceOfCall { _owner: recipient }.abi_encode();
            requests.push(call_request(None, token, calldata));

            // 7 Approve max with settlement_contract
            let calldata = approveCall { _spender: recipient, _value: U256::MAX }.abi_encode();
            requests.push(call_request(Some(self.settlement_contract), token, calldata));
        }

        requests
    }

    fn handle_response(
        traces: &[TraceResults],
        amount: U256,
        middle_amount: U256,
        take_from: Address,
    ) -> Result<(TokenQuality, Option<U256>, Option<U256>), String> {
        if traces.len() != 8 {
            return Err("unexpected number of traces".to_string());
        }

        let gas_in = match ensure_transaction_ok_and_get_gas(&traces[1])? {
            Ok(gas) => gas,
            Err(reason) => {
                return Ok((
                    TokenQuality::bad(format!(
                        "Transfer of token from on chain source {take_from:?} into settlement \
                     contract failed: {reason}"
                    )),
                    None,
                    None,
                ))
            }
        };
        let arbitrary = arbitrary_recipient();
        let gas_out = match ensure_transaction_ok_and_get_gas(&traces[4])? {
            Ok(gas) => gas,
            Err(reason) => {
                return Ok((
                    TokenQuality::bad(format!(
                        "Transfer token out of settlement contract to arbitrary recipient \
                     {arbitrary:?} failed: {reason}",
                    )),
                    None,
                    None,
                ))
            }
        };

        let gas_per_transfer = (gas_in + gas_out) / U256::from(2);

        let message = "\
            Failed to decode the token's balanceOf response because it did not \
            return 32 bytes. A common cause of this is a bug in the Vyper \
            smart contract compiler. See \
            https://github.com/cowprotocol/services/pull/781 for more \
            information.\
        ";
        let bad = TokenQuality::Bad { reason: message.to_string() };
        let balance_before_in = match decode_u256(&traces[0]) {
            Some(balance) => balance,
            None => return Ok((bad, Some(gas_per_transfer), None)),
        };
        let balance_after_in = match decode_u256(&traces[2]) {
            Some(balance) => balance,
            None => return Ok((bad, Some(gas_per_transfer), None)),
        };
        let balance_after_out = match decode_u256(&traces[5]) {
            Some(balance) => balance,
            None => return Ok((bad, Some(gas_per_transfer), None)),
        };
        let balance_recipient_before = match decode_u256(&traces[3]) {
            Some(balance) => balance,
            None => return Ok((bad, Some(gas_per_transfer), None)),
        };
        let balance_recipient_after = match decode_u256(&traces[6]) {
            Some(balance) => balance,
            None => return Ok((bad, Some(gas_per_transfer), None)),
        };

        let fees = calculate_fee_bps(
            ObservedTransfer {
                sent: amount,
                balance_before: balance_before_in,
                balance_after: balance_after_in,
            },
            ObservedTransfer {
                sent: middle_amount,
                balance_before: balance_recipient_before,
                balance_after: balance_recipient_after,
            },
        );

        tracing::debug!(%amount, %balance_before_in, %balance_after_in, %balance_after_out);

        // todo: Maybe do >= checks in case token transfer for whatever reason grants
        // user more than an amount transferred like an anti fee.

        let fees = match fees {
            Ok(f) => f,
            Err(e) => {
                return Ok((
                    TokenQuality::bad(format!("Failed to calculate fees for token transfer: {e}")),
                    None,
                    None,
                ))
            }
        };

        let computed_balance_after_in = match balance_before_in.checked_add(amount) {
            Some(amount) => amount,
            None => {
                return Ok((
                    TokenQuality::bad(format!(
                    "Transferring {amount} into settlement contract would overflow its balance."
                )),
                    Some(gas_per_transfer),
                    Some(fees),
                ))
            }
        };
        if balance_after_in != computed_balance_after_in {
            return Ok((
                TokenQuality::bad(format!(
                    "Transferring {amount} into settlement contract was expected to result in a \
                 balance of {computed_balance_after_in} but actually resulted in \
                 {balance_after_in}. A common cause for this is that the token takes a fee on \
                 transfer."
                )),
                Some(gas_per_transfer),
                Some(fees),
            ));
        }
        if balance_after_out != balance_before_in {
            return Ok((
                TokenQuality::bad(format!(
                "Transferring {amount} out of settlement contract was expected to result in the \
                 original balance of {balance_before_in} but actually resulted in \
                 {balance_after_out}."
            )),
                Some(gas_per_transfer),
                Some(fees),
            ));
        }
        let computed_balance_recipient_after = match balance_recipient_before.checked_add(amount) {
            Some(amount) => amount,
            None => {
                return Ok((
                    TokenQuality::bad(format!(
                    "Transferring {amount} into arbitrary recipient {arbitrary:?} would overflow \
                     its balance."
                )),
                    Some(gas_per_transfer),
                    Some(fees),
                ))
            }
        };
        if computed_balance_recipient_after != balance_recipient_after {
            return Ok((
                TokenQuality::bad(format!(
                    "Transferring {amount} into arbitrary recipient {arbitrary:?} was expected to \
                 result in a balance of {computed_balance_recipient_after} but actually resulted \
                 in {balance_recipient_after}. A common cause for this is that the token takes a \
                 fee on transfer."
                )),
                Some(gas_per_transfer),
                Some(fees),
            ));
        }

        if let Err(err) = ensure_transaction_ok_and_get_gas(&traces[7])? {
            return Ok((
                TokenQuality::bad(format!("Approval of U256::MAX failed: {err}")),
                Some(gas_per_transfer),
                Some(fees),
            ));
        }

        Ok((TokenQuality::Good, Some(gas_per_transfer), Some(fees)))
    }
}

/// Returns none if the length of the bytes in the trace output is not 32.
fn decode_u256(trace: &TraceResults) -> Option<U256> {
    let bytes = trace.output.iter().as_slice();
    if bytes.len() != 32 {
        return None;
    }
    Some(U256::from_be_bytes::<32>(bytes.try_into().unwrap()))
}

// The outer result signals communication failure with the node.
// The inner result is Ok(gas_price) or Err if the transaction failed.
fn ensure_transaction_ok_and_get_gas(trace: &TraceResults) -> Result<Result<U256, String>, String> {
    let transaction_traces = &trace.trace;
    let first = transaction_traces
        .first()
        .ok_or_else(|| "expected at least one trace".to_string())?;
    if let Some(error) = &first.error {
        return Ok(Err(format!("transaction failed: {error}")));
    }
    let call_result = match &first.result {
        Some(TraceOutput::Call(call)) => call,
        _ => return Err("no error but also no call result".to_string()),
    };
    Ok(Ok(U256::from(call_result.gas_used)))
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use alloy::primitives::{address, Address};
    use tycho_common::models::token::TokenOwnerStore;

    use super::*;
    use crate::test_fixtures::{TestFixture, TEST_BLOCK_NUMBER, TOKEN_HOLDERS, USDC_STR};

    const COWSWAP_SETTLEMENT: Address = address!("c9f2e6ea1637E499406986ac50ddC92401ce1f58");

    impl TestFixture {
        pub(crate) fn create_trace_call_detector(&self) -> TraceCallDetector {
            // We do not enable batching as the token pre-processor does not leverage it currently
            let rpc = self.create_rpc_client(false);

            // Use shared token holders
            let token_finder = TokenOwnerStore::new(TOKEN_HOLDERS.clone());

            TraceCallDetector::new(&rpc, Arc::new(token_finder), COWSWAP_SETTLEMENT)
        }
    }

    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_detect_impl_usdc() {
        let fixture = TestFixture::new();
        let detector = fixture.create_trace_call_detector();

        // USDC mainnet address
        let usdc_address = Address::from_str(USDC_STR).unwrap();

        // Test with the latest block
        let result = detector
            .detect_impl(usdc_address, BlockTag::Number(TEST_BLOCK_NUMBER))
            .await;

        match result {
            Ok((quality, gas_cost, transfer_tax)) => {
                println!("USDC Analysis Results:");
                println!("  Quality: {:?}", quality);
                println!("  Gas Cost: {:?}", gas_cost);
                println!("  Transfer Tax: {:?}", transfer_tax);

                // USDC should be a good token (no fees, standard behavior)
                assert!(matches!(quality, TokenQuality::Good));
                assert!(gas_cost.is_some());
                assert!(transfer_tax.is_some());

                // USDC should have 0 transfer tax
                if let Some(tax) = transfer_tax {
                    assert_eq!(tax, U256::ZERO, "USDC should not have transfer fees");
                }
            }
            Err(e) => {
                panic!("Failed to analyze USDC: {}", e);
            }
        }
    }
}
