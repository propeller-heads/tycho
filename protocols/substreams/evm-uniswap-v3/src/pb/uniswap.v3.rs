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
/// A change to a pool's balance.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BalanceDelta {
    /// The address of the ERC20 token.
    #[prost(bytes="vec", tag="1")]
    pub token_address: ::prost::alloc::vec::Vec<u8>,
    /// The delta of the token.
    #[prost(bytes="vec", tag="2")]
    pub amount: ::prost::alloc::vec::Vec<u8>,
    /// The sign of the delta, true for positive, false for negative.
    #[prost(bool, tag="3")]
    pub sign: bool,
    /// The address of the pool whose balance changed.
    #[prost(bytes="vec", tag="4")]
    pub pool_address: ::prost::alloc::vec::Vec<u8>,
    /// Used to determine the order of the balance changes. Necessary for the balance store.
    #[prost(uint64, tag="5")]
    pub ordinal: u64,
}
/// A group of BalanceDelta
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BalanceDeltas {
    #[prost(message, repeated, tag="1")]
    pub deltas: ::prost::alloc::vec::Vec<BalanceDelta>,
}
// @@protoc_insertion_point(module)
