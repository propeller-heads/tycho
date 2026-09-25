//! Builds the [`EntityCache`] from a state snapshot.

use std::{collections::HashMap, time::Instant};

use metrics::gauge;
use tracing::info;
use tycho_common::{
    models::{Address, Chain, ComponentId, ProtocolSystem},
    storage::{
        AccountSnapshot, ComponentSnapshot, StateSnapshot, StateSnapshotGateway, StorageError,
    },
};

use super::cache::{AccountWriteTimestamps, CachedAccount, CachedComponentState, EntityCache};

impl EntityCache {
    /// Builds the cache from the live state of `chain`: one database snapshot, read once through
    /// `gateway`. The gateway is used for the read and not kept; after this call the cache never
    /// touches the database.
    ///
    /// # Errors
    ///
    /// `StorageError` when the snapshot read fails. No cache exists after an error.
    pub async fn load(
        gateway: &impl StateSnapshotGateway,
        chain: Chain,
    ) -> Result<Self, StorageError> {
        let started = Instant::now();
        let snapshot = gateway.state_snapshot(chain).await?;
        let cache = Self::from_snapshot(snapshot);
        let elapsed = started.elapsed();
        let (accounts, components) = cache.entry_counts();
        gauge!("entity_cache_load_seconds").set(elapsed.as_secs_f64());
        gauge!("entity_cache_accounts").set(accounts as f64);
        gauge!("entity_cache_components").set(components as f64);
        info!(accounts, components, ?elapsed, "Entity cache loaded");
        Ok(cache)
    }

    /// Builds the cache from one snapshot: every row becomes an entry stamped with the block that
    /// wrote it.
    pub(crate) fn from_snapshot(snapshot: StateSnapshot) -> Self {
        let StateSnapshot { accounts, components } = snapshot;
        let mut account_entries: HashMap<Address, CachedAccount> =
            HashMap::with_capacity(accounts.len());
        for row in accounts {
            account_entries.insert(row.account.address.clone(), account_entry(row));
        }
        let mut component_entries: HashMap<
            ProtocolSystem,
            HashMap<ComponentId, CachedComponentState>,
        > = HashMap::new();
        for row in components {
            component_entries
                .entry(row.system.clone())
                .or_default()
                .insert(row.state.component_id.clone(), component_entry(row));
        }
        Self::from_entries(account_entries, component_entries)
    }
}

fn account_entry(row: AccountSnapshot) -> CachedAccount {
    let timestamps = AccountWriteTimestamps {
        slots: row.slot_written_at,
        native_balance: row.native_balance_written_at,
        code: row.code_written_at,
        token_balances: row.token_balance_written_at,
    };
    CachedAccount::from_snapshot(row.account, timestamps)
}

fn component_entry(row: ComponentSnapshot) -> CachedComponentState {
    CachedComponentState::from_snapshot(row.state, row.updated_at)
}

#[cfg(test)]
mod test {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tycho_common::{
        models::{contract::Account, protocol::ProtocolComponentState, Chain},
        storage::{StateSnapshotGateway, WriteTimestamp},
        Bytes,
    };

    use super::*;
    use crate::{
        services::state::window::FoldSink,
        testing::{self, aggregated_changes, with_state_delta},
    };

    const EXTRACTOR: &str = "ex";

    fn addr(n: u64) -> Bytes {
        Bytes::from(n).lpad(20, 0)
    }

    fn account_snapshot(block: u64) -> AccountSnapshot {
        let at = WriteTimestamp::from(&testing::block(block));
        let code = Bytes::from("0x6000");
        let slot = Bytes::from(1u64).lpad(32, 0);
        let account = Account::new(
            Chain::Ethereum,
            addr(1),
            "a".to_string(),
            HashMap::from([(slot.clone(), Bytes::from(1u64).lpad(32, 0))]),
            Bytes::from(10u64),
            HashMap::new(),
            code.clone(),
            tycho_common::keccak256(&code).into(),
            Bytes::zero(32),
            Bytes::from("0x02"),
            None,
        );
        AccountSnapshot {
            account,
            slot_written_at: HashMap::from([(slot, at)]),
            native_balance_written_at: at,
            code_written_at: at,
            token_balance_written_at: HashMap::new(),
        }
    }

