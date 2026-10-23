use crate::{common::*, config::DeploymentConfig, pb::tessera::v1::BlockStorageChanges};
use anyhow::{anyhow, Result};
use std::collections::{BTreeSet, HashSet};
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetArray, StoreGetProto, StoreGetRaw},
};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{
    abi::{erc20, weth},
    prelude::*,
};

const BASE_WETH: [u8; 20] = substreams::hex!("4200000000000000000000000000000000000006");

fn balance(token: &[u8], owner: &[u8]) -> Result<BigInt> {
    if zero(owner) {
        return Ok(BigInt::zero());
    }
    erc20::functions::BalanceOf { owner: owner.to_vec() }
        .call(token.to_vec())
        .ok_or_else(|| anyhow!("balanceOf failed for {} at {}", id(token), id(owner)))
}

/// How much one log changes `owner`'s token balance, or `None` if it does not touch it.
///
/// `tycho_substreams::balances::extract_balance_deltas_from_tx` decodes the same events, but it
/// takes `Deposit`/`Withdrawal` from any contract and its predicate cannot see which event
/// matched, so it cannot keep wrap and unwrap to Base's canonical WETH.
fn event_delta(log: &eth::v2::Log, owner: &[u8]) -> Option<BigInt> {
    // A zero owner means no treasury, as in `balance`; matching it would count every mint as
    // an outflow and every burn as an inflow.
    if zero(owner) {
        return None;
    }
    if let Some(erc20::events::Transfer { from, to, value }) =
        erc20::events::Transfer::match_and_decode(log)
    {
        return match (from == owner, to == owner) {
            (true, true) => Some(BigInt::zero()),
            (true, false) => Some(value.neg()),
            (false, true) => Some(value),
            _ => None,
        };
    }
    // Only Base's canonical WETH emits wrap/unwrap events in this accounting model.
    if log.address != BASE_WETH {
        return None;
    }
    if let Some(weth::events::Deposit { dst, wad }) = weth::events::Deposit::match_and_decode(log) {
        return (dst == owner).then_some(wad);
    }
    if let Some(weth::events::Withdrawal { src, wad }) =
        weth::events::Withdrawal::match_and_decode(log)
    {
        return (src == owner).then_some(wad.neg());
    }
    None
}

/// Store and RPC reads the treasury accounting depends on.
trait TreasuryInputs {
    /// Component ids of every pair holding `token`.
    fn pairs_for_token(&self, token: &[u8]) -> Vec<String>;
    /// Every token held by at least one known pair.
    fn pair_tokens(&self) -> BTreeSet<Vec<u8>>;
    /// `token.balanceOf(owner)` at the end of the current block.
    fn balance(&self, token: &[u8], owner: &[u8]) -> Result<BigInt>;
}

struct StoreInputs<'a> {
    components: &'a StoreGetProto<ProtocolComponent>,
    pair_store: &'a StoreGetArray<String>,
}

impl TreasuryInputs for StoreInputs<'_> {
    fn pairs_for_token(&self, token: &[u8]) -> Vec<String> {
        self.pair_store
            .get_last(format!("token:{}", id(token)))
            .unwrap_or_default()
    }

    fn pair_tokens(&self) -> BTreeSet<Vec<u8>> {
        let mut tokens = BTreeSet::new();
        for pair in self
            .pair_store
            .get_last("pairs")
            .unwrap_or_default()
        {
            if let Some(c) = self.components.get_last(pair) {
                tokens.extend(c.tokens);
            }
        }
        tokens
    }

    fn balance(&self, token: &[u8], owner: &[u8]) -> Result<BigInt> {
        balance(token, owner)
    }
}

