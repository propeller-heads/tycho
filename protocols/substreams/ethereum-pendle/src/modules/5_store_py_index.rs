use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetString, StoreNew, StoreSet, StoreSetBigInt},
};
use substreams_ethereum::{
    pb::eth::{rpc::RpcResponse, v2 as eth},
    rpc::RpcBatch,
};
use tycho_substreams::prelude::*;

use crate::{
    abi::pendle_yield_token,
    consts::PENDLE_MARKET,
    keys::{markets_by_yt_key, py_index_key},
    market_state::py_index_stored,
    registry::markets_of_yt,
};

/// Tracks each market's `pyIndexStored`, the floor half of the live PY index.
///
/// `map_protocol_changes` needs the value on refresh blocks, where the yield token has emitted
/// nothing, so it cannot be read off this block's logs — it has to be carried.
///
/// New markets are seeded with an `eth_call`. `NewInterestIndex` fires only on PY mint, redeem
/// and interest collection, which can be weeks apart, so without a seed the floor would be
/// missing for the whole gap and the index would follow the SY rate alone — wrong for any market
/// whose SY rate has fallen since its last interaction.
#[substreams::handlers::store]
pub fn store_py_index(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    markets_by_yt: StoreGetString,
    store: StoreSetBigInt,
) {
    for (component_id, index) in seed_new_markets(&new_components) {
        store.set(0, py_index_key(&component_id), &index);
    }

    let listed = |yt: &[u8]| markets_by_yt.get_last(markets_by_yt_key(yt));
    for (ordinal, market, index) in interest_index_updates(&block, listed) {
        store.set(ordinal, py_index_key(&market), &index);
    }
}

/// Every `NewInterestIndex` in `block`, as `(ordinal, market, index)` for each market `listed`
/// names for the emitting yield token, in block order. A yield token `listed` names no market for
/// yields nothing.
fn interest_index_updates(
    block: &eth::Block,
    listed: impl Fn(&[u8]) -> Option<String>,
) -> Vec<(u64, String, BigInt)> {
    let mut updates = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(index) = py_index_stored(log) else { continue };
            let Some(markets) = listed(&log.address) else { continue };
            for market in markets_of_yt(&markets) {
                updates.push((log.ordinal, market, index.clone()));
            }
        }
    }
    updates
}

/// Reads `pyIndexStored()` off the yield token of every market created in this block.
///
/// A yield token that will not answer is left unseeded rather than guessed at: the index then
/// falls back to the SY rate until the first `NewInterestIndex`.
fn seed_new_markets(components: &BlockTransactionProtocolComponents) -> Vec<(String, BigInt)> {
    let markets = new_markets(components);
    if markets.is_empty() {
        return vec![];
    }

    let mut batch = RpcBatch::new();
    for (_, yt) in &markets {
        batch = batch.add(pendle_yield_token::functions::PyIndexStored {}, yt.clone());
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();
    decoded_seeds(markets, &responses)
}

/// Every market created in `components`, with the address of its yield token.
///
/// Panics if a market carries no `yt_address`.
fn new_markets(components: &BlockTransactionProtocolComponents) -> Vec<(String, Vec<u8>)> {
    let mut markets = Vec::new();
    for tx_components in &components.tx_components {
        for component in &tx_components.components {
            let is_market = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_MARKET);
            if !is_market {
                continue;
            }
            let yt = component
                .get_attribute_value("yt_address")
                .unwrap_or_else(|| panic!("market {} has no yt_address", component.id));
            markets.push((component.id.clone(), yt));
        }
    }
    markets
}

