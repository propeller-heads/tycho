//! DollarStore indexing. The package README describes the components and what is indexed.
use std::collections::HashMap;

use anyhow::{anyhow, Result};
use itertools::Itertools;
use serde::Deserialize;
use substreams::{prelude::*, scalar::BigInt, store::StoreGetArray};
use substreams_ethereum::pb::eth::v2::{self as eth, TransactionTrace};
use tiny_keccak::{Hasher, Keccak};
use tycho_substreams::{
    balances::{extract_balance_deltas_from_tx, store_balance_changes},
    block_storage::get_block_storage_changes,
    contract::extract_contract_changes_builder,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

/// keccak256("AssetListed(address,uint16,uint8,address)")
const ASSET_LISTED_TOPIC: [u8; 32] = [
    0x4f, 0x48, 0xde, 0x36, 0x6c, 0x6b, 0x60, 0x95, 0x69, 0x68, 0x9d, 0xb8, 0x09, 0xca, 0xdb, 0xf4,
    0x71, 0xb9, 0x8f, 0xa0, 0x00, 0x7d, 0x51, 0x4c, 0xfd, 0x8f, 0xa0, 0x88, 0xe2, 0x11, 0xcc, 0xa4,
];
/// keccak256("PriceFeedUpdated(address,address)")
const PRICE_FEED_UPDATED_TOPIC: [u8; 32] = [
    0xa8, 0xab, 0xe0, 0x39, 0x84, 0x16, 0x47, 0x6d, 0xb5, 0xb0, 0x57, 0x37, 0xcd, 0x4d, 0xa3, 0xb3,
    0xcb, 0xde, 0x50, 0x12, 0xd9, 0x78, 0xa6, 0xa6, 0xc3, 0xfd, 0x49, 0xd3, 0x21, 0x75, 0x35, 0x36,
];
/// keccak256("Transfer(address,address,uint256)")
const TRANSFER_TOPIC: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];
/// The store's quote, and the two feed reads its peg check makes, each with its selector.
const GET_SWAP_QUOTE: (&str, [u8; 4]) =
    ("getSwapQuote(address,address,uint256)", [0x42, 0x60, 0x67, 0x71]);
const LATEST_ROUND_DATA: (&str, [u8; 4]) = ("latestRoundData()", [0xfe, 0xaf, 0x96, 0x8c]);
const DECIMALS: (&str, [u8; 4]) = ("decimals()", [0x31, 0x3c, 0xe5, 0x67]);
const PROTOCOL_TYPE: &str = "dollarstore_pair";
/// The `store_assets` key listing, in order, every `AssetListed` as `<asset>:<h|s>:<feed>` and
/// every `PriceFeedUpdated` as `<asset>:f:<feed>`, all hex.
const ASSETS_KEY: &str = "assets";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    /// The DollarStore proxy, hex without 0x.
    store: String,
}

