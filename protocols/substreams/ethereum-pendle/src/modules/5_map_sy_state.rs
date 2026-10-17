use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use substreams::{
    hex, log,
    scalar::BigInt,
    store::{StoreGet, StoreGetRaw, StoreGetString},
};
use substreams_ethereum::{pb::eth::v2 as eth, Event};
use tycho_substreams::prelude::*;

use crate::{
    abi::pendle_sy,
    consts::PENDLE_MARKET,
    keys::{contract_id, MARKET_REGISTRY},
    registry::markets_of_sy,
    sy::{
        class_attributes, classify_sy, is_class_attribute, is_uncapped, Classification,
        CREATION_PAUSED, CREATION_SUPPLY_CAP, CREATION_SY_PAUSED, PAUSED, SY_PAUSED, SY_SUPPLY_CAP,
        TOKEN_IN_CLASS_PREFIX, TOKEN_OUT_CLASS_PREFIX,
    },
    transfers::{is_market, new_sy_components},
};

/// `Paused(address)`, which OpenZeppelin's `Pausable` emits and every pausable SY inherits.
const PAUSED_TOPIC: [u8; 32] =
    hex!("62e78cea01bee320cd4e420270b5ea74000d11b0c9f74754ebdbfc544b05a258");
/// `Unpaused(address)`.
const UNPAUSED_TOPIC: [u8; 32] =
    hex!("5db9ee0a495bf2e6ff9c91a7834c1ba4fdd244a5e8aa4e537bd38aeae4b073aa");
/// EIP-1967 `Upgraded(address)`, emitted by an SY's proxy when its implementation changes.
const UPGRADED_TOPIC: [u8; 32] =
    hex!("bc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b");

/// The SY state that is not read per block: each token's class, and whether the SY is paused.
///
/// Both come from creation and from events, so they cost nothing per block:
///
/// - At creation, the classes the profile found and `paused()` as read then.
/// - `Paused` and `Unpaused` set `paused`, which refuses every quote against the SY while set.
/// - `Upgraded` and `Unpaused` re-run the classification at the end of the block. Every listed
///   token that answers its probe gets its class updated, or deleted if it no longer matches either
///   closed form. A token whose probe does not answer keeps the class it had: upgrades usually
///   happen while an SY is paused, and a paused SY reverts its previews. Re-running on `Unpaused`
///   is what classifies such an upgrade once the SY answers again. Only the tokens the component
///   lists can be classified: its token list is fixed at creation.
///
/// A paused SY reverts every transfer, and every market swap moves SY, so each market also carries
/// `sy_paused`: its SY's pause flag as read at the market's creation, then set by the SY's
/// `Paused` and `Unpaused` on every market priced against it.
///
/// A new component's own events in its creation block are skipped: its creation state is read at
/// the end of that block and already includes them.
#[substreams::handlers::map]
pub fn map_sy_state(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    components_store: StoreGetRaw,
    registry_store: StoreGetString,
) -> Result<BlockChanges> {
    let mut changes: BTreeMap<u64, TransactionChanges> = BTreeMap::new();
    let mut record = |tx: &Transaction, change: EntityChanges| {
        changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChanges { tx: Some(tx.clone()), ..Default::default() })
            .entity_changes
            .push(change);
    };

    let created = new_sy_components(&new_components);
    let seeded: HashSet<&str> = created
        .iter()
        .map(|(_, component)| component.id.as_str())
        .collect();
    for (tx, component) in &created {
        record(tx, creation_state(component));
    }
    let new_markets = new_market_components(&new_components);
    for (tx, market) in &new_markets {
        record(tx, market_creation_state(market));
    }
    let new_market_ids: HashSet<&str> = new_markets
        .iter()
        .map(|(_, market)| market.id.as_str())
        .collect();

    // The last upgrade or unpause per SY in this block, keyed by SY so a re-profile runs once.
    let mut reprofile: BTreeMap<String, Transaction> = BTreeMap::new();
    // Every pause change, from any emitter, for the markets of whichever SY it turns out to be.
    let mut pauses: Vec<(Transaction, String, bool)> = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(topic) = log.topics.first() else { continue };
            let sy_id = contract_id(&log.address);
            if topic.as_slice() == PAUSED_TOPIC || topic.as_slice() == UNPAUSED_TOPIC {
                pauses.push((tx.into(), sy_id.clone(), topic.as_slice() == PAUSED_TOPIC));
            }
            if seeded.contains(sy_id.as_str()) ||
                !components_store.has_last(&sy_id) ||
                is_market(&components_store, &sy_id)
            {
                continue;
            }
            if let Some(cap) = pendle_sy::events::SupplyCapUpdated::match_and_decode(log) {
                record(
                    &tx.into(),
                    EntityChanges {
                        component_id: sy_id,
                        attributes: vec![supply_cap_change(&cap.new_supply_cap)],
                    },
                );
                continue;
            }
            let paused = if topic.as_slice() == PAUSED_TOPIC {
                true
            } else if topic.as_slice() == UNPAUSED_TOPIC {
                reprofile.insert(sy_id.clone(), tx.into());
                false
            } else {
                if topic.as_slice() == UPGRADED_TOPIC {
                    reprofile.insert(sy_id, tx.into());
                }
                continue;
            };
            record(
                &tx.into(),
                EntityChanges {
                    component_id: sy_id,
                    attributes: vec![update(PAUSED, vec![u8::from(paused)])],
                },
            );
        }
    }

    // An SY's markets are found through the registry, which is parsed only on a block that paused
    // or unpaused something. A market can predate its SY's component, when the SY did not profile
    // for the first market on it, so the SY being a component is not required here.
    if !pauses.is_empty() {
        let registry = registry_store
            .get_last(MARKET_REGISTRY)
            .unwrap_or_default();
        for (tx, change) in market_pause_changes(&registry, &pauses, &new_market_ids) {
            record(&tx, change);
        }
    }

    for (sy_id, tx) in reprofile {
        let Some(tokens) = components_store.get_last(&sy_id) else { continue };
        let tokens: Vec<Vec<u8>> = serde_sibor::from_bytes(&tokens)
            .expect("deserializing component tokens from the component store");
        let address =
            hex::decode(sy_id.trim_start_matches("0x")).expect("SY component id is not hex");
        let Some(classification) = classify_sy(&address) else {
            // Deleting every class would retire the SY for good on what may be a transient
            // failure, so the classes it had stay until the next upgrade re-runs this.
            log::info!("SY {} did not classify on re-profile, keeping its classes", sy_id);
            continue;
        };
        record(
            &tx,
            EntityChanges {
                component_id: sy_id,
                attributes: reclassified(&address, &tokens, &classification),
            },
        );
    }

    Ok(BlockChanges { changes: changes.into_values().collect(), ..Default::default() })
}

