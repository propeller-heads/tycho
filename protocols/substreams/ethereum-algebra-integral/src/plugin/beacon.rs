//! Which implementation a plugin runs.
//!
//! A plugin is an `AlgebraPluginProxy`, an OpenZeppelin beacon proxy: it logs the beacon it is
//! bound to when it is constructed, and runs whatever implementation that beacon points at. Both
//! facts are in the block. The proxy's constructor calls `beacon.implementation()`, so the
//! creation transaction's trace holds the implementation of that moment, and the beacon logs
//! `Upgraded` whenever it is re-pointed. No RPC call is needed.

use substreams_ethereum::pb::eth::v2::{Call, TransactionTrace};

use crate::abi::beacon::{
    events::{BeaconUpgraded, Upgraded},
    functions::Implementation,
};

/// The beacon a plugin is bound to and the implementation that beacon pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginBeacon {
    pub beacon: Vec<u8>,
    pub implementation: Vec<u8>,
}

/// A beacon re-pointed to a new implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeaconUpgrade {
    pub beacon: Vec<u8>,
    pub implementation: Vec<u8>,
    /// Ordinal of the `Upgraded` log.
    pub ordinal: u64,
}

fn live_calls(trx: &TransactionTrace) -> impl Iterator<Item = &Call> {
    trx.calls
        .iter()
        .filter(|call| !call.state_reverted)
}

/// The beacon `plugin` was bound to in `trx`, with the implementation the plugin read from it
/// there.
///
/// `None` when `plugin` was not constructed as a beacon proxy in `trx`: it logged no
/// `BeaconUpgraded`, or never read its beacon's implementation.
pub fn plugin_beacon(trx: &TransactionTrace, plugin: &[u8]) -> Option<PluginBeacon> {
    let mut beacon = None;
    for call in live_calls(trx) {
        for log in &call.logs {
            if log.address == plugin && BeaconUpgraded::match_log(log) {
                // The proxy binds once, in its constructor; if it ever logged twice the last
                // beacon is the one it runs.
                beacon = BeaconUpgraded::decode(log)
                    .ok()
                    .map(|event| event.beacon);
            }
        }
    }
    let beacon = beacon?;

    let mut implementation = None;
    for call in live_calls(trx) {
        let reads_beacon = call.caller == plugin &&
            call.address == beacon &&
            !call.status_failed &&
            Implementation::match_call(call);
        if reads_beacon {
            if let Ok(address) = Implementation::output_call(call) {
                implementation = Some(address);
            }
        }
    }
    Some(PluginBeacon { beacon, implementation: implementation? })
}

