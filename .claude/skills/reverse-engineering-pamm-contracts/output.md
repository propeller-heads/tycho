# Output of a pAMM reverse-engineering run

One directory per venue, `research/pamm/<venue>/` unless the user names another, holding:

- `README.md`, in this order:
  1. **Contracts**: every address on the swap path with its role, code hash (printed by
     `inspect_contract.py`), verification status, and the block the analysis used.
  2. **Entry points**: selectors of swap, quote, price update and admin setters, with who may
     call each, how the input arrives (pull, push, callback), and entry points that skip the
     router with their share of fills.
  3. **Component identity**: what one pool is (an address, a router plus pair key, an engine
     plus token) and which directed token pairs it quotes.
  4. **Storage**: the slot table with packed bit ranges, every slot as a formula an indexer
     can compute per component, which slots the swap reads and writes, and every config
     value the model treats as constant with the setter that changes it.
  5. **Behaviour**: formulas with rounding, every check with its revert reason, the state
     transitions of a swap, and the external calls by kind. Each item cites the source or
     TAC lines it came from.
  6. **Proof**, per layer: "proven" or "not proven". Case count and mismatches per layer and
     entry point; the read-set diff and what each extra read is; the sequence tests; the
     second block; the deliberate break that made the diff fail; contract coverage with each
     unseen edge explained by an invariant, a case or a mock, grouped by reason; model branch
     coverage. A layer with an unexplained edge is "not proven".
  7. **Freshness**: the cadence numbers and what they mean for a quote from indexed state.
  8. **ProtocolSim hints**: for each method, where its answer comes from. Do not write the
     Rust; give formulas, slots and checks, and mark hints on unproven layers.
     - `get_amount_out`: the swap model; gas of the pricing call alone (transfers excluded),
       stated as such; `new_state`: every input the quote reads that the swap changes, in
       any contract, token balances included, not only the swap's own writes.
     - `get_limits`: every check that bounds size, as max input and max output, and the
       minimum input; a zero output is an error, never a price.
     - `fee`: per direction at zero size, explicit or folded into the spread, and whether it
       grows with size.
     - `spot_price`: the exact small-size formula with its rounding, so it matches a tiny
       quote.
     - `query_pool_swap`: is output monotone in input? A closed-form inverse, the contract's
       exact-out path, or a search.
     - `delta_transition`: the slots and balances indexing must deliver, per component;
       whether a balance belongs to the component or to a shared account; how each change is
       detected (event, call, or a storage diff of a shared contract filtered by the
       component's slots).
     - `apply_block`: what the execution block changes (freshness, block- or time-keyed
       state, decays).
     - **Quote timing**: a quote against block N executes at N+1. Pick one per venue and say
       why: quote from indexed state with an error when stale; an indicative price; or quote
       under pending overrides from the maker's published state, naming where that state is
       published.
     - Inputs a pool state cannot hold (`tx.origin`, per-sender pricing, blacklists, the
       priority fee).
  9. **Gaps**: anything not modelled, not tested or not proven, said plainly.
- `model.py` and the difftest script that runs it, runnable with the commands in the README.
- `cases.json`: inputs, overrides and on-chain results per case, and the storage words the
  model read (one shared snapshot plus per-case overrides is enough).
- `coverage.txt`: the final `coverage.py report` per contract, and the model's branch
  coverage.
- `watch.json` for `liveness.py`: the assumptions that, when broken, mean the flow has left
  this deployment. Venues rarely change code in place: they move a proxy's implementation,
  repoint a router getter, flip a pause or active flag, or redeploy everything at new
  addresses. Watch each: proxy slots, router pointers, every admin-writable slot a swap
  check reads (use "not equal" checks for values that are retuned), price-update and swap
  activity (filtered on the venue's key when updates go to a shared contract), and the
  venue's owner and pricers calling or creating contracts outside the known set, which is
  how the successor shows up. The file must pass `liveness.py` before you finish.
