//! The next block's state on a chain whose builders land makers' updates (Ethereum): the map a
//! `/state` endpoint answers to `titan_getPammStateOverrides`, read from every configured URL each
//! poll.
//!
//! On such a chain a maker's quote is written into the block that fills it: the maker's relay
//! entry lands ahead of the fill in a builder's block, and a fill in any other block finds no word
//! for that block. So nothing read at the latest block shows what the next block fills. The map
//! names the next block (`blockNumber`, hex, and the beacon `slot`, whose time is 1,606,824,023 +
//! 12 × slot) and, per entry, the storage that block's relay entries will write (`stateOverride`:
//! per account its `stateDiff`, `balance` and `nonce`): a listing's quote contract and the price
//! contract its update posts to. The reader runs every call under that block's number and time as
//! a block override and, as a state override, only the slots of the listings it samples: per
//! listing, its own slots of its desk contract (by the slot rule, [`super::slots`]) and its price
//! contracts, all from the one answer that covers it (see [`NextBlock::take`]).

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use alloy::primitives::{Address, B256, U256, U64};
use futures::{stream::FuturesUnordered, StreamExt};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::{
    pamm::protocols::efficient_prop::slots::{fills_at, is_pair_slot, ListingSlots},
    snapshot_feed::errors::FeedError,
};

/// The JSON-RPC method a `/state` endpoint answers.
pub(super) const STATE_METHOD: &str = "titan_getPammStateOverrides";

/// Ethereum's beacon genesis time, Unix seconds: slot `s` starts at this plus 12 s each.
pub(super) const BEACON_GENESIS: u64 = 1_606_824_023;

/// Seconds per beacon slot.
pub(super) const SLOT_SECS: u64 = 12;

/// How a chain whose builders land makers' updates spaces its blocks, which tells the reader
/// which map is for the block after the node's latest and when that block lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cadence {
    /// Ethereum's beacon slots: a block every 12 s at most, its time named by the map's `slot`.
    Slots,
    /// A block every `block_ms` milliseconds, its timestamp in whole seconds and its milliseconds
    /// in its header (BNB Chain's BEP-520): a map names its block alone, and the next block
    /// falls in the head's second or the one after.
    Blocks { block_ms: u64 },
}

impl Cadence {
    /// BNB Chain's: a block every 450 ms.
    pub(super) const BNB: Self = Self::Blocks { block_ms: 450 };

    /// How many whole seconds past its block's time a node's latest block may be when the node
    /// has stalled: two slots on Ethereum (with one missed its latest block is at most that old),
    /// and on a chain making several blocks a second, the second its next two blocks reach.
    pub(super) fn stall_secs(self) -> u64 {
        match self {
            Self::Slots => 2 * SLOT_SECS,
            Self::Blocks { block_ms } => (2 * block_ms).div_ceil(1000),
        }
    }
}

/// The largest `/state` answer read, bytes.
pub(super) const MAX_STATE_BODY: usize = 4 * 1024 * 1024;

/// The body of `response`, read up to `cap` bytes: an answer announcing or sending more fails.
pub(super) async fn capped_body(
    mut response: reqwest::Response,
    cap: usize,
    what: &str,
) -> Result<Vec<u8>, FeedError> {
    let over = || FeedError::Parsing(format!("{what}: the answer is over {cap} bytes"));
    if response
        .content_length()
        .is_some_and(|length| length > cap as u64)
    {
        return Err(over());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| FeedError::Connection(format!("{what}: {}", e.without_url())))?
    {
        if body.len() + chunk.len() > cap {
            return Err(over());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A hex quantity of up to 32 bytes as a `/state` answer may write it, compact (`0x1`) or padded:
/// the reader forwards what it parsed, re-encoded, never the answer's own text.
fn hex_number(text: &str) -> Result<U256, String> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .ok_or_else(|| format!("{text} is not 0x-prefixed hex"))?;
    if digits.is_empty() || digits.len() > 64 {
        return Err(format!("{text} is not a hex number of up to 32 bytes"));
    }
    U256::from_str_radix(digits, 16).map_err(|e| format!("{text}: {e}"))
}

/// A storage word as an answer writes it: [`hex_number`], held as the 32 bytes a node takes.
#[derive(Debug, Clone, Copy)]
struct Word(B256);

impl<'de> Deserialize<'de> for Word {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        hex_number(&text)
            .map(|word| Self(B256::from(word)))
            .map_err(serde::de::Error::custom)
    }
}

/// How long a poll keeps collecting the other URLs' maps after the first map carrying an entry
/// answers.
pub(super) const MERGE_WINDOW: Duration = Duration::from_millis(300);

/// One account's override as one entry gives it, its slots in the order and number written: a slot
/// named twice (two spellings of one key) is kept twice, so the merge can see the disagreement.
#[derive(Debug, Clone, Deserialize)]
struct RawAccount {
    #[serde(rename = "stateDiff", default, deserialize_with = "pairs")]
    state_diff: Vec<(B256, Word)>,
    #[serde(default)]
    balance: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
}

/// A JSON object read as its pairs, in order, keys parsed as `K` and none merged: two spellings of
/// one key stay two pairs.
fn pairs<'de, D, K, V>(deserializer: D) -> Result<Vec<(K, V)>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Deserialize<'de>,
    V: Deserialize<'de>,
{
    struct Pairs<K, V>(std::marker::PhantomData<(K, V)>);
    impl<'de, K: Deserialize<'de>, V: Deserialize<'de>> serde::de::Visitor<'de> for Pairs<K, V> {
        type Value = Vec<(K, V)>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut pairs = Vec::new();
            while let Some(pair) = map.next_entry()? {
                pairs.push(pair);
            }
            Ok(pairs)
        }
    }
    deserializer.deserialize_map(Pairs(std::marker::PhantomData))
}

/// One account's override, merged over the entries of one answer. A `balance` or `nonce` that is
/// absent or empty is not overridden; one given is held as the number it reads as.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AccountOverride {
    state_diff: BTreeMap<B256, B256>,
    balance: Option<U256>,
    nonce: Option<u64>,
    /// The slots two writes of one answer gave different values, or that an entry left out
    /// writes: the first value is the one kept in `state_diff`, and a listing owning such a slot
    /// is dropped.
    conflicts: BTreeSet<B256>,
}

impl AccountOverride {
    /// The account as `eth_call`'s state override takes it.
    fn to_json(&self) -> Value {
        let mut fields = serde_json::Map::new();
        fields.insert("stateDiff".into(), serde_json::json!(self.state_diff));
        // Each as the hex quantity a node takes, whatever spelling the answer gave it.
        if let Some(balance) = self.balance {
            fields.insert("balance".into(), Value::from(format!("{balance:#x}")));
        }
        if let Some(nonce) = self.nonce {
            fields.insert("nonce".into(), Value::from(format!("{nonce:#x}")));
        }
        Value::Object(fields)
    }
}

/// One entry of a map: the accounts one relay entry writes.
#[derive(Debug, Clone, Deserialize)]
struct Entry {
    /// The accounts written, an account named twice (two spellings of one address) kept twice.
    #[serde(rename = "stateOverride", deserialize_with = "pairs")]
    state_override: Vec<(Address, RawAccount)>,
}

/// The accounts one answer writes storage of, each with every slot its entries write.
type Accounts = BTreeMap<Address, AccountOverride>;

/// One URL's map: its block, that block's time, and the accounts its entries write.
#[derive(Debug, Clone)]
pub(super) struct Map {
    number: u64,
    /// Unix seconds: the slot's start; zero on [`Cadence::Blocks`], whose map names no time.
    time: u64,
    accounts: Accounts,
    /// Whether any entry of the answer could not be read whole: it covers no listing.
    malformed: bool,
}

/// The maps of one poll, in the configured order of the URLs that answered them.
#[derive(Debug, Clone)]
pub(super) struct StateMaps(Vec<Map>);

impl StateMaps {
    /// The next block's state, from the maps for the block after the node's latest, `head`, made
    /// at `head_time`, Unix seconds, at the slot right after the head's: `head_time` + 12 s.
    /// `None` where no map names both. A map for any other block is never used: one at or behind
    /// the head is for a block already made, and one further ahead was made on state the node has
    /// not seen. Nor is a map for that block at a later slot, whether a slot was missed or the map
    /// runs ahead of the wall clock: its doors would be sampled at a time the block does not land
    /// at. That poll serves nothing, and the next is read as usual.
    #[cfg(test)]
    pub(super) fn for_block_after(self, head: u64, head_time: u64) -> Option<NextBlock> {
        self.for_block_after_in(head, head_time, Cadence::Slots)
    }

    /// The same at `cadence`. On [`Cadence::Blocks`] a map names no time: the maps for the block
    /// after `head` are taken, and that block's time is the head's second, `head_time`, the first
    /// it may fall in (the reader samples it at the next second as well).
    pub(super) fn for_block_after_in(
        self,
        head: u64,
        head_time: u64,
        cadence: Cadence,
    ) -> Option<NextBlock> {
        let number = head.checked_add(1)?;
        let time = match cadence {
            Cadence::Slots => head_time.checked_add(SLOT_SECS)?,
            Cadence::Blocks { .. } => head_time,
        };
        let (answers, malformed): (Vec<Accounts>, Vec<bool>) = self
            .0
            .into_iter()
            .filter(|map| map.number == number && (cadence != Cadence::Slots || map.time == time))
            .map(|map| (map.accounts, map.malformed))
            .unzip();
        (!answers.is_empty()).then_some(NextBlock { number, time, answers, malformed })
    }
}

/// The next block's state: its number and time, and per answer, in the configured order of its
/// URL, the accounts it writes.
#[derive(Debug, Clone)]
pub(super) struct NextBlock {
    pub(super) number: u64,
    /// Unix seconds: the slot's start, or on [`Cadence::Blocks`] the head's second, the first
    /// the block may fall in.
    pub(super) time: u64,
    answers: Vec<Accounts>,
    /// Per answer, whether it has an entry that could not be read whole: it covers no listing.
    malformed: Vec<bool>,
}

/// What one listing's update writes: its own slots of its desk contract, by the slot rule
/// (`quote`), whose quote word the answer must write with a stamp the block fills, under the
/// head's desk epoch and on a desk the head has not stopped; and each price contract the listing
/// reads in the next block, which the same answer must write (`needs`). A listing's backup source
/// is not here: the fill reads the live source alone.
#[derive(Debug, Clone)]
pub(super) struct Wanted {
    pub quote: ListingSlots,
    /// Whether the listing's word may fill in the block after its landing: its carry life is set
    /// and its fence is not NONE. A stamp of the block before the map's then covers it too.
    pub carries: bool,
    pub needs: Vec<Address>,
}

