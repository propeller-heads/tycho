//! # Postgres based storage backend
//!
//! This postgres-based storage backend provides implementations for the
//! traits defined in the storage module.
//!
//! ## Design Decisions
//!
//! ### Representation of Enums as Tables
//!
//! Certain enums such as 'Chain' are modelled as tables in our implementation.
//! This decision stems from an understanding that while extending the Rust
//! codebase to include more enums is a straightforward task, modifying the type
//! of a SQL column can be an intricate process. By representing enums as
//! tables, we circumvent unnecessary migrations when modifying, for example the Chain enum.
//!
//! With this representation, it's important to synchronize them whenever the
//! enums members changed. This can be done automatically once at system
//! startup.
//!
//!
//! Note: A removed enum can be ignored safely even though it might instigate a
//! panic if an associated entity still exists in the database and retrieved
//! with a codebase which no longer presents the enum value.
//!
//! ### Timestamps
//!
//! We use naive timestamps throughout the code as it is assumed that the server
//! that will be running the application will always use UTC as it's local time.
//! Thus all naive timestamps on the application are implcitly in UTC. Be aware
//! that especially tests might run on machines that violate this assumption so
//! in tests make sure to create a timestamp aware timestamp and convert it to
//! UTC before using the naive value.
//!
//! #### Timestamp fields
//!
//! As the are multiple different timestamp columns below is a short summary how
//! these are used:
//!
//! * `inserted` and `modified_ts`: These are pure "book-keeping" values, used to track when the
//!   record was inserted or updated. They are not used in any business logic. These values are
//!   automatically set via Postgres triggers, so they don't need to be manually set.
//!
//! * `valid_from` and `valid_to`: These timestamps enable data versioning aka time-travel
//!   functionality. Hence, these should always be set correctly. `valid_from` must be set to the
//!   timestamp at which the entity was created - most often that will be the value of the
//!   corresponding `block.ts`. Same applies for `valid_to`. There are triggers in place to
//!   automatically set `valid_to` if you insert a new entity with the same identity (not primary
//!   key). But to delete a record, `valid_to` needs to be manually set as no automatic trigger
//!   exists for deletes yet.
//!
//! * `created_ts`: For entities that are immutable, this timestamp records when the entity was
//!   created and is used for time-travel functionality. For example, for contracts, this timestamp
//!   will be the block timestamp of its deployment.
//!
//! * `deleted_ts`: This serves a similar purpose to `created_ts`, but in reverse. It indicates when
//!   an entity was deleted.
//!
//! * `block.ts`: This is the timestamp attached to the block. Ideally, it should coincide with the
//!   validation/mining start time.
//!
//! ### Versioning
//!
//! This implementation utilizes temporal tables for recording the changes in
//! entities over time. In this model, `valid_from` and `valid_to` determine the
//! timeframe during which the facts provided by the record are regarded as
//! accurate (validity period). Typically, in temporal tables, a valid version
//! for a specific timestamp is found using the following predicate:
//!
//! ```sql
//! valid_from < version_ts AND (version_ts <= valid_to)
//! ```
//!
//! The `valid_to` can be set to a max timestamp (262142-12-31T23:59:59.9999Z), signifying that the
//! version remains valid. However, as all alterations within a block happen simultaneously,
//! this predicate might yield multiple valid versions for a single entity.
//!
//! To further assign a temporal sequence to these entities, the transaction
//! index within the block is recorded, usually through a `modify_tx` foreign
//! key.
//!
//! ```sql
//! SELECT * FROM table
//! JOIN transaction
//! WHERE valid_from < version_ts
//!     AND (version_ts <= valid_to)
//! ORDER BY entity_id, transaction.index DESC
//! DISTINCT ON entity_id
//! ```
//!
//! Here we select a set of versions by timestamp, then arrange rows by their
//! transaction index (descending) and choose the first row, thus obtaining the
//! latest version within the block (aka version at end of block).
//!
//! #### Contract Storage Table
//!
//! Special attention must be given to the contract_storage table, which also
//! records the previous value with each modification. This simplifies the
//! generation of a delta change structure utilized during reorgs for informing
//! clients about the necessary updates. Deletions in this table are modeled
//! as simple updates; in the case of deletion, it's value is updated to null.
//! This technique simplifies querying for delta changes while maintaining
//! efficiency at the cost of requiring additional storage space. As
//! `valid_from` and `valid_to` are not entirely sufficient to find a single
//! valid state within blockchain systems, the contract_storage table
//! additionally maintains an `ordinal` column. This column is redundant with
//! the transaction's index that produced the respective changes. This
//! redundancy is to avoid additional joins and further optimize query
//! performance.
//!
//! ### Reverts
//! If a reorg is observed, we will be asked by the stream to revert to a previous
//! block number. This is handled using the `ON DELETE CASCADE` feature provided by
//! postgres. Each state change is tracked by a creation or modification transaction
//! if the parent transaction is deleted, postgres will delete the corresponding
//! entry in the child table for us.
//! Now all we have to do is to unset valid_to columns that point directly to our
//! last reverted block.
//!
//! ### Atomic Transactions
//!
//! In our design, direct connection to the database and consequently beginning,
//! committing, or rolling back transactions isn't handled within these
//! common-purpose implementations. Rather, each operation receives a connection
//! reference which can either be a simple DB connection, or a DB connection
//! within a transactional context.
//!
//! This approach enables us to chain multiple common-purpose CRUD operations
//! into a single transaction. This guarantees preservation of valid state
//! throughout the application lifetime, even if the process panics during
//! database operations.
use std::{collections::HashMap, hash::Hash, ops::Deref, str::FromStr, sync::Arc, time::Duration};

use chrono::{Duration as ChronoDuration, NaiveDateTime, Utc};
use diesel::prelude::*;
use diesel_async::{
    pooled_connection::{deadpool::Pool, AsyncDieselConnectionManager},
    AsyncPgConnection, RunQueryDsl,
};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use tracing::{debug, info};
use tycho_common::{
    models::{token::Token, Chain, TxHash},
    storage::{BlockIdentifier, BlockOrTimestamp, StorageError, Version, VersionKind},
};
use unicode_segmentation::UnicodeSegmentation;

pub mod builder;
pub mod cache;
mod chain;
mod contract;
pub mod direct;
mod entry_point;
mod extraction_state;
mod orm;
mod protocol;
mod schema;
mod versioning;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("./migrations/");

