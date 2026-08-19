//! An in-memory copy of the token table, used to answer `get_tokens` without SQL.
//!
//! # Why this exists
//!
//! Chains like Base have millions of tokens. Answering a `/tokens` request from
//! Postgres means a `COUNT` query (with a subquery on the balance table for the
//! "recently traded" filter) plus an `OFFSET` scan — per page. A client paging
//! through the whole list repeats that work hundreds of times, taking seconds per
//! page and keeping DB connections busy. This module answers the same queries from
//! memory in microseconds to milliseconds, with results identical to the SQL path
//! (same rows, same order, same totals).
//!
//! # How data is laid out
//!
//! For every chain there is one [`ChainTokenStore`] holding all of that chain's
//! tokens in a single `Vec`, in the same order the SQL path returns them
//! (ascending `token.id`). Tokens are only ever appended, never removed, so a
//! token's *position* in this `Vec` is a small stable number that identifies it —
//! position 7 always means the same token. All filtering works on these positions
//! instead of on addresses, which matters because comparing positions is a single
//! integer comparison while comparing addresses means following a pointer to
//! 20 heap bytes.
//!
//! Two lookup structures sit next to the `Vec`:
//!
//! - `idx_by_address`: a `HashMap` from token address to position, for requests that ask for
//!   specific addresses.
//! - `quality_index`: for each quality value (0–100), the set of positions having that quality. The
//!   sets are `RoaringBitmap`s — think of a bitmap as one bit per position ("is token #7 in this
//!   set? look at bit 7"), and "roaring" as a standard trick to keep such bitmaps small and fast.
//!   The useful property: set operations work on 64 positions at a time (whole machine words), so
//!   "all tokens with quality between 51 and 100" is built by OR-ing a handful of bitmaps, and
//!   counting matches is nearly free.
//!
//! The "recently traded" filter needs no index at all: `last_traded` is a plain
//! `Vec<i64>` with one timestamp per position, and the filter is one integer
//! comparison per candidate while iterating. Scanning even 5M entries takes a few
//! milliseconds, and most queries scan far less because the quality bitmap
//! already narrowed the candidates. A per-timestamp index would be faster still
//! but needs pruning to stay bounded; the flat array cannot grow beyond one entry
//! per token.
//!
//! Answering a query then is: build the candidate set from the filters (a couple
//! of bitmap operations), walk it in position order (= `token.id` order, so
//! pagination is stable), count everything for the `total` field, and clone only
//! the tokens on the requested page.
//!
//! # How the cache stays correct
//!
//! The database remains the source of truth; the cache converges to it through
//! three mechanisms:
//!
//! 1. **Full load at startup** — one paged scan per chain, plus one query for the latest balance
//!    change per token (the "last traded" timestamps).
//! 2. **Write-through** — when *this* process writes tokens or balances to the DB (the gateway
//!    insert/update methods), it applies the same change to the cache. New tokens indexed by the
//!    extractor are queryable immediately.
//! 3. **Delta refresh** — a background task polls every minute for token rows whose `modified_ts`
//!    changed and for balance rows whose `valid_from` is new (see [`TokenCache::refresh`]). This
//!    picks up writers in *other* processes: the `analyze-tokens` cronjob updating quality, and the
//!    indexer's token and balance writes when this process only serves queries (the `rpc` command).
//!    This is why the `token(modified_ts)` and `component_balance_default(valid_from)` index
//!    migrations exist: without them every poll would scan whole tables.
//!
//! Known limit: write-through runs while the enclosing DB transaction is still
//! open, so on rollback the cache can run ahead of the database. This is
//! harmless: only finalized block data reaches this path, so the cache still
//! reflects on-chain state, and the database catches up when the write is
//! retried or the extractor re-processes from its cursor.
//!
//! # Concurrency
//!
//! Each chain's store sits behind one `RwLock`: many readers or one writer, and
//! the lock is never held across an `await`. Reads hold it for microseconds
//! (bitmap math plus cloning one page); writes are a few thousand map/bitmap
//! updates per block.
//!
//! # Cost
//!
//! Everything is paid in memory: roughly 300–400 bytes per token all-in, i.e.
//! ~240 MiB for ethereum (600k tokens) and ~1.9 GiB for Base (5M tokens),
//! measured against dev databases. The bitmaps and the timestamp vector are a
//! rounding error next to the token structs themselves.
use std::{
    collections::{BTreeMap, HashMap},
    ops::Bound,
    str::FromStr,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::Duration,
};

use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection, RunQueryDsl};
use roaring::RoaringBitmap;
use tracing::{debug, error, info, warn};
use tycho_common::{
    models::{protocol::QualityRange, token::Token, Address, Chain, PaginationParams},
    storage::{StorageError, WithTotal},
};

use crate::postgres::{orm, schema, PostgresError};

/// Number of rows fetched per query during the initial full load.
const LOAD_BATCH_SIZE: i64 = 500_000;

/// Number of rows fetched per query during a delta refresh. Smaller than the
/// load batch so readers interleave with the apply loop during a bulk backfill.
const REFRESH_BATCH_SIZE: i64 = 50_000;

/// How many times the initial load tries a query before giving up. Applies both
/// per batch and to the whole load, with exponential backoff between attempts.
const LOAD_RETRY_ATTEMPTS: u32 = 3;

/// Timestamp value for tokens that never appeared in a component balance.
/// `i64::MIN` sorts below any real threshold, matching the SQL `EXISTS` filter
/// which excludes such tokens.
const NEVER_TRADED: i64 = i64::MIN;

/// How far behind the sync marker each [`TokenCache::refresh`] re-reads, to pick
/// up writes whose transaction was still open during an earlier poll.
const REFRESH_OVERLAP_SECS: i64 = 600;

#[derive(Debug, Clone)]
pub(crate) struct TokenQuery {
    pub(crate) chain: Chain,
    pub(crate) addresses: Option<Vec<Address>>,
    pub(crate) quality_range: QualityRange,
    pub(crate) last_traded_ts_threshold: Option<NaiveDateTime>,
    pub(crate) pagination: Option<PaginationParams>,
}

#[derive(Default)]
struct ChainTokenStore {
    /// All tokens of the chain in ascending `token.id` order. Append-only.
    tokens: Vec<Arc<Token>>,
    /// Token address -> position in `tokens`.
    idx_by_address: HashMap<Address, u32>,
    /// Quality value -> positions of tokens with that quality.
    quality_index: BTreeMap<i32, RoaringBitmap>,
    /// Last traded timestamp (unix micros) per position, `NEVER_TRADED` if none.
    last_traded: Vec<i64>,
}

impl ChainTokenStore {
    /// Inserts or updates a token. With `overwrite` false an existing entry is left
    /// untouched, mirroring the `ON CONFLICT DO NOTHING` semantics of the token
    /// insert statement.
    fn upsert(&mut self, token: Token, overwrite: bool) {
        match self
            .idx_by_address
            .get(&token.address)
            .copied()
        {
            Some(idx) => {
                if !overwrite {
                    return;
                }
                let old = &self.tokens[idx as usize];
                if old.quality != token.quality {
                    if let Some(bitmap) = self
                        .quality_index
                        .get_mut(&(old.quality as i32))
                    {
                        bitmap.remove(idx);
                    }
                    self.quality_index
                        .entry(token.quality as i32)
                        .or_default()
                        .insert(idx);
                }
                self.tokens[idx as usize] = Arc::new(token);
            }
            None => {
                let idx = self.tokens.len() as u32;
                let address = token.address.clone();
                let quality = token.quality as i32;
                // Mutation order is a panic-tolerance invariant: positions are
                // pushed first so no index can reference a missing position, and
                // the address index goes last so a token only becomes reachable
                // by address once complete. `write_recovered` relies on this.
                self.tokens.push(Arc::new(token));
                self.last_traded.push(NEVER_TRADED);
                self.quality_index
                    .entry(quality)
                    .or_default()
                    .insert(idx);
                self.idx_by_address.insert(address, idx);
            }
        }
    }

    fn update_last_traded(&mut self, address: &Address, ts: NaiveDateTime) {
        if let Some(&idx) = self.idx_by_address.get(address) {
            let ts = ts.and_utc().timestamp_micros();
            let current = &mut self.last_traded[idx as usize];
            *current = (*current).max(ts);
        }
    }

