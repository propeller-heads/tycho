use std::{cmp, sync::Arc};

use alloy::{
    primitives::{Address, Bytes as AlloyBytes, U256},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        BlockNumberOrTag, TransactionInput, TransactionRequest,
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
    bytecode::{analyzeCall, SellLeg, ANALYZER_BYTECODE, FORWARDER_BYTECODE},
    calculate_fee_bps, map_block_tag, ObservedTransfer,
};
use crate::{rpc::EthereumRpcClient, BytesCodec};

/// Gas limit passed to the simulated `eth_call`. Set to the Ethereum block gas limit, which is
/// a safe upper bound for a single token analysis call.
const GAS_LIMIT: u64 = 30_000_000;

/// Smallest amount the holder sends into the settlement: large enough that small relative fees
/// are visible.
const MIN_AMOUNT: u64 = 100_000;

/// Maximum number of pools that receive a sell leg in one analysis.
pub(crate) const MAX_SELL_POOLS: usize = 8;

/// Largest balance difference that counts as rounding, not as a fee, once the settlement splits
/// what it received into sell legs. Tokens that convert amounts to shares can lose a wei per
/// transfer, and a slice can be as small as 6_250 wei, where one wei is already over 1 bps.
const SELL_ROUNDING_WEI: u64 = 2;

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

        let Some((owner, balance)) = self
            .finder
            .find_owner(token.to_bytes(), MIN_AMOUNT.into())
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok((
                TokenQuality::bad(format!(
                    "Could not find on chain source of the token with at least {MIN_AMOUNT} \
                     balance.",
                )),
                None,
                None,
            ));
        };
        let mut holder = Address::from_bytes(&owner);
        let mut amount = halve(to_u256(&balance));
        tracing::debug!(?token, ?holder, ?amount, "ethcall: found token owner");

        let found: Vec<(Address, U256)> = self
            .finder
            .find_pools(token.to_bytes())
            .await?
            .iter()
            // Pool ids that are not addresses cannot receive tokens.
            .filter(|(pool, _)| pool.len() == 20)
            .map(|(pool, balance)| (Address::from_bytes(pool), to_u256(balance)))
            .collect();
        let addresses: Vec<Address> = found
            .iter()
            .map(|(pool, _)| *pool)
            .collect();

        let mut pools = candidate_pools(holder, addresses.clone());
        let mut simulation = self
            .simulate(token, holder, amount, &pools, block_tag)
            .await?;
        // The holder is often the token's own pair. A token that calls its pair during a
        // transfer reverts while the pair runs the Analyzer code, so let the next pool hold.
        if simulation.transfer_failed() {
            if let Some((next, balance)) = found
                .iter()
                .find(|(pool, balance)| *pool != holder && *balance >= U256::from(MIN_AMOUNT))
            {
                tracing::debug!(
                    ?token,
                    ?holder,
                    ?next,
                    "ethcall: holder cannot transfer, retrying"
                );
                holder = *next;
                amount = halve(*balance);
                pools = candidate_pools(holder, addresses);
                simulation = self
                    .simulate(token, holder, amount, &pools, block_tag)
                    .await?;
            }
        }

        match simulation {
            Simulation::Returned(returns) => {
                Self::handle_response(*returns, amount, holder, &pools)
            }
            Simulation::Reverted(e) => Ok((
                TokenQuality::bad(format!("Token analysis simulation reverted: {e}")),
                None,
                None,
            )),
        }
    }

    /// Runs the Analyzer once, with `holder` as the source of `amount` and `pools` as the sell
    /// leg targets.
    async fn simulate(
        &self,
        token: Address,
        holder: Address,
        amount: U256,
        pools: &[Address],
        block_tag: BlockNumberOrTag,
    ) -> Result<Simulation, String> {
        let tx = TransactionRequest::default()
            .from(holder)
            .to(holder)
            .input(TransactionInput::both(
                analyzeCall {
                    token,
                    amount,
                    settlement: self.settlement_contract,
                    recipient: arbitrary_recipient(),
                    pools: pools.to_vec(),
                }
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

        let raw: AlloyBytes = match self
            .rpc
            .eth_call_with_state_overrides(tx, block_tag, overrides)
            .await
        {
            Ok(raw) => raw,
            // A revert is caused by the token itself (e.g. balanceOf reverting), not by the
            // RPC: the injected Analyzer only propagates reverts raised by token calls.
            // Report it as a Bad verdict so callers stop retrying the token indefinitely.
            Err(e) if e.is_execution_reverted() => return Ok(Simulation::Reverted(e.to_string())),
            Err(e) => return Err(format!("eth_call with state overrides failed: {e}")),
        };

        analyzeCall::abi_decode_returns(raw.as_ref())
            .map(|returns| Simulation::Returned(Box::new(returns)))
            .map_err(|e| format!("Failed to decode Analyzer return value: {e}"))
    }

    fn handle_response(
        r: <analyzeCall as SolCall>::Return,
        amount: U256,
        holder: Address,
        pools: &[Address],
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

        // The settlement keeps `sellAmount` per sell leg and sends the rest to the recipient.
        // The Analyzer sizes the budget at half of what arrived, so neither subtraction
        // underflows and the multiplication does not overflow for a well-formed response.
        let sell_budget = r
            .sellAmount
            .checked_mul(U256::from(r.sellLegs.len()))
            .ok_or("sell budget overflow")?;
        // The Solidity guard ensures balanceAfterIn >= balanceBeforeIn when transferInOk = true.
        let middle_amount = r
            .balanceAfterIn
            .checked_sub(r.balanceBeforeIn)
            .and_then(|received| received.checked_sub(sell_budget))
            .ok_or("settlement balance underflow after successful transfer in")?;
        let expected_balance_after_out = r
            .balanceBeforeIn
            .checked_add(sell_budget)
            .ok_or("settlement balance overflow after transfer out")?;

        // A U256 overflow in the fee maths is token state (a balance near U256::MAX), not an RPC
        // fault. Both transfers ran, so gas is known; the fee is not.
        let fees = match calculate_fee_bps(
            ObservedTransfer {
                sent: amount,
                balance_before: r.balanceBeforeIn,
                balance_after: r.balanceAfterIn,
            },
            ObservedTransfer {
                sent: middle_amount,
                balance_before: r.recipientBefore,
                balance_after: r.recipientAfter,
            },
        ) {
            Ok(fees) => fees,
            Err(e) => {
                return Ok((
                    TokenQuality::bad(format!("Failed to calculate transfer fee: {e}")),
                    Some(gas_per_transfer),
                    None,
                ))
            }
        };
        let sell_fee = highest_sell_fee(&r, pools);
        let tax = sell_fee
            .as_ref()
            .map_or(fees, |sell| fees.max(sell.fee_bps));

        // Safe: calculate_fee_bps already checked this sum for overflow.
        let computed_balance_after_in = r.balanceBeforeIn + amount;
        if r.balanceAfterIn != computed_balance_after_in {
            return Ok((
                TokenQuality::bad(format!(
                    "Transferring {amount} into settlement was expected to result in a balance \
                     of {computed_balance_after_in} but got {}. The token likely takes a fee on \
                     transfer.",
                    r.balanceAfterIn,
                )),
                Some(gas_per_transfer),
                Some(tax),
            ));
        }

        // Without sell legs the settlement sends back exactly what it received, so share rounding
        // cancels out. With them it keeps the difference of two rounded amounts.
        let settlement_off_by = r
            .balanceAfterOut
            .abs_diff(expected_balance_after_out);
        let allowed =
            if sell_budget.is_zero() { U256::ZERO } else { U256::from(SELL_ROUNDING_WEI) };
        if settlement_off_by > allowed {
            return Ok((
                TokenQuality::bad(format!(
                    "Transferring {middle_amount} out of settlement was expected to leave a \
                     balance of {expected_balance_after_out} but got {}.",
                    r.balanceAfterOut,
                )),
                Some(gas_per_transfer),
                Some(tax),
            ));
        }

        // Safe: calculate_fee_bps already checked this sum for overflow.
        let computed_recipient_after = r.recipientBefore + middle_amount;
        if r.recipientAfter != computed_recipient_after {
            return Ok((
                TokenQuality::bad(format!(
                    "Transferring {amount} to arbitrary recipient {recipient:#x} was expected \
                     to result in a balance of {computed_recipient_after} but got {}. The token \
                     likely takes a fee on transfer.",
                    r.recipientAfter,
                )),
                Some(gas_per_transfer),
                Some(tax),
            ));
        }

        if let Some(sell) = sell_fee {
            return Ok((
                TokenQuality::bad(format!(
                    "Transferring {} into pool {:#x} was expected to result in a balance of {} \
                     but got {}. The token likely takes a fee on transfer.",
                    r.sellAmount,
                    sell.pool,
                    sell.leg.balanceBefore + r.sellAmount,
                    sell.leg.balanceAfter,
                )),
                Some(gas_per_transfer),
                Some(tax),
            ));
        }

        if !r.approvalOk {
            return Ok((
                TokenQuality::bad("Approval of U256::MAX failed".to_string()),
                Some(gas_per_transfer),
                Some(tax),
            ));
        }

        Ok((TokenQuality::Good, Some(gas_per_transfer), Some(tax)))
    }
}

/// The outcome of one Analyzer `eth_call`.
enum Simulation {
    Returned(Box<<analyzeCall as SolCall>::Return>),
    /// The token reverted the whole call. Holds the RPC error message.
    Reverted(String),
}

impl Simulation {
    /// Whether the holder could not move the token into the settlement or out of it.
    fn transfer_failed(&self) -> bool {
        match self {
            Simulation::Returned(r) => !r.transferInOk || !r.transferOutOk,
            Simulation::Reverted(_) => false,
        }
    }
}

fn to_u256(balance: &Bytes) -> U256 {
    U256::from_be_bytes::<32>(
        balance
            .lpad(32, 0)
            .as_ref()
            .try_into()
            .expect("balance should be 32 bytes"),
    )
}

/// Half of the holder's balance, to reduce races between the owner lookup and the eth_call, but
/// at least `MIN_AMOUNT`.
fn halve(balance: U256) -> U256 {
    cmp::max(balance / U256::from(2), U256::from(MIN_AMOUNT))
}

/// Returns the pools that receive a sell leg: the holder first, then `found` in order, without
/// duplicates or the zero address, at most `MAX_SELL_POOLS`.
fn candidate_pools(holder: Address, found: Vec<Address>) -> Vec<Address> {
    let mut pools = Vec::with_capacity(MAX_SELL_POOLS);
    for pool in std::iter::once(holder).chain(found) {
        if pools.len() == MAX_SELL_POOLS {
            break;
        }
        if !pool.is_zero() && !pools.contains(&pool) {
            pools.push(pool);
        }
    }
    pools
}

/// A sell leg whose pool received less than it was sent.
struct SellFee<'a> {
    pool: Address,
    leg: &'a SellLeg,
    fee_bps: U256,
}

