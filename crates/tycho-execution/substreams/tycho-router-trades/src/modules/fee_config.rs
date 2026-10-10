//! Fee configuration tracking: FeeCalculator admin events and router fee-calculator rotations.
use anyhow::Result;
use substreams::{
    prelude::*,
    store::{StoreDelete, StoreSetString},
};
use substreams_ethereum::{
    pb::eth::v2::{Block, Log},
    Event,
};

use super::{block_timestamp, events, hex_addr, keys};
use crate::{
    abi::{
        fee_calculator::events as fc, fee_calculator_exemption::events as fc_exempt,
        fee_calculator_v3_0::events as fc_v3_0, tycho_router_v3_0::events as router_v3_0,
        tycho_router_v3_1::events as router_v3_1,
    },
    params::Params,
    pb::tycho::router::v1::{FeeConfigEvent, FeeConfigEvents},
};

/// Emits every FeeCalculator admin event and every fee-calculator rotation on a known router.
///
/// FeeCalculator events are matched by topic from any emitter (the FeeCalculator constructor
/// emits nothing, so the set of calculators is only known through router events and params);
/// the store keys them by emitter so unrelated emitters never influence a trade.
#[substreams::handlers::map]
pub fn map_fee_config_events(params: String, block: Block) -> Result<FeeConfigEvents> {
    let params = Params::parse(&params)?;
    let timestamp = block_timestamp(&block);
    let mut events = Vec::new();
    for tx in block.transactions() {
        for log in tx
            .receipt
            .as_ref()
            .map(|r| r.logs.as_slice())
            .unwrap_or_default()
        {
            let is_router = params.router(&log.address).is_some();
            let decoded =
                if is_router { decode_router_event(log) } else { decode_fee_calculator_event(log) };
            let Some(decoded) = decoded else {
                continue;
            };
            events.push(FeeConfigEvent {
                chain: params.chain.clone(),
                block_number: block.number,
                block_timestamp: timestamp,
                tx_hash: tx.hash.clone(),
                log_index: log.index,
                ordinal: log.ordinal,
                emitter: log.address.clone(),
                event: decoded.event.to_string(),
                client: decoded.client,
                old_value: decoded.old_value,
                new_value: decoded.new_value,
                bps_scale: decoded.bps_scale.unwrap_or_default(),
            });
        }
    }
    Ok(FeeConfigEvents { events })
}

/// Bps denominators of the two FeeCalculator generations.
const BPS_SCALE_UINT16: u64 = 10_000;
const BPS_SCALE_UINT32: u64 = 100_000_000;

/// One decoded fee-config event.
struct Decoded {
    event: &'static str,
    client: Vec<u8>,
    old_value: String,
    new_value: String,
    /// Denominator of the bps values the emitting calculator uses, when the event gives its
    /// generation away.
    ///
    /// The two generations widened the bps arguments from `uint16` to `uint32`, so an event that
    /// carries one has a different signature, and a different topic, in each. An event that
    /// carries no bps value has the same signature in both and identifies nothing, and a router
    /// event says nothing about a calculator at all; both leave this `None`.
    bps_scale: Option<u64>,
}

/// Whether the log is one the fee-config decoders recognise, from any emitter.
///
/// The block index uses this to keep the blocks `map_fee_config_events` needs. It knows no
/// router, so it tries both decoders.
pub(crate) fn is_fee_config_log(log: &Log) -> bool {
    decode_router_event(log).is_some() || decode_fee_calculator_event(log).is_some()
}

