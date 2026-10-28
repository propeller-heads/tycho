use base_pancakeswap_infinity_cl::{
    abi::cl_pool_manager::events::Initialize,
    modules::map_pool_created::{decode_address, pool_component},
    parameters,
};
use ethabi::ethereum_types::Address;
use serde::Deserialize;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetBigInt},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};
use tycho_substreams::prelude::*;

use super::alpha::{has_alpha_swap_permissions, parse_hooks, start_key, START_TIMESTAMP_ATTRIBUTE};

/// Deployment addresses, hex without `0x`; `hooks` comma-separated.
#[derive(Debug, Deserialize)]
struct Params {
    pool_manager: String,
    vault: String,
    hooks: String,
}

/// One `ProtocolComponent` per `CLPoolManager.Initialize` whose hook is a listed `CLAlphaHook`,
/// registers no swap callback but `beforeSwap`, and charges a static LP fee.
///
/// The component is the CL package's, with two changes: `hooks` is the real hook address, so
/// `UniswapV4State` attaches the `CLAlphaHook` handler, and the state carries
/// `alpha_start_timestamp`, read from `store_alpha_start_times` as of the `Initialize` log.
#[substreams::handlers::map]
pub fn map_alpha_pools_created(
    params: String,
    block: eth::Block,
    start_times: StoreGetBigInt,
) -> Result<BlockEntityChanges, substreams::errors::Error> {
    let params: Params = serde_qs::from_str(&params).map_err(|err| {
        anyhow::anyhow!("Invalid map_alpha_pools_created params {params:?}: {err}")
    })?;
    let pool_manager = decode_address("pool_manager", &params.pool_manager)?;
    let vault = decode_address("vault", &params.vault)?;
    let hooks = parse_hooks(&params.hooks)?;

    let mut new_pools = vec![];
    {
        let mut on_initialize = |event: Initialize, tx: &eth::TransactionTrace, log: &eth::Log| {
            if let Some(pool) =
                alpha_pool(event, tx, log, &pool_manager, &vault, &hooks, &start_times)
            {
                new_pools.push(pool);
            }
        };
        let mut eh = EventHandler::new(&block);
        eh.filter_by_address(vec![Address::from_slice(&pool_manager)]);
        eh.on::<Initialize, _>(&mut on_initialize);
        eh.handle_events();
    }

    Ok(BlockEntityChanges { block: None, changes: new_pools })
}

fn alpha_pool<S: StoreGet<BigInt>>(
    event: Initialize,
    tx: &eth::TransactionTrace,
    log: &eth::Log,
    pool_manager: &[u8],
    vault: &[u8],
    hooks: &[Vec<u8>],
    start_times: &S,
) -> Option<TransactionEntityChanges> {
    let fee: u32 = event.fee.clone().into();
    if !hooks.contains(&event.hooks) ||
        !has_alpha_swap_permissions(&event.parameters) ||
        parameters::is_dynamic_fee(fee)
    {
        return None;
    }

    let pool_id = event.id.to_vec().to_hex();
    let hook = event.hooks.clone();
    // `_beforeInitialize` reverts unless the start time is already set, so the store holds it by
    // the time the log is emitted. Zero, as an unset value would read, keeps the pool closed.
    let start = start_times
        .get_at(log.ordinal, start_key(&pool_id))
        .unwrap_or_else(|| {
            substreams::log::info!("alpha pool {pool_id} initialized without a start time");
            BigInt::zero()
        });

    let mut pool = pool_component(event, tx, log, pool_manager, vault);
    for attribute in &mut pool.component_changes[0].static_att {
        if attribute.name == "hooks" {
            attribute.value = hook.clone();
        }
    }
    pool.entity_changes[0]
        .attributes
        .push(Attribute {
            name: START_TIMESTAMP_ATTRIBUTE.to_string(),
            value: start.to_signed_bytes_be(),
            change: ChangeType::Creation.into(),
        });
    Some(pool)
}

#[cfg(test)]
mod tests {
    use base_pancakeswap_infinity_cl::parameters::{pool_state_base_slot, CL_POOLS_MAPPING_SLOT};
    use rstest::rstest;
    use substreams_ethereum::pb::eth::v2::{Call, StorageChange};

    use super::*;
    use crate::variant_modules::test_store::FakeStore;

    const POOL_MANAGER: [u8; 20] = [0xa0; 20];
    const VAULT: [u8; 20] = [0x23; 20];
    const HOOK: [u8; 20] = [0xb0; 20];
    const POOL_ID: [u8; 32] = [0xab; 32];
    const LOG_ORDINAL: u64 = 20;

