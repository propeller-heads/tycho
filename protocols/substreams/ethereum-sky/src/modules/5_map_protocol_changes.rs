use std::collections::{hash_map::Entry, HashMap};

use alloy_primitives::{keccak256, Address, FixedBytes, B256};
use anyhow::Result;
use itertools::Itertools;
use substreams::{pb::substreams::StoreDeltas, prelude::*};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

use crate::{
    abi::{
        dss_lite_psm::{
            events::File,
            functions::{Tin, Tout},
        },
        vat::functions::Dai as VatDai,
    },
    config::Config,
    modules::store_components::component_created,
};

const TIN: &str = "tin";
const TOUT: &str = "tout";
/// Wrapper attributes carrying the join escrows (`vat.dai[join]`, wad) its in-flight
/// DAI <-> USDS conversion burns through.
const DAI_ESCROW: &str = "dai_escrow";
const USDS_ESCROW: &str = "usds_escrow";

/// Storage slot index of the Vat's `dai` mapping (its sixth declared field; the Vat
/// is non-upgradeable, so the layout is frozen). Pinned against an on-chain
/// `eth_getStorageAt`-vs-`dai()` comparison in the tests below.
const VAT_DAI_SLOT: u8 = 5;

/// DssLitePsm encodes its `what` values as right-padded ASCII bytes32.
fn what(name: &str) -> [u8; 32] {
    FixedBytes::right_padding_from(name.as_bytes()).0
}

/// Assembles the final `BlockChanges`: components with their initial fee attributes,
/// `tin`/`tout` updates from LitePSM `File` events (mirrored onto the stateless
/// wrapper), and absolute balances (PSM balances additionally mirrored onto the
/// wrapper, with DAI relabelled as its USDS-deliverable inventory; the join escrows
/// read directly from Vat storage writes, as the converter's balances and the
/// wrapper's escrow attributes).
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    protocol_components: BlockTransactionProtocolComponents,
    components_store: StoreGetInt64,
    balance_store: StoreDeltas,
    deltas: BlockBalanceDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let config = Config::parse(&params)?;
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    let psm_created = component_created(&components_store, &config.psm_component_id());
    let wrapper_created = component_created(&components_store, &config.wrapper_component_id());
    let converter_created = component_created(&components_store, &config.converter_component_id());

    // The wrapper's creation-time state (fees and balances) is seeded from post-block
    // eth_call snapshots, so mirroring PSM state onto it starts strictly after its
    // creation block — everything within that block is already inside the seeds.
    let wrapper_mirrors = wrapper_created && block.number > config.wrapper_creation_block;

    for tx_component in protocol_components.tx_components {
        let tx = tx_component
            .tx
            .expect("transaction of component creation must be set");
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));

        for component in tx_component.components {
            builder.add_protocol_component(&component);
            // The PSM's creation anchor is its deployment, so tin/tout are
            // zero-initialized storage by definition (a File later in the same block
            // would still be emitted as an update). The wrapper is created after the
            // PSM, so its mirrored fees are seeded like the balances: a deterministic
            // eth_call snapshot of the PSM's post-block fees at its creation block.
            if component.id == config.psm_component_id() {
                builder.add_entity_change(&EntityChanges {
                    component_id: component.id,
                    attributes: vec![
                        fee_attribute(TIN, vec![0u8; 32]),
                        fee_attribute(TOUT, vec![0u8; 32]),
                    ],
                });
            } else if component.id == config.wrapper_component_id() {
                let (tin, tout) = psm_fees(&config.psm);
                builder.add_entity_change(&EntityChanges {
                    component_id: component.id,
                    attributes: vec![
                        fee_attribute(TIN, tin),
                        fee_attribute(TOUT, tout),
                        escrow_attribute(DAI_ESCROW, vat_escrow(&config.vat, &config.dai_join)),
                        escrow_attribute(USDS_ESCROW, vat_escrow(&config.vat, &config.usds_join)),
                    ],
                });
            } else if component.id == config.converter_component_id() {
                for (token, join) in
                    [(&config.dai, &config.dai_join), (&config.usds, &config.usds_join)]
                {
                    builder.add_balance_change(&BalanceChange {
                        token: token.to_vec(),
                        balance: vat_escrow(&config.vat, join)
                            .to_bytes_be()
                            .1,
                        component_id: component.id.clone().into_bytes(),
                    });
                }
            }
        }
    }

    handle_fee_updates(&config, &block, psm_created, wrapper_mirrors, &mut transaction_changes);
    let converter_tracks = converter_created && block.number > config.converter_creation_block;
    if converter_tracks || wrapper_mirrors {
        handle_escrow_updates(
            &config,
            &block,
            converter_tracks,
            wrapper_mirrors,
            &mut transaction_changes,
        );
    }

    for (_, (tx, balances)) in aggregate_balances_changes(balance_store, deltas) {
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for token_bc_map in balances.into_values() {
            for bc in token_bc_map.into_values() {
                builder.add_balance_change(&bc);

                // Mirror the PSM inventory onto the wrapper: same USDC buy-side, and
                // the DAI sell-side relabelled as USDS (converted 1:1 in-flight).
                if bc.component_id == config.psm_component_id().into_bytes() && wrapper_mirrors {
                    let token = if bc.token == config.dai.as_slice() {
                        config.usds.to_vec()
                    } else {
                        bc.token
                    };
                    builder.add_balance_change(&BalanceChange {
                        token,
                        balance: bc.balance,
                        component_id: config
                            .wrapper_component_id()
                            .into_bytes(),
                    });
                }
            }
        }
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        storage_changes: vec![],
    })
}