// +262142-12-31T23:59:59.999999999
const MAX_TS: NaiveDateTime = NaiveDateTime::MAX;

lazy_static! {
    /// Simplifies querying current and historical versions by introducing a special marker version.
    ///
    /// By setting `valid_to` to `MAX_TS = max_version_ts + 1s` for the latest versions, this approach
    /// collapses the query predicates from
    /// `valid_from <= target_version < valid_to or valid_to is "latest"`
    /// to a more streamlined
    /// `valid_from <= target_version < valid_to`.
    /// This enables `max_version_ts` queries to include `MAX_TS` entries (current versions),
    /// while excluding them from other version queries, effectively differentiating between
    /// current and historical data without additional predicates.
    static ref MAX_VERSION_TS: NaiveDateTime = NaiveDateTime::MAX - Duration::from_secs(1);
}

pub(crate) struct ValueIdTableCache<E> {
    map_id: HashMap<E, i64>,
    map_enum: HashMap<i64, E>,
}

/// Provides caching for enum and its database ID relationships.
///
/// Uses a double sided hash map to provide quick lookups in both directions.
impl<E> ValueIdTableCache<E>
where
    E: Eq + Hash + Clone + FromStr + std::fmt::Debug + std::fmt::Display,
    <E as FromStr>::Err: std::fmt::Debug,
{
    /// Creates a new cache from a slice of tuples.
    ///
    /// # Arguments
    ///
    /// * `entries` - A slice of tuples ideally obtained from a database query.
    pub fn from_tuples(entries: Vec<(i64, String)>) -> Self {
        let mut cache = Self { map_id: HashMap::new(), map_enum: HashMap::new() };
        for (id_, name_) in entries {
            let val = E::from_str(&name_).expect("valid enum value");
            cache.map_id.insert(val.clone(), id_);
            cache.map_enum.insert(id_, val);
        }
        cache
    }

    /// Fetches the associated database ID for an enum variant. Returns a StorageError
    /// if the enum variant is not found in the cache.
    ///
    /// # Arguments
    ///
    /// * `val` - The enum variant to lookup.
    fn try_get_id(&self, val: &E) -> Result<i64, StorageError> {
        self.map_id
            .get(val)
            .copied()
            .ok_or(StorageError::NotFound(val.to_string(), "id".to_string()))
    }

    /// Retrieves the corresponding enum variant for a database ID. Returns a StorageError
    /// if the database ID is not found in the cache.
    ///
    /// # Arguments
    ///
    /// * `id` - The database ID to lookup.
    fn try_get_value(&self, id: &i64) -> Result<E, StorageError> {
        self.map_enum
            .get(id)
            .cloned()
            .ok_or(StorageError::NotFound(id.to_string(), "enum".to_string()))
    }

    /// Checks if an enum variant exists in the cache. Returns `true` if the variant is found,
    /// otherwise returns `false`.
    ///
    /// # Arguments
    ///
    /// * `val` - The enum variant to check for existence.
    fn value_exists(&self, val: &E) -> bool {
        self.map_id.contains_key(val)
    }
}

type ChainEnumCache = ValueIdTableCache<Chain>;
// Maps the chain to the DB id of the native token
type NativeTokenEnumCache = ValueIdTableCache<Chain>;
/// ProtocolSystem is not handled as an Enum, because that would require us to restart the whole
/// application every time we want to add another System. Hence, to diverge from the implementation
/// of the Chain enum was a conscious decision.
type ProtocolSystemEnumCache = ValueIdTableCache<String>;

trait FromConnection<T> {
    async fn from_pool(pool: Pool<AsyncPgConnection>) -> Result<T, StorageError>;
    async fn from_connection(conn: &mut AsyncPgConnection) -> Result<T, StorageError>;
}

impl FromConnection<ChainEnumCache> for ChainEnumCache {
    async fn from_pool(pool: Pool<AsyncPgConnection>) -> Result<ChainEnumCache, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(err.to_string()))?;

        Self::from_connection(&mut conn).await
    }

    async fn from_connection(
        mut conn: &mut AsyncPgConnection,
    ) -> Result<ChainEnumCache, StorageError> {
        let results = async {
            use schema::chain::dsl::*;
            chain
                .select((id, name))
                .load(&mut conn)
                .await
                .expect("Failed to load chain ids!")
        }
        .await;
        Ok(Self::from_tuples(results))
    }
}

impl FromConnection<ProtocolSystemEnumCache> for ProtocolSystemEnumCache {
    async fn from_pool(
        pool: Pool<AsyncPgConnection>,
    ) -> Result<ProtocolSystemEnumCache, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(err.to_string()))?;

        Self::from_connection(&mut conn).await
    }

    async fn from_connection(
        mut conn: &mut AsyncPgConnection,
    ) -> Result<ProtocolSystemEnumCache, StorageError> {
        let results = async {
            use schema::protocol_system::dsl::*;
            protocol_system
                .select((id, name))
                .load(&mut conn)
                .await
                .expect("Failed to load protocol system ids!")
        }
        .await;
        Ok(Self::from_tuples(results))
    }
}

// Helper type to retrieve entities with their associated tx hashes.
#[derive(Debug)]
struct WithTxHash<T> {
    entity: T,
    tx: Option<TxHash>,
}

impl<T> Deref for WithTxHash<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.entity
    }
}

// Helper type to wrap entities with their associated ordinal.
#[derive(Debug)]
struct WithOrdinal<T, O> {
    entity: T,
    ordinal: O,
}

impl<T, O> WithOrdinal<T, O> {
    pub fn new(entity: T, ordinal: O) -> Self {
        Self { entity, ordinal }
    }
}

#[derive(Debug)]
struct PostgresError(StorageError);

impl From<diesel::result::Error> for PostgresError {
    fn from(value: diesel::result::Error) -> Self {
        PostgresError(StorageError::Unexpected(format!("DieselError: {value}")))
    }
}

impl From<PostgresError> for StorageError {
    fn from(value: PostgresError) -> Self {
        value.0
    }
}

impl From<StorageError> for PostgresError {
    fn from(value: StorageError) -> Self {
        PostgresError(value)
    }
}

fn truncate_to_byte_limit(input: &str, limit: usize) -> String {
    let mut result = String::new();
    let mut byte_count = 0;

    for grapheme in input.graphemes(true) {
        let grapheme_len = grapheme.len(); // UTF-8 byte length of the grapheme
        if byte_count + grapheme_len > limit {
            break;
        }
        result.push_str(grapheme);
        byte_count += grapheme_len;
    }

    result
}

