//! # Per-instance schema
//!
//! Tables of the `tycho` Postgres schema, where each protocol's data lives in an instance: one copy
//! of one protocol's data, made by one extractor version. Current state is list-partitioned by
//! `instance_id`, one leaf per instance (`<table>_i<id>`); history is range-partitioned by day on
//! `valid_to` across all instances.
//!
//! Every key of a per-instance table starts with `instance_id`. Entity ids repeat across
//! instances, and so do external ids, so a query that leaves out `instance_id` returns other
//! instances' rows.
//!
//! Instance leaves are created with the SQL functions `tycho.create_instance_leaves(id)` and
//! `tycho.attach_instance_leaves(id)` (see the migration `2026-11-09-000000_instance_schema`).
//! Tables in `public` (tokens, blocks, protocol types and names) stay shared; instance tables
//! refer to them by natural key, with no foreign key.
//!
//! Nothing reads or writes these tables yet; the read and write paths move onto them in later
//! changes.

pub mod orm;
pub mod schema;

use std::fmt;

use diesel::{
    deserialize::{self, FromSql, FromSqlRow},
    expression::AsExpression,
    pg::{Pg, PgValue},
    serialize::{self, Output, ToSql},
    sql_types::Int4,
};

/// Id of a protocol instance (`tycho.protocol_instance.id`).
///
/// A distinct type so that an instance id can't be passed where another id is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, AsExpression, FromSqlRow)]
#[diesel(sql_type = Int4)]
pub struct InstanceId(pub i32);

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ToSql<Int4, Pg> for InstanceId {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Pg>) -> serialize::Result {
        <i32 as ToSql<Int4, Pg>>::to_sql(&self.0, &mut out.reborrow())
    }
}

impl FromSql<Int4, Pg> for InstanceId {
    fn from_sql(bytes: PgValue<'_>) -> deserialize::Result<Self> {
        <i32 as FromSql<Int4, Pg>>::from_sql(bytes).map(Self)
    }
}

// Queries that join instance tables with the shared tables they refer to. Each pair needs its own
// invocation: a longer list would also pair the instance tables with each other, which the
// generated schema already does.
mod shared_tables {
    use super::schema::tycho::{
        account_balance, component_balance, protocol_component, protocol_component_holds_token,
        protocol_instance,
    };
    use crate::postgres::schema::{protocol_system, protocol_type, token};

    diesel::joinable!(protocol_instance -> protocol_system (protocol_system_id));

    diesel::allow_tables_to_appear_in_same_query!(protocol_instance, protocol_system);
    diesel::allow_tables_to_appear_in_same_query!(protocol_component, protocol_type);
    diesel::allow_tables_to_appear_in_same_query!(protocol_component_holds_token, token);
    diesel::allow_tables_to_appear_in_same_query!(component_balance, token);
    diesel::allow_tables_to_appear_in_same_query!(account_balance, token);
}

#[cfg(test)]
mod tests;
