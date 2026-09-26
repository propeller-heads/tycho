# LunarBase Substreams

Indexes LunarBase Prop AMM state on Base and BNB Smart Chain into Tycho-compatible
`BlockChanges`. Both manifests use `base_lunarbase.wasm`. Changes carry absolute
values at transaction level, including Base Flashblocks, so consumers can apply
partial-block updates and Substreams rollback messages consistently.

## Configured pools

These addresses match `LunarBaseIntegrations/mainnet/addresses.json` version 0.4.1.
Implementation addresses are upgradeable and must be verified at the validation block.

| Setting | Base | BNB Smart Chain |
|---|---|---|
| Manifest | `base-lunarbase.yaml` | `bsc-lunarbase.yaml` |
| Network / chain ID | `base` / 8453 | `bsc` / 56 |
| Pair | Native ETH / USDC | Native BNB / USDT |
| Pool proxy | `0x0000eFC4ec03a7c47D3a38A9Be7Ff1d52dD01b99` | `0x00007904d186680C709519e71f4Dc3e2DF8f1b99` |
| `token_x` | `0x0000000000000000000000000000000000000000` | `0x0000000000000000000000000000000000000000` |
| `token_y` | `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913` | `0x55d398326f99059fF775485246999027B3197955` |
| Token decimals X / Y | 18 / 6 | 18 / 18 |
| Configured start / bootstrap block | 51297755 | 121835899 |
| `quote_caller` (Tycho router) | `0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67` | `0x7f3D12BBaFb8955E51B3ab9588b34c8ad95BDA4e` |

Caller addresses come from `crates/tycho-execution/config/router_addresses.json`.
Configuring the BSC router here does not assert that its LunarBase executor is deployed
or registered.

Singleton parameters have this form:

```text
pool=0x...&token_x=0x...&token_y=0x...&bootstrap_block=51297755&quote_caller=0x...
```

For multiple known pools, use:

```text
pools=0xpool:0xtokenX:0xtokenY:bootstrapBlock,0xpool2:0xtokenX2:0xtokenY2:bootstrapBlock2&quote_caller=0x...
```

Both `map_protocol_components` and `map_protocol_changes` must receive the same
parameters. Each pool becomes a component keyed by its proxy address. `quote_caller`
is required and applies to every pool in the manifest; it must match the caller
seen by the Pool during execution.

## State schema and quote compatibility

Package 0.2.0 requires the corresponding Tycho simulator using
`lunarbase-pmm-math` 0.4.1. The Pool's v0.4 punishment affects the current trade;
successful swaps also change stored directional fees for subsequent quotes.

- `anchor_price_x96` now contains the complete uint160 anchor as 20 big-endian bytes.
- `max_punishment_x24` replaces `concentration_k` and uses four big-endian bytes.
  `MaxPunishmentX24Set` updates it; its deployment default is zero.
- `PunishmentApplied` sets absolute directional fees without changing
  `latest_update_block`. Only `StateUpdated` refreshes the operator update block.
- `Sync` supplies active reserves. These differ from raw contract balances, which
  can include pending deposits and fee buckets.
- `blacklist_fee_multiplier` stores the uint256 multiplier in 32 big-endian bytes;
  `BlacklistFeeMultiplierSet` replaces it. `quote_caller_whitelisted` is a one-byte
  boolean updated only by `WhitelistSet` events for the configured caller.

Historical quotes from implementations before v0.4 are unsupported by the new
simulator. Replaying their events to reconstruct later state does not make those
earlier quotes compatible. This package does not identify the v0.4 activation block
or automatically gate historical quote requests by implementation version.

Quotes use the indexed caller policy: a whitelisted caller has multiplier one;
otherwise the blacklist multiplier applies, with raw zero interpreted as one to
match the Pool getter. This is necessary even when the execution ABI is unchanged:
at Base block 51297469 the configured Tycho router was not whitelisted and the
blacklist multiplier was 100. Assuming a multiplier of one would misprice its swaps.

Changing `quote_caller` requires a fresh replay because whitelist events for other
addresses were previously ignored; replace any bootstrap snapshot with one for the
new caller. Quotes from this feed are specific to that caller; reconcile its
whitelist status and multiplier with block-pinned getters.
Fee-accounting buckets remain unindexed. Reserve transitions assume standard
tokens and sufficient capacity for all fees to be credited to their buckets.

## Bootstrap and historical replay

The component is created only on its exact `bootstrap_block`, which must contain
a successful transaction with a log from that pool. Every map/store must start no
later than that block. Choosing a later start also requires updating the bootstrap
parameter; changing only `initialBlock` loses component discovery.

For an existing pool, `bootstrap_states` supplies the complete state at the end of
the parent block. Its value is a JSON object keyed by lowercase pool address; each
entry contains `block_number`, `block_hash`, `quote_caller` and all eleven fixed-width
`attributes`. The exact JSON is embedded in both module parameter strings. The WASM
does not read a fixture file or make a runtime snapshot request.