/// Why a listing's slots were not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NotTaken {
    /// No answer writes its quote word: its maker sent no update for the block.
    NoEntry,
    /// Every answer that writes its quote word stamps a block the next block does not fill: an
    /// older one, or another.
    Stale,
    /// The head's desk word has the desk stopped: no answer covers any listing of it.
    Stopped,
    /// Every answer that writes its quote word with a good stamp writes it under another desk
    /// epoch than the head's: a word of another epoch is empty.
    OtherEpoch,
    /// Some answer wrote a slot of the desk contract that is the front's alone (slots 0 to 3 or
    /// any slot outside the pairs' words), which covers none of the desk's listings, and no
    /// answer covered the listing.
    Foreign,
    /// Every answer that writes its quote word with a good stamp under the head's epoch names a
    /// word carried from the head block that the head does not hold (byte for byte, its firm word
    /// too where the firm bit is set): no landing in the next block writes a stamp of the head
    /// block.
    NotHeld,
    /// An answer's entries write a slot the listing owns, or a price contract it needs, with
    /// different values.
    Conflict,
    /// Every answer with its maker's update has an entry that could not be read whole, which
    /// covers no listing.
    Malformed,
    /// The answer that covers it does not write this account, a price contract the listing
    /// reads.
    Missing(Address),
    /// Slots of this account it would take are taken from another answer for an earlier listing.
    OtherAnswer(Address),
}

/// What the listings taken hold of one account, and from which answer.
#[derive(Debug, Clone)]
enum Held {
    /// The whole account: every slot the answer writes, with its balance and nonce.
    Whole(usize),
    /// These slots, each from its answer, where that answer writes it.
    Slots(BTreeMap<B256, usize>),
}

impl Held {
    /// Whether taking `slots` of the account from `answer` (the whole account where `None`) would
    /// read a slot another answer's listing holds.
    fn clashes(&self, slots: Option<&BTreeSet<B256>>, answer: usize) -> bool {
        match (self, slots) {
            (Self::Whole(other), _) => *other != answer,
            (Self::Slots(held), None) => held
                .values()
                .any(|other| *other != answer),
            (Self::Slots(held), Some(slots)) => slots.iter().any(|slot| {
                held.get(slot)
                    .is_some_and(|other| *other != answer)
            }),
        }
    }
}

/// The state override of the listings taken, and per listing wanted whether it was.
#[derive(Debug, Clone)]
pub(super) struct Taken {
    pub state: Value,
    pub listings: Vec<Result<(), NotTaken>>,
}

impl NextBlock {
    /// The state override the doors of `wanted` are asked under, and which of them it serves.
    ///
    /// An answer covers a listing only as [`Self::covering`] says: it writes the listing's quote
    /// word with a stamp the next block fills ([`fills_at`]) under the head's desk
    /// epoch, writes no slot of the desk contract that is the front's, and none of its entries
    /// disagree on a slot the listing owns, on a desk the head has not stopped. Each listing's
    /// slots come from one answer: the first, in the configured order of the URLs, that covers it.
    /// A listing is taken only where that answer also writes every account it `needs` and none of
    /// them with two entries disagreeing on a slot (a price contract is a conflict as the quote
    /// word is); it then takes from that answer its own slots of its desk contract (its quote
    /// word, and its firm word where the quote word's firm bit is set) and each price contract it
    /// needs whole, so none of its slots mixes two answers. A listing
    /// that would take a slot an earlier listing took from another answer is not taken, and the
    /// earlier keeps its slots: listings sharing a desk contract have disjoint slots and are each
    /// taken from the answer that covers them, and ones sharing a price contract only from the
    /// same answer. No other account or slot of any answer is in the override: an answer cannot
    /// move a door's quote through the front, its pool, its tokens, the desk word or another
    /// listing's slots.
    pub(super) fn take<'a>(&self, wanted: impl IntoIterator<Item = &'a Wanted>) -> Taken {
        let mut held: BTreeMap<Address, Held> = BTreeMap::new();
        let listings = wanted
            .into_iter()
            .map(|wanted| {
                let (answer, word) = self.covering(wanted)?;
                let accounts = &self.answers[answer];
                if let Some(missing) = wanted
                    .needs
                    .iter()
                    .find(|account| !accounts.contains_key(*account))
                {
                    return Err(NotTaken::Missing(*missing));
                }
                if wanted
                    .needs
                    .iter()
                    .any(|account| !accounts[account].conflicts.is_empty())
                {
                    return Err(NotTaken::Conflict);
                }
                let quote = &wanted.quote;
                let own: Option<BTreeSet<B256>> = Some(quote.claims(&word));
                let takes: Vec<(Address, Option<&BTreeSet<B256>>)> =
                    std::iter::once((quote.account, own.as_ref()))
                        .chain(
                            wanted
                                .needs
                                .iter()
                                .map(|account| (*account, None)),
                        )
                        .collect();
                if let Some((clash, _)) = takes.iter().find(|(account, slots)| {
                    held.get(account)
                        .is_some_and(|other| other.clashes(*slots, answer))
                }) {
                    return Err(NotTaken::OtherAnswer(*clash));
                }
                for (account, slots) in takes {
                    match (held.get_mut(&account), slots) {
                        (Some(Held::Whole(_)), _) => {}
                        (Some(Held::Slots(taken)), Some(slots)) => {
                            taken.extend(slots.iter().map(|slot| (*slot, answer)));
                        }
                        (_, None) => {
                            held.insert(account, Held::Whole(answer));
                        }
                        (None, Some(slots)) => {
                            held.insert(
                                account,
                                Held::Slots(
                                    slots
                                        .iter()
                                        .map(|slot| (*slot, answer))
                                        .collect(),
                                ),
                            );
                        }
                    }
                }
                Ok(())
            })
            .collect();
        let state = Value::Object(
            held.iter()
                .map(|(account, held)| {
                    let state = match held {
                        Held::Whole(answer) => self.answers[*answer][account].to_json(),
                        Held::Slots(slots) => AccountOverride {
                            state_diff: slots
                                .iter()
                                .filter_map(|(slot, answer)| {
                                    let word = self.answers[*answer]
                                        .get(account)?
                                        .state_diff
                                        .get(slot)?;
                                    Some((*slot, *word))
                                })
                                .collect(),
                            balance: None,
                            nonce: None,
                            conflicts: BTreeSet::new(),
                        }
                        .to_json(),
                    };
                    (account.to_string(), state)
                })
                .collect(),
        );
        Taken { state, listings }
    }

    /// The answer covering `wanted`, with its quote word: the first that, for the desk contract,
    /// writes no slot that is the front's alone, writes the listing's quote word with a stamp the
    /// next block fills, under the head's desk epoch, and (where the stamp is the head block's,
    /// a word carried into the next block) equal to the head's own word, and writes no slot the
    /// listing owns with
    /// two values, in this answer or any other for the block that arrived (an answer after the
    /// poll's merge window, [`MERGE_WINDOW`], is not seen until the next poll). None covers a
    /// listing of a desk the head has stopped. The head's desk word is
    /// the reader's, read from the chain; a word's own bits never stand for it.
    fn covering(&self, wanted: &Wanted) -> Result<(usize, B256), NotTaken> {
        let quote = &wanted.quote;
        if quote.head.stopped {
            return Err(NotTaken::Stopped);
        }
        // An answer whose entries disagree on a slot the listing owns drops it for this block,
        // whichever answer covers it: the builder may place either. Nothing is kept past the
        // block: a later block's maps are read afresh.
        let conflicted = self
            .answers
            .iter()
            .zip(&self.malformed)
            .any(|(accounts, malformed)| {
                !malformed &&
                    accounts
                        .get(&quote.account)
                        .is_some_and(|account| {
                            account
                                .state_diff
                                .get(&quote.quote_slot)
                                .is_some_and(|word| {
                                    account
                                        .conflicts
                                        .iter()
                                        .any(|slot| quote.claims(word).contains(slot))
                                })
                        })
            });
        if conflicted {
            return Err(NotTaken::Conflict);
        }
        let (mut foreign, mut other_epoch, mut not_held, mut stale, mut malformed) =
            (false, false, false, false, false);
        for (answer, accounts) in self.answers.iter().enumerate() {
            // An answer with an entry that could not be read whole may write anything the parser
            // lost (a desk word, a firm word, a slot of another listing): it covers none.
            if self.malformed[answer] {
                malformed = true;
                continue;
            }
            let Some(account) = accounts.get(&quote.account) else {
                continue;
            };
            if account
                .state_diff
                .keys()
                .any(|slot| !is_pair_slot(slot))
            {
                foreign = true;
                continue;
            }
            let Some(word) = account
                .state_diff
                .get(&quote.quote_slot)
            else {
                continue;
            };
            if !fills_at(word, self.number, wanted.carries) {
                stale = true;
            } else if !quote.under_head_epoch(word) {
                other_epoch = true;
            } else if !quote.holds(word, account.state_diff.get(&quote.firm_slot), self.number) {
                not_held = true;
            } else {
                return Ok((answer, *word));
            }
        }
        Err(if foreign {
            NotTaken::Foreign
        } else if other_epoch {
            NotTaken::OtherEpoch
        } else if not_held {
            NotTaken::NotHeld
        } else if stale {
            NotTaken::Stale
        } else if malformed {
            NotTaken::Malformed
        } else {
            NotTaken::NoEntry
        })
    }
}

/// A JSON-RPC answer.
#[derive(Debug, Deserialize)]
struct Answer {
    #[serde(default)]
    result: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    error: Option<Value>,
}

/// Reads the map from each of its URLs. Neither its `Debug` output nor its errors carry a URL,
/// which may carry a key.
#[derive(Clone, derive_more::Debug)]
pub(super) struct NextBlockReader {
    #[debug(skip)]
    client: Client,
    #[debug(skip)]
    urls: Vec<Url>,
    /// How the chain spaces its blocks.
    pub(super) cadence: Cadence,
}

impl NextBlockReader {
    /// A reader of `urls`, each request given `timeout`, which the feed sets under its poll's own
    /// so that one URL that hangs does not fail the poll.
    #[cfg(test)]
    pub(super) fn new(urls: Vec<Url>, timeout: Duration) -> Result<Self, FeedError> {
        Self::new_in(urls, timeout, Cadence::Slots)
    }

