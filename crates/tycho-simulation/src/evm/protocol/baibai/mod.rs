//! BaiBai CurveBook (v5): exact-input simulation of the entrypoint's no-surcharge path.
mod decoder;
mod math;
mod state;

pub use state::BaibaiState;

#[cfg(test)]
mod tests;
