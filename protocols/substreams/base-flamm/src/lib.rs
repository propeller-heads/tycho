// Copyright (c) 2026 Everlong Labs Limited
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod config;
#[cfg(test)]
mod e2e_tests;
pub mod flamm;
pub mod modules;
#[cfg(test)]
mod testdata;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod verify_tests;
