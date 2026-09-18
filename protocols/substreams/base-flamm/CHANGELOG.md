# Changelog

## v0.1.0

- Initial release: native indexing of Everlong FLAMM on Base from block 51154966. Two components per pool (swap
  and lever-up) created from `FLAMMFactory.PoolCreated` behind an invariant-hook codehash allowlist; the pool's,
  hook's, spread hook's, router record's, financing account's, price feed's and factory's storage words, the
  Morpho Blue market and position words, the IRM rate and the four Chainlink feeds (rotation, read access, rounds,
  the DualAggregator's reveal ring) as attributes; inventory balances from the tracked words; seeds and
  immutables as manifest params verified by unit tests against the chain.
- Components list no `contracts` (the indexer resolves the list against accounts the stream created; a native
  integration creates none): the pool's contracts are static attributes.
- A pool whose external words (Morpho market, position and IRM words, feed proxy words, the aggregators behind
  the proxies) the manifest does not track is refused at creation, with the missing names logged.
- The feed attributes are one function of the proxies' and aggregators' storage words (`feeds::feed_state`),
  emitted whole at creation and as a before/after diff for every transaction that writes one of the words; the
  aggregators' events are not read (the tests decode them to cross-check the layouts). A `Deletion` therefore
  only ever names a row the indexer holds: `tycho-storage` fails a block's write on a deletion of a missing row,
  which the previous event-driven rotation clearing (`secondary_round` / `cutoff` on an OCR2 or uptime feed,
  `answer` / `started_at` / `updated_at` / the access pair on the DualAggregator feed) would have triggered at
  the next proxy rotation. A rotation to an aggregator the manifest does not list leaves the feed with
  `aggregator`, `phase` and `access_controller` only.
- Per transaction, tracked writes are applied in ordinal order and the feeds are valued at the transaction's
  end; nothing is emitted for a pool in the transactions of its creation block before its creation transaction.
- A pool created without the leverage pair (`leverageHook == spreadHook == 0`) is indexed with zero hooks and
  codehashes and tracks no spread hook; a pair with one zero is refused.
- The words store is asked once per `(address, slot)` per block (`WordView` memoises the lookup).
- Balances are emitted for the tokens whose inventory a transaction moved (a Morpho accrual with no managed
  supply moves none).
- The `DualAggregator` round emits `feed:mo0:round` and the ring word only (no `answer` / `started_at` /
  `updated_at`), the schema's `mo0` set.
- End to end without the hosted harness (`src/e2e_tests.rs`, `testdata/e2e_blocks.json.gz`): real Base blocks
  from the creator's deployments through the pool's creation, activation, every settled swap, two of the
  deposits, a withdrawal, a keeper recenter, the range test's stop blocks, three recent Chainlink-round blocks
  and the block that unpaused leverage (the other deposits, withdrawals and keeper transactions reach the fold
  through the synthetic catch-up blocks between them), fed to the module cores with the stores kept as the
  engine keeps them and folded as the indexer and the client hold them; the fold is the stream fixture the
  `flamm` `ProtocolSim` replays against the chain's own `previewSwap` / `previewLever` answers and every
  settled swap's receipt. The range test's expected components and skip flags are checked against what the
  package emits and the chain justifies. `flate2` is a dev-dependency (the fixtures are gzipped). The recorders
  of the fixtures are committed under `crates/tycho-simulation/src/evm/protocol/flamm/testdata/gen`.
- The first write of a tracked word the indexer holds no row for is emitted as a `Creation`, not an `Update`
  (a word never written, seeded or carried as a zero row by the creation snapshot): `tycho-indexer` restores a
  reverted `Update` from the row's prior value and counts one without any as an attribute miss, while a
  reverted `Creation` deletes the row. The replays assert the rule on every attribute of every block.
