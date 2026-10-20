use std::{env, time::Duration};

use dotenv::dotenv;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    simulation::indicatively_priced::SignedQuote,
};

use super::client_builder::ElfomofiClientBuilder;
use crate::snapshot_feed::errors::FeedError;

/// Resolves the ordinary RPC environment configuration without storing it in protocol state.
pub(super) fn regular_rpc_url(variable: &str) -> Result<String, FeedError> {
    env::var(variable)
        .ok()
        .or_else(|| {
            dotenv().ok()?;
            env::var(variable).ok()
        })
        .ok_or_else(|| {
            FeedError::InvalidInput(format!("Set {variable} in the environment or .env file"))
        })
}

/// Refreshes an unsigned execution quote using the same RPC configuration as orderbook updates.
pub(super) async fn request_quote(
    chain: Chain,
    max_age_seconds: u64,
    params: &GetAmountOutParams,
) -> Result<SignedQuote, FeedError> {
    ElfomofiClientBuilder::from_env(chain)
        .poll_time(Duration::from_secs(2))
        .max_age(Duration::from_secs(max_age_seconds))
        .build()?
        .request_binding_quote(params)
        .await
}
