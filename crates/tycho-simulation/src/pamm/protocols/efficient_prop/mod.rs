//! Efficient PropAMM listings on Base and Ethereum, each reached through its own listing door: one
//! `IPropAMM` contract per listing, which forwards to the listing's named fill on the PropAMM
//! front. What follows describes Base; Ethereum reads the doors another way (see
//! [On Ethereum](#on-ethereum)).
//!
//! Base is not served yet: [`EfficientPropFeedBuilder::build`] refuses it. Base's gas figures and
//! its recording were taken on an earlier test deployment that is no longer live, and no feed
//! should quote that one. Base's path below is whole and runs in this module's offline tests; it
//! is served once the figures are measured on the deployment Base runs.
//!
//! The front prices every listing on chain from what its operator last posted, so the
//! feed does not model the front: it reads each door's own `quote` over JSON-RPC every poll, at
//! a ladder of input sizes, and publishes one book per door priced from those samples. Each
//! component carries the door in its `pamm_address` static attribute, which tycho-execution's
//! generic `PropAMMSwapEncoder` packs for `PropAMMExecutor`: the router sends the input to the
//! door, then calls `swap(tokenIn, tokenOut, amountIn, 0, receiver, block.timestamp)`. The door
//! takes push payment (the front pulls the input from the door under a standing grant the door
//! set at its creation), a `minAmountOut` of zero is accepted (the router enforces the user's),
//! and a fill is all or nothing.
//!
//! # Finding the doors
//!
//! A door is created by the listing-door factory at the CREATE2 address of its listing id, and
//! anyone may open one. Listing ids are handed out in order from 1 and never reused, so each poll
//! reads the front's `doorView` for every id up to a little past the highest one seen (a zero pool
//! marks an id never reserved), and computes each door's address itself from the factory and the
//! door code hash the factory answers (`DOOR_CODEHASH`, read in the same request): no door code
//! hash is configured, since every door's address moves with its code. Code found there is that
//! door code, created by that factory. A listing is served once its door has code that names the
//! configured front and that listing.
//!
//! # Pricing
//!
//! Every read of one poll is pinned to one block: the block of the first request, which reads the
//! listings and the chain. For each door and side the feed asks `quoteFillable` for the most the
//! listing fills now, then `quote` at that size and at halvings of it
//! ([`EfficientPropFeedBuilder::rungs`] sizes in all), and below those at quarterings down to the
//! listing's least fill, read from its settings in the front's storage (of the quote token: the
//! ask's input, and for the bid the input that pays it at the full fill's average price), and at
//! that least fill itself. A size the door refuses (a revert: under the listing's minimum, past its
//! depth, a stale word) is no sample, never a zero price. The state quotes exactly at a sampled
//! size, along the chord between two neighbouring samples rounded down, and refuses sizes below the
//! smallest sample or above the largest. The chord is a lower bound on the fill wherever the output
//! is concave in the input, as the front's rung ladder and its per-fill fee make it; where the
//! samples show otherwise by more than the door's rounding down of each output explains, the state
//! quotes the smaller sample's output for that stretch instead.
//!
//! A listing's levels may age within a book's life: the front cuts them for the word's age past the
//! listing's grace (each side's drift, and a widening with the root of the age), and by a premium
//! for a window after the latest post. So each size is sampled in the same pinned call at the
//! block, and under a block override (`eth_call`'s fourth parameter) at the last block before the
//! book's life ends, and, where a premium's window closes within the life, at the last block before
//! it closes and at the first at or after it, on every side with a premium. On every listing
//! served, the drift and the widening only grow and the premium only ends as the front's clock
//! reads later, so the least of the price samples is the least the door quotes over the life, and
//! the sample keeps it. That holds only of the listings served: not of a SESSION listing's closed
//! phase or a tied listing, whose quotes may rise and fall again within a life, and which are not
//! served (see below). A listing reading a guarded source, whose quotes rise once its lane's hold
//! ends, is sampled through the hold as well (see below). That bounds the price, not the door's
//! refusals: a size refused at any sample is not quoted. A side whose largest fill is bound by its
//! output (a maker's bucket, the pool's room) pays the most at the first block after its premium
//! ends, so a size that passes the bound anywhere in the life passes it there, the bucket only
//! refilling and the drift only lowering the output after it. A listing that reads a price print is
//! also asked, in the same call, at the last block of each second of the life (see below).
//!
//! Those blocks are found on the front's own clock, read as the front reads it: on Base it counts
//! 200 ms a block from an anchor; where that count leaves the block's second, it reads the BaseTime
//! predeploy's milliseconds; and where those do not fall in the block's second either, the start
//! of the second. The first request reads the clock's settings and BaseTime at the read's block,
//! and each override's number and time are chosen so that the front's clock there reads at or
//! before the moment meant, a block every 200 ms as Base makes them. BaseTime answers the block's
//! second plus a sub-second the chain stores in it each block, which a block override does not
//! move, so where the clock reads BaseTime each override also sets that sub-second to its block's
//! own (a state override, `eth_call`'s third parameter): the last block before a moment is sampled
//! there, never a block up to a second younger. Where neither the count nor BaseTime falls in the
//! block's second, overrides step whole seconds. Each call under an override first reads the block
//! number, the timestamp and BaseTime there, and its answers count only where the front's clock
//! they give is the one meant.
//! A node that refuses the override, ignores it, or applies part of it, cannot show those later
//! quotes: then a side is served from the block's samples only where its levels cannot age within
//! the life (no drift and no widening, or a word that stays within its grace for all of it), no
//! premium window of it closes within the life, and its listing reads no price print; it is left
//! out otherwise.
//!
//! A front whose clock can step back is not served. With its count off BaseTime by less than a
//! second, the front reads the count in the blocks where it lands in the block's second and
//! BaseTime in the others, so a block after the read may read earlier than the read; a few samples
//! cannot bound what the door quotes over the life there (a print fed just before the read is
//! refused, and a refilling bucket has less room). On Base the count sits on BaseTime's reading,
//! and only a block cadence other than 200 ms after the anchor would move it off; the feed then
//! publishes no book of that front, with or without the node's overrides, and says why in its log.
//! The two clocks a served front reads never step back: its count on its anchor, and BaseTime
//! alone where the count is a second or more off it, as on a fork making a block a second.
//!
//! A listing that reads a price print is served only where every source it is bound to is one the
//! caller approved ([`EfficientPropFeedBuilder::approved_price_sources`], none by default). A
//! source is any contract registered on the front, which anyone may register, and the listing's
//! operator binds it. The source's code runs inside the quote and again inside the fill, and it can
//! tell the two apart: by what the call reads of its transaction and block, and by state the fill
//! has already changed when the source runs. On this route the front holds its lock through the
//! fill, and the input is already at the door. A source that tells them apart can fill a real
//! transaction for less than the book quoted, down to the route's minimum output. So approve a
//! source only where its code cannot change and its answer comes from its own stored prices and
//! the clock alone. A guarded source needs approval as any other. For a listing with no source, or
//! whose every source is approved, a fill pays what the door quotes from the same state. The
//! binding (the primary's and the backup's ids, record word 10 of the front's storage) is read with
//! the listing's settings at every poll's block, so a listing its operator rebinds is judged again
//! on the next poll.
//!
//! The same holds of a listing's two tokens. Their code runs inside the fill and can tell it from a
//! quote in the same ways, and the integration does not screen them: it serves the listings between
//! the tokens the caller's book configuration names, whatever their code. Name only tokens whose
//! code cannot change and moves exactly the amounts asked.
//!
//! A listing with a credit mode is not served on Base, nor on Ethereum unless the caller names it
//! with its gas ([`EfficientPropFeedBuilder::credit_listing`]): past its pool's custody an input
//! fills whole through the pool's credit route at the pool's own preview, which may pay less than a
//! smaller input, so a chord across that step could quote more than the door fills, and a fill
//! there costs its pool's own gas. How a named one is served is under [On Ethereum](#on-ethereum).
//!
//! A SESSION listing with a closed phase is not served: one whose session word (record word 13 of
//! the front's storage, read with its settings at the read's block) holds a nonzero `closedAfter`,
//! or whose session word the node does not answer. Once its print is `closedAfter` old, the front
//! prices the word's levels within a band that eases as the print ages, a step each whole second
//! of its age, while the word's drift and widening tighten them, so the door's quote is not
//! monotone in the clock: between two blocks sampled it may dip below the read, the life's end and
//! the premium's edge alike. And the gap in which the door refuses every size, between the print's
//! lapse and the closed phase, may be shorter than a second. A SESSION listing with no closed
//! phase reads its print as a PRINT listing does, and is served as one.
//!
//! A tied listing (its settings word's scopes) is not served either: it walks its maker's ladder
//! from its print rather than its word. It is read from the record at the read's block.
//!
//! A listing reading a print from a guarded source is served, sampled through its lane's hold. A
//! guarded source (its registration's guard bit, which the listing's copy of its live source,
//! record word 2, holds beside the source's lane) prices a fill's side at the worst of the prints
//! its lane still holds: a print a post replaced still prices that side for the lane's `HOLD_MS` on
//! the front's clock. So after a post that moved the price, the door quotes that side less within
//! the hold and more once it ends, and a size at the side's output bound (a maker's bucket, the
//! pool's room) is refused from the hold's end, as from a premium's end. A post the read saw was
//! made at or before the read's block, so its hold ends at most `HOLD_MS` after the read: in the
//! same call as the other samples, under block overrides, each size of both sides is also asked
//! at every block after the read's through the first at or past that, kept at the least, and
//! dropped where the door refuses it at any of them. The lane's `HOLD_MS` (a public immutable of
//! its code: 400 ms on Base's hold lane, where the front counts 200 ms blocks) is asked the first
//! time the listing is read; until the lane answers, 2,000 ms, the longest a lane holds, is
//! sampled. The lane holds on the clock the front reads, as Base's does. A node that refuses or
//! ignores the overrides cannot show the hold, so there a guarded listing is not quoted. A listing
//! reading a print whose live source's copy the node does not answer is left out.
//!
//! # A book's life
//!
//! A poll's books are published when its fetch ends, at most the request timeout after it starts;
//! a published snapshot is served until the next poll replaces it or until it is withdrawn, at
//! most the snapshot's age limit; and a route priced from it then needs time to land, the landing
//! allowance ([`EfficientPropFeedBuilder::landing_allowance`], two seconds by default). A book's
//! life is all of that, from the later of the wall clock at the poll's start and the read block's
//! time on the front's clock (on Base the block's own milliseconds): eight seconds by default. A
//! listing fills while its operator's standing word and the side's depth stand: the word for its
//! life from its maker time, capped by the front, and the depth until its expiry, both read from
//! the front's `doorView` at the poll's block. On a listing reading no print, a side whose word or
//! depth runs out within the book's life is left out. On one reading a print, the door refuses
//! every size from that block, as from a print's lapse, so the side is served up to that block, as
//! a side whose print lapses is (below). A poll that ends too late for its books to be served and
//! land within their life fails. The life can still end sooner when the operator replaces the word;
//! the next poll finds that.
//!
//! A listing that reads a price print (its fence kind PRINT or SESSION, read from its settings
//! word) fills only while the front takes that print: past the print's source window or the
//! listing's maximum age the door refuses every size. A book is served only as far as the print it
//! read lives, no later print assumed. So in the same call as its samples, under block overrides,
//! each side of such a listing is asked whether the door still fills its smallest and its largest
//! sampled size at the last block of each second of the life between the read's block and its last;
//! the samples at the life's end ask it there. The front refuses every size while it refuses the
//! print, so a fill of either size at an ask shows the print taken there, and where the door
//! refuses both at an ask the print's life ends within the book's. One size refused at an ask
//! while the other fills is refused for a reason of its own (the least fill under the listing's
//! minimum once the levels drift): the side is served without that size. A node that refuses or
//! ignores the overrides cannot show the print's life, so there every side reading a print is left
//! out. A listing served reads its print as a PRINT listing does, which refuses it from its lapse
//! on, so the asks find a lapse within the life.
//!
//! A side whose print lapses within the book's life gets a life of its own, ending with its print
//! (or with its word or depth, where those run out first).
//! A fourth request asks every size of it at the last ask its print lived through and at every
//! block between that ask and the next, under overrides as the others; the side's life ends after
//! the last of those blocks, run without a gap from that ask, at which the door fills any size,
//! and its samples are the least at the read, at those blocks and at any premium edge before
//! then. The side then quotes, on this process's clock, only until a route sent can still land
//! within that life: its end less the landing allowance and how far the front's clock leads the
//! wall clock (zero where it lags). A side that would quote for less than 200 ms from the poll's
//! end is left out, and so is one whose next ask comes too soon for that, without the fourth
//! request. So a PRINT listing whose print lives four seconds, posted every second, is served at
//! the default settings, each poll's book quoting while a route sent then lands on the print it
//! read. Listings that read no print are not asked, and a side whose print outlives the book's
//! life keeps the book's.
//!
//! # Fills between polls
//!
//! A fill walks the listing's depth, so after another taker's fill the door quotes a larger input
//! further down its ladder, at up to the next rung's offset less, until the next poll reads it: a
//! poll interval (250 ms by default), or up to the snapshot's age limit while polls fail. The feed
//! watches no fill or post logs, so a book may quote that much over the door in that time: a route
//! priced from it fills that much short, within the caller's slippage, or reverts at the router's
//! minimum output.
//!
//! Two listings of one pool share its custody: each door's samples reach the whole of what the
//! pool holds of the token it pays, and no book shows the other's claim on it. A route that fills
//! both near their largest sizes in one transaction (a WETH/USDC and a cbBTC/USDC listing whose
//! bids both pay the pool's USDC, say) has its second fill refused by the pool, and reverts. The
//! books carry no pool to group them by.
//!
//! # Gas
//!
//! The gas charged for a swap is one of six classes on Base, each per direction (see [`state`]): a
//! listing reading only its operator's word of its pool's hook pair (the pool's first listing, its
//! base the pool's token 0 and its quote token 1), the same past the hook pair (any further listing
//! of the pool, which the pool settles by another route, about 29,000 to 33,600 more), a listing of
//! the hook pair whose fill reads a source that is not guarded (a PRINT or SESSION listing; about
//! 12,300 to 12,800 more), one reading a print from a
//! guarded source (about 18,300 to 18,500 more), and each of the last two past the hook pair,
//! charged the sum of the two differences, which was not measured. Each figure is the least first
//! fill measured, with real transactions on a Base fork run on the front's counted clock, as live
//! Base runs it, with the receiver holding the output token, before refunds, and with the tokens'
//! own transfer cost as measured on WETH, USDC and cbBTC: a listing of tokens cheaper or dearer to
//! move spends that much less or more. The print figures were measured on a listing bound to the
//! protocol's stored price lane, the guarded ones on a guarded source on Base's hold lane with no
//! print held at the fill (a fill while the lane holds a replaced print spends about 4,800 to 5,500
//! more, not charged); a source other than those spends what its own code spends. A listing of an
//! asset-module pair is charged no class figure: see Stock listings below.
//!
//! On Ethereum the class reading a source that is not guarded was measured with real transactions
//! on an Ethereum fork, at three places: the hook pair, past it with the pool's asset (its token 0)
//! one of the pair's tokens, and past it with neither token the asset, each charged its own (a
//! USDT/USDC fill buying USDT spends about 18,300 more than wstETH/USDT's). Past the hook pair with
//! the asset, the figure follows whether the input is the asset, not which token is the base: a
//! pair whose quote is the asset is charged the asset-in figure where the swap pays the asset in.
//! It was measured with the asset as the base only. Each fill was sent right behind its block's
//! relay entries, the only fill of its side there, through Ethereum's `PropAMMExecutor`, with
//! wstETH, USDC and USDT, from a pool whose custody holds the output; each figure is the least
//! first fill seen at its place across every run on that state, less the refund the fill earns as
//! the only fill of its transaction (19,900 on every custody fill traced: the router's transfer
//! writes the door's balance of the input from zero and the fill clears it), so that a charge is
//! what the taker's transaction pays for the fill and never more. Same-size first fills vary by up
//! to about 500 gas from run to run, and other sizes cost up to about 1,100 more, so a fill may
//! spend up to about that much more than it is charged, and a later run may come in under its
//! charge. A listing reading only its word was measured the same way and is charged its own
//! figures, 10,074 to 10,287 under the print figures at the same place and direction. A listing
//! reading a guarded source is not served there. A fill of a word carried into the block after its
//! update's costs about 2,400 more than a first fill in the update's block; that is not charged. A
//! credit listing is charged the four figures its caller measured on its pool instead (see
//! [On Ethereum](#on-ethereum)).
//!
//! Where two pairs of one class were measured more than the fill's swing apart, the dearer is
//! charged its own figures (`ETHEREUM_PRINT_PAIRS` in [`state`]): wstETH/USDT past the hook pair
//! spends 5,186 more selling wstETH than wstETH/USDC, the cost of the pool's USDT payout over its
//! USDC one. Every figure is of a pool that holds some of both the pair's tokens. A custody fill
//! that pays into its pool a token the pool holds none of writes the pool's balance of it from
//! zero and is charged 16,964 more (`FIRST_BALANCE_WRITE_GAS`, the least of the 16,964 to 17,100
//! measured; the balance stays, so nothing is refunded): each Ethereum poll reads what each
//! listing's pool holds of its two tokens (`balanceOf`, beside the samples), and the state adds
//! it to a custody fill whose input the pool lacks. A routed fill is charged nothing more: a
//! balance its tokens write from zero is cleared again and all but about 1,900 is refunded.
//! A side's largest custody size is charged 9,600 less (`LAST_CUSTODY_FILL_REFUND`): that fill may
//! take the pool's whole balance of the output, which clears it and earns its transaction up to
//! that much more back; a smaller size leaves a balance and earns nothing more.
//!
//! The figures are what a fill costs its transaction, not a gas limit. A limit is sized from the
//! gas the call uses before refunds: the figure plus the refund (19,900 on a custody fill; on the
//! first pool's route 84,400 to 99,500). A fill also forwards its gas through the door, the main
//! contract and the pool, each call keeping a 64th back, so a transaction needs more gas than it
//! uses: the least limit that let a fill through was 11% to 20% above the gas it used before
//! refunds, measured through Ethereum's `PropAMMExecutor` on the first pool's four listings. Size
//! a limit from that sum with at least 25% on top, or the fill runs out of gas and reverts.
//!
//! # The tip cap: a limit no book can enforce
//!
//! A door's quote and fill read the transaction's priority fee (gas price less basefee). The feed
//! quotes with no gas price, so at no tip. A listing that reads no price print, or reads one only
//! from approved sources, prices the same at any tip under its cap: an approved source answers from
//! its own stored prices and the clock alone (see Pricing). One bound to a source that is not
//! approved is not served, since that source's code may read the fill's transaction, its gas price
//! among the rest, and answer it otherwise than the quote (see Pricing). But every
//! listing on Base has a taker tip cap: the front lets a listing's word stand only while it has
//! one, and refuses a swap sent with a priority fee above it (`TipCapped`), the whole route
//! reverting. The cap is the listing's own setting, which its operator may change, and the sender's
//! priority fee is chosen after the route is priced, so the books cannot hold a route to it. Each
//! book instead carries its listing's cap at the read, in wei per gas, in the
//! [`TIP_CAP_WEI_ATTRIBUTE`] static attribute (absent for a listing with none): a solver that
//! routes through a door must send at or under it. A caller that always sends above some priority
//! fee leaves out the listings that would refuse it with
//! [`EfficientPropFeedBuilder::min_tip_cap_wei`] (0 by default, none left out).
//!
//! # On Ethereum
//!
//! On Ethereum a listing's maker posts its quote as an update that lands only in a builder's
//! block, ahead of the fills that read it: one relay entry a pair, which writes the quote into the
//! listing's desk contract (the front's `deskCellOf` of `doorView`'s `desk`) and passes the pair's
//! price update to its price contract. A fill reads the maker's quote only in the block its update
//! landed in, or in the block after where both the listing and the maker's word carry (the
//! listing's carry bits in its settings word, the word's own carry bits, and a fence other than
//! NONE). In any other block the door refuses every size (`NoWord`). The node's latest block
//! therefore says nothing of what a door fills next, so each poll reads the doors at the next
//! block, as the state maps for it describe that block.
//!
//! Each poll reads, at once, the listings at the node's latest block (the first request, as on
//! Base, less BaseTime: that predeploy is Base's and has no code on Ethereum) and the next block's
//! state map from every URL given to [`EfficientPropFeedBuilder::state_urls`] (required on
//! Ethereum, refused on Base), such as the protocol api's `https://<api host>/v1/1/state`: one
//! JSON-RPC POST of `titan_getPammStateOverrides` a URL. A map names a block, its beacon slot and,
//! for each account the block's updates write, the storage they write. The poll waits up to 300 ms
//! for the other URLs once a map carrying an update has answered, and fails where every URL fails,
//! without logging a URL.
//!
//! Only a map for the block after the node's latest is used, and only at the slot right after the
//! latest block's, its time 12 s after the latest block's: a map at or behind the latest block is
//! for a block already made, one further ahead was made on state the node has not seen, and one at
//! a later slot, whether a slot was missed or the map runs ahead of the wall clock, would be read
//! at a time the next block does not land at. A map for another block or slot is dropped whichever
//! URL answers it, and another URL's map for the next block at its slot is still read. A node whose
//! latest block is more than two slots (one missed) and the lag allowance (4 s) behind the poll's
//! start has stalled, and that poll fails as lagging, map or none: the feed logs it as a warning
//! and backs off, as for any failed poll, at most 250 ms × 2³ = 2 s at the default tuning, under a
//! slot. Where no map is used the poll publishes no book and does not fail, so the feed does not
//! back off, and the next poll reads the maps afresh.
//! Otherwise every later request is pinned to the latest block and runs each call there under the
//! next block's number and time (its slot's time, as a block override) and the listings' accounts
//! (as a state override). A node that refuses or ignores either override fails the poll: the
//! latest block's state alone shows no quote.
//!
//! A listing is read there by one slot rule (`slots.rs`). Its maker has one desk contract, the
//! CREATE2 child of the front at salt `(1 << 252) | desk` under the configured creation code hash
//! ([`EfficientPropDeployment::cell_codehash`]), which the front's `deskCellOf` of the desk id must
//! confirm. The listing's pair is its `doorView` word index `i`: its quote word is at slot
//! `0x100 + i` (stamp in bits 0-31, the desk epoch it was written under in bits 32-62, carry at
//! 201, firm state at 205) and its firm word at `0x200 + i`, which counts only where the quote
//! word's bit 205 is set. Slot 1 is the desk word: the pusher, the desk epoch (bits 160-190) and a
//! stop (bit 224). It is read from the desk contract at the head, never taken from an answer, since
//! a pair's word shows neither a stop nor a later epoch. A map's answer covers a listing only where
//! all hold:
//! - it writes the listing's quote word with a stamp the next block fills: that block's, or the
//!   block before's where the listing carries (its carry life set and its fence not NONE) and the
//!   word does;
//! - the head's desk word has the desk not stopped;
//! - the word's epoch bits equal the head's desk epoch, all 31 bits (a word of another epoch is
//!   empty);
//! - it writes no slot of the desk contract but the pairs' words: a write of slots 0 to 3 or any
//!   slot outside `0x100`-`0x1ff` and `0x200`-`0x2ff` covers none of the desk's listings;
//! - none of the answer's entries disagree on a slot the listing owns: two entries of one answer
//!   writing one slot with different values drop the listing for that block, whichever answer
//!   covers it (and so do two entries posting different values to a price contract it reads, for
//!   that answer). Writes in different answers do not conflict: the first URL's answer that covers
//!   the listing is served. The rule sees the answers that arrive within the poll's merge window; a
//!   later one is seen at the next poll;
//! - no entry of the answer was left out as unreadable: an answer with any entry the parser cannot
//!   read whole (a bad balance or nonce, a slot key or value that does not parse, a stateOverride
//!   that is not an object, an address that does not parse) covers no listing for that block, since
//!   the entry may write anything, a desk word or a firm word among it. A later clean answer for
//!   the block still covers;
//! - a word stamped with the head block (carried into the next block) is the head's own word, read
//!   from the desk contract in the same batch, byte for byte (its firm word too where its firm bit
//!   is set): no landing in the next block can write a stamp of the head block.
//!
//! An answer that writes the desk contract without the quote word, or with another block's stamp,
//! carries no update of the listing for the next block, and its door is not asked. A registry
//! lane's write sits in the registry's account until a fill copies it, so it covers no listing; no
//! lane is modelled. A listing's slots come from one answer: the first, in the order the URLs are
//! given, that covers it. A listing whose fill reads a print (fence PRINT or SESSION) is served
//! only where that answer also writes the price contract of the source it reads (the front's
//! CREATE3 child at the live source's id): without it the door would be read on the latest block's
//! print, not the one the next block's fill reads. Its backup source is not read: the fill reads
//! the live source alone, and a backup's price contract is neither taken nor in the way of a
//! listing whose live source it is. A
//! listing's slots are taken from its answer alone, never some from one URL and some from
//! another, and a listing that would take a slot an earlier listing took from another answer is
//! not served, while the earlier one is: listings sharing a desk contract, each with its own pair
//! of slots, are each served from the answer that covers them. No other account or slot of any
//! map is overridden: a map cannot move a door's quote through the front, its pool, its tokens,
//! the desk word or another listing's slots. A listing no map covers has no update in the next
//! block, and its door refuses every size there: it is not served. A listing bound to no desk
//! reads desk 0, which no maker updates, and is not served.
//!
//! Each side is sampled at the next block and, where its listing carries, at the block after under
//! the same state, a slot later. A side lives through the block after only where the door fills
//! there every size it fills at the next block, and its samples are then the least of the two; it
//! lives through the next block alone, with the next block's samples, otherwise, as when the door
//! refuses every size in the block after. Those are the only blocks the update fills in, and the
//! samples are the door's own answers at each of them, so they hold through the side's life
//! whatever its word, depth, premium, print or source. A book's side is then served until its last
//! block's time less the landing allowance ([`EfficientPropFeedBuilder::landing_allowance`], one
//! second by default on Ethereum), on this process's clock: a route sent then still reaches the
//! builder before it seals that block. A poll whose map's block is far behind the poll's start
//! fails, as on Base. The rules of Pricing on which listings are served (approved sources, closed
//! sessions, tied listings) hold on Ethereum as on Base, and a credit listing is served there only
//! as below. A front on Ethereum runs the block's second as its clock; one on any other clock ends
//! the feed.
//!
//! A listing with a credit mode (1 to 3, `doorView`'s `creditMode`) is served on Ethereum only
//! where the caller named it with its gas ([`EfficientPropFeedBuilder::credit_listing`]); one not
//! named, or of a mode the front does not know, is left out. Past its pool's custody such a listing
//! fills an input whole through the pool's credit route, and the door's quote there is the pool's
//! own preview. So beside each door quote the poll asks the front's `quoteFrom` with `PLAN_ONLY`
//! (bit 253 of its tip word, at no tip, the door as the taker), the front's own decision of whether
//! the size routes: an answer tagged `ROUTED` (bit 255) shows a size the route fills, an untagged
//! one equal to the door's quote a size its pool's custody fills, and any other answer leaves the
//! size unsampled. In the block after, a size whose path differs from the next block's is no
//! sample. A side's samples are its custody sizes, then its routed ones: a custody size above a
//! routed one ends the run, as a refusal does, and so does a routed output that does not grow past
//! custody's last. The state quotes each run exactly and along its own chords, nothing between the
//! largest custody size and the smallest routed one (it hands back the largest custody size's fill
//! as the partial result, so amounts under the side's limit can fail there), and charges each fill
//! its path's configured gas ([`CreditGas`]): no class of `swap_gas` holds for such a listing, a
//! bid that borrows costing its pool's own, several times a custody fill. What the route refuses
//! at the sample (a debt cap, a rate ceiling under the market's rate, a darkened print, a pause, a
//! pool bucket) the door refuses, and the size is not quoted. What moves between the sample and
//! the fill (another taker's borrow lifting the market's rate past the ceiling, a feed update, a
//! venue's accrual) reverts the route at the taker's gas, held only by the route's minimum output.
//! A route's preview costs more gas than a custody quote, so an `eth_call` of 48 calls carrying a
//! credit listing's needs more of the gas a node grants one: a routed quote measured 563,000 to
//! 740,000 gas on the first pool (three lender positions), so a full call of 48 is about 36M,
//! inside the 50M most nodes grant an `eth_call` by default.
//!
//! The door's `quoteFillable` reports a credit listing's custody alone unless the pool's preview
//! fills the whole of the maker's depth, and the route fills an input whole or not at all. So a
//! side that may route is first asked how far it reaches, in one more batch of a poll that serves
//! such a listing (`reaches` in `reader.rs`): where it reports nothing fillable, its door's
//! `quote` at 17 sizes a factor of four apart across the token's scale; where it reports some, at
//! 17 sizes above it, a 64th more and that step doubled. The largest answered is its reach. The
//! side is then sampled at the reach and the sizes under it, and at eight even steps from the
//! reach to the first size refused. Every sample is still the door's own answer beside the
//! front's plan in one batch: the first asks only choose the sizes.
//!
//! A route borrows or repays inside the swap, which moves what some listings of its pool fill
//! after it in the same transaction: the pool's cheap lane refuses to pay a token it owes, so a
//! mode-0 sibling's fill of it reverts, and a mode-2 sibling's bid routes until a flush. Each
//! listing is a book of its own, so a credit listing is served its custody sizes alone where
//! another door sampled in the poll on its pool has a credit mode of its own or trades a token the
//! route may leave owed: the credit listing's own tokens but the pool's token 0, the collateral,
//! which a route supplies and withdraws and never borrows. Beside listings that trade neither it
//! keeps its route: on the first pool a routed wstETH/WETH fill, either way, left wstETH/USDC,
//! wstETH/USDT and USDT/USDC filling exactly their quotes in the same block and the next. All of
//! a pool's listings still share its custody: the largest sizes of two books that pay out one
//! token can exceed it together, as on any pool that several components trade.
//!
//! One route figure a side cannot hold every state: on the first pool a borrow that is the
//! block's second spends about 78,000 less than its first, a repay after more of the pool's
//! positions are in use spent 99,000 more, and the pool's settings move it by tens of thousands.
//! Measure a listing's figures on its pool as it stands, and again when its settings change, and
//! name the least seen: the figures are charges, never over what a first fill in a fresh block
//! spends. They are not limits. A size sampled inside custody fills through the route where
//! another fill of the block takes the pool's balance first (the front routes what custody cannot
//! pay), at several times the gas, so a solver's gas limit on any listing that has a route must
//! cover the route's figure; the next poll samples the pool as that fill left it, and the size is
//! then served as routed, at the route's figure.
//! In mode 3 sizes its custody fills whole route too,
//! drawing nothing, and are charged the route's figure, more than they spend. The plan's
//! `PLAN_ONLY` answer is the deployed build's: on an earlier build bit 253 is part of the
//! tip, a listing with a tip cap refuses every plan read, and one without answers a routed size
//! untagged, the door's own output, which would be quoted as custody. Name a credit listing only
//! where every allow-listed implementation has it.
//!
//! A listing bound to a guarded source is not served on Ethereum: there sources are price
//! contracts, and no lane holds a replaced print.
//!
//! An update that carries its desk's firm state (the pair's firm word, slot `0x200 + i`) is
//! accepted, and a streamed fill never reads that word. Signed firm quotes, which a taker fills
//! at the main contract's `fillQuote`, are not served by this integration: no door has a firm
//! entry.
//!
//! # Every desk's listings
//!
//! Nothing here names a maker. Listing ids are the front's own, handed out in order to every pool
//! and desk, and each poll scans `doorView` past the highest seen, so a new maker's listing is
//! found at the next poll. Its door is the factory's CREATE2 child at the listing's id under the
//! code hash the factory answers, and must name this front and that listing; its desk contract is
//! the front's CREATE2 child at its desk id, which the front's `deskCellOf` must confirm. Anyone
//! may open a listing's door (`open`), and the door is the same code for every listing, so a door
//! cannot be a hostile one: what a listing's operator and maker choose is its tokens, its price
//! source and its asset module, and each is served only as the caller allows (the book
//! configuration's tokens, [`EfficientPropFeedBuilder::approved_price_sources`], and the listings
//! named with their gas, below). A second maker listing a pair whose price source is approved
//! and whose tokens are known is served with no change here or in configuration, where its
//! listing has no credit mode and is no stock listing.
//!
//! # Stock listings
//!
//! A listing of an asset-module pair (`doorView`'s pair word bit 15: a stock token) runs its
//! module's rules inside every quote and every fill (`PropAMMFront._asset`): the session its
//! print was made in must be one the listing allows and no older than that session's longest, a
//! quiet window runs from each session's start, and a halt, a latch, a jump the operator has not
//! acknowledged, a multiplier that moved, an issuer's pause and the 15 minutes before a scheduled
//! switch each refuse the fill (`PropAMMStockRules.check`). A refused size is refused by the
//! door's `quote` too, at the block asked, so a closed market shows as a side with no sample and
//! the listing publishes no book: it is never priced from a print the fill would not take. The
//! samples are the door's answers at the blocks the fill can land in (on Ethereum the next block
//! and, where the word carries, the one after), so a refusal that starts with the clock inside a
//! book's life shows in them.
//!
//! The rules read the token itself (its multiplier, its switch time, its pause), each read given
//! up to 28,000 gas, so such a fill spends what its token's code spends, which no class figure
//! holds. A stock listing is therefore served only where the caller named it with its gas
//! ([`EfficientPropFeedBuilder::credit_listing`], Ethereum only): in a credit mode as any credit
//! listing, and in none at that figure's custody pair. One not named is left out. A SESSION
//! listing with a closed phase, which fills from an aged print inside a wider band while its
//! market is closed, is not served in either phase.
//!
//! Two cases the samples do not cover, both bounded by the route's minimum output:
//! - the map's time is its slot's start. Where that slot is missed after the poll and the update
//!   lands in a later block, the door reads the later block's time, and a word that ages pays less
//!   there than the book;
//! - a carried side is sampled at the block after under the next block's state. A fresh update of
//!   the maker landing in the block after replaces the carried word, and the door then pays that
//!   update's price.
//!
//! The doors are filled only through their push swap: tycho-execution's `PropAMMExecutor` on
//! Ethereum, the one the chain's other `IPropAMM` venues use, sends the input to the door and calls
//! `swap(…, block.timestamp)`. A book is never filled any other way.
//!
//! A book holds no route to the block it was read for. A route priced from it that lands in a
//! block without the maker's update ahead of it (a block of a builder that does not carry the
//! maker's updates, a block the update missed, or a block past the side's life) is refused by the
//! door, the whole route reverts, and the taker pays its gas. Only the next block's updates are in
//! the map; the books show no other block.
//!
//! A listing's tip cap reads as on Base, and a stored cap of zero is none.
//!
//! # What this integration depends on
//!
//! On Ethereum the quote contract's layout is configured
//! ([`EfficientPropDeployment::quote_layout`]), and the only one read is `"builder"`, the layout on
//! Ethereum and BNB Chain described above; any other value, or none, is refused when the feed is
//! built. On Base the setting is refused if given: Base's path samples the doors and reads no quote
//! contract (its layout there is not read).
//!
//! Each poll reads the front proxy's EIP-1967 implementation slot, at the latest block and again
//! at the block every door is read at, and ends the feed when it holds an implementation outside
//! the configured allow-list: an upgrade the integration was not checked against may change what a
//! door's quote means. The listing records' settings, controls, session and source words are read
//! from the front's storage at their layout in those implementations. The factory and the front are
//! configured by the caller ([`EfficientPropDeployment`]), with the desk contract creation code
//! hash on Ethereum; the feed checks that the factory serves that front and, on Ethereum, that the
//! front's `CELL_CODEHASH` is the configured one: a mismatch ends the feed, so nothing is listed
//! that cannot be simulated.

