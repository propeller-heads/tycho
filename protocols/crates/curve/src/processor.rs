//! [`CurveProcessor`]: the [`TxDeltaIndexer`] implementation.

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
};

use alloy::primitives::Address as AlloyAddress;
use num_bigint::BigInt;
use num_traits::ToPrimitive as _;
use revm::DatabaseRef;
use tracing::{debug, warn};
use tycho_common::{
    models::{
        blockchain::{BlockAggregatedChanges, PendingBlock},
        contract::AccountDelta,
        protocol::{ComponentBalance, ProtocolComponentStateDelta},
        Chain,
    },
    traits::TxDeltaIndexer,
    Bytes,
};
use tycho_simulation::evm::{
    engine_db::{
        engine_db_interface::EngineDatabaseInterface, tycho_db::PreCachedDB, SHARED_TYCHO_DB,
    },
    protocol::curve::{encode_readings, read_pool_readings, CurveVariant, POOL_STATE_ADJUSTED},
    simulation::SimulationEngine,
};

use crate::{
    balance::BalanceTracker,
    overrides::pending_overrides,
    registry::{normalize_id, PoolRegistry},
};

/// Derives the Curve deltas a pending block would produce.
///
/// Reads every affected pool's view getters against the pending block's accounts and emits the
/// readings as a [`POOL_STATE_ADJUSTED`] attribute, alongside the component balances the block's
/// transfer logs imply.
pub struct CurveProcessor<D: EngineDatabaseInterface + Clone + Debug>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    engine: SimulationEngine<D>,
    chain: Chain,
    extractor: String,
    finalized_block_height: u64,
    registry: PoolRegistry,
    balances: BalanceTracker,
}

impl CurveProcessor<PreCachedDB> {
    /// A processor reading the same indexed VM storage the stream decoder fills.
    ///
    /// This is the production wiring: confirmed pool storage arrives through the decoder, and
    /// only the pending block's own writes come from [`PendingBlock`].
    pub fn shared(chain: Chain, extractor: String) -> Self {
        Self::with_engine(chain, extractor, SimulationEngine::new(SHARED_TYCHO_DB.clone(), false))
    }
}

impl<D: EngineDatabaseInterface + Clone + Debug> CurveProcessor<D>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    /// A processor reading confirmed state from `engine`.
    ///
    /// The engine must have a current block set, which the pending block's own number and
    /// timestamp then override per call.
    pub fn with_engine(chain: Chain, extractor: String, engine: SimulationEngine<D>) -> Self {
        Self {
            engine,
            chain,
            extractor,
            finalized_block_height: 0,
            registry: PoolRegistry::default(),
            balances: BalanceTracker::default(),
        }
    }

    /// Number of pools the processor tracks.
    pub fn tracked_pools(&self) -> usize {
        self.registry.len()
    }

    /// The math variant resolved for a tracked component, if it is tracked.
    ///
    /// Which getters a pool exposes follows from its variant, so a caller re-reading the same
    /// pool needs the variant the processor settled on.
    pub fn pool_variant(&self, component_id: &str) -> Option<CurveVariant> {
        self.registry
            .get(component_id)
            .map(|pool| pool.variant)
    }

    /// The engine the pool getters read confirmed state from.
    ///
    /// Exposed for callers that own the database and decide which block it sits at. With the
    /// shared engine that block is maintained by the stream decoder, so production wiring never
    /// needs this.
    pub fn engine_mut(&mut self) -> &mut SimulationEngine<D> {
        &mut self.engine
    }
}

