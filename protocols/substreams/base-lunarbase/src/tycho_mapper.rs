use itertools::Itertools;
use tycho_substreams::prelude as tycho;

use crate::lunarbase;

pub fn to_tycho_block_changes(
    block: tycho::Block,
    changes: lunarbase::BlockChanges,
) -> tycho::BlockChanges {
    tycho::BlockChanges {
        block: Some(block),
        changes: changes
            .transactions
            .into_iter()
            .sorted_unstable_by_key(|tx| tx.tx.index)
            .filter_map(to_tycho_transaction_changes)
            .collect(),
        storage_changes: Vec::new(),
    }
}

fn to_tycho_transaction_changes(
    tx_changes: lunarbase::TransactionChanges,
) -> Option<tycho::TransactionChanges> {
    let mut out = tycho::TransactionChanges {
        tx: Some(tycho::Transaction {
            hash: tx_changes.tx.hash.to_vec(),
            from: tx_changes.tx.from.to_vec(),
            to: tx_changes.tx.to.to_vec(),
            index: tx_changes.tx.index,
        }),
        contract_changes: Vec::new(),
        entity_changes: tx_changes
            .state_updates
            .into_iter()
            .map(|(component_id, delta)| tycho::EntityChanges {
                component_id,
                attributes: delta
                    .updated_attributes
                    .into_iter()
                    .sorted_unstable_by(|(left, _), (right, _)| left.cmp(right))
                    .map(|(name, value)| tycho::Attribute {
                        name,
                        value,
                        change: tycho::ChangeType::Update.into(),
                    })
                    .collect(),
            })
            .filter(|entity| !entity.attributes.is_empty())
            .collect(),
        component_changes: tx_changes
            .new_protocol_components
            .into_iter()
            .map(to_tycho_protocol_component)
            .collect(),
        balance_changes: tx_changes
            .balance_changes
            .into_iter()
            .flat_map(|(component_id, balances)| {
                balances
                    .into_iter()
                    .map(move |balance| tycho::BalanceChange {
                        token: balance.token.to_vec(),
                        balance: balance.balance.to_be_bytes().to_vec(),
                        component_id: component_id.clone().into_bytes(),
                    })
            })
            .collect(),
        entrypoints: Vec::new(),
        entrypoint_params: Vec::new(),
    };

    if out.entity_changes.is_empty() &&
        out.component_changes.is_empty() &&
        out.balance_changes.is_empty()
    {
        return None;
    }

    out.entity_changes
        .sort_unstable_by(|left, right| {
            left.component_id
                .cmp(&right.component_id)
        });
    out.component_changes
        .sort_unstable_by(|left, right| left.id.cmp(&right.id));
    out.balance_changes
        .sort_unstable_by(|left, right| {
            (left.component_id.as_slice(), left.token.as_slice())
                .cmp(&(right.component_id.as_slice(), right.token.as_slice()))
        });
    Some(out)
}

pub fn to_tycho_protocol_component(
    component: lunarbase::ProtocolComponent,
) -> tycho::ProtocolComponent {
    tycho::ProtocolComponent::new(&component.id)
        .with_tokens(&component.tokens)
        .with_contracts(&component.contract_addresses)
        .as_swap_type(lunarbase::PROTOCOL_TYPE_NAME, tycho::ImplementationType::Custom)
}
