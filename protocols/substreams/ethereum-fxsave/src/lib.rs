// The `substreams::handlers` macros expand to pointer-taking `pub fn`s.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
mod config;
mod modules;

pub use config::Config;
pub use modules::*;
