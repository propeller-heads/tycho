use std::collections::HashMap;

use alloy_primitives::Address;
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

const TOTAL_YIELD_TOKEN: [u8; 4] = [0x65, 0xd2, 0xcb, 0x08];
const TOTAL_STABLE_TOKEN: [u8; 4] = [0x9f, 0xf3, 0x90, 0x38];
/// fxSP `previewDeposit(address,uint256)`.
const PREVIEW_DEPOSIT_TOKEN: [u8; 4] = [0xb8, 0xf8, 0x2b, 0x26];
/// fxSP `previewRedeem(uint256)`.
const PREVIEW_REDEEM: [u8; 4] = [0x4c, 0xda, 0xd5, 0x06];
/// fxSP `stableDepegPrice()`.
const STABLE_DEPEG_PRICE: [u8; 4] = [0x61, 0x1e, 0x99, 0x6d];
/// fxSAVE `previewDeposit(uint256)`.
const PREVIEW_DEPOSIT: [u8; 4] = [0xef, 0x8b, 0x30, 0xf7];

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
            for (token, selector) in
                [(config.fxusd, TOTAL_YIELD_TOKEN), (config.usdc, TOTAL_STABLE_TOKEN)]
            {
                let balance = call_uint(&config.fx_base_pool, selector)?;
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

/// Traces the calls the simulation reads, so DCI indexes every contract and slot they touch: the
/// USDC Chainlink feed and fxSP's USDC strategy through `previewDeposit`, and fxSAVE's staked
/// fxSP through its `previewDeposit`.
fn add_entrypoints(builder: &mut TransactionChangesBuilder, config: &Config) {
    let usdc_unit = BigInt::from(1_000_000u64);
    let wad = BigInt::from(10).pow(18);
    let fx_base_pool = config.fx_base_pool.to_vec();
    let calls = [
        (
            &fx_base_pool,
            "previewDeposit(address,uint256)",
            encode(&PREVIEW_DEPOSIT_TOKEN, &[address_word(&config.usdc), uint_word(&usdc_unit)]),
        ),
        (
            &fx_base_pool,
            "previewDeposit(address,uint256)",
            encode(&PREVIEW_DEPOSIT_TOKEN, &[address_word(&config.fxusd), uint_word(&wad)]),
        ),
        (&fx_base_pool, "previewRedeem(uint256)", encode(&PREVIEW_REDEEM, &[uint_word(&wad)])),
        (&fx_base_pool, "stableDepegPrice()", encode(&STABLE_DEPEG_PRICE, &[])),
        (
            &config.fxsave.to_vec(),
            "previewDeposit(uint256)",
            encode(&PREVIEW_DEPOSIT, &[uint_word(&wad)]),
        ),
    ];
    for (target, signature, calldata) in calls {
        let (entrypoint, entrypoint_params) = create_entrypoint(
            target.clone(),
            signature.to_string(),
            config.component_id(),
            TraceData::Rpc(RpcTraceData { caller: None, calldata }),
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
fn call_uint(target: &Address, selector: [u8; 4]) -> Result<BigInt> {
    let calls = rpc::RpcCalls {
        calls: vec![rpc::RpcCall { to_addr: target.to_vec(), data: selector.to_vec() }],
    };
    let responses = substreams_ethereum::rpc::eth_call(&calls).responses;
    let Some(response) = responses.first() else { bail!("eth_call to {target} returned nothing") };
    if response.failed || response.raw.len() != 32 {
        bail!("eth_call 0x{} to {target} failed", hex_selector(&selector));
    }
    Ok(BigInt::from_unsigned_bytes_be(&response.raw))
}

fn hex_selector(selector: &[u8; 4]) -> String {
    selector
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn encode(selector: &[u8; 4], words: &[[u8; 32]]) -> Vec<u8> {
    let mut calldata = selector.to_vec();
    for word in words {
        calldata.extend_from_slice(word);
    }
    calldata
}

fn address_word(address: &Address) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    word
}

fn uint_word(value: &BigInt) -> [u8; 32] {
    let bytes = value.to_bytes_be().1;
    let mut word = [0u8; 32];
    word[32 - bytes.len()..].copy_from_slice(&bytes);
    word
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn encodes_preview_deposit_calldata() {
        let usdc: Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();

        let calldata = encode(
            &PREVIEW_DEPOSIT_TOKEN,
            &[address_word(&usdc), uint_word(&BigInt::from(1_000_000u64))],
        );

        assert_eq!(
            calldata,
            [
                vec![0xb8, 0xf8, 0x2b, 0x26],
                [vec![0u8; 12], usdc.to_vec()].concat(),
                [vec![0u8; 29], vec![0x0f, 0x42, 0x40]].concat(),
            ]
            .concat()
        );
    }
}
