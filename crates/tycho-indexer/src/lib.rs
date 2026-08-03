pub mod cli;
pub mod extractor;
// `tonic::Status` is large enough to trigger `result_large_err`; this is an
// existing API shape, and boxing it here would be a larger compatibility change.
#[allow(clippy::result_large_err)]
pub mod pb;
pub mod services;
pub mod substreams;

#[cfg(test)]
#[allow(clippy::extra_unused_lifetimes)]
mod testing;

#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;
