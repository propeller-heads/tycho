//! The delta format: every message the recorded extractors emit for their recorded streams,
//! revert messages included, committed as JSON in `delta_format/`, one file per stream. A test
//! fails while the emitted messages differ from the files, so a change to the deltas an
//! unchanged extractor emits cannot merge without regenerating them, and reviewers see the
//! change as a JSON diff.
//!
//! The files hold the messages in a canonical form that stays the same from run to run and from
//! day to day:
//! - the watermarks reset, as [`deltas_only`] does;
//! - object keys sorted, and the arrays that hold a set sorted by their JSON text;
//! - block timestamps and component creation times relative to the stream's base timestamp, written
//!   as that offset after the Unix epoch (`1970-01-01T00:00:12` is 12 seconds after the base
//!   timestamp), since the stream's dates depend on the day it is written;
//! - pretty-printed, with a trailing newline.
//!
//! Behaviour behind the mocked RPC services (the tracer, token metadata, account extraction)
//! and extractor modules the recorded extractors do not run (the Uniswap V4 hooks DCI, the
//! post-processors) cannot show up in the files. Neither can the revert of a block the extractor
//! sealed under a hash substreams no longer names (`HeightMatch` in `handle_revert`), which a
//! consistent stream never sends; `test_revert_hash_miss_purges_from_target_height` covers it.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::PathBuf,
};

use chrono::{DateTime, NaiveDateTime};
use rstest::rstest;
use serde_json::Value;
use tycho_common::models::{
    blockchain::{
        AccountOverrides, AddressStorageLocation, Block, BlockAggregatedChanges, DCIUpdate,
        EntryPoint, RPCTracerParams, StorageOverride, TracingParams, TracingResult,
    },
    contract::{AccountBalance, AccountDelta},
    protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
    token::Token,
    Chain, ChangeType,
};
use tycho_storage::postgres::testing::run_against_db;

use crate::extractor::{
    delta_format::FormatStream,
    recorded_extractors::{
        build_extractor, deltas_only, feed, recorded_partial_block_stream, recorded_stream,
        seed_database, ExtractorKind, RecordedStream,
    },
};

/// Set to `1` to rewrite the format files from what the extractors emit instead of comparing.
const UPDATE_ENV: &str = "UPDATE_DELTA_FORMAT";

/// The directory holding the format files.
fn format_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/extractor/delta_format")
}

impl FormatStream {
    fn extractor(self) -> ExtractorKind {
        match self {
            Self::Native | Self::NativePartialBlocks => ExtractorKind::Native,
            Self::Dci => ExtractorKind::Dci,
        }
    }

    fn recorded(self) -> RecordedStream {
        match self {
            Self::Native => recorded_stream(ExtractorKind::Native),
            Self::NativePartialBlocks => recorded_partial_block_stream(),
            Self::Dci => recorded_stream(ExtractorKind::Dci),
        }
    }

    fn path(self) -> PathBuf {
        format_dir().join(self.file_name())
    }

    /// The committed file: the bytes the binary was built with, which the fingerprint covers.
    ///
    /// # Panics
    /// If the file is not UTF-8.
    fn committed(self) -> &'static str {
        std::str::from_utf8(self.committed_bytes())
            .unwrap_or_else(|err| panic!("{} is not UTF-8: {err}", self.path().display()))
    }

    /// # Panics
    /// If the file does not hold messages.
    fn committed_messages(self) -> Vec<BlockAggregatedChanges> {
        serde_json::from_str(self.committed())
            .unwrap_or_else(|err| panic!("{} does not hold messages: {err}", self.path().display()))
    }
}

/// `messages` in the committed format, with their timestamps made relative to `base_ts`.
fn canonical_json(messages: &[BlockAggregatedChanges], base_ts: NaiveDateTime) -> String {
    let mut canonical = Vec::with_capacity(messages.len());
    for message in messages {
        let message = relative_to(deltas_only(message), base_ts);
        let mut value =
            sorted_keys(serde_json::to_value(&message).expect("messages serialize to JSON"));
        sort_sets(&mut value);
        canonical.push(value);
    }
    let mut json = serde_json::to_string_pretty(&canonical).expect("JSON values serialize");
    json.push('\n');
    json
}