/// Emits `tin`/`tout` attribute updates from `File(bytes32,uint256)` events on the PSM,
/// mirrored onto the wrapper once it exists.
fn handle_fee_updates(
    config: &Config,
    block: &eth::v2::Block,
    psm_created: bool,
    wrapper_mirrors: bool,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for log_view in block.logs() {
        let log = log_view.log;
        if log.address != config.psm.as_slice() {
            continue;
        }
        let Some(file) = File::match_and_decode(log) else {
            continue;
        };
        let name = if file.what == what(TIN) {
            TIN
        } else if file.what == what(TOUT) {
            TOUT
        } else {
            // Other filed parameters (e.g. `buf`) do not affect pricing: swap
            // capacity is tracked through the PSM's actual DAI balance.
            continue;
        };

        let tx = log_view.receipt.transaction;
        let builder = transaction_changes
            .entry(tx.index.into())
            .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));
        if psm_created {
            builder.add_entity_change(&EntityChanges {
                component_id: config.psm_component_id(),
                attributes: vec![fee_attribute(name, log.data.clone())],
            });
        }
        if wrapper_mirrors {
            builder.add_entity_change(&EntityChanges {
                component_id: config.wrapper_component_id(),
                attributes: vec![fee_attribute(name, log.data.clone())],
            });
        }
    }
}

/// Emits the join escrows from Vat storage writes to the joins' `dai` slots: as the
/// converter's absolute balances and as the wrapper's `dai_escrow`/`usds_escrow`
/// attributes (each gated on its component tracking updates in this block). Each
/// write already carries the post-change value, so no delta aggregation is needed,
/// and every Vat channel touching the escrow (`join`/`exit`, but also
/// `move`/`frob`/`suck`/`fold` donations) is captured. Per transaction only the
/// write with the highest ordinal counts: the calls are in trace order, in which a
/// parent's post-child write precedes the child's.
fn handle_escrow_updates(
    config: &Config,
    block: &eth::v2::Block,
    converter_tracks: bool,
    wrapper_tracks: bool,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    let slots = escrow_slots(config);
    for trx in block.transactions() {
        let mut latest: HashMap<&Address, (u64, BigInt)> = HashMap::new();
        for call in trx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                let Some((token, balance)) = escrow_balance(&config.vat, &slots, change) else {
                    continue;
                };
                match latest.entry(token) {
                    Entry::Vacant(entry) => {
                        entry.insert((change.ordinal, balance));
                    }
                    Entry::Occupied(mut entry) => {
                        if change.ordinal >= entry.get().0 {
                            entry.insert((change.ordinal, balance));
                        }
                    }
                }
            }
        }
        if latest.is_empty() {
            continue;
        }
        let builder = transaction_changes
            .entry(trx.index.into())
            .or_insert_with(|| TransactionChangesBuilder::new(&(trx.into())));
        for (token, (_, balance)) in latest {
            if converter_tracks {
                builder.add_balance_change(&BalanceChange {
                    token: token.to_vec(),
                    balance: balance.to_bytes_be().1,
                    component_id: config
                        .converter_component_id()
                        .into_bytes(),
                });
            }
            if wrapper_tracks {
                let name = if *token == config.dai { DAI_ESCROW } else { USDS_ESCROW };
                builder.add_entity_change(&EntityChanges {
                    component_id: config.wrapper_component_id(),
                    attributes: vec![escrow_attribute(name, balance)],
                });
            }
        }
    }
}

/// The Vat storage slots holding the joins' escrows, paired with the converter-side
/// token each escrow bounds.
fn escrow_slots(config: &Config) -> [(B256, &Address); 2] {
    [(escrow_slot(&config.dai_join), &config.dai), (escrow_slot(&config.usds_join), &config.usds)]
}

/// Storage slot of `vat.dai[join]`: `keccak256(pad32(join) ++ pad32(VAT_DAI_SLOT))`.
fn escrow_slot(join: &Address) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(join.as_slice());
    buf[63] = VAT_DAI_SLOT;
    keccak256(buf)
}

