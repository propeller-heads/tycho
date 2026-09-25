//! The terminal module of the Robinhood Uniswap V4 with-hooks package: the block changes the
//! shared pipeline aggregates, with every pool of the Pons V2 MemeHook carrying the fee terms its
//! registration froze.
//!
//! Robinhood runs no dynamic contract indexing, so the block changes carry no storage payload.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use ethereum_uniswap_v4_shared::{
    pb::uniswap::v4::{Events, LiquidityChanges, TickDeltas},
    utils::protocol_changes::collect_transaction_changes,
};
use serde::Deserialize;
use substreams::pb::substreams::StoreDeltas;
use substreams_ethereum::pb::eth::v2::{self as eth};
use tycho_substreams::prelude::*;

use crate::{
    pons::{pons_static_attributes, PONS_HOOK_IDENTIFIER},
    storage::{pad32, tx_storage_writes, Word},
};

/// Query-string parameters of [`map_pons_enriched_block_changes`]. `pons_hooks` is a non-empty,
/// comma-separated list of 20-byte hexadecimal addresses, e.g.
/// `pons_hooks=0xe5e702641ea86f4ae6cc3cdaed2b886f976be044,0x...`. The singular `pons_hook` form
/// remains accepted for existing overrides, but cannot be combined with `pons_hooks`.
#[derive(Debug, Deserialize)]
pub struct Params {
    pub pons_hooks: Option<String>,
    pub pons_hook: Option<String>,
}

impl Params {
    /// Reads the module's parameters off its query string. Fails, naming the query string, when a
    /// parameter is missing or the string is not a query string.
    pub fn parse_from_query(input: &str) -> Result<Self> {
        serde_qs::from_str(input)
            .map_err(|e| anyhow!("failed to parse query params `{input}`: {e}"))
    }

    /// Returns the configured Pons hooks as raw addresses. Each comma-separated entry must be
    /// exactly 20 hexadecimal bytes, and duplicate addresses are rejected case-insensitively.
    pub fn pons_hook_addresses(&self) -> Result<Vec<[u8; 20]>> {
        let value = match (&self.pons_hooks, &self.pons_hook) {
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "set exactly one of `pons_hooks` or the legacy `pons_hook`, not both"
                ))
            }
            (Some(value), None) => value,
            (None, Some(value)) => value,
            (None, None) => {
                return Err(anyhow!(
                    "missing Pons hook configuration: set `pons_hooks` to one or more comma-separated addresses"
                ))
            }
        };

        if value.is_empty() {
            return Err(anyhow!("`pons_hooks` must contain at least one address"))
        }

        let mut addresses = Vec::new();
        let mut seen = HashSet::new();
        for (index, value) in value.split(',').enumerate() {
            if value.is_empty() {
                return Err(anyhow!("`pons_hooks` entry {} is empty", index + 1))
            }

            let digits = value
                .strip_prefix("0x")
                .unwrap_or(value);
            let bytes = hex::decode(digits).map_err(|e| {
                anyhow!("`pons_hooks` entry {} `{value}` is not hex: {e}", index + 1)
            })?;
            let address = <[u8; 20]>::try_from(bytes.as_slice()).map_err(|_| {
                anyhow!("`pons_hooks` entry {} `{value}` is not exactly 20 bytes", index + 1)
            })?;
            if !seen.insert(address) {
                return Err(anyhow!(
                    "`pons_hooks` entry {} `{value}` duplicates an earlier address",
                    index + 1
                ))
            }
            addresses.push(address);
        }

        Ok(addresses)
    }
}

#[substreams::handlers::map]
pub fn map_pons_enriched_block_changes(
    params: String,
    block: eth::Block,
    created_pools: BlockEntityChanges,
    events: Events,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    ticks_map_deltas: TickDeltas,
    ticks_store_deltas: StoreDeltas,
    pool_liquidity_changes: LiquidityChanges,
    pool_liquidity_store_deltas: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let pons_hooks = Params::parse_from_query(&params)?.pons_hook_addresses()?;

    let mut changes = collect_transaction_changes(
        created_pools,
        events,
        balances_map_deltas,
        balances_store_deltas,
        ticks_map_deltas,
        ticks_store_deltas,
        pool_liquidity_changes,
        pool_liquidity_store_deltas,
    );
    enrich_pons_creations(&pons_hooks, &block, &mut changes);

    Ok(BlockChanges { block: Some((&block).into()), changes, storage_changes: vec![] })
}