/// `message` with its block timestamp and component creation times moved from `base_ts` to the
/// Unix epoch.
fn relative_to(
    mut message: BlockAggregatedChanges,
    base_ts: NaiveDateTime,
) -> BlockAggregatedChanges {
    let epoch = DateTime::UNIX_EPOCH.naive_utc();
    message.block.ts = epoch + (message.block.ts - base_ts);
    for component in message
        .new_protocol_components
        .values_mut()
        .chain(
            message
                .deleted_protocol_components
                .values_mut(),
        )
    {
        component.created_at = epoch + (component.created_at - base_ts);
    }
    message
}

/// `value` with the keys of every object in it sorted. Dev builds enable serde_json's
/// `preserve_order`, so objects keep the order they were serialized in, which for a hash map
/// changes from run to run.
fn sorted_keys(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<(String, Value)> = object.into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sorted_keys(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(sorted_keys)
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value,
    }
}

/// Sorts the arrays of `message` that serialize a `HashSet`, whose order changes from run to
/// run. A set field this misses makes the format test fail at random, which
/// `the_committed_format_files_are_canonical` catches without a database.
///
/// # Panics
/// If one of them is not an array: the message type changed, and this list must follow.
fn sort_sets(message: &mut Value) {
    for delta in object_values(message, "state_deltas") {
        sort_set(&mut delta["deleted_attributes"]);
    }
    let dci_update = &mut message["dci_update"];
    for entry_points in object_values(dci_update, "new_entrypoints") {
        sort_set(entry_points);
    }
    for params in object_values(dci_update, "new_entrypoint_params") {
        sort_set(params);
    }
    for result in object_values(dci_update, "trace_results") {
        sort_set(&mut result["retriggers"]);
        for slots in object_values(result, "accessed_slots") {
            sort_set(slots);
        }
    }
}

/// The values of the object `value` holds at `key`.
///
/// # Panics
/// If `value` holds no object at `key`.
fn object_values<'a>(value: &'a mut Value, key: &str) -> impl Iterator<Item = &'a mut Value> {
    value[key]
        .as_object_mut()
        .unwrap_or_else(|| panic!("{key} is not an object"))
        .values_mut()
}

/// # Panics
/// If `set` is not an array.
fn sort_set(set: &mut Value) {
    let Value::Array(items) = set else {
        panic!("{set} does not serialize a set");
    };
    items.sort_by_cached_key(Value::to_string);
}

/// Lines of context shown around the first line that differs.
const CONTEXT_LINES: usize = 3;

/// How `actual`, described by `source`, first differs from `committed`: the first message that
/// differs, and the lines around the first line that does. `None` when they are equal.
fn first_difference(committed: &str, actual: &str, source: &str) -> Option<String> {
    if committed == actual {
        return None;
    }
    let committed_lines: Vec<&str> = committed.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let line = committed_lines
        .iter()
        .zip(&actual_lines)
        .take_while(|(committed_line, actual_line)| committed_line == actual_line)
        .count();
    if line == committed_lines.len() && line == actual_lines.len() {
        return Some("They differ in line endings.".to_string());
    }
    let first = line.saturating_sub(CONTEXT_LINES);
    let excerpt = |lines: &[&str]| {
        lines
            .get(
                first..
                    lines
                        .len()
                        .min(line + CONTEXT_LINES + 1),
            )
            .unwrap_or_default()
            .join("\n")
    };
    Some(format!(
        "{}. From line {}, the file reads\n{}\nand {source}:\n{}",
        differing_message(committed, actual, source),
        first + 1,
        excerpt(&committed_lines),
        excerpt(&actual_lines),
    ))
}

/// The first message that differs between `committed` and `actual`, by index, block and kind.
fn differing_message(committed: &str, actual: &str, source: &str) -> String {
    let parse = serde_json::from_str::<Vec<Value>>;
    let (Ok(committed), Ok(actual)) = (parse(committed), parse(actual)) else {
        return "The file does not hold a list of messages".to_string();
    };
    match committed
        .iter()
        .zip(&actual)
        .position(|(committed, actual)| committed != actual)
    {
        Some(index) => format!("Message {index} ({}) differs", describe(&actual[index])),
        None => {
            format!("The file holds {} messages, and {} {source}", committed.len(), actual.len())
        }
    }
}

/// The block and kind of `message`: `block 3, partial 1` or `revert to block 5`.
fn describe(message: &Value) -> String {
    let kind = if message["revert"] == true { "revert to block" } else { "block" };
    let partial = message
        .get("partial_block_index")
        .map(|index| format!(", partial {index}"))
        .unwrap_or_default();
    format!("{kind} {}{partial}", message["block"]["number"])
}