fn decode_router_event(log: &Log) -> Option<Decoded> {
    // A router event names a calculator; it says nothing about that calculator's generation.
    let rotation = |event, old_value, new_value| {
        Some(Decoded { event, client: Vec::new(), old_value, new_value, bps_scale: None })
    };
    if let Some(ev) = router_v3_1::FeeCalculatorActivated::match_and_decode(log) {
        return rotation(
            events::FEE_CALCULATOR_ACTIVATED,
            hex_addr(&ev.old_calculator),
            hex_addr(&ev.new_calculator),
        );
    }
    if let Some(ev) = router_v3_1::FeeCalculatorSet::match_and_decode(log) {
        return rotation(
            events::FEE_CALCULATOR_SET,
            ev.timelock_expires_at.to_string(),
            hex_addr(&ev.fee_calculator),
        );
    }
    if let Some(ev) = router_v3_0::FeeCalculatorUpdated::match_and_decode(log) {
        return rotation(
            events::FEE_CALCULATOR_UPDATED,
            hex_addr(&ev.old_calculator),
            hex_addr(&ev.new_calculator),
        );
    }
    None
}

fn decode_fee_calculator_event(log: &Log) -> Option<Decoded> {
    // Both ABIs are tried because the argument width puts each generation's events on their
    // own topics. That same difference is what names the denominator.
    macro_rules! bps_event {
        ($ty:ty, $name:expr, $scale:expr) => {
            if let Some(ev) = <$ty>::match_and_decode(log) {
                return Some(Decoded {
                    event: $name,
                    client: Vec::new(),
                    old_value: ev.old_fee_bps.to_string(),
                    new_value: ev.new_fee_bps.to_string(),
                    bps_scale: Some($scale),
                });
            }
        };
    }
    macro_rules! custom_bps_event {
        ($ty:ty, $name:expr, $scale:expr) => {
            if let Some(ev) = <$ty>::match_and_decode(log) {
                return Some(Decoded {
                    event: $name,
                    client: ev.client,
                    old_value: ev.old_fee_bps.to_string(),
                    new_value: ev.new_fee_bps.to_string(),
                    bps_scale: Some($scale),
                });
            }
        };
    }
    // Carries no bps value, so it has one signature in both generations and names neither.
    macro_rules! removed_event {
        ($ty:ty, $name:expr) => {
            if let Some(ev) = <$ty>::match_and_decode(log) {
                return Some(Decoded {
                    event: $name,
                    client: ev.client,
                    old_value: String::new(),
                    new_value: String::new(),
                    bps_scale: None,
                });
            }
        };
    }
    macro_rules! receiver_event {
        ($ty:ty) => {
            if let Some(ev) = <$ty>::match_and_decode(log) {
                return Some(Decoded {
                    event: events::ROUTER_FEE_RECEIVER_UPDATED,
                    client: Vec::new(),
                    old_value: hex_addr(&ev.old_receiver),
                    new_value: hex_addr(&ev.new_receiver),
                    bps_scale: None,
                });
            }
        };
    }
    bps_event!(
        fc::RouterFeeOnOutputUpdated,
        events::ROUTER_FEE_ON_OUTPUT_UPDATED,
        BPS_SCALE_UINT32
    );
    bps_event!(
        fc_v3_0::RouterFeeOnOutputUpdated,
        events::ROUTER_FEE_ON_OUTPUT_UPDATED,
        BPS_SCALE_UINT16
    );
    bps_event!(
        fc::RouterFeeOnClientFeeUpdated,
        events::ROUTER_FEE_ON_CLIENT_FEE_UPDATED,
        BPS_SCALE_UINT32
    );
    bps_event!(
        fc_v3_0::RouterFeeOnClientFeeUpdated,
        events::ROUTER_FEE_ON_CLIENT_FEE_UPDATED,
        BPS_SCALE_UINT16
    );
    custom_bps_event!(
        fc::CustomRouterFeeOnOutputUpdated,
        events::CUSTOM_FEE_ON_OUTPUT_UPDATED,
        BPS_SCALE_UINT32
    );
    custom_bps_event!(
        fc_v3_0::CustomRouterFeeOnOutputUpdated,
        events::CUSTOM_FEE_ON_OUTPUT_UPDATED,
        BPS_SCALE_UINT16
    );
    custom_bps_event!(
        fc::CustomRouterFeeOnClientFeeUpdated,
        events::CUSTOM_FEE_ON_CLIENT_FEE_UPDATED,
        BPS_SCALE_UINT32
    );
    custom_bps_event!(
        fc_v3_0::CustomRouterFeeOnClientFeeUpdated,
        events::CUSTOM_FEE_ON_CLIENT_FEE_UPDATED,
        BPS_SCALE_UINT16
    );
    removed_event!(fc::CustomRouterFeeOnOutputRemoved, events::CUSTOM_FEE_ON_OUTPUT_REMOVED);
    removed_event!(fc_v3_0::CustomRouterFeeOnOutputRemoved, events::CUSTOM_FEE_ON_OUTPUT_REMOVED);
    removed_event!(fc::CustomRouterFeeOnClientFeeRemoved, events::CUSTOM_FEE_ON_CLIENT_FEE_REMOVED);
    removed_event!(
        fc_v3_0::CustomRouterFeeOnClientFeeRemoved,
        events::CUSTOM_FEE_ON_CLIENT_FEE_REMOVED
    );
    receiver_event!(fc::RouterFeeReceiverUpdated);
    receiver_event!(fc_v3_0::RouterFeeReceiverUpdated);
    // Positive slippage arrived with the newer generation, so either event names it.
    if let Some(ev) = fc_exempt::PositiveSlippageExemptionSet::match_and_decode(log) {
        return Some(Decoded {
            event: events::POSITIVE_SLIPPAGE_EXEMPTION_SET,
            client: ev.client,
            old_value: String::new(),
            new_value: if ev.exempt { "1".to_string() } else { "0".to_string() },
            bps_scale: Some(BPS_SCALE_UINT32),
        });
    }
    if let Some(ev) = fc::PositiveSlippageToggled::match_and_decode(log) {
        return Some(Decoded {
            event: events::POSITIVE_SLIPPAGE_TOGGLED,
            client: Vec::new(),
            old_value: String::new(),
            new_value: if ev.enabled { "1".to_string() } else { "0".to_string() },
            bps_scale: Some(BPS_SCALE_UINT32),
        });
    }
    None
}

