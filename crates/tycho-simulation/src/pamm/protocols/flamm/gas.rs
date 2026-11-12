//! What a swap of an Efficient pool is charged in gas, and what it adds to a transaction's gas
//! limit.
//!
//! The charge is the gas inside the pool's own `swap()`, the part Tycho's gas guide asks a
//! protocol to report (`docs/for-dexs/protocol-integration/simulation/README.md:108`), net of the
//! refund the swap's transaction gets back, and never more than the fill costs in the state the
//! feed read. It is the least base, in a table generated from measured fills (`gas_table.rs`),
//! over the cells the feed's reads allow. A cell is the class of the hook that prices the pair
//! ([`hook_class`]), the chain, the venue kind, the side, how the fill pays its output and whether
//! it repays a debt in its input. The reads are ones the feed makes anyway: the pool's venue
//! records at discovery, each one's kind, status and tokens and whether it lets the pool repay
//! now, and at each poll the legs the sampler reads beside a walk, the pool's custody, supply,
//! posted collateral and debt in the pair's two tokens, which count as read where the walk's band
//! word is not zero. With the legs:
//! - a payout within the output's custody may take any path, one past it calls a venue for the
//!   output, and one past what the venues hold of the output, and 1/10,000 more for their supply
//!   interest until the fill, as well borrows it;
//! - a borrow may be a first one where the output owes nothing or more than one live venue lends
//!   it, and an owing one where the output owes or the input has collateral posted: a first borrow
//!   against collateral posted ahead may post nothing, as an owing one may, and costs no less;
//! - an input that owes nothing repays nothing, and one that owes repays where every venue that
//!   lends it is live and lets the pool repay now, and, where one of them holds a minimum position
//!   (Lista Moolah's), it lends the input alone and the input owes more than that minimum with a
//!   margin.
//!
//! A fill on a state a swap returned is another fill of the same route, in the same transaction,
//! on what the fill before it moved and warmed: it is charged its key's second-fill charge, the
//! least measured of the second of two fills of one pool in one transaction, which the reads,
//! moved by the fill before, do not narrow, and never more than the least base of its cells. So is
//! every fill on a pool of three or more tokens, as a fill on one of its pairs moves the others'
//! states unseen. A fill whose walks read no legs and one whose reads allow no cell take the least
//! base of their cells.
//!
//! A pair is served only where every live venue on it is of a kind its chain routes, with cells, a
//! second-fill charge and a limit for the pair's class, chain and side: not with a live venue of
//! any other kind, with a venue record that did not read, or with no live venue. Limits are worst
//! case instead: [`gas_limit_addition`].
use alloy::primitives::{keccak256, B256, U256};
use serde::{Deserialize, Serialize};
use tycho_common::models::Chain;

use super::{
    gas_table::{CELLS, LIMITS, SECOND_FILLS},
    ladder::Side,
};

const PUSH1: u8 = 0x60;
const PUSH32: u8 = 0x7f;

/// What the pool frees of an output before a draw grows with its venues' supply interest between
/// the read and the fill, so a payout past it by up to 1/`FREED_SLACK` of it may still be paid
/// with no borrow.
const FREED_SLACK: u64 = 10_000;

/// The venue kinds each chain routes, by chain id: those with cells in the table whose guards the
/// feed builds, 0 (Morpho Blue) and 1 (Aave V3) on Ethereum and Base, 1, 2 (Venus Core) and 3
/// (Lista Moolah, whose minimum position [`super::guard`] keeps off the ladders) on BSC. A pair
/// with a live venue of another kind is not served there.
const ROUTED_KINDS: [(u64, &[u8]); 3] = [(1, &[0, 1]), (8453, &[0, 1]), (56, &[1, 2, 3])];
/// How far over a minimum position an owing venue's debt must stay for a draw or a partial repay
/// to pass it (`RELEASE_SLACK`).
pub(super) const RELEASE_SLACK: u64 = 4;

/// A walk's legs: the input's held, supplied, posted and owed, then the output's.
pub(super) type Legs = [U256; 8];

/// How a fill pays its output, from the venue calls its trace shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Payout {
    /// No call moves the output out of a venue: custody pays it.
    Held,
    /// A call withdraws or releases the output from a venue, and none borrows it.
    Freed,
    /// A call borrows the output, which owed nothing before the fill.
    FirstBorrow,
    /// A call borrows the output, which owed before the fill.
    OwingBorrow,
}

/// One cell of the measured table: the fills of one class, chain, venue kind and side that pay
/// their output one way and repay a debt in their input or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Cell {
    /// The class ([`hook_class`]) of the hook that prices the pair.
    pub class: B256,
    /// The chain's id.
    pub chain: u64,
    /// The kind of lender behind the venue (`MMRouter.venue(pool, id).kind`).
    pub kind: u8,
    pub side: Side,
    pub payout: Payout,
    /// A call repays a debt in the input.
    pub repays: bool,
    /// The least gas measured over the cell's fills, net of the refund each transaction got back,
    /// less 1 %, rounded down to 1,000.
    pub base: u64,
}

/// What a fill of one class, chain, venue kind and side adds to a transaction's gas limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Limit {
    pub class: B256,
    pub chain: u64,
    pub kind: u8,
    pub side: Side,
    pub addition: u64,
}

/// What a fill of one class, chain, venue kind and side is charged on a state a swap returned:
/// the second of two fills of one pool in one transaction, either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SecondFill {
    pub class: B256,
    pub chain: u64,
    pub kind: u8,
    /// The second fill's own side.
    pub side: Side,
    /// The least gas measured over such second fills, net of the refund each earned, less 1 %,
    /// rounded down to 1,000.
    pub base: u64,
}

/// What one swap of an Efficient pool adds to a transaction's gas limit: an addition, not a limit,
/// for a solver to add to the limit of the rest of its route. It covers the dearest fill measured
/// of its key, on any path, in any state and to a fresh receiver, with the router's own work
/// around the pool's call, all before refunds, times 1.05 x 1.10. `side_sells_token_0` is whether
/// the swap sells the pool's token 0, its asset. `None` for a key with no measured fill, which the
/// feed does not serve. A state's own is [`super::state::FlammPairState::gas_limit_addition`].
pub fn gas_limit_addition(
    chain: Chain,
    class: B256,
    kind: u8,
    side_sells_token_0: bool,
) -> Option<u64> {
    let chain = chain.try_id().ok()?;
    let side = if side_sells_token_0 { Side::Sale } else { Side::Buy };
    LIMITS
        .iter()
        .find(|l| l.class == class && l.chain == chain && l.kind == kind && l.side == side)
        .map(|l| l.addition)
}

