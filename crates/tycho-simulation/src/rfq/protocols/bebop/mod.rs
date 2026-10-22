mod client;
pub mod feed;
mod models;
mod source;
pub mod state;

/// Protocol system stamped on every component this integration emits.
pub const PROTOCOL_SYSTEM: &str = "rfq:bebop";

/// The protocol system of components whose swaps settle through Tycho's
/// `BebopFallbackRouter`, which retries against a solver-named fallback pool when the
/// venue reverts. A feed carries it instead of [`PROTOCOL_SYSTEM`] when its builder was
/// given `with_fallback_router()`.
pub const FALLBACK_PROTOCOL_SYSTEM: &str = "fallback:rfq:bebop";

/// Component type stamped on every pair this integration emits.
pub(crate) const PROTOCOL_TYPE: &str = "bebop_pool";

#[cfg(test)]
mod tests {
    use super::*;

    /// tycho-execution resolves the fallback encoder by this exact string and keeps its own copy
    /// of it, so this assertion is what holds the two crates to one spelling.
    #[test]
    fn the_fallback_system_is_the_key_the_encoder_registry_expects() {
        assert_eq!(
            FALLBACK_PROTOCOL_SYSTEM,
            tycho_execution::encoding::evm::BEBOP_FALLBACK_PROTOCOL_SYSTEM
        );
    }
}