#[substreams::handlers::map]
pub fn map_relative_balances(
    params: String,
    block: eth::v2::Block,
    storage: BlockStorageChanges,
    new_components: BlockTransactionProtocolComponents,
    components: StoreGetProto<ProtocolComponent>,
    pair_store: StoreGetArray<String>,
    safety_store: StoreGetRaw,
) -> Result<BlockBalanceDeltas> {
    let config = DeploymentConfig::parse(&params)?;
    // Balance RPCs read the closing block state, so seeds must use its closing custodian.
    // Rotation accounting below recovers the opening custodian from the first slot write.
    let owner = safety_store
        .get_last("treasury")
        .unwrap_or_else(|| config.treasury.to_vec());
    let inputs = StoreInputs { components: &components, pair_store: &pair_store };
    let balance_deltas =
        balance_deltas_from_storage(&config, &block, &storage, new_components, &owner, &inputs)?;
    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Treasury balance deltas for one block, duplicated under every pair holding each token.
///
/// A pair created in the block is seeded with the treasury's closing balance and receives no
/// event deltas for its seeded tokens in that block; other pairs holding those tokens still do.
/// Transfer and WETH events are matched against the custodian that opened the block. A treasury
/// rotation then adds, per token, the new custodian's closing balance minus the old one's.
/// Deltas are ordered by ordinal.
fn balance_deltas_from_storage(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    storage: &BlockStorageChanges,
    new_components: BlockTransactionProtocolComponents,
    owner: &[u8],
    inputs: &impl TreasuryInputs,
) -> Result<Vec<BalanceDelta>> {
    let mut rotations = vec![];
    for group in &storage.transactions {
        let tx = group.tx()?;
        for w in &group.writes {
            if *w.address == config.tesseraswap && *w.key == TESSERASWAP_TREASURY_SLOT {
                rotations.push((tx, w));
            }
        }
    }
    rotations.sort_by_key(|(_, w)| w.ordinal);
    // Account the entire block against its opening custodian, then bridge end-of-block balances.
    let old_owner = rotations
        .first()
        .map(|(_, w)| address(&w.old_value).to_vec())
        .unwrap_or_else(|| owner.to_vec());
    let mut seeded = HashSet::new();
    let mut deltas = vec![];
    for group in new_components.tx_components {
        let tx = group
            .tx
            .ok_or_else(|| anyhow!("new components without a transaction"))?;
        // Balance aggregation groups consecutive deltas by transaction, so the seed must sort
        // inside its transaction's execution-ordinal range, never by transaction index.
        let ord = block
            .transaction_traces
            .iter()
            .find(|trace| u64::from(trace.index) == tx.index)
            .map(|trace| trace.begin_ordinal)
            .ok_or_else(|| {
                anyhow!("component transaction {} is missing from the block", tx.index)
            })?;
        for c in group.components {
            for token in c.tokens {
                let amount = inputs.balance(&token, owner)?;
                seeded.insert((token.clone(), c.id.clone()));
                deltas.push(BalanceDelta {
                    ord,
                    tx: Some(tx.clone()),
                    token,
                    component_id: c.id.as_bytes().to_vec(),
                    delta: amount.to_signed_bytes_be(),
                });
            }
        }
    }
    for log in block.logs() {
        if let Some(amount) = event_delta(log.log, &old_owner) {
            for pair in inputs.pairs_for_token(log.address()) {
                if seeded.contains(&(log.address().to_vec(), pair.clone())) {
                    continue;
                }
                deltas.push(BalanceDelta {
                    ord: log.ordinal(),
                    tx: Some(log.receipt.transaction.into()),
                    token: log.address().to_vec(),
                    component_id: pair.into_bytes(),
                    delta: amount.to_signed_bytes_be(),
                });
            }
        }
    }
    // Multiple rotations collapse to one opening-to-closing bridge. Intermediate owners
    // must not contribute extra bridges: both balance RPCs already include the whole block.
    // Attach the adjustment to the final rotation transaction and skip end-block seeds.
    if let Some((tx, write)) = rotations.last() {
        for token in inputs.pair_tokens() {
            let adjustment = inputs.balance(&token, owner)? - inputs.balance(&token, &old_owner)?;
            for pair in inputs.pairs_for_token(&token) {
                if seeded.contains(&(token.clone(), pair.clone())) {
                    continue;
                }
                deltas.push(BalanceDelta {
                    ord: write.ordinal,
                    tx: Some((*tx).clone()),
                    token: token.clone(),
                    component_id: pair.into_bytes(),
                    delta: adjustment.to_signed_bytes_be(),
                });
            }
        }
    }
    deltas.sort_by_key(|d| d.ord);
    Ok(deltas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::{slot, storage_changes};
    use alloy_primitives::keccak256;
    use std::collections::HashMap;

    const TREASURY: [u8; 20] = [0x31; 20];
    const NEW_TREASURY: [u8; 20] = [0x32; 20];
    const OUTSIDER: [u8; 20] = [0x39; 20];
    const USDC: [u8; 20] = [0x0c; 20];
    const BASE_TOKEN: [u8; 20] = [0x0b; 20];
    const UNHELD: [u8; 20] = [0x0e; 20];

    struct FakeInputs {
        holders: HashMap<Vec<u8>, Vec<String>>,
        balances: HashMap<(Vec<u8>, Vec<u8>), i64>,
    }

    impl TreasuryInputs for FakeInputs {
        fn pairs_for_token(&self, token: &[u8]) -> Vec<String> {
            self.holders
                .get(token)
                .cloned()
                .unwrap_or_default()
        }

        fn pair_tokens(&self) -> BTreeSet<Vec<u8>> {
            self.holders.keys().cloned().collect()
        }

        fn balance(&self, token: &[u8], owner: &[u8]) -> Result<BigInt> {
            let key = (token.to_vec(), owner.to_vec());
            Ok(BigInt::from(*self.balances.get(&key).unwrap_or(&0)))
        }
    }

    fn balance_deltas(
        config: &DeploymentConfig,
        block: &eth::v2::Block,
        new_components: BlockTransactionProtocolComponents,
        owner: &[u8],
        inputs: &impl TreasuryInputs,
    ) -> Result<Vec<BalanceDelta>> {
        let storage = storage_changes(block);
        balance_deltas_from_storage(config, block, &storage, new_components, owner, inputs)
    }

    fn config() -> DeploymentConfig {
        DeploymentConfig::parse(&format!(
            "tesseraswap={}&engine={}&treasury={}",
            hex::encode([0x11; 20]),
            hex::encode([0x22; 20]),
            hex::encode(TREASURY),
        ))
        .unwrap()
    }

    fn word(addr: &[u8]) -> Vec<u8> {
        let mut w = vec![0; 32];
        w[12..].copy_from_slice(addr);
        w
    }

    fn transfer(token: &[u8], from: &[u8], to: &[u8], value: u64, ordinal: u64) -> eth::v2::Log {
        eth::v2::Log {
            address: token.to_vec(),
            topics: vec![
                keccak256(b"Transfer(address,address,uint256)").to_vec(),
                word(from),
                word(to),
            ],
            data: slot(value).to_vec(),
            ordinal,
            ..Default::default()
        }
    }

    fn deposit(token: &[u8], dst: &[u8], value: u64, ordinal: u64) -> eth::v2::Log {
        eth::v2::Log {
            address: token.to_vec(),
            topics: vec![keccak256(b"Deposit(address,uint256)").to_vec(), word(dst)],
            data: slot(value).to_vec(),
            ordinal,
            ..Default::default()
        }
    }

    fn tx(
        index: u32,
        logs: Vec<eth::v2::Log>,
        writes: Vec<eth::v2::StorageChange>,
    ) -> eth::v2::TransactionTrace {
        eth::v2::TransactionTrace {
            status: 1,
            index,
            receipt: Some(eth::v2::TransactionReceipt { logs, ..Default::default() }),
            calls: vec![eth::v2::Call { storage_changes: writes, ..Default::default() }],
            ..Default::default()
        }
    }

    fn block(transactions: Vec<eth::v2::TransactionTrace>) -> eth::v2::Block {
        eth::v2::Block { transaction_traces: transactions, ..Default::default() }
    }

    fn created(pair: &str, tokens: &[&[u8]], tx_index: u64) -> BlockTransactionProtocolComponents {
        BlockTransactionProtocolComponents {
            tx_components: vec![TransactionProtocolComponents {
                tx: Some(Transaction { index: tx_index, ..Default::default() }),
                components: vec![ProtocolComponent {
                    id: pair.to_string(),
                    tokens: tokens
                        .iter()
                        .map(|t| t.to_vec())
                        .collect(),
                    ..Default::default()
                }],
            }],
        }
    }

    /// The signed deltas emitted for one (pair, token), in emission order.
    fn deltas_of(deltas: &[BalanceDelta], pair: &str, token: &[u8]) -> Vec<BigInt> {
        let mut values = vec![];
        for d in deltas {
            if d.component_id == pair.as_bytes() && d.token == token {
                values.push(BigInt::from_signed_bytes_be(&d.delta));
            }
        }
        values
    }

    /// Transaction indexes in emission order, one entry per run of consecutive deltas.
    fn tx_runs(deltas: &[BalanceDelta]) -> Vec<u64> {
        let mut runs = vec![];
        for d in deltas {
            let index = d.tx.as_ref().unwrap().index;
            if runs.last() != Some(&index) {
                runs.push(index);
            }
        }
        runs
    }

    #[test]
    fn self_transfer_nets_to_zero() {
        assert_eq!(
            event_delta(&transfer(&USDC, &TREASURY, &TREASURY, 100, 0), &TREASURY),
            Some(BigInt::zero())
        );
        assert_eq!(
            event_delta(&transfer(&USDC, &TREASURY, &OUTSIDER, 100, 0), &TREASURY),
            Some(BigInt::from(-100))
        );
    }

    #[test]
    fn zero_treasury_matches_no_events() {
        let none = [0; 20];
        // Mints come from the zero address and burns go to it; neither moves a treasury.
        assert_eq!(event_delta(&transfer(&USDC, &none, &OUTSIDER, 100, 0), &none), None);
        assert_eq!(event_delta(&transfer(&USDC, &OUTSIDER, &none, 100, 0), &none), None);
        assert_eq!(event_delta(&deposit(&BASE_WETH, &none, 40, 0), &none), None);
    }

    #[test]
    fn new_pair_seed_suppresses_only_its_own_events() {
        let inputs = FakeInputs {
            holders: HashMap::from([
                (USDC.to_vec(), vec!["old".to_string(), "new".to_string()]),
                (BASE_TOKEN.to_vec(), vec!["new".to_string()]),
            ]),
            balances: HashMap::from([
                ((USDC.to_vec(), TREASURY.to_vec()), 500),
                ((BASE_TOKEN.to_vec(), TREASURY.to_vec()), 7),
            ]),
        };
        let block = block(vec![
            tx(0, vec![], vec![]),
            tx(
                1,
                vec![
                    transfer(&USDC, &OUTSIDER, &TREASURY, 50, 10),
                    transfer(&UNHELD, &OUTSIDER, &TREASURY, 9, 11),
                ],
                vec![],
            ),
        ]);
        let deltas = balance_deltas(
            &config(),
            &block,
            created("new", &[&USDC, &BASE_TOKEN], 0),
            &TREASURY,
            &inputs,
        )
        .unwrap();

        assert_eq!(deltas_of(&deltas, "new", &USDC), vec![BigInt::from(500)]);
        assert_eq!(deltas_of(&deltas, "new", &BASE_TOKEN), vec![BigInt::from(7)]);
        assert_eq!(deltas_of(&deltas, "old", &USDC), vec![BigInt::from(50)]);
        assert!(deltas.iter().all(|d| d.token != UNHELD), "no pair holds this token");
    }

    #[test]
    fn seed_stays_inside_its_transaction_ordinal_range() {
        let inputs = FakeInputs {
            holders: HashMap::from([
                (USDC.to_vec(), vec!["old".to_string(), "new".to_string()]),
                (BASE_TOKEN.to_vec(), vec!["old".to_string()]),
            ]),
            balances: HashMap::from([((USDC.to_vec(), TREASURY.to_vec()), 500)]),
        };
        // Transaction indexes are far below execution ordinals, so an index-based seed for
        // tx 47 would sort between tx 3's deltas and split them.
        let swap = eth::v2::TransactionTrace {
            begin_ordinal: 30,
            end_ordinal: 60,
            ..tx(
                3,
                vec![
                    transfer(&USDC, &OUTSIDER, &TREASURY, 50, 40),
                    transfer(&BASE_TOKEN, &TREASURY, &OUTSIDER, 8, 55),
                ],
                vec![],
            )
        };
        let creation = eth::v2::TransactionTrace {
            begin_ordinal: 100,
            end_ordinal: 120,
            ..tx(47, vec![transfer(&USDC, &OUTSIDER, &TREASURY, 6, 110)], vec![])
        };
        let deltas = balance_deltas(
            &config(),
            &block(vec![swap, creation]),
            created("new", &[&USDC], 47),
            &TREASURY,
            &inputs,
        )
        .unwrap();

        // Balance aggregation groups consecutive deltas by transaction.
        assert_eq!(tx_runs(&deltas), vec![3, 47]);
        assert_eq!(deltas_of(&deltas, "old", &USDC), vec![BigInt::from(50), BigInt::from(6)]);
        assert_eq!(deltas_of(&deltas, "old", &BASE_TOKEN), vec![BigInt::from(-8)]);
        assert_eq!(deltas_of(&deltas, "new", &USDC), vec![BigInt::from(500)]);
    }

    #[test]
    fn seed_for_transaction_missing_from_block_returns_an_error() {
        let inputs = FakeInputs { holders: HashMap::new(), balances: HashMap::new() };
        let error = balance_deltas(
            &config(),
            &block(vec![tx(0, vec![], vec![])]),
            created("new", &[&USDC], 5),
            &TREASURY,
            &inputs,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("transaction 5"));
    }

    #[test]
    fn treasury_slot_writes_outside_tesseraswap_are_not_rotations() {
        let inputs = FakeInputs {
            holders: HashMap::from([(USDC.to_vec(), vec!["pair".to_string()])]),
            balances: HashMap::from([
                ((USDC.to_vec(), TREASURY.to_vec()), 80),
                ((USDC.to_vec(), NEW_TREASURY.to_vec()), 300),
            ]),
        };
        // The Engine's writes reach this module, and its slot 1 is not the treasury.
        let engine_write = eth::v2::StorageChange {
            address: config().engine.to_vec(),
            key: slot(1).to_vec(),
            old_value: word(&TREASURY),
            new_value: word(&NEW_TREASURY),
            ordinal: 20,
        };
        let block = block(vec![
            tx(0, vec![transfer(&USDC, &TREASURY, &OUTSIDER, 20, 5)], vec![]),
            tx(1, vec![], vec![engine_write]),
        ]);
        let deltas = balance_deltas(
            &config(),
            &block,
            BlockTransactionProtocolComponents::default(),
            &TREASURY,
            &inputs,
        )
        .unwrap();

        assert_eq!(deltas_of(&deltas, "pair", &USDC), vec![BigInt::from(-20)]);
    }

    #[test]
    fn weth_deposits_count_only_on_canonical_weth() {
        let inputs = FakeInputs {
            holders: HashMap::from([
                (BASE_WETH.to_vec(), vec!["pair".to_string()]),
                (OUTSIDER.to_vec(), vec!["pair".to_string()]),
            ]),
            balances: HashMap::new(),
        };
        let block = block(vec![tx(
            0,
            vec![deposit(&BASE_WETH, &TREASURY, 40, 1), deposit(&OUTSIDER, &TREASURY, 40, 2)],
            vec![],
        )]);
        let deltas = balance_deltas(
            &config(),
            &block,
            BlockTransactionProtocolComponents::default(),
            &TREASURY,
            &inputs,
        )
        .unwrap();

        assert_eq!(deltas_of(&deltas, "pair", &BASE_WETH), vec![BigInt::from(40)]);
        assert!(deltas_of(&deltas, "pair", &OUTSIDER).is_empty());
    }

    #[test]
    fn multiple_rotations_bridge_once_to_closing_owner_and_skip_new_pair_seed() {
        let inputs = FakeInputs {
            holders: HashMap::from([(USDC.to_vec(), vec!["existing".into(), "new".into()])]),
            balances: HashMap::from([
                ((USDC.to_vec(), TREASURY.to_vec()), 80),
                ((USDC.to_vec(), NEW_TREASURY.to_vec()), 300),
                // An intermediate custodian must not affect the final bridge.
                ((USDC.to_vec(), OUTSIDER.to_vec()), 999),
            ]),
        };
        let rotation = |old: &[u8], new: &[u8], ordinal| eth::v2::StorageChange {
            address: config().tesseraswap.to_vec(),
            key: slot(1).to_vec(),
            old_value: word(old),
            new_value: word(new),
            ordinal,
        };
        let block = block(vec![
            tx(0, vec![transfer(&USDC, &TREASURY, &OUTSIDER, 20, 5)], vec![]),
            tx(1, vec![], vec![rotation(&TREASURY, &OUTSIDER, 20)]),
            tx(2, vec![], vec![rotation(&OUTSIDER, &NEW_TREASURY, 30)]),
        ]);
        let deltas =
            balance_deltas(&config(), &block, created("new", &[&USDC], 1), &NEW_TREASURY, &inputs)
                .unwrap();
        assert_eq!(
            deltas_of(&deltas, "existing", &USDC),
            vec![BigInt::from(-20), BigInt::from(220)]
        );
        assert_eq!(deltas_of(&deltas, "new", &USDC), vec![BigInt::from(300)]);
        let bridge = deltas
            .iter()
            .find(|d| d.ord == 30)
            .unwrap();
        assert_eq!(bridge.tx.as_ref().unwrap().index, 2);
    }
}
