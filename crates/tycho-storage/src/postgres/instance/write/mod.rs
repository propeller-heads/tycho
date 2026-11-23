//! Write methods of [`PostgresGatewayNew`](super::gateway::PostgresGatewayNew), one file per
//! domain.
//!
//! Each method writes rows of one instance, or of the shared `public` tables that instances refer
//! to. The block and order of each change come from the batch, see [`super::origins`].

/// Rows per insert statement, far below the bind parameter limit of Postgres.
const ROWS_PER_INSERT: usize = 1000;

mod balance;
pub mod block;
mod extraction_state;
mod state;
mod token;
#[cfg_attr(not(test), expect(dead_code, reason = "no instance write op versions rows yet"))]
pub mod versioning;