/// A new SY's classes and pause flag, taken from what its profile read at creation.
fn creation_state(component: &ProtocolComponent) -> EntityChanges {
    let mut attributes = Vec::new();
    for attribute in &component.static_att {
        if is_class_attribute(&attribute.name) {
            attributes.push(update(&attribute.name, attribute.value.clone()));
        }
    }
    let paused = component
        .get_attribute_value(CREATION_PAUSED)
        .unwrap_or_else(|| panic!("SY {} was created without its pause seed", component.id));
    attributes.push(update(PAUSED, paused));
    if let Some(cap) = component.get_attribute_value(CREATION_SUPPLY_CAP) {
        attributes.push(update(SY_SUPPLY_CAP, cap));
    }
    EntityChanges { component_id: component.id.clone(), attributes }
}

/// The supply cap an SY's `SupplyCapUpdated` sets: the new cap, or a deletion when it lifts the cap
/// by setting `type(uint256).max`.
fn supply_cap_change(new_cap: &BigInt) -> Attribute {
    if is_uncapped(new_cap) {
        return Attribute {
            name: SY_SUPPLY_CAP.to_string(),
            value: vec![],
            change: ChangeType::Deletion.into(),
        };
    }
    update(SY_SUPPLY_CAP, new_cap.to_signed_bytes_be())
}

/// `sy_paused` on every market of each SY that paused or unpaused, in event order, leaving out the
/// markets created this block, whose creation read already includes it. An emitter that is not an
/// SY in the registry has no markets and yields nothing.
fn market_pause_changes(
    registry: &str,
    pauses: &[(Transaction, String, bool)],
    new_markets: &HashSet<&str>,
) -> Vec<(Transaction, EntityChanges)> {
    let mut changes = Vec::new();
    for (tx, sy_id, paused) in pauses {
        for market in markets_of_sy(registry, sy_id) {
            if new_markets.contains(market.as_str()) {
                continue;
            }
            changes.push((
                tx.clone(),
                EntityChanges {
                    component_id: market,
                    attributes: vec![update(SY_PAUSED, vec![u8::from(*paused)])],
                },
            ));
        }
    }
    changes
}