impl<D: EngineDatabaseInterface + Clone + Debug> TxDeltaIndexer for CurveProcessor<D>
where
    D: Send,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    /// Advances the pool registry and the confirmed component balances.
    ///
    /// Components without a `coins` attribute or a resolvable variant are skipped rather than
    /// failing the block: their getters cannot be read, so tracking them would only produce
    /// per-pool read failures later.
    fn apply_block(&mut self, block: &BlockAggregatedChanges) -> anyhow::Result<()> {
        self.chain = block.chain;
        self.finalized_block_height = block.finalized_block_height;

        for (id, component) in &block.new_protocol_components {
            if !self
                .registry
                .register(id, component, &self.engine)
            {
                debug!(
                    component = %id,
                    "curve component skipped: no usable coins or unresolved variant"
                );
            }
        }

        for id in block.deleted_protocol_components.keys() {
            self.registry.remove(id);
            self.balances
                .forget_pool(&normalize_id(id));
        }

        for (id, token_balances) in &block.component_balances {
            let key = normalize_id(id);
            for (token, balance) in token_balances {
                self.balances
                    .set(key.clone(), token.clone(), &balance.balance);
            }
        }

        Ok(())
    }

    /// Reads the state `pending` would leave behind, for every tracked pool it touches.
    ///
    /// A pool whose getters fail is dropped with a warning; every other pool still produces its
    /// delta. Internal state is untouched, so repeated calls with the same pending block return
    /// the same result.
    fn generate_deltas(&mut self, pending: &PendingBlock) -> BlockAggregatedChanges {
        if pending.accounts().is_empty() && !pending.txs().is_empty() {
            // Curve's state is in contract storage, not in its logs. With no post-execution
            // accounts there is nothing to override, so every pool would read confirmed state
            // and every quote would silently be one block stale.
            warn!(
                extractor = %self.extractor,
                txs = pending.txs().len(),
                "pending block carries no accounts; no curve state deltas can be derived"
            );
        }

        let overrides = pending_overrides(pending);
        let touched: Vec<AlloyAddress> = pending
            .accounts()
            .keys()
            .filter_map(as_address)
            .collect();

        let mut state_deltas: HashMap<String, ProtocolComponentStateDelta> = HashMap::new();
        for pool in self
            .registry
            .affected_by(touched.iter())
        {
            let readings = match read_pool_readings(
                &self.engine,
                &pool.address,
                pool.variant,
                pool.n_coins,
                &overrides,
            ) {
                Ok(readings) => readings,
                Err(error) => {
                    warn!(pool = %pool.id, %error, "curve pending read failed; pool dropped");
                    continue;
                }
            };
            let encoded = match encode_readings(&readings) {
                Ok(encoded) => encoded,
                Err(error) => {
                    warn!(pool = %pool.id, %error, "curve readings encode failed; pool dropped");
                    continue;
                }
            };
            state_deltas.insert(
                pool.id.clone(),
                ProtocolComponentStateDelta {
                    component_id: pool.id.clone(),
                    updated_attributes: HashMap::from([(POOL_STATE_ADJUSTED.to_string(), encoded)]),
                    ..Default::default()
                },
            );
        }

        BlockAggregatedChanges {
            extractor: self.extractor.clone(),
            chain: self.chain,
            block: pending.block().clone(),
            finalized_block_height: self.finalized_block_height,
            state_deltas,
            component_balances: self.pending_balances(pending),
            account_deltas: self.tracked_account_deltas(pending),
            ..Default::default()
        }
    }
}