/// Maps a Vat storage write on one of the joins' `dai` slots to that escrow's new
/// absolute value (rad), converted to wad.
fn escrow_balance<'a>(
    vat: &Address,
    slots: &[(B256, &'a Address); 2],
    change: &eth::v2::StorageChange,
) -> Option<(&'a Address, BigInt)> {
    if change.address != vat.as_slice() {
        return None;
    }
    let (_, token) = slots
        .iter()
        .find(|(slot, _)| change.key == slot.as_slice())?;
    let rad = BigInt::from_unsigned_bytes_be(&change.new_value);
    Some((token, rad / BigInt::from(10).pow(27)))
}

fn fee_attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_owned(), value, change: ChangeType::Update.into() }
}

/// Encodes an escrow value (wad) as an unsigned big-endian attribute, matching the
/// balance encoding the simulation decodes with `U256::from_be_slice`.
fn escrow_attribute(name: &str, escrow: BigInt) -> Attribute {
    fee_attribute(name, escrow.to_bytes_be().1)
}

/// Post-block snapshot of the PSM's `tin`/`tout` via deterministic eth_calls,
/// encoded as attribute values. A failed call means a provider issue: fail the
/// module loudly rather than seed a wrong fee.
fn psm_fees(psm: &Address) -> (Vec<u8>, Vec<u8>) {
    let tin = Tin {}
        .call(psm.to_vec())
        .unwrap_or_else(|| panic!("tin eth_call failed for psm {psm}"));
    let tout = Tout {}
        .call(psm.to_vec())
        .unwrap_or_else(|| panic!("tout eth_call failed for psm {psm}"));
    (tin.to_bytes_be().1, tout.to_bytes_be().1)
}

/// Post-block snapshot of a join's internal dai escrow (`vat.dai[join]`, rad) via a
/// deterministic eth_call, converted to wad. A failed call means a provider issue:
/// fail the module loudly rather than seed a wrong balance.
fn vat_escrow(vat: &Address, join: &Address) -> BigInt {
    let rad = VatDai { param0: join.to_vec() }
        .call(vat.to_vec())
        .unwrap_or_else(|| panic!("vat.dai eth_call failed for join {join}"));
    rad / BigInt::from(10).pow(27)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use substreams::hex;

    use super::*;

    /// Pins the abigen-derived event matching to the topic verified against the
    /// deployed DssLitePsm bytecode, guarding against typos in the ABI json.
    #[test]
    fn file_event_matches_onchain_topic() {
        let log = eth::v2::Log {
            topics: vec![
                hex!("e986e40cc8c151830d4f61050f4fb2e4add8567caad2d5f5496f9158e91fe4c7").to_vec(),
                what(TIN).to_vec(),
            ],
            data: [0u8; 32].to_vec(),
            ..Default::default()
        };
        let file = File::match_and_decode(&log).expect("File event must match");
        assert_eq!(file.what, what(TIN));
        assert_eq!(file.data, BigInt::zero());
        assert_eq!(&what(TIN)[..3], b"tin");
        assert!(what(TIN)[3..].iter().all(|b| *b == 0));
        assert_eq!(&what(TOUT)[..4], b"tout");
    }

    /// Pins the slot derivation against mainnet: `eth_getStorageAt(vat, this slot)`
    /// returns the same value as `vat.dai(DaiJoin)`, verifying both `VAT_DAI_SLOT`
    /// and the key encoding.
    #[test]
    fn escrow_slot_matches_onchain_storage_layout() {
        let dai_join = address!("9759a6ac90977b93b58547b4a71c78317f391a28");
        assert_eq!(
            escrow_slot(&dai_join),
            b256!("7adce341587c7ad28f80a9ef54da0d2f0bfd917b11856be2d246a8c66685b1a0")
        );
    }

    #[test]
    fn escrow_balance_maps_slots_to_tokens_and_converts_rad_to_wad() {
        let vat = address!("35d1b3f3d7966a1dfe207aa4514c12a259a0492b");
        let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
        let usds = address!("dc035d45d973e3ec169d2276ddab16f1e407384f");
        let dai_join = address!("9759a6ac90977b93b58547b4a71c78317f391a28");
        let usds_join = address!("3c0f895007ca717aa01c8693e59df1e8c3777feb");
        let slots = [(escrow_slot(&dai_join), &dai), (escrow_slot(&usds_join), &usds)];

        let rad = BigInt::from(7) * BigInt::from(10).pow(27) + BigInt::from(1);
        let change = eth::v2::StorageChange {
            address: vat.to_vec(),
            key: escrow_slot(&usds_join).to_vec(),
            new_value: rad.to_bytes_be().1,
            ..Default::default()
        };
        let (token, balance) = escrow_balance(&vat, &slots, &change).unwrap();
        assert_eq!(*token, usds);
        assert_eq!(balance, BigInt::from(7));

        let elsewhere = eth::v2::StorageChange { address: dai_join.to_vec(), ..change.clone() };
        assert!(escrow_balance(&vat, &slots, &elsewhere).is_none());
        let other_slot = eth::v2::StorageChange { key: [0xab; 32].to_vec(), ..change.clone() };
        assert!(escrow_balance(&vat, &slots, &other_slot).is_none());
    }
}
