//! The terminal module of the Robinhood Uniswap V4 with-hooks package: the block changes the
//! shared pipeline aggregates, with every pool of the Pons V2 MemeHook carrying the fee terms its
//! registration froze.
//!
//! Robinhood runs no dynamic contract indexing, so the block changes carry no storage payload.

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

/// Query-string parameters of [`map_pons_enriched_block_changes`], e.g.
/// `pons_hook=0xe5e702641ea86f4ae6cc3cdaed2b886f976be044`.
#[derive(Debug, Deserialize)]
pub struct Params {
    pub pons_hook: String,
}

impl Params {
    /// Reads the module's parameters off its query string. Fails, naming the query string, when a
    /// parameter is missing or the string is not a query string.
    pub fn parse_from_query(input: &str) -> Result<Self> {
        serde_qs::from_str(input)
            .map_err(|e| anyhow!("failed to parse query params `{input}`: {e}"))
    }

    /// The Pons hook as the 20 raw bytes a component's `hooks` static attribute carries. Hex in
    /// either case, with or without a `0x` prefix. Fails, naming the value, when it is not 20
    /// hex-encoded bytes.
    pub fn pons_hook_address(&self) -> Result<[u8; 20]> {
        let digits = self
            .pons_hook
            .strip_prefix("0x")
            .unwrap_or(&self.pons_hook);
        let bytes = hex::decode(digits)
            .map_err(|e| anyhow!("pons_hook `{}` is not hex: {e}", self.pons_hook))?;

        <[u8; 20]>::try_from(bytes.as_slice())
            .map_err(|_| anyhow!("pons_hook `{}` is not 20 bytes", self.pons_hook))
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
    let pons_hook = Params::parse_from_query(&params)?.pons_hook_address()?;

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
    enrich_pons_creations(&pons_hook, &block, &mut changes);

    Ok(BlockChanges { block: Some((&block).into()), changes, storage_changes: vec![] })
}

/// Adds the Pons static attributes to every component in `changes` that `pons_hook` created:
/// `hook_identifier` always, and the fee terms `registerPool` froze when the creating transaction
/// is in `block` and wrote them.
///
/// A component whose registration writes are missing, reverted or out of range keeps
/// `hook_identifier` alone and the reason is logged, so a consumer that needs the fee terms
/// rejects the pool instead of pricing it with a substituted value. Components of other hooks, and
/// every other change, are left untouched.
pub fn enrich_pons_creations(
    pons_hook: &[u8; 20],
    block: &eth::Block,
    changes: &mut [TransactionChanges],
) {
    for tx_changes in changes {
        enrich_transaction(pons_hook, block, tx_changes);
    }
}

fn enrich_transaction(
    pons_hook: &[u8; 20],
    block: &eth::Block,
    tx_changes: &mut TransactionChanges,
) {
    let creates_a_pons_pool = tx_changes
        .component_changes
        .iter()
        .any(|component| is_pons_creation(component, pons_hook));
    if !creates_a_pons_pool {
        return
    }

    let tx_hash = tx_changes
        .tx
        .as_ref()
        .map_or_else(String::new, |tx| hex::encode(&tx.hash));
    let writes = tx_changes
        .tx
        .as_ref()
        .and_then(|tx| find_transaction(block, &tx.hash))
        .map(|trace| tx_storage_writes(trace, pons_hook));

    for component in &mut tx_changes.component_changes {
        if !is_pons_creation(component, pons_hook) {
            continue
        }
        component.static_att.push(Attribute {
            name: "hook_identifier".to_string(),
            value: PONS_HOOK_IDENTIFIER.as_bytes().to_vec(),
            change: ChangeType::Creation.into(),
        });

        let Some(writes) = writes.as_ref() else {
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
        match pons_static_attributes(&pool_id, writes) {
            Some(attributes) => component.static_att.extend(attributes),
            None => substreams::log::info!(
                "pons: pool {} keeps no fee terms from transaction {}",
                component.id,
                tx_hash
            ),
        }
    }
}

/// Whether `component` is a pool `pons_hook` created.
fn is_pons_creation(component: &ProtocolComponent, pons_hook: &[u8; 20]) -> bool {
    component.change == i32::from(ChangeType::Creation) &&
        static_attribute(component, "hooks") == Some(pons_hook.as_slice())
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
    fn registration_call(pool_id: &str, word_0: &str, word_4: &str, reverted: bool) -> Call {
        let base = mapping_slot(&word(pool_id), PONS_LAUNCHES_SLOT);
        let storage_changes = [(0u64, word_0), (4, word_4)]
            .into_iter()
            .enumerate()
            .map(|(ordinal, (offset, value))| StorageChange {
                address: PONS_HOOK.to_vec(),
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
            vec![registration_call(POOL_100_100, WORD_0_100_100, WORD_4_100_100, false)],
        )]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)])];

        enrich_pons_creations(&PONS_HOOK, &block, &mut changes);

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
            vec![registration_call(POOL_100_100, WORD_0_100_100, WORD_4_100_100, false)],
        )]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &OTHER_HOOK)])];

        enrich_pons_creations(&PONS_HOOK, &block, &mut changes);

        assert_eq!(attribute_names(&changes[0].component_changes[0]), ["pool_id", "hooks"]);
    }

    #[test]
    fn a_component_that_is_not_a_creation_is_left_untouched() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(POOL_100_100, WORD_0_100_100, WORD_4_100_100, false)],
        )]);
        let mut component = creation_component(POOL_100_100, &PONS_HOOK);
        component.change = i32::from(ChangeType::Update);
        let mut changes = vec![transaction_changes(&TX_ONE, vec![component])];

        enrich_pons_creations(&PONS_HOOK, &block, &mut changes);

        assert_eq!(attribute_names(&changes[0].component_changes[0]), ["pool_id", "hooks"]);
    }

    #[test]
    fn a_transaction_without_pons_writes_yields_only_the_identifier() {
        let block = block(vec![trace(&TX_ONE, vec![Call::default()])]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)])];

        enrich_pons_creations(&PONS_HOOK, &block, &mut changes);

        assert_eq!(
            attribute_names(&changes[0].component_changes[0]),
            ["pool_id", "hooks", "hook_identifier"]
        );
    }

    #[test]
    fn pons_writes_in_a_reverted_call_yield_only_the_identifier() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(POOL_100_100, WORD_0_100_100, WORD_4_100_100, true)],
        )]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)])];

        enrich_pons_creations(&PONS_HOOK, &block, &mut changes);

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

        enrich_pons_creations(&PONS_HOOK, &block, &mut changes);

        assert_eq!(
            attribute_names(&changes[0].component_changes[0]),
            ["pool_id", "hooks", "hook_identifier"]
        );
    }

    #[test]
    fn changes_without_a_transaction_yield_only_the_identifier() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(POOL_100_100, WORD_0_100_100, WORD_4_100_100, false)],
        )]);
        let mut changes =
            vec![transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)])];
        changes[0].tx = None;

        enrich_pons_creations(&PONS_HOOK, &block, &mut changes);

        assert_eq!(
            attribute_names(&changes[0].component_changes[0]),
            ["pool_id", "hooks", "hook_identifier"]
        );
    }

    #[test]
    fn a_component_without_a_pool_id_yields_only_the_identifier() {
        let block = block(vec![trace(
            &TX_ONE,
            vec![registration_call(POOL_100_100, WORD_0_100_100, WORD_4_100_100, false)],
        )]);
        let mut component = creation_component(POOL_100_100, &PONS_HOOK);
        component
            .static_att
            .retain(|attribute| attribute.name != "pool_id");
        let mut changes = vec![transaction_changes(&TX_ONE, vec![component])];

        enrich_pons_creations(&PONS_HOOK, &block, &mut changes);

        assert_eq!(attribute_names(&changes[0].component_changes[0]), ["hooks", "hook_identifier"]);
    }

    #[test]
    fn two_pools_created_in_different_transactions_keep_their_own_terms() {
        let block = block(vec![
            trace(
                &TX_ONE,
                vec![registration_call(POOL_100_100, WORD_0_100_100, WORD_4_100_100, false)],
            ),
            trace(&TX_TWO, vec![registration_call(POOL_0_100, WORD_0_0_100, WORD_4_0_100, false)]),
        ]);
        let mut changes = vec![
            transaction_changes(&TX_ONE, vec![creation_component(POOL_100_100, &PONS_HOOK)]),
            transaction_changes(&TX_TWO, vec![creation_component(POOL_0_100, &PONS_HOOK)]),
        ];

        enrich_pons_creations(&PONS_HOOK, &block, &mut changes);

        let first = &changes[0].component_changes[0];
        assert_eq!(attribute_value(first, "pons_hook_fee_bps"), [0x64]);
        assert_eq!(attribute_value(first, "pons_creator_tax_bps"), [0x64]);
        let second = &changes[1].component_changes[0];
        assert_eq!(attribute_value(second, "pons_hook_fee_bps"), [0x64]);
        assert_eq!(attribute_value(second, "pons_creator_tax_bps"), [0x00]);
    }

    #[test]
    fn params_parse_a_prefixed_address() {
        let params =
            Params::parse_from_query("pons_hook=0xe5e702641ea86f4ae6cc3cdaed2b886f976be044")
                .expect("a prefixed address parses");

        assert_eq!(
            params
                .pons_hook_address()
                .expect("a prefixed address decodes"),
            PONS_HOOK
        );
    }

    #[test]
    fn params_parse_an_unprefixed_uppercase_address() {
        let params = Params::parse_from_query("pons_hook=E5E702641EA86F4AE6CC3CDAED2B886F976BE044")
            .expect("an unprefixed address parses");

        assert_eq!(
            params
                .pons_hook_address()
                .expect("an unprefixed address decodes"),
            PONS_HOOK
        );
    }

    #[test]
    fn params_reject_an_address_that_is_not_twenty_bytes() {
        let params = Params::parse_from_query("pons_hook=0xe5e702641ea86f4ae6cc3cdaed2b886f976be0")
            .expect("the query string itself is well formed");

        assert!(params.pons_hook_address().is_err());
    }

    #[test]
    fn params_reject_a_non_hex_address() {
        let params = Params::parse_from_query("pons_hook=0xzzz")
            .expect("the query string itself is well formed");

        assert!(params.pons_hook_address().is_err());
    }

    #[test]
    fn params_reject_a_query_string_without_the_hook() {
        assert!(Params::parse_from_query("pool_manager=0x00").is_err());
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
                .pons_hook_address()
                .expect("the manifest hook address decodes"),
            PONS_HOOK
        );
    }
}