/// The class of a hook's runtime code: its hash with every `PUSH32` immediate zeroed and the
/// compiler's metadata tail cut off. Solidity writes each immutable as a `PUSH32`, so two hooks
/// built from one source with different immutables (their pool, their tokens) share a class.
///
/// A class fixes the code around those immediates, not the immediates: anyone can deploy a
/// contract that differs from a hook only in a 32-byte constant, and it shares the hook's class.
/// So list only a class whose quote and fill run the same code, whatever its constants: one like
/// that may price badly, but it pays what it quotes.
pub fn hook_class(code: &[u8]) -> B256 {
    let mut masked = without_metadata(code).to_vec();
    let mut pc = 0;
    while pc < masked.len() {
        let op = masked[pc];
        if (PUSH1..=PUSH32).contains(&op) {
            let width = usize::from(op - PUSH1) + 1;
            let end = (pc + 1 + width).min(masked.len());
            if op == PUSH32 {
                masked[pc + 1..end].fill(0);
            }
            pc = end;
        } else {
            pc += 1;
        }
    }
    keccak256(&masked)
}

/// `code` less the CBOR map solc appends to it, whose length its last two bytes give; the whole
/// of `code` where its tail reads as no such map.
fn without_metadata(code: &[u8]) -> &[u8] {
    let Some(tail) = code.len().checked_sub(2) else { return code };
    let length = usize::from(u16::from_be_bytes([code[tail], code[tail + 1]]));
    let Some(start) = tail.checked_sub(length) else { return code };
    match code[start] {
        // a CBOR map of one to three entries: `ipfs`/`bzzr`, `solc`, `experimental`
        0xa1..=0xa3 if length > 0 => &code[..start],
        _ => code,
    }
}

/// A venue record of a pool, as discovery reads it for the charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Venue {
    /// The kind of lender behind it.
    pub kind: u8,
    /// Its status is zero: it is neither retired nor fenced.
    pub live: bool,
    /// The pool's indices of its credit pair's collateral token and debt token.
    pub coll: u8,
    pub debt: u8,
    /// It lets the pool repay now (`MMRouter.venueActions` has `CAN_REPAY`); false where that was
    /// not read or did not read.
    pub repays: bool,
    /// Its account holds a partial repay at a minimum position (`capabilities()` has
    /// `CAP_HELD_REPAY`), and that minimum, `minimumDebt(id)`, where it answered ok.
    pub held_repay: Option<Minimum>,
}

/// A venue's minimum position, where its account holds one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Minimum {
    /// `minimumDebt(id)` answered ok with this floor.
    Read(U256),
    /// It answered not ok, which a venue that lends nothing does, or did not read.
    Unread,
}

/// A minimum with the margin a fill that lands after the read needs, as the venue's minimum moves
/// with its oracle: 1 % over, rounded up.
pub(super) fn with_margin(floor: U256) -> U256 {
    floor.saturating_add(floor.div_ceil(U256::from(100u64)))
}

impl Venue {
    fn touches(&self, a: u8, b: u8) -> bool {
        [self.coll, self.debt]
            .iter()
            .any(|t| *t == a || *t == b)
    }
}

/// A pool's venue records, and whether one went unread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Venues {
    pub records: Vec<Venue>,
    /// The pool has more records than discovery reads.
    pub unread: bool,
}

/// The cells, second-fill charges, limit additions and routed kinds a feed charges by.
#[derive(Clone, Debug)]
pub(super) struct GasTable {
    cells: Vec<Cell>,
    limits: Vec<Limit>,
    second_fills: Vec<SecondFill>,
    routed: Vec<(u64, Vec<u8>)>,
}

impl Default for GasTable {
    /// The generated table, and the kinds each chain routes.
    fn default() -> Self {
        Self {
            cells: CELLS.to_vec(),
            limits: LIMITS.to_vec(),
            second_fills: SECOND_FILLS.to_vec(),
            routed: ROUTED_KINDS
                .iter()
                .map(|(chain, kinds)| (*chain, kinds.to_vec()))
                .collect(),
        }
    }
}

impl GasTable {
    #[cfg(test)]
    pub(super) fn new(
        cells: Vec<Cell>,
        limits: Vec<Limit>,
        second_fills: Vec<SecondFill>,
        routed: Vec<(u64, Vec<u8>)>,
    ) -> Self {
        Self { cells, limits, second_fills, routed }
    }

    /// What the charge of a swap selling a pool's token `token_in` for its token `token_out`
    /// reads at discovery, on `chain`, where the hook that prices the pair is of `class`. `None`
    /// where the pair is not served that way: a venue record went unread, or no venue on the pair
    /// is live, or a live one is of a kind the chain does not route or with no cells, second-fill
    /// charge or limit for the class, chain and side. A venue on the pair is one whose collateral
    /// or debt token is either token.
    pub(super) fn key(
        &self,
        chain: u64,
        class: B256,
        token_in: u8,
        token_out: u8,
        venues: &Venues,
    ) -> Option<GasKey> {
        if venues.unread {
            return None;
        }
        let side = if token_in == 0 { Side::Sale } else { Side::Buy };
        let (_, routed) = self
            .routed
            .iter()
            .find(|(id, _)| *id == chain)?;
        let live: Vec<&Venue> = venues
            .records
            .iter()
            .filter(|v| v.live && v.touches(token_in, token_out))
            .collect();
        if live.is_empty() {
            return None;
        }
        let mut cells: Vec<Cell> = Vec::new();
        let mut limit_addition = 0;
        let mut second = u64::MAX;
        for kind in live.iter().map(|v| v.kind) {
            if cells.iter().any(|c| c.kind == kind) {
                continue;
            }
            if !routed.contains(&kind) {
                return None;
            }
            let found = cells.len();
            cells.extend(
                self.cells
                    .iter()
                    .filter(|c| {
                        c.class == class && c.chain == chain && c.kind == kind && c.side == side
                    })
                    .copied(),
            );
            let limit = self.limits.iter().find(|l| {
                l.class == class && l.chain == chain && l.kind == kind && l.side == side
            })?;
            let second_fill = self.second_fills.iter().find(|s| {
                s.class == class && s.chain == chain && s.kind == kind && s.side == side
            })?;
            if cells.len() == found {
                return None;
            }
            limit_addition = limit_addition.max(limit.addition);
            second = second.min(second_fill.base);
        }
        Some(GasKey {
            cells,
            limit_addition,
            second,
            lenders_out: live
                .iter()
                .filter(|v| v.debt == token_out)
                .count(),
            repay: Repay::of(venues, token_in),
        })
    }

