//! Which tracked components a block's `Transfer` events moved, and by how much.

use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetRaw},
};
use substreams_ethereum::{pb::eth::v2 as eth, Event};
use tycho_substreams::{
    abi::{erc20, weth},
    prelude::*,
};

use crate::{
    consts::{PENDLE_SY, WETH},
    keys::{contract_id, market_tokens_key},
};

/// One component's side of a `Transfer` of a token it declares.
pub struct ComponentTransfer<'a> {
    pub tx: &'a eth::TransactionTrace,
    pub log: &'a eth::Log,
    pub component_id: String,
    /// Signed: positive when the component received the token.
    pub amount: BigInt,
}

/// Every side of every `Transfer` in `block` that moves a token a component declares, markets and
/// SYs alike.
pub fn component_transfers<'a>(
    block: &'a eth::Block,
    components_store: &StoreGetRaw,
) -> Vec<ComponentTransfer<'a>> {
    let mut transfers = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            for (holder, amount) in log_holder_deltas(log) {
                let component_id = contract_id(&holder);
                let Some(tokens) = components_store.get_last(&component_id) else { continue };
                let tokens: Vec<Vec<u8>> = serde_sibor::from_bytes(&tokens)
                    .expect("deserializing component tokens from the component store");
                if !tokens.contains(&log.address) {
                    continue;
                }
                transfers.push(ComponentTransfer { tx, log, component_id, amount });
            }
        }
    }
    transfers
}

/// Whether `component_id` is a market rather than an SY. Only markets have their token roles
/// stored.
pub fn is_market(components_store: &StoreGetRaw, component_id: &str) -> bool {
    components_store.has_last(market_tokens_key(component_id))
}

/// One change to an SY's supply: a mint or a burn of its shares.
pub struct SupplyChange<'a> {
    pub tx: &'a eth::TransactionTrace,
    pub log: &'a eth::Log,
    pub sy_id: String,
    /// Signed: positive for a mint, negative for a burn.
    pub amount: BigInt,
}

/// Every mint and burn of an SY component's shares in `block`: a `Transfer` the SY itself emits
/// from or to the zero address.
pub fn supply_changes<'a>(
    block: &'a eth::Block,
    components_store: &StoreGetRaw,
) -> Vec<SupplyChange<'a>> {
    let mut changes = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(transfer) = erc20::events::Transfer::match_and_decode(log) else { continue };
            let Some(amount) = supply_delta(&transfer) else { continue };
            let sy_id = contract_id(&log.address);
            if !components_store.has_last(&sy_id) || is_market(components_store, &sy_id) {
                continue;
            }
            changes.push(SupplyChange { tx, log, sy_id, amount });
        }
    }
    changes
}

/// The supply change a transfer makes: its value for a mint, minus its value for a burn, `None`
/// for a transfer between holders or of nothing.
fn supply_delta(transfer: &erc20::events::Transfer) -> Option<BigInt> {
    if transfer.value == BigInt::zero() {
        return None;
    }
    match (is_zero_address(&transfer.from), is_zero_address(&transfer.to)) {
        (true, false) => Some(transfer.value.clone()),
        (false, true) => Some(transfer.value.neg()),
        (true, true) | (false, false) => None,
    }
}

/// Every SY component created in `components`, with the transaction that created it.
pub fn new_sy_components(
    components: &BlockTransactionProtocolComponents,
) -> Vec<(&Transaction, &ProtocolComponent)> {
    let mut created = Vec::new();
    for tx_components in &components.tx_components {
        let tx = tx_components
            .tx
            .as_ref()
            .expect("component batch without a transaction");
        for component in &tx_components.components {
            let is_sy = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_SY);
            if is_sy {
                created.push((tx, component));
            }
        }
    }
    created
}

/// The signed balance change a log makes to each holder: a `Transfer`'s two sides, or WETH9's
/// `Deposit` and `Withdrawal`, which wrap and unwrap ether without a `Transfer`. An SY taking
/// WETH and unwrapping it to stake ether would otherwise keep that WETH on its books.
///
/// The two WETH events are honoured from WETH9 only. Another token emitting events of the same
/// shape alongside its own mint and burn `Transfer`s would be counted twice.
fn log_holder_deltas(log: &eth::Log) -> Vec<(Vec<u8>, BigInt)> {
    if let Some(transfer) = erc20::events::Transfer::match_and_decode(log) {
        return holder_deltas(&transfer);
    }
    if log.address.as_slice() != WETH {
        return vec![];
    }
    if let Some(deposit) = weth::events::Deposit::match_and_decode(log) {
        if deposit.wad != BigInt::zero() {
            return vec![(deposit.dst, deposit.wad)];
        }
    } else if let Some(withdrawal) = weth::events::Withdrawal::match_and_decode(log) {
        if withdrawal.wad != BigInt::zero() {
            return vec![(withdrawal.src, withdrawal.wad.neg())];
        }
    }
    vec![]
}

/// Splits a transfer into the signed change it makes to each side's balance.
///
/// Both sides are returned: a transfer between two tracked components moves both, and stopping at
/// the first match would leave the other permanently overstated. Mints and burns name the zero
/// address, which is never a component, so that side is dropped here rather than costing a store
/// lookup.
///
/// A transfer whose sender and receiver are the same address yields nothing. The two deltas would
/// net to zero, but they would carry the same component, token and ordinal, and
/// `store_balance_changes` panics on an ordinal it has already seen for that pair — which would
/// halt the module for that block and every block after it.
fn holder_deltas(transfer: &erc20::events::Transfer) -> Vec<(Vec<u8>, BigInt)> {
    if transfer.value == BigInt::zero() || transfer.from == transfer.to {
        return vec![];
    }
    let mut deltas = Vec::new();
    if !is_zero_address(&transfer.to) {
        deltas.push((transfer.to.clone(), transfer.value.clone()));
    }
    if !is_zero_address(&transfer.from) {
        deltas.push((transfer.from.clone(), transfer.value.neg()));
    }
    deltas
}