fn storage_error_from_diesel(
    err: diesel::result::Error,
    entity: &str,
    id: &str,
    fetch_args: Option<String>,
) -> PostgresError {
    let err_string = err.to_string();
    match err {
        diesel::result::Error::DatabaseError(
            diesel::result::DatabaseErrorKind::UniqueViolation,
            details,
        ) => {
            if let Some(col) = details.column_name() {
                if col == "id" {
                    return PostgresError(StorageError::DuplicateEntry(
                        entity.to_owned(),
                        id.to_owned(),
                    ));
                }
            }
            PostgresError(StorageError::Unexpected(err_string))
        }
        diesel::result::Error::NotFound => {
            if let Some(related_entitiy) = fetch_args {
                return PostgresError(StorageError::NoRelatedEntity(
                    entity.to_owned(),
                    id.to_owned(),
                    related_entitiy,
                ));
            }
            PostgresError(StorageError::NotFound(entity.to_owned(), id.to_owned()))
        }
        _ => PostgresError(StorageError::Unexpected(err_string)),
    }
}

async fn maybe_lookup_block_ts(
    block: &BlockOrTimestamp,
    conn: &mut AsyncPgConnection,
) -> Result<NaiveDateTime, StorageError> {
    match block {
        BlockOrTimestamp::Block(BlockIdentifier::Hash(h)) => Ok(orm::Block::by_hash(h, conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "Block", &hex::encode(h), None))?
            .ts),
        BlockOrTimestamp::Block(BlockIdentifier::Number((chain, no))) => {
            Ok(orm::Block::by_number(*chain, *no, conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, "Block", &format!("{no}"), None))?
                .ts)
        }
        BlockOrTimestamp::Block(BlockIdentifier::Latest(chain)) => {
            Ok(orm::Block::most_recent(*chain, conn)
                .await
                .map_err(|err| storage_error_from_diesel(err, "Block", "latest", None))?
                .ts)
        }
        BlockOrTimestamp::Timestamp(ts) => Ok(*ts),
    }
}

async fn maybe_lookup_version_ts(
    version: &Version,
    conn: &mut AsyncPgConnection,
) -> Result<NaiveDateTime, StorageError> {
    if !matches!(version.1, VersionKind::Last) {
        return Err(StorageError::Unsupported(format!("Unsupported version kind: {:?}", version.1)));
    }
    maybe_lookup_block_ts(&version.0, conn).await
}

#[derive(Clone)]
pub(crate) struct PostgresGateway {
    protocol_system_id_cache: Arc<ProtocolSystemEnumCache>,
    chain_id_cache: Arc<ChainEnumCache>,
    native_token_id_cache: Arc<NativeTokenEnumCache>,
    /// Any versions dated before this date, as per their `valid_to` column, will be
    /// discarded and never be inserted into the db. We supply this as an absolute date
    /// since updating it must be done carefully. To avoid gaps in versions this can't
    /// be updated once an extractor has crossed it, but has not yet crossed the new
    /// horizon (aka it should never move faster than an extractor).
    retention_horizon: NaiveDateTime,
}

impl PostgresGateway {
    pub fn with_cache(
        chain_cache: Arc<ChainEnumCache>,
        native_token_cache: Arc<NativeTokenEnumCache>,
        protocol_system_cache: Arc<ProtocolSystemEnumCache>,
        retention_horizon: NaiveDateTime,
    ) -> Self {
        Self {
            protocol_system_id_cache: protocol_system_cache,
            chain_id_cache: chain_cache,
            native_token_id_cache: native_token_cache,
            retention_horizon,
        }
    }

    #[allow(dead_code)]
    pub async fn from_connection(conn: &mut AsyncPgConnection) -> Self {
        let chain_cache = ChainEnumCache::from_connection(conn)
            .await
            .expect("Failed ot load chain enum cache");
        let protocol_system_cache = ProtocolSystemEnumCache::from_connection(conn)
            .await
            .expect("Failed to load protocol system cache");
        let native_token_cache = Self::native_token_cache_from_connection(conn, &chain_cache)
            .await
            .expect("Failed to load native token cache");

        Self::with_cache(
            Arc::new(chain_cache),
            Arc::new(native_token_cache),
            Arc::new(protocol_system_cache),
            NaiveDateTime::default(),
        )
    }

    fn get_chain_id(&self, chain: &Chain) -> Result<i64, StorageError> {
        self.chain_id_cache.try_get_id(chain)
    }

    fn get_chain(&self, id: &i64) -> Result<Chain, StorageError> {
        self.chain_id_cache.try_get_value(id)
    }

    fn get_native_token_id(&self, chain: &Chain) -> Result<i64, StorageError> {
        self.native_token_id_cache
            .try_get_id(chain)
    }

    fn get_protocol_system_id(&self, protocol_system: &String) -> Result<i64, StorageError> {
        self.protocol_system_id_cache
            .try_get_id(protocol_system)
    }

    fn get_protocol_system(&self, id: &i64) -> Result<String, StorageError> {
        self.protocol_system_id_cache
            .try_get_value(id)
    }

    pub async fn new(
        pool: Pool<AsyncPgConnection>,
        retention_horizon: NaiveDateTime,
    ) -> Result<Self, StorageError> {
        let chain_cache = ChainEnumCache::from_pool(pool.clone()).await?;
        let native_token_cache = Self::native_cache_from_pool(pool.clone(), &chain_cache).await?;
        let protocol_system_cache: ValueIdTableCache<String> =
            ProtocolSystemEnumCache::from_pool(pool.clone()).await?;
        let gw = PostgresGateway::with_cache(
            Arc::new(chain_cache),
            Arc::new(native_token_cache),
            Arc::new(protocol_system_cache),
            retention_horizon,
        );

        Ok(gw)
    }