    /// The table with every kind it has cells for routed on every chain it has cells on, so a
    /// check can charge each measured fill whatever the routed lists say.
    #[cfg(test)]
    pub(super) fn routing_every_kind(mut self) -> Self {
        let mut routed: Vec<(u64, Vec<u8>)> = Vec::new();
        for cell in &self.cells {
            match routed
                .iter_mut()
                .find(|(chain, _)| *chain == cell.chain)
            {
                Some((_, kinds)) if kinds.contains(&cell.kind) => {}
                Some((_, kinds)) => kinds.push(cell.kind),
                None => routed.push((cell.chain, vec![cell.kind])),
            }
        }
        self.routed = routed;
        self
    }
}

/// What one direction's charge reads at discovery: its cells, the most it adds to a limit, its
/// second-fill charge, and what the venue records say of its tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GasKey {
    /// The cells of its class, chain and side, of every live venue kind on its pair.
    cells: Vec<Cell>,
    /// The most a fill this way adds to a transaction's limit, over those kinds.
    limit_addition: u64,
    /// The least second-fill charge of its class, chain and side over those kinds, as a second
    /// fill may draw on any of them.
    second: u64,
    /// The live venues that lend the output.
    lenders_out: usize,
    /// Whether a fill into an owing input repays.
    repay: Repay,
}

/// Whether a fill into an owing input repays, from the venues that lend the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Repay {
    /// No venue lends the input, or one that does is not live, does not let the pool repay now or
    /// holds a minimum position beside another lender or with a minimum that did not read: the
    /// repay can be passed over.
    Unsure,
    /// Every venue that lends the input is live and lets the pool repay now.
    Sure,
    /// As `Sure`, the one venue that lends the input holding a minimum position: sure where the
    /// input owes more than this, the minimum with its margin and the release slack.
    Over(U256),
}

impl Repay {
    fn of(venues: &Venues, token_in: u8) -> Self {
        let lending: Vec<&Venue> = venues
            .records
            .iter()
            .filter(|v| v.debt == token_in)
            .collect();
        if lending.is_empty() ||
            !lending
                .iter()
                .all(|v| v.live && v.repays)
        {
            return Self::Unsure;
        }
        let held: Vec<Minimum> = lending
            .iter()
            .filter_map(|v| v.held_repay)
            .collect();
        match (held.as_slice(), lending.len()) {
            ([], _) => Self::Sure,
            ([Minimum::Read(floor)], 1) => {
                Self::Over(with_margin(*floor).saturating_add(U256::from(RELEASE_SLACK)))
            }
            _ => Self::Unsure,
        }
    }

    fn sure(self, owed_in: U256) -> bool {
        match self {
            Self::Unsure => false,
            Self::Sure => true,
            Self::Over(edge) => owed_in > edge,
        }
    }
}

/// How far a payout reaches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Within the output's custody.
    Custody,
    /// Past custody, within what the venues hold of the output as well.
    Venues,
    /// Past both: the pool borrows the output.
    Borrow,
}

impl GasKey {
    /// The charge of a fill this way: by its payout, from `legs`, where one of the pair's walks
    /// read them, and the least base of its cells where none did, on a pool of two tokens; its
    /// second-fill charge, at most that least base, on a state a swap returned and on every fill of
    /// a pool of more tokens.
    pub(super) fn side_gas(&self, legs: Option<&Legs>, two_tokens: bool) -> SideGas {
        let least = self
            .cells
            .iter()
            .map(|c| c.base)
            .min()
            .unwrap_or_default();
        let second = self.second.min(least);
        if !two_tokens {
            return SideGas {
                least: second,
                second,
                steps: None,
                limit_addition: self.limit_addition,
            };
        }
        let steps = legs.map(|legs| {
            let [_, _, posted_in, owed_in, held_out, supplied_out, posted_out, owed_out] = *legs;
            let charge = |reach: Reach| {
                self.cells
                    .iter()
                    .filter(|c| self.allows(c, reach, owed_in, posted_in, owed_out))
                    .map(|c| c.base)
                    .min()
                    .unwrap_or(least)
            };
            let freed = held_out
                .saturating_add(supplied_out)
                .saturating_add(posted_out);
            Steps {
                held_out,
                freed_top: freed.saturating_add(freed / U256::from(FREED_SLACK)),
                within_custody: charge(Reach::Custody),
                past_custody: charge(Reach::Venues),
                past_freed: charge(Reach::Borrow),
            }
        });
        SideGas { least, second, steps, limit_addition: self.limit_addition }
    }

    /// Whether a fill whose payout reaches `reach`, with the input owing `owed_in` and posting
    /// `posted_in` as collateral and the output owing `owed_out`, can take a path of `cell`. A
    /// first borrow against collateral posted ahead posts only what the borrow needs past it, often
    /// nothing, unlike every measured first borrow, so it may cost less than their cells; it costs
    /// no less than an owing borrow, which writes no debt from zero, so it may take owing cells.
    fn allows(
        &self,
        cell: &Cell,
        reach: Reach,
        owed_in: U256,
        posted_in: U256,
        owed_out: U256,
    ) -> bool {
        let pays = match cell.payout {
            Payout::Held => reach == Reach::Custody,
            Payout::Freed => reach != Reach::Borrow,
            Payout::FirstBorrow => owed_out.is_zero() || self.lenders_out > 1,
            Payout::OwingBorrow => !owed_out.is_zero() || !posted_in.is_zero(),
        };
        let takes =
            if owed_in.is_zero() { !cell.repays } else { cell.repays || !self.repay.sure(owed_in) };
        pays && takes
    }
}

