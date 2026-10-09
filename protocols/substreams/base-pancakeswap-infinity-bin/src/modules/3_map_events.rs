use crate::{
    abi::bin_pool_manager::events::{Burn, Donate, Initialize, Mint, ProtocolFeeUpdated, Swap},
    pb::pancakeswap::infinity::bin::{
        events::{
            pool_event::{self, Type},
            PoolEvent,
        },
        Events, Pool,
    },
};
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::{
    pb::eth::v2::{self as eth, Log, TransactionTrace},
    Event,
};
use substreams_helper::hex::Hexable;

/// Decodes BinPoolManager logs of known pools into `Events`, sorted by log ordinal.
///
/// `Donate` is decoded here, unlike the CL package: in Bin a donate moves real bin reserves, not
/// just fee growth, so skipping it would drift balances.
///
/// `Mint` and `Burn` keep only `ids[]`. The packed `amounts[]` are ignored because
/// `6_map_bin_changes` reads the resulting reserves from storage instead.
///
/// Ordinals are load-bearing: `6_map_bin_changes` joins these events to storage writes and to
/// `store_active_id` by ordinal, so `log_ordinal` must be the log's own and the output must stay
/// sorted.
#[substreams::handlers::map]
pub fn map_events(
    params: String,
    block: eth::Block,
    pools_store: StoreGetProto<Pool>,
) -> Result<Events, anyhow::Error> {
    let pool_manager = hex::decode(&params).expect("pool manager is hex");
    let mut pool_manager_events = block
        .transaction_traces
        .into_iter()
        .filter(|tx| tx.status == 1)
        .flat_map(|tx| {
            let receipt = tx
                .receipt
                .as_ref()
                .expect("all transaction traces have a receipt");

            receipt
                .logs
                .iter()
                .filter_map(|log| log_to_event(log, &tx, &pool_manager, &pools_store))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    pool_manager_events.sort_unstable_by_key(|e| e.log_ordinal);

    Ok(Events { pool_events: pool_manager_events })
}

/// Resolves the pool the log belongs to and wraps the decoded payload. `decode_log` already
/// rejected other emitters, so an unknown pool id here was filtered out at creation.
fn log_to_event(
    log: &Log,
    tx: &TransactionTrace,
    pool_manager: &[u8],
    pools_store: &StoreGetProto<Pool>,
) -> Option<PoolEvent> {
    let (pool_id, event) = decode_log(log, pool_manager)?;
    let pool = pools_store.get_last(format!("pool:{pool_id}"))?;

    Some(PoolEvent {
        log_ordinal: log.ordinal,
        pool_id,
        currency0: pool.currency0.to_hex(),
        currency1: pool.currency1.to_hex(),
        transaction: Some(tx.into()),
        r#type: Some(event),
    })
}

/// Pool id and payload of a BinPoolManager log, `None` for any other log.
///
/// `Initialize` is decoded here as well as in `1_map_pool_created` because `store_active_id` seeds
/// the active bin from it.
fn decode_log(log: &Log, pool_manager: &[u8]) -> Option<(String, Type)> {
    if log.address != pool_manager {
        return None;
    }
    if let Some(init) = Initialize::match_and_decode(log) {
        Some((
            init.id.to_vec().to_hex(),
            Type::Initialize(pool_event::Initialize {
                fee: init.fee.into(),
                bin_step: crate::parameters::bin_step(&init.parameters) as u32,
                active_id: init.active_id.into(),
                hooks: init.hooks.to_vec().to_hex(),
                parameters: init.parameters.to_vec(),
            }),
        ))
    } else if let Some(swap) = Swap::match_and_decode(log) {
        Some((
            swap.id.to_vec().to_hex(),
            Type::Swap(pool_event::Swap {
                sender: swap.sender.to_hex(),
                amount0: swap.amount0.to_string(),
                amount1: swap.amount1.to_string(),
                active_id: swap.active_id.into(),
                fee: swap.fee.into(),
            }),
        ))
    } else if let Some(mint) = Mint::match_and_decode(log) {
        Some((
            mint.id.to_vec().to_hex(),
            Type::Mint(pool_event::Mint {
                sender: mint.sender.to_hex(),
                ids: bin_ids(&mint.ids),
                salt: mint.salt.to_vec().to_hex(),
            }),
        ))
    } else if let Some(burn) = Burn::match_and_decode(log) {
        Some((
            burn.id.to_vec().to_hex(),
            Type::Burn(pool_event::Burn {
                sender: burn.sender.to_hex(),
                ids: bin_ids(&burn.ids),
                salt: burn.salt.to_vec().to_hex(),
            }),
        ))
    } else if let Some(donate) = Donate::match_and_decode(log) {
        Some((
            donate.id.to_vec().to_hex(),
            Type::Donate(pool_event::Donate {
                sender: donate.sender.to_hex(),
                amount0: donate.amount0.to_string(),
                amount1: donate.amount1.to_string(),
                bin_id: donate.bin_id.into(),
            }),
        ))
    } else if let Some(fee_updated) = ProtocolFeeUpdated::match_and_decode(log) {
        let pool_id = fee_updated.id.to_vec().to_hex();
        Some((
            pool_id.clone(),
            Type::ProtocolFeeUpdated(pool_event::ProtocolFeeUpdated {
                pool_id,
                protocol_fee: fee_updated.protocol_fee.into(),
            }),
        ))
    } else {
        None
    }
}

/// Bin ids are `uint256[]` on the wire, u24 in practice.
fn bin_ids(ids: &[substreams::scalar::BigInt]) -> Vec<u32> {
    ids.iter()
        .map(|id| Into::<u32>::into(id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tiny_keccak::{Hasher, Keccak};

    const POOL_ID: [u8; 32] = [0xab; 32];
    const SENDER: [u8; 20] = [0x11; 20];
    const POOL_MANAGER: [u8; 20] = [0xc6; 20];

    fn topic(signature: &str) -> Vec<u8> {
        let mut hasher = Keccak::v256();
        hasher.update(signature.as_bytes());
        let mut out = [0u8; 32];
        hasher.finalize(&mut out);
        out.to_vec()
    }

    fn word(value: u128) -> Vec<u8> {
        let mut out = vec![0u8; 32];
        out[16..].copy_from_slice(&value.to_be_bytes());
        out
    }

    fn padded(bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; 32 - bytes.len()];
        out.extend_from_slice(bytes);
        out
    }

    fn log(signature: &str, indexed: &[Vec<u8>], data: Vec<Vec<u8>>) -> Log {
        let mut topics = vec![topic(signature)];
        topics.extend(indexed.iter().cloned());

        Log {
            address: POOL_MANAGER.to_vec(),
            topics,
            data: data.concat(),
            index: 0,
            block_index: 0,
            ordinal: 7,
        }
    }

    /// `parameters` carries the bin step in bits 16..32, and `activeId` is a u24 in a full word.
    #[test]
    fn initialize_carries_bin_step_and_active_id() {
        let mut parameters = vec![0u8; 32];
        // Bin step above an empty hook bitmap.
        parameters[28..].copy_from_slice(&(10u32 << 16).to_be_bytes());
        let log = log(
            "Initialize(bytes32,address,address,address,uint24,bytes32,uint24)",
            &[POOL_ID.to_vec(), padded(&SENDER), padded(&[0x22; 20])],
            vec![word(0), word(7), parameters, word(8_388_608)],
        );

        let (pool_id, event) = decode_log(&log, &POOL_MANAGER).expect("Initialize decodes");

        assert_eq!(pool_id, POOL_ID.to_vec().to_hex());
        let Type::Initialize(init) = event else { panic!("wrong event type") };
        assert_eq!((init.fee, init.bin_step, init.active_id), (7, 10, 8_388_608));
    }

    /// `amount0` and `amount1` are int128, so a sold side must survive as a negative string.
    #[test]
    fn swap_keeps_signed_amounts() {
        let mut negative = vec![0xffu8; 32];
        negative[31] = 0xfe;
        let log = log(
            "Swap(bytes32,address,int128,int128,uint24,uint24,uint16)",
            &[POOL_ID.to_vec(), padded(&SENDER)],
            vec![word(1_000), negative, word(8_388_607), word(7), word(0)],
        );

        let (_, event) = decode_log(&log, &POOL_MANAGER).expect("Swap decodes");

        let Type::Swap(swap) = event else { panic!("wrong event type") };
        assert_eq!((swap.amount0.as_str(), swap.amount1.as_str()), ("1000", "-2"));
        assert_eq!(swap.active_id, 8_388_607);
    }

    #[test]
    fn foreign_emitter_is_skipped() {
        let mut log = log(
            "Swap(bytes32,address,int128,int128,uint24,uint24,uint16)",
            &[POOL_ID.to_vec(), padded(&SENDER)],
            vec![word(1_000), word(0), word(8_388_607), word(7), word(0)],
        );
        log.address = vec![0x77; 20];

        assert!(decode_log(&log, &POOL_MANAGER).is_none(), "only the pool manager's logs count");
    }

    /// Only the six BinPoolManager events decode; everything else in a block is skipped.
    #[rstest]
    #[case::wrong_signature("Transfer(address,address,uint256)")]
    #[case::cl_pool_event("Swap(bytes32,address,int128,int128,uint160,uint24,int24,uint24)")]
    fn unrelated_logs_are_skipped(#[case] signature: &str) {
        let log = log(signature, &[POOL_ID.to_vec(), padded(&SENDER)], vec![word(0)]);

        assert!(
            decode_log(&log, &POOL_MANAGER).is_none(),
            "{signature} must not decode as a Bin event"
        );
    }
}
