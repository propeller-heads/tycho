//! Fields of the transfer path that can start a fee: for a token that charges nothing, each
//! field its transfer reads and a privileged function writes gets new values through state
//! overrides, and the legs run again. No function is called, so neither its name nor its
//! argument checks matter: the question is what the transfer code does with the field.
//!
//! A write starts a fee when a leg that ran delivers less, or when every leg succeeds and a
//! contract outside the trade starts to emit events: a token calling a fee manager, or swapping
//! its fees through a router.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::{Address, Bytes, B256, U256};
use futures::stream::{self, StreamExt};
use tracing::warn;

use super::{
    code::{CodeSummary, StorageField},
    owner_calls::{self, Callers, HOLDER_FUNCTIONS, NEVER_CALLED, PRIVILEGED_CALL_CONCURRENCY},
    scenario::{derived_address, Outcome, Scenario, TokenContext},
    swap,
};

/// Upper bound on field writes tried per token, each costing one simulation.
const MAX_FIELD_WRITES: usize = 48;
/// Values written into number fields: 5% for a fee rate in percent, per mille or basis points,
/// and less than 100% in any of them, so that the buy still delivers part of its amount.
const NUMBER_VALUES: [u64; 3] = [5, 50, 500];
/// Fee increases below this many basis points come from rounding, not from the token.
const FEE_TOLERANCE_BPS: i64 = 10;
/// Runtime code of the canary: emits an empty `LOG0` and returns a zero word, so that a call to
/// it succeeds and shows in the logs. `PUSH1 0 PUSH1 0 LOG0 PUSH1 32 PUSH1 0 RETURN`.
const CANARY_CODE: [u8; 10] = [0x60, 0x00, 0x60, 0x00, 0xa0, 0x60, 0x20, 0x60, 0x00, 0xf3];

/// The address written into `address` fields, which runs `CANARY_CODE`.
pub(super) fn canary() -> Address {
    derived_address(b"tycho-token-analysis-canary")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FieldType {
    Uint { bits: usize },
    Bool,
    Address,
}

impl FieldType {
    fn parse(abi_type: &str) -> Option<Self> {
        match abi_type {
            "bool" => Some(FieldType::Bool),
            "address" => Some(FieldType::Address),
            _ => {
                let bits: usize = abi_type
                    .strip_prefix("uint")?
                    .parse()
                    .ok()?;
                (bits > 0 && bits <= 256 && bits.is_multiple_of(8))
                    .then_some(FieldType::Uint { bits })
            }
        }
    }

    fn bytes(self) -> usize {
        match self {
            FieldType::Uint { bits } => bits / 8,
            FieldType::Bool => 1,
            FieldType::Address => 20,
        }
    }
}

/// A field of the transfer path at a concrete storage slot, and the functions that write it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TransferField {
    pub slot: B256,
    /// Byte offset from the low-order end of the slot, as Solidity packs storage.
    pub offset: u8,
    pub field_type: FieldType,
    /// For example "uint16 at slot 0x…0b, byte 2" or "mapping(address => bool)[pool] at slot
    /// 0x…".
    pub label: String,
    /// Selectors of the privileged functions that write the field.
    pub writers: Vec<Bytes>,
}

impl TransferField {
    /// Whether the field is an `address` field whose value in `read_only` is one of
    /// `addresses`.
    fn holds_any(&self, read_only: &BTreeMap<B256, B256>, addresses: &BTreeSet<Address>) -> bool {
        let Some(word) = read_only.get(&self.slot) else { return false };
        self.field_type == FieldType::Address &&
            addresses.contains(&Address::from_word(B256::from(self.value_in(*word))))
    }

    /// The field's value in `word`.
    fn value_in(&self, word: B256) -> U256 {
        let (start, end) = self.byte_range();
        U256::from_be_slice(&word[start..end])
    }

    /// `word` with the field set to `value`, which must fit.
    fn written_into(&self, word: B256, value: U256) -> B256 {
        let (start, end) = self.byte_range();
        let mut word = word;
        word[start..end].copy_from_slice(&value.to_be_bytes::<32>()[32 - (end - start)..]);
        word
    }

    fn byte_range(&self) -> (usize, usize) {
        let end = 32 - usize::from(self.offset);
        (end - self.field_type.bytes(), end)
    }

    /// The values to write, other than the current one: the numbers of `NUMBER_VALUES` that fit,
    /// the other boolean, or the canary.
    fn new_values(&self, current: U256) -> Vec<U256> {
        let values = match self.field_type {
            FieldType::Uint { bits } => NUMBER_VALUES
                .iter()
                .map(|v| U256::from(*v))
                .filter(|v| bits >= 256 || *v < U256::from(1) << bits)
                .collect(),
            FieldType::Bool => vec![U256::from(u8::from(current.is_zero()))],
            FieldType::Address => vec![U256::from_be_slice(canary().as_slice())],
        };
        values
            .into_iter()
            .filter(|v| *v != current)
            .collect()
    }
}