    /// Evaluates a query against the store.
    ///
    /// Results are in ascending `token.id` order and `total` counts all matches
    /// regardless of pagination, matching the SQL implementation of `get_tokens`.
    ///
    /// The four match arms below are the same algorithm with shortcuts applied
    /// where a filter is absent:
    /// - address/quality filters, no traded filter: the candidate bitmap alone is the answer — its
    ///   size is `total` and the page is cut straight from it.
    /// - with a traded filter: walk the candidates once, keeping a running count and grabbing the
    ///   rows that fall inside the requested page.
    /// - no filters at all: `total` is the store size and the page is a plain slice of the token
    ///   vector.
    fn query(&self, query: &TokenQuery) -> WithTotal<Vec<Token>> {
        let candidates = self.candidate_bitmap(query);
        let ts_threshold = query
            .last_traded_ts_threshold
            .map(|ts| ts.and_utc().timestamp_micros());

        let (offset, limit) = query
            .pagination
            .as_ref()
            .map(|p| (p.offset().max(0) as usize, p.page_size.max(0) as usize))
            .unwrap_or((0, usize::MAX));

        match (candidates, ts_threshold) {
            (Some(bitmap), None) => {
                let total = bitmap.len() as i64;
                let entity = bitmap
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|idx| (*self.tokens[idx as usize]).clone())
                    .collect();
                WithTotal { entity, total: Some(total) }
            }
            (Some(bitmap), Some(threshold)) => self.paginate_filtered(
                bitmap.iter().map(|idx| idx as usize),
                threshold,
                offset,
                limit,
            ),
            (None, None) => {
                let total = self.tokens.len() as i64;
                let entity = self
                    .tokens
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|token| (**token).clone())
                    .collect();
                WithTotal { entity, total: Some(total) }
            }
            (None, Some(threshold)) => {
                self.paginate_filtered(0..self.tokens.len(), threshold, offset, limit)
            }
        }
    }

    /// Combines the address and quality filters into a single set of candidate
    /// positions. `None` means "no filter" (all tokens are candidates).
    ///
    /// The address filter becomes a set via hashmap lookups (addresses not in the
    /// store are simply dropped, like a SQL `IN` list with unknown values). The
    /// quality filter is the union of the per-quality sets in the requested range
    /// — at most 101 unions, since quality is 0–100. When both filters are given,
    /// the answer is their intersection.
    fn candidate_bitmap(&self, query: &TokenQuery) -> Option<RoaringBitmap> {
        let mut candidates: Option<RoaringBitmap> = query
            .addresses
            .as_ref()
            .map(|addresses| {
                addresses
                    .iter()
                    .filter_map(|address| {
                        self.idx_by_address
                            .get(address)
                            .copied()
                    })
                    .collect()
            });

        let quality_bounds = (query.quality_range.min, query.quality_range.max);
        if quality_bounds != (None, None) {
            let lower = quality_bounds
                .0
                .map_or(Bound::Unbounded, Bound::Included);
            let upper = quality_bounds
                .1
                .map_or(Bound::Unbounded, Bound::Included);
            let mut quality_bitmap = RoaringBitmap::new();
            for (_, bitmap) in self.quality_index.range((lower, upper)) {
                quality_bitmap |= bitmap;
            }
            candidates = Some(match candidates {
                Some(address_bitmap) => address_bitmap & quality_bitmap,
                None => quality_bitmap,
            });
        }

        candidates
    }

    /// Single pass over candidate positions applying the last-traded filter:
    /// counts every match (for `total`) and clones only the rows whose running
    /// index falls inside the requested page.
    fn paginate_filtered(
        &self,
        positions: impl Iterator<Item = usize>,
        threshold: i64,
        offset: usize,
        limit: usize,
    ) -> WithTotal<Vec<Token>> {
        let mut total = 0usize;
        let mut entity = Vec::new();
        for pos in positions {
            if self.last_traded[pos] <= threshold {
                continue;
            }
            if total >= offset && entity.len() < limit {
                entity.push((*self.tokens[pos]).clone());
            }
            total += 1;
        }
        WithTotal { entity, total: Some(total as i64) }
    }
}

/// The public face of the cache: one [`ChainTokenStore`] per chain plus the
/// bookkeeping needed to keep them in sync with the database (see the module
/// docs for the overall design).
pub struct TokenCache {
    chains: HashMap<Chain, RwLock<ChainTokenStore>>,
    /// Maps the `chain` table's numeric ids to [`Chain`] values, so the delta
    /// refresh can tell which store a returned row belongs to.
    chain_ids: HashMap<i64, Chain>,
    /// Largest `token.modified_ts` this cache has seen; `refresh` polls for rows
    /// newer than this (minus a safety overlap).
    last_sync: RwLock<NaiveDateTime>,
    /// Largest `component_balance_default.valid_from` this cache has seen;
    /// `refresh` polls for balance rows newer than this (minus a safety overlap).
    last_balance_sync: RwLock<NaiveDateTime>,
}

