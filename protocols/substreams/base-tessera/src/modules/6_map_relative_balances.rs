use crate::{common::*, config::DeploymentConfig};
use anyhow::{anyhow, Result};
use std::collections::{BTreeSet, HashSet};
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetProto, StoreGetString},
};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{
    abi::{erc20, weth},
    prelude::*,
};

fn balance(token: &[u8], owner: &[u8]) -> Result<BigInt> {
    if zero(owner) {
        return Ok(BigInt::zero());
    }
    erc20::functions::BalanceOf { owner: owner.to_vec() }
        .call(token.to_vec())
        .ok_or_else(|| anyhow!("balanceOf failed for {} at {}", id(token), id(owner)))
}
fn event_delta(log: &eth::v2::Log, owner: &[u8]) -> Option<BigInt> {
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
    if log.address != substreams::hex!("4200000000000000000000000000000000000006") {
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
trait BalanceSources {
    /// Component ids of every pair holding `token`.
    fn pairs_for_token(&self, token: &[u8]) -> Vec<String>;
    /// Every token held by at least one known pair.
    fn pair_tokens(&self) -> BTreeSet<Vec<u8>>;
    /// `token.balanceOf(owner)` at the end of the current block.
    fn balance(&self, token: &[u8], owner: &[u8]) -> Result<BigInt>;
}

struct StoreSources<'a> {
    components: &'a StoreGetProto<ProtocolComponent>,
    pair_store: &'a StoreGetString,
}

impl BalanceSources for StoreSources<'_> {
    fn pairs_for_token(&self, token: &[u8]) -> Vec<String> {
        pairs(self.pair_store, &format!("token:{}", id(token)))
    }

    fn pair_tokens(&self) -> BTreeSet<Vec<u8>> {
        let mut tokens = BTreeSet::new();
        for pair in pairs(self.pair_store, "pairs") {
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
    new_components: BlockTransactionProtocolComponents,
    components: StoreGetProto<ProtocolComponent>,
    pair_store: StoreGetString,
    treasury_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let config = DeploymentConfig::parse(&params)?;
    let owner = treasury_store
        .get_last("treasury")
        .map(hex::decode)
        .transpose()?
        .unwrap_or(config.treasury.clone());
    let sources = StoreSources { components: &components, pair_store: &pair_store };
    let balance_deltas = balance_deltas(&config, &block, new_components, &owner, &sources)?;
    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Treasury balance deltas for one block, duplicated under every pair holding each token.
///
/// A pair created in the block is seeded with the treasury's closing balance and receives no
/// event deltas for its seeded tokens in that block; other pairs holding those tokens still do.
/// Transfer and WETH events are matched against the custodian that opened the block. A treasury
/// rotation then adds, per token, the new custodian's closing balance minus the old one's.
/// Deltas are ordered by ordinal.
fn balance_deltas(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    owner: &[u8],
    sources: &impl BalanceSources,
) -> Result<Vec<BalanceDelta>> {
    let mut rotations: Vec<_> = block
        .transactions()
        .flat_map(|tx| {
            tx.calls
                .iter()
                .filter(|c| !c.state_reverted)
                .flat_map(move |c| {
                    c.storage_changes
                        .iter()
                        .map(move |w| (tx, w))
                })
        })
        .filter(|(_, w)| w.address == config.tesseraswap && w.key == slot(config.treasury_slot))
        .collect();
    rotations.sort_by_key(|(_, w)| w.ordinal);
    // Account the entire block against its opening custodian, then bridge end-of-block balances.
    let old_owner = rotations
        .first()
        .map(|(_, w)| address(&w.old_value))
        .unwrap_or(owner.to_vec());
    let mut seeded = HashSet::new();
    let mut deltas = vec![];
    for group in new_components.tx_components {
        let tx = group.tx.expect("component transaction");
        for c in group.components {
            for token in c.tokens {
                let amount = sources.balance(&token, owner)?;
                seeded.insert((token.clone(), c.id.clone()));
                deltas.push(BalanceDelta {
                    ord: tx.index,
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
            for pair in sources.pairs_for_token(log.address()) {
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
    if let Some((tx, write)) = rotations.last() {
        for token in sources.pair_tokens() {
            let adjustment =
                sources.balance(&token, owner)? - sources.balance(&token, &old_owner)?;
            for pair in sources.pairs_for_token(&token) {
                if seeded.contains(&(token.clone(), pair.clone())) {
                    continue;
                }
                deltas.push(BalanceDelta {
                    ord: write.ordinal,
                    tx: Some((*tx).into()),
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
    use keccak_hash::keccak;
    use std::collections::HashMap;

    const TREASURY: [u8; 20] = [0x31; 20];
    const NEW_TREASURY: [u8; 20] = [0x32; 20];
    const OUTSIDER: [u8; 20] = [0x39; 20];
    const USDC: [u8; 20] = [0x0c; 20];
    const BASE_TOKEN: [u8; 20] = [0x0b; 20];
    const UNHELD: [u8; 20] = [0x0e; 20];
    const WETH: [u8; 20] = substreams::hex!("4200000000000000000000000000000000000006");

    struct FakeSources {
        holders: HashMap<Vec<u8>, Vec<String>>,
        balances: HashMap<(Vec<u8>, Vec<u8>), i64>,
    }

    impl BalanceSources for FakeSources {
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

    fn config() -> DeploymentConfig {
        DeploymentConfig::parse(&format!(
            "tesseraswap={}&engine={}&treasury={}&treasury_slot=1&pair_map_slot=8\
             &pair_base_token_slot=48&pair_quote_token_slot=49&pair_lib_slot=51\
             &pair_write_helper_slot=52",
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
                keccak(b"Transfer(address,address,uint256)")
                    .as_bytes()
                    .to_vec(),
                word(from),
                word(to),
            ],
            data: slot(value),
            ordinal,
            ..Default::default()
        }
    }

    fn deposit(token: &[u8], dst: &[u8], value: u64, ordinal: u64) -> eth::v2::Log {
        eth::v2::Log {
            address: token.to_vec(),
            topics: vec![
                keccak(b"Deposit(address,uint256)")
                    .as_bytes()
                    .to_vec(),
                word(dst),
            ],
            data: slot(value),
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
    fn new_pair_seed_suppresses_only_its_own_events() {
        let sources = FakeSources {
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
            &sources,
        )
        .unwrap();

        assert_eq!(deltas_of(&deltas, "new", &USDC), vec![BigInt::from(500)]);
        assert_eq!(deltas_of(&deltas, "new", &BASE_TOKEN), vec![BigInt::from(7)]);
        assert_eq!(deltas_of(&deltas, "old", &USDC), vec![BigInt::from(50)]);
        assert!(deltas.iter().all(|d| d.token != UNHELD), "no pair holds this token");
    }

    #[test]
    fn rotation_matches_events_to_old_custodian_then_bridges() {
        let sources = FakeSources {
            holders: HashMap::from([(USDC.to_vec(), vec!["pair".to_string()])]),
            balances: HashMap::from([
                ((USDC.to_vec(), TREASURY.to_vec()), 80),
                ((USDC.to_vec(), NEW_TREASURY.to_vec()), 300),
            ]),
        };
        let rotation = eth::v2::StorageChange {
            address: config().tesseraswap,
            key: slot(1),
            old_value: word(&TREASURY),
            new_value: word(&NEW_TREASURY),
            ordinal: 20,
        };
        let block = block(vec![
            tx(0, vec![transfer(&USDC, &TREASURY, &OUTSIDER, 20, 5)], vec![]),
            tx(1, vec![], vec![rotation]),
        ]);
        // The treasury store already holds the new custodian by the time this map runs.
        let deltas = balance_deltas(
            &config(),
            &block,
            BlockTransactionProtocolComponents::default(),
            &NEW_TREASURY,
            &sources,
        )
        .unwrap();

        // Opening 100 − 20 moved out of the old custodian + (300 − 80) bridged = 300.
        assert_eq!(deltas_of(&deltas, "pair", &USDC), vec![BigInt::from(-20), BigInt::from(220)]);
    }

    #[test]
    fn weth_deposits_count_only_on_canonical_weth() {
        let sources = FakeSources {
            holders: HashMap::from([
                (WETH.to_vec(), vec!["pair".to_string()]),
                (OUTSIDER.to_vec(), vec!["pair".to_string()]),
            ]),
            balances: HashMap::new(),
        };
        let block = block(vec![tx(
            0,
            vec![deposit(&WETH, &TREASURY, 40, 1), deposit(&OUTSIDER, &TREASURY, 40, 2)],
            vec![],
        )]);
        let deltas = balance_deltas(
            &config(),
            &block,
            BlockTransactionProtocolComponents::default(),
            &TREASURY,
            &sources,
        )
        .unwrap();

        assert_eq!(deltas_of(&deltas, "pair", &WETH), vec![BigInt::from(40)]);
        assert!(deltas_of(&deltas, "pair", &OUTSIDER).is_empty());
    }
}