/// A new market's `sy_paused`, taken from what was read at its creation.
fn market_creation_state(market: &ProtocolComponent) -> EntityChanges {
    let paused = market
        .get_attribute_value(CREATION_SY_PAUSED)
        .unwrap_or_else(|| panic!("market {} was created without its SY's pause seed", market.id));
    EntityChanges { component_id: market.id.clone(), attributes: vec![update(SY_PAUSED, paused)] }
}

fn new_market_components(
    components: &BlockTransactionProtocolComponents,
) -> Vec<(&Transaction, &ProtocolComponent)> {
    let mut created = Vec::new();
    for tx_components in &components.tx_components {
        let tx = tx_components
            .tx
            .as_ref()
            .expect("component batch without a transaction");
        for component in &tx_components.components {
            let is_market = component
                .protocol_type
                .as_ref()
                .is_some_and(|t| t.name == PENDLE_MARKET);
            if is_market {
                created.push((tx, component));
            }
        }
    }
    created
}

/// Every class attribute of the listed tokens after a re-classification: an update for each token
/// that classified, a deletion for each listed token and direction that answered its probe and no
/// longer matches, and nothing for one that did not answer, which keeps its class.
///
/// The SY itself is on its own token list but is never one of its entry or exit tokens, so it
/// gets no attribute at all.
fn reclassified(sy: &[u8], listed: &[Vec<u8>], classification: &Classification) -> Vec<Attribute> {
    let Classification {
        tokens_in,
        tokens_out,
        undetermined_in,
        undetermined_out,
        entry_list_unread,
        exit_list_unread,
        ..
    } = classification;
    let keep = |classes: &[(Vec<u8>, crate::sy::TokenClass)]| {
        classes
            .iter()
            .filter(|(token, _)| listed.contains(token))
            .cloned()
            .collect::<Vec<_>>()
    };
    let classified = class_attributes(&keep(tokens_in), &keep(tokens_out));

    let mut attributes = Vec::new();
    for token in listed
        .iter()
        .filter(|token| token.as_slice() != sy)
    {
        for (prefix, undetermined, list_unread) in [
            (TOKEN_IN_CLASS_PREFIX, undetermined_in, entry_list_unread),
            (TOKEN_OUT_CLASS_PREFIX, undetermined_out, exit_list_unread),
        ] {
            let name = format!("{prefix}{}", hex::encode(token));
            match classified
                .iter()
                .find(|(classified_name, _)| *classified_name == name)
            {
                Some((_, value)) => attributes.push(update(&name, value.clone())),
                None if *list_unread || undetermined.contains(token) => {}
                None => attributes.push(Attribute {
                    name,
                    value: vec![],
                    change: ChangeType::Deletion.into(),
                }),
            }
        }
    }
    attributes
}

