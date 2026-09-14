//! The market fee, which is configuration rather than market state.
//!
//! `lnFeeRateRoot` and `reserveFeePercent` are not immutable, despite the brief listing them as
//! static: `PendleMarketV7.readState()` reads both from the factory, and the override is keyed by
//! the **calling router**, so a market's effective fee is only well-defined relative to one. That
//! router is `ROUTER_V4` here.
//!
//! The two factory generations disagree on both the event shape and what is keyed by what:
//!
//! | | original `0x27b1dAcd…` | V3 and later |
//! |---|---|---|
//! | base config | `NewMarketConfig(treasury, lnFeeRateRoot, reserveFeePercent)` — factory-wide | `lnFeeRateRoot` per market, from `CreateNewMarket` |
//! | reserve fee | same event | `NewTreasuryAndFeeReserve(treasury, reserveFeePercent)` — factory-wide |
//! | override | `SetOverriddenFee(router, lnFeeRateRoot, reserveFeePercent)`, `UnsetOverriddenFee(router)` | `SetOverriddenFee(router, market, lnFeeRateRoot)` |
//! | resolution | the config *is* the value | `overriddenFee != 0 ? overriddenFee : creation value` |
//!
//! Both halves were confirmed on mainnet rather than read off the sources: the original factory's
//! `getMarketConfig(router)` returns `499875041000000 / 80` for Router V4, matching
//! `readState(router)` on the wstETH market exactly; factory V6's
//! `getMarketConfig(market, router)` returns `overriddenFee = 0` for market
//! `0xe10afe8c…bde3`, whose effective `lnFeeRateRoot` equals its `CreateNewMarket` value
//! `3992021269537452`.

use substreams::{log, scalar::BigInt};
use substreams_ethereum::{pb::eth::v2 as eth, rpc::RpcBatch, Event};

use crate::{
    abi::{pendle_market_factory, pendle_market_factory_v1},
    consts::{MARKET_FACTORIES_V3_PLUS, MARKET_FACTORY_V1, ROUTER_V4},
    registry::MarketEntry,
};

/// Attribute holding the fee rate the market's rate-space fee is derived from.
pub const LN_FEE_RATE_ROOT: &str = "ln_fee_rate_root";
/// Attribute holding the share of the fee that leaves the market for the treasury.
pub const RESERVE_FEE_PERCENT: &str = "reserve_fee_percent";

/// Which markets a factory's fee event moves.
///
/// Only the scope, not the new values. Reproducing each generation's resolution rules in Rust —
/// override versus base, zero-means-none, which of the two `getMarketConfig` overloads applies —
/// is exactly the kind of re-derivation that goes quietly wrong. The event says *when* and *which*;
/// the value itself is then read back from the factory, which is the same source
/// `PendleMarket.readState()` uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeeScope {
    /// Every market this factory deployed. The original factory's config is factory-wide, and
    /// V3+'s reserve fee is too.
    Factory(Vec<u8>),
    /// One market, from V3+'s `(router, market)`-keyed override.
    Market(Vec<u8>),
}

/// Decodes a factory log into the set of markets whose fee it changes, or `None` if it is not a
/// fee event.
///
/// Overrides for any router other than Router V4 are dropped here: applying one would change a
/// quote for a swap that will not be routed through it.
pub fn fee_scope(log: &eth::Log) -> Option<FeeScope> {
    let address: [u8; 20] = log.address.as_slice().try_into().ok()?;

    if address == MARKET_FACTORY_V1 {
        if pendle_market_factory_v1::events::NewMarketConfig::match_log(log) {
            return Some(FeeScope::Factory(log.address.clone()));
        }
        if let Some(event) =
            pendle_market_factory_v1::events::SetOverriddenFee::match_and_decode(log)
        {
            if event.router != ROUTER_V4 {
                return None;
            }
            return Some(FeeScope::Factory(log.address.clone()));
        }
        if let Some(event) =
            pendle_market_factory_v1::events::UnsetOverriddenFee::match_and_decode(log)
        {
            if event.router != ROUTER_V4 {
                return None;
            }
            return Some(FeeScope::Factory(log.address.clone()));
        }
        return None;
    }

    if MARKET_FACTORIES_V3_PLUS.contains(&address) {
        if pendle_market_factory::events::NewTreasuryAndFeeReserve::match_log(log) {
            return Some(FeeScope::Factory(log.address.clone()));
        }
        if let Some(event) = pendle_market_factory::events::SetOverriddenFee::match_and_decode(log)
        {
            if event.router != ROUTER_V4 {
                return None;
            }
            return Some(FeeScope::Market(event.market));
        }
    }
    None
}

