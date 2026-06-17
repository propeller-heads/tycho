use std::collections::{HashMap, HashSet};

use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive as _;
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges, LogInput, TxInput},
        protocol::{ComponentBalance, ProtocolComponentStateDelta},
        Chain,
    },
    traits::TxDeltaIndexer,
    Bytes,
};
use tycho_substreams::prelude::{
    Attribute, BalanceChange, ChangeType, EntityChanges, Transaction as SubstreamsTx,
    TransactionChanges, TransactionChangesBuilder,
};

use crate::{
    balance::event_to_balances,
    events::{decode_log, Pool, PoolEvent, TxRef},
    output::event_to_attribute_updates,
};

#[derive(Clone)]
pub struct UniswapV2Processor {
    chain: Chain,
    extractor: String,
    last_block: Option<Block>,
    finalized_block_height: u64,
    pools: HashMap<String, Pool>,
}

impl TxDeltaIndexer for UniswapV2Processor {
    fn apply_block(&mut self, block: &BlockAggregatedChanges) {
        self.chain = block.chain;
        self.last_block = Some(block.block.clone());
        self.finalized_block_height = block.finalized_block_height;

        // UniswapV2 `Sync` events carry absolute reserves, so `generate_deltas` is fully
        // described by each event plus the pool's token list. The only state to reconstruct
        // is the pool registry — reserves and balances need no running accumulators.
        for (id, comp) in &block.new_protocol_components {
            if comp.tokens.len() >= 2 {
                let key = normalize_id(id);
                self.pools.insert(
                    key.clone(),
                    Pool {
                        address: hex::decode(&key).unwrap_or_default(),
                        token0: comp.tokens[0].to_vec(),
                        token1: comp.tokens[1].to_vec(),
                    },
                );
            }
        }

        for id in block.deleted_protocol_components.keys() {
            self.pools.remove(&normalize_id(id));
        }
    }

    /// Applies a batch of in-flight transactions against the current state and returns the
    /// protocol state deltas they would produce.
    ///
    /// Component ids in the output use the same format the substreams packages emit
    /// ("0x"-prefixed lower-case hex), so they match decoder state keys downstream.
    ///
    /// Works on a clone of internal state so repeated calls with the same (or different)
    /// transactions always produce results relative to the last `apply_block` call.
    fn generate_deltas(&mut self, txs: &[TxInput]) -> BlockAggregatedChanges {
        let mut scratch = self.clone();
        let tx_changes = scratch.build_tx_changes(txs);

        let mut state_deltas: HashMap<String, ProtocolComponentStateDelta> = HashMap::new();
        let mut component_balances: HashMap<String, HashMap<Bytes, ComponentBalance>> =
            HashMap::new();

        for changes in tx_changes {
            let tx_hash = changes
                .tx
                .as_ref()
                .map(|t| Bytes::from(t.hash.clone()))
                .unwrap_or_default();

            for ec in changes.entity_changes {
                let component_id = emitted_id(&ec.component_id);
                let delta = state_deltas
                    .entry(component_id.clone())
                    .or_insert_with(|| ProtocolComponentStateDelta {
                        component_id: component_id.clone(),
                        updated_attributes: HashMap::new(),
                        deleted_attributes: HashSet::new(),
                    });
                for attr in ec.attributes {
                    if attr.change == i32::from(ChangeType::Deletion) {
                        delta
                            .deleted_attributes
                            .insert(attr.name.clone());
                        delta
                            .updated_attributes
                            .remove(&attr.name);
                    } else {
                        delta
                            .updated_attributes
                            .insert(attr.name.clone(), Bytes::from(attr.value));
                        delta
                            .deleted_attributes
                            .remove(&attr.name);
                    }
                }
            }

            for bc in changes.balance_changes {
                let comp_id = emitted_id(&hex::encode(&bc.component_id));
                let token = Bytes::from(bc.token);
                let balance = Bytes::from(bc.balance);
                let balance_float = BigInt::from_bytes_be(Sign::Plus, balance.as_ref())
                    .to_f64()
                    .unwrap_or(f64::MAX);
                component_balances
                    .entry(comp_id.clone())
                    .or_default()
                    .insert(
                        token.clone(),
                        ComponentBalance {
                            token,
                            balance,
                            balance_float,
                            modify_tx: tx_hash.clone(),
                            component_id: comp_id,
                        },
                    );
            }
        }

        BlockAggregatedChanges {
            extractor: self.extractor.clone(),
            chain: self.chain,
            block: self.pending_block(),
            finalized_block_height: self.finalized_block_height,
            state_deltas,
            component_balances,
            ..Default::default()
        }
    }
}