#[rstest]
#[case::native(FormatStream::Native)]
#[case::native_partial_blocks(FormatStream::NativePartialBlocks)]
#[case::dci(FormatStream::Dci)]
#[tokio::test]
async fn test_serial_db_emitted_deltas_match_the_committed_format(#[case] stream: FormatStream) {
    run_against_db(move |pool| async move {
        let database = seed_database(&pool).await;
        let extractor = build_extractor(&database, stream.extractor()).await;
        let recorded = stream.recorded();

        let emitted = feed(&extractor, &recorded.messages, recorded.delivery).await;

        let actual = canonical_json(&emitted, recorded.base_ts);
        let path = stream.path();
        if std::env::var(UPDATE_ENV).is_ok_and(|value| value == "1") {
            fs::write(&path, &actual)
                .unwrap_or_else(|err| panic!("cannot write {}: {err}", path.display()));
            return;
        }
        if let Some(difference) =
            first_difference(stream.committed(), &actual, "the emitted messages")
        {
            panic!(
                "The messages emitted for the {stream:?} stream differ from {}. If the change is \
                 intended, rerun this test with {UPDATE_ENV}=1, review the diff and commit the \
                 file. Doing so changes every extractor's fingerprint, so clients resync from a \
                 snapshot after the deploy.\n{difference}",
                path.display()
            );
        }
    })
    .await;
}

/// Writing the parsed files again gives the files back: the canonical form drops nothing they
/// hold, and sorts every set the messages serialize.
#[rstest]
#[case::native(FormatStream::Native)]
#[case::native_partial_blocks(FormatStream::NativePartialBlocks)]
#[case::dci(FormatStream::Dci)]
fn the_committed_format_files_are_canonical(#[case] stream: FormatStream) {
    // The files' timestamps already count from the Unix epoch.
    let rewritten = canonical_json(&stream.committed_messages(), DateTime::UNIX_EPOCH.naive_utc());

    if let Some(difference) =
        first_difference(stream.committed(), &rewritten, "the file written again")
    {
        panic!("{} is not canonical.\n{difference}", stream.path().display());
    }
}

/// The fingerprint covers the files of `FormatStream::ALL` only, so a format file outside it
/// would not change fingerprints when it changes.
#[test]
fn every_format_file_belongs_to_a_stream() {
    let dir = format_dir();
    let mut files = BTreeSet::new();
    for entry in
        fs::read_dir(&dir).unwrap_or_else(|err| panic!("cannot list {}: {err}", dir.display()))
    {
        let entry = entry.unwrap_or_else(|err| panic!("cannot list {}: {err}", dir.display()));
        files.insert(
            entry
                .file_name()
                .into_string()
                .unwrap_or_else(|name| panic!("{name:?} in {} is not UTF-8", dir.display())),
        );
    }
    let streams: BTreeSet<String> = FormatStream::ALL
        .iter()
        .map(|stream| stream.file_name().to_string())
        .collect();

    assert_eq!(files, streams, "add every format file's stream to FormatStream::ALL");
}

/// The reason for every field no recorded message exercises.
const UNEXERCISED: [(&str, &str); 15] = [
    (
        "BlockAggregatedChanges.finalized_block_height",
        "reset in the format, as `deltas_only` does: a block replayed after a restart can arrive \
         final where it first arrived pending, and tycho-client does not read it",
    ),
    (
        "BlockAggregatedChanges.db_committed_block_height",
        "reset in the format: it records how far the database writes had landed when the \
         extractor sent the message, not anything about the block",
    ),
    ("BlockAggregatedChanges.chain", ETHEREUM),
    ("Block.chain", ETHEREUM),
    ("AccountDelta.chain", ETHEREUM),
    ("Token.chain", ETHEREUM),
    ("ProtocolComponent.chain", ETHEREUM),
    (
        "ProtocolComponentStateDelta.created_attributes",
        "never serialized: an indexer-internal hint for reverts",
    ),
    ("RPCTracerParams.state_overrides", HOOKS),
    ("RPCTracerParams.prune_addresses", HOOKS),
    ("AccountOverrides.slots", HOOKS),
    ("AccountOverrides.native_balance", HOOKS),
    ("AccountOverrides.code", HOOKS),
    ("StorageOverride::Diff", HOOKS),
    ("StorageOverride::Replace", HOOKS),
];
const ETHEREUM: &str = "every recorded extractor runs on Ethereum, which is Chain's default";
const HOOKS: &str = "part of tracing params substreams cannot set (RpcTraceData has a caller and \
                     calldata only); only the Uniswap V4 hooks DCI sets them, which the recorded \
                     extractors do not run";