/// Materialises the fee configuration so `map_trades` can resolve the bps in effect per trade.
#[substreams::handlers::store]
pub fn store_fee_config(events: FeeConfigEvents, store: StoreSetString) {
    for ev in &events.events {
        for action in scale_action(ev)
            .into_iter()
            .chain(std::iter::once(store_action(ev)))
        {
            match action {
                StoreAction::Set { key, value } => store.set(ev.ordinal, key, &value),
                StoreAction::DeletePrefix(key) => store.delete_prefix(ev.ordinal as i64, &key),
                StoreAction::Ignore => {}
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum StoreAction {
    Set { key: String, value: String },
    DeletePrefix(String),
    Ignore,
}

/// The scale an event gives away about its emitter, as a store write.
///
/// A router can be rotated onto a calculator of another generation, so the scale belongs to the
/// calculator that emitted the event, not to the router that resolves to it.
fn scale_action(ev: &FeeConfigEvent) -> Option<StoreAction> {
    if ev.bps_scale == 0 {
        return None;
    }
    Some(StoreAction::Set {
        key: keys::fee_bps_scale(&ev.emitter),
        value: ev.bps_scale.to_string(),
    })
}

fn store_action(ev: &FeeConfigEvent) -> StoreAction {
    let fc = ev.emitter.as_slice();
    let set = |key| StoreAction::Set { key, value: ev.new_value.clone() };
    match ev.event.as_str() {
        events::ROUTER_FEE_ON_OUTPUT_UPDATED => set(keys::fee_on_output(fc)),
        events::ROUTER_FEE_ON_CLIENT_FEE_UPDATED => set(keys::fee_on_client_fee(fc)),
        events::CUSTOM_FEE_ON_OUTPUT_UPDATED => set(keys::custom_fee_on_output(fc, &ev.client)),
        events::CUSTOM_FEE_ON_CLIENT_FEE_UPDATED => {
            set(keys::custom_fee_on_client_fee(fc, &ev.client))
        }
        events::CUSTOM_FEE_ON_OUTPUT_REMOVED => {
            StoreAction::DeletePrefix(keys::custom_fee_on_output(fc, &ev.client))
        }
        events::CUSTOM_FEE_ON_CLIENT_FEE_REMOVED => {
            StoreAction::DeletePrefix(keys::custom_fee_on_client_fee(fc, &ev.client))
        }
        events::POSITIVE_SLIPPAGE_TOGGLED => set(keys::positive_slippage(fc)),
        events::POSITIVE_SLIPPAGE_EXEMPTION_SET => {
            set(keys::positive_slippage_exempt(fc, &ev.client))
        }
        events::FEE_CALCULATOR_ACTIVATED | events::FEE_CALCULATOR_UPDATED => {
            set(keys::router_fee_calculator(fc))
        }
        events::ROUTER_FEE_RECEIVER_UPDATED | events::FEE_CALCULATOR_SET => StoreAction::Ignore,
        other => {
            substreams::log::info!("ignoring unknown fee config event {}", other);
            StoreAction::Ignore
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str) -> FeeConfigEvent {
        FeeConfigEvent {
            emitter: vec![0xaa; 20],
            client: vec![0xbb; 20],
            event: name.to_string(),
            new_value: "42".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn plans_global_and_custom_fee_updates() {
        assert_eq!(
            store_action(&event(events::ROUTER_FEE_ON_OUTPUT_UPDATED)),
            StoreAction::Set { key: keys::fee_on_output(&[0xaa; 20]), value: "42".to_string() }
        );
        assert_eq!(
            store_action(&event(events::CUSTOM_FEE_ON_CLIENT_FEE_UPDATED)),
            StoreAction::Set {
                key: keys::custom_fee_on_client_fee(&[0xaa; 20], &[0xbb; 20]),
                value: "42".to_string(),
            }
        );
    }

    #[test]
    fn plans_positive_slippage_exemption() {
        assert_eq!(
            store_action(&event(events::POSITIVE_SLIPPAGE_EXEMPTION_SET)),
            StoreAction::Set {
                key: keys::positive_slippage_exempt(&[0xaa; 20], &[0xbb; 20]),
                value: "42".to_string(),
            }
        );
    }

    /// keccak256("RouterFeeOnOutputUpdated(uint16,uint16)"), the first generation.
    const FEE_ON_OUTPUT_UINT16: &str =
        "050cb5c90bbab79023a902aac364bc8e45cca66341fa7800d99e2590f8c1e85d";
    /// keccak256("RouterFeeOnOutputUpdated(uint32,uint32)"), the generation after it.
    const FEE_ON_OUTPUT_UINT32: &str =
        "c7592fa0fb5814aa63c60ac263038cefb86531a5d16bbf3c5e9ef59eb6087059";
    /// keccak256("CustomRouterFeeOnOutputRemoved(address)"), which both generations share.
    const CUSTOM_REMOVED: &str = "9dbcfb046701dc2252b780788e18747ee949d31085c2836c2332c1a067d5b0e2";

    fn bps_log(topic: &str, old: u64, new: u64) -> Log {
        let mut data = Vec::with_capacity(64);
        for value in [old, new] {
            data.extend_from_slice(&[0u8; 24]);
            data.extend_from_slice(&value.to_be_bytes());
        }
        Log { topics: vec![hex::decode(topic).unwrap()], data, ..Default::default() }
    }

    /// The rate 0.1% as each generation writes it: 10 of 10000, then 100000 of 100000000. Both
    /// numbers are what the deployed calculators really emitted.
    #[test]
    fn a_bps_event_names_the_generation_that_emitted_it() {
        let first = decode_fee_calculator_event(&bps_log(FEE_ON_OUTPUT_UINT16, 0, 10)).unwrap();
        assert_eq!(first.event, events::ROUTER_FEE_ON_OUTPUT_UPDATED);
        assert_eq!(first.new_value, "10");
        assert_eq!(first.bps_scale, Some(BPS_SCALE_UINT16));

        let next = decode_fee_calculator_event(&bps_log(FEE_ON_OUTPUT_UINT32, 0, 100_000)).unwrap();
        assert_eq!(next.event, events::ROUTER_FEE_ON_OUTPUT_UPDATED);
        assert_eq!(next.new_value, "100000");
        assert_eq!(next.bps_scale, Some(BPS_SCALE_UINT32));
    }

    #[test]
    fn an_event_carrying_no_bps_names_no_generation() {
        let mut client_topic = vec![0u8; 12];
        client_topic.extend_from_slice(&[0xcc; 20]);
        let log = Log {
            topics: vec![hex::decode(CUSTOM_REMOVED).unwrap(), client_topic],
            ..Default::default()
        };
        let decoded = decode_fee_calculator_event(&log).unwrap();
        assert_eq!(decoded.event, events::CUSTOM_FEE_ON_OUTPUT_REMOVED);
        assert_eq!(decoded.bps_scale, None);
    }

    #[test]
    fn stores_the_scale_against_the_emitter() {
        let mut ev = event(events::ROUTER_FEE_ON_OUTPUT_UPDATED);
        ev.bps_scale = BPS_SCALE_UINT32;
        assert_eq!(
            scale_action(&ev),
            Some(StoreAction::Set {
                key: keys::fee_bps_scale(&[0xaa; 20]),
                value: "100000000".to_string(),
            })
        );
    }

    #[test]
    fn stores_no_scale_for_an_event_that_names_none() {
        assert_eq!(scale_action(&event(events::CUSTOM_FEE_ON_OUTPUT_REMOVED)), None);
    }

    /// The fallback for a calculator no observed event gives away is the generation the router
    /// shipped with, and V2 has no calculator at all.
    #[test]
    fn a_router_generation_still_offers_a_default_scale() {
        use crate::params::RouterVersion;
        assert_eq!(RouterVersion::V2.default_bps_scale(), None);
        assert_eq!(RouterVersion::V3_0.default_bps_scale(), Some(BPS_SCALE_UINT16));
        assert_eq!(RouterVersion::V3_1.default_bps_scale(), Some(BPS_SCALE_UINT32));
    }

    /// keccak256("PositiveSlippageExemptionSet(address,bool)").
    const EXEMPTION_TOPIC: &str =
        "a6baebb00b4a9c84cac5db0f46f5d661a368efaf0577353494dd33083ec5979a";

    fn exemption_log(client: [u8; 20], exempt: bool) -> Log {
        let mut client_topic = vec![0u8; 12];
        client_topic.extend_from_slice(&client);
        let mut data = vec![0u8; 32];
        data[31] = u8::from(exempt);
        Log {
            topics: vec![hex::decode(EXEMPTION_TOPIC).unwrap(), client_topic],
            data,
            ..Default::default()
        }
    }

    #[test]
    fn decodes_positive_slippage_exemption() {
        let decoded = decode_fee_calculator_event(&exemption_log([0xcc; 20], true)).unwrap();
        assert_eq!(decoded.event, events::POSITIVE_SLIPPAGE_EXEMPTION_SET);
        assert_eq!(decoded.client, vec![0xcc; 20]);
        assert_eq!(decoded.old_value, String::new());
        assert_eq!(decoded.new_value, "1");
        // Positive slippage only exists on the newer generation, so the event names its scale.
        assert_eq!(decoded.bps_scale, Some(BPS_SCALE_UINT32));
    }

    #[test]
    fn decodes_positive_slippage_exemption_removal() {
        let decoded = decode_fee_calculator_event(&exemption_log([0xcc; 20], false));
        assert_eq!(decoded.map(|d| d.new_value), Some("0".to_string()));
    }

    #[test]
    fn plans_custom_fee_removal() {
        assert_eq!(
            store_action(&event(events::CUSTOM_FEE_ON_OUTPUT_REMOVED)),
            StoreAction::DeletePrefix(keys::custom_fee_on_output(&[0xaa; 20], &[0xbb; 20]))
        );
    }

    #[test]
    fn ignores_fee_receiver_updates() {
        assert_eq!(store_action(&event(events::ROUTER_FEE_RECEIVER_UPDATED)), StoreAction::Ignore);
    }
}
