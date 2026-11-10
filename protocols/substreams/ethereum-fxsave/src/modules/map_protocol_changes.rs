use std::collections::HashMap;

use alloy_primitives::{keccak256, Address, B256, U256};
use anyhow::{bail, Result};
use itertools::Itertools;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::{self, rpc};
use tycho_substreams::{
    block_storage::get_block_storage_changes,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

use crate::config::Config;

/// fxSP storage slot of `totalYieldToken`, the fxUSD it holds.
const TOTAL_YIELD_TOKEN_SLOT: u8 = 50;
/// fxSP storage slot of `totalStableToken`, the USDC it holds (idle and in its strategy) as of its
/// last sync.
const TOTAL_STABLE_TOKEN_SLOT: u8 = 51;

/// Assembles the `BlockChanges`: the component with its DCI entrypoints and initial balances,
/// then fxSP's fxUSD and USDC holdings whenever they change.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
) -> Result<BlockChanges> {
    let config = Config::parse(&params)?;
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    for tx_components in new_components.tx_components {
        let Some(tx) = tx_components.tx else { bail!("component created without a transaction") };
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for component in &tx_components.components {
            builder.add_protocol_component(component);
            add_entrypoints(builder, &config);
            for (token, getter) in
                [(config.fxusd, "totalYieldToken()"), (config.usdc, "totalStableToken()")]
            {
                let balance = call_uint(&config.fx_base_pool, getter)?;
                builder.add_balance_change(&balance_change(&config, token, balance));
            }
        }
    }

    if block.number > config.creation_block {
        add_holding_changes(&block, &config, &mut transaction_changes);
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        storage_changes: get_block_storage_changes(&block),
    })
}

/// Traces calls that read the state the simulation reads, so DCI indexes every contract and slot
/// behind it. fxSP's `previewDeposit(USDC)` reaches its supply and holdings, the USDC oracle and
/// Chainlink feed, and the USDC strategy; `previewRedeem` reaches the same fxSP state without the
/// oracle, so it stays traced while the oracle reverts; `stableDepegPrice` reaches the depeg gate.
/// fxSAVE's `previewDeposit` reaches its supply, its idle fxSP (`fxSP.balanceOf(fxSAVE)`) and its
/// gauge stake; `getThreshold` reaches the slot that decides whether a deposit stakes into the
/// gauge.
fn add_entrypoints(builder: &mut TransactionChangesBuilder, config: &Config) {
    let usdc_unit: B256 = U256::from(1_000_000u64).into();
    let wad: B256 = U256::from(10u64.pow(18)).into();
    let calls = [
        (
            config.fx_base_pool,
            "previewDeposit(address,uint256)",
            vec![config.usdc.into_word(), usdc_unit],
        ),
        (config.fx_base_pool, "previewRedeem(uint256)", vec![wad]),
        (config.fx_base_pool, "stableDepegPrice()", vec![]),
        (config.fxsave, "previewDeposit(uint256)", vec![wad]),
        (config.fxsave, "getThreshold()", vec![]),
    ];
    for (target, signature, args) in calls {
        let (entrypoint, entrypoint_params) = create_entrypoint(
            target.to_vec(),
            signature.to_string(),
            config.component_id(),
            TraceData::Rpc(RpcTraceData { caller: None, calldata: calldata(signature, &args) }),
        );
        builder.add_entrypoint(&entrypoint);
        builder.add_entrypoint_params(&entrypoint_params);
    }
}

/// Emits fxSP's fxUSD and USDC holdings from the last write to their slots in each transaction.
fn add_holding_changes(
    block: &eth::v2::Block,
    config: &Config,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for tx in block.transactions() {
        let mut latest: HashMap<u8, (u64, Vec<u8>)> = HashMap::new();
        for call in tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address != config.fx_base_pool.as_slice() {
                    continue;
                }
                let Some(slot) = holding_slot(&change.key) else { continue };
                let is_later = latest
                    .get(&slot)
                    .is_none_or(|(ordinal, _)| change.ordinal > *ordinal);
                if is_later {
                    latest.insert(slot, (change.ordinal, change.new_value.clone()));
                }
            }
        }
        if latest.is_empty() {
            continue;
        }
        let tx_meta: Transaction = tx.into();
        let builder = transaction_changes
            .entry(tx_meta.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx_meta));
        for (slot, (_, value)) in latest {
            let token = if slot == TOTAL_YIELD_TOKEN_SLOT { config.fxusd } else { config.usdc };
            let balance = BigInt::from_unsigned_bytes_be(&value);
            builder.add_balance_change(&balance_change(config, token, balance));
        }
    }
}

fn holding_slot(key: &[u8]) -> Option<u8> {
    let (last, rest) = key.split_last()?;
    let is_holding = *last == TOTAL_YIELD_TOKEN_SLOT || *last == TOTAL_STABLE_TOKEN_SLOT;
    (is_holding && rest.iter().all(|byte| *byte == 0)).then_some(*last)
}

fn balance_change(config: &Config, token: Address, balance: BigInt) -> BalanceChange {
    BalanceChange {
        token: token.to_vec(),
        balance: balance.to_bytes_be().1,
        component_id: config.component_id().into_bytes(),
    }
}