/// The fields of the transfer path that a privileged function can change: read without writing
/// by a traced leg, written by a function other than the holders' functions and with a caller,
/// and neither the owner nor a proxy's implementation. Plain fields come from evmole's layout;
/// entries of `address` mappings are found by their key, for each address that takes part in the
/// trade.
///
/// A field that holds one of `implementations` is an upgrade: a new value there replaces the
/// token's code, which the proxy flag reports, and breaks the legs instead of charging a fee.
pub(super) fn transfer_fields(
    code: &CodeSummary,
    context: &TokenContext,
    callers: &Callers,
    read_only: &BTreeMap<B256, B256>,
    implementations: &BTreeSet<Address>,
) -> Vec<TransferField> {
    let owner_fields = owner_calls::owner_fields(code);
    let parties = [
        ("pool", context.pool),
        ("trader", context.trader),
        ("wallet", context.wallet),
        ("token", context.token),
        ("router", swap::router()),
        ("buyer", swap::buyer()),
    ];
    let mut fields = Vec::new();
    for record in &code.storage {
        if owner_fields.contains(&(record.slot, record.offset)) {
            continue;
        }
        let Some(writers) = privileged_writers(code, record, callers) else { continue };
        if let Some(field_type) = FieldType::parse(&record.r#type) {
            if !read_only.contains_key(&record.slot) || overflows_slot(record.offset, field_type) {
                continue;
            }
            let field = TransferField {
                slot: record.slot,
                offset: record.offset,
                field_type,
                label: format!("{} at slot {}, byte {}", record.r#type, record.slot, record.offset),
                writers,
            };
            if !field.holds_any(read_only, implementations) {
                fields.push(field);
            }
            continue;
        }
        let Some(field_type) = address_mapping_value(&record.r#type) else { continue };
        for (name, key) in parties {
            let slot = owner_calls::mapping_entry(key, record.slot);
            if read_only.contains_key(&slot) {
                fields.push(TransferField {
                    slot,
                    offset: 0,
                    field_type,
                    label: format!("{}[{name}] at slot {}", record.r#type, record.slot),
                    writers: writers.clone(),
                });
            }
        }
    }
    fields
}

/// Whether a field of `field_type` at `offset` would run past the end of its slot.
fn overflows_slot(offset: u8, field_type: FieldType) -> bool {
    usize::from(offset) + field_type.bytes() > 32
}

/// The value type of a `mapping(address => T)` with a `T` this module writes.
fn address_mapping_value(abi_type: &str) -> Option<FieldType> {
    let compact = abi_type.replace(' ', "");
    let value = compact
        .strip_prefix("mapping(address=>")?
        .strip_suffix(')')?;
    FieldType::parse(value)
}

/// The functions with a caller that write `record`, unless a holder's function writes it too:
/// then it is a balance or an allowance, not configuration.
fn privileged_writers(
    code: &CodeSummary,
    record: &StorageField,
    callers: &Callers,
) -> Option<Vec<Bytes>> {
    let written_by_holders = record.writes.iter().any(|w| {
        HOLDER_FUNCTIONS
            .iter()
            .any(|h| w.as_ref() == h)
    });
    if written_by_holders {
        return None;
    }
    let state_changing: BTreeSet<&Bytes> = code
        .functions
        .iter()
        .filter(|f| f.may_change_state())
        .map(|f| &f.selector)
        .collect();
    let writers: Vec<Bytes> = record
        .writes
        .iter()
        .filter(|w| state_changing.contains(w))
        .filter(|w| !NEVER_CALLED.contains(&w.as_ref()))
        .filter(|w| callers.of(w).is_some())
        .cloned()
        .collect();
    (!writers.is_empty()).then_some(writers)
}

/// One field set to one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FieldWrite {
    pub field: TransferField,
    pub value: U256,
}

/// The legs run with `writes` applied.
#[derive(Debug, Clone)]
pub(super) struct FieldProbe {
    pub name: String,
    pub writes: Vec<FieldWrite>,
}

impl FieldProbe {
    /// The slot values to override: `read_only` with every write applied, packed fields of one
    /// slot together.
    fn slots(&self, read_only: &BTreeMap<B256, B256>) -> BTreeMap<B256, B256> {
        let mut slots = BTreeMap::new();
        for write in &self.writes {
            let slot = write.field.slot;
            let word = slots
                .get(&slot)
                .or_else(|| read_only.get(&slot))
                .copied()
                .unwrap_or_default();
            slots.insert(
                slot,
                write
                    .field
                    .written_into(word, write.value),
            );
        }
        slots
    }
}

/// One write of each new value of each field, then all fields together with their first new
/// value, which catches a fee that needs two fields at once. Cut to `MAX_FIELD_WRITES`.
pub(super) fn field_probes(
    fields: &[TransferField],
    read_only: &BTreeMap<B256, B256>,
) -> Vec<FieldProbe> {
    let mut probes = Vec::new();
    let mut together = Vec::new();
    for field in fields {
        let current = field.value_in(
            read_only
                .get(&field.slot)
                .copied()
                .unwrap_or_default(),
        );
        let values = field.new_values(current);
        if let Some(first) = values.first() {
            together.push(FieldWrite { field: field.clone(), value: *first });
        }
        for value in values {
            probes.push(FieldProbe {
                name: format!("field:{}={value}", field.label),
                writes: vec![FieldWrite { field: field.clone(), value }],
            });
        }
    }
    probes.truncate(MAX_FIELD_WRITES - 1);
    if together.len() > 1 {
        probes.push(FieldProbe { name: "field:all".to_string(), writes: together });
    }
    probes
}

/// A field write that started a fee.
#[derive(Debug, Clone)]
pub(super) struct FeeStart {
    pub probe: FieldProbe,
    /// What the write changed, e.g. "the legs charged buy 500 bp, transfer 500 bp, sell 500 bp".
    pub change: String,
}

impl FeeStart {
    /// For the verdict's reason.
    pub fn describe(&self) -> String {
        let mut writes = Vec::new();
        let mut writers = BTreeSet::new();
        for write in &self.probe.writes {
            writes.push(format!("{} set to {}", write.field.label, write.value));
            writers.extend(
                write
                    .field
                    .writers
                    .iter()
                    .map(ToString::to_string),
            );
        }
        format!(
            "the token charges no fee now, but with {} {}; written by {}",
            writes.join(" and "),
            self.change,
            writers
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Runs `probes` on top of `context` and keeps those that start a fee against `base`. A probe
/// that could not be simulated is left out.
pub(super) async fn find_fee_starts(
    context: &TokenContext,
    probes: Vec<FieldProbe>,
    read_only: &BTreeMap<B256, B256>,
    base: &Outcome,
) -> Vec<FeeStart> {
    let runs: Vec<_> = probes
        .iter()
        .map(|probe| {
            let mut context = context.clone();
            for (slot, value) in probe.slots(read_only) {
                context.set_storage(context.token, slot, value);
            }
            context
                .code
                .insert(canary(), Bytes::from_static(&CANARY_CODE));
            async move {
                context
                    .run(&Scenario::realistic(&probe.name))
                    .await
            }
        })
        .collect();
    let outcomes: Vec<Result<Outcome, String>> = stream::iter(runs)
        .buffered(PRIVILEGED_CALL_CONCURRENCY)
        .collect()
        .await;
    let mut starts = Vec::new();
    for (probe, outcome) in probes.into_iter().zip(outcomes) {
        match outcome {
            Ok(outcome) => {
                if let Some(change) = fee_change(base, &outcome) {
                    starts.push(FeeStart { probe, change });
                }
            }
            Err(error) => {
                warn!(token = ?context.token, probe = probe.name, error, "Field probe failed")
            }
        }
    }
    starts
}

/// What started in `outcome` compared with `base`, a run without a fee: a fee above the
/// tolerance on a leg that ran, or, when every leg succeeded, events from a contract and of a
/// kind `base` did not see. A leg that failed without a fee shows a limit or a gate, not a fee.
fn fee_change(base: &Outcome, outcome: &Outcome) -> Option<String> {
    let fees = outcome.fees();
    let base_tax = i64::try_from(base.fees().max_tax()).unwrap_or(i64::MAX);
    let tax = i64::try_from(fees.max_tax()).unwrap_or(i64::MAX);
    if tax > base_tax + FEE_TOLERANCE_BPS {
        let leg = |fee: Option<i64>| fee.map_or_else(|| "-".to_string(), |bps| format!("{bps} bp"));
        return Some(format!(
            "the legs charged buy {}, transfer {}, sell {}",
            leg(fees.buy),
            leg(fees.transfer),
            leg(fees.sell)
        ));
    }
    if !outcome.all_legs_ok() {
        return None;
    }
    let seen: BTreeSet<(Address, Option<B256>)> = base
        .leg_logs
        .iter()
        .map(|log| (log.address(), log.topics().first().copied()))
        .collect();
    let emitters: BTreeSet<Address> = outcome
        .leg_logs
        .iter()
        .filter(|log| !seen.contains(&(log.address(), log.topics().first().copied())))
        .map(|log| log.address())
        .collect();
    if emitters.is_empty() {
        return None;
    }
    let emitters: Vec<String> = emitters
        .iter()
        .map(|a| if *a == canary() { "the written address".to_string() } else { a.to_string() })
        .collect();
    Some(format!("the legs made {} emit new events", emitters.join(", ")))
}
