//! Plugins of Algebra Integral pools.
//!
//! A plugin implementation is a set of modules behind one contract, and each module keeps its
//! configuration at slots fixed by its code. [`modules`] lists those slots per module;
//! [`attributes::plugin_attributes`] turns a plugin's storage writes into the pool's attributes.
//! [`beacon`] finds the implementation a plugin runs, which is what the simulation picks its
//! modules by. [`changes`] is the entry point: a pool's plugin attributes from one transaction.

pub mod attributes;
pub mod beacon;
pub mod changes;
pub mod modules;
pub mod storage;
