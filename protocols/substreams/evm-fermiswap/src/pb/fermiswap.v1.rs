// @generated
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Pair {
    #[prost(bytes="vec", tag="1")]
    pub base_asset: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="2")]
    pub quote_asset: ::prost::alloc::vec::Vec<u8>,
}
// @@protoc_insertion_point(module)
