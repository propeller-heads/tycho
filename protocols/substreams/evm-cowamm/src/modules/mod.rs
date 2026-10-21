use crate::pb::cowamm::{self};
pub use map_components_with_balances::map_components_with_balances;
pub use map_cowpool_binds::map_cowpool_binds;
pub use map_cowpool_creations::map_cowpool_creations;
pub use map_cowpools::map_cowpools;
pub use map_protocol_changes::map_protocol_changes;
pub use store_balances::store_balances;
pub use store_components::store_components;
pub use store_cowpool_binds::store_cowpool_binds;
pub use store_cowpools::store_cowpools;
use substreams_ethereum::pb::eth::v2::TransactionTrace;
use tycho_substreams::prelude::*;

#[path = "1_map_cowpool_creations.rs"]
mod map_cowpool_creations;

#[path = "2_map_cowpool_binds.rs"]
mod map_cowpool_binds;

#[path = "2_store_cowpool_binds.rs"]
mod store_cowpool_binds;

#[path = "3_map_cowpools.rs"]
mod map_cowpools;

#[path = "3_store_cowpools.rs"]
mod store_cowpools;

#[path = "4_map_components_with_balances.rs"]
mod map_components_with_balances;

#[path = "4_store_components.rs"]
mod store_components;

#[path = "5_store_balances.rs"]
mod store_balances;

#[path = "6_map_protocol_changes.rs"]
mod map_protocol_changes;
mod utils;

impl From<&TransactionTrace> for cowamm::Transaction {
    fn from(value: &TransactionTrace) -> Self {
        Self {
            hash: value.hash.clone(),
            from: value.from.clone(),
            to: value.to.clone(),
            index: value.index.into(),
        }
    }
}

impl From<&cowamm::Transaction> for Transaction {
    fn from(value: &cowamm::Transaction) -> Self {
        Self {
            hash: value.hash.clone(),
            from: value.from.clone(),
            to: value.to.clone(),
            index: value.index,
        }
    }
}

impl From<&cowamm::CowProtocolComponent> for ProtocolComponent {
    fn from(component: &cowamm::CowProtocolComponent) -> Self {
        ProtocolComponent {
            id: component.id.clone(),
            tokens: component.tokens.clone(),
            contracts: vec![],
            static_att: component
                .static_att
                .iter()
                .map(|attr| Attribute {
                    name: attr.name.clone(),
                    value: attr.value.clone(),
                    change: ChangeType::Creation.into(),
                })
                .collect(),
            change: ChangeType::Creation.into(),
            protocol_type: Some(ProtocolType {
                name: "cowamm_pool".to_string(),
                financial_type: FinancialType::Swap.into(),
                attribute_schema: vec![],
                implementation_type: ImplementationType::Custom.into(),
            }),
        }
    }
}

impl From<&BalanceDelta> for cowamm::CowBalanceDelta {
    fn from(delta: &BalanceDelta) -> Self {
        cowamm::CowBalanceDelta {
            ord: delta.ord,
            tx: Some(cowamm::Transaction {
                from: delta.tx.clone().unwrap().from.clone(),
                to: delta.tx.clone().unwrap().to.clone(),
                hash: delta.tx.clone().unwrap().hash.clone(),
                index: delta.tx.clone().unwrap().index,
            }),
            token: delta.token.clone(),
            delta: delta.delta.clone(),
            component_id: delta.component_id.clone(),
        }
    }
}

impl From<cowamm::CowBalanceDelta> for BalanceDelta {
    fn from(delta: cowamm::CowBalanceDelta) -> Self {
        BalanceDelta {
            ord: delta.ord,
            tx: Some(Transaction {
                from: delta.tx.clone().unwrap().from.clone(),
                to: delta.tx.clone().unwrap().to.clone(),
                hash: delta.tx.clone().unwrap().hash.clone(),
                index: delta.tx.clone().unwrap().index,
            }),
            token: delta.token.clone(),
            delta: delta.delta.clone(),
            component_id: delta.component_id.clone(),
        }
    }
}
