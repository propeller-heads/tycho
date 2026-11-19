//! The plugin implementations each DEX deploys, and the modules each is made of.
//!
//! One file per DEX. A DEX adds an entry for every implementation its beacons point at, before
//! it upgrades a beacon to one.

use tycho_common::simulation::errors::SimulationError;

pub mod nami;

/// Registers the plugin handlers this crate ships with.
///
/// Calling it more than once is harmless: each registration replaces the previous one for the
/// same chain and implementation.
pub fn initialize_plugin_handlers() -> Result<(), SimulationError> {
    nami::register_plugin_handlers()
}