    /// The same on a chain spacing its blocks at `cadence`.
    pub(super) fn new_in(
        urls: Vec<Url>,
        timeout: Duration,
        cadence: Cadence,
    ) -> Result<Self, FeedError> {
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| FeedError::InvalidInput(format!("Efficient PropAMM state client: {e}")))?;
        Ok(Self { client, urls, cadence })
    }

    /// Asks every URL for its map at once, and keeps each map in the configured order of its URL.
    /// Once one URL has answered with a map carrying an entry, the others have [`MERGE_WINDOW`]
    /// more to answer: a map with no entry does not start the window, so a fast empty answer does
    /// not cut off a slower one carrying the updates. An answer that is not a map with a block
    /// number and a slot on the beacon grid is left out, and so is an entry that does not parse;
    /// the read fails only where no URL answers with a map.
    pub(super) async fn read(&self) -> Result<StateMaps, FeedError> {
        let mut pending: FuturesUnordered<_> = self
            .urls
            .iter()
            .enumerate()
            .map(|(i, url)| async move { (i, self.read_one(url).await) })
            .collect();
        let mut maps: Vec<Option<Map>> = vec![None; self.urls.len()];
        let mut failures = Vec::new();
        let mut window = None;
        loop {
            let next = match window {
                None => pending.next().await,
                Some(deadline) => tokio::time::timeout_at(deadline, pending.next())
                    .await
                    .unwrap_or_default(),
            };
            let Some((i, answer)) = next else {
                break;
            };
            match answer {
                Ok(map) => {
                    if !map.accounts.is_empty() {
                        window.get_or_insert_with(|| tokio::time::Instant::now() + MERGE_WINDOW);
                    }
                    maps[i] = Some(map);
                }
                Err(error) => {
                    debug!(url = i, %error, "state map not read");
                    failures.push(error);
                }
            }
        }
        let maps: Vec<Map> = maps.into_iter().flatten().collect();
        if maps.is_empty() {
            return Err(FeedError::Connection(format!(
                "Efficient PropAMM: no state URL answered with a map: {}",
                failures
                    .first()
                    .map_or_else(|| "none answered in time".to_string(), ToString::to_string)
            )));
        }
        Ok(StateMaps(maps))
    }

    /// One URL's map.
    async fn read_one(&self, url: &Url) -> Result<Map, FeedError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": STATE_METHOD,
            "params": [],
        });
        let response = self
            .client
            .post(url.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                FeedError::Connection(format!("Efficient PropAMM state map: {}", e.without_url()))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(FeedError::Connection(format!(
                "Efficient PropAMM state map: HTTP {status}"
            )));
        }
        let body = capped_body(response, MAX_STATE_BODY, "Efficient PropAMM state map").await?;
        let answer: Answer = serde_json::from_slice(&body)
            .map_err(|e| FeedError::Parsing(format!("Efficient PropAMM state map: {e}")))?;
        if let Some(error) = answer.error {
            return Err(FeedError::Connection(format!(
                "Efficient PropAMM state map refused: {error}"
            )));
        }
        let result = answer.result.ok_or_else(|| {
            FeedError::Parsing("Efficient PropAMM state map: no result".to_string())
        })?;
        parse_map_in(result, self.cadence)
    }
}

/// A map from a `/state` answer's result: its `blockNumber` (hex), its `slot` (a number, whose
/// start must be a Unix second) and every other key naming a `stateOverride` an entry; a key that
/// names none (a `timestamp`, say) is passed over. A slot's value, a balance and a nonce are read
/// as hex numbers, compact or padded, and forwarded re-encoded. An entry that does not parse is
/// left out, with a warning naming how many and the first reason; the rest are kept. The entries'
/// accounts are gathered per account: one answer may write an account from several entries, and
/// where two writes of the same slot (in two entries, or an account or slot named twice in one)
/// differ, the first in key order is kept and the slot is marked in conflict, which drops the
/// listing that owns it. Addresses and slot keys are normalised before they are compared; two
/// spellings of one key stay apart in the JSON and are compared, while byte-identical duplicate
/// JSON keys collapse in the JSON parse itself, the later value kept. An entry that cannot be read
/// whole is left out, and the answer is marked malformed: it covers no listing for its block (a
/// later clean answer still can), whatever the entry was meant to write.
#[cfg(test)]
fn parse_map(result: BTreeMap<String, Value>) -> Result<Map, FeedError> {
    parse_map_in(result, Cadence::Slots)
}

/// [`parse_map`] at `cadence`: on [`Cadence::Blocks`] no `slot` is read (one given is passed
/// over), the map's block's time coming from the head it follows
/// ([`StateMaps::for_block_after_in`]).
fn parse_map_in(mut result: BTreeMap<String, Value>, cadence: Cadence) -> Result<Map, FeedError> {
    let number = result
        .remove("blockNumber")
        .and_then(|number| serde_json::from_value::<U64>(number).ok())
        .ok_or_else(|| {
            FeedError::Parsing("Efficient PropAMM state map: no hex blockNumber".to_string())
        })?;
    let slot = result.remove("slot");
    let time = match cadence {
        Cadence::Slots => slot
            .and_then(|slot| slot.as_u64())
            .and_then(|slot| {
                slot.checked_mul(SLOT_SECS)?
                    .checked_add(BEACON_GENESIS)
            })
            .ok_or_else(|| {
                FeedError::Parsing(
                    "Efficient PropAMM state map: no slot on the beacon grid".to_string(),
                )
            })?,
        Cadence::Blocks { .. } => 0,
    };
    let mut accounts = Accounts::new();
    let mut dropped: Vec<(String, String)> = Vec::new();
    // Whether any entry could not be read whole: the answer then covers no listing.
    let mut malformed = false;
    for (key, entry) in result {
        // Only an object naming a `stateOverride` is an entry: any other key of the answer (a
        // `timestamp`, say) writes nothing and is passed over.
        if entry.get("stateOverride").is_none() {
            continue;
        }
        let parsed = match serde_json::from_value::<Entry>(entry) {
            Ok(parsed) => parsed,
            Err(error) => {
                malformed = true;
                dropped.push((key, error.to_string()));
                continue;
            }
        };
        let numbers = match numbers(&parsed) {
            Ok(numbers) => numbers,
            Err(reason) => {
                malformed = true;
                dropped.push((key, reason));
                continue;
            }
        };
        for ((address, state), (balance, nonce)) in parsed
            .state_override
            .into_iter()
            .zip(numbers)
        {
            if state.state_diff.is_empty() {
                continue;
            }
            let account = accounts
                .entry(address)
                .or_insert_with(|| AccountOverride {
                    state_diff: BTreeMap::new(),
                    balance: None,
                    nonce: None,
                    conflicts: BTreeSet::new(),
                });
            for (slot, Word(value)) in state.state_diff {
                match account.state_diff.entry(slot) {
                    std::collections::btree_map::Entry::Vacant(vacant) => {
                        vacant.insert(value);
                    }
                    std::collections::btree_map::Entry::Occupied(held) => {
                        if *held.get() != value {
                            account.conflicts.insert(slot);
                        }
                    }
                }
            }
            account.balance = account.balance.or(balance);
            account.nonce = account.nonce.or(nonce);
        }
    }
    if let Some((key, reason)) = dropped.first() {
        warn!(
            dropped = dropped.len(),
            first = %key,
            %reason,
            "state map entries left out"
        );
    }
    Ok(Map { number: number.to(), time, accounts, malformed })
}

/// An account's balance and nonce, where an entry gives them.
type BalanceAndNonce = (Option<U256>, Option<u64>);

/// Each account's balance and nonce where an entry gives them, in the entry's order. Fails where
/// one given is not a hex number, a nonce one of 64 bits.
fn numbers(entry: &Entry) -> Result<Vec<BalanceAndNonce>, String> {
    entry
        .state_override
        .iter()
        .map(|(account, state)| {
            let balance = given(state.balance.as_deref())
                .map(hex_number)
                .transpose()
                .map_err(|e| format!("{account}'s balance: {e}"))?;
            let nonce = given(state.nonce.as_deref())
                .map(|nonce| u64::try_from(hex_number(nonce)?).map_err(|e| format!("{nonce}: {e}")))
                .transpose()
                .map_err(|e| format!("{account}'s nonce: {e}"))?;
            Ok((balance, nonce))
        })
        .collect()
}

