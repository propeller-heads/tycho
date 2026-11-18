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
    count_to_i64,
    instance::{
        active::active_instance_ids, gateway::PostgresGatewayNew, schema::tycho::component_balance,
    },
    orm,
    schema::{token, token_price},
    PostgresError,
};

impl PostgresGatewayNew {
    /// Returns the matching tokens in token id order, and the number of matching tokens on all
    /// pages. Without a page, that number is the number of returned tokens, with no count query.
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

        let mut query = filtered()
            .order_by(token::id)
            .select((orm::Token::as_select(), token::address.assume_not_null()));
        let mut total = None;
        if let Some(pagination) = pagination_params {
            total = Some(
                filtered()
                    .count()
                    .get_result::<i64>(conn)
                    .await
                    .map_err(PostgresError::from)?,
            );
            query = query
                .limit(pagination.page_size)
                .offset(pagination.offset());
        }
        let rows: Vec<(orm::Token, Address)> = query
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        let tokens: Vec<Token> = rows
            .into_iter()
            .map(|(row, address)| row.to_model(&address, chain))
            .collect::<Result<_, _>>()?;
        let total = total.unwrap_or_else(|| count_to_i64(tokens.len()));
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