/// Every field of the messages and their nested types holds a value in at least one committed
/// message, or is listed in `UNEXERCISED`, so a change to how a field is produced changes the
/// format. `Coverage` destructures every type without `..`: a new field fails to compile until
/// it is checked there.
#[test]
fn the_committed_format_exercises_every_field() {
    // `AccountDelta`'s `code` and `change` are crate-private in tycho-common, so
    // `Coverage::account_delta` cannot destructure it without `..`; its serialized fields stand
    // in for the destructuring.
    let account_delta = serde_json::to_value(AccountDelta::default()).expect("serializes");
    let account_delta_fields: BTreeSet<&str> = account_delta
        .as_object()
        .expect("AccountDelta serializes to an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        account_delta_fields,
        BTreeSet::from(["address", "balance", "chain", "change", "code", "slots"]),
        "AccountDelta's fields changed: update Coverage::account_delta and this list"
    );

    let mut coverage = Coverage::default();
    // `AccountOverrides` only appears in `RPCTracerParams.state_overrides`, which no recorded
    // message holds: check an empty one, so its fields and `StorageOverride`'s variants are
    // listed too.
    coverage.account_overrides(&AccountOverrides { slots: None, native_balance: None, code: None });
    for stream in FormatStream::ALL {
        for message in stream.committed_messages() {
            coverage.message(&message);
        }
    }

    let unexercised: BTreeSet<&str> = coverage
        .fields
        .iter()
        .filter(|(_, exercised)| !**exercised)
        .map(|(field, _)| *field)
        .collect();
    let exempt: BTreeSet<&str> = UNEXERCISED
        .iter()
        .map(|(field, _)| *field)
        .collect();
    let missing: Vec<&str> = unexercised
        .difference(&exempt)
        .copied()
        .collect();
    let stale: Vec<&str> = exempt
        .difference(&unexercised)
        .copied()
        .collect();
    let mut problems = Vec::new();
    if !missing.is_empty() {
        problems.push(format!(
            "No committed message exercises {missing:?}, so the format would not change with how \
             they are produced: exercise them in a recorded stream, or list them in UNEXERCISED \
             with the reason."
        ));
    }
    if !stale.is_empty() {
        problems.push(format!(
            "{stale:?} in UNEXERCISED are exercised or no longer exist: remove them."
        ));
    }
    assert!(problems.is_empty(), "{}", problems.join(" "));
}

/// Every field and enum variant of the messages and their nested types, with whether one of the
/// messages exercises it: holds a value that is not empty or not the type's default, or holds
/// the variant. A nested type's fields are listed once a value of that type is checked; the field
/// or variant holding it is checked in every message, so it shows up as unexercised when no
/// message reaches the nested type.
#[derive(Default)]
struct Coverage {
    fields: BTreeMap<&'static str, bool>,
}

impl Coverage {
    fn check(&mut self, field: &'static str, exercised: bool) {
        *self.fields.entry(field).or_default() |= exercised;
    }