fn store_address(params: &str) -> Result<Vec<u8>> {
    let params: Params = serde_qs::from_str(params)?;
    let store = hex::decode(params.store)?;
    if store.len() != 20 {
        return Err(anyhow!("store must be a 20-byte address"));
    }
    Ok(store)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PoolKind {
    Hub,
    Spoke,
}

/// A listed asset, the kind of pool it was first listed in, and its current price feed.
#[derive(Clone, Debug, PartialEq)]
struct Asset {
    address: Vec<u8>,
    kind: PoolKind,
    feed: Vec<u8>,
}

/// The asset and its price feed, if the log is the store's `AssetListed`.
fn asset_listed(log: &eth::Log, store: &[u8]) -> Option<(Asset, Vec<u8>)> {
    if log.address != store ||
        log.topics.len() != 3 ||
        log.topics[0] != ASSET_LISTED_TOPIC ||
        log.data.len() != 64
    {
        return None;
    }
    // poolId is a uint16 topic; pool 0 is the hub. The data is (uint8 decimals, address feed).
    let kind = if log.topics[2].iter().all(|b| *b == 0) { PoolKind::Hub } else { PoolKind::Spoke };
    let feed = log.data[44..64].to_vec();
    Some((Asset { address: log.topics[1][12..].to_vec(), kind, feed: feed.clone() }, feed))
}

/// The asset and its new price feed, if the log is the store's `PriceFeedUpdated`.
fn price_feed_updated(log: &eth::Log, store: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if log.address != store || log.topics.len() != 3 || log.topics[0] != PRICE_FEED_UPDATED_TOPIC {
        return None;
    }
    Some((log.topics[1][12..].to_vec(), log.topics[2][12..].to_vec()))
}

/// Every asset ever listed, each once, in order of first listing, with the pool kind of its first
/// listing and its latest feed.
fn parse_assets(entries: Option<Vec<String>>) -> Vec<Asset> {
    let mut assets: Vec<Asset> = vec![];
    for entry in entries.unwrap_or_default() {
        let mut parts = entry.split(':');
        let (Some(address), Some(kind), Some(feed)) = (parts.next(), parts.next(), parts.next())
        else {
            panic!("store_assets entry written by this package: {entry}");
        };
        let address = hex::decode(address).expect("store_assets entry written by this package");
        let feed = hex::decode(feed).expect("store_assets entry written by this package");
        match assets
            .iter_mut()
            .find(|a| a.address == address)
        {
            Some(asset) => asset.feed = feed,
            None => {
                // A feed change is only accepted for a listed asset, so it never comes first.
                let kind = if kind == "h" { PoolKind::Hub } else { PoolKind::Spoke };
                assets.push(Asset { address, kind, feed });
            }
        }
    }
    assets
}

fn keccak(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Keccak::v256();
    for part in parts {
        hasher.update(part);
    }
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

fn pair_id(store: &[u8], a: &[u8], b: &[u8]) -> String {
    let (token0, token1) = if a < b { (a, b) } else { (b, a) };
    format!("0x{}", hex::encode(keccak(&[store, token0, token1])))
}

/// The assets an asset forms a one-leg pair with: every other asset for a hub asset, the hub
/// assets for a spoke asset.
fn partners<'a>(asset: &'a Asset, assets: &'a [Asset]) -> impl Iterator<Item = &'a Asset> {
    assets.iter().filter(move |other| {
        other.address != asset.address &&
            (asset.kind == PoolKind::Hub || other.kind == PoolKind::Hub)
    })
}

/// Records every `AssetListed` and `PriceFeedUpdated` event of the store.
#[substreams::handlers::store]
fn store_assets(params: String, block: eth::Block, assets: StoreAppend<String>) {
    let store = store_address(&params).expect("invalid params");
    for log in block.logs() {
        let entry = if let Some((asset, feed)) = asset_listed(log.log, &store) {
            let kind = if asset.kind == PoolKind::Hub { "h" } else { "s" };
            format!("{}:{kind}:{}", hex::encode(asset.address), hex::encode(feed))
        } else if let Some((asset, feed)) = price_feed_updated(log.log, &store) {
            format!("{}:f:{}", hex::encode(asset), hex::encode(feed))
        } else {
            continue;
        };
        assets.append(log.ordinal(), ASSETS_KEY, entry);
    }
}

/// Creates a component for each pair a newly listed asset forms with the assets listed before it.
#[substreams::handlers::map]
fn map_components(
    params: String,
    block: eth::Block,
    assets: StoreGetArray<String>,
) -> Result<BlockTransactionProtocolComponents> {
    let store = store_address(&params)?;
    let mut tx_components: Vec<TransactionProtocolComponents> = vec![];
    for log in block.logs() {
        let Some((asset, _)) = asset_listed(log.log, &store) else { continue };
        // store_assets wrote this listing at the log's ordinal, so read just before it.
        let before = parse_assets(assets.get_at(log.ordinal() - 1, ASSETS_KEY));
        if before
            .iter()
            .any(|a| a.address == asset.address)
        {
            continue;
        }
        let components = partners(&asset, &before)
            .map(|other| {
                let tokens = [&asset.address, &other.address]
                    .into_iter()
                    .sorted()
                    .collect::<Vec<_>>();
                ProtocolComponent::new(&pair_id(&store, &asset.address, &other.address))
                    .with_tokens(&tokens)
                    .with_contracts(&[&store])
                    .as_swap_type(PROTOCOL_TYPE, ImplementationType::Vm)
            })
            .collect::<Vec<_>>();
        if components.is_empty() {
            continue;
        }
        let tx: Transaction = log.receipt.transaction.into();
        match tx_components.last_mut() {
            Some(last) if last.tx.as_ref() == Some(&tx) => last.components.extend(components),
            _ => tx_components.push(TransactionProtocolComponents { tx: Some(tx), components }),
        }
    }
    Ok(BlockTransactionProtocolComponents { tx_components })
}

/// Whether a transaction has an ERC-20 transfer into or out of the store. Byte comparisons only,
/// so the transfer decoding runs on these transactions alone.
fn moves_store_tokens(tx: &TransactionTrace, store: &[u8]) -> bool {
    tx.receipt
        .as_ref()
        .is_some_and(|receipt| {
            receipt.logs.iter().any(|log| {
                log.topics.len() == 3 &&
                    log.topics[0] == TRANSFER_TOPIC &&
                    (log.topics[1][12..] == *store || log.topics[2][12..] == *store)
            })
        })
}

/// The store's balance changes in every token, from its transfers, from the store's creation. A
/// token sent before it is listed is counted, so a later `rescueTokens` of it nets out.
#[substreams::handlers::map]
fn map_balance_deltas(params: String, block: eth::Block) -> Result<BlockBalanceDeltas> {
    let store = store_address(&params)?;
    let mut balance_deltas = vec![];
    for tx in block.transactions() {
        if !moves_store_tokens(tx, &store) {
            continue;
        }
        let deltas = extract_balance_deltas_from_tx(tx, |_, holder| holder == store.as_slice());
        // A transfer from the store to itself yields two deltas at one ordinal, which nets to zero
        // and would break the ordinal order store_balance_changes requires.
        let mut per_log: HashMap<(u64, Vec<u8>), usize> = HashMap::new();
        for delta in &deltas {
            *per_log
                .entry((delta.ord, delta.token.clone()))
                .or_default() += 1;
        }
        balance_deltas.extend(
            deltas
                .into_iter()
                .filter(|d| per_log[&(d.ord, d.token.clone())] == 1),
        );
    }
    Ok(BlockBalanceDeltas { balance_deltas })
}

/// The store's absolute balance of each token, keyed `<store hex>:<token hex>`.
#[substreams::handlers::store]
fn store_balances(deltas: BlockBalanceDeltas, balances: StoreAddBigInt) {
    store_balance_changes(deltas, balances);
}

/// The key store_balance_changes writes for a holder and token.
fn balance_key(store: &[u8], token: &[u8]) -> String {
    format!("{}:{}", hex::encode(store), hex::encode(token))
}

fn balance_bytes(balance: Option<BigInt>) -> Vec<u8> {
    let balance = balance.unwrap_or_else(BigInt::zero);
    // Every transfer is counted from the store's creation, so the sum is never negative. Clamp
    // rather than fail, so an unforeseen token cannot halt indexing.
    if balance < BigInt::zero() {
        return vec![0];
    }
    balance.to_bytes_be().1
}

fn word(address: &[u8]) -> Vec<u8> {
    let mut word = vec![0u8; 12];
    word.extend(address);
    word
}

fn get_swap_quote_calldata(offer: &[u8], want: &[u8]) -> Vec<u8> {
    let mut calldata = GET_SWAP_QUOTE.1.to_vec();
    calldata.extend(word(offer));
    calldata.extend(word(want));
    // The largest amount: the quote then reads everything a full fill reads.
    calldata.extend([0xffu8; 32]);
    calldata
}

fn add_entrypoint(
    builder: &mut TransactionChangesBuilder,
    target: &[u8],
    (signature, selector): (&str, [u8; 4]),
    component_id: &str,
    calldata: Vec<u8>,
) {
    let calldata = if calldata.is_empty() { selector.to_vec() } else { calldata };
    let (entrypoint, entrypoint_params) = create_entrypoint(
        target.to_vec(),
        signature.to_string(),
        component_id.to_string(),
        TraceData::Rpc(RpcTraceData { caller: None, calldata }),
    );
    builder.add_entrypoint(&entrypoint);
    builder.add_entrypoint_params(&entrypoint_params);
}

/// Entrypoints on a price feed's two reads. The store's quote reaches the feed only when the
/// asset can be sold in, and DCI retraces only when an address it called changes, so a feed set
/// while its asset is paused would otherwise never be indexed.
fn add_feed_entrypoints(builder: &mut TransactionChangesBuilder, feed: &[u8], component_id: &str) {
    // The store never holds a zero feed: listing calls the feed and setPriceFeed rejects zero.
    for read in [LATEST_ROUND_DATA, DECIMALS] {
        add_entrypoint(builder, feed, read, component_id, vec![]);
    }
}

#[substreams::handlers::map]
fn map_protocol_changes(
    params: String,
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    assets: StoreGetArray<String>,
    balances: StoreGetBigInt,
) -> Result<BlockChanges> {
    let store = store_address(&params)?;
    let mut changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    if !new_components.tx_components.is_empty() {
        let end_ordinals: HashMap<u64, u64> = block
            .transactions()
            .map(|tx| (tx.index as u64, tx.end_ordinal))
            .collect();
        for tx_components in &new_components.tx_components {
            let tx = tx_components
                .tx
                .as_ref()
                .ok_or_else(|| anyhow!("component without a transaction"))?;
            let end_ordinal = *end_ordinals
                .get(&tx.index)
                .ok_or_else(|| anyhow!("component transaction {} not in block", tx.index))?;
            let builder = changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));
            for component in &tx_components.components {
                builder.add_protocol_component(component);
                for (offer, want) in [
                    (&component.tokens[0], &component.tokens[1]),
                    (&component.tokens[1], &component.tokens[0]),
                ] {
                    add_entrypoint(
                        builder,
                        &store,
                        GET_SWAP_QUOTE,
                        &component.id,
                        get_swap_quote_calldata(offer, want),
                    );
                }
                // Both assets' current feeds.
                let listed = parse_assets(assets.get_at(end_ordinal, ASSETS_KEY));
                for asset in listed
                    .iter()
                    .filter(|a| component.tokens.contains(&a.address))
                {
                    add_feed_entrypoints(builder, &asset.feed, &component.id);
                }
                // The component's balances, and the proxy's balances in the VM, start at the
                // store's balances after this transaction.
                let mut contract = InterimContractChange::new(&store, false);
                for token in &component.tokens {
                    let balance =
                        balance_bytes(balances.get_at(end_ordinal, balance_key(&store, token)));
                    contract.upsert_token_balance(token, &balance);
                    builder.add_balance_change(&BalanceChange {
                        token: token.clone(),
                        balance,
                        component_id: component.id.as_bytes().to_vec(),
                    });
                }
                builder.add_contract_changes(&contract);
            }
        }
    }

    // A new feed for a listed asset, from `PriceFeedUpdated` or from listing the asset again: link
    // it to every pair of the asset. A first listing's pairs got their feeds above.
    for log in block.logs() {
        let (token, feed) = if let Some((asset, feed)) = asset_listed(log.log, &store) {
            let before = parse_assets(assets.get_at(log.ordinal() - 1, ASSETS_KEY));
            if !before
                .iter()
                .any(|a| a.address == asset.address)
            {
                continue;
            }
            (asset.address, feed)
        } else if let Some(update) = price_feed_updated(log.log, &store) {
            update
        } else {
            continue;
        };
        let listed = parse_assets(assets.get_at(log.ordinal(), ASSETS_KEY));
        let Some(asset) = listed
            .iter()
            .find(|a| a.address == token)
        else {
            continue
        };
        let tx: Transaction = log.receipt.transaction.into();
        let builder = changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for other in partners(asset, &listed) {
            add_feed_entrypoints(builder, &feed, &pair_id(&store, &token, &other.address));
        }
    }

    // The store's balance of each token goes to the VM as the proxy's token balance, and to every
    // pair of a listed token as its component balance.
    let last_delta = deltas
        .balance_deltas
        .iter()
        .filter_map(|d| Some(((d.tx.clone()?, d.token.clone()), d.ord)))
        .into_grouping_map()
        .max()
        .into_iter()
        .sorted_by_key(|((tx, _), ord)| (tx.index, *ord));
    for ((tx, token), ord) in last_delta {
        let balance = balance_bytes(balances.get_at(ord, balance_key(&store, &token)));
        let listed = parse_assets(assets.get_at(ord, ASSETS_KEY));
        let Some(asset) = listed
            .iter()
            .find(|a| a.address == token)
        else {
            continue
        };
        let builder = changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        let mut contract = InterimContractChange::new(&store, false);
        contract.upsert_token_balance(&token, &balance);
        builder.add_contract_changes(&contract);
        for other in partners(asset, &listed) {
            builder.add_balance_change(&BalanceChange {
                token: token.clone(),
                balance: balance.clone(),
                component_id: pair_id(&store, &token, &other.address).into_bytes(),
            });
        }
    }

    extract_contract_changes_builder(&block, |address| address == store.as_slice(), &mut changes);

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        storage_changes: get_block_storage_changes(&block),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STORE: &str = "825e1e13d804816b8ccda405f1a7b7915461ddc4";
    const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const USDS: &str = "dc035d45d973e3ec169d2276ddab16f1e407384f";
    const USDG: &str = "e343167631d89b6ffc58b88d6b7fb0228795491d";
    const PYUSD: &str = "6c3ea9036406852006290770bedfcaba0e23a0e8";
    const USDG_FEED: &str = "14f0737d6b705259e521ea6e9e3506ac78dbd311";

    fn addr(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    fn topic(address: &str) -> Vec<u8> {
        let mut topic = vec![0u8; 12];
        topic.extend(addr(address));
        topic
    }

    fn asset(address: &str, kind: PoolKind) -> Asset {
        Asset { address: addr(address), kind, feed: addr(USDG_FEED) }
    }

    #[test]
    fn constants_match_their_signatures() {
        let selector = |sig: &str| keccak(&[sig.as_bytes()])[..4].to_vec();
        assert_eq!(keccak(&[b"AssetListed(address,uint16,uint8,address)"]), ASSET_LISTED_TOPIC);
        assert_eq!(keccak(&[b"PriceFeedUpdated(address,address)"]), PRICE_FEED_UPDATED_TOPIC);
        assert_eq!(keccak(&[b"Transfer(address,address,uint256)"]), TRANSFER_TOPIC);
        for (sig, sel) in [GET_SWAP_QUOTE, LATEST_ROUND_DATA, DECIMALS] {
            assert_eq!(selector(sig), sel.to_vec(), "{sig}");
        }
    }

    #[test]
    fn pair_id_matches_the_adapter() {
        // cast keccak $(cast concat-hex <store> <usdc> <usdg>)
        assert_eq!(
            pair_id(&addr(STORE), &addr(USDG), &addr(USDC)),
            "0x097cc630397efe83d759e75de6f9f996bb350abdd015a099f837a2a69c42d5e4"
        );
        assert_eq!(
            pair_id(&addr(STORE), &addr(USDC), &addr(USDS)),
            "0x48883064a1a16978e8857f235d726998fd83be81c77ef5303f7b5d151b7e0b1a"
        );
    }

    #[test]
    fn parse_assets_keeps_first_kind_and_latest_feed() {
        let old_feed = "1111111111111111111111111111111111111111";
        let entries = vec![
            format!("{USDC}:h:{USDG_FEED}"),
            format!("{USDG}:s:{old_feed}"),
            format!("{USDG}:h:{old_feed}"),
            format!("{USDG}:f:{USDG_FEED}"),
        ];
        assert_eq!(
            parse_assets(Some(entries)),
            vec![asset(USDC, PoolKind::Hub), asset(USDG, PoolKind::Spoke)]
        );
        assert!(parse_assets(None).is_empty());
    }

    #[test]
    fn partners_exclude_spoke_to_spoke() {
        let assets = vec![
            asset(USDC, PoolKind::Hub),
            asset(USDS, PoolKind::Hub),
            asset(USDG, PoolKind::Spoke),
            asset(PYUSD, PoolKind::Spoke),
        ];
        let of = |i: usize| {
            partners(&assets[i], &assets)
                .map(|a| hex::encode(&a.address))
                .collect::<Vec<_>>()
        };
        assert_eq!(of(0), vec![USDS, USDG, PYUSD]);
        assert_eq!(of(2), vec![USDC, USDS]);
    }

    fn asset_listed_log(address: &str, pool_id: u8) -> eth::Log {
        let mut pool = [0u8; 32];
        pool[31] = pool_id;
        let mut data = vec![0u8; 31];
        data.push(6); // decimals
        data.extend(topic(USDG_FEED));
        eth::Log {
            address: addr(address),
            topics: vec![ASSET_LISTED_TOPIC.to_vec(), topic(USDG), pool.to_vec()],
            data,
            ..Default::default()
        }
    }

    #[test]
    fn asset_listed_reads_kind_and_feed() {
        assert_eq!(
            asset_listed(&asset_listed_log(STORE, 1), &addr(STORE)),
            Some((asset(USDG, PoolKind::Spoke), addr(USDG_FEED)))
        );
        assert_eq!(
            asset_listed(&asset_listed_log(STORE, 0), &addr(STORE)).map(|(a, _)| a.kind),
            Some(PoolKind::Hub)
        );
    }

    #[test]
    fn asset_listed_ignores_other_emitters() {
        assert_eq!(asset_listed(&asset_listed_log(USDC, 1), &addr(STORE)), None);
    }

    #[test]
    fn price_feed_updated_reads_asset_and_feed() {
        let log = eth::Log {
            address: addr(STORE),
            topics: vec![PRICE_FEED_UPDATED_TOPIC.to_vec(), topic(USDG), topic(USDG_FEED)],
            ..Default::default()
        };
        assert_eq!(price_feed_updated(&log, &addr(STORE)), Some((addr(USDG), addr(USDG_FEED))));
    }

    #[test]
    fn get_swap_quote_calldata_encodes_both_assets_and_max_amount() {
        let calldata = get_swap_quote_calldata(&addr(USDC), &addr(USDG));
        let expected = format!("42606771{:0>64}{:0>64}{}", USDC, USDG, "f".repeat(64));
        assert_eq!(hex::encode(calldata), expected);
    }

    #[test]
    fn balance_key_is_the_store_balance_changes_key() {
        // store_balance_changes keys by "<component_id>:<hex token>", and
        // extract_balance_deltas_from_tx sets component_id to the holder's hex address.
        assert_eq!(balance_key(&addr(STORE), &addr(USDC)), format!("{STORE}:{USDC}"));
    }

    #[test]
    fn negative_balance_is_clamped_to_zero() {
        assert_eq!(balance_bytes(Some(BigInt::from(-5))), vec![0]);
        assert_eq!(balance_bytes(Some(BigInt::from(258))), vec![1, 2]);
        assert_eq!(balance_bytes(None), vec![0]);
    }
}