/// Pairs each market with the `pyIndexStored()` its yield token answered, the responses being in
/// market order. A market whose response failed, did not decode, or is missing is left out.
fn decoded_seeds(
    markets: Vec<(String, Vec<u8>)>,
    responses: &[RpcResponse],
) -> Vec<(String, BigInt)> {
    let mut seeds = Vec::new();
    for ((component_id, _), response) in markets
        .into_iter()
        .zip(responses.iter())
    {
        let Some(index) =
            RpcBatch::decode::<_, pendle_yield_token::functions::PyIndexStored>(response)
        else {
            continue;
        };
        seeds.push((component_id, index));
    }
    seeds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::PENDLE_SY;

    /// The wstETH market, its SY and yield token, and the PY index that yield token reported at
    /// block 25807775 (see `crate::market_state`); a second market on the same yield token.
    const MARKET: &str = "0x34280882267ffa6383b363e278b027be083bbe3b";
    const SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
    const YT: &str = "04b7fa1e727d7290d6e24fa9b426d0c940283a95";
    const PY_INDEX: u64 = 1242190142783145750;
    const OTHER_MARKET: &str = "0x00000000000000000000000000000000000000cc";
    const OTHER_YT: &str = "00000000000000000000000000000000000000f1";
    /// `keccak("NewInterestIndex(uint256)")`.
    const NEW_INTEREST_INDEX_TOPIC: &str =
        "71475f2f645813fdbebf53a58968008bff11ee21a58f01c5a9cc263d0bc4703d";

    fn word(value: u64) -> Vec<u8> {
        let mut word = vec![0u8; 24];
        word.extend_from_slice(&value.to_be_bytes());
        word
    }

    fn market(id: &str, yt: Option<&str>) -> ProtocolComponent {
        let attributes: Vec<(&str, Vec<u8>)> = yt
            .map(|yt| ("yt_address", hex::decode(yt).unwrap()))
            .into_iter()
            .collect();
        ProtocolComponent::new(id)
            .with_attributes(&attributes)
            .as_swap_type(PENDLE_MARKET, ImplementationType::Custom)
    }

    fn created(components: Vec<ProtocolComponent>) -> BlockTransactionProtocolComponents {
        BlockTransactionProtocolComponents {
            tx_components: vec![TransactionProtocolComponents {
                tx: Some(Transaction { index: 1, ..Default::default() }),
                components,
            }],
        }
    }

    fn answered(index: u64) -> RpcResponse {
        RpcResponse { raw: word(index), failed: false }
    }

    fn reverted() -> RpcResponse {
        RpcResponse { raw: vec![], failed: true }
    }

    fn new_interest_index(yt: &str, index: u64, ordinal: u64) -> eth::Log {
        eth::Log {
            address: hex::decode(yt).unwrap(),
            topics: vec![hex::decode(NEW_INTEREST_INDEX_TOPIC).unwrap(), word(index)],
            ordinal,
            ..Default::default()
        }
    }

    fn block(logs: Vec<eth::Log>) -> eth::Block {
        eth::Block {
            transaction_traces: vec![eth::TransactionTrace {
                status: 1,
                calls: vec![eth::Call { logs, ..Default::default() }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Only markets are seeded, each from its own yield token; an SY created alongside is not.
    #[test]
    fn every_new_market_is_seeded_from_its_yield_token() {
        let components = created(vec![
            market(MARKET, Some(YT)),
            ProtocolComponent::new(SY).as_swap_type(PENDLE_SY, ImplementationType::Custom),
            market(OTHER_MARKET, Some(OTHER_YT)),
        ]);
        assert_eq!(
            new_markets(&components),
            vec![
                (MARKET.to_string(), hex::decode(YT).unwrap()),
                (OTHER_MARKET.to_string(), hex::decode(OTHER_YT).unwrap()),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "has no yt_address")]
    fn a_market_without_its_yield_token_halts() {
        new_markets(&created(vec![market(MARKET, None)]));
    }

    /// A yield token that does not answer leaves its market unseeded rather than guessed at, and
    /// the markets after it keep their own answers. A batch that failed as a whole seeds nothing.
    #[test]
    fn a_market_whose_yield_token_does_not_answer_is_left_unseeded() {
        let markets = vec![
            (MARKET.to_string(), hex::decode(YT).unwrap()),
            (OTHER_MARKET.to_string(), hex::decode(OTHER_YT).unwrap()),
            ("0x00000000000000000000000000000000000000dd".to_string(), vec![0xdd; 20]),
        ];
        assert_eq!(
            decoded_seeds(markets.clone(), &[reverted(), answered(PY_INDEX), answered(7)]),
            vec![
                (OTHER_MARKET.to_string(), BigInt::from(PY_INDEX)),
                ("0x00000000000000000000000000000000000000dd".to_string(), BigInt::from(7)),
            ]
        );
        assert!(decoded_seeds(markets, &[]).is_empty());
    }

    /// A `NewInterestIndex` moves every market its yield token backs, at the log's ordinal. A yield
    /// token backing no market moves nothing.
    #[test]
    fn an_interest_index_moves_every_market_of_its_yield_token() {
        let block =
            block(vec![new_interest_index(YT, PY_INDEX, 7), new_interest_index(OTHER_YT, 1, 8)]);
        let listed =
            |yt: &[u8]| (hex::encode(yt) == YT).then(|| format!("{MARKET};{OTHER_MARKET};"));
        assert_eq!(
            interest_index_updates(&block, listed),
            vec![
                (7, MARKET.to_string(), BigInt::from(PY_INDEX)),
                (7, OTHER_MARKET.to_string(), BigInt::from(PY_INDEX)),
            ]
        );
    }
}