    fn message(&mut self, message: &BlockAggregatedChanges) {
        let BlockAggregatedChanges {
            extractor,
            chain,
            block,
            finalized_block_height,
            db_committed_block_height,
            revert,
            state_deltas,
            account_deltas,
            new_tokens,
            new_protocol_components,
            deleted_protocol_components,
            component_balances,
            account_balances,
            component_tvl,
            dci_update,
            partial_block_index,
        } = message;
        self.check("BlockAggregatedChanges.extractor", !extractor.is_empty());
        self.check("BlockAggregatedChanges.chain", *chain != Chain::default());
        self.check("BlockAggregatedChanges.finalized_block_height", *finalized_block_height != 0);
        self.check(
            "BlockAggregatedChanges.db_committed_block_height",
            db_committed_block_height.is_some(),
        );
        self.check("BlockAggregatedChanges.revert", *revert);
        self.check("BlockAggregatedChanges.state_deltas", !state_deltas.is_empty());
        self.check("BlockAggregatedChanges.account_deltas", !account_deltas.is_empty());
        self.check("BlockAggregatedChanges.new_tokens", !new_tokens.is_empty());
        self.check(
            "BlockAggregatedChanges.new_protocol_components",
            !new_protocol_components.is_empty(),
        );
        self.check(
            "BlockAggregatedChanges.deleted_protocol_components",
            !deleted_protocol_components.is_empty(),
        );
        self.check("BlockAggregatedChanges.component_balances", !component_balances.is_empty());
        self.check("BlockAggregatedChanges.account_balances", !account_balances.is_empty());
        self.check(
            "BlockAggregatedChanges.component_tvl",
            component_tvl
                .values()
                .any(|tvl| *tvl != 0.0),
        );
        self.check("BlockAggregatedChanges.dci_update", *dci_update != DCIUpdate::default());
        self.check("BlockAggregatedChanges.partial_block_index", partial_block_index.is_some());

        self.block(block);
        for delta in state_deltas.values() {
            self.state_delta(delta);
        }
        for delta in account_deltas.values() {
            self.account_delta(delta);
        }
        for token in new_tokens.values() {
            self.token(token);
        }
        for component in new_protocol_components
            .values()
            .chain(deleted_protocol_components.values())
        {
            self.component(component);
        }
        for balance in component_balances
            .values()
            .flat_map(HashMap::values)
        {
            self.component_balance(balance);
        }
        for balance in account_balances
            .values()
            .flat_map(HashMap::values)
        {
            self.account_balance(balance);
        }
        self.dci_update(dci_update);
    }

    fn block(&mut self, block: &Block) {
        let Block { number, chain, hash, parent_hash, ts } = block;
        self.check("Block.number", *number != 0);
        self.check("Block.chain", *chain != Chain::default());
        self.check("Block.hash", !hash.is_empty());
        self.check("Block.parent_hash", !parent_hash.is_empty());
        self.check("Block.ts", *ts != NaiveDateTime::default());
    }

    fn state_delta(&mut self, delta: &ProtocolComponentStateDelta) {
        let ProtocolComponentStateDelta {
            component_id,
            updated_attributes,
            deleted_attributes,
            created_attributes,
        } = delta;
        self.check("ProtocolComponentStateDelta.component_id", !component_id.is_empty());
        self.check(
            "ProtocolComponentStateDelta.updated_attributes",
            !updated_attributes.is_empty(),
        );
        self.check(
            "ProtocolComponentStateDelta.deleted_attributes",
            !deleted_attributes.is_empty(),
        );
        self.check(
            "ProtocolComponentStateDelta.created_attributes",
            !created_attributes.is_empty(),
        );
    }

    /// See `the_committed_format_exercises_every_field` for why this one destructures with `..`.
    fn account_delta(&mut self, delta: &AccountDelta) {
        let AccountDelta { chain, address, slots, balance, .. } = delta;
        self.check("AccountDelta.chain", *chain != Chain::default());
        self.check("AccountDelta.address", !address.is_empty());
        self.check("AccountDelta.slots", !slots.is_empty());
        self.check("AccountDelta.balance", balance.is_some());
        self.check("AccountDelta.code", delta.code().is_some());
        self.check("AccountDelta.change", delta.change_type() != ChangeType::default());
    }

    fn token(&mut self, token: &Token) {
        let Token { address, symbol, decimals, tax, gas, chain, quality } = token;
        self.check("Token.address", !address.is_empty());
        self.check("Token.symbol", !symbol.is_empty());
        self.check("Token.decimals", *decimals != 0);
        self.check("Token.tax", *tax != 0);
        self.check("Token.gas", gas.iter().any(Option::is_some));
        self.check("Token.chain", *chain != Chain::default());
        self.check("Token.quality", *quality != 0);
    }

    fn component(&mut self, component: &ProtocolComponent) {
        let ProtocolComponent {
            id,
            protocol_system,
            protocol_type_name,
            chain,
            tokens,
            contract_addresses,
            static_attributes,
            change,
            creation_tx,
            created_at,
        } = component;
        self.check("ProtocolComponent.id", !id.is_empty());
        self.check("ProtocolComponent.protocol_system", !protocol_system.is_empty());
        self.check("ProtocolComponent.protocol_type_name", !protocol_type_name.is_empty());
        self.check("ProtocolComponent.chain", *chain != Chain::default());
        self.check("ProtocolComponent.tokens", !tokens.is_empty());
        self.check("ProtocolComponent.contract_addresses", !contract_addresses.is_empty());
        self.check("ProtocolComponent.static_attributes", !static_attributes.is_empty());
        self.check("ProtocolComponent.change", *change != ChangeType::default());
        self.check("ProtocolComponent.creation_tx", !creation_tx.is_empty());
        self.check("ProtocolComponent.created_at", *created_at != NaiveDateTime::default());
    }

