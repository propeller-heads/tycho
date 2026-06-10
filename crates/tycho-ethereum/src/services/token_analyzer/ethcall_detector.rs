use std::{cmp, sync::Arc};

use alloy::{
    primitives::{Address, Bytes as AlloyBytes, U256},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        TransactionInput, TransactionRequest,
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
    arbitrary_recipient,
    bytecode::{analyzeCall, ANALYZER_BYTECODE, FORWARDER_BYTECODE},
    calculate_fee, map_block_tag,
};
use crate::{rpc::EthereumRpcClient, BytesCodec};

/// Gas limit passed to the simulated `eth_call`. Set to the Ethereum block gas limit, which is
/// a safe upper bound for a single token analysis call.
const GAS_LIMIT: u64 = 30_000_000;

/// `TokenAnalyzer` implementation using `eth_call` with bytecode state overrides.
///
/// Injects the Analyzer contract at the token holder's address and the Forwarder contract at the
/// settlement address, then executes the full round-trip transfer simulation in a single
/// `eth_call`. Compatible with any EVM chain that supports `eth_call` state overrides.
pub struct EthCallDetector {
    rpc: EthereumRpcClient,
    finder: Arc<dyn TokenOwnerFinding>,
    settlement_contract: Address,
}

impl EthCallDetector {
    pub fn new(
        rpc: &EthereumRpcClient,
        finder: Arc<dyn TokenOwnerFinding>,
        settlement_contract: Address,
    ) -> Self {
        Self { rpc: rpc.clone(), finder, settlement_contract }
    }
}

#[async_trait::async_trait]
impl TokenAnalyzer for EthCallDetector {
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
        tracing::debug!(?token, ?quality, "ethcall detector: determined token quality");
        Ok((
            quality,
            transfer_cost.map(|cost| cost.try_into().unwrap_or(8_000_000)),
            tax.map(|cost| cost.try_into().unwrap_or(10_000)),
        ))
    }
}

impl EthCallDetector {
    pub async fn detect_impl(
        &self,
        token: Address,
        block: BlockTag,
    ) -> Result<(TokenQuality, Option<U256>, Option<U256>), String> {
        let block_tag = map_block_tag(block);

        // Arbitrary amount that is large enough that small relative fees should be
        // visible.
        const MIN_AMOUNT: u64 = 100_000;
        let (holder, amount) = match self
            .finder
            .find_owner(token.to_bytes(), MIN_AMOUNT.into())
            .await
            .map_err(|e| e.to_string())?
        {
            Some((address, balance)) => {
                // Use half the balance to reduce races between find_owner and the eth_call.
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
                tracing::debug!(?token, ?address, ?amount, "ethcall: found token owner");
                (Address::from_bytes(&address), amount)
            }
            None => {
                return Ok((
                    TokenQuality::bad(format!(
                        "Could not find on chain source of the token with at least \
                         {MIN_AMOUNT} balance.",
                    )),
                    None,
                    None,
                ))
            }
        };

        let recipient = arbitrary_recipient();

        let tx = TransactionRequest::default()
            .from(holder)
            .to(holder)
            .input(TransactionInput::both(
                analyzeCall { token, amount, settlement: self.settlement_contract, recipient }
                    .abi_encode()
                    .into(),
            ))
            .gas_limit(GAS_LIMIT);

        let mut overrides = StateOverride::default();
        overrides.insert(
            holder,
            AccountOverride {
                code: Some(AlloyBytes::copy_from_slice(ANALYZER_BYTECODE)),
                ..Default::default()
            },
        );
        overrides.insert(
            self.settlement_contract,
            AccountOverride {
                code: Some(AlloyBytes::copy_from_slice(FORWARDER_BYTECODE)),
                ..Default::default()
            },
        );

        let raw: AlloyBytes = self
            .rpc
            .eth_call_with_state_overrides(tx, block_tag, overrides)
            .await
            .map_err(|e| format!("eth_call with state overrides failed: {e}"))?;

        let returns = analyzeCall::abi_decode_returns(raw.as_ref())
            .map_err(|e| format!("Failed to decode Analyzer return value: {e}"))?;

        Self::handle_response(returns, amount, holder)
    }

