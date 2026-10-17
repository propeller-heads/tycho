use anyhow::Result;
use substreams::{log, scalar::BigInt};
use substreams_ethereum::{pb::eth::v2 as eth, rpc::RpcBatch, Event};
use tycho_substreams::prelude::*;

use crate::{
    abi::{pendle_market, pendle_market_factory, pendle_market_factory_v1},
    consts::{MARKET_FACTORIES_V3_PLUS, MARKET_FACTORY_V1, PENDLE_MARKET},
    keys::contract_id,
    sy::{profile_sy, read_paused, CREATION_SY_PAUSED},
};

/// Discovers markets from the factories' creation events and resolves what the events omit.
///
/// `CreateNewMarket` carries neither the market's `expiry` nor its SY and YT, and the two
/// factory generations do not even agree on the event's shape, so each creation is followed by
/// `eth_call`s against the market and its SY. These reads are immutable per market, so this is
/// one burst per market lifetime rather than per block.
///
/// Emits two component types: the market itself, and the SY behind it. The SY is emitted on
/// every market that references it; `map_protocol_components` drops the repeats.
#[substreams::handlers::map]
pub fn map_market_components(block: eth::Block) -> Result<BlockTransactionProtocolComponents> {
    let mut tx_components = Vec::new();
    for tx in block.transactions() {
        let mut components = Vec::new();
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            if is_unknown_factory_event(log) {
                log::info!(
                    "Pendle factory 0x{} emitted an event this package does not decode (topic0 \
                     0x{}); the factory may have been upgraded",
                    hex::encode(&log.address),
                    log.topics
                        .first()
                        .map(hex::encode)
                        .unwrap_or_default()
                );
            }
            let Some(creation) = decode_creation(log) else { continue };
            components.extend(build_components(&creation));
        }
        if !components.is_empty() {
            tx_components.push(TransactionProtocolComponents { tx: Some(tx.into()), components });
        }
    }
    Ok(BlockTransactionProtocolComponents { tx_components })
}

/// A market creation, normalised across the two factory generations.
struct MarketCreation {
    factory: Vec<u8>,
    market: Vec<u8>,
    scalar_root: BigInt,
    initial_anchor: BigInt,
    /// The rate the market was deployed with. The original factory does not put it in the event,
    /// so it is zero there and the fee has to come from `getMarketConfig` instead.
    ln_fee_rate_root: BigInt,
}

fn decode_creation(log: &eth::Log) -> Option<MarketCreation> {
    let address: [u8; 20] = log.address.as_slice().try_into().ok()?;
    if address == MARKET_FACTORY_V1 {
        let event = pendle_market_factory_v1::events::CreateNewMarket::match_and_decode(log)?;
        return Some(MarketCreation {
            factory: log.address.clone(),
            market: event.market,
            scalar_root: event.scalar_root,
            initial_anchor: event.initial_anchor,
            ln_fee_rate_root: BigInt::zero(),
        });
    }
    if MARKET_FACTORIES_V3_PLUS.contains(&address) {
        let event = pendle_market_factory::events::CreateNewMarket::match_and_decode(log)?;
        return Some(MarketCreation {
            factory: log.address.clone(),
            market: event.market,
            scalar_root: event.scalar_root,
            initial_anchor: event.initial_anchor,
            ln_fee_rate_root: event.ln_fee_rate_root,
        });
    }
    None
}

/// Whether `log` comes from a known factory but matches no event that factory's ABI declares.
///
/// The V6 factory is an upgradeable proxy. An implementation that changed an event's shape would
/// stop `decode_creation` and `fee_scope` matching it without an error, so new markets or fee
/// updates would go missing silently. Logging what does not match leaves a trace of that. Admin
/// events the ABIs leave out, such as an `Upgraded` on the proxy, are logged too, which is wanted.
fn is_unknown_factory_event(log: &eth::Log) -> bool {
    let Ok(address) = <[u8; 20]>::try_from(log.address.as_slice()) else { return false };
    if address == MARKET_FACTORY_V1 {
        return !(pendle_market_factory_v1::events::CreateNewMarket::match_log(log) ||
            pendle_market_factory_v1::events::NewMarketConfig::match_log(log) ||
            pendle_market_factory_v1::events::SetOverriddenFee::match_log(log) ||
            pendle_market_factory_v1::events::UnsetOverriddenFee::match_log(log));
    }
    if MARKET_FACTORIES_V3_PLUS.contains(&address) {
        return !(pendle_market_factory::events::CreateNewMarket::match_log(log) ||
            pendle_market_factory::events::SetOverriddenFee::match_log(log) ||
            pendle_market_factory::events::NewTreasuryAndFeeReserve::match_log(log));
    }
    false
}