fn is_zero_address(address: &[u8]) -> bool {
    address.iter().all(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKET: [u8; 20] = [0x34; 20];
    const SY: [u8; 20] = [0xcb; 20];
    const ZERO: [u8; 20] = [0x00; 20];

    fn transfer(from: [u8; 20], to: [u8; 20], value: i64) -> erc20::events::Transfer {
        erc20::events::Transfer { from: from.to_vec(), to: to.to_vec(), value: BigInt::from(value) }
    }

    /// A transfer between two tracked components moves both balances. Stopping at the first
    /// match — as the factory template does — leaves the sender permanently overstated.
    #[test]
    fn a_transfer_moves_both_sides() {
        assert_eq!(
            holder_deltas(&transfer(SY, MARKET, 100)),
            vec![(MARKET.to_vec(), BigInt::from(100)), (SY.to_vec(), BigInt::from(-100))]
        );
    }

    #[test]
    fn a_mint_only_credits_the_receiver() {
        assert_eq!(
            holder_deltas(&transfer(ZERO, MARKET, 100)),
            vec![(MARKET.to_vec(), BigInt::from(100))]
        );
    }

    #[test]
    fn a_burn_only_debits_the_sender() {
        assert_eq!(
            holder_deltas(&transfer(MARKET, ZERO, 100)),
            vec![(MARKET.to_vec(), BigInt::from(-100))]
        );
    }

    /// Zero-value transfers are legal ERC-20 and some routers emit them. They would contribute
    /// nothing but still occupy an ordinal in the aggregation.
    #[test]
    fn a_zero_transfer_moves_nothing() {
        assert!(holder_deltas(&transfer(SY, MARKET, 0)).is_empty());
    }

    /// A self-transfer emits nothing at all. Emitting the two halves that net to zero would put
    /// two deltas for one component, token and ordinal into the aggregation, which panics on a
    /// repeated ordinal rather than netting them.
    #[test]
    fn a_self_transfer_emits_nothing() {
        assert!(holder_deltas(&transfer(MARKET, MARKET, 100)).is_empty());
    }

    /// The aggregation the module feeds rejects a repeated ordinal for one component and token,
    /// so no single log may ever produce two deltas that collide on all three.
    #[test]
    fn no_log_yields_two_deltas_for_the_same_component_and_token() {
        for (from, to) in [(MARKET, MARKET), (SY, MARKET), (ZERO, MARKET), (MARKET, ZERO)] {
            let deltas = holder_deltas(&transfer(from, to, 100));
            let mut holders: Vec<&Vec<u8>> = deltas
                .iter()
                .map(|(holder, _)| holder)
                .collect();
            holders.sort();
            let count = holders.len();
            holders.dedup();
            assert_eq!(holders.len(), count, "duplicate holder for a transfer {from:?} -> {to:?}");
        }
    }

    /// WETH9 `Deposit(address indexed dst, uint wad)` and `Withdrawal(address indexed src, uint
    /// wad)`, by topic0 as mainnet logs carry them.
    const WETH_DEPOSIT: &str = "e1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c";
    const WETH_WITHDRAWAL: &str =
        "7fcf532c15f0a6db0bd6d0e038bea71d30d808c7d98cb3bf7268a95bf5081b65";

    fn weth_event(address: [u8; 20], topic0: &str, holder: [u8; 20], wad: u64) -> eth::Log {
        let mut holder_topic = vec![0u8; 12];
        holder_topic.extend_from_slice(&holder);
        let mut data = vec![0u8; 24];
        data.extend_from_slice(&wad.to_be_bytes());
        eth::Log {
            address: address.to_vec(),
            topics: vec![hex::decode(topic0).unwrap(), holder_topic],
            data,
            ..Default::default()
        }
    }

    /// An SY that takes WETH and unwraps it to stake ether emits no `Transfer` for the unwrap, only
    /// WETH's `Withdrawal`. Without it the WETH stays on the SY's books for good.
    #[test]
    fn weth_wraps_and_unwraps_move_the_holder() {
        assert_eq!(
            log_holder_deltas(&weth_event(WETH, WETH_WITHDRAWAL, SY, 100)),
            vec![(SY.to_vec(), BigInt::from(-100))]
        );
        assert_eq!(
            log_holder_deltas(&weth_event(WETH, WETH_DEPOSIT, SY, 100)),
            vec![(SY.to_vec(), BigInt::from(100))]
        );
    }

    /// Another token emitting events of the same shape is not WETH, and may also emit `Transfer`s
    /// for the same movement, which would then be counted twice.
    #[test]
    fn deposit_and_withdrawal_count_only_from_weth() {
        assert!(log_holder_deltas(&weth_event(MARKET, WETH_WITHDRAWAL, SY, 100)).is_empty());
        assert!(log_holder_deltas(&weth_event(MARKET, WETH_DEPOSIT, SY, 100)).is_empty());
    }

    #[test]
    fn a_mint_raises_supply_and_a_burn_lowers_it() {
        assert_eq!(supply_delta(&transfer(ZERO, MARKET, 100)), Some(BigInt::from(100)));
        assert_eq!(supply_delta(&transfer(MARKET, ZERO, 100)), Some(BigInt::from(-100)));
    }

    #[test]
    fn a_transfer_between_holders_leaves_supply_alone() {
        assert_eq!(supply_delta(&transfer(SY, MARKET, 100)), None);
        assert_eq!(supply_delta(&transfer(ZERO, MARKET, 0)), None);
    }
}