use tycho_common::Bytes;

mod feed;
mod ladder;
mod overrides;
mod reader;
mod slots;
pub mod state;

pub use feed::{EfficientPropFeed, EfficientPropFeedBuilder};
pub use state::{CreditGas, DirectionGas, EfficientPropState};

/// Protocol system stamped on every component this integration emits; tycho-execution keys the
/// generic pAMM encoder on it.
pub const PROTOCOL_SYSTEM: &str = "efficient_prop";

/// Component type stamped on every listing this integration emits.
pub(crate) const PROTOCOL_TYPE: &str = "efficient_prop_listing";

/// Static attribute naming the listing door a swap goes to, which the encoder packs into the swap.
pub const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// Static attribute holding the listing's taker tip cap, wei per gas above the base fee,
/// big-endian: a swap sent with a higher priority fee reverts. Absent where the listing has no cap.
pub const TIP_CAP_WEI_ATTRIBUTE: &str = "tip_cap_wei";

/// The one quote contract layout this integration reads: see
/// [`EfficientPropDeployment::quote_layout`].
pub const QUOTE_LAYOUT_BUILDER: &str = "builder";

/// One PropAMM front and the listing-door factory that opens doors onto it, on the chain of the
/// feed's [`BookFeedConfig`](crate::book::BookFeedConfig).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EfficientPropDeployment {
    /// The layout of the quote contract the makers' updates write, on Ethereum, where the feed
    /// reads it: required there and only [`QUOTE_LAYOUT_BUILDER`], the layout on Ethereum and BNB
    /// Chain (one desk contract a maker, a pair of words a listing). Any other value is refused
    /// when the feed is built. Unset on Base, and refused if set: Base's path samples the doors
    /// and reads no quote contract, whose layout there is not read.
    pub quote_layout: Option<String>,
    /// The front's proxy.
    pub front: Bytes,
    /// The listing-door factory, whose `FRONT` must be `front`.
    pub factory: Bytes,
    /// The creation code hash of the desk contracts, which the front's `CELL_CODEHASH` names: a
    /// desk contract is the front's CREATE2 child at salt `(1 << 252) | desk` under it. Required
    /// on Ethereum, where makers' updates land in blocks; refused on Base. The doors' creation
    /// code hash is not configured: it is read from `factory`.
    pub cell_codehash: Option<Bytes>,
    /// The front implementations this integration was checked against. A poll that finds the
    /// proxy on any other ends the feed.
    pub implementations: Vec<Bytes>,
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use tycho_execution::encoding::evm::{EFFICIENT_PROP_PROTOCOL_SYSTEM, PAMM_ADDRESS_ATTRIBUTE};

    /// tycho-execution keys the generic pAMM encoder on the protocol system and reads the door
    /// from the attribute, keeping its own copy of each; this holds the two crates to one
    /// spelling.
    #[test]
    fn the_component_carries_the_names_the_encoder_reads() {
        assert_eq!(super::PROTOCOL_SYSTEM, EFFICIENT_PROP_PROTOCOL_SYSTEM);
        assert_eq!(super::PAMM_ADDRESS_ATTRIBUTE, PAMM_ADDRESS_ATTRIBUTE);
    }
}