impl UniswapV2Processor {
    pub fn new(chain: Chain, extractor: String) -> Self {
        Self {
            chain,
            extractor,
            last_block: None,
            finalized_block_height: 0,
            pools: HashMap::new(),
        }
    }

    /// Constructs the pending-block descriptor for `generate_deltas` output.
    ///
    /// Number is `last_block + 1`; hash is zeroed because the block has not
    /// been mined yet.
    fn pending_block(&self) -> Block {
        match &self.last_block {
            Some(b) => Block {
                number: b.number + 1,
                hash: Bytes::default(),
                parent_hash: b.hash.clone(),
                chain: b.chain,
                ts: b.ts,
            },
            None => Block::default(),
        }
    }

    fn build_tx_changes(&mut self, txs: &[TxInput]) -> Vec<TransactionChanges> {
        let mut tx_builders: HashMap<Vec<u8>, (u64, TransactionChangesBuilder)> = HashMap::new();

        for tx in txs {
            if !tx.succeeded() {
                continue;
            }

            let tx_ref = TxRef {
                hash: tx.hash().to_vec(),
                from: tx.from().to_vec(),
                to: tx.to().to_vec(),
                index: tx.index(),
            };

            let mut events: Vec<PoolEvent> = Vec::new();
            for log in tx.logs() {
                let pool_hex = hex::encode(log.address().as_ref());
                let Some(pool) = self.pools.get(&pool_hex) else { continue };
                let ordinal = tx.index() * 100_000 + log.log_index() as u64;
                let pb_log = log_input_to_pb(log, ordinal);
                if let Some(event) = decode_log(&pb_log, pool, &tx_ref) {
                    events.push(event);
                }
            }

            if events.is_empty() {
                continue;
            }

            tx_builders
                .entry(tx.hash().to_vec())
                .or_insert_with(|| {
                    let substreams_tx = SubstreamsTx {
                        hash: tx.hash().to_vec(),
                        from: tx.from().to_vec(),
                        to: tx.to().to_vec(),
                        index: tx.index(),
                    };
                    (tx.index(), TransactionChangesBuilder::new(&substreams_tx))
                });

            for event in events {
                let (_, builder) = tx_builders
                    .get_mut(tx.hash().as_ref())
                    .expect("builder inserted above");
                Self::apply_event(event, builder);
            }
        }

        let mut ordered: Vec<(u64, TransactionChangesBuilder)> =
            tx_builders.into_values().collect();
        ordered.sort_unstable_by_key(|(idx, _)| *idx);
        ordered
            .into_iter()
            .filter_map(|(_, b)| b.build())
            .collect()
    }

    fn apply_event(event: PoolEvent, builder: &mut TransactionChangesBuilder) {
        let pool_hex = hex::encode(&event.pool_address);

        for balance in event_to_balances(&event) {
            builder.add_balance_change(&BalanceChange {
                component_id: event.pool_address.clone(),
                token: balance.token,
                balance: balance.balance,
            });
        }

        for attr_update in event_to_attribute_updates(&event) {
            builder.add_entity_change(&EntityChanges {
                component_id: pool_hex.clone(),
                attributes: vec![Attribute {
                    name: attr_update.name,
                    value: attr_update.value,
                    change: ChangeType::Update.into(),
                }],
            });
        }
    }
}

/// Canonical internal key for a component id: lower-case hex without the "0x" prefix.
fn normalize_id(id: &str) -> String {
    id.trim_start_matches("0x")
        .to_lowercase()
}

/// Formats a canonical internal key into the id format the substreams packages emit
/// ("0x"-prefixed lower-case hex).
fn emitted_id(canonical_hex: &str) -> String {
    format!("0x{canonical_hex}")
}

fn log_input_to_pb(log: &LogInput, ordinal: u64) -> substreams_ethereum::pb::eth::v2::Log {
    substreams_ethereum::pb::eth::v2::Log {
        address: log.address().to_vec(),
        topics: log
            .topics()
            .iter()
            .map(|t| t.to_vec())
            .collect(),
        data: log.data().to_vec(),
        ordinal,
        ..Default::default()
    }
}