impl<D: EngineDatabaseInterface + Clone + Debug> CurveProcessor<D>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    /// Absolute component balances after `pending`, keyed by emitted component id.
    ///
    /// ERC20 balances come from the block's transfer logs on top of the confirmed running total,
    /// matching the substreams byte for byte. A pool's native ETH balance is taken from the
    /// account itself: the substreams derive it from per-call balance changes in the block trace,
    /// which the logs in a [`PendingBlock`] do not carry, and the account's own post-execution
    /// balance is the more direct answer anyway.
    fn pending_balances(
        &self,
        pending: &PendingBlock,
    ) -> HashMap<String, HashMap<Bytes, ComponentBalance>> {
        let mut balances: HashMap<String, HashMap<Bytes, ComponentBalance>> = HashMap::new();

        for change in self
            .balances
            .pending(&self.registry, pending.txs())
        {
            let Some(pool) = self.registry.get(&change.pool_key) else { continue };
            insert_balance(
                &mut balances,
                &pool.id,
                change.token,
                &change.balance,
                change.modify_tx,
            );
        }

        for (address, delta) in pending.accounts() {
            let Some(balance) = &delta.balance else { continue };
            let key = normalize_id(&hex::encode(address.as_ref()));
            let Some(pool) = self.registry.get(&key) else { continue };
            if !pool.holds_native_eth() {
                continue;
            }
            insert_balance(
                &mut balances,
                &pool.id,
                Bytes::from(vec![0u8; 20]),
                &BigInt::from_bytes_be(num_bigint::Sign::Plus, balance.as_ref()),
                Bytes::default(),
            );
        }

        balances
    }

    /// The pending block's account changes, restricted to contracts a tracked pool depends on.
    ///
    /// Mirrors the substreams' `extract_contract_changes` filter. Nothing in the pending path
    /// consumes these today — `CurveState` rebuilds from the state attribute instead — but they
    /// are what the confirmed stream carries for this protocol, so the output stays complete.
    fn tracked_account_deltas(&self, pending: &PendingBlock) -> HashMap<Bytes, AccountDelta> {
        let mut tracked: HashSet<AlloyAddress> = HashSet::new();
        for address in pending.accounts().keys() {
            if let Some(address) = as_address(address) {
                if self.registry.is_tracked(&address) {
                    tracked.insert(address);
                }
            }
        }

        pending
            .accounts()
            .iter()
            .filter(|(address, _)| {
                as_address(address).is_some_and(|address| tracked.contains(&address))
            })
            .map(|(address, delta)| (address.clone(), delta.clone()))
            .collect()
    }
}

fn insert_balance(
    balances: &mut HashMap<String, HashMap<Bytes, ComponentBalance>>,
    component_id: &str,
    token: Bytes,
    balance: &BigInt,
    modify_tx: Bytes,
) {
    // Signed, unclamped: the Curve substreams emit `to_signed_bytes_be()` of the running total.
    let encoded = Bytes::from(balance.to_signed_bytes_be());
    let balance_float = balance.to_f64().unwrap_or(f64::MAX);
    balances
        .entry(component_id.to_string())
        .or_default()
        .insert(
            token.clone(),
            ComponentBalance {
                token,
                balance: encoded,
                balance_float,
                modify_tx,
                component_id: component_id.to_string(),
            },
        );
}

fn as_address(bytes: &Bytes) -> Option<AlloyAddress> {
    (bytes.len() == 20).then(|| AlloyAddress::from_slice(bytes.as_ref()))
}

#[cfg(test)]
mod tests {
    use tycho_common::models::{
        blockchain::{Block, LogInput, TxInput},
        protocol::ProtocolComponent,
        ChangeType,
    };
    use tycho_simulation::evm::engine_db::tycho_db::PreCachedDB;

    use super::*;

    /// 3pool: its variant resolves from the legacy address table, so no VM probing is needed.
    const POOL: &str = "0xbebc44782c7db0a1a60cb6fe97d0b483032ff1c7";
    const DAI: [u8; 20] = [0x6bu8; 20];
    const USDC: [u8; 20] = [0xa0u8; 20];

    /// A processor whose engine has no current block, so every pool read fails. That isolates
    /// the parts of `generate_deltas` that do not depend on the VM.
    fn processor() -> CurveProcessor<PreCachedDB> {
        CurveProcessor::with_engine(
            Chain::Ethereum,
            "vm:curve".to_string(),
            SimulationEngine::new(PreCachedDB::new().unwrap(), false),
        )
    }