/// Reads a `uint256` getter of `target` at this block.
fn call_uint(target: &Address, getter: &str) -> Result<BigInt> {
    let calls = rpc::RpcCalls {
        calls: vec![rpc::RpcCall { to_addr: target.to_vec(), data: calldata(getter, &[]) }],
    };
    let responses = substreams_ethereum::rpc::eth_call(&calls).responses;
    let Some(response) = responses.first() else { bail!("eth_call to {target} returned nothing") };
    if response.failed {
        bail!("eth_call {getter} to {target} reverted");
    }
    if response.raw.len() != 32 {
        bail!("eth_call {getter} to {target} returned {} bytes, not 32", response.raw.len());
    }
    Ok(BigInt::from_unsigned_bytes_be(&response.raw))
}

/// ABI-encodes a call to `signature` whose arguments are all static words.
fn calldata(signature: &str, args: &[B256]) -> Vec<u8> {
    let mut calldata = keccak256(signature)[..4].to_vec();
    for arg in args {
        calldata.extend_from_slice(arg.as_slice());
    }
    calldata
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, hex};
    use rstest::rstest;

    use super::*;
    use crate::config::tests::PARAMS;

    const FX_BASE_POOL: Address = address!("65c9a641afceb9c0e6034e558a319488fa0fa3be");
    const FXSAVE: Address = address!("7743e50f534a7f9f1791dde7dcd89f7783eefc39");

    fn slot_key(slot: u8) -> Vec<u8> {
        let mut key = vec![0u8; 32];
        key[31] = slot;
        key
    }

    #[test]
    fn recognises_only_the_holding_slots() {
        assert_eq!(holding_slot(&slot_key(50)), Some(TOTAL_YIELD_TOKEN_SLOT));
        assert_eq!(holding_slot(&slot_key(51)), Some(TOTAL_STABLE_TOKEN_SLOT));
        assert_eq!(holding_slot(&slot_key(52)), None);

        let mut mapping_key = slot_key(50);
        mapping_key[0] = 1;
        assert_eq!(holding_slot(&mapping_key), None);
    }

    #[rstest]
    #[case::fxsp_preview_deposit("previewDeposit(address,uint256)", "b8f82b26")]
    #[case::fxsp_preview_redeem("previewRedeem(uint256)", "4cdad506")]
    #[case::fxsp_stable_depeg_price("stableDepegPrice()", "611e996d")]
    #[case::fxsave_preview_deposit("previewDeposit(uint256)", "ef8b30f7")]
    #[case::fxsave_get_threshold("getThreshold()", "e75235b8")]
    #[case::fxsp_total_yield_token("totalYieldToken()", "65d2cb08")]
    #[case::fxsp_total_stable_token("totalStableToken()", "9ff39038")]
    fn test_selector_matches_cast_sig(#[case] signature: &str, #[case] selector: &str) {
        assert_eq!(hex::encode(calldata(signature, &[])), selector);
    }

    /// Calldata from `cast calldata`.
    #[test]
    fn test_entrypoints() {
        let config = Config::parse(PARAMS).unwrap();
        let mut builder = TransactionChangesBuilder::new(&Transaction::default());
        // The builder drops a transaction that carries entrypoints alone.
        builder.add_protocol_component(&ProtocolComponent {
            id: config.component_id(),
            ..Default::default()
        });

        add_entrypoints(&mut builder, &config);

        let changes = builder.build().unwrap();
        let signatures: HashMap<&str, (Address, &str)> = changes
            .entrypoints
            .iter()
            .map(|entrypoint| {
                let target = Address::from_slice(&entrypoint.target);
                (entrypoint.id.as_str(), (target, entrypoint.signature.as_str()))
            })
            .collect();
        let calldata: Vec<(&str, String)> = changes
            .entrypoint_params
            .iter()
            .map(|params| match &params.trace_data {
                Some(TraceData::Rpc(rpc)) => {
                    (params.entrypoint_id.as_str(), hex::encode(&rpc.calldata))
                }
                other => panic!("expected RPC trace data, got {other:?}"),
            })
            .collect();
        let traced: Vec<(Address, &str, &str)> = calldata
            .iter()
            .map(|(id, calldata)| {
                let (target, signature) = signatures[id];
                (target, signature, calldata.as_str())
            })
            .sorted_unstable()
            .collect();
        assert_eq!(
            traced,
            vec![
                (
                    FX_BASE_POOL,
                    "previewDeposit(address,uint256)",
                    "b8f82b26000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48\
                     00000000000000000000000000000000000000000000000000000000000f4240",
                ),
                (
                    FX_BASE_POOL,
                    "previewRedeem(uint256)",
                    "4cdad5060000000000000000000000000000000000000000000000000de0b6b3a7640000",
                ),
                (FX_BASE_POOL, "stableDepegPrice()", "611e996d"),
                (FXSAVE, "getThreshold()", "e75235b8"),
                (
                    FXSAVE,
                    "previewDeposit(uint256)",
                    "ef8b30f70000000000000000000000000000000000000000000000000de0b6b3a7640000",
                ),
            ]
        );
    }
}