fn update(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Update.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sy::TokenClass;

    const SY: [u8; 20] = [0xcb; 20];
    const WSTETH: [u8; 20] = [0x7f; 20];
    const STETH: [u8; 20] = [0xae; 20];

    fn classification(
        tokens_in: &[(Vec<u8>, TokenClass)],
        tokens_out: &[(Vec<u8>, TokenClass)],
        undetermined: &[Vec<u8>],
    ) -> Classification {
        Classification {
            decimals: 18,
            asset: STETH.to_vec(),
            asset_decimals: 18,
            tokens_in: tokens_in.to_vec(),
            tokens_out: tokens_out.to_vec(),
            undetermined_in: undetermined.to_vec(),
            undetermined_out: undetermined.to_vec(),
            entry_list_unread: false,
            exit_list_unread: false,
        }
    }

    fn by_name(attributes: &[Attribute]) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = attributes
            .iter()
            .map(|a| (a.name.clone(), a.change == i32::from(ChangeType::Deletion)))
            .collect();
        out.sort();
        out
    }

    /// A token that stops matching either closed form after an upgrade loses its class rather than
    /// keeping the one it had, and a token that never had one is deleted harmlessly. The SY itself
    /// is never classified, so it gets nothing.
    #[test]
    fn a_reclassification_deletes_what_no_longer_classifies() {
        let listed = vec![SY.to_vec(), WSTETH.to_vec(), STETH.to_vec()];
        let attributes = reclassified(
            &SY,
            &listed,
            &classification(
                &[(WSTETH.to_vec(), TokenClass::OneToOne)],
                &[(WSTETH.to_vec(), TokenClass::OneToOne)],
                &[],
            ),
        );
        let wsteth = hex::encode(WSTETH);
        let steth = hex::encode(STETH);
        assert_eq!(
            by_name(&attributes),
            vec![
                (format!("token_in_class_0x{wsteth}"), false),
                (format!("token_in_class_0x{steth}"), true),
                (format!("token_out_class_0x{wsteth}"), false),
                (format!("token_out_class_0x{steth}"), true),
            ]
        );
    }

    /// A token an upgrade adds is not on the component's fixed token list, so it gets no class.
    #[test]
    fn a_reclassification_ignores_tokens_the_component_does_not_list() {
        let attributes = reclassified(
            &SY,
            &[WSTETH.to_vec()],
            &classification(
                &[(STETH.to_vec(), TokenClass::IndexRate), (WSTETH.to_vec(), TokenClass::OneToOne)],
                &[],
                &[],
            ),
        );
        assert!(attributes
            .iter()
            .all(|a| !a.name.contains(&hex::encode(STETH))));
    }

    /// An upgrade made while the SY is paused re-profiles against previews that revert. Those
    /// tokens keep the classes they had instead of losing them for good; the re-profile on
    /// `Unpaused` classifies them once they answer.
    #[test]
    fn a_token_whose_probe_did_not_answer_keeps_its_class() {
        let listed = vec![SY.to_vec(), WSTETH.to_vec(), STETH.to_vec()];
        let attributes = reclassified(
            &SY,
            &listed,
            &classification(
                &[(WSTETH.to_vec(), TokenClass::OneToOne)],
                &[],
                &[STETH.to_vec(), WSTETH.to_vec()],
            ),
        );
        let wsteth = hex::encode(WSTETH);
        assert_eq!(by_name(&attributes), vec![(format!("token_in_class_0x{wsteth}"), false)]);
    }

    /// An SY whose `getTokensOut()` does not answer on a re-profile has an unknown exit list, not
    /// an empty one: its exit tokens keep their classes rather than all being deleted.
    #[test]
    fn an_unread_token_list_keeps_that_directions_classes() {
        let mut unread = classification(&[(WSTETH.to_vec(), TokenClass::OneToOne)], &[], &[]);
        unread.exit_list_unread = true;
        let attributes = reclassified(&SY, &[WSTETH.to_vec(), STETH.to_vec()], &unread);
        let wsteth = hex::encode(WSTETH);
        let steth = hex::encode(STETH);
        assert_eq!(
            by_name(&attributes),
            vec![
                (format!("token_in_class_0x{wsteth}"), false),
                (format!("token_in_class_0x{steth}"), true),
            ]
        );
    }

    /// A pause reaches every market of its SY, an unpause clears it, and a market created this
    /// block is left to its creation read. A `Paused` from a contract that is no SY changes
    /// nothing.
    #[test]
    fn a_pause_reaches_every_market_of_its_sy() {
        use crate::registry::MarketEntry;
        let sy = format!("0x{}", hex::encode(SY));
        let market = |id: &str, sy: &str| MarketEntry {
            id: id.to_string(),
            sy: sy.to_string(),
            expiry: 1,
            factory: "27b1dacd74688af24a64bd3c9c1b143118740784".to_string(),
            ln_fee_rate_root_at_creation: "0".to_string(),
        };
        let registry = [
            market("0xaa", &sy),
            market("0xbb", "0xcafe"),
            market("0xcc", &sy),
            market("0xdd", &sy),
        ]
        .iter()
        .map(|entry| format!("{};", entry.encode()))
        .collect::<String>();
        let tx = |index: u64| Transaction { index, ..Default::default() };
        let pauses = vec![
            (tx(1), sy.clone(), true),
            (tx(2), "0x0000000000000000000000000000000000000001".to_string(), true),
            (tx(3), sy.clone(), false),
        ];
        let created_this_block = HashSet::from(["0xdd"]);

        let changes: Vec<(u64, String, Vec<u8>)> =
            market_pause_changes(&registry, &pauses, &created_this_block)
                .into_iter()
                .map(|(tx, change)| {
                    assert_eq!(change.attributes.len(), 1);
                    assert_eq!(change.attributes[0].name, SY_PAUSED);
                    (tx.index, change.component_id, change.attributes[0].value.clone())
                })
                .collect();
        assert_eq!(
            changes,
            vec![
                (1, "0xaa".to_string(), vec![1]),
                (1, "0xcc".to_string(), vec![1]),
                (3, "0xaa".to_string(), vec![0]),
                (3, "0xcc".to_string(), vec![0]),
            ]
        );
    }

    #[test]
    fn a_market_starts_with_its_sy_pause_as_read_at_creation() {
        let market = ProtocolComponent::new("0x34280882267ffa6383b363e278b027be083bbe3b")
            .with_attributes(&[("expiry", vec![1u8]), (CREATION_SY_PAUSED, vec![1u8])]);
        let state = market_creation_state(&market);
        assert_eq!(by_name(&state.attributes), vec![(SY_PAUSED.to_string(), false)]);
        assert_eq!(state.attributes[0].value, vec![1u8]);
    }

    #[test]
    fn creation_state_carries_the_classes_and_the_pause_flag() {
        let component = ProtocolComponent::new("0xcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcb")
            .with_attributes(&[
                ("sy_decimals", vec![18u8]),
                (
                    "token_in_class_0x7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
                    b"one_to_one".to_vec(),
                ),
                (CREATION_PAUSED, vec![1u8]),
            ]);
        let state = creation_state(&component);
        assert_eq!(
            by_name(&state.attributes),
            vec![
                ("paused".to_string(), false),
                ("token_in_class_0x7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f".to_string(), false),
            ]
        );
        let paused = state
            .attributes
            .iter()
            .find(|a| a.name == PAUSED)
            .unwrap();
        assert_eq!(paused.value, vec![1u8]);
    }

    /// `SupplyCapUpdated(uint256)` as a capped SY emits it: topic0 only, the new cap as data.
    fn supply_cap_updated(new_cap: &BigInt) -> eth::Log {
        let mut data = vec![0u8; 32];
        let bytes = new_cap.to_bytes_be().1;
        data[32 - bytes.len()..].copy_from_slice(&bytes);
        eth::Log {
            address: SY.to_vec(),
            topics: vec![hex::decode(
                "4e44c8be34d12f1b7f56b13b4bbe97e64ca37a91916f86c73412da80c21748e2",
            )
            .unwrap()],
            data,
            ..Default::default()
        }
    }

    #[test]
    fn a_cap_update_sets_the_new_cap() {
        let cap = BigInt::from(74_800_000u64) * BigInt::from(10u64).pow(18);
        let event = pendle_sy::events::SupplyCapUpdated::match_and_decode(supply_cap_updated(&cap))
            .expect("the event decodes");
        let change = supply_cap_change(&event.new_supply_cap);
        assert_eq!(change.name, SY_SUPPLY_CAP);
        assert_eq!(change.change, i32::from(ChangeType::Update));
        assert_eq!(BigInt::from_signed_bytes_be(&change.value), cap);
    }

    /// Setting the cap to `type(uint256).max` lifts it, so the attribute goes.
    #[test]
    fn lifting_the_cap_deletes_it() {
        let uncapped = BigInt::from(2).pow(256) - BigInt::from(1);
        let event =
            pendle_sy::events::SupplyCapUpdated::match_and_decode(supply_cap_updated(&uncapped))
                .expect("the event decodes");
        assert_eq!(
            supply_cap_change(&event.new_supply_cap).change,
            i32::from(ChangeType::Deletion)
        );
    }

    #[test]
    fn creation_state_carries_the_supply_cap() {
        let component = ProtocolComponent::new("0xcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcb")
            .with_attributes(&[(CREATION_PAUSED, vec![0u8]), (CREATION_SUPPLY_CAP, vec![0x10, 0])]);
        let state = creation_state(&component);
        let cap = state
            .attributes
            .iter()
            .find(|a| a.name == SY_SUPPLY_CAP)
            .expect("the cap is carried");
        assert_eq!(cap.value, vec![0x10, 0]);
    }
}
