use std::{cmp::Ordering, collections::HashMap};

use chrono::NaiveDateTime;
use diesel::{dsl::exists, pg::Pg, prelude::*};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{
        protocol::QualityRange,
        token::{Token, TokenOwner},
        Address, Chain, ComponentId, PaginationParams,
    },
    storage::{StorageError, WithTotal},
    Bytes,
};

use super::snapshot::{begin_snapshot, end_snapshot};
use crate::postgres::{
    instance::{
        active::active_instance_ids,
        gateway::PostgresGatewayNew,
        schema::tycho::{component_balance, protocol_component, protocol_instance},
    },
    orm,
    protocol::compare_balances,
    schema::{protocol_system, token, token_price},
    PostgresError,
};

impl PostgresGatewayNew {
    /// Returns the matching tokens in token id order, and the number of matching tokens on all
    /// pages.
    ///
    /// - `address`: only tokens with these addresses.
    /// - `quality`: only tokens with a quality in this range.
    /// - `traded_n_days_ago`: only tokens with a balance that changed after this time in a
    ///   component of an active instance.
    /// - `pagination_params`: the page of tokens to return. Without it, all matching tokens.
    ///
    /// `NotFound` for an unknown chain.
    pub async fn get_tokens(
        &self,
        chain: Chain,
        address: Option<&[&Address]>,
        quality: QualityRange,
        traded_n_days_ago: Option<NaiveDateTime>,
        pagination_params: Option<&PaginationParams>,
        conn: &mut AsyncPgConnection,
    ) -> Result<WithTotal<Vec<Token>>, StorageError> {
        self.get_chain_id(&chain)?;
        let snapshot = begin_snapshot(conn).await?;
        let result =
            tokens_of(conn, chain, address, quality, traded_n_days_ago, pagination_params).await;
        end_snapshot(conn, snapshot, result).await
    }

