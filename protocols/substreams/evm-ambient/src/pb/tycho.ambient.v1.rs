// @generated
/// A change to a pool's balance. Ambient specific.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AmbientBalanceDelta {
    /// The address of the ERC20 token whose balance changed.
    #[prost(bytes="vec", tag="1")]
    pub pool_hash: ::prost::alloc::vec::Vec<u8>,
    /// The token type: it can be base or quote.
    #[prost(string, tag="2")]
    pub token_type: ::prost::alloc::string::String,
    /// The delta of the token.
    #[prost(bytes="vec", tag="3")]
    pub token_delta: ::prost::alloc::vec::Vec<u8>,
    /// Used to determine the order of the balance changes. Necessary for the balance store.
    #[prost(uint64, tag="4")]
    pub ordinal: u64,
    /// Transaction index of the balance change
    #[prost(uint64, tag="5")]
    pub tx_index: u64,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AmbientProtocolComponent {
    /// A unique identifier for the component within the protocol.
    #[prost(string, tag="1")]
    pub id: ::prost::alloc::string::String,
    /// Addresses of the ERC20 tokens used by the component.
    #[prost(bytes="vec", repeated, tag="2")]
    pub tokens: ::prost::alloc::vec::Vec<::prost::alloc::vec::Vec<u8>>,
    /// Ambient pool index [static attribute for ambient pools]
    #[prost(bytes="vec", tag="3")]
    pub pool_index: ::prost::alloc::vec::Vec<u8>,
    /// Transaction index for the component creation
    #[prost(uint64, tag="4")]
    pub tx_index: u64,
}
/// Ambient pool changes within a single block
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BlockPoolChanges {
    /// New protocol components added in this block
    #[prost(message, repeated, tag="1")]
    pub new_components: ::prost::alloc::vec::Vec<AmbientProtocolComponent>,
    /// Balance changes on this block
    #[prost(message, repeated, tag="2")]
    pub balance_deltas: ::prost::alloc::vec::Vec<AmbientBalanceDelta>,
}
// @@protoc_insertion_point(module)