    fn handle_response(
        r: <analyzeCall as SolCall>::Return,
        amount: U256,
        holder: Address,
    ) -> Result<(TokenQuality, Option<U256>, Option<U256>), String> {
        if !r.transferInOk {
            return Ok((
                TokenQuality::bad(format!(
                    "Transfer of token from on-chain source {holder:#x} into settlement \
                     contract failed",
                )),
                None,
                None,
            ));
        }

        let recipient = arbitrary_recipient();

        if !r.transferOutOk {
            return Ok((
                TokenQuality::bad(format!(
                    "Transfer of token out of settlement contract to arbitrary recipient \
                     {recipient:#x} failed",
                )),
                None,
                None,
            ));
        }

        let gas_per_transfer = (r.gasIn + r.gasOut) / U256::from(2);

        // The Solidity guard ensures balanceAfterIn >= balanceBeforeIn when transferInOk = true,
        // so this subtraction is always safe.
        let middle_amount = r
            .balanceAfterIn
            .checked_sub(r.balanceBeforeIn)
            .ok_or("settlement balance underflow after successful transfer in")?;

        let fees = calculate_fee(
            amount,
            middle_amount,
            r.balanceBeforeIn,
            r.balanceAfterIn,
            r.recipientBefore,
            r.recipientAfter,
        )
        .map_err(|e| format!("Failed to calculate transfer fee: {e}"))?;

        let computed_balance_after_in = r
            .balanceBeforeIn
            .checked_add(amount)
            .ok_or("settlement balance overflow when checking transfer in")?;
        if r.balanceAfterIn != computed_balance_after_in {
            return Ok((
                TokenQuality::bad(format!(
                    "Transferring {amount} into settlement was expected to result in a balance \
                     of {computed_balance_after_in} but got {}. The token likely takes a fee on \
                     transfer.",
                    r.balanceAfterIn,
                )),
                Some(gas_per_transfer),
                Some(fees),
            ));
        }

        if r.balanceAfterOut != r.balanceBeforeIn {
            return Ok((
                TokenQuality::bad(format!(
                    "Transferring {amount} out of settlement was expected to restore the \
                     original balance of {} but got {}.",
                    r.balanceBeforeIn, r.balanceAfterOut,
                )),
                Some(gas_per_transfer),
                Some(fees),
            ));
        }

        let computed_recipient_after = r
            .recipientBefore
            .checked_add(middle_amount)
            .ok_or("recipient balance overflow when checking transfer out")?;
        if r.recipientAfter != computed_recipient_after {
            return Ok((
                TokenQuality::bad(format!(
                    "Transferring {amount} to arbitrary recipient {recipient:#x} was expected \
                     to result in a balance of {computed_recipient_after} but got {}. The token \
                     likely takes a fee on transfer.",
                    r.recipientAfter,
                )),
                Some(gas_per_transfer),
                Some(fees),
            ));
        }

        if !r.approvalOk {
            return Ok((
                TokenQuality::bad("Approval of U256::MAX failed".to_string()),
                Some(gas_per_transfer),
                Some(fees),
            ));
        }

        Ok((TokenQuality::Good, Some(gas_per_transfer), Some(fees)))
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use alloy::primitives::{address, Address};
    use tycho_common::models::token::{TokenOwnerStore, TokenQuality};

    use super::*;
    use crate::test_fixtures::{
        TestFixture, TEST_BLOCK_NUMBER, TOKEN_HOLDERS, USDC_STR, USDT_STR, WETH_STR,
    };

    const COWSWAP_SETTLEMENT: Address = address!("c9f2e6ea1637E499406986ac50ddC92401ce1f58");

    // Return value builder for unit tests — all fields default to zero / true.
    fn good_return(amount: U256) -> <analyzeCall as SolCall>::Return {
        type R = <analyzeCall as SolCall>::Return;
        R {
            transferInOk: true,
            transferOutOk: true,
            approvalOk: true,
            balanceBeforeIn: U256::ZERO,
            balanceAfterIn: amount,
            balanceAfterOut: U256::ZERO,
            recipientBefore: U256::ZERO,
            recipientAfter: amount,
            gasIn: U256::from(30_000_u64),
            gasOut: U256::from(25_000_u64),
        }
    }

    #[test]
    fn handle_response_good_token() {
        let amount = U256::from(1_000_000_u64);
        let result = EthCallDetector::handle_response(good_return(amount), amount, Address::ZERO);
        let (quality, gas, tax) = result.unwrap();
        assert_eq!(quality, TokenQuality::Good);
        assert_eq!(gas, Some(U256::from(27_500_u64))); // (30_000 + 25_000) / 2
        assert_eq!(tax, Some(U256::ZERO));
    }

    #[test]
    fn handle_response_transfer_in_failed() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.transferInOk = false;
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert!(gas.is_none());
        assert!(tax.is_none());
    }