    fn parameters(bitmap: u16, tick_spacing: u32) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[16..].copy_from_slice(&(((tick_spacing as u128) << 16) | bitmap as u128).to_be_bytes());
        out
    }

    fn initialize(hook: [u8; 20], bitmap: u16, fee: u32) -> Initialize {
        Initialize {
            id: POOL_ID,
            currency0: vec![0x11; 20],
            currency1: vec![0x22; 20],
            hooks: hook.to_vec(),
            fee: BigInt::from(fee),
            parameters: parameters(bitmap, 10),
            sqrt_price_x96: BigInt::from(1u64) << 96,
            tick: BigInt::zero(),
        }
    }

    /// The creating transaction, with the slot0 write `initialize` makes before the log.
    fn tx() -> eth::TransactionTrace {
        eth::TransactionTrace {
            calls: vec![Call {
                storage_changes: vec![StorageChange {
                    address: POOL_MANAGER.to_vec(),
                    key: pool_state_base_slot(&POOL_ID, CL_POOLS_MAPPING_SLOT).to_vec(),
                    old_value: vec![0; 32],
                    new_value: vec![0x01; 32],
                    ordinal: LOG_ORDINAL - 1,
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn log() -> eth::Log {
        eth::Log { ordinal: LOG_ORDINAL, ..Default::default() }
    }

    fn start_store(writes: Vec<(u64, u64)>) -> FakeStore<BigInt> {
        let key = start_key(&POOL_ID.to_vec().to_hex());
        FakeStore::with(
            writes
                .into_iter()
                .map(|(ordinal, start)| (key.as_str(), ordinal, BigInt::from(start)))
                .collect(),
        )
    }

    fn create(event: Initialize, store: &FakeStore<BigInt>) -> Option<TransactionEntityChanges> {
        alpha_pool(event, &tx(), &log(), &POOL_MANAGER, &VAULT, &[HOOK.to_vec()], store)
    }

    fn attribute<'a>(attributes: &'a [Attribute], name: &str) -> Option<&'a [u8]> {
        attributes
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| attribute.value.as_slice())
    }

    /// The CL component, with the real hook in `hooks` and the start time in the state.
    #[rstest]
    #[case::v2_v3(0x45)]
    #[case::v1(0x55)]
    fn listed_hook_pool_is_created_with_the_hook_and_its_start_time(#[case] bitmap: u16) {
        let store = start_store(vec![(5, 1_759_834_800)]);

        let pool = create(initialize(HOOK, bitmap, 67), &store).expect("an alpha pool is indexed");

        let statics = &pool.component_changes[0].static_att;
        assert_eq!(attribute(statics, "hooks"), Some(HOOK.as_slice()));
        assert_eq!(attribute(statics, "hook_address"), Some(HOOK.as_slice()));
        let state = &pool.entity_changes[0].attributes;
        assert_eq!(
            attribute(state, START_TIMESTAMP_ATTRIBUTE),
            Some(
                BigInt::from(1_759_834_800u64)
                    .to_signed_bytes_be()
                    .as_slice()
            )
        );
        assert_eq!(attribute(state, "balance_owner"), Some(VAULT.as_slice()));
    }

    /// The start time as of the `Initialize` log: a later write in the block is an update, not
    /// the creation value, and no write at all reads as zero, a closed pool.
    #[rstest]
    #[case::set_in_an_earlier_tx(vec![(3, 100)], 100)]
    #[case::moved_before_initialize(vec![(3, 100), (LOG_ORDINAL - 2, 200)], 200)]
    #[case::moved_after_initialize(vec![(3, 100), (LOG_ORDINAL + 1, 300)], 100)]
    #[case::never_set(vec![], 0)]
    fn start_time_is_read_as_of_the_initialize_log(
        #[case] writes: Vec<(u64, u64)>,
        #[case] expected: u64,
    ) {
        let pool = create(initialize(HOOK, 0x45, 67), &start_store(writes)).unwrap();
        assert_eq!(
            attribute(&pool.entity_changes[0].attributes, START_TIMESTAMP_ATTRIBUTE),
            Some(
                BigInt::from(expected)
                    .to_signed_bytes_be()
                    .as_slice()
            )
        );
    }

    #[rstest]
    #[case::unlisted_hook(initialize([0x77; 20], 0x45, 67))]
    #[case::no_hook(initialize([0x00; 20], 0x00, 67))]
    #[case::after_swap_registered(initialize(HOOK, 0x45 | 1 << 7, 67))]
    #[case::returns_delta_registered(initialize(HOOK, 0x45 | 1 << 10, 67))]
    #[case::dynamic_fee(initialize(HOOK, 0x45, 0x800000))]
    fn anything_else_is_left_to_other_packages(#[case] event: Initialize) {
        assert!(create(event, &start_store(vec![(1, 100)])).is_none());
    }
}
