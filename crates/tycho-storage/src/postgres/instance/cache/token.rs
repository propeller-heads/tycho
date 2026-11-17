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
use std::{collections::HashMap, sync::RwLock, time::Instant};

use chrono::NaiveDateTime;
use diesel::{dsl::max, prelude::*};
use diesel_async::{
    pooled_connection::deadpool::Pool, scoped_futures::ScopedFutureExt, AsyncPgConnection,
    RunQueryDsl,
};
use tracing::info;
use tycho_common::{
    models::{token::Token, Address, Chain},
    storage::{StorageError, WithTotal},
};

use crate::postgres::{
    count_to_i64,
    instance::{
        active::active_protocols,
        cache::token_store::{ChainTokenStore, TokenQuery, LOAD_BATCH_SIZE},
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
    #[cfg_attr(test, expect(dead_code, reason = "the token poll reads it"))]
    last_sync: RwLock<NaiveDateTime>,
}

struct Tokens {
    /// `last_traded` holds the newest time over all units.
    store: ChainTokenStore,
    #[cfg_attr(test, expect(dead_code, reason = "the write-through reads it"))]
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
}