    // Could not use FromConnection trait as it is already implemented for a Chain->id map type.
    // Also this custom 'from connection' fn signature allows us to reuse the already fetched
    // chain cache.
    async fn native_token_cache_from_connection(
        mut conn: &mut AsyncPgConnection,
        chain_cache: &ChainEnumCache,
    ) -> Result<NativeTokenEnumCache, StorageError> {
        let mut native_tokens = Vec::new();

        // loop through cached chains and fetch their native token ids
        for chain in chain_cache.map_enum.values() {
            let chain_id = chain_cache.try_get_id(chain)?;
            let native_token = chain.native_token();
            let token_id = async {
                schema::token::table
                    .inner_join(
                        schema::account::table
                            .on(schema::token::account_id.eq(schema::account::id)),
                    )
                    .select(schema::token::id)
                    .filter(schema::account::chain_id.eq(chain_id))
                    .filter(schema::account::address.eq(native_token.address))
                    .first(&mut conn)
                    .await
                    .expect("Failed to load native token id!")
            }
            .await;
            native_tokens.push((token_id, chain.to_string()));
        }
        Ok(NativeTokenEnumCache::from_tuples(native_tokens))
    }

    // Could not use FromConnection trait as it is already implemented for a Chain->id map type.
    // Also this custom 'from connection' fn signature allows us to reuse the already fetched
    // chain cache.
    async fn native_cache_from_pool(
        pool: Pool<AsyncPgConnection>,
        chain_cache: &ChainEnumCache,
    ) -> Result<NativeTokenEnumCache, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(err.to_string()))?;

        Self::native_token_cache_from_connection(&mut conn, chain_cache).await
    }
}

/// Establishes a connection to the database and creates a connection pool.
///
/// This function takes in the URL of the database as an argument and returns a pool
/// of connections that the application can use to interact with the database. If there's
/// any error during the creation of this pool, it is converted into a `StorageError` for
/// uniform error handling across the application.
///
/// # Arguments
///
/// - `db_url`: A string slice that holds the URL of the database to connect to.
///
/// # Returns
///
/// A Result which is either:
///
/// - `Ok`: Contains a `Pool` of `AsyncPgConnection`s if the connection was established
///   successfully.
/// - `Err`: Contains a `StorageError` if there was an issue creating the connection pool.
async fn connect(
    db_url: &str,
    retention_horizon: Option<NaiveDateTime>,
) -> Result<Pool<AsyncPgConnection>, StorageError> {
    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(db_url);
    let pool = Pool::builder(config)
        .build()
        .map_err(|err| StorageError::Unexpected(err.to_string()))?;
    run_migrations(db_url, retention_horizon);
    Ok(pool)
}

/// Ensures the `Chain` enum is present in the database, if not it inserts it.
///
/// This function serves as a way to ensure all chains found within the `chains`  
/// slice are present within the database. It does this by inserting each chain into
/// the `chain` table. If a conflict arises during this operation (indicating that
/// the chain already exists in the database), it simply does nothing for that
/// specific operation and moves on.
///
/// It uses a connection from the passed in `Pool<AsyncPgConnection>` asynchronously.
/// In case of any error during these operations, the function will panic with an
/// appropriate error message.
///
///
/// # Arguments
///
/// - `chains`: A slice containing chains which need to be ensured in the database.
/// - `pool`: An instance of `Pool` containing `AsyncPgConnection`s used to interact with the
///   database.
///
/// # Panics
///
/// This function will panic under two circumstances:
///
/// - If it failed to get a connection from the provided pool.
/// - If there was an issue ensuring the presence of chains in the database.
async fn ensure_chains(chains: &[Chain], pool: Pool<AsyncPgConnection>) {
    let mut conn = pool.get().await.expect("connection ok");

    for chain in chains {
        diesel::insert_into(schema::chain::table)
            .values(schema::chain::name.eq(chain.to_string()))
            .on_conflict_do_nothing()
            .execute(&mut conn)
            .await
            .expect("Could not ensure chain in database");

        let chain_id: i64 = schema::chain::table
            .select(schema::chain::id)
            .filter(schema::chain::name.eq(chain.to_string()))
            .first(&mut conn)
            .await
            .expect("Chain must exist after insert");

        ensure_token_with_price(chain_id, &chain.native_token(), &mut conn).await;
        ensure_token_with_price(chain_id, &chain.wrapped_native_token(), &mut conn).await;
    }

    debug!("Ensured chain enum and native token presence for: {:?}", chains);
}

pub(crate) async fn ensure_token_with_price(
    chain_id: i64,
    token: &Token,
    conn: &mut AsyncPgConnection,
) {
    diesel::insert_into(schema::account::table)
        .values((
            schema::account::chain_id.eq(chain_id),
            schema::account::title.eq(format!("{}_{}", token.symbol, token.address)),
            schema::account::address.eq(token.address.as_ref()),
        ))
        .on_conflict_do_nothing()
        .execute(conn)
        .await
        .expect("Could not ensure token account in database");

    let account_id: i64 = schema::account::table
        .select(schema::account::id)
        .filter(schema::account::chain_id.eq(chain_id))
        .filter(schema::account::address.eq(token.address.as_ref()))
        .first(conn)
        .await
        .expect("Account must exist after insert");

    diesel::insert_into(schema::token::table)
        .values((
            schema::token::account_id.eq(account_id),
            schema::token::symbol.eq(&token.symbol),
            schema::token::decimals.eq(token.decimals as i32),
            schema::token::gas.eq(Vec::<Option<i64>>::new()),
            schema::token::quality.eq(100),
        ))
        .on_conflict_do_nothing()
        .execute(conn)
        .await
        .expect("Could not ensure token in database");

    let token_id: i64 = schema::token::table
        .select(schema::token::id)
        .filter(schema::token::account_id.eq(account_id))
        .first(conn)
        .await
        .expect("Token must exist after insert");

    let price = 10.0_f64.powi(token.decimals as i32);
    diesel::insert_into(schema::token_price::table)
        .values((schema::token_price::token_id.eq(token_id), schema::token_price::price.eq(price)))
        .on_conflict(schema::token_price::token_id)
        .do_update()
        .set(schema::token_price::price.eq(price))
        .execute(conn)
        .await
        .expect("Could not ensure token price in database");
}

async fn ensure_protocol_systems(protocol_systems: &[String], pool: Pool<AsyncPgConnection>) {
    let mut conn = pool.get().await.expect("connection ok");

    diesel::insert_into(schema::protocol_system::table)
        .values(
            protocol_systems
                .iter()
                .map(|ps| schema::protocol_system::name.eq(ps))
                .collect::<Vec<_>>(),
        )
        .on_conflict_do_nothing()
        .execute(&mut conn)
        .await
        .expect("Could not ensure protocol system enum's in database");

    debug!("Ensured protocol system enum presence for: {:?}", protocol_systems);
}

