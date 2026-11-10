//! Encode/decode round trips over every `PoolEvent` variant.
//!
//! `src/pb` is generated, but the oneof's tag list once fell out of step with the proto, and prost
//! then decodes the unlisted variants to `None`. A sample of every variant keeps that from
//! recurring silently.

use prost::Message;
use tycho_substreams::prelude::Transaction;

use crate::pb::nami::v4::events::{
    pool_event::{self, Typ},
    PoolEvent,
};

/// Names a variant. The match is exhaustive, so a variant added to the proto has to be added to
/// `sample_typs` as well.
fn variant_name(typ: &Typ) -> &'static str {
    match typ {
        Typ::Initialize(_) => "Initialize",
        Typ::Mint(_) => "Mint",
        Typ::Collect(_) => "Collect",
        Typ::Burn(_) => "Burn",
        Typ::Swap(_) => "Swap",
        Typ::Flash(_) => "Flash",
        Typ::Skim(_) => "Skim",
        Typ::Fee(_) => "Fee",
        Typ::Plugin(_) => "Plugin",
        Typ::PluginConfig(_) => "PluginConfig",
        Typ::MevxRouter(_) => "MevxRouter",
        Typ::MinGasLeft(_) => "MinGasLeft",
        Typ::CallGasBudget(_) => "CallGasBudget",
        Typ::Incentive(_) => "Incentive",
    }
}

/// One sample per variant, every field non-empty so a dropped field shows up too.
fn sample_typs() -> Vec<Typ> {
    vec![
        Typ::Initialize(pool_event::Initialize { sqrt_price: vec![1, 2, 3], tick: -7 }),
        Typ::Mint(pool_event::Mint {
            tick_lower: -60,
            tick_upper: 60,
            amount: vec![4],
            amount_0: vec![5],
            amount_1: vec![6],
        }),
        Typ::Collect(pool_event::Collect { amount_0: vec![7], amount_1: vec![8] }),
        Typ::Burn(pool_event::Burn { tick_lower: -120, tick_upper: 120, amount: vec![9] }),
        Typ::Swap(pool_event::Swap {
            amount_0: vec![10],
            amount_1: vec![11],
            sqrt_price: vec![12],
            liquidity: vec![13],
            tick: 14,
        }),
        Typ::Flash(pool_event::Flash { paid_0: vec![15], paid_1: vec![16] }),
        Typ::Skim(pool_event::Skim { amount_0: vec![17], amount_1: vec![18] }),
        Typ::Fee(pool_event::Fee { fee: 500 }),
        Typ::Plugin(pool_event::Plugin { plugin: vec![0xAA; 20] }),
        Typ::PluginConfig(pool_event::PluginConfig { plugin_config: 195 }),
        Typ::MevxRouter(pool_event::MevxRouter { router: vec![0xBB; 20] }),
        Typ::MinGasLeft(pool_event::MinGasLeft { min_gas_left: vec![0x0F, 0x42, 0x40] }),
        Typ::CallGasBudget(pool_event::CallGasBudget { call_gas_budget: vec![0x4C, 0x4B, 0x40] }),
        Typ::Incentive(pool_event::Incentive { incentive: vec![0xCC; 20] }),
    ]
}

fn sample_event(typ: Typ) -> PoolEvent {
    PoolEvent {
        log_ordinal: 42,
        pool_address: vec![0x01; 20],
        transaction: Some(Transaction {
            hash: vec![0x04; 32],
            from: vec![0x05; 20],
            to: vec![0x06; 20],
            index: 3,
        }),
        typ: Some(typ),
    }
}

#[test]
fn test_pool_event_round_trips_every_variant() {
    for typ in sample_typs() {
        let name = variant_name(&typ);
        let event = sample_event(typ);
        let decoded = PoolEvent::decode(event.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, event, "{name} does not survive an encode/decode round trip");
    }
}
