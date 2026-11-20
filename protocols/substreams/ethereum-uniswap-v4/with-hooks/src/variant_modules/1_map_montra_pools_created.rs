//! Creates a component for every pool the `PoolManager` initialises with a Montra or an Ascend
//! hook. The Elysium manifest runs it in place of `map_pools_created`, which creates every pool
//! whose hook has swap permissions. Both build the component with [`pool_creation`].

use ethereum_uniswap_v4_shared::abi::pool_manager::events::Initialize;
use substreams_ethereum::{pb::eth::v2 as eth, Event};
use tycho_substreams::prelude::*;

use crate::{map_pool_created::pool_creation, montra::params::Params};

#[substreams::handlers::map]
pub fn map_montra_pools_created(
    params: String,
    block: eth::Block,
) -> Result<BlockEntityChanges, substreams::errors::Error> {
    let params = Params::parse(&params)?;
    Ok(BlockEntityChanges { block: None, changes: pools_created(&params, &block) })
}

/// One [`pool_creation`] per `Initialize` the configured `PoolManager` emitted in `block` for a
/// pool whose `PoolKey.hooks` is a configured Montra or Ascend hook. Besides the emitter check, the
/// hook lists are the only filter: no pool of another hook is created, Montra's own later hooks
/// included, and every pool of a listed hook is, whatever its tokens, fee or tick spacing. Every
/// pool of these hooks has `key_lp_fee` 0x800000, the dynamic-fee flag; the `montra_*` and
/// `ascend_*` attributes carry its fees.
///
/// Deployment assumption: the Ascend launch writes `PoolData` words 0 to 3 and `Bid` word 2 in the
/// same transaction as `Initialize`. Observed in the prestate trace of one launch transaction; the
/// other launches are known only by their `Launched` events (4962 pools to block 8365800), which
/// show that each pool launched, not what each launch wrote.
/// `map_montra_enriched_block_changes` reads those writes for the pool's `ascend_*` attributes, so
/// a pool initialised without them would get zero for every attribute, and the simulation refuses
/// a pool whose fee or EMA is zero.
pub(crate) fn pools_created(params: &Params, block: &eth::Block) -> Vec<TransactionEntityChanges> {
    block
        .logs()
        // This filter runs first because any contract can emit an `Initialize` with a real pool
        // id.
        .filter(|view| view.log.address == *params.pool_manager())
        .filter_map(|view| {
            Initialize::match_and_decode(view.log).map(|event| (view.receipt.transaction, event))
        })
        .filter(|(_, event)| {
            params.is_montra_hook(&event.hooks) || params.is_ascend_hook(&event.hooks)
        })
        .map(|(tx, event)| pool_creation(tx, event, params.pool_manager()))
        .collect()
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use rstest::rstest;

    use super::*;
    use crate::{
        address_list::Address,
        montra::test_fixtures::{
            initialize_log, launch_block, params, params_with_montra_hooks, ASCEND_HOOK, HOOK,
            LAUNCH_TX, POOL_MANAGER,
        },
    };

    /// `map_pools_created`'s tests pin every field `pool_creation` builds; this one checks what
    /// the Montra module passes it: the transaction, the event and the `PoolManager`.
    #[test]
    fn test_montra_initialize_creates_the_pool() {
        let changes =
            pools_created(&params(), &launch_block(vec![initialize_log(&POOL_MANAGER, &HOOK)]));

        assert_eq!(changes.len(), 1);
        let change = &changes[0];
        let tx = change.tx.as_ref().unwrap();
        assert_eq!((tx.hash.as_slice(), tx.index), (LAUNCH_TX.as_slice(), 1));
        let id = "0x35bd0e6a8b82e0e18b0381757d97ba01cde11a02740f006132dd40395ffc8ce2";
        assert_eq!(
            change
                .component_changes
                .iter()
                .map(|component| component.id.as_str())
                .collect::<Vec<_>>(),
            vec![id]
        );
        let entity = &change.entity_changes[0];
        assert_eq!(entity.component_id, id);
        assert_eq!(
            (entity.attributes[0].name.as_str(), entity.attributes[0].value.as_slice()),
            ("balance_owner", hex!("e2e105d3f7209a9da11f83ccf3e7398a753823f1").as_slice())
        );
    }

    /// An `Initialize` creates no pool unless the `PoolManager` emitted it for a listed hook.
    #[rstest]
    #[case::pool_of_another_hook(POOL_MANAGER, hex!("0000000000000000000000000000000000000080"))]
    #[case::initialize_from_another_contract(hex!("00000000000000000000000000000000000000aa"), HOOK)]
    fn test_initialize_creates_no_pool(#[case] emitter: Address, #[case] hook: Address) {
        let changes =
            pools_created(&params(), &launch_block(vec![initialize_log(&emitter, &hook)]));

        assert_eq!(changes, vec![]);
    }

    /// An `Initialize` of a pool of the Ascend hook creates the pool, with the hook as a static
    /// attribute, exactly as for a Montra pool.
    #[test]
    fn test_ascend_initialize_creates_the_pool() {
        let changes = pools_created(
            &params(),
            &launch_block(vec![initialize_log(&POOL_MANAGER, &ASCEND_HOOK)]),
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].component_changes[0].static_att[2].value, ASCEND_HOOK.to_vec());
    }

    #[test]
    fn test_every_listed_hook_creates_pools() {
        let second_hook = hex!("0000000000000000000000000000000000000080");
        let params = params_with_montra_hooks(&[HOOK, second_hook]);

        let changes = pools_created(
            &params,
            &launch_block(vec![
                initialize_log(&POOL_MANAGER, &HOOK),
                initialize_log(&POOL_MANAGER, &second_hook),
                initialize_log(&POOL_MANAGER, &ASCEND_HOOK),
            ]),
        );

        assert_eq!(
            changes
                .iter()
                .map(|change| change.component_changes[0].static_att[2]
                    .value
                    .clone())
                .collect::<Vec<_>>(),
            vec![HOOK.to_vec(), second_hook.to_vec(), ASCEND_HOOK.to_vec()]
        );
    }
}