    #[test]
    fn handle_response_transfer_out_failed() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.transferOutOk = false;
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert!(gas.is_none());
        assert!(tax.is_none());
    }

    #[test]
    fn handle_response_approval_failed() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.approvalOk = false;
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert!(gas.is_some());
        assert!(tax.is_some());
    }

    #[test]
    fn handle_response_fee_on_transfer_inbound() {
        // Token takes 1% fee: 1_000_000 sent, only 990_000 received.
        let amount = U256::from(1_000_000_u64);
        let received = U256::from(990_000_u64);
        let mut r = good_return(amount);
        r.balanceAfterIn = received;
        r.recipientAfter = received; // recipient gets what settlement received
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert!(gas.is_some());
        // Fee should be ~100 bps (1%)
        assert_eq!(tax, Some(U256::from(100_u64)));
    }

    impl TestFixture {
        pub(crate) fn create_ethcall_detector(&self) -> EthCallDetector {
            let rpc = self.create_rpc_client(false);
            let finder = TokenOwnerStore::new(TOKEN_HOLDERS.clone());
            EthCallDetector::new(&rpc, Arc::new(finder), COWSWAP_SETTLEMENT)
        }
    }

    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_detect_impl_usdc() {
        let fixture = TestFixture::new();
        let detector = fixture.create_ethcall_detector();
        let usdc = Address::from_str(USDC_STR).unwrap();

        let (quality, gas, tax) = detector
            .detect_impl(usdc, BlockTag::Number(TEST_BLOCK_NUMBER))
            .await
            .expect("detect_impl failed");

        assert_eq!(quality, TokenQuality::Good);
        assert!(gas.is_some_and(|g| g > U256::ZERO));
        assert_eq!(tax, Some(U256::ZERO));
    }

    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_detect_impl_weth() {
        let fixture = TestFixture::new();
        let detector = fixture.create_ethcall_detector();
        let weth = Address::from_str(WETH_STR).unwrap();

        let (quality, gas, tax) = detector
            .detect_impl(weth, BlockTag::Number(TEST_BLOCK_NUMBER))
            .await
            .expect("detect_impl failed");

        assert_eq!(quality, TokenQuality::Good);
        assert!(gas.is_some_and(|g| g > U256::ZERO));
        assert_eq!(tax, Some(U256::ZERO));
    }

    // USDT (0xdAC1...) omits the bool return value from approve(). This reproduces the bug where
    // forwardApprove uses a typed call that reverts when ABI-decoding USDT's empty return data,
    // setting approvalOk = false and misclassifying USDT as Bad. Expected to FAIL until the
    // Forwarder uses a low-level call for approve.
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_detect_impl_usdt() {
        let fixture = TestFixture::new();
        let detector = fixture.create_ethcall_detector();
        let usdt = Address::from_str(USDT_STR).unwrap();

        let (quality, gas, tax) = detector
            .detect_impl(usdt, BlockTag::Number(TEST_BLOCK_NUMBER))
            .await
            .expect("detect_impl failed");

        assert_eq!(quality, TokenQuality::Good, "USDT should be classified as Good");
        assert!(gas.is_some_and(|g| g > U256::ZERO));
        assert_eq!(tax, Some(U256::ZERO));
    }

    mod arbitrum {
        use super::*;
        use crate::test_fixtures::{ARB_ARB_STR, ARB_TOKEN_HOLDERS, ARB_USDC_STR, ARB_WETH_STR};

        const ARB_COWSWAP_SETTLEMENT: Address =
            address!("9008D19f58AAbD9eD0D60971565AA8510560ab41");

        impl TestFixture {
            pub(crate) fn create_arb_ethcall_detector(&self) -> EthCallDetector {
                let rpc = self.create_rpc_client(false);
                let finder = TokenOwnerStore::new(ARB_TOKEN_HOLDERS.clone());
                EthCallDetector::new(&rpc, Arc::new(finder), ARB_COWSWAP_SETTLEMENT)
            }
        }

        #[tokio::test]
        #[ignore = "require ARB_RPC_URL"]
        async fn arb_usdc() {
            let fixture = TestFixture::new_arbitrum();
            let detector = fixture.create_arb_ethcall_detector();
            let token = Address::from_str(ARB_USDC_STR).unwrap();
            let (quality, gas, _tax) = detector
                .detect_impl(token, BlockTag::Latest)
                .await
                .expect("detect_impl failed");
            assert_eq!(quality, TokenQuality::Good, "Arbitrum USDC should be Good");
            assert!(gas.is_some_and(|g| g > U256::ZERO));
        }

        #[tokio::test]
        #[ignore = "require ARB_RPC_URL"]
        async fn arb_weth() {
            let fixture = TestFixture::new_arbitrum();
            let detector = fixture.create_arb_ethcall_detector();
            let token = Address::from_str(ARB_WETH_STR).unwrap();
            let (quality, gas, _tax) = detector
                .detect_impl(token, BlockTag::Latest)
                .await
                .expect("detect_impl failed");
            assert_eq!(quality, TokenQuality::Good, "Arbitrum WETH should be Good");
            assert!(gas.is_some_and(|g| g > U256::ZERO));
        }

        #[tokio::test]
        #[ignore = "require ARB_RPC_URL"]
        async fn arb_arb_token() {
            let fixture = TestFixture::new_arbitrum();
            let detector = fixture.create_arb_ethcall_detector();
            let token = Address::from_str(ARB_ARB_STR).unwrap();
            let (quality, gas, _tax) = detector
                .detect_impl(token, BlockTag::Latest)
                .await
                .expect("detect_impl failed");
            assert_eq!(quality, TokenQuality::Good, "ARB token should be Good");
            assert!(gas.is_some_and(|g| g > U256::ZERO));
        }
    }
}