impl TokenCache {
    pub async fn from_pool(
        pool: Pool<AsyncPgConnection>,
        chains: &[Chain],
    ) -> Result<Self, StorageError> {
        let mut attempt = 0u32;
        loop {
            let mut conn = pool
                .get()
                .await
                .map_err(|err| StorageError::Unexpected(err.to_string()))?;
            match Self::from_connection(&mut conn, chains).await {
                Ok(cache) => return Ok(cache),
                Err(err) if attempt < LOAD_RETRY_ATTEMPTS - 1 => {
                    attempt += 1;
                    // A fresh pooled connection covers the case the batch-level
                    // retry cannot: the connection itself died mid-load.
                    warn!(%err, attempt, "Token cache load failed; retrying with a new connection");
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Loads the tokens of the given chains. Chains present in the `chain` table
    /// but not requested are not loaded and not queryable; chain rows whose name
    /// this build does not recognize are skipped with a warning, so a shared
    /// database cannot prevent startup.
    pub async fn from_connection(
        conn: &mut AsyncPgConnection,
        chains: &[Chain],
    ) -> Result<Self, StorageError> {
        if chains.is_empty() {
            return Err(StorageError::Unexpected(
                "Token cache requires at least one configured chain".to_string(),
            ));
        }

        let start = std::time::Instant::now();
        let chain_rows: Vec<(i64, String)> = schema::chain::table
            .select((schema::chain::id, schema::chain::name))
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        let mut chain_ids = HashMap::new();
        let mut stores = HashMap::new();
        let mut last_sync = NaiveDateTime::default();
        let mut last_balance_sync = NaiveDateTime::default();
        for (chain_id, chain_name) in chain_rows {
            let Ok(chain) = Chain::from_str(&chain_name) else {
                warn!(chain = %chain_name, "Skipping unknown chain in chain table");
                continue;
            };
            if !chains.contains(&chain) {
                continue;
            }
            chain_ids.insert(chain_id, chain);

            let (store, max_modified_ts, max_valid_from) =
                Self::load_chain(conn, chain, chain_id).await?;
            last_sync = last_sync.max(max_modified_ts);
            last_balance_sync = last_balance_sync.max(max_valid_from);
            info!(
                chain = %chain,
                n_tokens = store.tokens.len(),
                elapsed = ?start.elapsed(),
                "Loaded token cache"
            );
            stores.insert(chain, RwLock::new(store));
        }

        Ok(Self {
            chains: stores,
            chain_ids,
            last_sync: RwLock::new(last_sync),
            last_balance_sync: RwLock::new(last_balance_sync),
        })
    }

    async fn load_chain(
        conn: &mut AsyncPgConnection,
        chain: Chain,
        chain_id: i64,
    ) -> Result<(ChainTokenStore, NaiveDateTime, NaiveDateTime), StorageError> {
        let mut store = ChainTokenStore::default();
        let mut idx_by_db_id: HashMap<i64, u32> = HashMap::new();
        let mut max_modified_ts = NaiveDateTime::default();

        let mut last_db_id = i64::MIN;
        loop {
            let mut attempt = 0u32;
            let batch: Vec<(orm::Token, Address)> = loop {
                let result = schema::token::table
                    .inner_join(schema::account::table)
                    .filter(schema::account::chain_id.eq(chain_id))
                    .filter(schema::token::id.gt(last_db_id))
                    .order(schema::token::id.asc())
                    .limit(LOAD_BATCH_SIZE)
                    .select((orm::Token::as_select(), schema::account::address))
                    .load(conn)
                    .await;
                match result {
                    Ok(batch) => break batch,
                    Err(err) if attempt < LOAD_RETRY_ATTEMPTS - 1 => {
                        attempt += 1;
                        // The id cursor makes a retry resume exactly where the
                        // failed query stopped; nothing loaded is redone.
                        warn!(%err, attempt, "Token cache load batch failed; retrying");
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
                    Err(err) => return Err(PostgresError::from(err).into()),
                }
            };

            let batch_len = batch.len();
            for (orm_token, address) in batch {
                last_db_id = orm_token.id;
                max_modified_ts = max_modified_ts.max(orm_token.modified_ts);
                idx_by_db_id.insert(orm_token.id, store.tokens.len() as u32);
                store.upsert(to_model_token(&orm_token, &address, chain), true);
            }
            if (batch_len as i64) < LOAD_BATCH_SIZE {
                break;
            }
        }

        // Latest balance change per token, mirroring the SQL `EXISTS` filter on
        // `component_balance_default.valid_from`.
        let mut attempt = 0u32;
        let last_traded: Vec<(i64, NaiveDateTime)> = loop {
            let result = schema::component_balance_default::table
                .inner_join(schema::protocol_component::table)
                .filter(schema::protocol_component::chain_id.eq(chain_id))
                .select((
                    schema::component_balance_default::token_id,
                    schema::component_balance_default::valid_from,
                ))
                .order_by((
                    schema::component_balance_default::token_id.asc(),
                    schema::component_balance_default::valid_from.desc(),
                ))
                .distinct_on(schema::component_balance_default::token_id)
                .load(conn)
                .await;
            match result {
                Ok(last_traded) => break last_traded,
                Err(err) if attempt < LOAD_RETRY_ATTEMPTS - 1 => {
                    attempt += 1;
                    warn!(%err, attempt, "Token cache last traded load failed; retrying");
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
                Err(err) => return Err(PostgresError::from(err).into()),
            }
        };

        let mut max_valid_from = NaiveDateTime::default();
        for (token_db_id, valid_from) in last_traded {
            max_valid_from = max_valid_from.max(valid_from);
            if let Some(&idx) = idx_by_db_id.get(&token_db_id) {
                store.last_traded[idx as usize] = valid_from.and_utc().timestamp_micros();
            }
        }

        Ok((store, max_modified_ts, max_valid_from))
    }

    pub(crate) fn query_tokens(
        &self,
        query: &TokenQuery,
    ) -> Result<WithTotal<Vec<Token>>, StorageError> {
        let store = self.store(&query.chain)?;
        Ok(read_recovered(store).query(query))
    }

    /// Inserts tokens that are not yet cached; existing entries are left untouched,
    /// mirroring the `ON CONFLICT DO NOTHING` insert semantics.
    pub(crate) fn add_tokens(&self, tokens: &[Token]) {
        self.write_tokens(tokens, false);
    }

    /// Inserts or overwrites tokens with the given values.
    pub(crate) fn upsert_tokens(&self, tokens: &[Token]) {
        self.write_tokens(tokens, true);
    }

    fn write_tokens(&self, tokens: &[Token], overwrite: bool) {
        let mut by_chain: HashMap<Chain, Vec<&Token>> = HashMap::new();
        for token in tokens {
            by_chain
                .entry(token.chain)
                .or_default()
                .push(token);
        }
        for (chain, chain_tokens) in by_chain {
            let Ok(store) = self.store(&chain) else {
                error!(chain = %chain, "Token upsert for chain missing from token cache");
                continue;
            };
            let mut guard = write_recovered(store);
            for token in chain_tokens {
                guard.upsert(token.clone(), overwrite);
            }
        }
    }

    pub(crate) fn update_last_traded<'a>(
        &self,
        chain: &Chain,
        updates: impl Iterator<Item = (&'a Address, NaiveDateTime)>,
    ) {
        let Ok(store) = self.store(chain) else {
            error!(chain = %chain, "Balance update for chain missing from token cache");
            return;
        };
        let mut guard = write_recovered(store);
        for (address, ts) in updates {
            guard.update_last_traded(address, ts);
        }
    }

    /// Loads tokens modified since the last sync and balances traded since the
    /// last balance sync, so the cache catches up on writes made by other
    /// processes: the token analysis cron and, when this process only serves
    /// queries, the indexer's balance writes. The token poll runs first so a
    /// token and its first trade arriving in the same window apply in order.
    /// Each poll advances its sync marker only on success, so a failed poll is
    /// retried on the next tick. Returns the number of rows read across both
    /// lookback windows, an upper bound on (not a count of) actual changes.
    pub async fn refresh(&self, conn: &mut AsyncPgConnection) -> Result<usize, StorageError> {
        let n_tokens = self.refresh_tokens(conn).await?;
        let n_balances = self.refresh_balances(conn).await?;
        Ok(n_tokens + n_balances)
    }

    /// Loads tokens modified since the last sync and writes them into the cache.
    /// Only rows of the configured chains are read.
    ///
    /// The query re-reads a window of [`REFRESH_OVERLAP_SECS`] before the sync
    /// marker. This closes a race: a write from a transaction that was still open
    /// during the previous poll carries a `modified_ts` from *before* that poll,
    /// so a strict `> last_sync` filter would skip it forever. Re-reading recent
    /// history is safe because writing the same token twice is a no-op.
    async fn refresh_tokens(&self, conn: &mut AsyncPgConnection) -> Result<usize, StorageError> {
        self.refresh_tokens_paged(conn, REFRESH_BATCH_SIZE)
            .await
    }

    /// See [`TokenCache::refresh_tokens`]. The window is read in `batch_size`
    /// chunks along the `token.id` cursor and each chunk is applied before the
    /// next is fetched, so a bulk quality backfill neither allocates the whole
    /// window nor holds a write lock for its duration.
    async fn refresh_tokens_paged(
        &self,
        conn: &mut AsyncPgConnection,
        batch_size: i64,
    ) -> Result<usize, StorageError> {
        let last_sync = *read_recovered(&self.last_sync);
        let since = last_sync - chrono::Duration::seconds(REFRESH_OVERLAP_SECS);
        let chain_db_ids: Vec<i64> = self.chain_ids.keys().copied().collect();

        let mut n_rows = 0usize;
        // The marker never moves backwards: starting from `last_sync` (not `since`)
        // keeps an empty poll from sliding it into the past.
        let mut max_modified_ts = last_sync;
        let mut last_id = i64::MIN;
        loop {
            let rows: Vec<(orm::Token, Address, i64)> = schema::token::table
                .inner_join(schema::account::table)
                .filter(schema::token::modified_ts.gt(since))
                .filter(schema::account::chain_id.eq_any(chain_db_ids.clone()))
                .filter(schema::token::id.gt(last_id))
                .order(schema::token::id.asc())
                .limit(batch_size)
                .select((
                    orm::Token::as_select(),
                    schema::account::address,
                    schema::account::chain_id,
                ))
                .load(conn)
                .await
                .map_err(PostgresError::from)?;

            let batch_len = rows.len();
            n_rows += batch_len;
            let mut refreshed = Vec::with_capacity(batch_len);
            for (orm_token, address, chain_id) in rows {
                last_id = orm_token.id;
                let Some(chain) = self.chain_ids.get(&chain_id) else {
                    continue;
                };
                max_modified_ts = max_modified_ts.max(orm_token.modified_ts);
                refreshed.push(to_model_token(&orm_token, &address, *chain));
            }
            self.upsert_tokens(&refreshed);
            if (batch_len as i64) < batch_size {
                break;
            }
        }

        // Advanced only once every batch loaded; a mid-loop failure re-reads the
        // whole window next tick, which is safe because upserts are idempotent.
        *write_recovered(&self.last_sync) = max_modified_ts;
        Ok(n_rows)
    }

    /// Applies the newest `component_balance_default.valid_from` per token written
    /// since the last balance sync as that token's `last_traded` timestamp. Only
    /// rows of the configured chains are read.
    ///
    /// Re-reads a [`REFRESH_OVERLAP_SECS`] window for the same reason as the token
    /// poll; re-applying is a no-op because `update_last_traded` keeps the maximum.
    async fn refresh_balances(&self, conn: &mut AsyncPgConnection) -> Result<usize, StorageError> {
        self.refresh_balances_paged(conn, REFRESH_BATCH_SIZE)
            .await
    }

    /// See [`TokenCache::refresh_balances`]. Pages along the `token_id` cursor,
    /// which is also the `DISTINCT ON` key, so `LIMIT` yields up to `batch_size`
    /// distinct tokens per query and each batch is applied before the next fetch.
    async fn refresh_balances_paged(
        &self,
        conn: &mut AsyncPgConnection,
        batch_size: i64,
    ) -> Result<usize, StorageError> {
        let last_sync = *read_recovered(&self.last_balance_sync);
        let since = last_sync - chrono::Duration::seconds(REFRESH_OVERLAP_SECS);
        let chain_db_ids: Vec<i64> = self.chain_ids.keys().copied().collect();

        let mut n_rows = 0usize;
        // The marker never moves backwards: starting from `last_sync` (not `since`)
        // keeps an empty poll from sliding it into the past.
        let mut max_valid_from = last_sync;
        let mut last_token_id = i64::MIN;
        loop {
            let rows: Vec<(i64, Address, i64, NaiveDateTime)> =
                schema::component_balance_default::table
                    .inner_join(schema::token::table.inner_join(schema::account::table))
                    .filter(schema::component_balance_default::valid_from.gt(since))
                    .filter(schema::account::chain_id.eq_any(chain_db_ids.clone()))
                    .filter(schema::component_balance_default::token_id.gt(last_token_id))
                    .order_by((
                        schema::component_balance_default::token_id.asc(),
                        schema::component_balance_default::valid_from.desc(),
                    ))
                    .distinct_on(schema::component_balance_default::token_id)
                    .limit(batch_size)
                    .select((
                        schema::component_balance_default::token_id,
                        schema::account::address,
                        schema::account::chain_id,
                        schema::component_balance_default::valid_from,
                    ))
                    .load(conn)
                    .await
                    .map_err(PostgresError::from)?;

            let batch_len = rows.len();
            n_rows += batch_len;
            let mut by_chain: HashMap<Chain, Vec<(Address, NaiveDateTime)>> = HashMap::new();
            for (token_id, address, chain_id, valid_from) in rows {
                last_token_id = token_id;
                let Some(chain) = self.chain_ids.get(&chain_id) else {
                    continue;
                };
                max_valid_from = max_valid_from.max(valid_from);
                by_chain
                    .entry(*chain)
                    .or_default()
                    .push((address, valid_from));
            }
            for (chain, updates) in &by_chain {
                self.update_last_traded(
                    chain,
                    updates
                        .iter()
                        .map(|(address, ts)| (address, *ts)),
                );
            }
            if (batch_len as i64) < batch_size {
                break;
            }
        }

        // Advanced only once every batch loaded; a mid-loop failure re-reads the
        // whole window next tick, which is safe because re-applying keeps the max.
        *write_recovered(&self.last_balance_sync) = max_valid_from;
        Ok(n_rows)
    }

    /// Spawns a detached task calling `refresh` every `period`, so the cache picks
    /// up token and balance writes from other processes (e.g. the token analysis
    /// cron, or the indexer when this process only serves queries).
    pub fn spawn_refresh_task(self: &Arc<Self>, pool: Pool<AsyncPgConnection>, period: Duration) {
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

    fn store(&self, chain: &Chain) -> Result<&RwLock<ChainTokenStore>, StorageError> {
        self.chains
            .get(chain)
            .ok_or_else(|| StorageError::NotFound("Chain".to_string(), chain.to_string()))
    }
}

#[cfg(test)]
impl TokenCache {
    fn new_for_tests(chains: &[Chain]) -> Self {
        Self {
            chains: chains
                .iter()
                .map(|chain| (*chain, RwLock::new(ChainTokenStore::default())))
                .collect(),
            chain_ids: HashMap::new(),
            last_sync: RwLock::new(NaiveDateTime::default()),
            last_balance_sync: RwLock::new(NaiveDateTime::default()),
        }
    }
}

/// Recovers a poisoned lock instead of propagating the panic: a writer that
/// panicked mid-mutation cannot break positional invariants (see the mutation
/// order in `ChainTokenStore::upsert`), so the store stays servable and the
/// interrupted write converges through the delta refresh.
fn read_recovered<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| {
        error!("Token cache lock poisoned; recovering");
        poisoned.into_inner()
    })
}

/// See [`read_recovered`].
fn write_recovered<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poisoned| {
        error!("Token cache lock poisoned; recovering");
        poisoned.into_inner()
    })
}

fn to_model_token(orm_token: &orm::Token, address: &Address, chain: Chain) -> Token {
    let gas_usage: Vec<_> = orm_token
        .gas
        .iter()
        .map(|gas| gas.map(|value| value as u64))
        .collect();
    Token::new(
        address,
        orm_token.symbol.as_str(),
        orm_token.decimals as u32,
        orm_token.tax as u64,
        gas_usage.as_slice(),
        chain,
        orm_token.quality as u32,
    )
}

#[cfg(test)]
mod test {
    use chrono::DateTime;

    use super::*;

    fn make_token(seed: u8, quality: u32) -> Token {
        Token::new(
            &Address::from([seed; 20]),
            &format!("TOK{seed}"),
            18,
            0,
            &[Some(64_000)],
            Chain::Ethereum,
            quality,
        )
    }

    fn ts(secs: i64) -> NaiveDateTime {
        DateTime::from_timestamp(secs, 0)
            .unwrap()
            .naive_utc()
    }

    fn base_query() -> TokenQuery {
        TokenQuery {
            chain: Chain::Ethereum,
            addresses: None,
            quality_range: QualityRange::None(),
            last_traded_ts_threshold: None,
            pagination: None,
        }
    }

    fn store_with_tokens(qualities: &[u32]) -> TokenCache {
        let cache = TokenCache::new_for_tests(&[Chain::Ethereum]);
        let tokens: Vec<Token> = qualities
            .iter()
            .enumerate()
            .map(|(position, quality)| make_token(position as u8, *quality))
            .collect();
        cache.add_tokens(&tokens);
        cache
    }

    fn result_symbols(result: &WithTotal<Vec<Token>>) -> Vec<&str> {
        result
            .entity
            .iter()
            .map(|token| token.symbol.as_str())
            .collect()
    }

    #[test]
    fn test_query_all_preserves_insertion_order() {
        let cache = store_with_tokens(&[100, 0, 50]);

        let result = cache
            .query_tokens(&base_query())
            .unwrap();

        assert_eq!(result.total, Some(3));
        assert_eq!(result_symbols(&result), ["TOK0", "TOK1", "TOK2"]);
    }

    #[test]
    fn test_quality_range_filters() {
        let cache = store_with_tokens(&[100, 0, 50, 75, 10]);

        let min_only = cache
            .query_tokens(&TokenQuery { quality_range: QualityRange::min_only(50), ..base_query() })
            .unwrap();
        assert_eq!(min_only.total, Some(3));
        assert_eq!(result_symbols(&min_only), ["TOK0", "TOK2", "TOK3"]);

        let min_max = cache
            .query_tokens(&TokenQuery { quality_range: QualityRange::new(10, 75), ..base_query() })
            .unwrap();
        assert_eq!(result_symbols(&min_max), ["TOK2", "TOK3", "TOK4"]);
    }

    #[test]
    fn test_update_moves_quality_index_entry() {
        let cache = store_with_tokens(&[100, 100]);
        let mut updated = make_token(0, 5);
        updated.symbol = "TOK0v2".to_string();
        cache.upsert_tokens(&[updated]);

        let high = cache
            .query_tokens(&TokenQuery { quality_range: QualityRange::min_only(50), ..base_query() })
            .unwrap();
        assert_eq!(result_symbols(&high), ["TOK1"]);

        let low = cache
            .query_tokens(&TokenQuery { quality_range: QualityRange::new(0, 49), ..base_query() })
            .unwrap();
        assert_eq!(result_symbols(&low), ["TOK0v2"]);
    }

    #[test]
    fn test_add_does_not_overwrite_existing() {
        let cache = store_with_tokens(&[100]);
        let mut duplicate = make_token(0, 5);
        duplicate.symbol = "SHOULD_NOT_APPEAR".to_string();
        cache.add_tokens(&[duplicate]);

        let result = cache
            .query_tokens(&base_query())
            .unwrap();
        assert_eq!(result_symbols(&result), ["TOK0"]);
        assert_eq!(result.entity[0].quality, 100);
    }

    #[test]
    fn test_last_traded_filter_excludes_never_and_older() {
        let cache = store_with_tokens(&[100, 100, 100]);
        cache.update_last_traded(
            &Chain::Ethereum,
            [(&Address::from([0u8; 20]), ts(1_000)), (&Address::from([1u8; 20]), ts(2_000))]
                .into_iter(),
        );

        let result = cache
            .query_tokens(&TokenQuery { last_traded_ts_threshold: Some(ts(1_000)), ..base_query() })
            .unwrap();

        // TOK0 traded exactly at the threshold (strict `>` excludes it), TOK2 never.
        assert_eq!(result.total, Some(1));
        assert_eq!(result_symbols(&result), ["TOK1"]);
    }

    #[test]
    fn test_last_traded_is_monotonic() {
        let cache = store_with_tokens(&[100]);
        let address = Address::from([0u8; 20]);
        cache.update_last_traded(&Chain::Ethereum, [(&address, ts(2_000))].into_iter());
        cache.update_last_traded(&Chain::Ethereum, [(&address, ts(1_000))].into_iter());

        let result = cache
            .query_tokens(&TokenQuery { last_traded_ts_threshold: Some(ts(1_500)), ..base_query() })
            .unwrap();
        assert_eq!(result.total, Some(1));
    }

    #[test]
    fn test_pagination_boundaries() {
        let cache = store_with_tokens(&[100, 100, 100, 100, 100]);

        let page = |page_number: i64| {
            cache
                .query_tokens(&TokenQuery {
                    pagination: Some(PaginationParams::new(page_number, 2)),
                    ..base_query()
                })
                .unwrap()
        };

        assert_eq!(result_symbols(&page(0)), ["TOK0", "TOK1"]);
        assert_eq!(result_symbols(&page(1)), ["TOK2", "TOK3"]);
        assert_eq!(result_symbols(&page(2)), ["TOK4"]);
        assert!(page(3).entity.is_empty());
        // Total is independent of the requested page.
        assert_eq!(page(3).total, Some(5));
    }

    #[test]
    fn test_pagination_with_last_traded_filter() {
        let cache = store_with_tokens(&[100, 100, 100, 100]);
        for seed in [0u8, 2, 3] {
            cache.update_last_traded(
                &Chain::Ethereum,
                [(&Address::from([seed; 20]), ts(5_000))].into_iter(),
            );
        }

        let result = cache
            .query_tokens(&TokenQuery {
                last_traded_ts_threshold: Some(ts(1_000)),
                pagination: Some(PaginationParams::new(1, 2)),
                ..base_query()
            })
            .unwrap();

        assert_eq!(result.total, Some(3));
        assert_eq!(result_symbols(&result), ["TOK3"]);
    }

    #[test]
    fn test_address_filter_orders_by_insertion_and_ignores_unknown() {
        let cache = store_with_tokens(&[100, 100, 100]);

        let result = cache
            .query_tokens(&TokenQuery {
                addresses: Some(vec![
                    Address::from([2u8; 20]),
                    Address::from([0u8; 20]),
                    Address::from([9u8; 20]),
                ]),
                ..base_query()
            })
            .unwrap();

        assert_eq!(result.total, Some(2));
        assert_eq!(result_symbols(&result), ["TOK0", "TOK2"]);
    }

    #[test]
    fn test_address_and_quality_filters_combine() {
        let cache = store_with_tokens(&[100, 10, 100]);

        let result = cache
            .query_tokens(&TokenQuery {
                addresses: Some(vec![Address::from([0u8; 20]), Address::from([1u8; 20])]),
                quality_range: QualityRange::min_only(50),
                ..base_query()
            })
            .unwrap();

        assert_eq!(result_symbols(&result), ["TOK0"]);
    }

    #[test]
    fn test_cache_survives_a_poisoned_store_lock() {
        let cache = store_with_tokens(&[100]);

        let store = cache.chains.get(&Chain::Ethereum).unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.write().unwrap();
            panic!("poison the token cache store lock");
        }));
        assert!(store.is_poisoned());

        let total = || {
            cache
                .query_tokens(&base_query())
                .unwrap()
                .total
        };
        assert_eq!(total(), Some(1), "reads must survive poisoning");
        cache.upsert_tokens(&[make_token(1, 100)]);
        assert_eq!(total(), Some(2), "writes must survive poisoning");
    }

