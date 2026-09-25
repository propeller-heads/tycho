//! Builds the [`EntityCache`] from a state snapshot.

use std::{collections::HashMap, time::Instant};

use metrics::gauge;
use tracing::{info, warn};
use tycho_common::{
    models::{Address, Chain, ComponentId, ProtocolSystem},
    storage::{AccountSnapshot, ComponentSnapshot, CursorSnapshot, StateSnapshot, StorageError},
};
use tycho_storage::postgres::cache::CachedGateway;

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
        gateway: &CachedGateway,
        chain: Chain,
        extractors: &[String],
    ) -> Result<Self, StorageError> {
        let started = Instant::now();
        let snapshot = gateway.state_snapshot(chain).await?;
        let cache = Self::from_snapshot(snapshot, extractors);
        let elapsed = started.elapsed();
        let (accounts, components) = cache.entry_counts();
        gauge!("entity_cache_load_seconds").set(elapsed.as_secs_f64());
        gauge!("entity_cache_accounts").set(accounts as f64);
        gauge!("entity_cache_components").set(components as f64);
        info!(accounts, components, ?elapsed, "Entity cache loaded");
        Ok(cache)
    }

    /// Builds the cache from one snapshot: every row becomes an entry stamped with the block that
    /// wrote it. `extractors` names the configured extractors whose cursors are reported.
    pub(crate) fn from_snapshot(snapshot: StateSnapshot, extractors: &[String]) -> Self {
        let StateSnapshot { accounts, components, cursors } = snapshot;
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
        report_cursors(extractors, &cursors);
        Self::from_entries(account_entries, component_entries)
    }
}

/// Logs where each configured extractor's stream resumes. A configured extractor without a
/// cursor row starts from its configured block; that is a first run, not an error.
fn report_cursors(extractors: &[String], cursors: &[CursorSnapshot]) {
    for extractor in extractors {
        match cursors
            .iter()
            .find(|c| &c.extractor == extractor)
        {
            Some(c) => {
                info!(extractor, block = c.block_number, hash = %c.block_hash, "Snapshot cursor")
            }
            None => warn!(extractor, "No saved cursor in the snapshot; the extractor starts fresh"),
        }
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
    use tycho_common::{
        models::{contract::Account, protocol::ProtocolComponentState, Chain},
        storage::WriteTimestamp,
        Bytes,
    };

    use super::*;
    use crate::{
        services::state::window::FoldSink,
        testing::{self, aggregated_changes, with_state_delta},
    };

    const EXTRACTOR: &str = "ex";

    fn cursors() -> Vec<CursorSnapshot> {
        vec![CursorSnapshot {
            extractor: EXTRACTOR.to_string(),
            cursor: b"c".to_vec(),
            block_hash: Bytes::zero(32),
            block_number: 5,
        }]
    }

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
            cursors: cursors(),
        };

        let cache = EntityCache::from_snapshot(snapshot, &[EXTRACTOR.to_string()]);

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
        let snapshot = StateSnapshot {
            accounts: vec![],
            components: vec![component_snapshot(5)],
            cursors: cursors(),
        };
        let cache = EntityCache::from_snapshot(snapshot, &[EXTRACTOR.to_string()]);
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
}