    /// Returns the price of each token that has one, by token address.
    ///
    /// `NotFound` for an unknown chain.
    pub async fn get_token_prices(
        &self,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Bytes, f64>, StorageError> {
        self.get_chain_id(chain)?;
        let snapshot = begin_snapshot(conn).await?;
        let result: Result<Vec<(Bytes, f64)>, StorageError> = token_price::table
            .inner_join(token::table)
            .select((token::address.assume_not_null(), token_price::price))
            .load(conn)
            .await
            .map_err(|err| PostgresError::from(err).into());
        let prices = end_snapshot(conn, snapshot, result).await?;
        Ok(prices.into_iter().collect())
    }

    /// Returns, per token, the component of an active instance with the largest balance of the
    /// token, and the component's protocol system.
    ///
    /// - `min_balance`: only balances whose float value is at least this.
    ///
    /// Balances compare as unsigned integers. On a tie, the component of the lower instance id,
    /// then the lower component id, stays. A token without such a balance is absent. `NotFound`
    /// for an unknown chain.
    pub async fn get_token_owners(
        &self,
        chain: &Chain,
        tokens: &[Address],
        min_balance: Option<f64>,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Address, TokenOwner>, StorageError> {
        self.get_chain_id(chain)?;
        let snapshot = begin_snapshot(conn).await?;
        let result = owners_of(conn, tokens, min_balance).await;
        end_snapshot(conn, snapshot, result).await
    }
}

/// Reads the tokens of [`PostgresGatewayNew::get_tokens`] and the number of matching tokens on
/// all pages.
async fn tokens_of(
    conn: &mut AsyncPgConnection,
    chain: Chain,
    address: Option<&[&Address]>,
    quality: QualityRange,
    traded_n_days_ago: Option<NaiveDateTime>,
    pagination_params: Option<&PaginationParams>,
) -> Result<WithTotal<Vec<Token>>, StorageError> {
    let instances = match traded_n_days_ago {
        Some(_) => active_instance_ids(conn).await?,
        None => Vec::new(),
    };
    let filtered = || {
        let mut query = token::table.into_boxed::<Pg>();
        if let Some(address) = address {
            query = query.filter(
                token::address
                    .assume_not_null()
                    .eq_any(address),
            );
        }
        if let Some(min) = quality.min {
            query = query.filter(token::quality.ge(min));
        }
        if let Some(max) = quality.max {
            query = query.filter(token::quality.le(max));
        }
        if let Some(since) = traded_n_days_ago {
            query = query.filter(exists(
                component_balance::table
                    .filter(component_balance::instance_id.eq_any(&instances))
                    .filter(component_balance::token.eq(token::address.assume_not_null()))
                    .filter(component_balance::valid_from.gt(since)),
            ));
        }
        query
    };

    let total = filtered()
        .count()
        .get_result::<i64>(conn)
        .await
        .map_err(PostgresError::from)?;
    let mut query = filtered()
        .order_by(token::id)
        .select((orm::Token::as_select(), token::address.assume_not_null()));
    if let Some(pagination) = pagination_params {
        query = query
            .limit(pagination.page_size)
            .offset(pagination.offset());
    }
    let rows: Vec<(orm::Token, Address)> = query
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

    let tokens = rows
        .into_iter()
        .map(|(row, address)| row.to_model(&address, chain))
        .collect::<Result<_, _>>()?;
    Ok(WithTotal { entity: tokens, total: Some(total) })
}

/// Reads the token owners of [`PostgresGatewayNew::get_token_owners`], by token address.
async fn owners_of(
    conn: &mut AsyncPgConnection,
    tokens: &[Address],
    min_balance: Option<f64>,
) -> Result<HashMap<Address, TokenOwner>, StorageError> {
    let instances = active_instance_ids(conn).await?;
    if instances.is_empty() {
        return Ok(HashMap::new());
    }

    let mut query = component_balance::table
        .inner_join(
            protocol_component::table.on(protocol_component::instance_id
                .eq(component_balance::instance_id)
                .and(protocol_component::id.eq(component_balance::protocol_component_id))),
        )
        .inner_join(
            protocol_instance::table.on(protocol_instance::id.eq(protocol_component::instance_id)),
        )
        .inner_join(
            protocol_system::table
                .on(protocol_system::id.eq(protocol_instance::protocol_system_id)),
        )
        .filter(component_balance::instance_id.eq_any(&instances))
        .filter(protocol_component::instance_id.eq_any(&instances))
        .filter(component_balance::token.eq_any(tokens))
        .order_by((component_balance::instance_id, component_balance::protocol_component_id))
        .select((
            component_balance::token,
            protocol_system::name,
            protocol_component::external_id,
            component_balance::new_balance,
        ))
        .into_boxed::<Pg>();
    if let Some(min_balance) = min_balance {
        query = query.filter(component_balance::balance_float.ge(min_balance));
    }
    let rows: Vec<(Address, String, ComponentId, Bytes)> = query
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

    let mut owners: HashMap<Address, TokenOwner> = HashMap::new();
    for (token, protocol_system, component_id, balance) in rows {
        let larger = owners
            .get(&token)
            .is_none_or(|current| {
                compare_balances(&balance, &current.balance) == Ordering::Greater
            });
        if larger {
            owners.insert(token, TokenOwner { protocol_system, component_id, balance });
        }
    }
    Ok(owners)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, NaiveDateTime};
    use diesel::prelude::*;
    use diesel_async::{
        AnsiTransactionManager, AsyncPgConnection, RunQueryDsl, TransactionManager,
    };
    use tycho_common::{
        models::{
            protocol::QualityRange,
            token::{Token, TokenOwner},
            Address, Chain, PaginationParams,
        },
        storage::StorageError,
        Bytes,
    };

    use crate::postgres::{
        db_fixtures,
        instance::{gateway::PostgresGatewayNew, InstanceId},
        schema,
        testing::instance_test_connection,
    };

    const A: &str = "0x00000000000000000000000000000000000000a1";
    const B: &str = "0x00000000000000000000000000000000000000b1";
    const C: &str = "0x00000000000000000000000000000000000000c1";

    struct Setup {
        gateway: PostgresGatewayNew,
        chain_id: i64,
        active: InstanceId,
        inactive: InstanceId,
        uniswap_v4: InstanceId,
    }

    /// "vm:curve" and "uniswap_v4" on ethereum, each pointing to an instance. "vm:curve" has a
    /// second `ready` instance it does not point to.
    async fn setup(conn: &mut AsyncPgConnection) -> Setup {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        let curve_system_id =
            db_fixtures::insert_protocol_system(conn, "vm:curve".to_string()).await;
        let uniswap_v4_system_id =
            db_fixtures::insert_protocol_system(conn, "uniswap_v4".to_string()).await;
        let active = db_fixtures::insert_instance(conn, curve_system_id, "0.0.1").await;
        let inactive = db_fixtures::insert_instance(conn, curve_system_id, "0.0.2").await;
        let uniswap_v4 = db_fixtures::insert_instance(conn, uniswap_v4_system_id, "0.0.1").await;
        db_fixtures::set_active_instance(conn, curve_system_id, Some(active)).await;
        db_fixtures::set_active_instance(conn, uniswap_v4_system_id, Some(uniswap_v4)).await;
        let gateway = PostgresGatewayNew::from_connection(conn).await;
        Setup { gateway, chain_id, active, inactive, uniswap_v4 }
    }

    /// Inserts a token at the `0x`-prefixed `address` and returns its id.
    async fn token(
        conn: &mut AsyncPgConnection,
        chain_id: i64,
        address: &str,
        symbol: &str,
        quality: i32,
    ) -> i64 {
        let hex = address.trim_start_matches("0x");
        db_fixtures::insert_token(conn, chain_id, hex, symbol, 18, Some(quality))
            .await
            .1
    }

    /// Inserts a pool of the instance that holds `token`, with a balance of it.
    async fn traded(conn: &mut AsyncPgConnection, instance: InstanceId, token: &str) {
        pool(conn, instance, "pool", token, "0x01", 1.0).await;
    }

    /// Inserts the pool `external_id` of the instance with a balance of `token`.
    async fn pool(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
        external_id: &str,
        token: &str,
        balance: &str,
        balance_float: f64,
    ) {
        let id =
            db_fixtures::insert_instance_component(conn, instance, external_id, &[b(token)]).await;
        db_fixtures::insert_instance_component_balance(
            conn,
            instance,
            id,
            b(token),
            b(balance),
            balance_float,
        )
        .await;
    }

    fn owner(protocol_system: &str, component_id: &str, balance: &str) -> TokenOwner {
        TokenOwner {
            protocol_system: protocol_system.to_string(),
            component_id: component_id.to_string(),
            balance: b(balance),
        }
    }

    fn b(hex: &str) -> Bytes {
        Bytes::from(hex)
    }

    fn addresses(tokens: &[Token]) -> Vec<Address> {
        tokens
            .iter()
            .map(|token| token.address.clone())
            .collect()
    }

    #[tokio::test]
    async fn test_tokens_by_address() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = token(&mut conn, s.chain_id, A, "A", 100).await;
        token(&mut conn, s.chain_id, B, "B", 100).await;
        // The account keeps its address, so only a read of token.address finds the token at C.
        diesel::update(schema::token::table.find(a))
            .set(schema::token::address.eq(b(C)))
            .execute(&mut conn)
            .await
            .expect("moving the token address failed");

        let result = s
            .gateway
            .get_tokens(
                Chain::Ethereum,
                Some(&[&b(C)]),
                QualityRange::None(),
                None,
                None,
                &mut conn,
            )
            .await
            .unwrap();

        assert_eq!(addresses(&result.entity), vec![b(C)]);
        assert_eq!(result.entity[0].symbol, "A");
        assert_eq!(result.total, Some(1));
    }

