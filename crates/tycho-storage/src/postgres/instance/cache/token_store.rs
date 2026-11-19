//! # How data is laid out
//!
//! For every chain there is one [`ChainTokenStore`] holding all of that chain's
//! tokens in a single `Vec`, in the same order the SQL path returns them
//! (ascending `token.id`). Tokens are only ever appended, never removed, so a
//! token's *position* in this `Vec` is a small stable number that identifies it —
//! position 7 always means the same token. All filtering works on these positions
//! instead of on addresses, which matters because comparing positions is a single
//! integer comparison while comparing addresses means following a pointer to
//! 20 heap bytes.
//!
//! Two lookup structures sit next to the `Vec`:
//!
//! - `idx_by_address`: a `HashMap` from token address to position, for requests that ask for
//!   specific addresses.
//! - `quality_index`: for each quality value (0–100), the set of positions having that quality. The
//!   sets are `RoaringBitmap`s — think of a bitmap as one bit per position ("is token #7 in this
//!   set? look at bit 7"), and "roaring" as a standard trick to keep such bitmaps small and fast.
//!   The useful property: set operations work on 64 positions at a time (whole machine words), so
//!   "all tokens with quality between 51 and 100" is built by OR-ing a handful of bitmaps, and
//!   counting matches is nearly free.
//!
//! The "recently traded" filter needs no index at all: `last_traded` is a plain
//! `Vec<i64>` with one timestamp per position, and the filter is one integer
//! comparison per candidate while iterating. Scanning even 5M entries takes a few
//! milliseconds, and most queries scan far less because the quality bitmap
//! already narrowed the candidates. A per-timestamp index would be faster still
//! but needs pruning to stay bounded; the flat array cannot grow beyond one entry
//! per token.
//!
//! Answering a query then is: build the candidate set from the filters (a couple
//! of bitmap operations), walk it in position order (= `token.id` order, so
//! pagination is stable), count everything for the `total` field, and clone only
//! the tokens on the requested page.
use std::{
    collections::{BTreeMap, HashMap},
    ops::Bound,
    sync::Arc,
};

use chrono::NaiveDateTime;
use roaring::RoaringBitmap;
use tycho_common::{
    models::{protocol::QualityRange, token::Token, Address, Chain, PaginationParams},
    storage::{StorageError, WithTotal},
};

use crate::postgres::{convert_int, count_to_i64, page_window};

/// Number of rows fetched per query during the initial full load.
pub const LOAD_BATCH_SIZE: i64 = 500_000;

/// Timestamp value for tokens that never appeared in a component balance.
/// `i64::MIN` sorts below any real threshold, matching the SQL `EXISTS` filter
/// which excludes such tokens.
pub const NEVER_TRADED: i64 = i64::MIN;

/// How far behind the sync marker each
/// [`TokenCache::refresh`](crate::postgres::token_cache::TokenCache::refresh) re-reads, to pick
/// up writes whose transaction was still open during an earlier poll.
pub const REFRESH_OVERLAP_SECS: i64 = 600;

#[derive(Debug, Clone)]
pub struct TokenQuery {
    pub(crate) chain: Chain,
    pub(crate) addresses: Option<Vec<Address>>,
    pub(crate) quality_range: QualityRange,
    pub(crate) last_traded_ts_threshold: Option<NaiveDateTime>,
    pub(crate) pagination: Option<PaginationParams>,
}

#[derive(Default)]
pub struct ChainTokenStore {
    /// All tokens of the chain in ascending `token.id` order. Append-only.
    pub(crate) tokens: Vec<Arc<Token>>,
    /// Token address -> position in `tokens`.
    pub(crate) idx_by_address: HashMap<Address, u32>,
    /// Quality value -> positions of tokens with that quality.
    quality_index: BTreeMap<i32, RoaringBitmap>,
    /// Last traded timestamp (unix micros) per position, `NEVER_TRADED` if none.
    pub(crate) last_traded: Vec<i64>,
}

impl ChainTokenStore {
    /// Inserts or updates a token. With `overwrite` false an existing entry is left
    /// untouched, mirroring the `ON CONFLICT DO NOTHING` semantics of the token
    /// insert statement.
    pub(crate) fn upsert(&mut self, token: Token, overwrite: bool) -> Result<(), StorageError> {
        match self
            .idx_by_address
            .get(&token.address)
            .copied()
        {
            Some(idx) => {
                if !overwrite {
                    return Ok(());
                }
                let old = &self.tokens[idx as usize];
                if old.quality != token.quality {
                    let old_quality: i32 = convert_int(old.quality, "token quality")?;
                    let new_quality: i32 = convert_int(token.quality, "token quality")?;
                    if let Some(bitmap) = self.quality_index.get_mut(&old_quality) {
                        bitmap.remove(idx);
                    }
                    self.quality_index
                        .entry(new_quality)
                        .or_default()
                        .insert(idx);
                }
                self.tokens[idx as usize] = Arc::new(token);
            }
            None => {
                let idx: u32 = convert_int(self.tokens.len(), "token position")?;
                let quality: i32 = convert_int(token.quality, "token quality")?;
                self.idx_by_address
                    .insert(token.address.clone(), idx);
                self.quality_index
                    .entry(quality)
                    .or_default()
                    .insert(idx);
                self.tokens.push(Arc::new(token));
                self.last_traded.push(NEVER_TRADED);
            }
        }
        Ok(())
    }