/// Adds the Pons static attributes to every component in `changes` that a configured Pons hook
/// created: `hook_identifier` always, and the fee terms `registerPool` froze when the creating
/// transaction is in `block` and wrote them.
///
/// A component whose registration writes are missing, reverted or out of range keeps
/// `hook_identifier` alone and the reason is logged, so a consumer that needs the fee terms
/// rejects the pool instead of pricing it with a substituted value. Components of other hooks, and
/// every other change, are left untouched.
pub fn enrich_pons_creations(
    pons_hooks: &[[u8; 20]],
    block: &eth::Block,
    changes: &mut [TransactionChanges],
) {
    for tx_changes in changes {
        enrich_transaction(pons_hooks, block, tx_changes);
    }
}

fn enrich_transaction(
    pons_hooks: &[[u8; 20]],
    block: &eth::Block,
    tx_changes: &mut TransactionChanges,
) {
    let creates_a_pons_pool = tx_changes
        .component_changes
        .iter()
        .any(|component| configured_pons_hook(component, pons_hooks).is_some());
    if !creates_a_pons_pool {
        return
    }

    let tx_hash = tx_changes
        .tx
        .as_ref()
        .map_or_else(String::new, |tx| hex::encode(&tx.hash));
    let trace = tx_changes
        .tx
        .as_ref()
        .and_then(|tx| find_transaction(block, &tx.hash));

    for component in &mut tx_changes.component_changes {
        let Some(pons_hook) = configured_pons_hook(component, pons_hooks) else { continue };
        component.static_att.push(Attribute {
            name: "hook_identifier".to_string(),
            value: PONS_HOOK_IDENTIFIER.as_bytes().to_vec(),
            change: ChangeType::Creation.into(),
        });

        let Some(trace) = trace else {
            substreams::log::info!(
                "pons: pool {} keeps no fee terms: transaction {} is not in this block",
                component.id,
                tx_hash
            );
            continue
        };
        let Some(pool_id) = pool_id(component) else {
            substreams::log::info!(
                "pons: pool {} keeps no fee terms: its pool_id attribute is missing or oversized",
                component.id
            );
            continue
        };
        let writes = tx_storage_writes(trace, pons_hook);
        match pons_static_attributes(&pool_id, &writes) {
            Some(attributes) => component.static_att.extend(attributes),
            None => substreams::log::info!(
                "pons: pool {} keeps no fee terms from transaction {}",
                component.id,
                tx_hash
            ),
        }
    }
}

/// Returns the configured hook that created `component`.
fn configured_pons_hook<'a>(
    component: &ProtocolComponent,
    pons_hooks: &'a [[u8; 20]],
) -> Option<&'a [u8; 20]> {
    if component.change != i32::from(ChangeType::Creation) {
        return None
    }

    let hooks = static_attribute(component, "hooks")?;
    pons_hooks
        .iter()
        .find(|pons_hook| hooks == pons_hook.as_slice())
}

fn static_attribute<'a>(component: &'a ProtocolComponent, name: &str) -> Option<&'a [u8]> {
    component
        .static_att
        .iter()
        .find(|attribute| attribute.name == name)
        .map(|attribute| attribute.value.as_slice())
}

/// The pool id `component` carries, as the storage key the Pons `launches` mapping uses.
fn pool_id(component: &ProtocolComponent) -> Option<Word> {
    pad32(static_attribute(component, "pool_id")?)
}