/// What a swap on one side of a pair is charged, by how far its payout reaches, and what it adds to
/// a transaction's gas limit; all zero on a side that quotes nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SideGas {
    /// The charge where the legs do not place a fill: the least base of the side's cells, or on a
    /// pool of more than two tokens its second-fill charge.
    least: u64,
    /// The charge on a state a swap returned.
    second: u64,
    /// The charge by the payout's reach, where the legs were read on a pool of two tokens.
    steps: Option<Steps>,
    limit_addition: u64,
}

/// A side's charge where its legs were read: within the output's custody, past it, and past what
/// the venues hold of the output as well.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Steps {
    held_out: U256,
    /// The output's custody, supply and posted collateral, and 1/`FREED_SLACK` more.
    freed_top: U256,
    within_custody: u64,
    past_custody: u64,
    past_freed: u64,
}

impl SideGas {
    /// A side charged `gas` on every fill, adding `limit_addition` to a limit.
    #[cfg(test)]
    pub(super) fn flat(gas: u64, limit_addition: u64) -> Self {
        Self { least: gas, second: gas, steps: None, limit_addition }
    }

    /// The gas a fill paying `payout` is charged: the second-fill charge on a state a swap
    /// returned, whatever it pays.
    pub(super) fn charge(&self, payout: U256, moved: bool) -> u64 {
        if moved {
            return self.second;
        }
        let Some(steps) = self.steps else { return self.least };
        if payout <= steps.held_out {
            steps.within_custody
        } else if payout <= steps.freed_top {
            steps.past_custody
        } else {
            steps.past_freed
        }
    }