/// Every `Upgraded` log of `trx`, in log order.
///
/// Proxies of every kind log this event, so the caller keeps only the upgrades of beacons it
/// tracks.
pub fn beacon_upgrades(trx: &TransactionTrace) -> Vec<BeaconUpgrade> {
    let mut upgrades = Vec::new();
    for call in live_calls(trx) {
        for log in &call.logs {
            if !Upgraded::match_log(log) {
                continue;
            }
            let Ok(event) = Upgraded::decode(log) else {
                continue;
            };
            upgrades.push(BeaconUpgrade {
                beacon: log.address.clone(),
                implementation: event.implementation,
                ordinal: log.ordinal,
            });
        }
    }
    upgrades.sort_by_key(|upgrade| upgrade.ordinal);
    upgrades
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use substreams_ethereum::pb::eth::v2::Log;

    use super::*;

    // Ethereum transaction 0xa7571666e8df2f53e75dac29aaccff54e1c0e9879ff42f0dd5eb60991944d981,
    // which created pool 0xa30dab616372d6f46b3d619d6b7c9bf050c98660 and its plugin.
    const PLUGIN: [u8; 20] = hex!("2f643a734d8e090b82762bd982f1428ea482752e");
    const BEACON: [u8; 20] = hex!("44ce18e6e618a35f05ddd384571fc32b3fa0b879");
    const IMPLEMENTATION: [u8; 20] = hex!("6580d2f0bab746ae2e655f92e2207f7eb0a13788");
    // A second beacon proxy constructed in the same transaction.
    const OTHER_PROXY: [u8; 20] = hex!("1bb06aedc78a4b4854a2466034af0a82dfb09b9f");
    const OTHER_BEACON: [u8; 20] = hex!("d9ccfb7842daa3f072537e136cb48025cba9d3c8");
    const OTHER_IMPLEMENTATION: [u8; 20] = hex!("83ba9a304d8412932472978b9659f4df251b10f2");

    const BEACON_UPGRADED_TOPIC: [u8; 32] =
        hex!("1cf3b03a6cf19fa2baba4df148e9dcabedea7f8a5c07840e207e5c089be95d3e");
    const UPGRADED_TOPIC: [u8; 32] =
        hex!("bc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b");
    const IMPLEMENTATION_SELECTOR: [u8; 4] = hex!("5c60da1b");

    fn word(address: &[u8; 20]) -> Vec<u8> {
        let mut word = vec![0u8; 12];
        word.extend_from_slice(address);
        word
    }

    fn address_log(emitter: &[u8; 20], topic: [u8; 32], address: &[u8; 20], ordinal: u64) -> Log {
        Log {
            address: emitter.to_vec(),
            topics: vec![topic.to_vec(), word(address)],
            ordinal,
            ..Default::default()
        }
    }

    fn implementation_read(proxy: &[u8; 20], beacon: &[u8; 20], returned: &[u8; 20]) -> Call {
        Call {
            caller: proxy.to_vec(),
            address: beacon.to_vec(),
            input: IMPLEMENTATION_SELECTOR.to_vec(),
            return_data: word(returned),
            ..Default::default()
        }
    }

    fn constructor(proxy: &[u8; 20], beacon: &[u8; 20], ordinal: u64) -> Call {
        Call {
            address: proxy.to_vec(),
            logs: vec![address_log(proxy, BEACON_UPGRADED_TOPIC, beacon, ordinal)],
            ..Default::default()
        }
    }

    fn creation_trx() -> TransactionTrace {
        TransactionTrace {
            calls: vec![
                constructor(&PLUGIN, &BEACON, 10),
                implementation_read(&PLUGIN, &BEACON, &IMPLEMENTATION),
                constructor(&OTHER_PROXY, &OTHER_BEACON, 20),
                implementation_read(&OTHER_PROXY, &OTHER_BEACON, &OTHER_IMPLEMENTATION),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn plugin_beacon_reads_the_plugins_own_beacon_and_implementation() {
        let found = plugin_beacon(&creation_trx(), &PLUGIN);

        assert_eq!(
            found,
            Some(PluginBeacon { beacon: BEACON.to_vec(), implementation: IMPLEMENTATION.to_vec() })
        );
    }

    #[test]
    fn plugin_beacon_keeps_proxies_of_one_transaction_apart() {
        let found = plugin_beacon(&creation_trx(), &OTHER_PROXY);

        assert_eq!(
            found,
            Some(PluginBeacon {
                beacon: OTHER_BEACON.to_vec(),
                implementation: OTHER_IMPLEMENTATION.to_vec()
            })
        );
    }

    #[test]
    fn address_that_is_not_a_beacon_proxy_has_no_beacon() {
        assert_eq!(plugin_beacon(&creation_trx(), &BEACON), None);
    }

    #[test]
    fn reverted_constructor_is_ignored() {
        let mut trx = creation_trx();
        trx.calls[0].state_reverted = true;

        assert_eq!(plugin_beacon(&trx, &PLUGIN), None);
    }

    #[test]
    fn failed_or_missing_implementation_read_yields_nothing() {
        let mut failed = creation_trx();
        failed.calls[1].status_failed = true;
        let mut missing = creation_trx();
        missing.calls.remove(1);
        let mut malformed = creation_trx();
        malformed.calls[1].return_data = vec![1, 2, 3];

        assert_eq!(plugin_beacon(&failed, &PLUGIN), None);
        assert_eq!(plugin_beacon(&missing, &PLUGIN), None);
        assert_eq!(plugin_beacon(&malformed, &PLUGIN), None);
    }

    #[test]
    fn implementation_read_by_another_caller_is_not_the_plugins() {
        let mut trx = creation_trx();
        trx.calls[1] = implementation_read(&OTHER_PROXY, &BEACON, &OTHER_IMPLEMENTATION);

        assert_eq!(plugin_beacon(&trx, &PLUGIN), None);
    }

    #[test]
    fn beacon_upgrades_lists_every_upgrade_in_log_order() {
        let trx = TransactionTrace {
            calls: vec![
                Call {
                    logs: vec![address_log(
                        &OTHER_BEACON,
                        UPGRADED_TOPIC,
                        &OTHER_IMPLEMENTATION,
                        9,
                    )],
                    ..Default::default()
                },
                Call {
                    logs: vec![
                        address_log(&BEACON, UPGRADED_TOPIC, &IMPLEMENTATION, 4),
                        // Not an upgrade.
                        address_log(&PLUGIN, BEACON_UPGRADED_TOPIC, &BEACON, 5),
                    ],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert_eq!(
            beacon_upgrades(&trx),
            [
                BeaconUpgrade {
                    beacon: BEACON.to_vec(),
                    implementation: IMPLEMENTATION.to_vec(),
                    ordinal: 4
                },
                BeaconUpgrade {
                    beacon: OTHER_BEACON.to_vec(),
                    implementation: OTHER_IMPLEMENTATION.to_vec(),
                    ordinal: 9
                },
            ]
        );
    }

    #[test]
    fn reverted_upgrade_is_ignored() {
        let trx = TransactionTrace {
            calls: vec![Call {
                state_reverted: true,
                logs: vec![address_log(&BEACON, UPGRADED_TOPIC, &IMPLEMENTATION, 4)],
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(beacon_upgrades(&trx).is_empty());
    }
}
