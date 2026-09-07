//! Grouping of directional per-pair entries into one book per token pair.
//!
//! Two-sided providers publish a pair in either or both orientations (WETH/USDC and USDC/WETH
//! over the same inventory). Emitting each orientation as its own component would advertise the
//! same liquidity twice, so such providers group the entries by unordered pair and settle on one
//! orientation per pair.

use std::collections::{HashMap, HashSet};

use tycho_common::Bytes;

/// Every published entry of one token pair, plus the orientation the pair is emitted in.
pub(crate) struct PairGroup<T> {
    /// The pair's addresses in ascending order, independent of how the provider oriented its
    /// entries; the stable identity of the pair.
    pub token0: Bytes,
    pub token1: Bytes,
    /// The orientation the pair is emitted in; see [`canonical_orientation`].
    pub base: Bytes,
    pub quote: Bytes,
    pub entries: Vec<T>,
}

/// The orientation a pair is emitted in: the USD quote token on the quote side when exactly one
/// of the two is one, otherwise the address-sorted order (`token0` base, `token1` quote). The
/// choice depends only on the pair and the quote-token set, never on which orientation a provider
/// happened to publish, so a pair keeps its orientation across polls.
pub(crate) fn canonical_orientation(
    token0: Bytes,
    token1: Bytes,
    usd_quote_tokens: &HashSet<Bytes>,
) -> (Bytes, Bytes) {
    let (token0, token1) = sort_pair(token0, token1);
    match (usd_quote_tokens.contains(&token0), usd_quote_tokens.contains(&token1)) {
        (true, false) => (token1, token0),
        (false, true) | (true, true) | (false, false) => (token0, token1),
    }
}

fn sort_pair(a: Bytes, b: Bytes) -> (Bytes, Bytes) {
    if a.as_ref() <= b.as_ref() {
        (a, b)
    } else {
        (b, a)
    }
}

/// Groups `entries` by unordered token pair. `pair_of` returns an entry's published
/// `(base, quote)`. Groups come back keyed by their sorted pair.
pub(crate) fn group_by_pair<T>(
    entries: impl IntoIterator<Item = T>,
    pair_of: impl Fn(&T) -> (Bytes, Bytes),
    usd_quote_tokens: &HashSet<Bytes>,
) -> HashMap<(Bytes, Bytes), PairGroup<T>> {
    let mut groups: HashMap<(Bytes, Bytes), PairGroup<T>> = HashMap::new();
    for entry in entries {
        let (base, quote) = pair_of(&entry);
        let key = sort_pair(base, quote);
        groups
            .entry(key.clone())
            .or_insert_with(|| {
                let (base, quote) =
                    canonical_orientation(key.0.clone(), key.1.clone(), usd_quote_tokens);
                PairGroup { token0: key.0, token1: key.1, base, quote, entries: Vec::new() }
            })
            .entries
            .push(entry);
    }
    groups
}

/// Groups `entries` by unordered pair and keeps one entry per pair: the one published in the
/// group's orientation when both orientations are present, otherwise the single entry as it was
/// published. For providers whose entries carry both sides of the book, so that the mirrored
/// orientation adds no liquidity.
pub(crate) fn drop_mirrored<T>(
    entries: impl IntoIterator<Item = T>,
    pair_of: impl Fn(&T) -> (Bytes, Bytes),
    usd_quote_tokens: &HashSet<Bytes>,
) -> Vec<T> {
    group_by_pair(entries, &pair_of, usd_quote_tokens)
        .into_values()
        .filter_map(|group| {
            let PairGroup { base, quote, mut entries, .. } = group;
            if entries.len() == 1 {
                return entries.pop();
            }
            let canonical = entries.iter().position(|entry| {
                let (entry_base, entry_quote) = pair_of(entry);
                entry_base == base && entry_quote == quote
            });
            // Several entries in the same orientation are the provider's bug; the first one
            // stands in for the pair.
            Some(entries.swap_remove(canonical.unwrap_or(0)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn addr(last_byte: u8) -> Bytes {
        Bytes::from(vec![last_byte; 20])
    }

    #[rstest]
    #[case::token0_is_quote(true, false, false)]
    #[case::token1_is_quote(false, true, true)]
    #[case::both_are_quotes(true, true, true)]
    #[case::neither_is_quote(false, false, true)]
    fn orientation_prefers_the_single_quote_token_and_falls_back_to_address_order(
        #[case] token0_is_quote: bool,
        #[case] token1_is_quote: bool,
        #[case] expected_quote_is_token1: bool,
    ) {
        let token0 = addr(0x11);
        let token1 = addr(0x22);
        let mut quotes = HashSet::new();
        if token0_is_quote {
            quotes.insert(token0.clone());
        }
        if token1_is_quote {
            quotes.insert(token1.clone());
        }

        let (base, quote) = canonical_orientation(token1.clone(), token0.clone(), &quotes);

        if expected_quote_is_token1 {
            assert_eq!((base, quote), (token0, token1));
        } else {
            assert_eq!((base, quote), (token1, token0));
        }
    }

    #[test]
    fn drop_mirrored_keeps_the_canonical_orientation_of_a_mirrored_pair() {
        let weth = addr(0x22);
        let usdc = addr(0x11);
        let quotes = HashSet::from([usdc.clone()]);
        let entries = vec![
            (usdc.clone(), weth.clone(), "usdc/weth"),
            (weth.clone(), usdc.clone(), "weth/usdc"),
        ];

        let kept = drop_mirrored(entries, |(b, q, _)| (b.clone(), q.clone()), &quotes);

        assert_eq!(kept, vec![(weth, usdc, "weth/usdc")]);
    }

    #[test]
    fn drop_mirrored_keeps_a_single_orientation_as_published() {
        let weth = addr(0x22);
        let usdc = addr(0x11);
        let quotes = HashSet::from([usdc.clone()]);
        // Published against the canonical orientation, but alone: kept untouched.
        let entries = vec![(usdc.clone(), weth.clone(), "usdc/weth")];

        let kept = drop_mirrored(entries, |(b, q, _)| (b.clone(), q.clone()), &quotes);

        assert_eq!(kept, vec![(usdc, weth, "usdc/weth")]);
    }
}