The fixtures below retain the snapshot object plus its capture evidence. To derive
the parameter value from a fixture before packing, use
`json.dumps({capture["pool"]: capture["snapshot"]}, separators=(",", ":"))` and append
it as `&bootstrap_states=<JSON>` to both module parameter strings. Passing a file
path as `bootstrap_states` is unsupported.

The parser requires `block_number == bootstrap_block - 1`, the configured caller,
all attributes and their expected byte widths. At bootstrap it verifies the actual
parent block hash, seeds the complete attributes and active reserve balances, then
applies the bootstrap block's events. A mismatched parent hash stops indexing.

Without `bootstrap_states`, initialization starts paused with zero price, reserves,
fees and maximum punishment, block delay two, multiplier one and a non-whitelisted
caller. These defaults are appropriate for a replay from verified deployment,
not for an already operating pool after an upgrade.

The Base manifest starts at **51297755** with a complete snapshot from parent block
**51297754**, hash
`0x206fa0a273e401dd2eea44d9a2fae97bb3aeff4e15e3a9a6e85d5fae96810a27`.
[The Base capture fixture](bootstrap/base.json) records the first pool-log transaction
`0x89f3ab2178d63f32a9b15caf7c53225f1dca3bef15e147a2f393487712fff7e6`
at the replay start.

The BSC manifest starts at **121835899** with a complete snapshot from parent block
**121835898**, hash
`0x723d5311d9e312cf0040da3dca6b056a80814889cc6abc23b3c27f68b734d461`.
[The BSC capture fixture](bootstrap/bsc.json) records the first pool-log transaction
`0x8cc377be7958278c637c45419c2754c5ed953914cb375294bb9fa4fbcdefcc97`
at the replay start. These transactions identify component discovery during replay,
not the original pool deployments or the v0.4 upgrade transactions.

Both fixtures record the RPC source, checked block hashes, implementation, caller
policy and six observed quote results in both directions. Snapshot bytes encode
block-pinned getter results; quotes are observed `eth_call` responses, not generated
expectations from the Rust simulator. Each parent hash was checked again after
capture. The captured Base caller has multiplier 100, while the BSC caller has
multiplier one; neither caller was whitelisted at its snapshot block.

## Validation and release

The Base range covers **51297755–51297765** and the BSC range covers
**121835899–121835909**. Both enable component checks, snapshot decoding, simulation
and execution from their verified parent snapshots. From `protocols/testing`, run:

```bash
cargo run -- range --package base-lunarbase --chain base
cargo run -- range --package bsc-lunarbase --chain bsc
```

The BSC harness uses `https://bnb.streamingfast.io:443`, listed in the
[official Substreams endpoints](https://docs.substreams.dev/reference-material/chain-support/chains-and-endpoints).
This authenticated Substreams endpoint is separate from the archive JSON-RPC URL
used to validate state and execute simulated swaps.

See [the test harness instructions](../../testing/CLAUDE.md) for RPC, Substreams and
isolated database requirements; the harness resets its configured database.
The complete Substreams/RPC/database end-to-end range tests have not been run as
part of preparing these captures; the presence of their configurations does not
establish passing integration tests.

Before publishing or enabling either updated feed:

1. Verify the active implementation and compare all indexed quote inputs against
   block-pinned getters after the v0.4 upgrade.
2. Compare quotes and simulated swaps in both directions with on-chain execution,
   using the actual caller fee policy. Include fee changes after a swap, punishment
   cap boundaries, reserve limits and the exact freshness boundary.
3. Publish the matching Substreams schema and simulator together, rebuilding
   snapshots by replay for the configured caller, using a complete verified parent
   snapshot when starting after deployment. Old snapshots lack the required
   punishment and caller-policy fields and encode the anchor at the old width.
4. Release each manifest separately using package `base-lunarbase` and config file
   `base-lunarbase` or `bsc-lunarbase`, following the package release instructions.

For the release workflow, select the `base-lunarbase-0.2.0` tag and dispatch
the `Release Substreams` workflow once with `package=base-lunarbase,
config_file=base-lunarbase`, and once with `package=base-lunarbase,
config_file=bsc-lunarbase`. The release script's default manifest search uses the
package's `base` prefix, so BSC requires the explicit second argument. The equivalent
script commands, run from `protocols/substreams` at that tag, are:

```bash
./release.sh base-lunarbase base-lunarbase
./release.sh base-lunarbase bsc-lunarbase
```

These release commands upload to the configured S3 repository. For a local
validation without publishing, build with the committed lockfile and pack each
manifest to a local file:

```bash
cargo build --locked --package base-lunarbase --target wasm32-unknown-unknown --release
substreams pack base-lunarbase/base-lunarbase.yaml -o /tmp/base-lunarbase-v0.2.0.spkg
substreams pack base-lunarbase/bsc-lunarbase.yaml -o /tmp/bsc-lunarbase-v0.2.0.spkg
```

The BSC manifest enables indexing configuration only. The repository has no
registered BSC LunarBase executor address or default live LunarBase subscription.
Verify the deployed executor, router registration and caller fee policy before
adding those production registrations. The generic execution test harness uses
the existing LunarBase runtime fixture through a local bytecode override.
