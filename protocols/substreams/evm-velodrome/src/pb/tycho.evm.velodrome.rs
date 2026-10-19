// @generated
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Pool {
    #[prost(bytes="vec", tag="1")]
    pub address: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="2")]
    pub token0: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="3")]
    pub token1: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="4")]
    pub created_tx_hash: ::prost::alloc::vec::Vec<u8>,
}
/// A tick spacing and fee.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TickSpacingFee {
    #[prost(int32, tag="1")]
    pub tick_spacing: i32,
    #[prost(uint64, tag="2")]
    pub fee: u64,
}
/// A group of TickSpacingFee
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TickSpacingFees {
    #[prost(message, repeated, tag="1")]
    pub tick_spacing_fees: ::prost::alloc::vec::Vec<TickSpacingFee>,
}
// @@protoc_insertion_point(module)
