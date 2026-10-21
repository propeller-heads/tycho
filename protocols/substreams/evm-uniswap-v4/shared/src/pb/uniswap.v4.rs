// @generated
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Pool {
    ///   // The pool address.
    #[prost(bytes="vec", tag="1")]
    pub id: ::prost::alloc::vec::Vec<u8>,
    /// The token0 address.
    #[prost(bytes="vec", tag="2")]
    pub currency0: ::prost::alloc::vec::Vec<u8>,
    /// The token1 address.
    #[prost(bytes="vec", tag="3")]
    pub currency1: ::prost::alloc::vec::Vec<u8>,
    /// The transaction where the pool was created.
    #[prost(bytes="vec", tag="4")]
    pub created_tx_hash: ::prost::alloc::vec::Vec<u8>,
}
/// A struct describing a transaction.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Transaction {
    /// The transaction hash.
    #[prost(bytes="vec", tag="1")]
    pub hash: ::prost::alloc::vec::Vec<u8>,
    /// The sender of the transaction.
    #[prost(bytes="vec", tag="2")]
    pub from: ::prost::alloc::vec::Vec<u8>,
    /// The receiver of the transaction.
    #[prost(bytes="vec", tag="3")]
    pub to: ::prost::alloc::vec::Vec<u8>,
    /// The transactions index within the block.
    #[prost(uint64, tag="4")]
    pub index: u64,
}
/// A change to a pool's tick.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TickDelta {
    /// The address of the pool.
    #[prost(bytes="vec", tag="1")]
    pub pool_address: ::prost::alloc::vec::Vec<u8>,
    /// The index of the tick.
    #[prost(int32, tag="2")]
    pub tick_index: i32,
    /// The liquidity net delta of this tick. Bigint encoded as signed little endian bytes.
    #[prost(bytes="vec", tag="3")]
    pub liquidity_net_delta: ::prost::alloc::vec::Vec<u8>,
    /// Used to determine the order of the balance changes. Necessary for the balance store.
    #[prost(uint64, tag="4")]
    pub ordinal: u64,
    #[prost(message, optional, tag="5")]
    pub transaction: ::core::option::Option<Transaction>,
}
/// A group of TickDelta
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TickDeltas {
    #[prost(message, repeated, tag="1")]
    pub deltas: ::prost::alloc::vec::Vec<TickDelta>,
}
/// A change to a pool's liquidity.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LiquidityChange {
    /// The address of the pool.
    #[prost(bytes="vec", tag="1")]
    pub pool_address: ::prost::alloc::vec::Vec<u8>,
    /// The liquidity changed amount. Bigint encoded as signed little endian bytes.
    #[prost(bytes="vec", tag="2")]
    pub value: ::prost::alloc::vec::Vec<u8>,
    /// The type of update, can be absolute or delta.
    #[prost(enumeration="LiquidityChangeType", tag="3")]
    pub change_type: i32,
    /// Used to determine the order of the balance changes. Necessary for the balance store.
    #[prost(uint64, tag="4")]
    pub ordinal: u64,
    #[prost(message, optional, tag="5")]
    pub transaction: ::core::option::Option<Transaction>,
}
/// A group of LiquidityChange
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LiquidityChanges {
    #[prost(message, repeated, tag="1")]
    pub changes: ::prost::alloc::vec::Vec<LiquidityChange>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Events {
    #[prost(message, repeated, tag="3")]
    pub pool_events: ::prost::alloc::vec::Vec<events::PoolEvent>,
}
/// Nested message and enum types in `Events`.
pub mod events {
    #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
    pub struct PoolEvent {
        #[prost(uint64, tag="100")]
        pub log_ordinal: u64,
        /// Changed from pool_address to pool_id as V4 uses PoolId
        #[prost(string, tag="102")]
        pub pool_id: ::prost::alloc::string::String,
        /// Changed from token0 to currency0
        #[prost(string, tag="103")]
        pub currency0: ::prost::alloc::string::String,
        /// Changed from token1 to currency1
        #[prost(string, tag="104")]
        pub currency1: ::prost::alloc::string::String,
        #[prost(message, optional, tag="105")]
        pub transaction: ::core::option::Option<super::Transaction>,
        #[prost(oneof="pool_event::Type", tags="1, 2, 3, 4, 5")]
        pub r#type: ::core::option::Option<pool_event::Type>,
    }
    /// Nested message and enum types in `PoolEvent`.
    pub mod pool_event {
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Initialize {
            #[prost(string, tag="1")]
            pub sqrt_price_x96: ::prost::alloc::string::String,
            #[prost(int32, tag="2")]
            pub tick: i32,
            #[prost(uint32, tag="3")]
            pub fee: u32,
            #[prost(int32, tag="4")]
            pub tick_spacing: i32,
            /// Address of the hooks contract
            #[prost(string, tag="5")]
            pub hooks: ::prost::alloc::string::String,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct ModifyLiquidity {
            #[prost(string, tag="1")]
            pub sender: ::prost::alloc::string::String,
            #[prost(int32, tag="2")]
            pub tick_lower: i32,
            #[prost(int32, tag="3")]
            pub tick_upper: i32,
            /// Changed to support signed integers
            #[prost(string, tag="4")]
            pub liquidity_delta: ::prost::alloc::string::String,
            /// Added salt field
            #[prost(string, tag="5")]
            pub salt: ::prost::alloc::string::String,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Swap {
            #[prost(string, tag="1")]
            pub sender: ::prost::alloc::string::String,
            /// Signed int128
            #[prost(string, tag="2")]
            pub amount0: ::prost::alloc::string::String,
            /// Signed int128
            #[prost(string, tag="3")]
            pub amount1: ::prost::alloc::string::String,
            #[prost(string, tag="4")]
            pub sqrt_price_x96: ::prost::alloc::string::String,
            #[prost(string, tag="5")]
            pub liquidity: ::prost::alloc::string::String,
            #[prost(int32, tag="6")]
            pub tick: i32,
            /// Added fee field
            #[prost(uint32, tag="7")]
            pub fee: u32,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Donate {
            #[prost(string, tag="1")]
            pub sender: ::prost::alloc::string::String,
            #[prost(string, tag="2")]
            pub amount0: ::prost::alloc::string::String,
            #[prost(string, tag="3")]
            pub amount1: ::prost::alloc::string::String,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct ProtocolFeeUpdated {
            #[prost(string, tag="1")]
            pub pool_id: ::prost::alloc::string::String,
            #[prost(uint32, tag="2")]
            pub protocol_fee: u32,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Oneof)]
        pub enum Type {
            #[prost(message, tag="1")]
            Initialize(Initialize),
            #[prost(message, tag="2")]
            ModifyLiquidity(ModifyLiquidity),
            #[prost(message, tag="3")]
            Swap(Swap),
            #[prost(message, tag="4")]
            Donate(Donate),
            #[prost(message, tag="5")]
            ProtocolFeeUpdated(ProtocolFeeUpdated),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum LiquidityChangeType {
    Delta = 0,
    Absolute = 1,
}
impl LiquidityChangeType {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            LiquidityChangeType::Delta => "DELTA",
            LiquidityChangeType::Absolute => "ABSOLUTE",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "DELTA" => Some(Self::Delta),
            "ABSOLUTE" => Some(Self::Absolute),
            _ => None,
        }
    }
}
// @@protoc_insertion_point(module)