    #[test]
    fn test_unknown_chain_is_an_error() {
        let cache = store_with_tokens(&[100]);
        let result = cache.query_tokens(&TokenQuery { chain: Chain::Base, ..base_query() });
        assert!(matches!(result, Err(StorageError::NotFound(_, _))));
    }
}

/// Benchmark against a real database, comparing the cache path with the SQL path.
///
/// Read-only: connects without running migrations and forces a read-only session.
/// Run with:
///   DATABASE_URL=... cargo test -p tycho-storage --release --lib \
///     token_cache_benchmark -- --ignored --nocapture
#[cfg(test)]
mod benchmark {
    use std::time::Instant;

    use diesel_async::AsyncConnection;

    use super::*;
    use crate::postgres::PostgresGateway;

    fn rss_mib() -> f64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("VmRSS:")
                        .map(|value| {
                            value
                                .trim()
                                .trim_end_matches(" kB")
                                .parse::<f64>()
                                .unwrap_or(0.0) /
                                1024.0
                        })
                })
            })
            .unwrap_or(0.0)
    }

    async fn read_only_connection(db_url: &str) -> AsyncPgConnection {
        let mut conn = AsyncPgConnection::establish(db_url)
            .await
            .expect("failed to connect");
        diesel::sql_query("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
            .execute(&mut conn)
            .await
            .expect("failed to set session read-only");
        conn
    }

    struct Scenario {
        name: &'static str,
        quality: QualityRange,
        traded_days: Option<i64>,
        page: Option<(i64, i64)>,
        addresses: Option<Vec<Address>>,
    }

    #[tokio::test]
    #[ignore]
    async fn token_cache_benchmark() {
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let chain = std::env::var("BENCH_CHAIN")
            .map(|name| Chain::from_str(&name).expect("invalid BENCH_CHAIN"))
            .unwrap_or(Chain::Ethereum);
        let mut conn = read_only_connection(&db_url).await;

        let gateway = PostgresGateway::from_connection(&mut conn).await;
        assert!(gateway.token_cache.is_none(), "gateway must use the SQL path");

        let rss_before = rss_mib();
        let load_start = Instant::now();
        let cache = TokenCache::from_connection(&mut conn, &[chain])
            .await
            .expect("cache load failed");
        let load_elapsed = load_start.elapsed();
        let rss_after = rss_mib();

        // Paginated bootstrap queries: an unpaginated query clones every cached token,
        // which is too much transient memory on multi-million token chains.
        let n_tokens = cache
            .query_tokens(&TokenQuery {
                chain,
                addresses: None,
                quality_range: QualityRange::None(),
                last_traded_ts_threshold: None,
                pagination: Some(PaginationParams::new(0, 1)),
            })
            .expect("query failed")
            .total
            .unwrap();
        println!("== token cache benchmark ==");
        println!("chain: {chain}");
        println!("tokens: {n_tokens}");
        println!("cache load: {load_elapsed:?}, RSS {rss_before:.0} MiB -> {rss_after:.0} MiB");

        // Sample addresses spread across the first 100k tokens for the address filter.
        let sample_addresses: Vec<Address> = {
            let first_page = cache
                .query_tokens(&TokenQuery {
                    chain,
                    addresses: None,
                    quality_range: QualityRange::None(),
                    last_traded_ts_threshold: None,
                    pagination: Some(PaginationParams::new(0, 100_000)),
                })
                .unwrap()
                .entity;
            first_page
                .iter()
                .step_by((first_page.len() / 100).max(1))
                .map(|token| token.address.clone())
                .take(100)
                .collect()
        };

        let page_size = 3_000i64;
        let deep_page = (n_tokens / 2) / page_size;
        let scenarios = vec![
            Scenario {
                name: "all tokens, page 0",
                quality: QualityRange::None(),
                traded_days: None,
                page: Some((0, page_size)),
                addresses: None,
            },
            Scenario {
                name: "all tokens, deep page",
                quality: QualityRange::None(),
                traded_days: None,
                page: Some((deep_page, page_size)),
                addresses: None,
            },
            Scenario {
                name: "min_quality=51, page 0",
                quality: QualityRange::min_only(51),
                traded_days: None,
                page: Some((0, page_size)),
                addresses: None,
            },
            Scenario {
                name: "min_quality=51 traded_30d, page 0",
                quality: QualityRange::min_only(51),
                traded_days: Some(30),
                page: Some((0, page_size)),
                addresses: None,
            },
            Scenario {
                name: "min_quality=0 traded_30d, page 0",
                quality: QualityRange::min_only(0),
                traded_days: Some(30),
                page: Some((0, page_size)),
                addresses: None,
            },
            Scenario {
                name: "100 addresses",
                quality: QualityRange::None(),
                traded_days: None,
                page: Some((0, page_size)),
                addresses: Some(sample_addresses),
            },
        ];

        for scenario in &scenarios {
            let threshold = scenario
                .traded_days
                .map(|days| chrono::Utc::now().naive_utc() - chrono::Duration::days(days));
            let pagination = scenario
                .page
                .map(|(page, size)| PaginationParams::new(page, size));

            let address_refs: Option<Vec<&Address>> = scenario
                .addresses
                .as_ref()
                .map(|addresses| addresses.iter().collect());

            // Warm the DB page cache with one run, then measure the second.
            let mut sql_result = None;
            let mut sql_elapsed = Duration::default();
            for _ in 0..2 {
                let started = Instant::now();
                sql_result = Some(
                    gateway
                        .get_tokens(
                            chain,
                            address_refs.as_deref(),
                            scenario.quality.clone(),
                            threshold,
                            pagination.as_ref(),
                            &mut conn,
                        )
                        .await
                        .expect("sql query failed"),
                );
                sql_elapsed = started.elapsed();
            }
            let sql_result = sql_result.unwrap();

            let query = TokenQuery {
                chain,
                addresses: scenario.addresses.clone(),
                quality_range: scenario.quality.clone(),
                last_traded_ts_threshold: threshold,
                pagination,
            };
            let started = Instant::now();
            let cache_result = cache
                .query_tokens(&query)
                .expect("cache query failed");
            let cache_elapsed = started.elapsed();

            let equal_totals = sql_result.total == cache_result.total;
            let sql_addresses: Vec<&Address> = sql_result
                .entity
                .iter()
                .map(|token| &token.address)
                .collect();
            let cache_addresses: Vec<&Address> = cache_result
                .entity
                .iter()
                .map(|token| &token.address)
                .collect();
            let equal_pages = sql_addresses == cache_addresses;

            println!(
                "{:40} sql {:>12?}  cache {:>10?}  speedup {:>8.0}x  total {:>9?}  match totals={} pages={}",
                scenario.name,
                sql_elapsed,
                cache_elapsed,
                sql_elapsed.as_secs_f64() / cache_elapsed.as_secs_f64().max(1e-9),
                sql_result.total.unwrap(),
                equal_totals,
                equal_pages,
            );
        }

        // Full paging sweep at client defaults: every page through the filtered list.
        let threshold = chrono::Utc::now().naive_utc() - chrono::Duration::days(30);
        let total = cache
            .query_tokens(&TokenQuery {
                chain,
                addresses: None,
                quality_range: QualityRange::min_only(51),
                last_traded_ts_threshold: Some(threshold),
                pagination: Some(PaginationParams::new(0, 1)),
            })
            .unwrap()
            .total
            .unwrap();
        let n_pages = (total + page_size - 1) / page_size;

        let started = Instant::now();
        for page in 0..n_pages {
            cache
                .query_tokens(&TokenQuery {
                    chain,
                    addresses: None,
                    quality_range: QualityRange::min_only(51),
                    last_traded_ts_threshold: Some(threshold),
                    pagination: Some(PaginationParams::new(page, page_size)),
                })
                .unwrap();
        }
        let cache_sweep = started.elapsed();

        let sql_pages = n_pages.min(10);
        let started = Instant::now();
        for page in 0..sql_pages {
            gateway
                .get_tokens(
                    chain,
                    None,
                    QualityRange::min_only(51),
                    Some(threshold),
                    Some(&PaginationParams::new(page, page_size)),
                    &mut conn,
                )
                .await
                .expect("sql query failed");
        }
        let sql_sweep = started.elapsed();

        println!(
            "full sweep (q>=51, 30d, {n_pages} pages): cache {cache_sweep:?}; sql {:?} for first {sql_pages} pages (~{:.1?} extrapolated)",
            sql_sweep,
            sql_sweep * (n_pages.max(1) as u32) / (sql_pages.max(1) as u32),
        );
    }
}