fn run_migrations(db_url: &str, retention_horizon: Option<NaiveDateTime>) {
    info!("Upgrading database...");
    let mut conn = PgConnection::establish(db_url).expect("Connection to database should succeed");
    conn.run_pending_migrations(MIGRATIONS)
        .expect("migrations should execute without errors");
    backfill_historical_partitions(&mut conn, retention_horizon)
        .expect("Failed to backfill historical partitions");
}

/// Ensures daily partitions exist for the past 3 months on all pg_partman-managed tables.
///
/// `pg_partman`'s `create_parent` only pre-creates future partitions (`p_premake` days ahead).
/// In a freshly created database — e.g. an integration-test environment replaying historical
/// blocks — there are no partitions for past dates, so archive rows with `valid_to` in that
/// range fall into the default partition. The default partition carries a `UNIQUE (entity_key)`
/// constraint that is already satisfied by the current row, which either aborts the write
/// transaction or (with `on_conflict_do_nothing`) silently drops the archive row, breaking
/// point-in-time reads at the stop block.
///
/// In production the partitions already exist (created day-by-day by pg_partman), so this call is
/// a no-op. In a fresh database it creates missing partitions for the replay/retention window.
///
/// The range defaults to:
/// - start: `retention_horizon` (if provided) or `now - 3 months`
/// - end: `now`
///
/// Integration tests can override bounds via:
/// - `PARTITION_BACKFILL_START` (NaiveDateTime format, e.g. `2024-01-01T00:00:00`)
/// - `PARTITION_BACKFILL_END` (same format)
fn backfill_historical_partitions(
    conn: &mut PgConnection,
    retention_horizon: Option<NaiveDateTime>,
) -> Result<(), diesel::result::Error> {
    const TABLES: &[&str] = &[
        "public.contract_storage",
        "public.protocol_state",
        "public.component_balance",
    ];

    let now = Utc::now().naive_utc();
    let default_start = retention_horizon.unwrap_or(now - ChronoDuration::days(90));
    let start = std::env::var("PARTITION_BACKFILL_START")
        .ok()
        .and_then(|v| NaiveDateTime::parse_from_str(&v, "%Y-%m-%dT%H:%M:%S").ok())
        .unwrap_or(default_start)
        .min(now);
    let end = std::env::var("PARTITION_BACKFILL_END")
        .ok()
        .and_then(|v| NaiveDateTime::parse_from_str(&v, "%Y-%m-%dT%H:%M:%S").ok())
        .unwrap_or(now)
        .max(start);

    for table in TABLES {
        diesel::RunQueryDsl::execute(
            diesel::sql_query(format!(
                "SELECT partman.create_partition_time(\
                    p_parent_table    := '{table}', \
                    p_partition_times := ARRAY(\
                        SELECT generate_series(\
                            date_trunc('day', $1),\
                            date_trunc('day', $2),\
                            '1 day'::interval\
                        )::timestamptz\
                    )\
                )"
            ))
            .bind::<diesel::sql_types::Timestamptz, _>(start)
            .bind::<diesel::sql_types::Timestamptz, _>(end),
            conn,
        )?;
    }
    Ok(())
}

// TODO: add cfg(test) once we have better mocks to be used in indexer crate
pub mod testing {
    //! # Reusable components to write tests against the DB.
    use std::future::Future;

    use diesel::sql_query;
    use diesel_async::{
        pooled_connection::{deadpool::Pool, AsyncDieselConnectionManager},
        AsyncPgConnection, RunQueryDsl,
    };

    async fn setup_pool() -> Pool<AsyncPgConnection> {
        let database_url =
            std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
        let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(database_url);
        Pool::builder(config).build().unwrap()
    }

    async fn teardown(conn: &mut AsyncPgConnection) {
        let tables = vec![
            // put block early so most FKs cascade, it would
            // be better to find the correct order tough.
            "extraction_state",
            "block",
            "contract_storage",
            "contract_code",
            "account_balance",
            "protocol_component_holds_token",
            "protocol_component_holds_contract",
            "component_balance",
            "token",
            "account",
            "protocol_state",
            "protocol_component",
            "protocol_type",
            "protocol_system",
            "transaction",
            "chain",
        ];
        for t in tables.iter() {
            sql_query(format!("DELETE FROM {t};"))
                .execute(conn)
                .await
                .unwrap_or_else(|e| panic!("Error truncating {t} table: {e}"));
        }
        dbg!("Teardown completed");
    }

    /// Run tests that require committing data to the db.
    ///
    /// This function will run tests that are expected to commit data into the database, e.g.
    /// because the test setups are too complex for using `begin_test_transaction`. Please only use
    /// this as a last resort as these tests are slow and have to be run serially. Using a test
    /// transaction is preferred where possible.  
    ///
    /// The method will pass a connection pool to the actual test function, catch any panics and
    /// then purge all data in the tables so that the next test can run from a clean slate.
    ///
    /// ## Interference with other tests
    /// While this function runs, the db will actually contain data.
    ///
    /// This is likely to interfere with other tests using this same function. To mitigate this, the
    /// test name or the package should contain the string `serial_db`, this way nextest will
    /// automatically put these test into a separate group.
    /// Other tests that rely on a empty db (most tests unsing test_transactions) will likely
    /// be affected if run in parrallel with tests using this function. CI will automatically
    /// partition the serial and parallel tests into two separate groups.
    ///
    /// ## Example
    /// ```
    /// use tycho_indexer::storage::postgres::testing::run_against_db;
    ///
    /// #[tokio::test]
    /// async fn test_serial_db_mytest_name() {
    ///     run_against_db(|connection_pool| async move {
    ///         println!("here goes actual test code")
    ///     }).await;
    /// }
    /// ```
    ///
    /// ## Future
    /// We should consider moving these test to their own database. That would require running
    /// migrations on these databases though. For now tests run fast enough though.
    pub async fn run_against_db<F, Fut>(test_f: F)
    where
        F: FnOnce(Pool<AsyncPgConnection>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let connection_pool = setup_pool().await;
        let inner_pool = connection_pool.clone();
        let res = tokio::spawn(async move {
            test_f(inner_pool).await;
        })
        .await;

        let mut connection = connection_pool
            .get()
            .await
            .expect("Failed to get a connection from the pool");

        teardown(&mut connection).await;
        res.unwrap();
    }
}

