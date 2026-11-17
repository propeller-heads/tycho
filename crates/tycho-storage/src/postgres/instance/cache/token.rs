//! The tokens of one chain in memory, with the trades of each active instance, to answer
//! `get_tokens` without SQL.
//!
//! The tokens are in a [`ChainTokenStore`]; its module describes how the data is laid out.
//!
//! # Units
//!
//! A unit holds the trades of one active instance: per token, the newest balance change in a
//! component of that instance. The cache answers with the newest trade over all units, so a token
//! counts as traded when any active instance traded it.
//!
//! Only the instances that are active at load time get a unit. When a pointer moves later, the
//! cache keeps the unit of the old instance until restart: trades of the old instance still count,
//! and the new instance has no unit.
//!
//! # Known limit
//!
//! The poll reads tokens only, so trade times change only by write-through. A process that does
//! not write the balances itself keeps the trade times of its load: its traded filter misses
//! tokens whose first trade comes after the load, and the gap grows with uptime.
//!
//! # Cost
//!
//! The memory of the store, plus about 20 to 30 bytes per traded token per unit.
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use chrono::NaiveDateTime;
use diesel::{dsl::max, prelude::*};
use diesel_async::{
    pooled_connection::deadpool::Pool, scoped_futures::ScopedFutureExt, AsyncPgConnection,
    RunQueryDsl,
};
use tracing::{debug, error, info};
use tycho_common::{
    models::{token::Token, Address, Chain},
    storage::{StorageError, WithTotal},
};

use crate::postgres::{
    count_to_i64,
    instance::{
        active::active_protocols,
        cache::token_store::{ChainTokenStore, TokenQuery, LOAD_BATCH_SIZE, REFRESH_OVERLAP_SECS},
        schema::tycho::component_balance,
        InstanceId,
    },
    orm,
    schema::token,
    snapshot::{bound_snapshot_reads, force_custom_plans, snapshot_transaction},
    PostgresError,
};

/// The tokens of one chain, with the trades of each active instance (see the module docs).
pub struct TokenCache {
    chain: Chain,
    tokens: RwLock<Tokens>,
    /// Largest `token.modified_ts` seen; the poll reads rows newer than this, less an overlap.
    last_sync: RwLock<NaiveDateTime>,
}

struct Tokens {
    /// `last_traded` holds the newest time over all units.
    store: ChainTokenStore,
    units: HashMap<InstanceId, TradedUnit>,
}

/// The trades of one active instance.
struct TradedUnit {
    protocol: String,
    /// Newest balance change (unix micros) per token position.
    last_traded: HashMap<u32, i64>,
}

