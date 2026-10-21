// @generated
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CowPool {
    #[prost(bytes="vec", tag="1")]
    pub address: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="2")]
    pub token_a: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="3")]
    pub token_b: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="4")]
    pub lp_token: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="5")]
    pub weight_a: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="6")]
    pub weight_b: ::prost::alloc::vec::Vec<u8>,
    #[prost(uint64, tag="7")]
    pub fee: u64,
    #[prost(bytes="vec", tag="8")]
    pub created_tx_hash: ::prost::alloc::vec::Vec<u8>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CowPools {
    #[prost(message, repeated, tag="1")]
    pub pools: ::prost::alloc::vec::Vec<CowPool>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Transaction {
    #[prost(bytes="vec", tag="1")]
    pub hash: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="2")]
    pub from: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="3")]
    pub to: ::prost::alloc::vec::Vec<u8>,
    #[prost(uint64, tag="4")]
    pub index: u64,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CowPoolBind {
    #[prost(bytes="vec", tag="1")]
    pub address: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="2")]
    pub token: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="3")]
    pub weight: ::prost::alloc::vec::Vec<u8>,
    /// Amount of the token bound to the pool
    #[prost(bytes="vec", tag="4")]
    pub amount: ::prost::alloc::vec::Vec<u8>,
    /// tx that the bind happened
    #[prost(message, optional, tag="5")]
    pub tx: ::core::option::Option<Transaction>,
    /// ordinal of the event the bind was recorded in the block
    #[prost(uint64, tag="6")]
    pub ordinal: u64,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CowPoolBinds {
    #[prost(message, repeated, tag="1")]
    pub binds: ::prost::alloc::vec::Vec<CowPoolBind>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CowPoolCreation {
    #[prost(bytes="vec", tag="1")]
    pub address: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="2")]
    pub lp_token: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="3")]
    pub created_tx_hash: ::prost::alloc::vec::Vec<u8>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CowPoolCreations {
    #[prost(message, repeated, tag="1")]
    pub pools: ::prost::alloc::vec::Vec<CowPoolCreation>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Attribute {
    /// The name of the attribute.
    #[prost(string, tag="1")]
    pub name: ::prost::alloc::string::String,
    /// The value of the attribute.
    #[prost(bytes="vec", tag="2")]
    pub value: ::prost::alloc::vec::Vec<u8>,
    /// The type of change the attribute underwent.
    #[prost(enumeration="ChangeType", tag="3")]
    pub change: i32,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ProtocolType {
    #[prost(string, tag="1")]
    pub name: ::prost::alloc::string::String,
    #[prost(enumeration="FinancialType", tag="2")]
    pub financial_type: i32,
    #[prost(message, repeated, tag="3")]
    pub attribute_schema: ::prost::alloc::vec::Vec<Attribute>,
    #[prost(enumeration="ImplementationType", tag="4")]
    pub implementation_type: i32,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CowProtocolComponent {
    #[prost(string, tag="1")]
    pub id: ::prost::alloc::string::String,
    #[prost(bytes="vec", repeated, tag="2")]
    pub tokens: ::prost::alloc::vec::Vec<::prost::alloc::vec::Vec<u8>>,
    #[prost(bytes="vec", repeated, tag="3")]
    pub contracts: ::prost::alloc::vec::Vec<::prost::alloc::vec::Vec<u8>>,
    #[prost(message, repeated, tag="4")]
    pub static_att: ::prost::alloc::vec::Vec<Attribute>,
    #[prost(enumeration="ChangeType", tag="5")]
    pub change: i32,
    #[prost(message, optional, tag="6")]
    pub protocol_type: ::core::option::Option<ProtocolType>,
}
/// A message containing protocol components that were created by a single tx.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TransactionProtocolComponents {
    #[prost(message, optional, tag="1")]
    pub tx: ::core::option::Option<Transaction>,
    #[prost(message, repeated, tag="2")]
    pub components: ::prost::alloc::vec::Vec<CowProtocolComponent>,
}
/// All protocol components that were created within a block with their corresponding tx.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BlockTransactionProtocolComponents {
    #[prost(message, repeated, tag="1")]
    pub tx_components: ::prost::alloc::vec::Vec<TransactionProtocolComponents>,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CowBalanceDelta {
    #[prost(uint64, tag="1")]
    pub ord: u64,
    #[prost(message, optional, tag="2")]
    pub tx: ::core::option::Option<Transaction>,
    #[prost(bytes="vec", tag="3")]
    pub token: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="4")]
    pub delta: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes="vec", tag="5")]
    pub component_id: ::prost::alloc::vec::Vec<u8>,
}
/// A set of balances deltas, usually a group of changes within a single block.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BlockBalanceDeltas {
    #[prost(message, repeated, tag="1")]
    pub balance_deltas: ::prost::alloc::vec::Vec<CowBalanceDelta>,
}
/// set of new components added in the block and deltas 
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BlockPoolChanges {
    #[prost(message, optional, tag="1")]
    pub tx_protocol_components: ::core::option::Option<BlockTransactionProtocolComponents>,
    #[prost(message, optional, tag="2")]
    pub block_balance_deltas: ::core::option::Option<BlockBalanceDeltas>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum ChangeType {
    Unspecified = 0,
    Update = 1,
    Creation = 2,
    Deletion = 3,
}
impl ChangeType {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            ChangeType::Unspecified => "CHANGE_TYPE_UNSPECIFIED",
            ChangeType::Update => "CHANGE_TYPE_UPDATE",
            ChangeType::Creation => "CHANGE_TYPE_CREATION",
            ChangeType::Deletion => "CHANGE_TYPE_DELETION",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "CHANGE_TYPE_UNSPECIFIED" => Some(Self::Unspecified),
            "CHANGE_TYPE_UPDATE" => Some(Self::Update),
            "CHANGE_TYPE_CREATION" => Some(Self::Creation),
            "CHANGE_TYPE_DELETION" => Some(Self::Deletion),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum FinancialType {
    Swap = 0,
    Lend = 1,
    Leverage = 2,
    Psm = 3,
}
impl FinancialType {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            FinancialType::Swap => "SWAP",
            FinancialType::Lend => "LEND",
            FinancialType::Leverage => "LEVERAGE",
            FinancialType::Psm => "PSM",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "SWAP" => Some(Self::Swap),
            "LEND" => Some(Self::Lend),
            "LEVERAGE" => Some(Self::Leverage),
            "PSM" => Some(Self::Psm),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum ImplementationType {
    Vm = 0,
    Custom = 1,
}
impl ImplementationType {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            ImplementationType::Vm => "VM",
            ImplementationType::Custom => "CUSTOM",
        }
    }
    /// Creates an enum from field names used in the ProtoBuf definition.
    pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
        match value {
            "VM" => Some(Self::Vm),
            "CUSTOM" => Some(Self::Custom),
            _ => None,
        }
    }
}
// @@protoc_insertion_point(module)