/// Returns the sell leg with the highest non-zero fee, or `None` when every pool received the
/// full amount, give or take `SELL_ROUNDING_WEI`. A leg whose transfer failed, or whose fee
/// overflows U256, tells nothing about a tax and is skipped.
fn highest_sell_fee<'a>(
    r: &'a <analyzeCall as SolCall>::Return,
    pools: &[Address],
) -> Option<SellFee<'a>> {
    let mut highest: Option<SellFee> = None;
    for (pool, leg) in pools.iter().zip(&r.sellLegs) {
        if !leg.ok {
            tracing::debug!(?pool, "ethcall: sell leg into pool failed, skipping it");
            continue;
        }
        let transfer = ObservedTransfer {
            sent: r.sellAmount,
            balance_before: leg.balanceBefore,
            balance_after: leg.balanceAfter,
        };
        let fee_bps = match transfer.fee_bps() {
            Ok(fee_bps) => fee_bps,
            Err(e) => {
                tracing::debug!(?pool, e, "ethcall: sell leg fee overflows, skipping it");
                continue;
            }
        };
        // Safe: fee_bps already checked this sum for overflow.
        let shortfall = (leg.balanceBefore + r.sellAmount).saturating_sub(leg.balanceAfter);
        if shortfall <= U256::from(SELL_ROUNDING_WEI) {
            continue;
        }
        if !fee_bps.is_zero() &&
            highest
                .as_ref()
                .is_none_or(|h| fee_bps > h.fee_bps)
        {
            highest = Some(SellFee { pool: *pool, leg, fee_bps });
        }
    }
    highest
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr, sync::Arc};

    use alloy::primitives::{address, Address};
    use tycho_common::models::token::{TokenOwnerStore, TokenQuality};

    use super::*;
    use crate::test_fixtures::{TestFixture, TEST_BLOCK_NUMBER, TOKEN_HOLDERS, USDC_STR, WETH_STR};

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
            sellAmount: U256::ZERO,
            sellLegs: vec![],
        }
    }

    // Return value for a token without any tax, where the settlement kept `sell_amount` for each
    // of `pools` sell legs and every pool received it in full.
    fn good_return_with_sells(
        amount: U256,
        sell_amount: U256,
        pools: usize,
    ) -> <analyzeCall as SolCall>::Return {
        let budget = sell_amount * U256::from(pools);
        let mut r = good_return(amount);
        r.balanceAfterOut = budget;
        r.recipientAfter = amount - budget;
        r.sellAmount = sell_amount;
        r.sellLegs = (0..pools)
            .map(|i| SellLeg {
                ok: true,
                balanceBefore: U256::from(i * 7),
                balanceAfter: U256::from(i * 7) + sell_amount,
            })
            .collect();
        r
    }

    #[test]
    fn handle_response_good_token() {
        let amount = U256::from(1_000_000_u64);
        let result =
            EthCallDetector::handle_response(good_return(amount), amount, Address::ZERO, &[]);
        let (quality, gas, tax) = result.unwrap();
        assert_eq!(quality, TokenQuality::Good);
        assert_eq!(gas, Some(U256::from(27_500_u64))); // (30_000 + 25_000) / 2
        assert_eq!(tax, Some(U256::ZERO));
    }

    #[test]
    fn handle_response_sell_budget_stays_in_settlement() {
        let amount = U256::from(1_000_000_u64);
        let r = good_return_with_sells(amount, U256::from(125_000_u64), 4);
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
        assert_eq!(quality, TokenQuality::Good);
        assert_eq!(gas, Some(U256::from(27_500_u64)));
        assert_eq!(tax, Some(U256::ZERO));
    }

    #[rstest::rstest]
    #[case::one_wei_more(1)]
    #[case::two_wei_less(-2)]
    fn handle_response_settlement_rounding_with_sells_is_good(#[case] delta: i64) {
        // Share-based tokens can leave the settlement a wei off after it sends `received - budget`.
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return_with_sells(amount, U256::from(125_000_u64), 4);
        r.balanceAfterOut = U256::from(500_000_i64 + delta);
        let (quality, _, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
        assert_eq!(quality, TokenQuality::Good);
        assert_eq!(tax, Some(U256::ZERO));
    }

    #[test]
    fn handle_response_settlement_three_wei_off_with_sells_is_bad() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return_with_sells(amount, U256::from(125_000_u64), 4);
        r.balanceAfterOut = U256::from(500_003_u64);
        let (quality, _, _) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
    }

    #[test]
    fn handle_response_settlement_one_wei_off_without_sells_is_bad() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.balanceAfterOut = U256::from(1_u64);
        let (quality, _, _) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
    }

    #[test]
    fn handle_response_sell_budget_sent_to_recipient_is_bad() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return_with_sells(amount, U256::from(125_000_u64), 4);
        r.balanceAfterOut = U256::ZERO;
        r.recipientAfter = amount;
        let (quality, _, _) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
    }

    fn pools(n: usize) -> Vec<Address> {
        (1..=n)
            .map(|i| Address::with_last_byte(i as u8))
            .collect()
    }

    #[test]
    fn handle_response_sell_fee_is_bad_with_tax() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return_with_sells(amount, U256::from(125_000_u64), 4);
        // Pool 3 keeps only 95% of what it was sent: a 5% sell tax.
        r.sellLegs[2].balanceAfter = r.sellLegs[2].balanceBefore + U256::from(118_750_u64);
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &pools(4)).unwrap();
        let TokenQuality::Bad { reason } = quality else { panic!("expected Bad, got {quality:?}") };
        assert!(
            reason.contains(&format!("{:#x}", pools(4)[2])),
            "reason must name the pool: {reason}"
        );
        assert_eq!(gas, Some(U256::from(27_500_u64)));
        assert_eq!(tax, Some(U256::from(500_u64)));
    }

    #[test]
    fn handle_response_failed_sell_leg_is_ignored() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return_with_sells(amount, U256::from(125_000_u64), 4);
        r.sellLegs[1].ok = false;
        r.sellLegs[1].balanceAfter = r.sellLegs[1].balanceBefore;
        let (quality, _, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &pools(4)).unwrap();
        assert_eq!(quality, TokenQuality::Good);
        assert_eq!(tax, Some(U256::ZERO));
    }

    #[test]
    fn handle_response_pool_receiving_more_has_no_fee() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return_with_sells(amount, U256::from(125_000_u64), 4);
        // A swap-back adds tokens to the pool during the sell.
        r.sellLegs[0].balanceAfter += U256::from(40_000_u64);
        let (quality, _, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &pools(4)).unwrap();
        assert_eq!(quality, TokenQuality::Good);
        assert_eq!(tax, Some(U256::ZERO));
    }

    #[test]
    fn handle_response_sell_one_wei_short_has_no_fee() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return_with_sells(amount, U256::from(125_000_u64), 4);
        r.sellLegs[3].balanceAfter -= U256::from(1_u64);
        let (quality, _, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &pools(4)).unwrap();
        assert_eq!(quality, TokenQuality::Good);
        assert_eq!(tax, Some(U256::ZERO));
    }

    #[test]
    fn handle_response_small_sell_two_wei_short_has_no_fee() {
        // MIN_AMOUNT split over 8 pools: a 2 wei shortfall is 3 bps of a 6_250 wei slice.
        let amount = U256::from(100_000_u64);
        let mut r = good_return_with_sells(amount, U256::from(6_250_u64), 8);
        r.sellLegs[5].balanceAfter -= U256::from(2_u64);
        let (quality, _, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &pools(8)).unwrap();
        assert_eq!(quality, TokenQuality::Good);
        assert_eq!(tax, Some(U256::ZERO));
    }

    #[test]
    fn handle_response_small_sell_fee_is_bad() {
        let amount = U256::from(100_000_u64);
        let mut r = good_return_with_sells(amount, U256::from(6_250_u64), 8);
        // 1% of 6_250.
        r.sellLegs[5].balanceAfter -= U256::from(62_u64);
        let (quality, _, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &pools(8)).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert_eq!(tax, Some(U256::from(99_u64)));
    }

    #[test]
    fn handle_response_tax_is_max_of_all_legs() {
        // 1% inbound fee, then 2% and 3% sell taxes on two of the pools.
        let amount = U256::from(1_000_000_u64);
        let received = U256::from(990_000_u64);
        let sell_amount = U256::from(247_500_u64);
        let mut r = good_return_with_sells(received, sell_amount, 2);
        r.balanceAfterIn = received;
        r.sellLegs[0].balanceAfter = r.sellLegs[0].balanceBefore + U256::from(242_550_u64);
        r.sellLegs[1].balanceAfter = r.sellLegs[1].balanceBefore + U256::from(240_075_u64);
        let (quality, _, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &pools(2)).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert_eq!(tax, Some(U256::from(300_u64)));
    }

    #[test]
    fn candidate_pools_put_holder_first_and_drop_duplicates_and_zero() {
        let holder = Address::with_last_byte(9);
        let found = vec![
            Address::with_last_byte(1),
            holder,
            Address::ZERO,
            Address::with_last_byte(2),
            Address::with_last_byte(1),
        ];
        assert_eq!(
            candidate_pools(holder, found),
            vec![holder, Address::with_last_byte(1), Address::with_last_byte(2)]
        );
    }

    #[test]
    fn candidate_pools_keep_the_first_eight() {
        let holder = Address::with_last_byte(100);
        let found = pools(12);
        let mut expected = vec![holder];
        expected.extend(&found[..MAX_SELL_POOLS - 1]);
        assert_eq!(candidate_pools(holder, found), expected);
    }

    #[test]
    fn handle_response_transfer_in_failed() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.transferInOk = false;
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
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
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
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
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
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
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert!(gas.is_some());
        // Fee should be ~100 bps (1%)
        assert_eq!(tax, Some(U256::from(100_u64)));
    }

    #[test]
    fn handle_response_fee_on_transfer_with_settlement_balance() {
        // Settlement already holds 50_000 when the 1% fee token credits it with 990_000.
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.balanceBeforeIn = U256::from(50_000_u64);
        r.balanceAfterIn = U256::from(1_040_000_u64);
        r.balanceAfterOut = U256::from(50_000_u64);
        r.recipientAfter = U256::from(990_000_u64);
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert!(gas.is_some());
        assert_eq!(tax, Some(U256::from(100_u64)));
    }

    #[test]
    fn handle_response_fee_overflow_is_bad() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.balanceBeforeIn = U256::MAX;
        r.balanceAfterIn = U256::MAX;
        let (quality, gas, tax) = EthCallDetector::handle_response(r, amount, Address::ZERO, &[])
            .expect("a balance near U256::MAX must yield a Bad verdict, not an error");
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert_eq!(gas, Some(U256::from(27_500_u64)));
        assert!(tax.is_none());
    }

    #[test]
    fn handle_response_credits_more_than_sent_is_bad() {
        let amount = U256::from(1_000_000_u64);
        let mut r = good_return(amount);
        r.balanceAfterIn = amount + U256::from(1_u64);
        r.recipientAfter = amount + U256::from(1_u64);
        let (quality, gas, tax) =
            EthCallDetector::handle_response(r, amount, Address::ZERO, &[]).unwrap();
        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert_eq!(gas, Some(U256::from(27_500_u64)));
        assert_eq!(tax, Some(U256::ZERO));
    }

    impl TestFixture {
        pub(crate) fn create_ethcall_detector(&self) -> EthCallDetector {
            let rpc = self.create_rpc_client(false);
            let finder = TokenOwnerStore::new(TOKEN_HOLDERS.clone());
            EthCallDetector::new(&rpc, Arc::new(finder), COWSWAP_SETTLEMENT)
        }
    }

    #[tokio::test]
    async fn detect_impl_maps_execution_revert_to_bad() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":3,"message":"execution reverted","data":"0x"}}"#,
            )
            .expect(1)
            .create_async()
            .await;

        let rpc = EthereumRpcClient::new(&server.url()).expect("mock rpc client");
        let token = Bytes::from_str("e172e9b6cfbeeb5593bdce3f077356fdb33af904").unwrap();
        let holder = Bytes::from_str("000000000004444c5dc75cb358380d2e3de08a90").unwrap();
        let finder = TokenOwnerStore::new(HashMap::from([(
            token.clone(),
            (holder, U256::from(1_000_000_u64).to_bytes()),
        )]));
        let detector = EthCallDetector::new(&rpc, Arc::new(finder), COWSWAP_SETTLEMENT);

        let (quality, gas, tax) = detector
            .analyze(token, BlockTag::Latest)
            .await
            .expect("a reverting simulation must yield a Bad verdict, not an error");

        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert!(gas.is_none());
        assert!(tax.is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn detect_impl_sends_found_pools_to_the_analyzer() {
        let mut server = mockito::Server::new_async().await;
        let pool = "5555555555555555555555555555555555555555";
        let mock = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Regex(pool.to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":3,"message":"execution reverted","data":"0x"}}"#,
            )
            .expect(1)
            .create_async()
            .await;

        let rpc = EthereumRpcClient::new(&server.url()).expect("mock rpc client");
        let token = Bytes::from_str("e172e9b6cfbeeb5593bdce3f077356fdb33af904").unwrap();
        let holder = Bytes::from_str("000000000004444c5dc75cb358380d2e3de08a90").unwrap();
        let finder = TokenOwnerStore::with_pools(
            HashMap::from([(token.clone(), (holder, U256::from(1_000_000_u64).to_bytes()))]),
            HashMap::from([(
                token.clone(),
                vec![(Bytes::from_str(pool).unwrap(), U256::from(500_000_u64).to_bytes())],
            )]),
        );
        let detector = EthCallDetector::new(&rpc, Arc::new(finder), COWSWAP_SETTLEMENT);

        detector
            .analyze(token, BlockTag::Latest)
            .await
            .expect("the mock answers only a call that carries the pool");

        mock.assert_async().await;
    }

    const FIRST_HOLDER: &str = "1111111111111111111111111111111111111111";
    const NEXT_POOL: &str = "2222222222222222222222222222222222222222";

    // Answers an `analyze` eth_call sent from `holder` with `ret`, echoing the request id.
    async fn mock_analyze_from(
        server: &mut mockito::ServerGuard,
        holder: &str,
        ret: <analyzeCall as SolCall>::Return,
        hits: usize,
    ) -> mockito::Mock {
        let result = format!("0x{}", alloy::hex::encode(analyzeCall::abi_encode_returns(&ret)));
        server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Regex(format!(r#""from":"0x{holder}""#)))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": result})
                    .to_string()
                    .into()
            })
            .expect(hits)
            .create_async()
            .await
    }

    fn holder_and_next_pool(next_balance: u64) -> (Bytes, TokenOwnerStore) {
        let token = Bytes::from_str("e172e9b6cfbeeb5593bdce3f077356fdb33af904").unwrap();
        let holder = Bytes::from_str(FIRST_HOLDER).unwrap();
        let balance = U256::from(2_000_000_u64).to_bytes();
        let finder = TokenOwnerStore::with_pools(
            HashMap::from([(token.clone(), (holder.clone(), balance.clone()))]),
            HashMap::from([(
                token.clone(),
                vec![
                    (holder, balance),
                    (Bytes::from_str(NEXT_POOL).unwrap(), U256::from(next_balance).to_bytes()),
                ],
            )]),
        );
        (token, finder)
    }

    #[tokio::test]
    async fn detect_impl_retries_with_the_next_pool_when_the_holder_cannot_transfer() {
        let mut server = mockito::Server::new_async().await;
        let mut failed = good_return(U256::from(1_000_000_u64));
        failed.transferOutOk = false;
        let first = mock_analyze_from(&mut server, FIRST_HOLDER, failed, 1).await;
        // The next pool holds 400_000, so it sends 200_000.
        let next =
            mock_analyze_from(&mut server, NEXT_POOL, good_return(U256::from(200_000_u64)), 1)
                .await;
        let rpc = EthereumRpcClient::new(&server.url()).expect("mock rpc client");
        let (token, finder) = holder_and_next_pool(400_000);
        let detector = EthCallDetector::new(&rpc, Arc::new(finder), COWSWAP_SETTLEMENT);

        let (quality, _, tax) = detector
            .analyze(token, BlockTag::Latest)
            .await
            .unwrap();

        assert_eq!(quality, TokenQuality::Good);
        assert_eq!(tax, Some(0));
        first.assert_async().await;
        next.assert_async().await;
    }

    #[tokio::test]
    async fn detect_impl_does_not_retry_with_a_pool_below_the_minimum_amount() {
        let mut server = mockito::Server::new_async().await;
        let mut failed = good_return(U256::from(1_000_000_u64));
        failed.transferInOk = false;
        let first = mock_analyze_from(&mut server, FIRST_HOLDER, failed, 1).await;
        let rpc = EthereumRpcClient::new(&server.url()).expect("mock rpc client");
        let (token, finder) = holder_and_next_pool(99_999);
        let detector = EthCallDetector::new(&rpc, Arc::new(finder), COWSWAP_SETTLEMENT);

        let (quality, _, _) = detector
            .analyze(token, BlockTag::Latest)
            .await
            .unwrap();

        assert!(matches!(quality, TokenQuality::Bad { .. }));
        first.assert_async().await;
    }

    #[tokio::test]
    async fn detect_impl_does_not_retry_after_a_fee_verdict() {
        let mut server = mockito::Server::new_async().await;
        let amount = U256::from(1_000_000_u64);
        let mut fee = good_return(amount);
        fee.balanceAfterIn = U256::from(990_000_u64);
        fee.recipientAfter = U256::from(990_000_u64);
        let first = mock_analyze_from(&mut server, FIRST_HOLDER, fee, 1).await;
        let next = mock_analyze_from(&mut server, NEXT_POOL, good_return(amount), 0).await;
        let rpc = EthereumRpcClient::new(&server.url()).expect("mock rpc client");
        let (token, finder) = holder_and_next_pool(400_000);
        let detector = EthCallDetector::new(&rpc, Arc::new(finder), COWSWAP_SETTLEMENT);

        let (quality, _, tax) = detector
            .analyze(token, BlockTag::Latest)
            .await
            .unwrap();

        assert!(matches!(quality, TokenQuality::Bad { .. }));
        assert_eq!(tax, Some(100));
        first.assert_async().await;
        next.assert_async().await;
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