fn build_components(creation: &MarketCreation) -> Vec<ProtocolComponent> {
    let market_hex = format!("0x{}", hex::encode(&creation.market));

    let responses = RpcBatch::new()
        .add(pendle_market::functions::ReadTokens {}, creation.market.clone())
        .add(pendle_market::functions::Expiry {}, creation.market.clone())
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let Some((sy_address, pt_address, yt_address)) = responses
        .first()
        .and_then(RpcBatch::decode::<_, pendle_market::functions::ReadTokens>)
    else {
        log::info!("Skipping market {}: readTokens() did not resolve", market_hex);
        return vec![];
    };
    let Some(expiry) = responses
        .get(1)
        .and_then(RpcBatch::decode::<_, pendle_market::functions::Expiry>)
    else {
        log::info!("Skipping market {}: expiry() did not resolve", market_hex);
        return vec![];
    };

    let sy = profile_sy(&sy_address);

    // `new` rather than `at_contract`: the latter also lists the address in the component's
    // `contracts`, which makes the indexer resolve it to a stored Account. Pendle is a native
    // integration and never emits contract changes, so no such Account is ever written — and an
    // SY typically predates the market that first references it, so it is not created inside any
    // sane indexing range either. The id is identical between the two constructors.
    let mut attributes = vec![
        (
            "scalar_root",
            creation
                .scalar_root
                .to_signed_bytes_be(),
        ),
        (
            "initial_anchor",
            creation
                .initial_anchor
                .to_signed_bytes_be(),
        ),
        ("expiry", expiry.to_signed_bytes_be()),
        ("factory", creation.factory.clone()),
        (
            "ln_fee_rate_root_at_creation",
            creation
                .ln_fee_rate_root
                .to_signed_bytes_be(),
        ),
        ("sy_address", sy_address.clone()),
        ("pt_address", pt_address.clone()),
        ("yt_address", yt_address.clone()),
    ];
    if let Some(sy) = &sy {
        attributes.push(("sy_decimals", BigInt::from(sy.decimals).to_signed_bytes_be()));
        attributes.push(("asset_decimals", BigInt::from(sy.asset_decimals).to_signed_bytes_be()));
    }
    // A paused SY reverts every transfer, so the market cannot trade either. The profile already
    // read it; only a market whose SY did not profile costs a read of its own.
    let sy_paused = match &sy {
        Some(sy) => sy.paused,
        None => read_paused(&sy_address),
    };
    attributes.push((CREATION_SY_PAUSED, vec![u8::from(sy_paused)]));
    let market = ProtocolComponent::new(&contract_id(&creation.market))
        .with_tokens(&[sy_address.clone(), pt_address, yt_address])
        .with_attributes(&attributes)
        .as_swap_type(PENDLE_MARKET, ImplementationType::Custom);

    // The market quotes from its own curve and the SY rate the refresh reads, so an SY that does
    // not resolve now costs only its wrap edges. Its component is left out, which also leaves it
    // out of `store_sy_seen`, so the next market on the same SY profiles it again.
    let Some(sy) = sy else {
        log::info!(
            "Market {}: SY {} did not resolve, emitting the market only",
            market_hex,
            hex::encode(&sy_address)
        );
        return vec![market];
    };

    // An SY whose conversions none of the two closed forms explain contributes no wrap edges,
    // so it gets no component — but its market still quotes PT against SY.
    if sy.tokens_in.is_empty() && sy.tokens_out.is_empty() {
        log::info!(
            "Market {}: SY 0x{} has no quotable tokens, emitting the market only",
            market_hex,
            hex::encode(&sy.address)
        );
        return vec![market];
    }

    vec![market, sy.into_component()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACTORY_V6: &str = "6d247b1c044fa1e22e6b04fa9f71baf99eb29a9f";
    /// Topic0s taken from chain rather than from the bindings under test.
    const NEW_TREASURY_AND_FEE_RESERVE: &str =
        "c612910a1561af820dd8961721344b949df6bfcb3cd8dda1f87a5f25e80852cb";
    const TREASURY_TOPIC: &str = "0000000000000000000000008270400d528c34e1596ef367eedec99080a1b592";
    /// EIP-1967 `Upgraded(address)`.
    const UPGRADED: &str = "bc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b";
    const IMPLEMENTATION_TOPIC: &str =
        "0000000000000000000000001111111111111111111111111111111111111111";

    /// One ABI word of data, which every event used here carries.
    fn log(address: &str, topics: &[&str]) -> eth::Log {
        eth::Log {
            address: hex::decode(address).unwrap(),
            topics: topics
                .iter()
                .map(|t| hex::decode(t).unwrap())
                .collect(),
            data: vec![0; 32],
            ..Default::default()
        }
    }

    #[test]
    fn a_declared_factory_event_is_known() {
        let treasury = log(FACTORY_V6, &[NEW_TREASURY_AND_FEE_RESERVE, TREASURY_TOPIC]);
        assert!(!is_unknown_factory_event(&treasury));
    }

    #[test]
    fn a_proxy_upgrade_on_a_factory_is_unknown() {
        assert!(is_unknown_factory_event(&log(FACTORY_V6, &[UPGRADED, IMPLEMENTATION_TOPIC])));
    }

    /// A known event whose shape changed — here, one indexed topic fewer — no longer matches, which
    /// is the case the guard exists for.
    #[test]
    fn a_declared_event_with_a_changed_shape_is_unknown() {
        assert!(is_unknown_factory_event(&log(FACTORY_V6, &[NEW_TREASURY_AND_FEE_RESERVE])));
    }

    #[test]
    fn a_log_from_any_other_address_is_not_a_factory_event() {
        let market = "34280882267ffa6383b363e278b027be083bbe3b";
        assert!(!is_unknown_factory_event(&log(market, &[UPGRADED, IMPLEMENTATION_TOPIC])));
    }
}