/// A value the map gives, where it is not empty.
fn given(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pamm::protocols::efficient_prop::{
        slots::DeskHead,
        test_support::{quote_word, FIRM_BASE, QUOTE_BASE, VECTOR_WORDS},
    };

    /// The head the tests' maps are for the block after, and its time, a slot before slot 1,000's.
    const HEAD: u64 = 9;
    const HEAD_TIME: u64 = BEACON_GENESIS + SLOT_SECS * 999;

    /// The key of pair 0's quote word.
    fn quote_key() -> String {
        B256::from(U256::from(QUOTE_BASE)).to_string()
    }

    /// A quote word of desk epoch 0 stamped with block `number`, `value` in its mantissa bits to
    /// tell answers apart.
    fn stamped(value: u8, number: u64) -> B256 {
        B256::from(
            U256::from_be_bytes(quote_word(number, 0, false, false).0) |
                (U256::from(value) << 64usize),
        )
    }

    /// A map for block `number` at slot 1,000, an entry per `(key, account, value)` writing the
    /// account's pair 0 quote word, `value` stamped with that block (a price contract's account
    /// is written at the same key: its slots are taken whole).
    fn map(number: u64, entries: &[(&str, Address, u8)]) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("blockNumber".into(), Value::from(format!("{number:#x}")));
        map.insert("slot".into(), Value::from(1_000u64));
        for (key, account, value) in entries {
            map.insert(
                (*key).to_string(),
                serde_json::json!({ "stateOverride": { account.to_string(): {
                    "stateDiff": { quote_key(): stamped(*value, number).to_string() },
                    "balance": "0x1",
                    "nonce": "0x2",
                }}}),
            );
        }
        Value::Object(map)
    }

    fn parsed(value: Value) -> Map {
        parse_map(serde_json::from_value(value).unwrap()).unwrap()
    }

    fn next_block(maps: Vec<Value>) -> NextBlock {
        StateMaps(maps.into_iter().map(parsed).collect())
            .for_block_after(HEAD, HEAD_TIME)
            .unwrap()
    }

    /// A listing at word index `i` of the desk contract `desk`, the head's desk word at epoch
    /// `epoch`, stopped or not, the head holding no word of the pair, reading no price contract.
    fn pair(desk: Address, i: u8, epoch: u32, stopped: bool) -> Wanted {
        pair_holding(desk, i, epoch, stopped, (B256::ZERO, B256::ZERO))
    }

    /// As [`pair`], the head holding the pair's quote and firm words `head`.
    fn pair_holding(desk: Address, i: u8, epoch: u32, stopped: bool, head: (B256, B256)) -> Wanted {
        Wanted {
            quote: ListingSlots {
                account: desk,
                quote_slot: B256::from(U256::from(QUOTE_BASE + u64::from(i))),
                firm_slot: B256::from(U256::from(FIRM_BASE + u64::from(i))),
                head: DeskHead { epoch, stopped },
                head_quote: head.0,
                head_firm: head.1,
            },
            carries: false,
            needs: Vec::new(),
        }
    }

    /// A listing at word index 0 of `cell`, its desk at epoch 0 and running.
    fn quote_contract(cell: Address) -> Wanted {
        pair(cell, 0, 0, false)
    }

    /// Only the maps for the block after the head are kept, whatever their order: a map behind it,
    /// or one ahead of it, as one URL's glitch or a map of another chain would be, is not used,
    /// and the others still are.
    #[test]
    fn only_the_maps_for_the_block_after_the_head_are_used() {
        let (a, b) = (Address::repeat_byte(0xa), Address::repeat_byte(0xb));
        let behind = parsed(map(HEAD, &[("old", a, 9)]));
        let ahead = parsed(map(HEAD + 1_000_000, &[("far", a, 7)]));
        let next = parsed(map(HEAD + 1, &[("desk-a", a, 1), ("desk-b", b, 1)]));

        let next = StateMaps(vec![behind, ahead, next])
            .for_block_after(HEAD, HEAD_TIME)
            .unwrap();

        assert_eq!(next.number, HEAD + 1);
        assert_eq!(next.time, BEACON_GENESIS + SLOT_SECS * 1_000);
        let taken = next.take(&[quote_contract(a), quote_contract(b)]);
        assert_eq!(taken.listings, [Ok(()), Ok(())]);
        assert_eq!(
            taken.state[a.to_string()]["stateDiff"][quote_key()],
            stamped(1, HEAD + 1).to_string()
        );
        assert!(taken.state[a.to_string()]
            .get("balance")
            .is_none());
    }

    /// A map whose slot is not the one right after the head's is not used: one at or before the
    /// head's slot, and one a slot or more later (a missed slot, or a map ahead of the wall
    /// clock). Nor is one for a head whose time is between two slots.
    #[rstest::rstest]
    #[case::at_the_heads_time(999)]
    #[case::before_it(998)]
    #[case::a_slot_late(1_001)]
    #[case::two_slots_late(1_002)]
    fn a_map_not_at_the_slot_after_the_heads_is_not_used(#[case] slot: u64) {
        let mut value = map(HEAD + 1, &[("desk", Address::repeat_byte(1), 1)]);
        value["slot"] = Value::from(slot);

        assert!(StateMaps(vec![parsed(value)])
            .for_block_after(HEAD, HEAD_TIME)
            .is_none());
        assert!(StateMaps(vec![parsed(map(HEAD + 1, &[]))])
            .for_block_after(HEAD, HEAD_TIME + 1)
            .is_none());
    }

    /// A map without its block, or whose slot has no Unix second, is no map; an entry that does
    /// not parse is left out and the rest kept.
    #[test]
    fn a_map_needs_its_block_and_slot_and_drops_bad_entries() {
        let mut no_slot = map(10, &[]);
        no_slot
            .as_object_mut()
            .unwrap()
            .remove("slot");
        let mut past_the_clock = map(10, &[]);
        past_the_clock["slot"] = Value::from(u64::MAX / SLOT_SECS);
        let mut no_block = map(10, &[]);
        no_block
            .as_object_mut()
            .unwrap()
            .remove("blockNumber");
        let good = Address::repeat_byte(1);
        let mut bad = map(10, &[("good", good, 1)]);
        bad.as_object_mut()
            .unwrap()
            .insert("bad".into(), serde_json::json!({ "stateOverride": { "0x01": {} } }));

        for value in [no_slot, past_the_clock, no_block] {
            assert!(parse_map(serde_json::from_value(value).unwrap()).is_err());
        }
        assert_eq!(
            parsed(bad)
                .accounts
                .keys()
                .collect::<Vec<_>>(),
            [&good]
        );
    }

    /// Two URLs carry the same quote contract's update under different keys: the first URL's
    /// write is taken, whatever the keys.
    #[test]
    fn the_first_urls_answer_wins_whatever_the_keys() {
        let cell = Address::repeat_byte(0xd);
        let first = map(HEAD + 1, &[("zz-update", cell, 2)]);
        let second = map(HEAD + 1, &[("aa-update", cell, 1)]);

        let taken = next_block(vec![first, second]).take(&[quote_contract(cell)]);

        assert_eq!(
            taken.state[cell.to_string()]["stateDiff"][quote_key()],
            stamped(2, HEAD + 1).to_string()
        );
    }

    /// A price contract is never mixed from two answers: the first URL writes its slots 0 and 4,
    /// the second slots 0 and 5, and the override is the first's whole.
    #[test]
    fn one_price_contract_is_never_mixed_from_two_answers() {
        let (cell, price) = (Address::repeat_byte(0xd), Address::repeat_byte(0x5));
        let entry = |key: &str, account: Address, value: u8, extra: u8| {
            (
                key.to_string(),
                serde_json::json!({ "stateOverride": { account.to_string(): { "stateDiff": {
                    quote_key(): stamped(value, HEAD + 1).to_string(),
                    B256::from(U256::from(extra)).to_string(): B256::repeat_byte(value).to_string(),
                }}}}),
            )
        };
        let answer = |value: u8, extra: u8| {
            let mut map = serde_json::Map::new();
            map.insert("blockNumber".into(), Value::from(format!("{:#x}", HEAD + 1)));
            map.insert("slot".into(), Value::from(1_000u64));
            let (key, cell_entry) = entry("a-cell", cell, value, 0);
            map.insert(key, cell_entry);
            let (key, price_entry) = entry("b-price", price, value, extra);
            map.insert(key, price_entry);
            Value::Object(map)
        };
        // The cell entry also writes slot 0, which the helper keys as the extra: strip it.
        let strip = |mut map: Value| {
            let diff = map["a-cell"]["stateOverride"][cell.to_string()]["stateDiff"]
                .as_object_mut()
                .unwrap();
            diff.remove(&B256::ZERO.to_string());
            map
        };
        let (first, second) = (strip(answer(2, 4)), strip(answer(1, 5)));
        let wanted = Wanted { needs: vec![price], ..quote_contract(cell) };

        let taken = next_block(vec![first, second]).take(&[wanted]);

        assert_eq!(taken.listings, [Ok(())]);
        let diff = taken.state[price.to_string()]["stateDiff"]
            .as_object()
            .unwrap();
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[&quote_key()], stamped(2, HEAD + 1).to_string());
        assert_eq!(
            diff[&B256::from(U256::from(4u8)).to_string()],
            B256::repeat_byte(2).to_string()
        );
    }

    /// A listing's quote contract and the price contract it reads come from one answer, the first
    /// that writes its quote contract: where that answer leaves the price contract out, the
    /// listing is not taken, even though a later answer writes both.
    #[test]
    fn a_listing_reading_a_price_contract_needs_it_in_the_same_answer() {
        let (cell, price) = (Address::repeat_byte(0xd), Address::repeat_byte(0x5));
        let first = map(HEAD + 1, &[("cell", cell, 1)]);
        let second = map(HEAD + 1, &[("cell", cell, 2), ("price", price, 2)]);
        let wanted = Wanted { needs: vec![price], ..quote_contract(cell) };

        let taken =
            next_block(vec![first.clone(), second.clone()]).take(std::slice::from_ref(&wanted));
        assert_eq!(taken.listings, [Err(NotTaken::Missing(price))]);
        assert_eq!(taken.state, serde_json::json!({}));

        let taken = next_block(vec![second, first]).take(&[wanted]);
        assert_eq!(taken.listings, [Ok(())]);
        assert_eq!(
            taken.state[price.to_string()]["stateDiff"][quote_key()],
            stamped(2, HEAD + 1).to_string()
        );
    }

    /// Only the accounts of the listings taken are in the override: another account an answer
    /// writes (the front, a pool, a token, a source the listing does not read) is not, and a price
    /// contract the listing needs is.
    #[test]
    fn only_the_listings_accounts_are_overridden() {
        let (cell, needed, front, other_source) = (
            Address::repeat_byte(0xd),
            Address::repeat_byte(0xb),
            Address::repeat_byte(0xf),
            Address::repeat_byte(0xe),
        );
        let answer = map(
            HEAD + 1,
            &[
                ("cell", cell, 1),
                ("price", needed, 1),
                ("front", front, 1),
                ("other", other_source, 1),
            ],
        );
        let wanted = Wanted { needs: vec![needed], ..quote_contract(cell) };

        let taken = next_block(vec![answer]).take(&[wanted]);

        let accounts: BTreeSet<Address> = taken
            .state
            .as_object()
            .unwrap()
            .keys()
            .map(|account| account.parse().unwrap())
            .collect();
        assert_eq!(accounts, BTreeSet::from([cell, needed]));
    }

    /// No answer writes the quote contract: not taken. Two listings sharing a price contract
    /// whose quote contracts come from different answers: the second is not taken, so the shared
    /// account is never two answers'.
    #[test]
    fn a_listing_is_not_taken_without_its_entry_or_against_an_earlier_one() {
        let (one, two, price) =
            (Address::repeat_byte(1), Address::repeat_byte(2), Address::repeat_byte(5));
        let first = map(HEAD + 1, &[("one", one, 1), ("price", price, 1)]);
        let second = map(HEAD + 1, &[("two", two, 2), ("price", price, 2)]);
        let nobody = Address::repeat_byte(9);

        let taken = next_block(vec![first, second]).take(&[
            Wanted { needs: vec![price], ..quote_contract(one) },
            Wanted { needs: vec![price], ..quote_contract(two) },
            quote_contract(nobody),
        ]);

        assert_eq!(
            taken.listings,
            [Ok(()), Err(NotTaken::OtherAnswer(price)), Err(NotTaken::NoEntry)]
        );
        assert!(taken
            .state
            .get(two.to_string())
            .is_none());
    }

    /// A price contract is taken whole with its balance and nonce, an empty one not overridden
    /// rather than set to zero; a desk contract's override carries neither.
    #[test]
    fn an_empty_balance_is_not_overridden() {
        let (cell, price) = (Address::repeat_byte(1), Address::repeat_byte(5));
        let mut value = map(HEAD + 1, &[("desk", cell, 1), ("price", price, 1)]);
        value["price"]["stateOverride"][price.to_string()]["balance"] = Value::from("");
        let wanted = Wanted { needs: vec![price], ..quote_contract(cell) };

        let taken = next_block(vec![value]).take(&[wanted]);

        let state = taken.state[price.to_string()]
            .as_object()
            .unwrap();
        assert!(!state.contains_key("balance"));
        assert_eq!(state["nonce"], "0x2");
        assert!(taken.state[cell.to_string()]
            .get("nonce")
            .is_none());
    }

    /// A key of the answer that names no `stateOverride` is not an entry, and costs the answer
    /// nothing: the builder's own sample carries a `timestamp`.
    #[test]
    fn a_key_that_is_no_entry_is_passed_over() {
        let cell = Address::repeat_byte(1);
        let mut value = map(HEAD + 1, &[("desk", cell, 1)]);
        value["timestamp"] = Value::from(1_792_115_920_127_487_452u64);
        value["venues"] = serde_json::json!([]);
        value["note"] = serde_json::json!({ "maker": "none" });

        let taken = next_block(vec![value]).take(&[quote_contract(cell)]);

        assert_eq!(taken.listings, [Ok(())]);
    }

    /// A slot's value, a balance and a nonce are read as hex numbers however they are padded, and
    /// reach the node as a 32-byte word and two compact quantities.
    #[test]
    fn numbers_are_forwarded_re_encoded() {
        let (cell, price) = (Address::repeat_byte(1), Address::repeat_byte(5));
        let mut value = map(HEAD + 1, &[("desk", cell, 1), ("price", price, 1)]);
        let account = &mut value["price"]["stateOverride"][price.to_string()];
        account["stateDiff"][quote_key()] = Value::from("0x1");
        account["balance"] = Value::from("0x0003e8");
        account["nonce"] = Value::from("0x01");
        let wanted = Wanted { needs: vec![price], ..quote_contract(cell) };

        let taken = next_block(vec![value]).take(&[wanted]);

        assert_eq!(taken.listings, [Ok(())]);
        let state = &taken.state[price.to_string()];
        assert_eq!(state["stateDiff"][quote_key()], B256::with_last_byte(1).to_string());
        assert_eq!(state["balance"], "0x3e8");
        assert_eq!(state["nonce"], "0x1");
    }

    /// A number with no `0x`, one over its width and one that is not hex leave their entry out.
    #[rstest::rstest]
    #[case::a_decimal_balance("balance", "1000")]
    #[case::a_nonce_over_64_bits("nonce", "0x10000000000000000")]
    #[case::a_bare_prefix("balance", "0x")]
    fn a_number_that_does_not_read_marks_the_answer_malformed(
        #[case] field: &str,
        #[case] text: &str,
    ) {
        let (cell, price) = (Address::repeat_byte(1), Address::repeat_byte(5));
        let mut value = map(HEAD + 1, &[("desk", cell, 1), ("price", price, 1)]);
        value["price"]["stateOverride"][price.to_string()][field] = Value::from(text);

        let taken = next_block(vec![value]).take(&[quote_contract(cell)]);

        assert_eq!(taken.listings, [Err(NotTaken::Malformed)]);
    }

    /// An answer over the cap is not read, and a URL that hangs past the reader's timeout costs
    /// the read that long and no more: the other URL's map is served.
    #[tokio::test]
    async fn a_large_or_a_hung_answer_does_not_hold_the_read() {
        use crate::snapshot_feed::http::test_support::{spawn_http_server, MockResponse};
        let answer =
            |value: Value| serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": value });
        let good = map(HEAD + 1, &[("desk", Address::repeat_byte(1), 1)]);
        let mut large = good.clone();
        large["pad"] = Value::from("0".repeat(MAX_STATE_BODY));
        let (good, large) = (answer(good).to_string(), answer(large).to_string());
        let served = good.clone();
        let hung = spawn_http_server(move |_| {
            Some(MockResponse {
                status: "200 OK",
                body: served.clone(),
                delay: Duration::from_secs(30),
            })
        })
        .await;
        let large = spawn_http_server(move |_| Some(("200 OK", large.clone()))).await;
        let good = spawn_http_server(move |_| Some(("200 OK", good.clone()))).await;
        let urls = [&hung, &large, &good]
            .map(|server| Url::parse(&server.url()).unwrap())
            .to_vec();
        let reader = NextBlockReader::new(urls, Duration::from_millis(300)).unwrap();

        let started = std::time::Instant::now();
        let maps = reader.read().await.unwrap();

        assert_eq!(maps.0.len(), 1);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    /// The block the tests' maps are for.
    const NEXT: u64 = HEAD + 1;

    /// The desk contract of the tests below, and its two listings' word indices.
    const DESK: Address = Address::repeat_byte(0xd);
    const A: u8 = 0;
    const B: u8 = 7;

    fn key(slot: u64) -> B256 {
        B256::from(U256::from(slot))
    }

    fn quote_of(i: u8) -> u64 {
        QUOTE_BASE + u64::from(i)
    }

    fn firm_of(i: u8) -> u64 {
        FIRM_BASE + u64::from(i)
    }

    /// The words one entry writes in an account, each `(slot, word)`.
    type Words<'a> = &'a [(u64, B256)];

    /// A map for the block after the head, an entry per `(key, account, words)` writing each of
    /// `words` in the account, with a balance and a nonce.
    fn writes(entries: &[(&str, Address, Words<'_>)]) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("blockNumber".into(), Value::from(format!("{NEXT:#x}")));
        map.insert("slot".into(), Value::from(1_000u64));
        for (name, account, writes) in entries {
            let diff: serde_json::Map<String, Value> = writes
                .iter()
                .map(|(slot, word)| (key(*slot).to_string(), Value::from(word.to_string())))
                .collect();
            map.insert(
                (*name).to_string(),
                serde_json::json!({ "stateOverride": { account.to_string(): {
                    "stateDiff": diff, "balance": "0x1", "nonce": "0x2",
                }}}),
            );
        }
        Value::Object(map)
    }

    /// A quote word of desk epoch 0, stamped for the next block unless `number` says otherwise.
    fn word(value: u8, number: u64) -> B256 {
        stamped(value, number)
    }

    /// The slots the override writes in `account`.
    fn diff_of(taken: &Taken, account: Address) -> BTreeMap<u64, B256> {
        taken.state[account.to_string()]["stateDiff"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(slot, word)| {
                let slot: U256 = slot.parse().unwrap();
                (slot.to(), word.as_str().unwrap().parse().unwrap())
            })
            .collect()
    }

    /// An answer covers a listing only where it writes the listing's quote word with a stamp the
    /// next block fills: that block's, or the block before's where the listing carries and the
    /// word does. One that writes the desk contract without that word carries no update of the
    /// listing, and one stamping another block is stale.
    #[rstest::rstest]
    #[case::the_next_block(Some(NEXT), false, false, true, Ok(()))]
    #[case::the_latest_carried(Some(HEAD), true, true, true, Ok(()))]
    #[case::the_latest_carried_but_the_head_holds_another_word(
        Some(HEAD),
        true,
        true,
        false,
        Err(NotTaken::NotHeld)
    )]
    #[case::the_latest_listing_not_carrying(Some(HEAD), false, true, true, Err(NotTaken::Stale))]
    #[case::the_latest_word_not_carrying(Some(HEAD), true, false, true, Err(NotTaken::Stale))]
    #[case::two_blocks_back(Some(HEAD - 1), true, true, true, Err(NotTaken::Stale))]
    #[case::a_block_ahead(Some(NEXT + 1), true, true, true, Err(NotTaken::Stale))]
    #[case::no_quote_word(None, false, false, true, Err(NotTaken::NoEntry))]
    fn an_answer_covers_a_listing_only_where_it_stamps_a_block_it_fills(
        #[case] stamp: Option<u64>,
        #[case] carries: bool,
        #[case] word_carries: bool,
        #[case] head_holds_it: bool,
        #[case] expected: Result<(), NotTaken>,
    ) {
        // Another pair's quote word is written either way: not the listing's.
        let mut written = vec![(quote_of(B), B256::repeat_byte(4))];
        written.extend(stamp.map(|stamp| (quote_of(A), quote_word(stamp, 0, word_carries, false))));
        let answer = writes(&[("desk", DESK, &written)]);

        let head_quote = match (stamp, head_holds_it) {
            (Some(stamp), true) => quote_word(stamp, 0, word_carries, false),
            _ => quote_word(HEAD - 7, 0, false, false),
        };
        let listing =
            Wanted { carries, ..pair_holding(DESK, A, 0, false, (head_quote, B256::ZERO)) };

        let taken = next_block(vec![answer]).take(&[listing]);

        assert_eq!(taken.listings, [expected]);
        assert_eq!(
            taken
                .state
                .get(DESK.to_string())
                .is_some(),
            expected.is_ok()
        );
    }

    /// A stale answer does not hold a listing back from a later one that stamps the next block:
    /// the listing is taken from the later.
    #[test]
    fn a_stale_answer_gives_way_to_a_later_fresh_one() {
        let stale = writes(&[("desk", DESK, &[(quote_of(A), word(1, HEAD - 1))])]);
        let fresh = writes(&[("desk", DESK, &[(quote_of(A), word(2, NEXT))])]);

        let taken = next_block(vec![stale, fresh]).take(&[pair(DESK, A, 0, false)]);

        assert_eq!(taken.listings, [Ok(())]);
        assert_eq!(diff_of(&taken, DESK), BTreeMap::from([(quote_of(A), word(2, NEXT))]));
    }

    /// Two listings on one desk contract, at word indices 0 and 7: an answer stamping only A's
    /// word serves A and not B, though it writes B's firm word, and the override holds A's quote
    /// word alone, with no balance or nonce.
    #[test]
    fn an_answer_stamping_one_listing_on_a_desk_serves_it_and_not_the_other() {
        let answer = writes(&[(
            "a",
            DESK,
            &[
                (quote_of(A), word(1, NEXT)),
                (firm_of(A), B256::repeat_byte(1)),
                (firm_of(B), B256::repeat_byte(1)),
            ],
        )]);

        let taken =
            next_block(vec![answer]).take(&[pair(DESK, A, 0, false), pair(DESK, B, 0, false)]);

        assert_eq!(taken.listings, [Ok(()), Err(NotTaken::NoEntry)]);
        assert_eq!(diff_of(&taken, DESK), BTreeMap::from([(quote_of(A), word(1, NEXT))]));
        let state = taken.state[DESK.to_string()]
            .as_object()
            .unwrap();
        assert!(!state.contains_key("balance") && !state.contains_key("nonce"));
    }

    /// The firm word is taken only where the quote word's bit 205 is set, and then from the answer
    /// that covers the listing: the vectors' slot 7 record, with its firm section.
    #[test]
    fn the_firm_word_is_taken_only_under_bit_205() {
        let (i, epoch, hex, number) = VECTOR_WORDS[1];
        let firm: B256 = "00000005ffff00000000000000000064000000000000000f0000000000000000"
            .parse()
            .unwrap();
        let quote: B256 = hex.parse().unwrap();
        let mut cleared = U256::from_be_bytes(quote.0);
        cleared.set_bit(205, false);
        let listing = |epoch| Wanted { carries: false, ..pair(DESK, i, epoch, false) };
        let answer = |quote: B256| {
            let mut map = writes(&[("b", DESK, &[(quote_of(i), quote), (firm_of(i), firm)])]);
            map["blockNumber"] = Value::from(format!("{number:#x}"));
            map
        };
        let next = |map: Value| {
            StateMaps(vec![parsed(map)])
                .for_block_after(number - 1, BEACON_GENESIS + SLOT_SECS * 999)
                .unwrap()
        };

        let with = next(answer(quote)).take(&[listing(epoch)]);
        let without = next(answer(B256::from(cleared))).take(&[listing(epoch)]);

        assert_eq!(with.listings, [Ok(())]);
        assert_eq!(
            diff_of(&with, DESK),
            BTreeMap::from([(quote_of(i), quote), (firm_of(i), firm)])
        );
        assert_eq!(without.listings, [Ok(())]);
        assert_eq!(diff_of(&without, DESK), BTreeMap::from([(quote_of(i), B256::from(cleared))]));
    }

    /// A stale stamp on pair A is not served, and pair B on the same desk contract, stamped for the
    /// next block in the same answer, is.
    #[test]
    fn a_stale_stamp_on_one_listing_on_a_desk_leaves_the_other_served() {
        let answer = writes(&[(
            "desk",
            DESK,
            &[(quote_of(A), word(1, HEAD - 1)), (quote_of(B), word(2, NEXT))],
        )]);

        let taken =
            next_block(vec![answer]).take(&[pair(DESK, A, 0, false), pair(DESK, B, 0, false)]);

        assert_eq!(taken.listings, [Err(NotTaken::Stale), Ok(())]);
        assert_eq!(diff_of(&taken, DESK), BTreeMap::from([(quote_of(B), word(2, NEXT))]));
    }

    /// A stop at the head darkens every listing of the desk, whatever the answers write: the
    /// answer's words are well stamped under the head's epoch.
    #[test]
    fn a_stop_at_the_head_darkens_both_listings_of_a_desk() {
        let answer = writes(&[(
            "desk",
            DESK,
            &[(quote_of(A), word(1, NEXT)), (quote_of(B), word(2, NEXT))],
        )]);

        let taken =
            next_block(vec![answer]).take(&[pair(DESK, A, 0, true), pair(DESK, B, 0, true)]);

        assert_eq!(taken.listings, [Err(NotTaken::Stopped), Err(NotTaken::Stopped)]);
        assert_eq!(taken.state, serde_json::json!({}));
    }

    /// The stop is the head's, never the pair word's: a pair word's bit 63 is always 0, so a word
    /// that is well stamped under the epoch the head holds shows no stop, and a stop at the head
    /// still darkens it.
    #[test]
    fn a_stop_shows_in_the_head_and_never_in_the_pair_word() {
        let word = quote_word(NEXT, 5, false, false);
        let answer = writes(&[("desk", DESK, &[(quote_of(A), word)])]);
        assert!(!U256::from_be_bytes(word.0).bit(63));

        let running = next_block(vec![answer.clone()]).take(&[pair(DESK, A, 5, false)]);
        let stopped = next_block(vec![answer]).take(&[pair(DESK, A, 5, true)]);

        assert_eq!(running.listings, [Ok(())]);
        assert_eq!(stopped.listings, [Err(NotTaken::Stopped)]);
    }

    /// A revive after a stop raises the head's epoch: a pair word written under the epoch before
    /// is empty, though well stamped for the block with bit 63 clear, and one under the new epoch
    /// is covered. Listings of one desk share the head's epoch.
    #[test]
    fn a_word_of_the_epoch_before_a_revive_is_empty() {
        let before = quote_word(NEXT, 5, false, false);
        let after = quote_word(NEXT, 6, false, false);
        let answer = writes(&[("desk", DESK, &[(quote_of(A), before), (quote_of(B), after)])]);

        let taken =
            next_block(vec![answer]).take(&[pair(DESK, A, 6, false), pair(DESK, B, 6, false)]);

        assert_eq!(taken.listings, [Err(NotTaken::OtherEpoch), Ok(())]);
        assert_eq!(diff_of(&taken, DESK), BTreeMap::from([(quote_of(B), after)]));
    }

    /// All 31 bits of the epoch count: the epochs 16,847,216 and 4,464 agree in their low 16 bits,
    /// and a word of one is empty under a head at the other, both ways.
    #[rstest::rstest]
    #[case::word_big_head_small(16_847_216, 4_464)]
    #[case::word_small_head_big(4_464, 16_847_216)]
    fn epochs_agreeing_in_their_low_16_bits_are_not_one_epoch(
        #[case] word_epoch: u32,
        #[case] head_epoch: u32,
    ) {
        assert_eq!(word_epoch & 0xffff, head_epoch & 0xffff);
        let answer =
            writes(&[("desk", DESK, &[(quote_of(A), quote_word(NEXT, word_epoch, false, false))])]);

        let taken = next_block(vec![answer.clone()]).take(&[pair(DESK, A, head_epoch, false)]);
        let same = next_block(vec![answer]).take(&[pair(DESK, A, word_epoch, false)]);

        assert_eq!(taken.listings, [Err(NotTaken::OtherEpoch)]);
        assert_eq!(same.listings, [Ok(())]);
    }

    /// The vectors' three records are each covered at their own pair slot (0, 7 and 255), under
    /// their own desk epoch (0, 3 and 16,847,216) and block, with exactly their own words in the
    /// override, and are not covered at another slot of the desk or under another desk epoch.
    #[test]
    fn the_vectors_records_are_covered_at_their_slots_and_epochs() {
        for (index, (i, epoch, hex, number)) in VECTOR_WORDS.into_iter().enumerate() {
            let quote: B256 = hex.parse().unwrap();
            let mut map = writes(&[("record", DESK, &[(quote_of(i), quote)])]);
            map["blockNumber"] = Value::from(format!("{number:#x}"));
            let next = || {
                StateMaps(vec![parsed(map.clone())])
                    .for_block_after(number - 1, BEACON_GENESIS + SLOT_SECS * 999)
                    .unwrap()
            };
            let other_slot = i ^ 1;
            let other_epoch = VECTOR_WORDS[(index + 1) % 3].1;

            let taken = next().take(&[Wanted { carries: false, ..pair(DESK, i, epoch, false) }]);
            let elsewhere = next().take(&[pair(DESK, other_slot, epoch, false)]);
            let under_another = next().take(&[pair(DESK, i, other_epoch, false)]);

            assert_eq!(taken.listings, [Ok(())], "slot {i}");
            assert!(diff_of(&taken, DESK)[&quote_of(i)] == quote);
            assert!(diff_of(&taken, DESK)
                .keys()
                .all(|slot| *slot == quote_of(i)));
            assert_eq!(elsewhere.listings, [Err(NotTaken::NoEntry)], "slot {i}");
            assert_eq!(under_another.listings, [Err(NotTaken::OtherEpoch)], "slot {i}");
        }
    }

    /// An answer that writes a slot of the desk contract that is the front's, slots 0 to 3 or one
    /// outside the pairs' words, covers none of the desk's listings, though it stamps both pairs'
    /// words well; a write to another pair's words does not.
    #[rstest::rstest]
    #[case::slot_0(0)]
    #[case::the_desk_word(1)]
    #[case::the_registry(2)]
    #[case::the_word_key(3)]
    #[case::past_the_firm_words(0x300)]
    #[case::between(0xff)]
    fn an_answer_writing_a_desk_level_slot_covers_none_of_the_desks_listings(#[case] slot: u64) {
        let answer = writes(&[(
            "desk",
            DESK,
            &[
                (quote_of(A), word(1, NEXT)),
                (quote_of(B), word(2, NEXT)),
                (slot, B256::repeat_byte(9)),
            ],
        )]);

        let taken =
            next_block(vec![answer]).take(&[pair(DESK, A, 0, false), pair(DESK, B, 0, false)]);

        assert_eq!(taken.listings, [Err(NotTaken::Foreign), Err(NotTaken::Foreign)]);
        assert_eq!(taken.state, serde_json::json!({}));
    }

    /// A desk-level write in one answer does not darken a listing a later clean answer covers, and
    /// another desk contract's answer is unaffected.
    #[test]
    fn a_foreign_write_costs_only_its_own_answer() {
        let (other, third) = (Address::repeat_byte(0xe), Address::repeat_byte(0xf));
        let bad =
            writes(&[("desk", DESK, &[(quote_of(A), word(1, NEXT)), (3, B256::repeat_byte(9))])]);
        let clean = writes(&[
            ("desk", DESK, &[(quote_of(A), word(2, NEXT))]),
            ("other", other, &[(quote_of(A), word(2, NEXT))]),
        ]);
        let also_bad =
            writes(&[("third", third, &[(quote_of(A), word(3, NEXT)), (1, B256::ZERO)])]);

        let taken = next_block(vec![bad, clean, also_bad]).take(&[
            pair(DESK, A, 0, false),
            pair(other, A, 0, false),
            pair(third, A, 0, false),
        ]);

        assert_eq!(taken.listings, [Ok(()), Ok(()), Err(NotTaken::Foreign)]);
        assert_eq!(diff_of(&taken, DESK), BTreeMap::from([(quote_of(A), word(2, NEXT))]));
    }

    /// A lane's write sits in the registry's account until a fill copies it: an answer that writes
    /// only the registry's account, whatever it holds, covers no listing.
    #[test]
    fn a_write_to_the_registrys_account_covers_no_listing() {
        let registry = Address::repeat_byte(0xda);
        let answer = writes(&[(
            "lane",
            registry,
            &[(quote_of(A), word(1, NEXT)), (0, B256::repeat_byte(1))],
        )]);

        let taken = next_block(vec![answer]).take(&[pair(DESK, A, 0, false)]);

        assert_eq!(taken.listings, [Err(NotTaken::NoEntry)]);
        assert_eq!(taken.state, serde_json::json!({}));
    }

    /// A conflict in any answer for the block drops the listing for that block, even where another
    /// answer for it is clean, whichever comes first: the builder may place either.
    #[test]
    fn a_conflict_in_any_answer_for_the_block_drops_the_listing_whatever_the_order() {
        let clean = writes(&[("desk", DESK, &[(quote_of(A), word(1, NEXT))])]);
        let conflicting = writes(&[
            ("a-1", DESK, &[(quote_of(A), word(2, NEXT))]),
            ("a-2", DESK, &[(quote_of(A), word(3, NEXT))]),
        ]);

        let clean_first =
            next_block(vec![clean.clone(), conflicting.clone()]).take(&[pair(DESK, A, 0, false)]);
        let conflicting_first =
            next_block(vec![conflicting, clean]).take(&[pair(DESK, A, 0, false)]);

        assert_eq!(clean_first.listings, [Err(NotTaken::Conflict)]);
        assert_eq!(conflicting_first.listings, [Err(NotTaken::Conflict)]);
        assert_eq!(clean_first.state, serde_json::json!({}));
    }

    /// The drop lasts one target block: nothing is kept, and a clean answer for a later target
    /// covers the listing again.
    #[test]
    fn a_conflict_drops_the_listing_for_one_target_block_only() {
        let conflicting = writes(&[
            ("a-1", DESK, &[(quote_of(A), word(2, NEXT))]),
            ("a-2", DESK, &[(quote_of(A), word(3, NEXT))]),
        ]);
        let mut later = writes(&[("desk", DESK, &[(quote_of(A), word(4, NEXT + 1))])]);
        later["blockNumber"] = Value::from(format!("{:#x}", NEXT + 1));
        later["slot"] = Value::from(1_001u64);
        let listing = pair(DESK, A, 0, false);

        let dropped = next_block(vec![conflicting]).take(std::slice::from_ref(&listing));
        let after = StateMaps(vec![parsed(later)])
            .for_block_after(HEAD + 1, HEAD_TIME + SLOT_SECS)
            .unwrap()
            .take(&[listing]);

        assert_eq!(dropped.listings, [Err(NotTaken::Conflict)]);
        assert_eq!(after.listings, [Ok(())]);
    }

    /// Two entries of one answer writing the same slot with different values drop the listing
    /// that owns it, and leave the other listing on the desk served; the same value twice is no
    /// conflict. A later clean answer does not cover the dropped listing.
    #[test]
    fn two_entries_writing_one_slot_differently_drop_that_listing() {
        let first = word(1, NEXT);
        let second = word(2, NEXT);
        let conflicting = writes(&[
            ("a-1", DESK, &[(quote_of(A), first), (quote_of(B), word(5, NEXT))]),
            ("a-2", DESK, &[(quote_of(A), second)]),
        ]);
        let clean = writes(&[("desk", DESK, &[(quote_of(A), first)])]);
        let same = writes(&[
            ("a-1", DESK, &[(quote_of(A), first)]),
            ("a-2", DESK, &[(quote_of(A), first)]),
        ]);
        let listings = [pair(DESK, A, 0, false), pair(DESK, B, 0, false)];

        let taken = next_block(vec![conflicting.clone()]).take(&listings);
        let after_a_clean_one = next_block(vec![conflicting, clean]).take(&listings);
        let twice = next_block(vec![same]).take(&listings[..1]);

        assert_eq!(taken.listings, [Err(NotTaken::Conflict), Ok(())]);
        assert_eq!(diff_of(&taken, DESK), BTreeMap::from([(quote_of(B), word(5, NEXT))]));
        assert_eq!(after_a_clean_one.listings, [Err(NotTaken::Conflict), Ok(())]);
        assert_eq!(twice.listings, [Ok(())]);
    }

    /// A conflict on the firm word costs the listing only where its quote word counts the firm
    /// word (bit 205).
    #[rstest::rstest]
    #[case::firm_counts(true, Err(NotTaken::Conflict))]
    #[case::firm_does_not(false, Ok(()))]
    fn a_conflict_on_the_firm_word_matters_only_under_bit_205(
        #[case] firm: bool,
        #[case] expected: Result<(), NotTaken>,
    ) {
        let answer = writes(&[
            (
                "a-1",
                DESK,
                &[
                    (quote_of(A), quote_word(NEXT, 0, false, firm)),
                    (firm_of(A), B256::repeat_byte(1)),
                ],
            ),
            ("a-2", DESK, &[(firm_of(A), B256::repeat_byte(2))]),
        ]);

        let taken = next_block(vec![answer]).take(&[pair(DESK, A, 0, false)]);

        assert_eq!(taken.listings, [expected]);
    }

    /// Two answers disagree on pair A's word: A is taken from the first, and pair B on the same
    /// desk, stamped only by the second, from the second, so the override holds each pair's slots
    /// from one answer. A third listing bound to A's slot that only the second answer covers (A
    /// carries, the third does not) clashes with A there, and is the one not taken.
    #[test]
    fn a_conflict_on_one_listings_slots_on_a_desk_leaves_the_other_served() {
        let carried = quote_word(HEAD, 0, true, false);
        let first = writes(&[("a", DESK, &[(quote_of(A), carried)])]);
        let second = writes(&[(
            "desk",
            DESK,
            &[(quote_of(A), word(2, NEXT)), (quote_of(B), word(2, NEXT))],
        )]);
        let a = Wanted { carries: true, ..pair_holding(DESK, A, 0, false, (carried, B256::ZERO)) };
        let shares_a = pair(DESK, A, 0, false);
        let b = pair(DESK, B, 0, false);

        let taken = next_block(vec![first, second]).take(&[a, shares_a, b]);

        assert_eq!(taken.listings, [Ok(()), Err(NotTaken::OtherAnswer(DESK)), Ok(())]);
        assert_eq!(
            diff_of(&taken, DESK),
            BTreeMap::from([(quote_of(A), carried), (quote_of(B), word(2, NEXT))])
        );
    }

    /// Two entries of one answer posting different prints to one price contract (the normal shape
    /// of a maker with two pairs on one source) are a conflict on the price contract: every listing
    /// that needs it is not taken from that answer.
    #[test]
    fn two_entries_disagreeing_on_a_price_contract_slot_drop_the_listings_that_need_it() {
        let price = Address::repeat_byte(0x5);
        let (pa, pb) = (B256::repeat_byte(0xa1), B256::repeat_byte(0xb2));
        let entry = |pair: u8, quote: B256, print: B256| {
            serde_json::json!({ "stateOverride": {
                DESK.to_string(): { "stateDiff": { key(quote_of(pair)).to_string(): quote.to_string() } },
                price.to_string(): { "stateDiff": { key(0).to_string(): print.to_string() } },
            }})
        };
        let mut map = writes(&[]);
        map["entry-a"] = entry(A, word(1, NEXT), pa);
        map["entry-b"] = entry(B, word(2, NEXT), pb);
        let listings = [
            Wanted { needs: vec![price], ..pair(DESK, A, 0, false) },
            Wanted { needs: vec![price], ..pair(DESK, B, 0, false) },
        ];

        let taken = next_block(vec![map]).take(&listings);

        assert_eq!(taken.listings, [Err(NotTaken::Conflict), Err(NotTaken::Conflict)]);
        assert_eq!(taken.state, serde_json::json!({}));
    }

    /// The same print posted twice is no conflict.
    #[test]
    fn two_entries_posting_one_print_to_a_price_contract_are_served() {
        let price = Address::repeat_byte(0x5);
        let print = B256::repeat_byte(0xa1);
        let entry = |pair: u8, quote: B256| {
            serde_json::json!({ "stateOverride": {
                DESK.to_string(): { "stateDiff": { key(quote_of(pair)).to_string(): quote.to_string() } },
                price.to_string(): { "stateDiff": { key(0).to_string(): print.to_string() } },
            }})
        };
        let mut map = writes(&[]);
        map["entry-a"] = entry(A, word(1, NEXT));
        map["entry-b"] = entry(B, word(2, NEXT));
        let listings = [
            Wanted { needs: vec![price], ..pair(DESK, A, 0, false) },
            Wanted { needs: vec![price], ..pair(DESK, B, 0, false) },
        ];

        let taken = next_block(vec![map]).take(&listings);

        assert_eq!(taken.listings, [Ok(()), Ok(())]);
    }

    /// The malformed entries a builder might send: a balance that is not hex, a nonce given as a
    /// JSON number, a slot key that is not 32 bytes (a compact "0x1"), a stateOverride that is not
    /// an object, an address that does not parse.
    fn malformed_entries() -> Vec<Value> {
        let other = Address::repeat_byte(0xe).to_string();
        let write = |slot: B256, word: B256| {
            serde_json::json!({ DESK.to_string(): {
                "stateDiff": { slot.to_string(): word.to_string() },
            }})
        };
        let with = |mut accounts: Value, name: &str, state: Value| {
            accounts[name] = state;
            serde_json::json!({ "stateOverride": accounts })
        };
        vec![
            with(
                write(key(1), B256::repeat_byte(0xff)),
                &other,
                serde_json::json!({ "stateDiff": {}, "balance": "0xnot-hex" }),
            ),
            with(
                write(key(1), B256::repeat_byte(0xff)),
                &other,
                serde_json::json!({ "stateDiff": {}, "nonce": 2 }),
            ),
            serde_json::json!({ "stateOverride": { DESK.to_string(): { "stateDiff": {
                "0x1": B256::repeat_byte(0xff).to_string(),
            }}}}),
            serde_json::json!({ "stateOverride": { DESK.to_string(): { "stateDiff": {
                key(1).to_string(): "0xnot-a-word",
            }}}}),
            serde_json::json!({ "stateOverride": "not an object" }),
            serde_json::json!({ "stateOverride": { "not-an-address": { "stateDiff": {} } } }),
        ]
    }

    /// An answer with any entry the parser cannot read whole covers no listing for its block,
    /// whatever that entry was meant to write: a desk word (slot 1) under a numeric nonce or a
    /// compact slot key, a firm word under bit 205, another listing's slot. It fails closed.
    #[rstest::rstest]
    #[case::a_balance_not_hex(0)]
    #[case::a_numeric_nonce(1)]
    #[case::a_compact_slot_key(2)]
    #[case::a_value_that_is_not_a_word(3)]
    #[case::a_state_override_that_is_no_object(4)]
    #[case::an_address_that_does_not_parse(5)]
    fn an_answer_with_an_entry_left_out_covers_no_listing(#[case] which: usize) {
        let mut map = writes(&[("a-1", DESK, &[(quote_of(A), word(1, NEXT))])]);
        map["a-2"] = malformed_entries().remove(which);

        let taken = next_block(vec![map]).take(&[pair(DESK, A, 0, false)]);

        assert_eq!(taken.listings, [Err(NotTaken::Malformed)]);
        assert_eq!(taken.state, serde_json::json!({}));
    }

    /// A left-out desk word write is not Foreign as the same entry parsed would be, and a left-out
    /// firm write raises no conflict, yet neither listing is served: the answer is malformed.
    #[test]
    fn a_left_out_desk_word_or_firm_write_darkens_the_listing_like_a_parsed_one() {
        let desk_word = |nonce: Value| {
            let mut map = writes(&[("a-1", DESK, &[(quote_of(A), word(1, NEXT))])]);
            map["a-2"] = serde_json::json!({ "stateOverride": {
                DESK.to_string(): { "stateDiff": { key(1).to_string(): B256::repeat_byte(0xff).to_string() } },
                Address::repeat_byte(0xe).to_string(): { "stateDiff": {}, "nonce": nonce },
            }});
            next_block(vec![map]).take(&[pair(DESK, A, 0, false)])
        };
        let firm = |nonce: Value| {
            let mut map =
                writes(&[("a-1", DESK, &[(quote_of(A), quote_word(NEXT, 0, false, true))])]);
            map["a-2"] = serde_json::json!({ "stateOverride": {
                DESK.to_string(): { "stateDiff": { key(firm_of(A)).to_string(): B256::repeat_byte(2).to_string() } },
                Address::repeat_byte(0xe).to_string(): { "stateDiff": {}, "nonce": nonce },
            }});
            next_block(vec![map]).take(&[pair(DESK, A, 0, false)])
        };

        assert_eq!(desk_word(serde_json::json!(2)).listings, [Err(NotTaken::Malformed)]);
        assert_eq!(desk_word(serde_json::json!("0x2")).listings, [Err(NotTaken::Foreign)]);
        assert_eq!(firm(serde_json::json!(2)).listings, [Err(NotTaken::Malformed)]);
        assert_eq!(firm(serde_json::json!("0x2")).listings, [Ok(())]);
    }

    /// A malformed answer costs coverage for its own block only where no other answer covers: a
    /// later clean answer for the same block covers the listing, and the next block's maps are
    /// read afresh.
    #[test]
    fn a_clean_answer_still_covers_where_another_for_the_block_is_malformed() {
        let mut malformed = writes(&[("a-1", DESK, &[(quote_of(A), word(1, NEXT))])]);
        malformed["a-2"] = malformed_entries().remove(1);
        let clean = writes(&[("a", DESK, &[(quote_of(A), word(2, NEXT))])]);

        let first = next_block(vec![malformed.clone(), clean]).take(&[pair(DESK, A, 0, false)]);
        let alone = next_block(vec![malformed]).take(&[pair(DESK, A, 0, false)]);

        assert_eq!(first.listings, [Ok(())]);
        assert_eq!(diff_of(&first, DESK), BTreeMap::from([(quote_of(A), word(2, NEXT))]));
        assert_eq!(alone.listings, [Err(NotTaken::Malformed)]);
    }

    /// One entry naming the desk contract twice (checksummed and lower case), or one slot twice
    /// (two spellings of its key), with different values is a conflict: the spellings normalise to
    /// one address and one key, and neither value silently replaces the other.
    #[test]
    fn one_entry_naming_an_address_or_a_slot_twice_conflicts_on_different_values() {
        let (checksummed, lower) = (DESK.to_string(), format!("{DESK:#x}"));
        assert_ne!(checksummed, lower);
        let mut twice = writes(&[]);
        twice["a"] = serde_json::json!({ "stateOverride": {
            checksummed: { "stateDiff": { key(quote_of(A)).to_string(): word(1, NEXT).to_string() } },
            lower: { "stateDiff": { key(quote_of(A)).to_string(): word(2, NEXT).to_string() } },
        }});
        let (upper, plain) = (
            format!("{:#x}", key(quote_of(255)))
                .to_uppercase()
                .replace("0X", "0x"),
            format!("{:#x}", key(quote_of(255))),
        );
        assert_ne!(upper, plain);
        let mut slot_twice = writes(&[]);
        slot_twice["a"] = serde_json::json!({ "stateOverride": {
            DESK.to_string(): { "stateDiff": {
                upper: word(1, NEXT).to_string(),
                plain: word(2, NEXT).to_string(),
            }},
        }});
        let same = {
            let mut same = writes(&[]);
            same["a"] = serde_json::json!({ "stateOverride": {
                DESK.to_string(): { "stateDiff": { key(quote_of(A)).to_string(): word(1, NEXT).to_string() } },
                format!("{DESK:#x}"): { "stateDiff": { key(quote_of(A)).to_string(): word(1, NEXT).to_string() } },
            }});
            same
        };

        let address = next_block(vec![twice]).take(&[pair(DESK, A, 0, false)]);
        let slot = next_block(vec![slot_twice]).take(&[pair(DESK, 255, 0, false)]);
        let equal = next_block(vec![same]).take(&[pair(DESK, A, 0, false)]);

        assert_eq!(address.listings, [Err(NotTaken::Conflict)]);
        assert_eq!(slot.listings, [Err(NotTaken::Conflict)]);
        assert_eq!(equal.listings, [Ok(())]);
    }

    /// A listing's backup source is never read by its fill, so a backup's price contract is not
    /// among what a listing takes, and cannot push out another listing whose live source it is.
    #[test]
    fn a_backup_source_does_not_darken_the_listing_whose_live_source_it_is() {
        let (y_desk, p634, p8) =
            (Address::repeat_byte(0xe), Address::repeat_byte(0x61), Address::repeat_byte(0x68));
        let print = B256::repeat_byte(8);
        let z_entries: [(&str, Address, Words<'_>); 2] =
            [("z", DESK, &[(quote_of(A), word(1, NEXT))]), ("z-print", p8, &[(0, print)])];
        let first = writes(&z_entries);
        let y_word = [(quote_of(A), word(2, NEXT))];
        let y_print = [(0, print)];
        let mut both: Vec<(&str, Address, Words<'_>)> =
            vec![("y", y_desk, &y_word), ("y-print", p634, &y_print)];
        both.extend(z_entries);
        let second = writes(&both);
        let y = Wanted { needs: vec![p634], ..pair(y_desk, A, 0, false) };
        let z = Wanted { needs: vec![p8], ..pair(DESK, A, 0, false) };

        let taken = next_block(vec![first, second]).take(&[y, z]);

        assert_eq!(taken.listings, [Ok(()), Ok(())]);
        // Y's backup (P8, written by the second answer too) is not Y's to take: P8 is Z's, from
        // the first answer.
        assert_eq!(diff_of(&taken, p8), BTreeMap::from([(0, print)]));
    }

    /// A word stamped with the head block, carried into the next, covers the listing only where
    /// it is the head's own word byte for byte: a different word, an older one or none held is not
    /// taken from the answer, and a later answer that is fresh still covers the listing.
    #[test]
    fn a_carried_word_is_taken_only_where_the_head_holds_it() {
        let carried = quote_word(HEAD, 0, true, false);
        let other = quote_word(HEAD, 0, true, true);
        let fresh = word(5, NEXT);
        let carried_answer = writes(&[("a", DESK, &[(quote_of(A), carried)])]);
        let fresh_answer = writes(&[("a", DESK, &[(quote_of(A), fresh)])]);
        let holds = |head: B256| Wanted {
            carries: true,
            ..pair_holding(DESK, A, 0, false, (head, B256::ZERO))
        };

        let held = next_block(vec![carried_answer.clone()]).take(&[holds(carried)]);
        let another = next_block(vec![carried_answer.clone()]).take(&[holds(other)]);
        let none = next_block(vec![carried_answer.clone()]).take(&[holds(B256::ZERO)]);
        let then_fresh = next_block(vec![carried_answer, fresh_answer]).take(&[holds(other)]);

        assert_eq!(held.listings, [Ok(())]);
        assert_eq!(another.listings, [Err(NotTaken::NotHeld)]);
        assert_eq!(none.listings, [Err(NotTaken::NotHeld)]);
        assert_eq!(then_fresh.listings, [Ok(())]);
        assert_eq!(diff_of(&then_fresh, DESK), BTreeMap::from([(quote_of(A), fresh)]));
    }

    /// Where the carried word's firm bit is set, the answer's firm word, if it writes one, must
    /// also be the head's; an answer that writes none leaves the chain's, which is the head's.
    #[rstest::rstest]
    #[case::the_same_firm_word(Some(1), Ok(()))]
    #[case::another_firm_word(Some(2), Err(NotTaken::NotHeld))]
    #[case::no_firm_word_written(None, Ok(()))]
    fn a_carried_word_with_a_firm_bit_needs_the_heads_firm_word_too(
        #[case] firm: Option<u8>,
        #[case] expected: Result<(), NotTaken>,
    ) {
        let carried = quote_word(HEAD, 0, true, true);
        let head_firm = B256::repeat_byte(1);
        let mut written = vec![(quote_of(A), carried)];
        written.extend(firm.map(|firm| (firm_of(A), B256::repeat_byte(firm))));
        let answer = writes(&[("a", DESK, &written)]);
        let listing =
            Wanted { carries: true, ..pair_holding(DESK, A, 0, false, (carried, head_firm)) };

        let taken = next_block(vec![answer]).take(&[listing]);

        assert_eq!(taken.listings, [expected]);
    }

    /// A word stamped for the next block is the answer's own landing, and needs nothing of the
    /// head.
    #[test]
    fn a_word_stamped_for_the_next_block_needs_nothing_of_the_head() {
        let answer = writes(&[("a", DESK, &[(quote_of(A), word(1, NEXT))])]);
        let listing = pair_holding(DESK, A, 0, false, (B256::repeat_byte(9), B256::repeat_byte(9)));

        let taken = next_block(vec![answer]).take(&[listing]);

        assert_eq!(taken.listings, [Ok(())]);
    }
}