impl TokenCache {
    /// Loads the cache in one snapshot of the database. See [`Self::load`].
    #[cfg_attr(
        test,
        expect(dead_code, reason = "a test transaction cannot start the snapshot transaction")
    )]
    pub(crate) async fn from_pool(
        pool: Pool<AsyncPgConnection>,
        chain: Chain,
    ) -> Result<Self, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(err.to_string()))?;
        snapshot_transaction(&mut conn)
            .run(|conn| {
                async move {
                    bound_snapshot_reads(conn).await?;
                    force_custom_plans(conn).await?;
                    Result::<_, PostgresError>::Ok(Self::load(chain, conn).await?)
                }
                .scope_boxed()
            })
            .await
            .map_err(StorageError::from)
    }

    /// Loads all tokens as tokens of `chain`, and a unit for each active instance.
    ///
    /// Run it in one `snapshot_transaction`, so that the tokens, the pointers and the trades come
    /// from one snapshot. A balance of a token without a token row does not count.
    pub(crate) async fn load(
        chain: Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<Self, StorageError> {
        let start = Instant::now();
        let mut store = ChainTokenStore::default();
        let mut last_sync = NaiveDateTime::default();
        let mut last_id = i64::MIN;
        loop {
            let page: Vec<(orm::Token, Address)> = token::table
                .filter(token::id.gt(last_id))
                .order(token::id.asc())
                .limit(LOAD_BATCH_SIZE)
                .select((orm::Token::as_select(), token::address.assume_not_null()))
                .load(conn)
                .await
                .map_err(PostgresError::from)?;

            let page_len = page.len();
            for (row, address) in page {
                last_id = row.id;
                last_sync = last_sync.max(row.modified_ts);
                store.upsert(row.to_model(&address, chain)?, true)?;
            }
            if count_to_i64(page_len) < LOAD_BATCH_SIZE {
                break;
            }
        }

        let protocols = active_protocols(conn).await?;
        let instances: Vec<InstanceId> = protocols
            .iter()
            .map(|(_, instance)| *instance)
            .collect();
        // Current rows only, so the newest `valid_from` gives the same answer as the SQL filter.
        let trades: Vec<(InstanceId, Address, NaiveDateTime)> = component_balance::table
            .filter(component_balance::instance_id.eq_any(&instances))
            .group_by((component_balance::instance_id, component_balance::token))
            .select((
                component_balance::instance_id,
                component_balance::token,
                max(component_balance::valid_from).assume_not_null(),
            ))
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        let mut traded: HashMap<InstanceId, HashMap<u32, i64>> = HashMap::new();
        for (instance, address, valid_from) in trades {
            let Some(&pos) = store.idx_by_address.get(&address) else {
                continue;
            };
            let ts = valid_from.and_utc().timestamp_micros();
            traded
                .entry(instance)
                .or_default()
                .insert(pos, ts);
            let newest = &mut store.last_traded[pos as usize];
            *newest = (*newest).max(ts);
        }
        let units: HashMap<InstanceId, TradedUnit> = protocols
            .into_iter()
            .map(|(protocol, instance)| {
                let last_traded = traded
                    .remove(&instance)
                    .unwrap_or_default();
                (instance, TradedUnit { protocol, last_traded })
            })
            .collect();

        for (instance, unit) in &units {
            info!(
                protocol = %unit.protocol,
                %instance,
                n_traded_tokens = unit.last_traded.len(),
                "Loaded token cache unit"
            );
        }
        info!(
            %chain,
            n_tokens = store.tokens.len(),
            elapsed = ?start.elapsed(),
            "Loaded token cache"
        );
        Ok(Self {
            chain,
            tokens: RwLock::new(Tokens { store, units }),
            last_sync: RwLock::new(last_sync),
        })
    }

    /// Returns the matching tokens, as `get_tokens` of the gateway does. A token counts as traded
    /// after a time when a unit has a newer trade of it. `NotFound` for a chain other than the
    /// cache's chain.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a poisoned lock means a writer panicked mid-update, so the store cannot be trusted"
    )]
    pub(crate) fn query_tokens(
        &self,
        query: &TokenQuery,
    ) -> Result<WithTotal<Vec<Token>>, StorageError> {
        if query.chain != self.chain {
            return Err(StorageError::NotFound("Chain".to_string(), query.chain.to_string()));
        }
        let tokens = self
            .tokens
            .read()
            .expect("token cache lock poisoned");
        tokens.store.query(query)
    }

    /// Adds tokens that are not cached; a cached token stays as it is (`ON CONFLICT DO NOTHING`).
    pub(crate) fn add_tokens(&self, tokens: &[Token]) -> Result<(), StorageError> {
        self.write_tokens(tokens, false)
    }

    /// Adds tokens, or replaces the cached values.
    pub(crate) fn upsert_tokens(&self, tokens: &[Token]) -> Result<(), StorageError> {
        self.write_tokens(tokens, true)
    }

    fn write_tokens(&self, tokens: &[Token], overwrite: bool) -> Result<(), StorageError> {
        let mut guard = self
            .tokens
            .write()
            .map_err(|_poisoned| {
                StorageError::Unexpected("token cache lock poisoned".to_string())
            })?;
        for token in tokens {
            if token.chain != self.chain {
                error!(chain = %token.chain, "Token write for a chain other than the cache's chain");
                continue;
            }
            guard
                .store
                .upsert(token.clone(), overwrite)?;
        }
        Ok(())
    }

    /// Reads the token rows that changed since the last poll and writes them into the cache, so
    /// the cache sees writes of other processes, like the token analysis job. Returns the number
    /// of rows read, which counts rows re-read unchanged.
    ///
    /// The poll re-reads the [`REFRESH_OVERLAP_SECS`] before the marker: a transaction that was
    /// open during the last poll writes a `modified_ts` from before that poll. The marker moves
    /// only on success, so a failed poll runs again on the next tick.
    pub(crate) async fn refresh(
        &self,
        conn: &mut AsyncPgConnection,
    ) -> Result<usize, StorageError> {
        let last_sync = *self
            .last_sync
            .read()
            .expect("token cache lock poisoned");
        let since = last_sync - chrono::Duration::seconds(REFRESH_OVERLAP_SECS);

        let rows: Vec<(orm::Token, Address)> = token::table
            .filter(token::modified_ts.gt(since))
            .order(token::id.asc())
            .select((orm::Token::as_select(), token::address.assume_not_null()))
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        let n_refreshed = rows.len();
        // Start from `last_sync`, so an empty poll does not move the marker back.
        let max_modified_ts = rows
            .iter()
            .map(|(row, _)| row.modified_ts)
            .fold(last_sync, NaiveDateTime::max);
        let refreshed = rows
            .into_iter()
            .map(|(row, address)| row.to_model(&address, self.chain))
            .collect::<Result<Vec<_>, _>>()?;
        self.upsert_tokens(&refreshed)?;

        *self
            .last_sync
            .write()
            .expect("token cache lock poisoned") = max_modified_ts;
        Ok(n_refreshed)
    }

    /// Spawns a detached task that calls `refresh` every `period`.
    #[cfg_attr(test, expect(dead_code, reason = "a test cannot give the task a connection pool"))]
    pub(crate) fn spawn_refresh_task(
        self: &Arc<Self>,
        pool: Pool<AsyncPgConnection>,
        period: Duration,
    ) {
        let cache = Arc::clone(self);
        tokio::spawn(async move {
            info!(period_secs = period.as_secs(), "Token cache refresh task started");
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; skip it, the cache was just loaded.
            interval.tick().await;
            loop {
                interval.tick().await;
                // A bounded wait so pool starvation is visible instead of a
                // silently stalled task.
                match tokio::time::timeout(Duration::from_secs(30), pool.get()).await {
                    Ok(Ok(mut conn)) => match cache.refresh(&mut conn).await {
                        Ok(n_rows_in_window) => {
                            // Counts every row in the lookback window, most of which
                            // were re-read unchanged — not the number of new changes.
                            debug!(n_rows_in_window, "Token cache refresh completed");
                        }
                        Err(err) => error!(%err, "Token cache refresh failed"),
                    },
                    Ok(Err(err)) => error!(%err, "Token cache refresh could not get a connection"),
                    Err(_) => {
                        error!("Token cache refresh timed out waiting for a DB connection")
                    }
                }
            }
        });
    }

    /// Marks trades of `instance`. Does nothing when `instance` has no unit, or for a token that
    /// is not cached. A time never moves back.
    pub(crate) fn update_last_traded<'a>(
        &self,
        instance: InstanceId,
        updates: impl Iterator<Item = (&'a Address, NaiveDateTime)>,
    ) {
        let mut guard = self
            .tokens
            .write()
            .expect("token cache lock poisoned");
        let Tokens { store, units } = &mut *guard;
        let Some(unit) = units.get_mut(&instance) else {
            return;
        };
        for (address, ts) in updates {
            let Some(&pos) = store.idx_by_address.get(address) else {
                continue;
            };
            let micros = ts.and_utc().timestamp_micros();
            let traded = unit
                .last_traded
                .entry(pos)
                .or_insert(micros);
            *traded = (*traded).max(micros);
            store.update_last_traded(address, ts);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime};
    use diesel::prelude::*;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use tycho_common::{
        models::{protocol::QualityRange, token::Token, Address, Chain, PaginationParams},
        storage::StorageError,
    };

    use super::TokenCache;
    use crate::postgres::{
        db_fixtures,
        instance::{
            cache::token_store::TokenQuery,
            read::token_tests::{addresses, b, pool, setup, token, A, B, C},
            schema::tycho::{component_balance, protocol_component},
            InstanceId,
        },
        testing::instance_test_connection,
    };

    const D: &str = "0x00000000000000000000000000000000000000d1";
    const UNKNOWN: &str = "0x00000000000000000000000000000000000000dd";
    const NO_ROW: &str = "0x00000000000000000000000000000000000000ee";
    const E: &str = "0x00000000000000000000000000000000000000e1";

    fn day(d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2020, 1, d)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date")
    }

    /// Inserts the pool `external_id` of the instance with a balance of `token` that changed at
    /// `at`.
    async fn trade(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
        external_id: &str,
        token: &str,
        at: NaiveDateTime,
    ) {
        pool(conn, instance, external_id, token, "0x01", 1.0).await;
        let component = protocol_component::table
            .filter(protocol_component::instance_id.eq(instance))
            .filter(protocol_component::external_id.eq(external_id))
            .select(protocol_component::id);
        diesel::update(
            component_balance::table
                .filter(component_balance::instance_id.eq(instance))
                .filter(component_balance::protocol_component_id.eq_any(component))
                .filter(component_balance::token.eq(b(token))),
        )
        .set(component_balance::valid_from.eq(at))
        .execute(conn)
        .await
        .expect("setting the balance change time failed");
    }

    fn traded_after(threshold: NaiveDateTime) -> TokenQuery {
        TokenQuery {
            chain: Chain::Ethereum,
            addresses: None,
            quality_range: QualityRange::None(),
            last_traded_ts_threshold: Some(threshold),
            pagination: None,
        }
    }

    async fn load(conn: &mut AsyncPgConnection) -> TokenCache {
        TokenCache::load(Chain::Ethereum, conn)
            .await
            .expect("loading the token cache failed")
    }

    #[tokio::test]
    async fn test_load_counts_trades_of_active_instances_only() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        for (address, symbol) in [(A, "A"), (B, "B"), (C, "C")] {
            token(&mut conn, s.chain_id, address, symbol, 100).await;
        }
        trade(&mut conn, s.active, "curve_pool", A, day(2)).await;
        trade(&mut conn, s.uniswap_v4, "v4_pool", B, day(3)).await;
        trade(&mut conn, s.inactive, "inactive_pool", C, day(5)).await;

        let cache = load(&mut conn).await;
        let result = cache
            .query_tokens(&traded_after(day(1)))
            .unwrap();

        assert_eq!(addresses(&result.entity), vec![b(A), b(B)]);
        assert_eq!(result.total, Some(2));
    }

    #[tokio::test]
    async fn test_load_takes_the_newest_trade_over_units() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, A, "A", 100).await;
        trade(&mut conn, s.active, "curve_pool", A, day(2)).await;
        trade(&mut conn, s.uniswap_v4, "v4_pool", A, day(4)).await;

        let cache = load(&mut conn).await;
        let before = cache
            .query_tokens(&traded_after(day(3)))
            .unwrap();
        let at = cache
            .query_tokens(&traded_after(day(4)))
            .unwrap();

        assert_eq!(addresses(&before.entity), vec![b(A)]);
        assert_eq!(addresses(&at.entity), Vec::<Address>::new());
    }

    #[tokio::test]
    async fn test_cache_matches_sql() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        for (address, symbol, quality) in [(A, "A", 100), (B, "B", 50), (C, "C", 10), (D, "D", 100)]
        {
            token(&mut conn, s.chain_id, address, symbol, quality).await;
        }
        trade(&mut conn, s.active, "curve_a", A, day(2)).await;
        trade(&mut conn, s.uniswap_v4, "v4_b", B, day(4)).await;
        trade(&mut conn, s.inactive, "inactive_c", C, day(5)).await;
        trade(&mut conn, s.active, "curve_d", D, day(2)).await;
        trade(&mut conn, s.uniswap_v4, "v4_d", D, day(4)).await;
        let cache = load(&mut conn).await;

        let qualities = [
            QualityRange::None(),
            QualityRange::min_only(50),
            QualityRange { min: None, max: Some(50) },
        ];
        let thresholds = [None, Some(day(1)), Some(day(3))];
        let pages = [
            None,
            Some(PaginationParams::new(0, 2)),
            Some(PaginationParams::new(1, 2)),
            Some(PaginationParams::new(5, 2)),
        ];
        let address_sets = [None, Some(vec![b(A), b(UNKNOWN)])];
        for quality_range in &qualities {
            for threshold in thresholds {
                for pagination in &pages {
                    for address_set in &address_sets {
                        let query = TokenQuery {
                            chain: Chain::Ethereum,
                            addresses: address_set.clone(),
                            quality_range: quality_range.clone(),
                            last_traded_ts_threshold: threshold,
                            pagination: pagination.clone(),
                        };
                        let address_refs: Option<Vec<&Address>> = address_set
                            .as_ref()
                            .map(|set| set.iter().collect());

                        let cached = cache.query_tokens(&query).unwrap();
                        let sql = s
                            .gateway
                            .get_tokens(
                                Chain::Ethereum,
                                address_refs.as_deref(),
                                quality_range.clone(),
                                threshold,
                                pagination.as_ref(),
                                &mut conn,
                            )
                            .await
                            .unwrap();

                        assert_eq!(cached.entity, sql.entity, "{query:?}");
                        assert_eq!(cached.total, sql.total, "{query:?}");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn test_load_skips_balances_of_unknown_tokens() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        trade(&mut conn, s.active, "curve_pool", NO_ROW, day(2)).await;

        let cache = load(&mut conn).await;
        let result = cache
            .query_tokens(&TokenQuery { addresses: Some(vec![b(NO_ROW)]), ..traded_after(day(1)) })
            .unwrap();

        assert_eq!(addresses(&result.entity), Vec::<Address>::new());
        assert_eq!(result.total, Some(0));
    }

    #[tokio::test]
    async fn test_pointer_move_after_load_keeps_the_old_unit() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, A, "A", 100).await;
        token(&mut conn, s.chain_id, C, "C", 100).await;
        trade(&mut conn, s.active, "curve_pool", A, day(2)).await;
        trade(&mut conn, s.inactive, "inactive_pool", C, day(2)).await;

        let cache = load(&mut conn).await;
        db_fixtures::set_active_instance(&mut conn, s.curve_system_id, Some(s.inactive)).await;
        let result = cache
            .query_tokens(&traded_after(day(1)))
            .unwrap();

        assert_eq!(addresses(&result.entity), vec![b(A)]);
    }

    #[tokio::test]
    async fn test_query_of_another_chain_is_not_found() {
        let mut conn = instance_test_connection().await;
        setup(&mut conn).await;

        let cache = load(&mut conn).await;
        let result = cache.query_tokens(&TokenQuery { chain: Chain::Base, ..traded_after(day(1)) });

        assert!(
            matches!(&result, Err(StorageError::NotFound(entity, _)) if entity == "Chain"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn test_trade_of_an_instance_without_a_unit_changes_nothing() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, C, "C", 100).await;
        let cache = load(&mut conn).await;

        cache.update_last_traded(s.inactive, [(&b(C), day(9))].into_iter());
        let result = cache
            .query_tokens(&traded_after(day(8)))
            .unwrap();

        assert_eq!(addresses(&result.entity), Vec::<Address>::new());
    }

    #[tokio::test]
    async fn test_trade_of_a_unit_marks_the_token() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, C, "C", 100).await;
        let cache = load(&mut conn).await;

        cache.update_last_traded(s.active, [(&b(C), day(9))].into_iter());
        let result = cache
            .query_tokens(&traded_after(day(8)))
            .unwrap();

        assert_eq!(addresses(&result.entity), vec![b(C)]);
    }

    #[tokio::test]
    async fn test_trade_keeps_the_newest_time() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, A, "A", 100).await;
        trade(&mut conn, s.active, "curve_pool", A, day(4)).await;
        let cache = load(&mut conn).await;

        cache.update_last_traded(s.active, [(&b(A), day(2))].into_iter());
        let result = cache
            .query_tokens(&traded_after(day(3)))
            .unwrap();

        assert_eq!(addresses(&result.entity), vec![b(A)]);
    }

    #[tokio::test]
    async fn test_trade_of_an_unknown_token_is_ignored() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, A, "A", 100).await;
        let cache = load(&mut conn).await;
        let all = TokenQuery { last_traded_ts_threshold: None, ..traded_after(day(1)) };
        let before = cache.query_tokens(&all).unwrap();

        cache.update_last_traded(s.active, [(&b(NO_ROW), day(9))].into_iter());
        let after = cache.query_tokens(&all).unwrap();

        assert_eq!(before.entity, after.entity);
        assert_eq!(before.total, after.total);
    }

    #[tokio::test]
    async fn test_add_keeps_and_upsert_replaces() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, A, "A", 100).await;
        let cache = load(&mut conn).await;
        let all = TokenQuery { last_traded_ts_threshold: None, ..traded_after(day(1)) };
        let at_zero = Token::new(&b(A), "A", 18, 0, &[], Chain::Ethereum, 0);

        cache
            .add_tokens(&[
                at_zero.clone(),
                Token::new(&b(E), "E", 18, 0, &[], Chain::Ethereum, 100),
            ])
            .unwrap();
        let kept = cache.query_tokens(&all).unwrap();
        assert_eq!(
            kept.entity
                .iter()
                .find(|t| t.address == b(A))
                .unwrap()
                .quality,
            100
        );
        assert!(addresses(&kept.entity).contains(&b(E)));

        cache.upsert_tokens(&[at_zero]).unwrap();
        let replaced = cache
            .query_tokens(&TokenQuery {
                quality_range: QualityRange { min: None, max: Some(0) },
                ..all
            })
            .unwrap();
        assert_eq!(addresses(&replaced.entity), vec![b(A)]);
    }
}