// TODO: add cfg(test) once we have better mocks to be used in indexer crate
pub mod db_fixtures {
    //! # General Purpose Fixtures for Database State Modification
    //!
    //! The module contains fixtures that are designed to alter the database state
    //! for testing purposes.
    //!
    //! This module doesn't rely on any locally specific code from the Postgres
    //! packages, except for the autogenerated `schema` module. Given that `schema`
    //! is generated by examining our table schema, it's reasonable to assert that
    //! this module belongs to the `schema` and not the package itself.
    //!
    //! A key goal of these fixtures is to prevent reliance on application code when
    //! setting up test data, thereby avoiding cyclical dependencies. For example,
    //! if you're modifying how an entity is inserted, and this change affects the
    //! data setup for other tests, these tests would start failing – a situation we
    //! want to avoid. This could lead to complex, hard-to-resolve issues,
    //! particularly if you're using the insertion method to validate that a second
    //! insertion fails, while simultaneously working on the insertion method. In
    //! such cases, running your tests becomes impossible if the insertion method
    //! encounters bugs.
    //!
    //! # Heads Up
    //! We advise adding only general-purpose methods to this module, such as those
    //! for adding or removing a single row/entry, or maximum entries along with
    //! their child entities. More intricate setups should be localized where they
    //! are explicitly used.
    //!
    //! If you need to share more complex setups and decide to include them here,
    //! please think through whether this is the suitable location, or whether a
    //! local copy might serve your needs better. For instance, if the complete
    //! shared setup isn't necessary for your test case, copy it and keep only
    //! the entries that are crucial to your test case.
    use std::str::FromStr;