    pub(super) fn limit_addition(&self) -> u64 {
        self.limit_addition
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::{collections::HashMap, env, fs};

    use alloy::primitives::b256;
    use rstest::rstest;

    use super::*;

    /// `PUSH32 <word> PUSH1 0x01 STOP` followed by a one-entry metadata map of `len` bytes.
    fn code(word: u8, metadata: &[u8]) -> Vec<u8> {
        let mut code = vec![PUSH32];
        code.extend([word; 32]);
        code.extend([PUSH1, 0x01, 0x00]);
        code.extend(metadata);
        code.extend(
            u16::try_from(metadata.len())
                .unwrap()
                .to_be_bytes(),
        );
        code
    }

    #[test]
    fn immutables_and_metadata_do_not_change_the_class() {
        let a = code(0x11, &[0xa1, 0x64, b's', b'o', b'l', b'c']);
        let b = code(0x22, &[0xa1, 0x64, b's', b'o', b'l', b'd']);
        assert_eq!(hook_class(&a), hook_class(&b));
    }

    #[test]
    fn other_code_is_another_class() {
        let a = code(0x11, &[0xa1, 0x00]);
        let mut b = a.clone();
        b[34] = 0x02; // the PUSH1 immediate, which is code, not an immutable
        assert_ne!(hook_class(&a), hook_class(&b));
    }

    #[test]
    fn a_truncated_push_or_empty_code_still_hashes() {
        assert_eq!(hook_class(&[]), keccak256([]));
        assert_eq!(hook_class(&[PUSH32, 0x01]), keccak256([PUSH32, 0x00]));
    }

    /// Every key with a measured fill has a cell paid from custody that repays nothing, a
    /// second-fill charge and a limit addition over its dearest base: a fill that makes no venue
    /// call and one after another fill can always be charged, and every fill is limited at or over
    /// what it costs.
    #[test]
    fn every_key_has_a_custody_cell_a_second_fill_and_a_limit_over_its_cells() {
        let mut keys: Vec<(B256, u64, u8, Side)> = Vec::new();
        for cell in CELLS {
            let key = (cell.class, cell.chain, cell.kind, cell.side);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        assert!(!keys.is_empty());
        for (class, chain, kind, side) in keys {
            let cells: Vec<&Cell> = CELLS
                .iter()
                .filter(|c| {
                    c.class == class && c.chain == chain && c.kind == kind && c.side == side
                })
                .collect();
            assert!(
                cells
                    .iter()
                    .any(|c| c.payout == Payout::Held && !c.repays),
                "no custody cell for {chain} {kind} {side:?}"
            );
            assert!(cells.iter().all(|c| c.base > 0));
            let second = SECOND_FILLS
                .iter()
                .find(|s| s.class == class && s.chain == chain && s.kind == kind && s.side == side)
                .unwrap_or_else(|| panic!("no second-fill charge for {chain} {kind} {side:?}"));
            assert!(second.base > 0);
            let dearest = cells
                .iter()
                .map(|c| c.base)
                .max()
                .unwrap();
            let addition = LIMITS
                .iter()
                .find(|l| l.class == class && l.chain == chain && l.kind == kind && l.side == side)
                .unwrap_or_else(|| panic!("no limit for {chain} {kind} {side:?}"))
                .addition;
            assert!(addition > dearest, "{chain} {kind} {side:?}: {addition} <= {dearest}");
        }
    }

    #[test]
    fn a_limit_addition_is_only_for_a_measured_key() {
        let chain = Chain::Base;
        let measured = LIMITS
            .iter()
            .find(|l| l.chain == chain.id())
            .unwrap();
        let sale = measured.side == Side::Sale;
        assert_eq!(
            gas_limit_addition(chain, measured.class, measured.kind, sale),
            Some(measured.addition)
        );
        assert_eq!(gas_limit_addition(chain, B256::ZERO, measured.kind, sale), None);
        assert_eq!(gas_limit_addition(chain, measured.class, 9, sale), None);
        assert_eq!(gas_limit_addition(Chain::Arbitrum, measured.class, measured.kind, sale), None);
    }

    /// The class of the cells of [`table`].
    pub(in crate::pamm::protocols::flamm) const CLASS: B256 =
        b256!("00000000000000000000000000000000000000000000000000000000000000c1");
    const BASE_ID: u64 = 8453;

    fn cell(kind: u8, side: Side, payout: Payout, repays: bool, base: u64) -> Cell {
        Cell { class: CLASS, chain: BASE_ID, kind, side, payout, repays, base }
    }

    fn limit(kind: u8, side: Side, addition: u64) -> Limit {
        Limit { class: CLASS, chain: BASE_ID, kind, side, addition }
    }

    fn second_fill(kind: u8, side: Side, base: u64) -> SecondFill {
        SecondFill { class: CLASS, chain: BASE_ID, kind, side, base }
    }

    /// A table of [`CLASS`] on Base, kinds 0 and 1 routed, kind 2 with cells and not routed. Kind 0
    /// sells at 100,000 from custody, 200,000 freed, 250,000 on a first borrow and 300,000 on an
    /// owing one, and buys at 110,000 from custody, 160,000 repaying from custody and 260,000
    /// repaying and freed; kind 1 sells at 120,000 from custody and 500,000 on an owing borrow.
    /// After another fill, kind 0 sells at 60,000 and buys at 70,000, and kind 1 sells at 50,000.
    pub(in crate::pamm::protocols::flamm) fn table() -> GasTable {
        use Payout::{FirstBorrow, Freed, Held, OwingBorrow};
        use Side::{Buy, Sale};
        GasTable::new(
            vec![
                cell(0, Sale, Held, false, 100_000),
                cell(0, Sale, Freed, false, 200_000),
                cell(0, Sale, FirstBorrow, false, 250_000),
                cell(0, Sale, OwingBorrow, false, 300_000),
                cell(0, Buy, Held, false, 110_000),
                cell(0, Buy, Held, true, 160_000),
                cell(0, Buy, Freed, true, 260_000),
                cell(1, Sale, Held, false, 120_000),
                cell(1, Sale, OwingBorrow, false, 500_000),
                cell(2, Sale, Held, false, 90_000),
            ],
            vec![
                limit(0, Sale, 900_000),
                limit(0, Buy, 800_000),
                limit(1, Sale, 1_100_000),
                limit(2, Sale, 700_000),
            ],
            vec![
                second_fill(0, Sale, 60_000),
                second_fill(0, Buy, 70_000),
                second_fill(1, Sale, 50_000),
                second_fill(2, Sale, 40_000),
            ],
            vec![(BASE_ID, vec![0, 1])],
        )
    }

    /// A table that serves `class` on Base over kinds 0 and 1, either side: 100,000 from custody,
    /// 300,000 on any other path and 50,000 after another fill, for tests whose pools are not of a
    /// measured class.
    pub(in crate::pamm::protocols::flamm) fn table_serving(class: B256) -> GasTable {
        table_serving_on(class, BASE_ID, &[0, 1])
    }

    /// A table that serves `class` over `kinds` on `chain`, at the figures of [`table_serving`].
    pub(in crate::pamm::protocols::flamm) fn table_serving_on(
        class: B256,
        chain: u64,
        kinds: &[u8],
    ) -> GasTable {
        let (mut cells, mut limits, mut second_fills) = (Vec::new(), Vec::new(), Vec::new());
        for &kind in kinds {
            for side in [Side::Sale, Side::Buy] {
                let custody = Cell {
                    class,
                    chain,
                    kind,
                    side,
                    payout: Payout::Held,
                    repays: false,
                    base: 100_000,
                };
                cells.push(custody);
                cells.push(Cell { repays: true, base: 300_000, ..custody });
                for payout in [Payout::Freed, Payout::FirstBorrow, Payout::OwingBorrow] {
                    cells.push(Cell { payout, base: 300_000, ..custody });
                }
                limits.push(Limit { class, chain, kind, side, addition: 1_000_000 });
                second_fills.push(SecondFill { class, chain, kind, side, base: 50_000 });
            }
        }
        GasTable::new(cells, limits, second_fills, vec![(chain, kinds.to_vec())])
    }

    /// A live venue of `kind` that posts token 0 and borrows token 1, and lets the pool repay.
    pub(in crate::pamm::protocols::flamm) fn venue(kind: u8) -> Venue {
        Venue { kind, live: true, coll: 0, debt: 1, repays: true, held_repay: None }
    }

    /// A live venue of kind 0 that lends token 1, lets the pool repay and holds a partial repay at
    /// `minimum`.
    fn held(minimum: Minimum) -> Venue {
        Venue { held_repay: Some(minimum), ..venue(0) }
    }

    fn venues(records: Vec<Venue>) -> Venues {
        Venues { records, unread: false }
    }

    /// The legs of a walk whose output holds `held`, has `supplied` at its venues and owes
    /// `owed_out`, and whose input owes `owed_in`.
    fn legs(held: u64, supplied: u64, owed_out: u64, owed_in: u64) -> Legs {
        let mut legs = [U256::ZERO; 8];
        legs[3] = U256::from(owed_in);
        legs[4] = U256::from(held);
        legs[5] = U256::from(supplied);
        legs[7] = U256::from(owed_out);
        legs
    }

    fn sale_key(records: Vec<Venue>) -> GasKey {
        table()
            .key(BASE_ID, CLASS, 0, 1, &venues(records))
            .unwrap()
    }

    fn buy_key(records: Vec<Venue>) -> GasKey {
        table()
            .key(BASE_ID, CLASS, 1, 0, &venues(records))
            .unwrap()
    }

    /// A sale whose output holds 1,000, has 500 more at its venues and owes nothing: any path
    /// within custody, one freeing the output past it, and a first borrow past both.
    #[rstest]
    #[case::at_custody(1_000, 100_000)]
    #[case::one_past_custody(1_001, 200_000)]
    #[case::at_what_the_venues_free(1_500, 200_000)]
    #[case::one_past_what_they_free(1_501, 250_000)]
    fn a_payout_is_charged_by_how_far_it_reaches(#[case] payout: u64, #[case] gas: u64) {
        let side = sale_key(vec![venue(0)]).side_gas(Some(&legs(1_000, 500, 0, 0)), true);
        assert_eq!(side.charge(U256::from(payout), false), gas);
    }

    /// What the venues free grows with their supply interest until the fill, so a payout past the
    /// read by up to 1/10,000 of it may be freed with no borrow: here 1,500,000 freed reaches
    /// 1,500,150, past which a fill borrows.
    #[rstest]
    #[case::at_what_the_venues_free(1_500_000, 200_000)]
    #[case::one_past_it(1_500_001, 200_000)]
    #[case::at_its_slack(1_500_150, 200_000)]
    #[case::one_past_its_slack(1_500_151, 250_000)]
    fn a_payout_just_past_what_the_venues_free_is_charged_as_freed(
        #[case] payout: u64,
        #[case] gas: u64,
    ) {
        let side = sale_key(vec![venue(0)]).side_gas(Some(&legs(1_000_000, 500_000, 0, 0)), true);
        assert_eq!(side.charge(U256::from(payout), false), gas);
    }

    /// An output that owes borrows owing, and may borrow first only where another venue lends it.
    #[rstest]
    #[case::nothing_owed(0, 1, 250_000)]
    #[case::owed_at_its_one_lender(1, 1, 300_000)]
    #[case::owed_where_another_venue_lends(1, 2, 250_000)]
    fn a_borrow_is_first_or_owing_by_what_the_output_owes(
        #[case] owed_out: u64,
        #[case] lenders: usize,
        #[case] gas: u64,
    ) {
        let side = sale_key(vec![venue(0); lenders]).side_gas(Some(&legs(0, 0, owed_out, 0)), true);
        assert_eq!(side.charge(U256::from(1u64), false), gas);
    }

    /// Every measured first borrow posts its input before it borrows. One against collateral the
    /// input already has posted borrows alone, under those and no cheaper than an owing borrow, so
    /// where the input has collateral posted a borrow may take an owing cell: here 300,000 against
    /// a first borrow's 400,000.
    #[rstest]
    #[case::nothing_posted(0, 0, 400_000)]
    #[case::posted_ahead(1, 0, 300_000)]
    #[case::posted_and_owed(1, 1, 300_000)]
    fn a_borrow_against_posted_collateral_may_take_an_owing_cell(
        #[case] posted_in: u64,
        #[case] owed_out: u64,
        #[case] gas: u64,
    ) {
        use Payout::{FirstBorrow, Held, OwingBorrow};
        let table = GasTable::new(
            vec![
                cell(0, Side::Sale, Held, false, 100_000),
                cell(0, Side::Sale, FirstBorrow, false, 400_000),
                cell(0, Side::Sale, OwingBorrow, false, 300_000),
            ],
            vec![limit(0, Side::Sale, 900_000)],
            vec![second_fill(0, Side::Sale, 60_000)],
            vec![(BASE_ID, vec![0])],
        );
        let mut read = legs(0, 0, owed_out, 0);
        read[2] = U256::from(posted_in);
        let side = table
            .key(BASE_ID, CLASS, 0, 1, &venues(vec![venue(0)]))
            .unwrap()
            .side_gas(Some(&read), true);
        assert_eq!(side.charge(U256::from(1u64), false), gas);
    }

    /// An input that owes repays where every venue lending it lets the pool repay now; where one
    /// refuses or is not live, the repay may be passed over, and a path without one is charged.
    #[rstest]
    #[case::nothing_owed(0, venue(0), 110_000)]
    #[case::owed_and_repaid(1, venue(0), 160_000)]
    #[case::owed_at_a_venue_refusing_repays(1, Venue { repays: false, ..venue(0) }, 110_000)]
    fn an_input_that_owes_repays_where_its_lenders_let_it(
        #[case] owed_in: u64,
        #[case] lender: Venue,
        #[case] gas: u64,
    ) {
        let side = buy_key(vec![lender]).side_gas(Some(&legs(10, 0, 0, owed_in)), true);
        assert_eq!(side.charge(U256::from(10u64), false), gas);
    }

    /// A venue that holds a partial repay at a minimum position takes it only where the debt
    /// stays over that minimum, so the repay is sure only where it lends the input alone and the
    /// input owes more than the minimum with its 1 % margin and the release slack of 4: 1,000
    /// becomes 1,014.
    #[rstest]
    #[case::one_over_the_edge(1_015, vec![held(Minimum::Read(U256::from(1_000u64)))], 160_000)]
    #[case::at_the_edge(1_014, vec![held(Minimum::Read(U256::from(1_000u64)))], 110_000)]
    #[case::its_minimum_unread(1_000_000, vec![held(Minimum::Unread)], 110_000)]
    #[case::beside_another_lender(
        1_000_000,
        vec![held(Minimum::Read(U256::from(1_000u64))), venue(0)],
        110_000
    )]
    fn a_minimum_position_leaves_a_repay_sure_only_over_it(
        #[case] owed_in: u64,
        #[case] lenders: Vec<Venue>,
        #[case] gas: u64,
    ) {
        let side = buy_key(lenders).side_gas(Some(&legs(10, 0, 0, owed_in)), true);
        assert_eq!(side.charge(U256::from(10u64), false), gas);
    }

