use ethabi::ethereum_types::H160;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};

use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use ethereum_uniswap_v4_shared::{abi::pool_manager::events::Initialize, HookPermissionsDetector};

use tycho_substreams::prelude::*;

use crate::address_list::{parse_address, Address};

/// Creates a component for every pool the `PoolManager` given as the module's whole parameter
/// (an address, `0x` optional) initialises with a hook that has swap permissions. Fails, naming
/// the parameter, when it is not a 20-byte hex address.
#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
) -> Result<BlockEntityChanges, substreams::errors::Error> {
    let pool_manager = parse_address("pool_manager", &params)?;
    let mut new_pools: Vec<TransactionEntityChanges> = vec![];

    get_new_pools(&block, &mut new_pools, &pool_manager);

    Ok(BlockEntityChanges { block: None, changes: new_pools })
}

/// Appends to `new_pools` a [`pool_creation`] per `Initialize` `pool_manager` emitted in `block`
/// for a pool whose hook has swap permissions.
pub(crate) fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionEntityChanges>,
    pool_manager: &Address,
) {
    let mut on_pool_created = |event: Initialize, tx: &eth::TransactionTrace, _log: &eth::Log| {
        // Only pools whose hook has swap permissions.
        if !HookPermissionsDetector::has_swap_hooks(&H160::from_slice(&event.hooks)) {
            return;
        }

        new_pools.push(pool_creation(tx, event, pool_manager));
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![H160::from(*pool_manager)]);

    eh.on::<Initialize, _>(&mut on_pool_created);
    eh.handle_events();
}

/// The change of transaction `tx` that creates the pool `event` initialises: its component, its
/// initial state attributes and its zero balances, with `balance_owner` set to `pool_manager`. The
/// component id is the pool id as 0x-prefixed lowercase hex. The tokens are distinct:
/// `PoolManager.initialize` reverts `CurrenciesOutOfOrderOrEqual` unless currency0 < currency1.
pub(crate) fn pool_creation(
    tx: &eth::TransactionTrace,
    event: Initialize,
    pool_manager: &[u8],
) -> TransactionEntityChanges {
    let id = event.id.to_hex();
    let component = ProtocolComponent::new(&id)
        .with_tokens(&[&event.currency0, &event.currency1])
        .with_attributes(&[
            ("tick_spacing", event.tick_spacing.to_signed_bytes_be()),
            ("pool_id", event.id.to_vec()),
            ("hooks", event.hooks),
            // The PoolKey fee: a static LP fee in hundredths of a bip, or 0x800000, the flag of a
            // dynamic-fee pool, whose fee in force the state attributes carry. It is needed to
            // rebuild the PoolKey for contract interactions.
            ("key_lp_fee", event.fee.to_signed_bytes_be()),
        ])
        .as_swap_type("uniswap_v4_pool", ImplementationType::Custom);

    let state = [
        ("balance_owner", pool_manager.to_vec()),
        ("liquidity", BigInt::zero().to_signed_bytes_be()),
        ("tick", event.tick.to_signed_bytes_be()),
        (
            "sqrt_price_x96",
            event
                .sqrt_price_x96
                .to_signed_bytes_be(),
        ),
        ("protocol_fees/zero2one", BigInt::zero().to_signed_bytes_be()),
        ("protocol_fees/one2zero", BigInt::zero().to_signed_bytes_be()),
    ]
    .into_iter()
    .map(|(name, value)| Attribute {
        name: name.to_string(),
        value,
        change: ChangeType::Creation.into(),
    })
    .collect();

    let balance_changes = [event.currency0, event.currency1]
        .into_iter()
        .map(|token| BalanceChange {
            token,
            balance: BigInt::zero().to_signed_bytes_be(),
            component_id: id.as_bytes().to_vec(),
        })
        .collect();

    TransactionEntityChanges {
        tx: Some(tx.into()),
        entity_changes: vec![EntityChanges { component_id: id, attributes: state }],
        component_changes: vec![component],
        balance_changes,
    }
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use super::*;
    use crate::launchpad::test_fixtures::{
        initialize_log, launch_block, HOOK, LAUNCH_TX, POOL_MANAGER, TOKEN,
    };

    fn attribute(name: &str, value: &[u8]) -> Attribute {
        Attribute {
            name: name.to_string(),
            value: value.to_vec(),
            change: ChangeType::Creation.into(),
        }
    }

    /// The whole change `map_pools_created` emits for an `Initialize` of a hook with a
    /// `beforeSwap` permission (bit 7 of 0x...f8ec), field by field.
    #[test]
    fn test_initialize_of_a_swap_hook_creates_the_pool() {
        let mut new_pools = Vec::new();
        get_new_pools(
            &launch_block(vec![initialize_log(&POOL_MANAGER, &HOOK)]),
            &mut new_pools,
            &POOL_MANAGER,
        );

        let id = "0x35bd0e6a8b82e0e18b0381757d97ba01cde11a02740f006132dd40395ffc8ce2";
        let pool_id = hex!("35bd0e6a8b82e0e18b0381757d97ba01cde11a02740f006132dd40395ffc8ce2");
        assert_eq!(
            new_pools,
            vec![TransactionEntityChanges {
                tx: Some(Transaction { hash: LAUNCH_TX.to_vec(), index: 1, ..Default::default() }),
                entity_changes: vec![EntityChanges {
                    component_id: id.to_string(),
                    attributes: vec![
                        attribute("balance_owner", &POOL_MANAGER),
                        attribute("liquidity", &[0]),
                        attribute("tick", &hex!("029748")),
                        attribute("sqrt_price_x96", &hex!("12ffcd142cc3f684e7ac97071c0a")),
                        attribute("protocol_fees/zero2one", &[0]),
                        attribute("protocol_fees/one2zero", &[0]),
                    ],
                }],
                component_changes: vec![ProtocolComponent {
                    id: id.to_string(),
                    tokens: vec![vec![0u8; 20], TOKEN.to_vec()],
                    contracts: vec![],
                    static_att: vec![
                        attribute("tick_spacing", &hex!("00c8")),
                        attribute("pool_id", &pool_id),
                        attribute("hooks", &HOOK),
                        attribute("key_lp_fee", &hex!("00800000")),
                    ],
                    change: ChangeType::Creation.into(),
                    protocol_type: Some(ProtocolType {
                        name: "uniswap_v4_pool".to_string(),
                        financial_type: FinancialType::Swap.into(),
                        attribute_schema: vec![],
                        implementation_type: ImplementationType::Custom.into(),
                    }),
                }],
                balance_changes: vec![
                    BalanceChange {
                        token: vec![0u8; 20],
                        balance: vec![0],
                        component_id: id.as_bytes().to_vec(),
                    },
                    BalanceChange {
                        token: TOKEN.to_vec(),
                        balance: vec![0],
                        component_id: id.as_bytes().to_vec(),
                    },
                ],
            }]
        );
    }

    /// A hook whose address sets only bit 8 (`afterRemoveLiquidity`), and neither bit 7
    /// (`beforeSwap`) nor bit 6 (`afterSwap`), creates no pool.
    #[test]
    fn test_initialize_of_a_hook_without_swap_permissions_creates_no_pool() {
        let mut new_pools = Vec::new();
        get_new_pools(
            &launch_block(vec![initialize_log(
                &POOL_MANAGER,
                &hex!("0000000000000000000000000000000000000100"),
            )]),
            &mut new_pools,
            &POOL_MANAGER,
        );

        assert_eq!(new_pools, vec![]);
    }
}
