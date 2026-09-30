//! BaiBai CurveBook v3: exact-input simulation for zero-fee takers.
mod decoder;
mod math;
mod state;

pub use state::BaibaiState;

#[cfg(test)]
mod tests;
