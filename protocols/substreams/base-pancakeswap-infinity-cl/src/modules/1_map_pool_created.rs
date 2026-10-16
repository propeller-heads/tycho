use crate::{
    abi::cl_pool_manager::events::Initialize,
    parameters::{self, CL_POOLS_MAPPING_SLOT},
};
use ethabi::ethereum_types::Address;
use serde::Deserialize;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};
use tycho_substreams::prelude::*;

/// Per-chain deployment addresses, hex without the 0x prefix.
#[derive(Debug, Deserialize, PartialEq)]
struct Params {
    /// `CLPoolManager`, the emitter of `Initialize`.
    pool_manager: String,
    /// Holds every pool's funds, so it is `balance_owner` on every component.
    vault: String,
}

fn decode_address(name: &str, value: &str) -> Result<Vec<u8>, substreams::errors::Error> {
    let bytes =
        hex::decode(value).map_err(|err| anyhow::anyhow!("Invalid {name} {value:?}: {err}"))?;
    if bytes.len() != 20 {
        return Err(anyhow::anyhow!(
            "Invalid {name} {value:?}: expected 20 bytes, got {}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// One `ProtocolComponent` per `CLPoolManager.Initialize`, for pools with no swap hook and a
/// static LP fee.
///
/// Ported from `ethereum-uniswap-v4/no-hooks/src/variant_modules/1_map_pool_created.rs`. Attribute
/// names are v4's so `UniswapV4State` decodes the components, except the hook address: it is
/// `hook_address`, never `hooks`, because that decoder attaches a VM hook handler to any component
/// with a `hooks` attribute.
#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
) -> Result<BlockEntityChanges, substreams::errors::Error> {
    let mut new_pools: Vec<TransactionEntityChanges> = vec![];
    let params: Params = serde_qs::from_str(&params)
        .map_err(|err| anyhow::anyhow!("Invalid map_pools_created params {params:?}: {err}"))?;
    let pool_manager = decode_address("pool_manager", &params.pool_manager)?;
    let vault = decode_address("vault", &params.vault)?;

    get_new_pools(&block, &mut new_pools, &pool_manager, &vault);

    Ok(BlockEntityChanges { block: None, changes: new_pools })
}

/// Collects the pools worth indexing from this block's `Initialize` logs.
fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionEntityChanges>,
    pool_manager: &[u8],
    vault: &[u8],
) {
    let mut on_pool_created = |event: Initialize, tx: &eth::TransactionTrace, log: &eth::Log| {
        if let Some(changes) = pool_created(event, tx, log, pool_manager, vault) {
            new_pools.push(changes);
        }
    };

    let mut eh = EventHandler::new(block);
    eh.filter_by_address(vec![Address::from_slice(pool_manager)]);
    eh.on::<Initialize, _>(&mut on_pool_created);
    eh.handle_events();
}

/// One component per pool, or `None` for a pool with a swap hook or a dynamic fee: both make the
/// quote depend on code this package does not index.
fn pool_created(
    event: Initialize,
    tx: &eth::TransactionTrace,
    log: &eth::Log,
    pool_manager: &[u8],
    vault: &[u8],
) -> Option<TransactionEntityChanges> {
    let fee: u32 = event.fee.clone().into();
    if parameters::has_swap_hooks(&event.parameters) || parameters::is_dynamic_fee(fee) {
        return None;
    }

    let component_id = event.id.to_vec().to_hex();
    let (fee_zero2one, fee_one2zero) = initial_protocol_fees(&event, tx, log, pool_manager);

    let mut static_att = vec![
        attribute(
            "tick_spacing",
            BigInt::from(parameters::tick_spacing(&event.parameters)).to_signed_bytes_be(),
        ),
        // Raw bytes32, as the v4 packages do. The reserved-attributes doc says UTF-8 string, but
        // no consumer reads it.
        attribute("pool_id", event.id.to_vec()),
        // Static LP fee in hundredths of a bip, kept raw because rebuilding the PoolKey needs it.
        attribute("key_lp_fee", event.fee.to_signed_bytes_be()),
        attribute("parameters", event.parameters.to_vec()),
        attribute("pool_manager", pool_manager.to_vec()),
        // Always zero: swap-hook pools are filtered above, so `UniswapV4State` treats every pool
        // as hookless and accepts an empty tick set. The real hook, if any, is `hook_address`.
        attribute("hooks", vec![0u8; 20]),
    ];
    if event
        .hooks
        .iter()
        .any(|byte| *byte != 0)
    {
        static_att.push(attribute("hook_address", event.hooks.to_vec()));
    }

    Some(TransactionEntityChanges {
        tx: Some(tx.into()),
        entity_changes: vec![EntityChanges {
            component_id: component_id.clone(),
            attributes: vec![
                attribute("balance_owner", vault.to_vec()),
                attribute("liquidity", BigInt::from(0).to_signed_bytes_be()),
                attribute("tick", event.tick.to_signed_bytes_be()),
                attribute(
                    "sqrt_price_x96",
                    event
                        .sqrt_price_x96
                        .to_signed_bytes_be(),
                ),
                attribute(
                    "protocol_fees/zero2one",
                    BigInt::from(fee_zero2one).to_signed_bytes_be(),
                ),
                attribute(
                    "protocol_fees/one2zero",
                    BigInt::from(fee_one2zero).to_signed_bytes_be(),
                ),
            ],
        }],
        component_changes: vec![ProtocolComponent {
            id: component_id.clone(),
            tokens: vec![event.currency0.clone(), event.currency1.clone()],
            contracts: vec![],
            static_att,
            change: i32::from(ChangeType::Creation),
            protocol_type: Some(ProtocolType {
                name: "pancakeswap_infinity_cl_pool".to_string(),
                financial_type: FinancialType::Swap.into(),
                attribute_schema: vec![],
                implementation_type: ImplementationType::Custom.into(),
            }),
        }],
        balance_changes: [event.currency0, event.currency1]
            .into_iter()
            .map(|token| BalanceChange {
                token,
                balance: BigInt::from(0).to_signed_bytes_be(),
                component_id: component_id.as_bytes().to_vec(),
            })
            .collect(),
    })
}

/// Protocol fee halves from the slot0 write `initialize` makes just before emitting the log, since
/// `Initialize` does not carry them. The last non-reverted write below the log's ordinal is the
/// pool's initial state.
/// [`_fetchProtocolFee`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/ProtocolFees.sol#L46-L78),
/// [`initialize`](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/CLPoolManager.sol#L113-L120).
fn initial_protocol_fees(
    event: &Initialize,
    tx: &eth::TransactionTrace,
    log: &eth::Log,
    pool_manager: &[u8],
) -> (u32, u32) {
    let slot0_key = parameters::pool_state_base_slot(&event.id, CL_POOLS_MAPPING_SLOT);
    let slot0_write = tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
        .filter(|change| {
            change.address == pool_manager &&
                change.key == slot0_key &&
                change.ordinal < log.ordinal
        })
        .max_by_key(|change| change.ordinal)
        .unwrap_or_else(|| {
            panic!(
                "no slot0 storage write for pool {} in tx {}",
                event.id.to_vec().to_hex(),
                tx.hash.to_hex()
            )
        });
    let slot0: [u8; 32] = slot0_write
        .new_value
        .clone()
        .try_into()
        .expect("slot0 is 32 bytes");

    parameters::split_protocol_fee(parameters::protocol_fee_from_slot0(&slot0))
}

/// Every attribute this module emits is a creation.
fn attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Creation.into() }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use substreams_ethereum::pb::eth::v2::{
        Block, Call, Log, StorageChange, TransactionReceipt, TransactionTrace,
    };
    use tiny_keccak::{Hasher, Keccak};

    use super::*;
    use crate::parameters::{
        fixtures::{pool_key_parameters, slot0_with_protocol_fee},
        CL_POOLS_MAPPING_SLOT,
    };

    const POOL_MANAGER: &str = "a0FfB9c1CE1Fe56963B0321B32E7A0302114058b";
    const VAULT: &str = "238a358808379702088667322f80aC48bAd5e6c4";
    const CURRENCY0: [u8; 20] = [0x11; 20];
    const CURRENCY1: [u8; 20] = [0x22; 20];
    const POOL_ID: [u8; 32] = [0xab; 32];
    const LOG_ORDINAL: u64 = 5;

    fn keccak(input: &[u8]) -> [u8; 32] {
        let mut hasher = Keccak::v256();
        hasher.update(input);
        let mut out = [0u8; 32];
        hasher.finalize(&mut out);
        out
    }

    fn word_from_address(address: &[u8; 20]) -> Vec<u8> {
        let mut word = vec![0u8; 12];
        word.extend_from_slice(address);
        word
    }

    fn word_from_u128(value: u128) -> Vec<u8> {
        let mut word = vec![0u8; 16];
        word.extend_from_slice(&value.to_be_bytes());
        word
    }

    /// `Initialize` as emitted: 3 indexed topics after the signature (`id`, `currency0`,
    /// `currency1`), then five words in `data` (`hooks`, `fee`, `parameters`, `sqrtPriceX96`,
    /// `tick`). `match_log` wants exactly 4 topics and 160 bytes.
    fn initialize_log(hooks: &[u8; 20], fee: u32, params: &[u8; 32]) -> Log {
        let mut data = Vec::with_capacity(160);
        data.extend(word_from_address(hooks));
        data.extend(word_from_u128(fee as u128));
        data.extend_from_slice(params);
        data.extend(word_from_u128(1u128 << 96)); // sqrtPriceX96 = 1.0
        data.extend(word_from_u128(0)); // tick 0
        Log {
            address: hex::decode(POOL_MANAGER).unwrap(),
            topics: vec![
                keccak(b"Initialize(bytes32,address,address,address,uint24,bytes32,uint160,int24)")
                    .to_vec(),
                POOL_ID.to_vec(),
                word_from_address(&CURRENCY0),
                word_from_address(&CURRENCY1),
            ],
            data,
            index: 0,
            block_index: 0,
            ordinal: LOG_ORDINAL,
        }
    }

    /// One successful tx: `log` in the receipt, `storage_changes` in one call. `EventHandler`
    /// walks receipt logs, `get_new_pools` walks `tx.calls[..].storage_changes`.
    fn block_with(log: Log, storage_changes: Vec<StorageChange>) -> Block {
        block_with_calls(log, vec![Call { storage_changes, ..Default::default() }])
    }

    fn block_with_calls(log: Log, calls: Vec<Call>) -> Block {
        Block {
            transaction_traces: vec![TransactionTrace {
                hash: vec![0x33; 32],
                from: vec![0x44; 20],
                to: hex::decode(POOL_MANAGER).unwrap(),
                index: 7,
                status: 1,
                receipt: Some(TransactionReceipt { logs: vec![log], ..Default::default() }),
                calls,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The slot0 storage write `map_pools_created` looks for, at `keccak(pool_id ++ 4)`.
    fn slot0_write(new_value: [u8; 32]) -> StorageChange {
        slot0_write_at(new_value, 1)
    }

    fn slot0_write_at(new_value: [u8; 32], ordinal: u64) -> StorageChange {
        StorageChange {
            address: hex::decode(POOL_MANAGER).unwrap(),
            key: parameters::pool_state_base_slot(&POOL_ID, CL_POOLS_MAPPING_SLOT).to_vec(),
            old_value: vec![0u8; 32],
            new_value: new_value.to_vec(),
            ordinal,
        }
    }

    fn run(block: &Block) -> Vec<TransactionEntityChanges> {
        let mut new_pools = vec![];
        get_new_pools(
            block,
            &mut new_pools,
            &hex::decode(POOL_MANAGER).unwrap(),
            &hex::decode(VAULT).unwrap(),
        );
        new_pools
    }

    fn attribute<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a Attribute> {
        attrs.iter().find(|a| a.name == name)
    }

    fn assert_attr(attrs: &[Attribute], name: &str, expected: impl Into<Vec<u8>>) {
        assert_eq!(attribute(attrs, name).unwrap().value, expected.into(), "attribute {name}");
    }

    fn static_pool_block(hooks: &[u8; 20], fee: u32, hook_bitmap: u16) -> Block {
        block_with(
            initialize_log(hooks, fee, &pool_key_parameters(hook_bitmap, 60)),
            vec![slot0_write(slot0_with_protocol_fee(200, 300))],
        )
    }

    #[test]
    fn params_parse_pool_manager_and_vault() {
        let params: Params =
            serde_qs::from_str(&format!("pool_manager={POOL_MANAGER}&vault={VAULT}")).unwrap();
        assert_eq!(
            params,
            Params { pool_manager: POOL_MANAGER.to_string(), vault: VAULT.to_string() }
        );
        assert!(serde_qs::from_str::<Params>(POOL_MANAGER).is_err(), "bare address is rejected");
        assert!(decode_address("vault", "0x1234").is_err(), "0x prefix and short input rejected");
    }

    #[test]
    fn static_hookless_pool_is_emitted_with_infinity_attributes() {
        let pools = run(&static_pool_block(&[0u8; 20], 500, 0));

        assert_eq!(pools.len(), 1);
        let pool = &pools[0];
        let component = &pool.component_changes[0];
        assert_eq!(component.id, POOL_ID.to_vec().to_hex());
        assert_eq!(component.tokens, vec![CURRENCY0.to_vec(), CURRENCY1.to_vec()]);
        assert_eq!(
            component
                .protocol_type
                .as_ref()
                .unwrap()
                .name,
            "pancakeswap_infinity_cl_pool"
        );

        let statics = &component.static_att;
        assert_attr(statics, "tick_spacing", BigInt::from(60).to_signed_bytes_be());
        assert_attr(statics, "key_lp_fee", BigInt::from(500).to_signed_bytes_be());
        assert_attr(statics, "parameters", pool_key_parameters(0, 60));
        assert_attr(statics, "pool_manager", hex::decode(POOL_MANAGER).unwrap());
        assert_attr(statics, "pool_id", POOL_ID.to_vec());
        assert_attr(statics, "hooks", vec![0u8; 20]);
        assert!(
            attribute(statics, "hook_address").is_none(),
            "a hookless pool must not carry a hook address"
        );

        let state = &pool.entity_changes[0].attributes;
        assert_attr(state, "balance_owner", hex::decode(VAULT).unwrap());
        assert_attr(state, "protocol_fees/zero2one", BigInt::from(200).to_signed_bytes_be());
        assert_attr(state, "protocol_fees/one2zero", BigInt::from(300).to_signed_bytes_be());

        assert_eq!(pool.balance_changes.len(), 2);
        assert!(
            pool.balance_changes
                .iter()
                .all(|b| b.balance == BigInt::from(0).to_signed_bytes_be()),
            "a new pool starts with zero balances"
        );
    }

    #[test]
    fn liquidity_only_hook_pool_is_emitted_with_hook_address() {
        let before_add_liquidity = 1 << 2;
        let pools = run(&static_pool_block(&[0x77u8; 20], 500, before_add_liquidity));

        assert_eq!(pools.len(), 1);
        let statics = &pools[0].component_changes[0].static_att;
        assert_attr(statics, "hook_address", vec![0x77u8; 20]);
        assert_attr(statics, "hooks", vec![0u8; 20]);
    }

    /// Any of the four swap callbacks puts the pool out of scope.
    #[rstest]
    #[case::before_swap(parameters::HOOKS_BEFORE_SWAP_OFFSET)]
    #[case::after_swap(parameters::HOOKS_AFTER_SWAP_OFFSET)]
    #[case::before_swap_returns_delta(parameters::HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET)]
    #[case::after_swap_returns_delta(parameters::HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET)]
    fn swap_hook_pool_is_skipped(#[case] bit: u8) {
        assert!(
            run(&static_pool_block(&[0x77u8; 20], 500, 1 << bit)).is_empty(),
            "swap hook bit {bit} must put the pool out of scope"
        );
    }

    #[test]
    fn dynamic_fee_pool_is_skipped() {
        assert!(
            run(&static_pool_block(&[0u8; 20], parameters::DYNAMIC_FEE_FLAG, 0)).is_empty(),
            "dynamic-fee pools are out of scope"
        );
    }

    #[test]
    #[should_panic(expected = "no slot0 storage write")]
    fn missing_slot0_write_panics() {
        let block =
            block_with(initialize_log(&[0u8; 20], 500, &pool_key_parameters(0, 60)), vec![]);
        run(&block);
    }

    #[test]
    fn reverted_and_later_slot0_writes_are_ignored() {
        let good = slot0_write_at(slot0_with_protocol_fee(200, 300), 1);
        let reverted = slot0_write_at(slot0_with_protocol_fee(1, 1), 2);
        let after_log = slot0_write_at(slot0_with_protocol_fee(2, 2), LOG_ORDINAL + 1);
        let block = block_with_calls(
            initialize_log(&[0u8; 20], 500, &pool_key_parameters(0, 60)),
            vec![
                Call { storage_changes: vec![good], ..Default::default() },
                Call {
                    storage_changes: vec![reverted],
                    state_reverted: true,
                    ..Default::default()
                },
                Call { storage_changes: vec![after_log], ..Default::default() },
            ],
        );

        let pools = run(&block);
        let state = &pools[0].entity_changes[0].attributes;
        assert_attr(state, "protocol_fees/zero2one", BigInt::from(200).to_signed_bytes_be());
        assert_attr(state, "protocol_fees/one2zero", BigInt::from(300).to_signed_bytes_be());
    }
}