/// Resolves a V3+ market's effective fee rate from its override and its creation-time value.
///
/// A zero override means no override — that is how the factory clears one, and how every market
/// that has never had one reads back.
pub fn effective_ln_fee_rate_root(overridden: &BigInt, at_creation: &BigInt) -> BigInt {
    if *overridden == BigInt::zero() {
        at_creation.clone()
    } else {
        overridden.clone()
    }
}

/// Selects the markets a fee event applies to.
///
/// The asymmetry is the point: the original factory's config is one setting shared by every
/// market it deployed, so one event moves all of them, while V3+ overrides one market at a time.
/// Treating the first like the second would leave every v1 market but one on a stale fee.
pub fn fan_out<'a>(scope: &FeeScope, markets: &'a [MarketEntry]) -> Vec<&'a MarketEntry> {
    match scope {
        FeeScope::Factory(factory) => markets
            .iter()
            .filter(|market| market.is_from(factory))
            .collect(),
        FeeScope::Market(address) => {
            let id = format!("0x{}", hex::encode(address));
            markets
                .iter()
                .filter(|market| market.id == id)
                .collect()
        }
    }
}

/// The fee a market charges a swap routed through Router V4.
pub struct MarketFee {
    pub ln_fee_rate_root: BigInt,
    pub reserve_fee_percent: BigInt,
}

