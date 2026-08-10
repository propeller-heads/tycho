// `AerodromeSlipstreamsState::new` is public but takes a `DynamicFeeConfig` and a
// `Vec<Observation>`, so neither the constructor nor the types it needs can be named from outside
// the crate. Benches build pools directly, so the wider surface rides `bench-internals` rather
// than becoming part of the default API.
#[cfg(not(feature = "bench-internals"))]
pub(crate) mod dynamic_fee_module;
#[cfg(feature = "bench-internals")]
pub mod dynamic_fee_module;

#[cfg(not(feature = "bench-internals"))]
pub(crate) mod observations;
#[cfg(feature = "bench-internals")]
pub mod observations;
