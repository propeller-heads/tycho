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
    map_block_tag,
};
use crate::{rpc::EthereumRpcClient, BytesCodec};

/// Gas limit passed to the simulated `eth_call`. Set to the Ethereum block gas limit, which is
/// a safe upper bound for a single token analysis call.
const GAS_LIMIT: u64 = 30_000_000;

/// `TokenAnalyzer` implementation using `eth_call` with bytecode state overrides.
///
/// Injects the Analyzer contract at the token holder's address and the Forwarder contract at the
/// settlement address, then executes the full analysis in a single `eth_call`. The Analyzer
/// performs four transfer legs:
///
///   Leg 1 (buy):        holder → settlement     — detects buy fees
///   Leg 2 (roundtrip):  settlement → holder     — detects fees even to the LP pool address
///   Leg 3 (retransfer): holder → settlement     — returns tokens for the sell test
///   Leg 4 (sell):       settlement → recipient  — detects sell-only fees
///
/// If legs 1–2 pass but leg 4 fails, the token has a sell-only fee.
///
/// Compatible with any EVM chain that supports `eth_call` state overrides.
pub struct EthCallDetector {
    rpc: EthereumRpcClient,
    finder: Arc<dyn TokenOwnerFinding>,
    settlement_contract: Address,
}

/// Returns the worst-case transfer tax in basis points across all three legs of the analysis:
/// the buy leg (holder → settlement), the roundtrip sell leg (settlement → holder), and the
/// sell-to-arbitrary leg (settlement → recipient). Using the maximum ensures the reported tax
/// is always conservative.
fn worst_fee(
    amount: U256,
    middle_amount: U256,
    roundtrip_received: U256,
    r: &<analyzeCall as SolCall>::Return,
) -> Result<U256, String> {
    let bps = U256::from(10_000_u32);

    // Buy fee: fraction of `amount` not received by settlement.
    let buy_fee = if middle_amount < amount && amount > U256::ZERO {
        (amount - middle_amount)
            .saturating_mul(bps)
            .checked_div(amount)
            .ok_or("division by zero in buy fee")?
    } else {
        U256::ZERO
    };

    // Roundtrip sell fee: fraction of `middle_amount` not returned to holder.
    let expected_holder = r
        .holderBefore
        .checked_sub(amount)
        .ok_or("holder balance underflow")?
        .checked_add(middle_amount)
        .ok_or("holder balance overflow")?;
    let roundtrip_fee = if r.holderAfter < expected_holder && middle_amount > U256::ZERO {
        (expected_holder - r.holderAfter)
            .saturating_mul(bps)
            .checked_div(middle_amount)
            .ok_or("division by zero in roundtrip fee")?
    } else {
        U256::ZERO
    };

    // Sell-to-arbitrary fee: fraction of `roundtrip_received` not received by the recipient.
    let expected_recipient = r
        .recipientBefore
        .checked_add(roundtrip_received)
        .ok_or("recipient balance overflow")?;
    let sell_fee = if r.recipientAfter < expected_recipient && roundtrip_received > U256::ZERO {
        (expected_recipient - r.recipientAfter)
            .saturating_mul(bps)
            .checked_div(roundtrip_received)
            .ok_or("division by zero in sell fee")?
    } else {
        U256::ZERO
    };

    Ok(buy_fee.max(roundtrip_fee).max(sell_fee))
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

        // Arbitrary amount that is large enough that small relative fees should be visible.
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
        // --- Leg 1: buy (holder → settlement) ---
        // Without a successful buy there is nothing meaningful to report for the sell legs.
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

        // --- Leg 2: roundtrip (settlement → holder) revert ---
        // When the roundtrip transfer reverts the Solidity contract returns early, so legs 3–4
        // did not run and their results are meaningless. Report the revert directly.
        if !r.roundtripOutOk {
            return Ok((
                TokenQuality::bad(format!(
                    "Transfer of token out of settlement contract back to holder {holder:#x} \
                     failed",
                )),
                None,
                None,
            ));
        }

        let gas_per_transfer = (r.gasIn + r.gasOut) / U256::from(2);

        // Safe: Solidity guards balanceAfterIn >= balanceBeforeIn when transferInOk = true.
        let middle_amount = r
            .balanceAfterIn
            .checked_sub(r.balanceBeforeIn)
            .ok_or("settlement balance underflow after successful transfer in")?;

        // roundtripReceived: what holder actually got back in leg 2.
        // holderAfter = holderBefore - amount + roundtripReceived
        // ⟹  roundtripReceived = holderAfter + amount - holderBefore
        // Safe: holderBefore >= amount (leg 1 succeeded).
        let roundtrip_received = r
            .holderAfter
            .checked_add(amount)
            .ok_or("overflow computing roundtrip received")?
            .checked_sub(r.holderBefore)
            .ok_or("underflow computing roundtrip received")?;

        // Compute the worst-case fee across all legs so that we always report the most
        // conservative (largest) tax, regardless of which specific check fires.
        let fees = worst_fee(amount, middle_amount, roundtrip_received, &r)?;

        // Collect issues from every remaining leg without returning early. A revert is
        // always worse than a fee; among fees, the higher basis-point rate wins.
        // Each entry: (is_revert: bool, fee_bps: U256, reason: String)
        let recipient = arbitrary_recipient();
        let mut issues: Vec<(bool, U256, String)> = Vec::new();

        // Buy fee
        let expected_after_in = r
            .balanceBeforeIn
            .checked_add(amount)
            .ok_or("settlement balance overflow")?;
        if r.balanceAfterIn != expected_after_in {
            let fee_bps = (amount - middle_amount)
                .saturating_mul(U256::from(10_000))
                .checked_div(amount)
                .unwrap_or(U256::ZERO);
            issues.push((
                false,
                fee_bps,
                format!(
                    "Transferring {amount} into settlement was expected to result in a balance of \
                     {expected_after_in} but got {}. The token likely takes a fee on transfer.",
                    r.balanceAfterIn
                ),
            ));
        }

        // Roundtrip: settlement kept some of the tokens (fee in settlement's favour)
        if r.balanceAfterRoundtrip > r.balanceBeforeIn {
            let kept = r.balanceAfterRoundtrip - r.balanceBeforeIn;
            let fee_bps = kept
                .saturating_mul(U256::from(10_000))
                .checked_div(middle_amount.max(U256::from(1)))
                .unwrap_or(U256::ZERO);
            issues.push((
                false,
                fee_bps,
                format!(
                    "After roundtrip, settlement balance was expected to be {} but got {}.",
                    r.balanceBeforeIn, r.balanceAfterRoundtrip
                ),
            ));
        }

        // Roundtrip: holder received less than expected (fee left settlement but not to holder)
        let expected_holder_after = r
            .holderBefore
            .checked_sub(amount)
            .ok_or("holder balance underflow")?
            .checked_add(middle_amount)
            .ok_or("holder balance overflow")?;
        if r.holderAfter != expected_holder_after {
            let fee_raw = expected_holder_after.saturating_sub(r.holderAfter);
            let fee_bps = fee_raw
                .saturating_mul(U256::from(10_000))
                .checked_div(middle_amount.max(U256::from(1)))
                .unwrap_or(U256::ZERO);
            issues.push((
                false,
                fee_bps,
                format!(
                    "Roundtrip transfer back to holder {holder:#x} was expected to result in a \
                     balance of {expected_holder_after} but got {}. The token takes a fee even \
                     on roundtrip transfers.",
                    r.holderAfter
                ),
            ));
        }

        // Sell: transfer to arbitrary recipient reverted
        if !r.transferOutOk {
            issues.push((
                true,
                U256::ZERO,
                format!(
                    "Transfer of token out of settlement contract to arbitrary recipient \
                     {recipient:#x} failed"
                ),
            ));
        } else {
            // Sell: recipient received less than expected (sell-only fee)
            let computed_recipient_after = r
                .recipientBefore
                .checked_add(roundtrip_received)
                .ok_or("recipient balance overflow")?;
            if r.recipientAfter != computed_recipient_after {
                let fee_raw = computed_recipient_after.saturating_sub(r.recipientAfter);
                let fee_bps = fee_raw
                    .saturating_mul(U256::from(10_000))
                    .checked_div(roundtrip_received.max(U256::from(1)))
                    .unwrap_or(U256::ZERO);
                issues.push((
                    false,
                    fee_bps,
                    format!(
                        "Sell-only fee: transferring {roundtrip_received} to arbitrary recipient \
                         {recipient:#x} was expected to result in a balance of \
                         {computed_recipient_after} but got {}.",
                        r.recipientAfter
                    ),
                ));
            }

            // Sell: settlement kept tokens
            if r.balanceAfterOut > r.balanceBeforeIn {
                let kept = r.balanceAfterOut - r.balanceBeforeIn;
                let fee_bps = kept
                    .saturating_mul(U256::from(10_000))
                    .checked_div(roundtrip_received.max(U256::from(1)))
                    .unwrap_or(U256::ZERO);
                issues.push((
                    false,
                    fee_bps,
                    format!(
                        "Transferring {roundtrip_received} out of settlement was expected to \
                         restore the original balance of {} but got {}.",
                        r.balanceBeforeIn, r.balanceAfterOut
                    ),
                ));
            }
        }

        // Take the worst issue: revert beats any fee; higher fee_bps beats lower.
        let worst = issues
            .into_iter()
            .max_by(|(rev_a, bps_a, _), (rev_b, bps_b, _)| rev_a.cmp(rev_b).then(bps_a.cmp(bps_b)));

        if let Some((_, _, reason)) = worst {
            return Ok((TokenQuality::bad(reason), Some(gas_per_transfer), Some(fees)));
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
    use crate::test_fixtures::{TestFixture, TEST_BLOCK_NUMBER, TOKEN_HOLDERS, USDC_STR, WETH_STR};

    const COWSWAP_SETTLEMENT: Address = address!("c9f2e6ea1637E499406986ac50ddC92401ce1f58");

    // Builds a fully-fee-free return value. The holder starts with 2x `amount` so
    // the roundtrip accounting (holderBefore - amount + received = holderBefore) holds.
    fn good_return(amount: U256) -> <analyzeCall as SolCall>::Return {
        type R = <analyzeCall as SolCall>::Return;
        let holder_balance = amount * U256::from(2);
        R {
            transferInOk: true,
            roundtripOutOk: true,
            transferOutOk: true,
            approvalOk: true,
            balanceBeforeIn: U256::ZERO,
            balanceAfterIn: amount,
            balanceAfterRoundtrip: U256::ZERO,
            holderBefore: holder_balance,
            holderAfter: holder_balance, // no roundtrip fee: holder balance unchanged
            balanceAfterOut: U256::ZERO,
            recipientBefore: U256::ZERO,
            recipientAfter: amount,
            gasIn: U256::from(30_000_u64),
            roundtripGasOut: U256::from(25_000_u64),
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
    fn handle_response_roundtrip_failed() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.roundtripOutOk = false;
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
        assert!(gas.is_some());
        assert!(tax.is_some());
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
    fn handle_response_buy_fee() {
        // Token takes 1% fee on inbound: 1_000_000 sent, only 990_000 reaches settlement.
        // The sell leg should pass cleanly — do NOT touch recipientAfter so that it stays
        // equal to roundtrip_received (= amount, since holderBefore/holderAfter are unchanged).
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.balanceAfterIn = U256::from(990_000_u64);
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO).unwrap();
        assert!(
            matches!(quality, TokenQuality::Bad { reason } if !reason.starts_with("Sell-only fee"))
        );
        assert!(gas.is_some());
        assert_eq!(tax, Some(U256::from(100_u64))); // ~100 bps (1%)
    }

    #[test]
    fn handle_response_roundtrip_sell_fee() {
        // Token takes 1% fee even when selling back to the holder (LP pool).
        let amount = U256::from(1_000_000_u64);
        let sell_fee = U256::from(10_000_u64);
        let mut r = good_return(amount);
        // holderBefore = 2_000_000, after roundtrip holder should have 2_000_000 but got less.
        r.holderAfter = r.holderBefore - sell_fee;
        let (quality, gas, _tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO).unwrap();
        assert!(
            matches!(quality, TokenQuality::Bad { reason } if !reason.starts_with("Sell-only fee"))
        );
        assert!(gas.is_some());
    }

    #[test]
    fn handle_response_sell_only_fee() {
        // Token has no fee on roundtrip but charges 1% when selling to an arbitrary address.
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        // Arbitrary recipient gets only 990_000 instead of 1_000_000.
        r.recipientAfter = U256::from(990_000_u64);
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO).unwrap();
        assert!(
            matches!(quality, TokenQuality::Bad { reason } if reason.starts_with("Sell-only fee"))
        );
        assert!(gas.is_some());
        assert!(tax.is_some());
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

    mod bsc {
        use alloy::primitives::address;

        use super::*;
        use crate::test_fixtures::{BSC_SELL_FEE_TOKEN_STR, BSC_TOKEN_HOLDERS};

        const BSC_COWSWAP_SETTLEMENT: Address =
            address!("9008D19f58AAbD9eD0D60971565AA8510560ab41");

        impl TestFixture {
            pub(crate) fn create_bsc_ethcall_detector(&self) -> EthCallDetector {
                let rpc = self.create_rpc_client(false);
                let finder = TokenOwnerStore::new(BSC_TOKEN_HOLDERS.clone());
                EthCallDetector::new(&rpc, Arc::new(finder), BSC_COWSWAP_SETTLEMENT)
            }
        }

        #[tokio::test]
        #[ignore = "require BSC_RPC_URL"]
        async fn bsc_sell_only_fee_token() {
            let fixture = TestFixture::new_bsc();
            let detector = fixture.create_bsc_ethcall_detector();
            let token = Address::from_str(BSC_SELL_FEE_TOKEN_STR).unwrap();

            let (quality, gas, _tax) = detector
                .detect_impl(token, BlockTag::Latest)
                .await
                .expect("detect_impl failed");

            // This token charges a sell fee in two possible ways:
            //   • "Sell-only fee" prefix — fee on transfers to non-whitelisted addresses
            // (to=arbitrary)   • "roundtrip" in reason — fee on transfers TO the LP
            // pair (to=LP = the DEX sell direction) Both are sell-fee patterns with no
            // buy fee on leg 1.
            assert!(
                matches!(&quality, TokenQuality::Bad { reason }
                    if reason.starts_with("Sell-only fee") || reason.contains("roundtrip")),
                "expected sell-fee detection, got: {:?}",
                quality,
            );
            assert!(gas.is_some());
        }
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