/// Equivalence tests against a real database: every query must return exactly the
/// same rows, order, and total through the cache as through the SQL path.
#[cfg(test)]
mod serial_db_test {
    use chrono::Utc;
    use tycho_common::{
        models::{protocol::ComponentBalance, Balance},
        Bytes,
    };

    use super::*;
    use crate::postgres::{db_fixtures, testing::run_against_db, PostgresGateway};

    const TX_HASH_0: &str = "0xbb7e16d797a9e2fbc537e30f91ed3d27a254dd9578aa4c3af3e5f0d3e8130945";
    const TX_HASH_1: &str = "0x3108322284d0a89a7accb288d1a94384d499504fe7e04441b0706c7628dee7b7";

    /// Inserts a chain with eight tokens of mixed quality, of which two appear in
    /// component balances: T3 at yesterday midnight and T5 half an hour later.
    /// Returns the token addresses in insertion order.
    async fn setup(conn: &mut AsyncPgConnection) -> Vec<Address> {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        // The gateway constructor requires the chain's native token to exist.
        db_fixtures::insert_token(
            conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;
        let blocks = db_fixtures::insert_blocks(conn, chain_id).await;
        let txns =
            db_fixtures::insert_txns(conn, &[(blocks[0], 1, TX_HASH_0), (blocks[1], 1, TX_HASH_1)])
                .await;
        let system_id = db_fixtures::insert_protocol_system(conn, "test_system".to_string()).await;
        let type_id = db_fixtures::insert_protocol_type(conn, "pool", None, None, None).await;
        let component_id = db_fixtures::insert_protocol_component(
            conn, "pool1", chain_id, system_id, type_id, txns[0], None, None,
        )
        .await;

        let qualities = [0, 10, 50, 51, 75, 100, 100, 100];
        let mut addresses = Vec::new();
        let mut token_ids = Vec::new();
        for (position, quality) in qualities.iter().enumerate() {
            let address_hex = format!("{:040x}", position + 1);
            let (_, token_id) = db_fixtures::insert_token(
                conn,
                chain_id,
                &address_hex,
                &format!("T{position}"),
                18,
                Some(*quality),
            )
            .await;
            addresses.push(Bytes::from_str(&address_hex).unwrap());
            token_ids.push(token_id);
        }

        db_fixtures::insert_component_balance(
            conn,
            Balance::from(1000u64.to_be_bytes().to_vec()),
            Bytes::zero(32),
            1000.0,
            token_ids[3],
            txns[0],
            component_id,
            None,
        )
        .await;
        db_fixtures::insert_component_balance(
            conn,
            Balance::from(2000u64.to_be_bytes().to_vec()),
            Bytes::zero(32),
            2000.0,
            token_ids[5],
            txns[1],
            component_id,
            None,
        )
        .await;

        addresses
    }

    /// Comparable projection of a result page. Token's `PartialEq` only compares
    /// addresses, so compare the fields we serve explicitly.
    fn page_values(tokens: &[Token]) -> Vec<(Address, String, u32, u64)> {
        tokens
            .iter()
            .map(|token| (token.address.clone(), token.symbol.clone(), token.quality, token.tax))
            .collect()
    }

    async fn assert_equivalent(
        sql_gateway: &PostgresGateway,
        cache: &TokenCache,
        conn: &mut AsyncPgConnection,
        query: TokenQuery,
    ) {
        let address_refs: Option<Vec<&Address>> = query
            .addresses
            .as_ref()
            .map(|addresses| addresses.iter().collect());
        let sql_result = sql_gateway
            .get_tokens(
                query.chain,
                address_refs.as_deref(),
                query.quality_range.clone(),
                query.last_traded_ts_threshold,
                query.pagination.as_ref(),
                conn,
            )
            .await
            .unwrap();
        let cache_result = cache.query_tokens(&query).unwrap();

        assert_eq!(sql_result.total, cache_result.total, "totals differ for {query:?}");
        assert_eq!(
            page_values(&sql_result.entity),
            page_values(&cache_result.entity),
            "pages differ for {query:?}"
        );
    }

    /// Every combination of the supported filters and pagination settings.
    fn query_matrix(addresses: &[Address]) -> Vec<TokenQuery> {
        let quality_filters = [
            QualityRange::None(),
            QualityRange::min_only(51),
            QualityRange::new(10, 75),
            QualityRange { min: None, max: Some(50) },
        ];
        let thresholds = [
            None,
            Some(db_fixtures::yesterday_midnight()),
            Some(Utc::now().naive_utc() - chrono::Duration::days(30)),
        ];
        let unknown = Bytes::from_str("00000000000000000000000000000000000000ff").unwrap();
        let address_filters = [
            None,
            Some(vec![addresses[3].clone(), addresses[5].clone(), addresses[0].clone(), unknown]),
        ];
        let paginations = [
            None,
            Some(PaginationParams::new(0, 3)),
            Some(PaginationParams::new(1, 3)),
            Some(PaginationParams::new(5, 3)),
        ];

        let mut queries = Vec::new();
        for quality in &quality_filters {
            for threshold in &thresholds {
                for address_filter in &address_filters {
                    for pagination in &paginations {
                        queries.push(TokenQuery {
                            chain: Chain::Ethereum,
                            addresses: address_filter.clone(),
                            quality_range: quality.clone(),
                            last_traded_ts_threshold: *threshold,
                            pagination: pagination.clone(),
                        });
                    }
                }
            }
        }
        queries
    }

    #[tokio::test]
    async fn test_serial_db_cache_matches_sql() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let addresses = setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            assert!(sql_gateway.token_cache.is_none());
            let cache = TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();

            for query in query_matrix(&addresses) {
                assert_equivalent(&sql_gateway, &cache, &mut conn, query).await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_load_is_scoped_to_requested_chains() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            setup(&mut conn).await;
            // Rows the load must ignore: a chain that was not requested and a
            // chain name this build does not recognize.
            let base_chain_id = db_fixtures::insert_chain(&mut conn, "base").await;
            db_fixtures::insert_token(
                &mut conn,
                base_chain_id,
                "000000000000000000000000000000000000beef",
                "BASETOK",
                18,
                Some(100),
            )
            .await;
            db_fixtures::insert_chain(&mut conn, "not-a-chain").await;

            let cache = TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();

            let query = |chain| TokenQuery {
                chain,
                addresses: None,
                quality_range: QualityRange::None(),
                last_traded_ts_threshold: None,
                pagination: None,
            };
            // Native token + the eight setup tokens; the base token is absent.
            let ethereum = cache
                .query_tokens(&query(Chain::Ethereum))
                .unwrap();
            assert_eq!(ethereum.total, Some(9));
            assert!(matches!(
                cache.query_tokens(&query(Chain::Base)),
                Err(StorageError::NotFound(_, _))
            ));

            // Refresh only reads the configured chains and stays consistent.
            cache.refresh(&mut conn).await.unwrap();
            assert!(matches!(
                cache.query_tokens(&query(Chain::Base)),
                Err(StorageError::NotFound(_, _))
            ));
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_cache_refresh_picks_up_external_writes() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let addresses = setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            let cache = TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();

            // Simulate the token analysis job: a quality update from another process.
            diesel::update(schema::token::table)
                .filter(schema::token::symbol.eq("T7"))
                .set(schema::token::quality.eq(5))
                .execute(&mut conn)
                .await
                .unwrap();

            let refreshed = cache.refresh(&mut conn).await.unwrap();
            assert!(refreshed >= 1, "refresh should have seen the updated token");

            for query in query_matrix(&addresses) {
                assert_equivalent(&sql_gateway, &cache, &mut conn, query).await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_cache_write_through() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let mut addresses = setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            let mut cached_gateway = sql_gateway.clone();
            cached_gateway.token_cache = Some(Arc::new(
                TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                    .await
                    .unwrap(),
            ));

            // New token insert, quality update, and a balance write for a token
            // that had never traded — all through the cache-enabled gateway.
            let new_address = Bytes::from_str("00000000000000000000000000000000000000aa").unwrap();
            let new_token =
                Token::new(&new_address, "NEW", 18, 0, &[Some(10)], Chain::Ethereum, 100);
            cached_gateway
                .add_tokens(&[new_token], &mut conn)
                .await
                .unwrap();
            addresses.push(new_address);

            let updated = Token::new(&addresses[6], "T6", 18, 10, &[Some(10)], Chain::Ethereum, 9);
            cached_gateway
                .update_tokens(&[updated], &mut conn)
                .await
                .unwrap();

            cached_gateway
                .add_component_balances(
                    &[ComponentBalance {
                        token: addresses[0].clone(),
                        balance: Balance::from(3000u64.to_be_bytes().to_vec()),
                        balance_float: 3000.0,
                        modify_tx: Bytes::from_str(TX_HASH_1).unwrap(),
                        component_id: "pool1".to_string(),
                    }],
                    &Chain::Ethereum,
                    &mut conn,
                )
                .await
                .unwrap();

            let cache = cached_gateway
                .token_cache
                .as_ref()
                .unwrap();
            for query in query_matrix(&addresses) {
                assert_equivalent(&sql_gateway, cache, &mut conn, query).await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_refresh_picks_up_external_balance_writes() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let addresses = setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            let cache = TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();

            // First trade of T1, written by "another process": plain fixtures,
            // not through the cache-enabled gateway.
            let t1_id: i64 = schema::token::table
                .inner_join(schema::account::table)
                .filter(schema::account::address.eq(addresses[1].clone()))
                .select(schema::token::id)
                .first(&mut conn)
                .await
                .unwrap();
            let tx_id: i64 = schema::transaction::table
                .filter(schema::transaction::hash.eq(Bytes::from_str(TX_HASH_1).unwrap()))
                .select(schema::transaction::id)
                .first(&mut conn)
                .await
                .unwrap();
            let component_id: i64 = schema::protocol_component::table
                .select(schema::protocol_component::id)
                .first(&mut conn)
                .await
                .unwrap();
            db_fixtures::insert_component_balance(
                &mut conn,
                Balance::from(500u64.to_be_bytes().to_vec()),
                Bytes::zero(32),
                500.0,
                t1_id,
                tx_id,
                component_id,
                None,
            )
            .await;

            let traded_t1 = TokenQuery {
                chain: Chain::Ethereum,
                addresses: Some(vec![addresses[1].clone()]),
                quality_range: QualityRange::None(),
                last_traded_ts_threshold: Some(db_fixtures::yesterday_midnight()),
                pagination: None,
            };
            assert_eq!(
                cache
                    .query_tokens(&traded_t1)
                    .unwrap()
                    .total,
                Some(0)
            );

            cache.refresh(&mut conn).await.unwrap();
            assert_eq!(
                cache
                    .query_tokens(&traded_t1)
                    .unwrap()
                    .total,
                Some(1),
                "refresh did not pick up the externally written balance"
            );

            // A second refresh re-reads the overlap window and stays equivalent.
            cache.refresh(&mut conn).await.unwrap();
            for query in query_matrix(&addresses) {
                assert_equivalent(&sql_gateway, &cache, &mut conn, query).await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_token_poll_pages_through_the_window() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let addresses = setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            let cache = TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();

            // Five external quality updates force three batches at batch_size 2.
            diesel::update(schema::token::table)
                .filter(schema::token::symbol.eq_any(vec!["T0", "T2", "T4", "T6", "T7"]))
                .set(schema::token::quality.eq(5))
                .execute(&mut conn)
                .await
                .unwrap();

            let n_rows = cache
                .refresh_tokens_paged(&mut conn, 2)
                .await
                .unwrap();
            assert!(n_rows >= 5, "all changed rows must be read across batches");

            for query in query_matrix(&addresses) {
                assert_equivalent(&sql_gateway, &cache, &mut conn, query).await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_balance_poll_pages_through_the_window() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let addresses = setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            let cache = TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();

            // First trades for three never-traded tokens, written externally.
            let tx_id: i64 = schema::transaction::table
                .filter(schema::transaction::hash.eq(Bytes::from_str(TX_HASH_1).unwrap()))
                .select(schema::transaction::id)
                .first(&mut conn)
                .await
                .unwrap();
            let component_id: i64 = schema::protocol_component::table
                .select(schema::protocol_component::id)
                .first(&mut conn)
                .await
                .unwrap();
            for position in [1usize, 2, 4] {
                let token_id: i64 = schema::token::table
                    .inner_join(schema::account::table)
                    .filter(schema::account::address.eq(addresses[position].clone()))
                    .select(schema::token::id)
                    .first(&mut conn)
                    .await
                    .unwrap();
                db_fixtures::insert_component_balance(
                    &mut conn,
                    Balance::from(500u64.to_be_bytes().to_vec()),
                    Bytes::zero(32),
                    500.0,
                    token_id,
                    tx_id,
                    component_id,
                    None,
                )
                .await;
            }

            let n_rows = cache
                .refresh_balances_paged(&mut conn, 2)
                .await
                .unwrap();
            assert!(n_rows >= 3, "all traded tokens must be read across batches");

            for query in query_matrix(&addresses) {
                assert_equivalent(&sql_gateway, &cache, &mut conn, query).await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_serial_db_refresh_covers_tokens_first_traded_after_load() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let mut addresses = setup(&mut conn).await;
            let sql_gateway = PostgresGateway::from_connection(&mut conn).await;
            let cache = TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                .await
                .unwrap();

            // A token discovered and traded after the initial load, both written
            // by "another process" — the rpc topology. One tick must pick up the
            // token (token poll) and its trade (balance poll).
            let chain_id: i64 = schema::chain::table
                .select(schema::chain::id)
                .first(&mut conn)
                .await
                .unwrap();
            let late_hex = "00000000000000000000000000000000000000cc";
            let (_, late_id) =
                db_fixtures::insert_token(&mut conn, chain_id, late_hex, "LATE", 18, Some(100))
                    .await;
            let tx_id: i64 = schema::transaction::table
                .filter(schema::transaction::hash.eq(Bytes::from_str(TX_HASH_1).unwrap()))
                .select(schema::transaction::id)
                .first(&mut conn)
                .await
                .unwrap();
            let component_id: i64 = schema::protocol_component::table
                .select(schema::protocol_component::id)
                .first(&mut conn)
                .await
                .unwrap();
            db_fixtures::insert_component_balance(
                &mut conn,
                Balance::from(700u64.to_be_bytes().to_vec()),
                Bytes::zero(32),
                700.0,
                late_id,
                tx_id,
                component_id,
                None,
            )
            .await;

            cache.refresh(&mut conn).await.unwrap();

            let late_address = Bytes::from_str(late_hex).unwrap();
            let traded_late = TokenQuery {
                chain: Chain::Ethereum,
                addresses: Some(vec![late_address.clone()]),
                quality_range: QualityRange::None(),
                last_traded_ts_threshold: Some(db_fixtures::yesterday_midnight()),
                pagination: None,
            };
            assert_eq!(
                cache
                    .query_tokens(&traded_late)
                    .unwrap()
                    .total,
                Some(1),
                "one refresh tick must apply both the new token and its first trade"
            );

            addresses.push(late_address);
            for query in query_matrix(&addresses) {
                assert_equivalent(&sql_gateway, &cache, &mut conn, query).await;
            }
        })
        .await;
    }
}

/// End-to-end check of the background refresh task: spawn it the way the
/// gateway builder does, write a quality change from "outside", and wait for
/// the cache to converge.
#[cfg(test)]
mod refresh_task_test {
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;

    use super::*;
    use crate::postgres::testing::run_against_db;

    #[tokio::test]
    async fn test_serial_db_refresh_task_converges() {
        run_against_db(|pool| async move {
            let mut conn = pool.get().await.unwrap();
            let chain_id = crate::postgres::db_fixtures::insert_chain(&mut conn, "ethereum").await;
            crate::postgres::db_fixtures::insert_token(
                &mut conn,
                chain_id,
                "00000000000000000000000000000000000000aa",
                "TOK",
                18,
                Some(100),
            )
            .await;

            let cache = Arc::new(
                TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                    .await
                    .unwrap(),
            );
            let db_url =
                std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
            let task_pool: Pool<AsyncPgConnection> =
                Pool::builder(AsyncDieselConnectionManager::new(db_url))
                    .build()
                    .unwrap();
            cache.spawn_refresh_task(task_pool, Duration::from_millis(200));

            // An out-of-process write: plain SQL, not via the gateway.
            diesel::update(schema::token::table)
                .filter(schema::token::symbol.eq("TOK"))
                .set(schema::token::quality.eq(5))
                .execute(&mut conn)
                .await
                .unwrap();

            let query = TokenQuery {
                chain: Chain::Ethereum,
                addresses: None,
                quality_range: QualityRange::new(0, 49),
                last_traded_ts_threshold: None,
                pagination: None,
            };
            let mut converged = false;
            for _ in 0..25 {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if cache
                    .query_tokens(&query)
                    .unwrap()
                    .total ==
                    Some(1)
                {
                    converged = true;
                    break;
                }
            }
            assert!(converged, "refresh task did not pick up the external quality change");
        })
        .await;
    }
}

/// Reproduces the `index` command's runtime topology: the refresh task is
/// spawned during a `block_on` setup phase on a runtime whose workers must
/// keep driving it afterwards, while the main thread parks on a channel.
#[cfg(test)]
mod refresh_task_topology_test {
    use diesel_async::{pooled_connection::AsyncDieselConnectionManager, AsyncConnection};

    use super::*;

    #[test]
    fn test_serial_db_refresh_task_survives_block_on_setup() {
        let db_url = std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
        let main_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .enable_all()
            .build()
            .unwrap();

        let cache = main_runtime.block_on(async {
            let manager: AsyncDieselConnectionManager<AsyncPgConnection> =
                AsyncDieselConnectionManager::new(db_url.clone());
            let pool = Pool::builder(manager).build().unwrap();
            let mut conn = pool.get().await.unwrap();
            let chain_id = crate::postgres::db_fixtures::insert_chain(&mut conn, "ethereum").await;
            crate::postgres::db_fixtures::insert_token(
                &mut conn,
                chain_id,
                "00000000000000000000000000000000000000bb",
                "TOPO",
                18,
                Some(100),
            )
            .await;
            let cache = Arc::new(
                TokenCache::from_connection(&mut conn, &[Chain::Ethereum])
                    .await
                    .unwrap(),
            );
            cache.spawn_refresh_task(pool, Duration::from_millis(200));
            cache
        });

        // Out-of-process write while the setup block_on has already returned.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
        let db_url_clone = db_url.clone();
        let cache_clone = Arc::clone(&cache);
        main_runtime.spawn(async move {
            let mut conn = AsyncPgConnection::establish(&db_url_clone)
                .await
                .unwrap();
            diesel::update(schema::token::table)
                .filter(schema::token::symbol.eq("TOPO"))
                .set(schema::token::quality.eq(5))
                .execute(&mut conn)
                .await
                .unwrap();

            let query = TokenQuery {
                chain: Chain::Ethereum,
                addresses: None,
                quality_range: QualityRange::new(0, 49),
                last_traded_ts_threshold: None,
                pagination: None,
            };
            let mut converged = false;
            for _ in 0..25 {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if cache_clone
                    .query_tokens(&query)
                    .unwrap()
                    .total ==
                    Some(1)
                {
                    converged = true;
                    break;
                }
            }
            // Cleanup so other tests see a clean DB.
            let _ = diesel::delete(schema::token::table)
                .execute(&mut conn)
                .await;
            let _ = diesel::delete(schema::account::table)
                .execute(&mut conn)
                .await;
            let _ = diesel::delete(schema::chain::table)
                .execute(&mut conn)
                .await;
            done_tx.send(converged).unwrap();
        });

        // Mirrors main.rs: the main thread parks on a std channel.
        let converged = done_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("verification task did not finish");
        assert!(converged, "refresh task stopped after setup block_on returned");
    }
}
