# Getting started

## Testing an EVM node's RPC capabilities

Before pointing the indexer at a new chain or node provider, check that the node supports every
JSON-RPC method the indexer uses. `test-evm-rpc-capabilities.sh` is chain-agnostic — it discovers a
historical block and calls Multicall3, which is deployed at the same address on virtually every EVM
chain.

```bash
RPC_URL=https://... TRACE_RPC_URL=https://... ./scripts/test-evm-rpc-capabilities.sh
```

`TRACE_RPC_URL` is optional and falls back to `RPC_URL`, exactly as the indexer does. Checks are
grouped by feature: `--core-only` covers what any deployment needs, `--dci-only` covers dynamic
contract indexing. DCI needs capabilities on both endpoints — the hooks DCI runs its slot detectors
on `RPC_URL`, only the entrypoint tracer uses `TRACE_RPC_URL`. Run `--help` for the full list of
environment variables (block offset, batch sizes, test contract).

The script exits non-zero when a required capability is missing. `eth_gasPrice` and
`eth_maxPriorityFeePerGas` are reported as warnings: the indexer itself does not call them, and
either one satisfies downstream consumers. The header comment lists each method alongside the
component that needs it.

---

## Migrating a PR from a related repo

When a PR is open in a related repository (`tycho-protocol-sdk`, `tycho-simulation`,
`tycho-execution`) and the work needs to land in this monorepo, use `migrate-pr.sh`.

### Prerequisites

1. Clone (or have a local checkout of) the source repository.
2. **Resolve any merge conflicts first**: rebase the source branch onto the source repo's `main`
   before migrating. Unresolved conflicts produce broken patches that fail during `git am`.

   ```bash
   cd ../tycho-protocol-sdk
   git fetch origin
   git rebase origin/main <branch-name>
   # resolve conflicts, then git rebase --continue
   ```

### Usage

Path mappings are **looked up automatically** from the source repo name. Just pass the repo
path and branch — no mapping arguments needed for known repos:

```bash
./scripts/migrate-pr.sh <source-repo-path> <branch-name>
```

| Source repo | Mappings applied automatically |
|---|---|
| `tycho-protocol-sdk` | `substreams→protocols/substreams`, `evm→protocols/adapter-integration/evm`, `protocol-testing→protocols/testing` |
| `tycho-simulation` | everything → `crates/tycho-simulation/` |
| `tycho-execution` | everything → `crates/tycho-execution/` |

```bash
# tycho-protocol-sdk PR — no extra args needed:
./scripts/migrate-pr.sh ../tycho-protocol-sdk ah/ENG-5053/fluid-indexing

# tycho-simulation PR:
./scripts/migrate-pr.sh ../tycho-simulation ah/my-feature
```

### Custom / extra mappings

Pass additional `src:dst` arguments to extend the default mappings (e.g. to bring over a
CI file change, or to migrate a new repo not yet in the table):

```bash
./scripts/migrate-pr.sh ../tycho-protocol-sdk ah/my-feature \
  ".github/workflows:protocols/ci"
```

For a repo not in the table, pass all mappings explicitly and add the repo to the table
in `migrate-pr.sh` for future use.

### Known manual steps after applying

The script automates path rewriting and strips common problem cases, but some things
need manual handling:

**Cargo.lock**: always stripped and must be regenerated after migration:
```bash
cargo check --workspace
git add -p  # stage only the Cargo.lock changes you want
```

**Cargo.toml / source file context conflicts**: when `-C0` can't apply a patch, the script
retries with `--reject`. Git applies all hunks it can and writes `<file>.rej` files for
the rest. Use `wiggle` to apply them — it uses word-level diffing and inserts
`<<<<<<<`/`=======`/`>>>>>>>` conflict markers for anything it can't resolve automatically:

```bash
brew install wiggle  # one-time setup

# Apply all .rej files; conflict markers appear in-place for anything unresolved
find . -name '*.rej' | while read -r rej; do
  target="${rej%.rej}"
  wiggle --merge "$target" "$rej" && rm "$rej"
done

# Resolve any remaining conflict markers in your editor, then:
git add <resolved-files>
git am --continue
```

**`include_str!()` and path literals**: the script rewrites path segments on added content
lines alongside the diff headers. A reference like `../../evm/test/executors/X.json` in
`protocol-testing/src/` is automatically rewritten to `../../adapter-integration/evm/test/executors/X.json`
so it resolves correctly from `protocols/testing/src/`.

### After migration

1. Run `cargo check --workspace` to regenerate `Cargo.lock`.
2. Push the branch and open a PR against this repo.
3. Close the original PR with a comment linking to the new PR.

---



## Compare scripts

All comparison scripts rely on using an archive node. You will need to set it using the 
`ETH_RPC_URL` env var.


### UniswapV2 & Balancer

These scripts are made to verify our data against a trusted source.

To run them you will first need to get some data from Tycho RPC. Use the state endpoints to 
get the state of the protocol you want to check and store the result in a json file with this 
name format: `{protocol}_{block_number}.json`. For example `uniswap_v2_10000.json`

Then and run it with the following command:
```bash
python compare-uniswap-v2.py <block_number>
```

Note, the script uses web3. If you have not got it installed already, you will need to do so:
```bash
pip install web3
```


### UniswapV3

You'll need the requests library installed, then pass block and pool addresses to compare:

```bash
python scripts/compare-uniswap-v3-the-graph.py \
    19510400 \
    0x1385fc1fe0418ea0b4fcf7adc61fc7535ab7f80d \
    0x6b6c7beadce465f8f2ada88903bdbbb170fa1f10
```