    fn component_snapshot(block: u64) -> ComponentSnapshot {
        ComponentSnapshot {
            system: EXTRACTOR.to_string(),
            state: ProtocolComponentState::new(
                "c1",
                HashMap::from([("x".to_string(), Bytes::from(1u64))]),
                HashMap::new(),
            ),
            updated_at: WriteTimestamp::from(&testing::block(block)),
        }
    }

    #[test]
    fn from_snapshot_builds_entries_that_read_back() {
        let snapshot = StateSnapshot {
            accounts: vec![account_snapshot(5)],
            components: vec![component_snapshot(5)],
        };

        let cache = EntityCache::from_snapshot(snapshot);

        let state = cache.read();
        assert_eq!(
            state
                .account(&addr(1))
                .map(Account::from),
            Some(account_snapshot(5).account)
        );
        assert_eq!(
            state
                .component(EXTRACTOR, "c1")
                .map(ProtocolComponentState::from),
            Some(component_snapshot(5).state)
        );
    }

    #[test]
    fn from_snapshot_stamps_entries_with_the_row_block() {
        let snapshot = StateSnapshot { accounts: vec![], components: vec![component_snapshot(5)] };
        let cache = EntityCache::from_snapshot(snapshot);
        let x = |cache: &EntityCache| {
            cache
                .read()
                .component(EXTRACTOR, "c1")
                .map(|c| ProtocolComponentState::from(c).attributes["x"].clone())
        };

        cache
            .fold(&with_state_delta(aggregated_changes(EXTRACTOR, 5, 5, Some(5)), "c1", 7))
            .unwrap();
        assert_eq!(x(&cache), Some(Bytes::from(1u64)), "block 5 is the row's own block");

        let mut same_second =
            with_state_delta(aggregated_changes(EXTRACTOR, 6, 6, Some(6)), "c1", 8);
        same_second.block.ts = testing::block(5).ts;
        cache.fold(&same_second).unwrap();
        assert_eq!(x(&cache), Some(Bytes::from(8u64)), "block 6 at the same second is newer");
    }

    /// Answers one `state_snapshot` call with a fixed result.
    struct FixedGateway(Mutex<Option<Result<StateSnapshot, StorageError>>>);

    impl FixedGateway {
        fn new(result: Result<StateSnapshot, StorageError>) -> Self {
            Self(Mutex::new(Some(result)))
        }
    }

    #[async_trait]
    impl StateSnapshotGateway for FixedGateway {
        async fn state_snapshot(&self, _chain: Chain) -> Result<StateSnapshot, StorageError> {
            self.0
                .lock()
                .unwrap()
                .take()
                .expect("state_snapshot is called once")
        }
    }

    #[tokio::test]
    async fn load_builds_the_cache_the_gateway_describes() {
        let gateway = FixedGateway::new(Ok(StateSnapshot {
            accounts: vec![account_snapshot(5)],
            components: vec![component_snapshot(5)],
        }));

        let cache = EntityCache::load(&gateway, Chain::Ethereum)
            .await
            .unwrap();

        assert_eq!(cache.entry_counts(), (1, 1));
    }

    #[tokio::test]
    async fn load_reports_a_failed_snapshot_read() {
        let gateway = FixedGateway::new(Err(StorageError::Unexpected("boom".to_string())));

        let err = EntityCache::load(&gateway, Chain::Ethereum)
            .await
            .err()
            .expect("the load must fail");

        assert!(matches!(err, StorageError::Unexpected(m) if m == "boom"));
    }

    #[tokio::test]
    async fn load_of_an_empty_snapshot_folds_normally() {
        let gateway = FixedGateway::new(Ok(StateSnapshot { accounts: vec![], components: vec![] }));
        let cache = EntityCache::load(&gateway, Chain::Ethereum)
            .await
            .unwrap();

        cache
            .fold(&with_state_delta(aggregated_changes(EXTRACTOR, 5, 5, Some(5)), "c1", 7))
            .unwrap();

        assert_eq!(cache.entry_counts(), (0, 0), "a delta for an unknown component is skipped");
    }
}