    #[test]
    fn a_venue_of_the_input_that_is_not_live_leaves_the_repay_unsure() {
        let retired = Venue { live: false, coll: 2, ..venue(0) };
        let side = buy_key(vec![venue(0), retired]).side_gas(Some(&legs(10, 0, 0, 1)), true);
        assert_eq!(side.charge(U256::from(10u64), false), 110_000);
    }

    /// Without legs, and where the reads allow no cell (a buy past what its venues free, with no
    /// borrow measured), the least base is charged.
    #[test]
    fn what_the_reads_cannot_place_takes_the_least_base() {
        let key = sale_key(vec![venue(0)]);
        assert_eq!(
            key.side_gas(None, true)
                .charge(U256::from(5u64), false),
            100_000
        );
        let buy = buy_key(vec![venue(0)]).side_gas(Some(&legs(0, 0, 0, 0)), true);
        assert_eq!(buy.charge(U256::from(5u64), false), 110_000);
    }

    /// A fill on a state a swap returned is charged its side's second-fill charge, whatever it
    /// pays and whatever the legs read, as the fill before moved what they read: 60,000 on kind
    /// 0's sale and 70,000 on its buy, under every cell of either.
    #[test]
    fn a_fill_on_a_moved_state_is_charged_its_second_fill_charge() {
        let sale = sale_key(vec![venue(0)]);
        for legs in [None, Some(legs(1_000, 500, 0, 0)), Some(legs(0, 0, 1, 0))] {
            for payout in [1u64, 1_000, 1_501, 1_000_000] {
                let side = sale.side_gas(legs.as_ref(), true);
                assert_eq!(side.charge(U256::from(payout), true), 60_000);
            }
        }
        let buy = buy_key(vec![venue(0)]).side_gas(Some(&legs(10, 0, 0, 1)), true);
        assert_eq!(buy.charge(U256::from(10u64), true), 70_000);
    }

