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
        #[prost(string, tag="102")]
        pub pool_address: ::prost::alloc::string::String,
        #[prost(string, tag="103")]
        pub token0: ::prost::alloc::string::String,
        #[prost(string, tag="104")]
        pub token1: ::prost::alloc::string::String,
        #[prost(message, optional, tag="105")]
        pub transaction: ::core::option::Option<super::Transaction>,
        #[prost(oneof="pool_event::Type", tags="1, 2, 3, 4, 5, 6, 7, 8")]
        pub r#type: ::core::option::Option<pool_event::Type>,
    }
    /// Nested message and enum types in `PoolEvent`.
    pub mod pool_event {
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Initialize {
            /// Unsigned
            #[prost(string, tag="1")]
            pub sqrt_price: ::prost::alloc::string::String,
            #[prost(int32, tag="2")]
            pub tick: i32,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Mint {
            #[prost(string, tag="1")]
            pub sender: ::prost::alloc::string::String,
            #[prost(string, tag="2")]
            pub owner: ::prost::alloc::string::String,
            /// Signed
            #[prost(int32, tag="3")]
            pub tick_lower: i32,
            /// Signed
            #[prost(int32, tag="4")]
            pub tick_upper: i32,
            /// Unsigned
            #[prost(string, tag="5")]
            pub amount: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="6")]
            pub amount_0: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="7")]
            pub amount_1: ::prost::alloc::string::String,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Collect {
            #[prost(string, tag="1")]
            pub owner: ::prost::alloc::string::String,
            #[prost(string, tag="2")]
            pub recipient: ::prost::alloc::string::String,
            #[prost(int32, tag="3")]
            pub tick_lower: i32,
            #[prost(int32, tag="4")]
            pub tick_upper: i32,
            /// Unsigned
            #[prost(string, tag="5")]
            pub amount_0: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="6")]
            pub amount_1: ::prost::alloc::string::String,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Burn {
            #[prost(string, tag="1")]
            pub owner: ::prost::alloc::string::String,
            #[prost(int32, tag="2")]
            pub tick_lower: i32,
            #[prost(int32, tag="3")]
            pub tick_upper: i32,
            /// Unsigned
            #[prost(string, tag="4")]
            pub amount: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="5")]
            pub amount_0: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="6")]
            pub amount_1: ::prost::alloc::string::String,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Swap {
            #[prost(string, tag="1")]
            pub sender: ::prost::alloc::string::String,
            #[prost(string, tag="2")]
            pub recipient: ::prost::alloc::string::String,
            /// Signed
            #[prost(string, tag="3")]
            pub amount_0: ::prost::alloc::string::String,
            /// Signed
            #[prost(string, tag="4")]
            pub amount_1: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="6")]
            pub sqrt_price: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="7")]
            pub liquidity: ::prost::alloc::string::String,
            #[prost(int32, tag="8")]
            pub tick: i32,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct Flash {
            #[prost(string, tag="1")]
            pub sender: ::prost::alloc::string::String,
            #[prost(string, tag="2")]
            pub recipient: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="3")]
            pub amount_0: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="4")]
            pub amount_1: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="5")]
            pub paid_0: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="6")]
            pub paid_1: ::prost::alloc::string::String,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct SetFeeProtocol {
            /// Unsigned
            #[prost(uint64, tag="1")]
            pub fee_protocol_0_old: u64,
            /// Unsigned
            #[prost(uint64, tag="2")]
            pub fee_protocol_1_old: u64,
            /// Unsigned
            #[prost(uint64, tag="3")]
            pub fee_protocol_0_new: u64,
            /// Unsigned
            #[prost(uint64, tag="4")]
            pub fee_protocol_1_new: u64,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
        pub struct CollectProtocol {
            #[prost(string, tag="1")]
            pub sender: ::prost::alloc::string::String,
            #[prost(string, tag="2")]
            pub recipient: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="3")]
            pub amount_0: ::prost::alloc::string::String,
            /// Unsigned
            #[prost(string, tag="4")]
            pub amount_1: ::prost::alloc::string::String,
        }
        #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Oneof)]
        pub enum Type {
            #[prost(message, tag="1")]
            Initialize(Initialize),
            #[prost(message, tag="2")]
            Mint(Mint),
            #[prost(message, tag="3")]
            Collect(Collect),
            #[prost(message, tag="4")]
            Burn(Burn),
            #[prost(message, tag="5")]
            Swap(Swap),
            #[prost(message, tag="6")]
            Flash(Flash),
            #[prost(message, tag="7")]
            SetFeeProtocol(SetFeeProtocol),
            #[prost(message, tag="8")]
            CollectProtocol(CollectProtocol),
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