/// Reads the current fee for each market straight from its factory, in one batch.
///
/// The two generations take different `getMarketConfig` overloads and return different second
/// fields — v1's *is* the rate, V3+'s is an override that falls back to the creation value — so
/// the call and the resolution are both chosen per market from the factory that deployed it.
///
/// A market whose factory does not answer is skipped rather than defaulted: a wrong fee is a
/// wrong quote, and the previously indexed value is the better stale answer.
pub fn read_market_fees(markets: &[MarketEntry]) -> Vec<(String, MarketFee)> {
    let mut batch = RpcBatch::new();
    for market in markets {
        let factory = hex::decode(&market.factory).expect("registry holds a non-hex factory");
        batch = if market.is_from(&MARKET_FACTORY_V1) {
            batch.add(
                pendle_market_factory_v1::functions::GetMarketConfig { router: ROUTER_V4.to_vec() },
                factory,
            )
        } else {
            let address = hex::decode(market.id.trim_start_matches("0x"))
                .expect("registry holds a non-hex market id");
            batch.add(
                pendle_market_factory::functions::GetMarketConfig {
                    market: address,
                    router: ROUTER_V4.to_vec(),
                },
                factory,
            )
        };
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let mut fees = Vec::new();
    for (market, response) in markets.iter().zip(responses.iter()) {
        let decoded = if market.is_from(&MARKET_FACTORY_V1) {
            RpcBatch::decode::<_, pendle_market_factory_v1::functions::GetMarketConfig>(response)
                .map(|(_, rate, reserve)| (rate, reserve))
        } else {
            RpcBatch::decode::<_, pendle_market_factory::functions::GetMarketConfig>(response).map(
                |(_, overridden, reserve)| {
                    let at_creation = market
                        .ln_fee_rate_root_at_creation
                        .parse::<BigInt>()
                        .expect("registry holds a non-numeric creation fee rate");
                    (effective_ln_fee_rate_root(&overridden, &at_creation), reserve)
                },
            )
        };
        let Some((ln_fee_rate_root, reserve_fee_percent)) = decoded else {
            log::info!("market {} could not resolve its fee, leaving it stale", market.id);
            continue;
        };
        fees.push((market.id.clone(), MarketFee { ln_fee_rate_root, reserve_fee_percent }));
    }
    fees
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACTORY_V1: &str = "27b1dacd74688af24a64bd3c9c1b143118740784";
    const FACTORY_V6: &str = "6d247b1c044fa1e22e6b04fa9f71baf99eb29a9f";
    const ROUTER_V4_TOPIC: &str =
        "000000000000000000000000888888888889758f76e7103c6cbf23abbf58f946";
    const OTHER_ROUTER_TOPIC: &str =
        "000000000000000000000000000000000000000000000000000000000000dead";

    /// Topic0s taken from chain, not from the bindings under test: deriving the expected topic
    /// from the same ABI the decoder uses would assert nothing. `NEW_MARKET_CONFIG` is the topic
    /// of the real log at block 19419012 — the one that halved the original factory's fee — and
    /// the rest are `keccak` of the signatures on the verified sources.
    const NEW_MARKET_CONFIG: &str =
        "e7b39c33a288abde49b8d831b2125b22dbb151766d6aa52b1511b340923d4587";
    const V1_SET_OVERRIDDEN_FEE: &str =
        "a19e7426d30754f50d54a9c1c82b400fc1b4516ab9e91e20269634fb8b422752";
    const V1_UNSET_OVERRIDDEN_FEE: &str =
        "de6e91eb95830256384b447e0ee564556c85ade807db58dc6e1d18688418aacd";
    const V3_SET_OVERRIDDEN_FEE: &str =
        "ea7fdf3abb8ced24e7f9c441f3e98071fb5ea1f9278e2b9202c4a6d306cce59f";
    const NEW_TREASURY_AND_FEE_RESERVE: &str =
        "c612910a1561af820dd8961721344b949df6bfcb3cd8dda1f87a5f25e80852cb";
    const TREASURY_TOPIC: &str = "0000000000000000000000008270400d528c34e1596ef367eedec99080a1b592";

    fn log(address: &str, topics: Vec<String>, data: &str) -> eth::Log {
        eth::Log {
            address: hex::decode(address).unwrap(),
            topics: topics
                .iter()
                .map(|t| hex::decode(t).unwrap())
                .collect(),
            data: hex::decode(data).unwrap(),
            index: 0,
            block_index: 0,
            ordinal: 0,
        }
    }

    fn word(value: u128) -> String {
        format!("{value:064x}")
    }

    /// The measured v1 config: `lnFeeRateRoot` 499875041000000, `reserveFeePercent` 80 — the
    /// values `getMarketConfig(RouterV4)` returns on mainnet today.
    #[test]
    fn v1_new_market_config_applies_to_the_whole_factory() {
        let log = log(
            FACTORY_V1,
            vec![NEW_MARKET_CONFIG.to_string(), TREASURY_TOPIC.to_string()],
            &format!("{}{}", word(499875041000000), word(80)),
        );
        assert_eq!(fee_scope(&log), Some(FeeScope::Factory(hex::decode(FACTORY_V1).unwrap())));
    }

    /// The override is keyed by the calling router. One set for a different router must not move
    /// a quote that will be executed through Router V4.
    #[test]
    fn an_override_for_another_router_is_ignored() {
        let mine = log(
            FACTORY_V1,
            vec![V1_SET_OVERRIDDEN_FEE.to_string(), ROUTER_V4_TOPIC.to_string()],
            &format!("{}{}", word(123), word(80)),
        );
        let theirs = log(
            FACTORY_V1,
            vec![V1_SET_OVERRIDDEN_FEE.to_string(), OTHER_ROUTER_TOPIC.to_string()],
            &format!("{}{}", word(123), word(80)),
        );
        assert!(fee_scope(&mine).is_some());
        assert_eq!(fee_scope(&theirs), None);
    }

    #[test]
    fn v1_unset_reports_that_the_base_config_must_be_re_read() {
        let log = log(
            FACTORY_V1,
            vec![V1_UNSET_OVERRIDDEN_FEE.to_string(), ROUTER_V4_TOPIC.to_string()],
            "",
        );
        assert_eq!(fee_scope(&log), Some(FeeScope::Factory(hex::decode(FACTORY_V1).unwrap())));
    }

    /// V3+ keys the override by `(router, market)`, so it moves exactly one market.
    #[test]
    fn v3_plus_override_targets_one_market() {
        let market = "00000000000000000000000034280882267ffa6383b363e278b027be083bbe3b";
        let log = log(
            FACTORY_V6,
            vec![
                V3_SET_OVERRIDDEN_FEE.to_string(),
                ROUTER_V4_TOPIC.to_string(),
                market.to_string(),
            ],
            &word(3992021269537452),
        );
        assert_eq!(
            fee_scope(&log),
            Some(FeeScope::Market(
                hex::decode("34280882267ffa6383b363e278b027be083bbe3b").unwrap()
            ))
        );
    }

    /// V3+ moves the reserve fee factory-wide and leaves the per-market rate alone.
    #[test]
    fn v3_plus_treasury_event_moves_only_the_reserve_fee() {
        let log = log(
            FACTORY_V6,
            vec![NEW_TREASURY_AND_FEE_RESERVE.to_string(), TREASURY_TOPIC.to_string()],
            &word(80),
        );
        assert_eq!(fee_scope(&log), Some(FeeScope::Factory(hex::decode(FACTORY_V6).unwrap())));
    }

    /// The two generations' `SetOverriddenFee` have different shapes. Decoding a V3+ log with the
    /// v1 decoder would read the market address as the fee, so the factory address has to gate
    /// which decoder runs.
    #[test]
    fn a_log_from_an_unknown_address_is_not_a_fee_update() {
        let log = log(
            "1111111111111111111111111111111111111111",
            vec![NEW_MARKET_CONFIG.to_string(), TREASURY_TOPIC.to_string()],
            &format!("{}{}", word(499875041000000), word(80)),
        );
        assert_eq!(fee_scope(&log), None);
    }

    fn market(id: &str, factory: &str) -> MarketEntry {
        MarketEntry {
            id: id.to_string(),
            sy: "0xcbc72d92b2dc8187414f6734718563898740c0bc".to_string(),
            expiry: 1830124800,
            factory: factory.to_string(),
            ln_fee_rate_root_at_creation: "0".to_string(),
        }
    }

    /// The original factory's config is one setting shared by every market it deployed. The
    /// measured history has a `NewMarketConfig` at block 19419012 that halved the rate *after* the
    /// last v1 market was created — so a fan-out that missed markets would leave them at double
    /// the correct fee forever.
    #[test]
    fn a_v1_config_change_reaches_every_market_of_that_factory() {
        let markets = vec![
            market("0xaaa", FACTORY_V1),
            market("0xbbb", FACTORY_V6),
            market("0xccc", FACTORY_V1),
        ];
        let scope = FeeScope::Factory(hex::decode(FACTORY_V1).unwrap());
        let hit: Vec<&str> = fan_out(&scope, &markets)
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(hit, vec!["0xaaa", "0xccc"]);
    }

    /// A factory-wide change on one factory must not touch another's markets.
    #[test]
    fn a_factory_change_does_not_cross_generations() {
        let markets = vec![market("0xaaa", FACTORY_V1), market("0xbbb", FACTORY_V6)];
        let scope = FeeScope::Factory(hex::decode(FACTORY_V6).unwrap());
        let hit = fan_out(&scope, &markets);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].id, "0xbbb");
    }

    /// V3+ overrides one market at a time, even though several share the factory.
    #[test]
    fn a_v3_plus_override_reaches_exactly_one_market() {
        let markets = vec![
            market("0x34280882267ffa6383b363e278b027be083bbe3b", FACTORY_V6),
            market("0xbbb", FACTORY_V6),
        ];
        let scope =
            FeeScope::Market(hex::decode("34280882267ffa6383b363e278b027be083bbe3b").unwrap());
        let hit = fan_out(&scope, &markets);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].id, "0x34280882267ffa6383b363e278b027be083bbe3b");
    }

    /// An override naming a market we do not track resolves to nothing rather than to everything.
    #[test]
    fn an_override_for_an_unknown_market_hits_nothing() {
        let markets = vec![market("0xaaa", FACTORY_V6)];
        let scope =
            FeeScope::Market(hex::decode("dead00000000000000000000000000000000beef").unwrap());
        assert!(fan_out(&scope, &markets).is_empty());
    }

    /// Zero is how the factory says "no override", which is also how a market that never had one
    /// reads back — measured on factory V6 for market `0xe10afe8c…bde3`.
    #[test]
    fn a_zero_override_falls_back_to_the_creation_value() {
        let at_creation = BigInt::from(3992021269537452_i64);
        assert_eq!(effective_ln_fee_rate_root(&BigInt::zero(), &at_creation), at_creation);
        let overridden = BigInt::from(123_456_i64);
        assert_eq!(effective_ln_fee_rate_root(&overridden, &at_creation), overridden);
    }
}