/// The trace of the successful transaction `hash` in `block`.
fn find_transaction<'a>(block: &'a eth::Block, hash: &[u8]) -> Option<&'a eth::TransactionTrace> {
    block
        .transactions()
        .find(|transaction| transaction.hash == hash)
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{
        Block, Call, StorageChange, TransactionTrace, TransactionTraceStatus,
    };

    use super::*;
    use crate::{
        pons::PONS_LAUNCHES_SLOT,
        storage::{mapping_slot, word_at_offset},
    };

    const PONS_HOOK: [u8; 20] = hex_literal::hex!("e5e702641ea86f4ae6cc3cdaed2b886f976be044");
    const SECOND_PONS_HOOK: [u8; 20] =
        hex_literal::hex!("0000000000000000000000000000000000000002");
    const OTHER_HOOK: [u8; 20] = hex_literal::hex!("0000000aa232009084bd71a5797d089aa4edfad4");

    // Two registered pools read off Robinhood; the raw responses behind the words are in
    // assets/pons-launches-storage.json and variant_modules/fixtures/.
    const POOL_100_100: &str = "c96847cc43f7595aafcbc1c99d335cb91be7ce1107524c87030f48a716a5f289";
    const WORD_0_100_100: &str = "00000000000000000000ab5983fe30f186055095305c862b0e097dab3b520101";
    const WORD_4_100_100: &str = "0000012c006413880bb80064263ed295dafae1d9aadd6e56c4b6f9f38ee019dd";
    const POOL_0_100: &str = "18c178f47be974b35b88b5f58a452d745b5410764c7fa114dd6cfebe86d3b46d";
    const WORD_0_0_100: &str = "00000000000000000000b7667b0d7f70002c5ba451a58b7e98ca8582e76d0001";
    const WORD_4_0_100: &str = "0000012c006413880bb80000263ed295dafae1d9aadd6e56c4b6f9f38ee019dd";

    const TX_ONE: [u8; 32] =
        hex_literal::hex!("f66ca58190cc186683267cb486bbc6e1f0469176b7ad4f92e43eb68a1a0936e5");
    const TX_TWO: [u8; 32] =
        hex_literal::hex!("0000000000000000000000000000000000000000000000000000000000000002");

    fn word(hex_str: &str) -> Word {
        pad32(&hex::decode(hex_str).expect("test fixture is hex")).expect("test fixture is a word")
    }

    /// A Creation component as `map_pools_created` emits it, carrying only the two static
    /// attributes the enrichment reads.
    fn creation_component(pool_id: &str, hooks: &[u8; 20]) -> ProtocolComponent {
        ProtocolComponent {
            id: format!("0x{pool_id}"),
            change: i32::from(ChangeType::Creation),
            static_att: vec![
                Attribute {
                    name: "pool_id".to_string(),
                    value: hex::decode(pool_id).expect("test fixture is hex"),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "hooks".to_string(),
                    value: hooks.to_vec(),
                    change: ChangeType::Creation.into(),
                },
            ],
            ..Default::default()
        }
    }

    fn transaction_changes(
        hash: &[u8; 32],
        components: Vec<ProtocolComponent>,
    ) -> TransactionChanges {
        TransactionChanges {
            tx: Some(Transaction { hash: hash.to_vec(), index: 2, ..Default::default() }),
            component_changes: components,
            ..Default::default()
        }
    }

    /// The call `registerPool` makes: the two `launches[pool_id]` words this decoder reads, written
    /// over a slot that held zero.
    fn registration_call(
        hook: &[u8; 20],
        pool_id: &str,
        word_0: &str,
        word_4: &str,
        reverted: bool,
    ) -> Call {
        let base = mapping_slot(&word(pool_id), PONS_LAUNCHES_SLOT);
        let storage_changes = [(0u64, word_0), (4, word_4)]
            .into_iter()
            .enumerate()
            .map(|(ordinal, (offset, value))| StorageChange {
                address: hook.to_vec(),
                key: word_at_offset(&base, offset).to_vec(),
                old_value: vec![0u8; 32],
                new_value: word(value).to_vec(),
                ordinal: ordinal as u64,
            })
            .collect();

        Call { storage_changes, state_reverted: reverted, ..Default::default() }
    }

    fn block(traces: Vec<TransactionTrace>) -> Block {
        Block { number: 58_759_099, transaction_traces: traces, ..Default::default() }
    }

    fn trace(hash: &[u8; 32], calls: Vec<Call>) -> TransactionTrace {
        TransactionTrace {
            hash: hash.to_vec(),
            index: 2,
            status: i32::from(TransactionTraceStatus::Succeeded),
            calls,
            ..Default::default()
        }
    }

    fn attribute_names(component: &ProtocolComponent) -> Vec<&str> {
        component
            .static_att
            .iter()
            .map(|attribute| attribute.name.as_str())
            .collect()
    }

    fn attribute_value<'a>(component: &'a ProtocolComponent, name: &str) -> &'a [u8] {
        component
            .static_att
            .iter()
            .find(|attribute| attribute.name == name)
            .unwrap_or_else(|| panic!("{name} is missing"))
            .value
            .as_slice()
    }

    #[test]
    fn a_pons_creation_gets_the_identifier_and_the_fee_terms() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(
                &PONS_HOOK,
                POOL_100_100,
                WORD_0_100_100,
                WORD_4_100_100,
                false,
            )],
        )]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)])];

        enrich_pons_creations(&[PONS_HOOK], &block, &mut changes);

        let component = &changes[0].component_changes[0];
        assert_eq!(
            attribute_names(component),
            ["pool_id", "hooks", "hook_identifier", "pons_hook_fee_bps", "pons_creator_tax_bps"]
        );
        assert_eq!(attribute_value(component, "hook_identifier"), b"pons_v2");
        assert_eq!(attribute_value(component, "pons_hook_fee_bps"), [0x64]);
        assert_eq!(attribute_value(component, "pons_creator_tax_bps"), [0x64]);
        for attribute in &component.static_att {
            assert_eq!(attribute.change, i32::from(ChangeType::Creation), "{}", attribute.name);
        }
    }

    #[test]
    fn a_component_of_another_hook_is_left_untouched() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(
                &PONS_HOOK,
                POOL_100_100,
                WORD_0_100_100,
                WORD_4_100_100,
                false,
            )],
        )]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &OTHER_HOOK)])];

        enrich_pons_creations(&[PONS_HOOK], &block, &mut changes);

        assert_eq!(attribute_names(&changes[0].component_changes[0]), ["pool_id", "hooks"]);
    }

    #[test]
    fn a_component_that_is_not_a_creation_is_left_untouched() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(
                &PONS_HOOK,
                POOL_100_100,
                WORD_0_100_100,
                WORD_4_100_100,
                false,
            )],
        )]);
        let mut component = creation_component(POOL_100_100, &PONS_HOOK);
        component.change = i32::from(ChangeType::Update);
        let mut changes = vec![transaction_changes(&TX_ONE, vec![component])];

        enrich_pons_creations(&[PONS_HOOK], &block, &mut changes);

        assert_eq!(attribute_names(&changes[0].component_changes[0]), ["pool_id", "hooks"]);
    }

    #[test]
    fn a_transaction_without_pons_writes_yields_only_the_identifier() {
        let block = block(vec![trace(&TX_ONE, vec![Call::default()])]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)])];

        enrich_pons_creations(&[PONS_HOOK], &block, &mut changes);

        assert_eq!(
            attribute_names(&changes[0].component_changes[0]),
            ["pool_id", "hooks", "hook_identifier"]
        );
    }

    #[test]
    fn pons_writes_in_a_reverted_call_yield_only_the_identifier() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(&PONS_HOOK, POOL_100_100, WORD_0_100_100, WORD_4_100_100, true)],
        )]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)])];

        enrich_pons_creations(&[PONS_HOOK], &block, &mut changes);

        assert_eq!(
            attribute_names(&changes[0].component_changes[0]),
            ["pool_id", "hooks", "hook_identifier"]
        );
    }

    #[test]
    fn a_transaction_absent_from_the_block_yields_only_the_identifier() {
        let block = block(vec![]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)])];

        enrich_pons_creations(&[PONS_HOOK], &block, &mut changes);

        assert_eq!(
            attribute_names(&changes[0].component_changes[0]),
            ["pool_id", "hooks", "hook_identifier"]
        );
    }

    #[test]
    fn changes_without_a_transaction_yield_only_the_identifier() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(
                &PONS_HOOK,
                POOL_100_100,
                WORD_0_100_100,
                WORD_4_100_100,
                false,
            )],
        )]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)])];
        changes[0].tx = None;

        enrich_pons_creations(&[PONS_HOOK], &block, &mut changes);

        assert_eq!(
            attribute_names(&changes[0].component_changes[0]),
            ["pool_id", "hooks", "hook_identifier"]
        );
    }

    #[test]
    fn a_component_without_a_pool_id_yields_only_the_identifier() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(
                &PONS_HOOK,
                POOL_100_100,
                WORD_0_100_100,
                WORD_4_100_100,
                false,
            )],
        )]);
        let mut component = creation_component(POOL_100_100, &PONS_HOOK);
        component
            .static_att
            .retain(|attribute| attribute.name != "pool_id");
        let mut changes = vec![transaction_changes(&TX_ONE, vec![component])];

        enrich_pons_creations(&[PONS_HOOK], &block, &mut changes);

        assert_eq!(attribute_names(&changes[0].component_changes[0]), ["hooks", "hook_identifier"]);
    }

    #[test]
    fn two_pools_created_in_different_transactions_keep_their_own_terms() {
        let block = block(vec![
            trace(
                &TX_ONE,
                vec![registration_call(
                    &PONS_HOOK,
                    POOL_100_100,
                    WORD_0_100_100,
                    WORD_4_100_100,
                    false,
                )],
            ),
            trace(
                &TX_TWO,
                vec![registration_call(&PONS_HOOK, POOL_0_100, WORD_0_0_100, WORD_4_0_100, false)],
            ),
        ]);
        let mut changes = vec![
            transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)]),
            transaction_changes(&TX_TWO, vec![creation_component(POOL_0_100, &PONS_HOOK)]),
        ];

        enrich_pons_creations(&[PONS_HOOK], &block, &mut changes);

        let first = &changes[0].component_changes[0];
        assert_eq!(attribute_value(first, "pons_hook_fee_bps"), [0x64]);
        assert_eq!(attribute_value(first, "pons_creator_tax_bps"), [0x64]);
        let second = &changes[1].component_changes[0];
        assert_eq!(attribute_value(second, "pons_hook_fee_bps"), [0x64]);
        assert_eq!(attribute_value(second, "pons_creator_tax_bps"), [0x00]);
    }

    #[test]
    fn two_configured_hooks_created_in_one_transaction_keep_their_own_terms() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![
                registration_call(&PONS_HOOK, POOL_100_100, WORD_0_100_100, WORD_4_100_100, false),
                registration_call(&SECOND_PONS_HOOK, POOL_0_100, WORD_0_0_100, WORD_4_0_100, false),
            ],
        )]);
        let mut changes = vec![transaction_changes(
            &TX_ONE,
            vec![
                creation_component(POOL_100_100, &PONS_HOOK),
                creation_component(POOL_0_100, &SECOND_PONS_HOOK),
                creation_component(POOL_100_100, &OTHER_HOOK),
            ],
        )];

        enrich_pons_creations(&[PONS_HOOK, SECOND_PONS_HOOK], &block, &mut changes);

        let first = &changes[0].component_changes[0];
        assert_eq!(attribute_value(first, "pons_hook_fee_bps"), [0x64]);
        assert_eq!(attribute_value(first, "pons_creator_tax_bps"), [0x64]);
        let second = &changes[0].component_changes[1];
        assert_eq!(attribute_value(second, "pons_hook_fee_bps"), [0x64]);
        assert_eq!(attribute_value(second, "pons_creator_tax_bps"), [0x00]);
        assert_eq!(attribute_names(&changes[0].component_changes[2]), ["pool_id", "hooks"]);
    }

    #[test]
    fn params_accept_the_canonical_singleton() {
        let params =
            Params::parse_from_query("pons_hooks=0xe5e702641ea86f4ae6cc3cdaed2b886f976be044")
                .expect("the singleton query parses");
        assert_eq!(
            params
                .pons_hook_addresses()
                .expect("the canonical singleton decodes"),
            vec![PONS_HOOK]
        );
    }

    #[test]
    fn params_accept_multiple_hooks_and_the_legacy_singleton() {
        let params = Params::parse_from_query(
            "pons_hooks=E5E702641EA86F4AE6CC3CDAED2B886F976BE044,0x0000000000000000000000000000000000000002",
        )
        .expect("the multiple-hook query parses");
        assert_eq!(
            params
                .pons_hook_addresses()
                .expect("multiple hooks decode"),
            vec![PONS_HOOK, SECOND_PONS_HOOK]
        );

        let legacy = Params::parse_from_query("pons_hook=e5e702641ea86f4ae6cc3cdaed2b886f976be044")
            .expect("the legacy query parses");
        assert_eq!(
            legacy
                .pons_hook_addresses()
                .expect("the legacy value decodes"),
            vec![PONS_HOOK]
        );
    }

    #[test]
    fn params_reject_malformed_empty_and_duplicate_lists() {
        for input in [
            "pons_hooks=",
            "pons_hooks=0xe5e702641ea86f4ae6cc3cdaed2b886f976be044,",
            "pons_hooks=0xzzz",
            "pons_hooks=0xe5e702641ea86f4ae6cc3cdaed2b886f976be0",
            "pons_hooks=0xe5e702641ea86f4ae6cc3cdaed2b886f976be044,E5E702641EA86F4AE6CC3CDAED2B886F976BE044",
            "pons_hook=0xe5e702641ea86f4ae6cc3cdaed2b886f976be044&pons_hooks=0x0000000000000000000000000000000000000002",
        ] {
            let error = Params::parse_from_query(input)
                .expect("the query syntax is valid")
                .pons_hook_addresses()
                .expect_err("the hook list must be rejected");
            assert!(!error.to_string().is_empty(), "{input}");
        }
    }

    #[test]
    fn params_reject_a_query_string_without_the_hook() {
        assert!(Params::parse_from_query("pool_manager=0x00")
            .expect("the query syntax is valid")
            .pons_hook_addresses()
            .is_err());
    }

    #[test]
    fn the_robinhood_manifest_carries_a_parseable_hook_address() {
        let manifest = include_str!("../../robinhood-uniswap-v4-with-hooks.yaml");

        let params = manifest
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("map_pons_enriched_block_changes:")
            })
            .map(|value| value.trim().trim_matches('"'))
            .expect("the manifest sets the module's params");

        assert_eq!(
            Params::parse_from_query(params)
                .expect("the manifest params parse")
                .pons_hook_addresses()
                .expect("the manifest hook list decodes"),
            vec![PONS_HOOK]
        );
    }
}
