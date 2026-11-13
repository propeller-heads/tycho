use std::collections::HashMap;

use chrono::NaiveDateTime;
use diesel::{dsl::exists, pg::Pg, prelude::*};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{protocol::QualityRange, token::Token, Address, Chain, PaginationParams},
    storage::{StorageError, WithTotal},
    Bytes,
};

use crate::postgres::{
    instance::{
        active::active_instance_ids, gateway::PostgresGatewayNew, schema::tycho::component_balance,
    },
    orm,
    schema::{token, token_price},
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

    /// Returns the price of each token that has one, by token address.
    ///
    /// `NotFound` for an unknown chain.
    pub async fn get_token_prices(
        &self,
        chain: &Chain,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Bytes, f64>, StorageError> {
        self.get_chain_id(chain)?;
        let prices: Vec<(Bytes, f64)> = token_price::table
            .inner_join(token::table)
            .select((token::address.assume_not_null(), token_price::price))
            .load(conn)
            .await
            .map_err(PostgresError::from)?;
        Ok(prices.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, NaiveDateTime};
    use diesel::prelude::*;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use tycho_common::{
        models::{protocol::QualityRange, token::Token, Address, Chain, PaginationParams},
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
    }

    /// "vm:curve" on ethereum, pointing to one instance, with a second `ready` instance it does
    /// not point to.
    async fn setup(conn: &mut AsyncPgConnection) -> Setup {
        let chain_id = db_fixtures::insert_chain(conn, "ethereum").await;
        let curve_system_id =
            db_fixtures::insert_protocol_system(conn, "vm:curve".to_string()).await;
        let active = db_fixtures::insert_instance(conn, curve_system_id, "0.0.1").await;
        let inactive = db_fixtures::insert_instance(conn, curve_system_id, "0.0.2").await;
        db_fixtures::set_active_instance(conn, curve_system_id, Some(active)).await;
        let gateway = PostgresGatewayNew::from_connection(conn).await;
        Setup { gateway, chain_id, active, inactive }
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
        let id = db_fixtures::insert_instance_component(conn, instance, "pool", &[b(token)]).await;
        db_fixtures::insert_instance_component_balance(
            conn,
            instance,
            id,
            b(token),
            b("0x01"),
            1.0,
        )
        .await;
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
}