    pub(crate) fn update_last_traded(&mut self, address: &Address, ts: NaiveDateTime) {
        if let Some(&idx) = self.idx_by_address.get(address) {
            let ts = ts.and_utc().timestamp_micros();
            let current = &mut self.last_traded[idx as usize];
            *current = (*current).max(ts);
        }
    }

    /// Evaluates a query against the store.
    ///
    /// Results are in ascending `token.id` order and `total` counts all matches
    /// regardless of pagination, matching the SQL implementation of `get_tokens`.
    ///
    /// The four match arms below are the same algorithm with shortcuts applied
    /// where a filter is absent:
    /// - address/quality filters, no traded filter: the candidate bitmap alone is the answer — its
    ///   size is `total` and the page is cut straight from it.
    /// - with a traded filter: walk the candidates once, keeping a running count and grabbing the
    ///   rows that fall inside the requested page.
    /// - no filters at all: `total` is the store size and the page is a plain slice of the token
    ///   vector.
    pub(crate) fn query(&self, query: &TokenQuery) -> Result<WithTotal<Vec<Token>>, StorageError> {
        let candidates = self.candidate_bitmap(query);
        let ts_threshold = query
            .last_traded_ts_threshold
            .map(|ts| ts.and_utc().timestamp_micros());

        let (offset, limit) = query
            .pagination
            .as_ref()
            .map(page_window)
            .transpose()?
            .unwrap_or((0, usize::MAX));

        let page = match (candidates, ts_threshold) {
            (Some(bitmap), None) => {
                let total: i64 = convert_int(bitmap.len(), "candidate count")?;
                let entity = bitmap
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|idx| (*self.tokens[idx as usize]).clone())
                    .collect();
                WithTotal { entity, total: Some(total) }
            }
            (Some(bitmap), Some(threshold)) => self.paginate_filtered(
                bitmap.iter().map(|idx| idx as usize),
                threshold,
                offset,
                limit,
            ),
            (None, None) => {
                let total = count_to_i64(self.tokens.len());
                let entity = self
                    .tokens
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|token| (**token).clone())
                    .collect();
                WithTotal { entity, total: Some(total) }
            }
            (None, Some(threshold)) => {
                self.paginate_filtered(0..self.tokens.len(), threshold, offset, limit)
            }
        };
        Ok(page)
    }

    /// Combines the address and quality filters into a single set of candidate
    /// positions. `None` means "no filter" (all tokens are candidates).
    ///
    /// The address filter becomes a set via hashmap lookups (addresses not in the
    /// store are simply dropped, like a SQL `IN` list with unknown values). The
    /// quality filter is the union of the per-quality sets in the requested range
    /// — at most 101 unions, since quality is 0–100. When both filters are given,
    /// the answer is their intersection.
    pub(crate) fn candidate_bitmap(&self, query: &TokenQuery) -> Option<RoaringBitmap> {
        let mut candidates: Option<RoaringBitmap> = query
            .addresses
            .as_ref()
            .map(|addresses| {
                addresses
                    .iter()
                    .filter_map(|address| {
                        self.idx_by_address
                            .get(address)
                            .copied()
                    })
                    .collect()
            });

        let quality_bounds = (query.quality_range.min, query.quality_range.max);
        if quality_bounds != (None, None) {
            let lower = quality_bounds
                .0
                .map_or(Bound::Unbounded, Bound::Included);
            let upper = quality_bounds
                .1
                .map_or(Bound::Unbounded, Bound::Included);
            let mut quality_bitmap = RoaringBitmap::new();
            for (_, bitmap) in self.quality_index.range((lower, upper)) {
                quality_bitmap |= bitmap;
            }
            candidates = Some(match candidates {
                Some(address_bitmap) => address_bitmap & quality_bitmap,
                None => quality_bitmap,
            });
        }

        candidates
    }

    /// Single pass over candidate positions applying the last-traded filter:
    /// counts every match (for `total`) and clones only the rows whose running
    /// index falls inside the requested page.
    pub(crate) fn paginate_filtered(
        &self,
        positions: impl Iterator<Item = usize>,
        threshold: i64,
        offset: usize,
        limit: usize,
    ) -> WithTotal<Vec<Token>> {
        let mut total = 0usize;
        let mut entity = Vec::new();
        for pos in positions {
            if self.last_traded[pos] <= threshold {
                continue;
            }
            if total >= offset && entity.len() < limit {
                entity.push((*self.tokens[pos]).clone());
            }
            total += 1;
        }
        WithTotal { entity, total: Some(count_to_i64(total)) }
    }
}