    /// A fill on a pool of three or more tokens may follow one on another of its pairs, which
    /// moves its state unseen, so every fill there is charged the second-fill charge.
    #[test]
    fn every_fill_on_a_pool_of_more_tokens_is_charged_its_second_fill_charge() {
        let sale = sale_key(vec![venue(0)]);
        for legs in [None, Some(legs(1_000, 500, 0, 0)), Some(legs(0, 0, 0, 0))] {
            for moved in [false, true] {
                let side = sale.side_gas(legs.as_ref(), false);
                assert_eq!(side.charge(U256::from(5u64), moved), 60_000);
            }
        }
    }

    /// A second-fill charge over the least base of its key's cells takes that least base.
    #[test]
    fn a_second_fill_charge_never_passes_the_least_base() {
        let table = GasTable::new(
            vec![cell(0, Side::Sale, Payout::Held, false, 100_000)],
            vec![limit(0, Side::Sale, 900_000)],
            vec![second_fill(0, Side::Sale, 150_000)],
            vec![(BASE_ID, vec![0])],
        );
        let key = table
            .key(BASE_ID, CLASS, 0, 1, &venues(vec![venue(0)]))
            .unwrap();
        for two_tokens in [true, false] {
            let side = key.side_gas(None, two_tokens);
            assert_eq!(side.charge(U256::ONE, true), 100_000);
        }
    }

    /// The cells of every live kind on the pair are allowed, and the dearest limit of them is
    /// added. Both venues lend the output, so an owing output may still borrow first at the other.
    /// A second fill may draw on either kind, so it takes the lesser second-fill charge.
    #[test]
    fn a_pair_over_two_kinds_takes_both_kinds_cells_and_the_larger_limit() {
        let key = sale_key(vec![venue(0), venue(1)]);
        let side = key.side_gas(Some(&legs(0, 0, 1, 0)), true);
        assert_eq!(side.charge(U256::from(1u64), false), 250_000);
        assert_eq!(side.limit_addition(), 1_100_000);
        assert_eq!(side.charge(U256::from(1u64), true), 50_000);
        assert_eq!(
            key.side_gas(None, true)
                .charge(U256::ONE, false),
            100_000
        );
    }

    #[rstest]
    #[case::no_venue(Vec::new(), false)]
    #[case::no_live_venue(vec![Venue { live: false, ..venue(0) }], false)]
    #[case::a_kind_the_chain_does_not_route(vec![venue(0), venue(2)], false)]
    #[case::a_kind_with_no_cells(vec![venue(3)], false)]
    #[case::an_unread_record(vec![venue(0)], true)]
    fn a_pair_whose_venues_the_table_cannot_charge_is_not_served(
        #[case] records: Vec<Venue>,
        #[case] unread: bool,
    ) {
        let venues = Venues { records, unread };
        assert_eq!(table().key(BASE_ID, CLASS, 0, 1, &venues), None);
    }

    /// A kind with cells and a limit and no second-fill charge for the side leaves it unserved,
    /// as a fill after another on its pool could not be charged.
    #[test]
    fn a_kind_with_no_second_fill_charge_is_not_served() {
        let table = GasTable::new(
            vec![cell(0, Side::Sale, Payout::Held, false, 100_000)],
            vec![limit(0, Side::Sale, 900_000)],
            vec![second_fill(0, Side::Buy, 60_000)],
            vec![(BASE_ID, vec![0])],
        );
        assert_eq!(table.key(BASE_ID, CLASS, 0, 1, &venues(vec![venue(0)])), None);
    }

    /// BSC routes Aave V3, Venus Core and Lista Moolah, whose guards are built, and Morpho Blue has
    /// no cells there.
    #[rstest]
    #[case::aave_v3(1, true)]
    #[case::venus_core(2, true)]
    #[case::lista_moolah(3, true)]
    #[case::morpho_blue(0, false)]
    fn bsc_serves_the_kinds_whose_guards_are_built(#[case] kind: u8, #[case] served: bool) {
        let class = CELLS
            .iter()
            .find(|c| c.chain == 56)
            .expect("the table has BSC cells")
            .class;
        let records = venues(vec![venue(kind)]);
        for (token_in, token_out) in [(0, 1), (1, 0)] {
            let key = GasTable::default().key(56, class, token_in, token_out, &records);
            assert_eq!(key.is_some(), served, "kind {kind}, token {token_in} in");
        }
    }

    #[test]
    fn a_venue_off_the_pair_or_another_class_or_chain_is_not_the_pairs() {
        let elsewhere = Venue { kind: 2, coll: 2, debt: 3, ..venue(0) };
        let venues = venues(vec![venue(0), elsewhere]);
        assert!(table()
            .key(BASE_ID, CLASS, 0, 1, &venues)
            .is_some());
        assert_eq!(table().key(BASE_ID, B256::ZERO, 0, 1, &venues), None);
        assert_eq!(table().key(1, CLASS, 0, 1, &venues), None);
        // kind 1 has no buy cells
        assert_eq!(table().key(BASE_ID, CLASS, 1, 0, &self::venues(vec![venue(1)])), None);
    }

    /// The least net gas measured of a fill paid from custody, a sale's then a buy's, by chain and
    /// kind, as the generator reports it.
    const CUSTODY_LEAST: [(u64, u8, [u64; 2]); 7] = [
        (1, 0, [201_504, 200_676]),
        (1, 1, [201_553, 191_086]),
        (8453, 0, [218_124, 217_385]),
        (8453, 1, [218_173, 207_795]),
        (56, 1, [195_810, 181_953]),
        (56, 2, [313_062, 299_205]),
        (56, 3, [195_144, 196_387]),
    ];

    /// A fill paid from custody is charged at most the least fill measured paying from custody of
    /// its own kind, chain and side, with its legs read and without them: every key's custody cell
    /// is measured on its own kind and chain.
    #[test]
    fn a_fill_paid_from_custody_is_charged_at_most_its_kinds_least_measured_one() {
        let table = GasTable::default();
        let held = legs(1_000_000, 0, 0, 0);
        for (chain, kind, least) in CUSTODY_LEAST {
            let class = CELLS
                .iter()
                .find(|c| c.chain == chain && c.kind == kind)
                .expect("the table has the kind's cells")
                .class;
            for (side, (token_in, token_out)) in [(0, 1), (1, 0)].into_iter().enumerate() {
                let key = table
                    .key(chain, class, token_in, token_out, &venues(vec![venue(kind)]))
                    .unwrap_or_else(|| panic!("{chain} {kind} is not served"));
                for read in [None, Some(&held)] {
                    let charge = key
                        .side_gas(read, true)
                        .charge(U256::ONE, false);
                    assert!(
                        charge <= least[side],
                        "{chain} {kind}, token {token_in} in: {charge} > {}",
                        least[side]
                    );
                }
            }
        }
    }