    use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime};
    use diesel::{prelude::*, sql_query};
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use serde_json::Value;
    use tycho_common::{
        keccak256,
        models::{Balance, Code, FinancialType, ImplementationType},
        Bytes,
    };

    use super::{schema, MAX_TS};
    use crate::postgres::orm;

    // Insert a new chain
    pub async fn insert_chain(conn: &mut AsyncPgConnection, name: &str) -> i64 {
        diesel::insert_into(schema::chain::table)
            .values(schema::chain::name.eq(name))
            .returning(schema::chain::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    pub fn yesterday_midnight() -> NaiveDateTime {
        let ts = chrono::Local::now().naive_utc() - chrono::Duration::days(1);
        NaiveDateTime::new(
            NaiveDate::from_ymd_opt(ts.year(), ts.month(), ts.day()).unwrap(),
            NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
        )
    }

    pub fn yesterday_half_past_midnight() -> NaiveDateTime {
        let ts = chrono::Local::now().naive_utc() - chrono::Duration::days(1);
        NaiveDateTime::new(
            NaiveDate::from_ymd_opt(ts.year(), ts.month(), ts.day()).unwrap(),
            NaiveTime::from_hms_opt(0, 30, 0).unwrap(),
        )
    }

    pub fn yesterday_one_am() -> NaiveDateTime {
        let ts = chrono::Local::now().naive_utc() - chrono::Duration::days(1);
        NaiveDateTime::new(
            NaiveDate::from_ymd_opt(ts.year(), ts.month(), ts.day()).unwrap(),
            NaiveTime::from_hms_opt(1, 0, 0).unwrap(),
        )
    }

    /// Inserts two sequential blocks
    pub async fn insert_blocks(conn: &mut AsyncPgConnection, chain_id: i64) -> Vec<i64> {
        let block_records = vec![
            (
                schema::block::hash.eq(Vec::from(
                    Bytes::from_str(
                        "88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6",
                    )
                    .unwrap(),
                )),
                schema::block::parent_hash.eq(Vec::from(
                    Bytes::from_str(
                        "d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3",
                    )
                    .unwrap(),
                )),
                schema::block::number.eq(1),
                schema::block::ts.eq(yesterday_midnight()),
                schema::block::chain_id.eq(chain_id),
            ),
            (
                schema::block::hash.eq(Vec::from(
                    Bytes::from_str(
                        "b495a1d7e6663152ae92708da4843337b958146015a2802f4193a410044698c9",
                    )
                    .unwrap(),
                )),
                schema::block::parent_hash.eq(Vec::from(
                    Bytes::from_str(
                        "88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6",
                    )
                    .unwrap(),
                )),
                schema::block::number.eq(2),
                schema::block::ts.eq(yesterday_half_past_midnight()),
                schema::block::chain_id.eq(chain_id),
            ),
        ];
        diesel::insert_into(schema::block::table)
            .values(&block_records)
            .returning(schema::block::id)
            .get_results(conn)
            .await
            .unwrap()
    }

    /// Insert a bunch of transactions using (block_id, index, hash)
    pub async fn insert_txns(conn: &mut AsyncPgConnection, txns: &[(i64, i64, &str)]) -> Vec<i64> {
        let from_val = Bytes::from_str("4648451b5F87FF8F0F7D622bD40574bb97E25980").unwrap();
        let to_val = Bytes::from_str("6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();
        let data: Vec<_> = txns
            .iter()
            .map(|(b, i, h)| {
                use schema::transaction::dsl::*;
                (
                    block_id.eq(b),
                    index.eq(i),
                    hash.eq(Bytes::from_str(h).expect("valid txhash")),
                    from.eq(from_val.clone()),
                    to.eq(to_val.clone()),
                )
            })
            .collect();
        diesel::insert_into(schema::transaction::table)
            .values(&data)
            .returning(schema::transaction::id)
            .get_results(conn)
            .await
            .unwrap()
    }

    pub async fn insert_account(
        conn: &mut AsyncPgConnection,
        address: &str,
        title: &str,
        chain_id: i64,
        tx_id: Option<i64>,
    ) -> i64 {
        let ts: Option<NaiveDateTime> = if let Some(id) = tx_id {
            Some(
                schema::transaction::table
                    .inner_join(schema::block::table)
                    .filter(schema::transaction::id.eq(id))
                    .select(schema::block::ts)
                    .first::<NaiveDateTime>(conn)
                    .await
                    .expect("setup tx id not found"),
            )
        } else {
            None
        };

        let query = diesel::insert_into(schema::account::table).values((
            schema::account::title.eq(title),
            schema::account::chain_id.eq(chain_id),
            schema::account::creation_tx.eq(tx_id),
            schema::account::created_at.eq(ts),
            schema::account::address.eq(hex::decode(address).unwrap()),
        ));
        query
            .returning(schema::account::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    pub async fn insert_slots(
        conn: &mut AsyncPgConnection,
        contract_id: i64,
        modify_tx: i64,
        valid_from: &NaiveDateTime,
        valid_to: Option<&NaiveDateTime>,
        slots: &[(u64, u64, Option<u64>)],
    ) {
        let data = slots
            .iter()
            .enumerate()
            .map(|(idx, (k, v, pv))| {
                let previous_value = pv.map(|pv| hex::decode(format!("{pv:064x}")).unwrap());
                (
                    schema::contract_storage::slot.eq(hex::decode(format!("{k:064x}")).unwrap()),
                    schema::contract_storage::value.eq(hex::decode(format!("{v:064x}")).unwrap()),
                    schema::contract_storage::previous_value.eq(previous_value),
                    schema::contract_storage::account_id.eq(contract_id),
                    schema::contract_storage::modify_tx.eq(modify_tx),
                    schema::contract_storage::valid_from.eq(valid_from),
                    schema::contract_storage::valid_to.eq(valid_to.unwrap_or(&MAX_TS)),
                    schema::contract_storage::ordinal.eq(idx as i64),
                )
            })
            .collect::<Vec<_>>();

        diesel::insert_into(schema::contract_storage::table)
            .values(&data)
            .execute(conn)
            .await
            .unwrap();
    }

    pub async fn insert_account_balance(
        conn: &mut AsyncPgConnection,
        new_balance: u64,
        token_id: i64,
        tx_id: i64,
        end_ts: Option<&NaiveDateTime>,
        account: i64,
    ) {
        let ts = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(tx_id))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");

        let mut b0 = [0; 32];
        let new_balance_bytes = new_balance.to_be_bytes();
        b0[24..].copy_from_slice(&new_balance_bytes);

        {
            diesel::insert_into(schema::account_balance::table)
                .values((
                    schema::account_balance::account_id.eq(account),
                    schema::account_balance::balance.eq(b0.as_slice()),
                    schema::account_balance::token_id.eq(token_id),
                    schema::account_balance::modify_tx.eq(tx_id),
                    schema::account_balance::valid_from.eq(ts),
                    schema::account_balance::valid_to.eq(end_ts),
                ))
                .execute(conn)
                .await
                .expect("balance insert ok");
        }
    }

    pub async fn insert_contract_code(
        conn: &mut AsyncPgConnection,
        account_id: i64,
        modify_tx: i64,
        code: Code,
    ) -> i64 {
        let ts = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(modify_tx))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");

        let data = (
            schema::contract_code::code.eq(&code),
            schema::contract_code::hash.eq(Bytes::from(&keccak256(&code))),
            schema::contract_code::account_id.eq(account_id),
            schema::contract_code::modify_tx.eq(modify_tx),
            schema::contract_code::valid_from.eq(ts),
        );

        diesel::insert_into(schema::contract_code::table)
            .values(data)
            .returning(schema::contract_code::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    pub async fn delete_account(conn: &mut AsyncPgConnection, target_id: i64, ts: &NaiveDateTime) {
        {
            use schema::account::dsl::*;
            diesel::update(account.filter(id.eq(target_id)))
                .set(deleted_at.eq(ts))
                .execute(conn)
                .await
                .expect("delete account table ok");
        }
        {
            use schema::account_balance::dsl::*;
            diesel::update(account_balance.filter(account_id.eq(target_id)))
                .set(valid_to.eq(ts))
                .execute(conn)
                .await
                .expect("delete balance table ok");
        }
        {
            use schema::contract_code::dsl::*;
            diesel::update(contract_code.filter(account_id.eq(target_id)))
                .set(valid_to.eq(ts))
                .execute(conn)
                .await
                .expect("delete code table ok");
        }
        {
            use schema::contract_storage::dsl::*;
            diesel::update(contract_storage.filter(account_id.eq(target_id)))
                .set(valid_to.eq(ts))
                .execute(conn)
                .await
                .expect("delete storage table ok");
        }
    }

    // Insert a new Component Balance
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_component_balance(
        conn: &mut AsyncPgConnection,
        balance: Balance,
        previous_balance: Balance,
        balance_float: f64,
        token_id: i64,
        tx_id: i64,
        protocol_component_id: i64,
        valid_to_tx: Option<i64>,
    ) {
        let ts: NaiveDateTime = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(tx_id))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");
        let valid_to_ts = match &valid_to_tx {
            Some(tx) => schema::transaction::table
                .inner_join(schema::block::table)
                .filter(schema::transaction::id.eq(tx))
                .select(schema::block::ts)
                .first::<NaiveDateTime>(conn)
                .await
                .expect("setup tx id not found"),
            None => MAX_TS,
        };
        diesel::insert_into(schema::component_balance::table)
            .values((
                schema::component_balance::protocol_component_id.eq(protocol_component_id),
                schema::component_balance::token_id.eq(token_id),
                schema::component_balance::modify_tx.eq(tx_id),
                schema::component_balance::new_balance.eq(balance),
                schema::component_balance::balance_float.eq(balance_float),
                schema::component_balance::previous_value.eq(previous_balance),
                schema::component_balance::valid_from.eq(ts),
                schema::component_balance::valid_to.eq(valid_to_ts),
            ))
            .execute(conn)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "component balance insert failed {token_id} {protocol_component_id} {balance_float}"
                )
            });
    }

    // Insert a new Protocol System
    pub async fn insert_protocol_system(conn: &mut AsyncPgConnection, name: String) -> i64 {
        diesel::insert_into(schema::protocol_system::table)
            .values(schema::protocol_system::name.eq(name))
            .returning(schema::protocol_system::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    // Insert a new Protocol Type
    pub async fn insert_protocol_type(
        conn: &mut AsyncPgConnection,
        name: &str,
        financial_type: Option<FinancialType>,
        attribute: Option<Value>,
        implementation_type: Option<ImplementationType>,
    ) -> i64 {
        let financial_type: orm::FinancialType = financial_type
            .unwrap_or(FinancialType::Swap)
            .into();
        let implementation_type: orm::ImplementationType = implementation_type
            .unwrap_or(ImplementationType::Custom)
            .into();
        let query = diesel::insert_into(schema::protocol_type::table).values((
            schema::protocol_type::name.eq(name),
            schema::protocol_type::financial_type.eq(financial_type),
            schema::protocol_type::attribute_schema.eq(attribute),
            schema::protocol_type::implementation.eq(implementation_type),
        ));
        query
            .returning(schema::protocol_type::id)
            .get_result(conn)
            .await
            .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    // Insert a new Protocol Component
    pub async fn insert_protocol_component(
        conn: &mut AsyncPgConnection,
        id: &str,
        chain_id: i64,
        system_id: i64,
        type_id: i64,
        tx_id: i64,
        token_ids: Option<Vec<i64>>,
        contract_code_ids: Option<Vec<i64>>,
    ) -> i64 {
        let ts: NaiveDateTime = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(tx_id))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");

        let query = diesel::insert_into(schema::protocol_component::table).values((
            schema::protocol_component::external_id.eq(id),
            schema::protocol_component::chain_id.eq(chain_id),
            schema::protocol_component::protocol_type_id.eq(type_id),
            schema::protocol_component::protocol_system_id.eq(system_id),
            schema::protocol_component::creation_tx.eq(tx_id),
            schema::protocol_component::created_at.eq(ts),
        ));
        let component_id = query
            .returning(schema::protocol_component::id)
            .get_result(conn)
            .await
            .unwrap();

        if let Some(t_ids) = token_ids {
            diesel::insert_into(schema::protocol_component_holds_token::table)
                .values(
                    t_ids
                        .iter()
                        .enumerate()
                        .map(|(t_idx, t_id)| {
                            (
                                schema::protocol_component_holds_token::protocol_component_id
                                    .eq(component_id),
                                schema::protocol_component_holds_token::token_id.eq(t_id),
                                schema::protocol_component_holds_token::token_index
                                    .eq(t_idx as i16),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
                .execute(conn)
                .await
                .expect("protocol component holds token insert ok");
        }

        if let Some(cc_ids) = contract_code_ids {
            diesel::insert_into(schema::protocol_component_holds_contract::table)
                .values(
                    cc_ids
                        .iter()
                        .map(|cc_id| {
                            (
                                schema::protocol_component_holds_contract::protocol_component_id
                                    .eq(component_id),
                                schema::protocol_component_holds_contract::contract_code_id
                                    .eq(cc_id),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
                .execute(conn)
                .await
                .expect("protocol component holds contract code insert ok");
        }
        component_id
    }

    // Insert a new Protocol State
    pub async fn insert_protocol_state(
        conn: &mut AsyncPgConnection,
        component_id: i64,
        tx_id: i64,
        attribute_name: String,
        attribute_value: Bytes,
        previous_value: Option<Bytes>,
        valid_to_tx: Option<i64>,
    ) {
        let ts: NaiveDateTime = schema::transaction::table
            .inner_join(schema::block::table)
            .filter(schema::transaction::id.eq(tx_id))
            .select(schema::block::ts)
            .first::<NaiveDateTime>(conn)
            .await
            .expect("setup tx id not found");
        let valid_to_ts: Option<NaiveDateTime> = match &valid_to_tx {
            Some(tx) => Some(
                schema::transaction::table
                    .inner_join(schema::block::table)
                    .filter(schema::transaction::id.eq(tx))
                    .select(schema::block::ts)
                    .first::<NaiveDateTime>(conn)
                    .await
                    .expect("setup tx id not found"),
            ),
            None => None,
        };

        let query = diesel::insert_into(schema::protocol_state::table).values((
            schema::protocol_state::protocol_component_id.eq(component_id),
            schema::protocol_state::modify_tx.eq(tx_id),
            schema::protocol_state::modified_ts.eq(ts),
            schema::protocol_state::valid_from.eq(ts),
            schema::protocol_state::valid_to.eq(valid_to_ts.unwrap_or(MAX_TS)),
            schema::protocol_state::attribute_name.eq(attribute_name),
            schema::protocol_state::attribute_value.eq(attribute_value),
            schema::protocol_state::previous_value.eq(previous_value),
        ));
        query
            .execute(conn)
            .await
            .expect("protocol state insert ok");
    }

    pub async fn insert_token(
        conn: &mut AsyncPgConnection,
        chain_id: i64,
        address: &str,
        symbol: &str,
        decimals: i32,
        quality: Option<i32>,
    ) -> (i64, i64) {
        let title = format!("token_{symbol}");
        let account_id = insert_account(conn, address, &title, chain_id, None).await;

        let quality = quality.unwrap_or(0);

        let query = diesel::insert_into(schema::token::table).values((
            schema::token::account_id.eq(account_id),
            schema::token::symbol.eq(symbol),
            schema::token::decimals.eq(decimals),
            schema::token::tax.eq(10),
            schema::token::gas.eq(vec![10]),
            schema::token::quality.eq(quality),
        ));
        (
            account_id,
            query
                .returning(schema::token::id)
                .get_result(conn)
                .await
                .unwrap(),
        )
    }

    pub async fn get_token_by_symbol(conn: &mut AsyncPgConnection, symbol: String) -> orm::Token {
        schema::token::table
            .filter(schema::token::symbol.eq(symbol.clone()))
            .select(schema::token::all_columns)
            .first::<orm::Token>(conn)
            .await
            .unwrap()
    }

    pub async fn insert_token_prices(data: &[(i64, f64)], conn: &mut AsyncPgConnection) {
        diesel::insert_into(schema::token_price::table)
            .values(
                data.iter()
                    .map(|(tid, price)| {
                        (
                            schema::token_price::token_id.eq(tid),
                            schema::token_price::price.eq(price),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .execute(conn)
            .await
            .expect("Inserting token prices fixture failed");
    }

    pub async fn calculate_component_tvl(conn: &mut AsyncPgConnection) {
        sql_query(
            r#"
        INSERT INTO component_tvl (protocol_component_id, tvl)
        SELECT 
            bal.protocol_component_id as protocol_component_id,
            SUM(bal.balance_float * token_price.price / POWER(10.0, token.decimals)) as tvl
        FROM 
            component_balance AS bal 
        INNER JOIN 
            token_price ON bal.token_id = token_price.token_id 
        INNER JOIN
            token ON bal.token_id = token.id
        WHERE 
            bal.valid_to = '262142-12-31 23:59:59.999999'
        GROUP BY 
            bal.protocol_component_id
        ON CONFLICT (protocol_component_id) 
        DO UPDATE SET 
            tvl = EXCLUDED.tvl;
        "#,
        )
        .execute(conn)
        .await
        .expect("calculating fixture component tvl failed");
    }
}