    fn component_balance(&mut self, balance: &ComponentBalance) {
        let ComponentBalance { token, balance, balance_float, modify_tx, component_id } = balance;
        self.check("ComponentBalance.token", !token.is_empty());
        self.check("ComponentBalance.balance", !balance.is_empty());
        self.check("ComponentBalance.balance_float", *balance_float != 0.0);
        self.check("ComponentBalance.modify_tx", !modify_tx.is_empty());
        self.check("ComponentBalance.component_id", !component_id.is_empty());
    }

    fn account_balance(&mut self, balance: &AccountBalance) {
        let AccountBalance { account, token, balance, modify_tx } = balance;
        self.check("AccountBalance.account", !account.is_empty());
        self.check("AccountBalance.token", !token.is_empty());
        self.check("AccountBalance.balance", !balance.is_empty());
        self.check("AccountBalance.modify_tx", !modify_tx.is_empty());
    }

    fn dci_update(&mut self, update: &DCIUpdate) {
        let DCIUpdate { new_entrypoints, new_entrypoint_params, trace_results } = update;
        self.check("DCIUpdate.new_entrypoints", !new_entrypoints.is_empty());
        self.check("DCIUpdate.new_entrypoint_params", !new_entrypoint_params.is_empty());
        self.check("DCIUpdate.trace_results", !trace_results.is_empty());
        for entry_point in new_entrypoints.values().flatten() {
            self.entry_point(entry_point);
        }
        let mut rpc_tracer = false;
        for (params, _component_id) in new_entrypoint_params.values().flatten() {
            match params {
                TracingParams::RPCTracer(params) => {
                    rpc_tracer = true;
                    self.rpc_tracer_params(params);
                }
            }
        }
        self.check("TracingParams::RPCTracer", rpc_tracer);
        for result in trace_results.values() {
            self.tracing_result(result);
        }
    }

    fn entry_point(&mut self, entry_point: &EntryPoint) {
        let EntryPoint { external_id, target, signature } = entry_point;
        self.check("EntryPoint.external_id", !external_id.is_empty());
        self.check("EntryPoint.target", !target.is_empty());
        self.check("EntryPoint.signature", !signature.is_empty());
    }

    fn rpc_tracer_params(&mut self, params: &RPCTracerParams) {
        let RPCTracerParams { caller, calldata, state_overrides, prune_addresses } = params;
        self.check("RPCTracerParams.caller", caller.is_some());
        self.check("RPCTracerParams.calldata", !calldata.is_empty());
        self.check("RPCTracerParams.state_overrides", state_overrides.is_some());
        self.check("RPCTracerParams.prune_addresses", prune_addresses.is_some());
        for overrides in state_overrides
            .iter()
            .flat_map(BTreeMap::values)
        {
            self.account_overrides(overrides);
        }
    }

    fn account_overrides(&mut self, overrides: &AccountOverrides) {
        let AccountOverrides { slots, native_balance, code } = overrides;
        self.check("AccountOverrides.slots", slots.is_some());
        self.check("AccountOverrides.native_balance", native_balance.is_some());
        self.check("AccountOverrides.code", code.is_some());
        let (diff, replace) = match slots {
            Some(StorageOverride::Diff(diff)) => (!diff.is_empty(), false),
            Some(StorageOverride::Replace(replace)) => (false, !replace.is_empty()),
            None => (false, false),
        };
        self.check("StorageOverride::Diff", diff);
        self.check("StorageOverride::Replace", replace);
    }

    fn tracing_result(&mut self, result: &TracingResult) {
        let TracingResult { retriggers, accessed_slots } = result;
        self.check("TracingResult.retriggers", !retriggers.is_empty());
        self.check("TracingResult.accessed_slots", !accessed_slots.is_empty());
        for (_address, location) in retriggers {
            self.address_storage_location(location);
        }
    }

    fn address_storage_location(&mut self, location: &AddressStorageLocation) {
        let AddressStorageLocation { key, offset } = location;
        self.check("AddressStorageLocation.key", !key.is_empty());
        self.check("AddressStorageLocation.offset", *offset != 0);
    }
}