    /// The least net gas measured of the second of two fills of one pool in one transaction, by the
    /// second fill's side, a sale's then a buy's, by chain and kind, as the generator reports it.
    const SECOND_FILL_LEAST: [(u64, u8, [u64; 2]); 7] = [
        (1, 0, [76_004, 71_176]),
        (1, 1, [78_053, 68_086]),
        (8453, 0, [79_624, 74_885]),
        (8453, 1, [81_673, 71_795]),
        (56, 1, [80_810, 71_453]),
        (56, 2, [83_961, 74_604]),
        (56, 3, [84_344, 74_987]),
    ];

    /// A fill on a state a swap returned is charged at most the least second fill measured of its
    /// kind, chain and side, whatever it pays and whatever legs were read.
    #[test]
    fn a_second_fill_is_charged_at_most_the_least_measured_one() {
        let table = GasTable::default();
        let held = legs(1_000_000, 0, 0, 0);
        for (chain, kind, least) in SECOND_FILL_LEAST {
            let class = CELLS
                .iter()
                .find(|c| c.chain == chain && c.kind == kind)
                .expect("the table has the kind's cells")
                .class;
            for (side, (token_in, token_out)) in [(0, 1), (1, 0)].into_iter().enumerate() {
                let key = table
                    .key(chain, class, token_in, token_out, &venues(vec![venue(kind)]))
                    .unwrap_or_else(|| panic!("{chain} {kind} is not served"));
                for read in [None, Some(&held)] {
                    for payout in [U256::ONE, U256::from(1_000_000u64), U256::MAX] {
                        let charge = key
                            .side_gas(read, true)
                            .charge(payout, true);
                        assert!(
                            charge <= least[side],
                            "{chain} {kind}, token {token_in} in: {charge} > {}",
                            least[side]
                        );
                    }
                }
            }
        }
    }

    /// Lista Moolah's minimum position for a USDT loan where the measured BSC fills ran, 15.02
    /// USDT.
    const LISTA_FLOOR: u128 = 15_018_627_453_444_232_346;

    /// The charge of every measured fill in the file `FLAMM_GAS_CHECK_ROWS` names, the generator's
    /// check rows, from this module's own code, at or under the fill's net gas: with its legs, and
    /// without them; a second fill of one pool in one transaction on the state the first returned;
    /// a Lista Moolah venue lets the pool repay and holds a repay at its minimum. Skipped where the
    /// variable is not set.
    #[test]
    #[ignore = "needs measured rows: set FLAMM_GAS_CHECK_ROWS to the check rows file"]
    fn no_measured_fill_is_charged_over_its_gas() {
        let Ok(path) = env::var("FLAMM_GAS_CHECK_ROWS") else {
            eprintln!("FLAMM_GAS_CHECK_ROWS is not set: the measured rows are not checked");
            return;
        };
        let text = fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        let header: Vec<&str> = lines
            .next()
            .unwrap()
            .split('\t')
            .collect();
        for column in ["posted_in", "moved"] {
            assert!(
                header.contains(&column),
                "{path} has no {column} column: generate the check rows again"
            );
        }
        let table = GasTable::default().routing_every_kind();
        let chains = HashMap::from([("ethereum", 1u64), ("base", 8453), ("bsc", 56)]);
        let (mut rows, mut over, mut far) = (0usize, Vec::new(), 0usize);
        let mut worst: HashMap<(u64, u8, &str, bool), u64> = HashMap::new();
        for line in lines {
            let row: HashMap<&str, &str> = header
                .iter()
                .copied()
                .zip(line.split('\t'))
                .collect();
            let number = |name: &str| U256::from_str_radix(row[name], 10).unwrap();
            let chain = chains[row["chain"]];
            let class: B256 = row["class"].parse().unwrap();
            let kind: u8 = row["kind"].parse().unwrap();
            let sale = row["side"] == "sale";
            let net: u64 = row["net"].parse().unwrap();
            let (held, freed) = (number("held_out"), number("freed_out"));
            let mut legs = [U256::ZERO; 8];
            legs[2] = number("posted_in");
            legs[3] = number("owed_in");
            legs[4] = held;
            legs[5] = freed - held;
            legs[7] = number("owed_out");
            let lender = match kind {
                3 => Venue { held_repay: Some(Minimum::Read(U256::from(LISTA_FLOOR))), ..venue(3) },
                _ => Venue { repays: row["repay_certain"] == "1", ..venue(kind) },
            };
            let lenders: usize = row["lenders_out"].parse().unwrap();
            let records = if sale { vec![lender; lenders] } else { vec![lender] };
            let (token_in, token_out) = if sale { (0, 1) } else { (1, 0) };
            let key = table
                .key(chain, class, token_in, token_out, &venues(records))
                .unwrap_or_else(|| panic!("no key for {line}"));
            let payout = number("q");
            let moved = row["moved"] == "1";
            for charge in [
                key.side_gas(Some(&legs), true)
                    .charge(payout, moved),
                key.side_gas(None, true)
                    .charge(payout, moved),
            ] {
                if charge > net {
                    over.push(format!("{charge} > {net}: {line}"));
                }
            }
            let under = net -
                key.side_gas(Some(&legs), true)
                    .charge(payout, moved)
                    .min(net);
            if under > 100_000 {
                far += 1;
            }
            let entry = worst
                .entry((chain, kind, row["side"], moved))
                .or_default();
            *entry = (*entry).max(under);
            rows += 1;
        }
        let mut worst: Vec<_> = worst.into_iter().collect();
        worst.sort();
        eprintln!(
            "{rows} measured fills checked, {far} under-charged by more than 100,000; worst \
             under-charge by chain, kind, side and fill:"
        );
        for ((chain, kind, side, moved), under) in worst {
            let fill = if moved { "second" } else { "first" };
            eprintln!("  {chain} {kind} {side} {fill}: {under}");
        }
        assert!(rows > 0);
        assert!(
            over.is_empty(),
            "{} fills charged over their gas:\n{}",
            over.len(),
            over.join("\n")
        );
    }
}