    #[tokio::test]
    async fn test_tokens_by_quality() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, A, "A", 5).await;
        token(&mut conn, s.chain_id, B, "B", 100).await;

        let result = s
            .gateway
            .get_tokens(Chain::Ethereum, None, QualityRange::min_only(50), None, None, &mut conn)
            .await
            .unwrap();

        assert_eq!(addresses(&result.entity), vec![b(B)]);
        assert_eq!(result.total, Some(1));
    }

    #[tokio::test]
    async fn test_traded_tokens_count_active_instances_only() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, A, "A", 100).await;
        token(&mut conn, s.chain_id, B, "B", 100).await;
        traded(&mut conn, s.active, A).await;
        traded(&mut conn, s.inactive, B).await;
        let before_balances: NaiveDateTime = NaiveDate::from_ymd_opt(2019, 12, 31)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date");

        let result = s
            .gateway
            .get_tokens(
                Chain::Ethereum,
                None,
                QualityRange::None(),
                Some(before_balances),
                None,
                &mut conn,
            )
            .await
            .unwrap();

        assert_eq!(addresses(&result.entity), vec![b(A)]);
        assert_eq!(result.total, Some(1));
    }

    #[tokio::test]
    async fn test_traded_tokens_skip_older_balance_changes() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        token(&mut conn, s.chain_id, A, "A", 100).await;
        traded(&mut conn, s.active, A).await;
        let after_balances: NaiveDateTime = NaiveDate::from_ymd_opt(2020, 1, 2)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date");

        let result = s
            .gateway
            .get_tokens(
                Chain::Ethereum,
                None,
                QualityRange::None(),
                Some(after_balances),
                None,
                &mut conn,
            )
            .await
            .unwrap();

        assert_eq!(addresses(&result.entity), Vec::<Address>::new());
        assert_eq!(result.total, Some(0));
    }

    #[tokio::test]
    async fn test_tokens_paging() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        for (address, symbol) in [(A, "A"), (B, "B"), (C, "C")] {
            token(&mut conn, s.chain_id, address, symbol, 100).await;
        }

        let result = s
            .gateway
            .get_tokens(
                Chain::Ethereum,
                None,
                QualityRange::None(),
                None,
                Some(&PaginationParams::new(0, 2)),
                &mut conn,
            )
            .await
            .unwrap();

        assert_eq!(addresses(&result.entity), vec![b(A), b(B)]);
        assert_eq!(result.total, Some(3));
    }

    #[tokio::test]
    async fn test_token_prices() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = token(&mut conn, s.chain_id, A, "A", 100).await;
        let b_id = token(&mut conn, s.chain_id, B, "B", 100).await;
        db_fixtures::insert_token_prices(&[(a, 1.0), (b_id, 2.0)], &mut conn).await;

        let prices = s
            .gateway
            .get_token_prices(&Chain::Ethereum, &mut conn)
            .await
            .unwrap();

        assert_eq!(prices, HashMap::from([(b(A), 1.0), (b(B), 2.0)]));
    }

    #[tokio::test]
    async fn test_token_reads_of_unknown_chain_fail() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;

        let tokens = s
            .gateway
            .get_tokens(Chain::Base, None, QualityRange::None(), None, None, &mut conn)
            .await;
        let prices = s
            .gateway
            .get_token_prices(&Chain::Base, &mut conn)
            .await;

        assert!(matches!(tokens, Err(StorageError::NotFound(..))), "{tokens:?}");
        assert!(matches!(prices, Err(StorageError::NotFound(..))), "{prices:?}");
    }

    #[tokio::test]
    async fn test_owner_is_the_largest_balance_across_protocols() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        pool(&mut conn, s.active, "curve_pool", A, "0xfe", 254.0).await;
        pool(&mut conn, s.uniswap_v4, "v4_pool", A, "0x00ff", 255.0).await;

        let owners = s
            .gateway
            .get_token_owners(&Chain::Ethereum, &[b(A)], None, &mut conn)
            .await
            .unwrap();

        assert_eq!(owners, HashMap::from([(b(A), owner("uniswap_v4", "v4_pool", "0x00ff"))]));
    }

    #[tokio::test]
    async fn test_owners_skip_instances_that_are_not_active() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        pool(&mut conn, s.active, "active_pool", A, "0x01", 1.0).await;
        pool(&mut conn, s.inactive, "inactive_pool", A, "0xff", 255.0).await;

        let owners = s
            .gateway
            .get_token_owners(&Chain::Ethereum, &[b(A)], None, &mut conn)
            .await
            .unwrap();

        assert_eq!(owners, HashMap::from([(b(A), owner("vm:curve", "active_pool", "0x01"))]));
    }

    #[tokio::test]
    async fn test_owners_respect_min_balance() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        pool(&mut conn, s.active, "pool", A, "0x0a", 10.0).await;

        let below = s
            .gateway
            .get_token_owners(&Chain::Ethereum, &[b(A)], Some(100.0), &mut conn)
            .await
            .unwrap();
        let above = s
            .gateway
            .get_token_owners(&Chain::Ethereum, &[b(A)], Some(5.0), &mut conn)
            .await
            .unwrap();

        assert_eq!(below, HashMap::new());
        assert_eq!(above, HashMap::from([(b(A), owner("vm:curve", "pool", "0x0a"))]));
    }

    #[tokio::test]
    async fn test_token_without_balance_has_no_owner() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        pool(&mut conn, s.active, "pool", A, "0x01", 1.0).await;

        let owners = s
            .gateway
            .get_token_owners(&Chain::Ethereum, &[b(A), b(B)], None, &mut conn)
            .await
            .unwrap();

        assert_eq!(owners, HashMap::from([(b(A), owner("vm:curve", "pool", "0x01"))]));
    }

    fn depth(conn: &mut AsyncPgConnection) -> Option<u32> {
        AnsiTransactionManager::transaction_manager_status_mut(conn)
            .transaction_depth()
            .expect("a valid transaction manager")
            .map(u32::from)
    }

    #[tokio::test]
    async fn test_token_reads_leave_no_transaction_open() {
        let mut conn = instance_test_connection().await;
        let s = setup(&mut conn).await;
        let a = token(&mut conn, s.chain_id, A, "A", 100).await;
        db_fixtures::insert_token_prices(&[(a, 1.0)], &mut conn).await;
        traded(&mut conn, s.active, A).await;
        let since: NaiveDateTime = NaiveDate::from_ymd_opt(2019, 12, 31)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date");
        let before = depth(&mut conn);

        s.gateway
            .get_tokens(Chain::Ethereum, None, QualityRange::None(), Some(since), None, &mut conn)
            .await
            .unwrap();
        assert_eq!(depth(&mut conn), before);
        s.gateway
            .get_token_owners(&Chain::Ethereum, &[b(A)], None, &mut conn)
            .await
            .unwrap();
        assert_eq!(depth(&mut conn), before);
        s.gateway
            .get_token_prices(&Chain::Ethereum, &mut conn)
            .await
            .unwrap();
        assert_eq!(depth(&mut conn), before);
    }
}
