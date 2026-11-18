//! Write methods of [`PostgresGatewayNew`](super::gateway::PostgresGatewayNew), one file per
//! domain.
//!
//! Each method writes rows of one instance, or of the shared `public` tables that instances refer
//! to. The block and order of each change come from the batch, see [`super::origins`].

pub mod block;
