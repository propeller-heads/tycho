// @generated
#[allow(clippy::derive_partial_eq_without_eq)]
#[allow(dead_code)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Pools {
    #[prost(message, repeated, tag="1")]
    pub pools: ::prost::alloc::vec::Vec<Pool>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[allow(dead_code)]
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
#[allow(clippy::derive_partial_eq_without_eq)]
#[allow(dead_code)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Events {
    #[prost(message, repeated, tag="1")]
    pub events: ::prost::alloc::vec::Vec<Event>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[allow(dead_code)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Event {
    #[prost(string, tag="100")]
    pub hash: ::prost::alloc::string::String,
    #[prost(uint32, tag="101")]
    pub log_index: u32,
    #[prost(uint64, tag="102")]
    pub log_ordinal: u64,
    #[prost(string, tag="103")]
    pub to: ::prost::alloc::string::String,
    #[prost(string, tag="104")]
    pub from: ::prost::alloc::string::String,
    #[prost(uint64, tag="105")]
    pub block_number: u64,
    #[prost(uint64, tag="106")]
    pub timestamp: u64,
    #[prost(string, tag="107")]
    pub pool: ::prost::alloc::string::String,
    #[prost(oneof="event::Type", tags="10, 20, 30, 40")]
    pub r#type: ::core::option::Option<event::Type>,
}
/// Nested message and enum types in `Event`.
pub mod event {
    #[allow(clippy::derive_partial_eq_without_eq)]
    #[allow(dead_code)]
#[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Type {
        #[prost(message, tag="10")]
        Deposit(super::DepositEvent),
        #[prost(message, tag="20")]
        Withdraw(super::WithdrawEvent),
        #[prost(message, tag="30")]
        Sync(super::SyncEvent),
        #[prost(message, tag="40")]
        Swap(super::SwapEvent),
    }
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DepositEvent {
    #[prost(string, repeated, tag="1")]
    pub input_token_amounts: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
    #[prost(string, optional, tag="2")]
    pub output_token_amount: ::core::option::Option<::prost::alloc::string::String>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct WithdrawEvent {
    #[prost(string, repeated, tag="1")]
    pub input_token_amounts: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
    #[prost(string, optional, tag="2")]
    pub output_token_amount: ::core::option::Option<::prost::alloc::string::String>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[allow(dead_code)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SyncEvent {
    #[prost(string, tag="1")]
    pub reserve0: ::prost::alloc::string::String,
    #[prost(string, tag="2")]
    pub reserve1: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[allow(dead_code)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SwapEvent {
    #[prost(string, tag="1")]
    pub token_in: ::prost::alloc::string::String,
    #[prost(string, tag="2")]
    pub amount_in: ::prost::alloc::string::String,
    #[prost(string, tag="3")]
    pub token_out: ::prost::alloc::string::String,
    #[prost(string, tag="4")]
    pub amount_out: ::prost::alloc::string::String,
}
// @@protoc_insertion_point(module)
