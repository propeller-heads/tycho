mod decoder;
pub mod hooks;
pub mod state;

/// Test-only: the recorded Robinhood Pons pool fixture, so the stream-decoder tests in
/// `crate::evm::decoder` can build the same snapshot this module's tests decode.
#[cfg(test)]
pub(crate) use decoder::pons_fixture;