    fn component() -> ProtocolComponent {
        ProtocolComponent {
            id: POOL.to_string(),
            tokens: vec![Bytes::from(DAI.to_vec()), Bytes::from(USDC.to_vec())],
            static_attributes: HashMap::from([(
                "coins".to_string(),
                Bytes::from(
                    format!(r#"["0x{}","0x{}"]"#, hex::encode(DAI), hex::encode(USDC)).into_bytes(),
                ),
            )]),
            ..Default::default()
        }
    }

    fn confirmed_block(balances: HashMap<Bytes, ComponentBalance>) -> BlockAggregatedChanges {
        BlockAggregatedChanges {
            chain: Chain::Ethereum,
            finalized_block_height: 100,
            new_protocol_components: HashMap::from([(POOL.to_string(), component())]),
            component_balances: HashMap::from([(POOL.to_string(), balances)]),
            ..Default::default()
        }
    }

    fn confirmed_balance(token: [u8; 20], balance: u8) -> HashMap<Bytes, ComponentBalance> {
        let token = Bytes::from(token.to_vec());
        HashMap::from([(
            token.clone(),
            ComponentBalance {
                token,
                balance: Bytes::from(vec![balance]),
                balance_float: balance as f64,
                modify_tx: Bytes::default(),
                component_id: POOL.to_string(),
            },
        )])
    }

    fn pending_block(txs: Vec<TxInput>, accounts: HashMap<Bytes, AccountDelta>) -> PendingBlock {
        let block = Block {
            number: 101,
            chain: Chain::Ethereum,
            hash: Bytes::from(vec![9u8; 32]),
            parent_hash: Bytes::from(vec![8u8; 32]),
            ts: chrono::DateTime::from_timestamp(1_759_842_947, 0)
                .unwrap()
                .naive_utc(),
        };
        PendingBlock::new(block, txs, accounts)
    }

    fn account(address: Bytes) -> (Bytes, AccountDelta) {
        (
            address.clone(),
            AccountDelta::new(
                Chain::Ethereum,
                address,
                HashMap::from([(Bytes::from(vec![1]), Some(Bytes::from(vec![2])))]),
                None,
                None,
                ChangeType::Update,
            ),
        )
    }

    /// A transaction whose single log transfers `value` DAI from `from` to `to`.
    fn transfer_tx(from: &Bytes, to: &Bytes, value: u8) -> TxInput {
        let mut topic = vec![0u8; 32];
        topic.copy_from_slice(
            alloy::primitives::keccak256(b"Transfer(address,address,uint256)").as_slice(),
        );
        let pad = |address: &Bytes| {
            let mut padded = vec![0u8; 12];
            padded.extend_from_slice(address.as_ref());
            Bytes::from(padded)
        };
        let mut amount = vec![0u8; 32];
        amount[31] = value;
        TxInput::new(
            Bytes::from(vec![7u8; 32]),
            Bytes::from(vec![0u8; 20]),
            Bytes::from(vec![0u8; 20]),
            0,
            vec![LogInput::new(
                Bytes::from(DAI.to_vec()),
                vec![Bytes::from(topic), pad(from), pad(to)],
                Bytes::from(amount),
                0,
            )],
            true,
        )
    }

    fn pool_address() -> Bytes {
        Bytes::from(hex::decode(POOL.trim_start_matches("0x")).unwrap())
    }

    #[test]
    fn test_apply_block_registers_pools_and_seeds_balances() {
        let mut processor = processor();

        processor
            .apply_block(&confirmed_block(confirmed_balance(DAI, 100)))
            .expect("apply_block failed");

        assert_eq!(processor.tracked_pools(), 1);

        // The seeded balance must be the base the pending transfer is applied to.
        let pending = processor.generate_deltas(&pending_block(
            vec![transfer_tx(&Bytes::from(USDC.to_vec()), &pool_address(), 5)],
            HashMap::new(),
        ));
        let balance = pending
            .component_balances
            .get(POOL)
            .and_then(|tokens| tokens.get(&Bytes::from(DAI.to_vec())))
            .expect("balance for the transferred token");
        assert_eq!(balance.balance, Bytes::from(vec![105]), "100 confirmed + 5 received");
    }

    #[test]
    fn test_deleted_components_stop_being_tracked() {
        let mut processor = processor();
        processor
            .apply_block(&confirmed_block(confirmed_balance(DAI, 100)))
            .expect("apply_block failed");

        let mut removal = BlockAggregatedChanges { chain: Chain::Ethereum, ..Default::default() };
        removal
            .deleted_protocol_components
            .insert(POOL.to_string(), component());
        processor
            .apply_block(&removal)
            .expect("apply_block failed");

        assert_eq!(processor.tracked_pools(), 0);
        let pending = processor.generate_deltas(&pending_block(
            vec![transfer_tx(&Bytes::from(USDC.to_vec()), &pool_address(), 5)],
            HashMap::from([account(pool_address())]),
        ));
        assert!(pending.component_balances.is_empty(), "a removed pool emits nothing");
        assert!(pending.account_deltas.is_empty());
    }

    #[test]
    fn test_block_metadata_comes_from_the_pending_block() {
        let mut processor = processor();
        processor
            .apply_block(&confirmed_block(HashMap::new()))
            .expect("apply_block failed");

        let pending = processor.generate_deltas(&pending_block(vec![], HashMap::new()));

        assert_eq!(pending.block.number, 101, "the pending block, not the confirmed parent");
        assert_eq!(pending.block.hash, Bytes::from(vec![9u8; 32]));
        assert_eq!(pending.extractor, "vm:curve");
        assert_eq!(pending.finalized_block_height, 100, "carried from the last confirmed block");
        assert_eq!(pending.chain, Chain::Ethereum);
    }

    #[test]
    fn test_account_deltas_are_restricted_to_tracked_contracts() {
        let mut processor = processor();
        processor
            .apply_block(&confirmed_block(HashMap::new()))
            .expect("apply_block failed");
        let stranger = Bytes::from(vec![0x99u8; 20]);

        let pending = processor.generate_deltas(&pending_block(
            vec![],
            HashMap::from([account(pool_address()), account(stranger.clone())]),
        ));

        assert!(pending
            .account_deltas
            .contains_key(&pool_address()));
        assert!(
            !pending
                .account_deltas
                .contains_key(&stranger),
            "an untracked contract must not leak into the output"
        );
    }

    /// A pool whose getters cannot be read is dropped, and the rest of the output still stands.
    /// Here every read fails because the engine has no block set.
    #[test]
    fn test_unreadable_pools_are_dropped_without_losing_balances() {
        let mut processor = processor();
        processor
            .apply_block(&confirmed_block(confirmed_balance(DAI, 100)))
            .expect("apply_block failed");

        let pending = processor.generate_deltas(&pending_block(
            vec![transfer_tx(&Bytes::from(USDC.to_vec()), &pool_address(), 5)],
            HashMap::from([account(pool_address())]),
        ));

        assert!(pending.state_deltas.is_empty(), "the unreadable pool produced no state delta");
        assert!(
            !pending.component_balances.is_empty(),
            "balances come from logs and must survive a failed state read"
        );
    }

    #[test]
    fn test_generate_deltas_is_repeatable() {
        let mut processor = processor();
        processor
            .apply_block(&confirmed_block(confirmed_balance(DAI, 100)))
            .expect("apply_block failed");
        let block = pending_block(
            vec![transfer_tx(&Bytes::from(USDC.to_vec()), &pool_address(), 5)],
            HashMap::from([account(pool_address())]),
        );

        let first = processor.generate_deltas(&block);
        let second = processor.generate_deltas(&block);

        assert_eq!(first.component_balances, second.component_balances);
        assert_eq!(first.account_deltas, second.account_deltas);
    }
}
