# Verification: the verifier's brief and evidence methods

How a review finding is confirmed or refuted before anyone acts on it. `tycho-review` hands each
non-trivial finding to a fresh verifier; the same methods serve anyone checking a claim about the
code or the chain.

## The verifier's brief

- **What a verifier receives:** the claim, its anchor (file and lines at the head) and the
  preconditions it rests on. Not the reviewer's reasoning: the verifier builds its own.
- **Every precondition needs evidence:** a file:line, a command with its pinned inputs and output,
  a chain read at a named block, a test result. Restating the reasoning is not evidence.
- **A refutation needs counter-evidence of its own.** One without it leaves the finding unverified,
  which makes it a question.
- **Record how each fact was confirmed:** the method used and its result go into the finding.
- **Pick an oracle independent of the code under test.** A test that calls the same production
  helper on both sides proves nothing about that helper. Keep consistency (two parts agree),
  correctness (both match the deployed contract or the spec), tightness (a limit is the real one)
  and coverage (the path actually ran) apart: proving one does not prove the others.
- **Builds or runs the change:** each method says whether it compiles or executes code from the
  change. Those methods run only in the review's worktree, one finding at a time, and never for an
  untrusted change; when they are needed and cannot run, the finding becomes a question that says
  why.

<!-- pieces -->

## General methods

### read-pinned-dependency-source
Back any claim about a dependency's behaviour by reading the source of the exact version the lockfile pins (or the published tarball, when it differs from an in-tree copy) and cite file:line.
Why: Plausible but wrong mechanisms about library internals lead to fixes for the wrong problem, and an in-tree crate can share a version number with a published one that behaves differently.
Steps:
1. Read the version from Cargo.lock (not Cargo.toml).
2. Open \~/.cargo/registry/src/*/\<crate>-\<version>/ (or `cargo download`/crates.io tarball) and cite path:line.
3. For proto/wire types, compare fields and tags with the in-repo type.
4. For a crate pinned to a crates.io version that also exists in-tree, diff the tarball against the in-tree source for the fix the package relies on.
5. If the claim is about runtime behaviour, confirm with a minimal harness (e.g. a local websockets server) before asserting it.
Builds or runs the change: no

### venue-api-live-probes
Establish an external API's real behaviour (supported environments, refusals and their codes, minimum accepted size, latency) by probing its live endpoints, bisecting thresholds in both directions across several inputs and environments, and pin the results in opt-in live tests.
Why: Provider behaviour drifts from, or is missing from, the docs; recorded refusals justify error classification, supported-environment lists and config defaults.
Steps:
1. Check the provider's own public listing (e.g. a supported-environments endpoint) against the config.
2. Send requests for an unsupported environment, a zero value and a below-minimum size; record status, code and message verbatim.
3. Bisect the minimum: start at the documented level and step +1, 0, -1, -2, -10 units and 50%/10%/1%, on several inputs and >=2 environments.
4. Time requests for latency.
5. Commit as opt-in live tests gated on the credential (e.g. `#[ignore = "Requires <KEY>"]`); never overlap runs against an API that allows one connection per key.
Builds or runs the change: no

### code-trace-with-permalinks
Back a failure or severity claim with a step-by-step code trace through permalinked lines (pinned commit), following the value or error from where it arises to where it is handled, and name the test that would confirm a trace not yet executed.
Why: Severity (whole pipeline vs one item, error vs silent loss) depends on the exact path, and a linked trace lets the author check each step instead of trusting the conclusion.
Steps:
1. Pin the PR head SHA and link every step as blob/\<sha>/path#Lx-Ly.
2. Follow the path end to end (e.g. parser -> state update -> storage layer -> error type -> the caller's error policy).
3. Name the error type/branch reached at each hop.
4. If not executed, write 'traced by hand' and name the test (e.g. an end-to-end integration test) that would confirm it.
Builds or runs the change: no

### drive-code-under-test-with-chosen-inputs
Prove a behaviour claim by running the code under test on chosen inputs (out-of-range, repeated, boundary, a live instance of the degenerate state found in production data, or the same inputs as a correct reference implementation) through its existing fixtures and reporting the concrete outputs in a table; when validating a port against the original, run both on every live input at one fixed point, classify each case as exact match, failure on both sides, or mismatch, and root-cause each mismatch.
Why: A gap read off the code can be mistaken; numbers produced by the code itself make the bug and its size undeniable.
Steps:
1. Reuse the file's own test fixture or constructor (no new mocks).
2. Call the function with the input the claim is about: beyond the reported limit, the same call twice, zero, or the same inputs passed to a sibling implementation known to be right.
3. Print the outputs and tabulate them next to the expected bound or the reference.
4. Offer the snippet as the failing test. Edge cases: query production data (or a local copy synced from it) for the degenerate condition, run the code under review on that instance with current data, and quote its identifier, state and result. Ports: a port whose only evidence is a few fixture tests, with no population-wide comparison against the original's reference implementation, gets the full-population run with the three-way classification.
Builds or runs the change: yes

### vacuous-test-by-wrong-implementation
Show a test is vacuous by naming a concrete wrong-but-self-consistent implementation (swapped key order, dropped writes, unchanged key) and pointing at the fixture data that lets it pass.
Why: Naming the passing wrong implementation demonstrates vacuity concretely instead of asserting it, and tells the author what the test has to distinguish.
Steps:
1. Read what the assertion compares and where its expected value comes from.
2. Invent the simplest wrong implementation that keeps the same helpers/constants (so expected and actual move together).
3. Inspect fixture data for degeneracy (two outputs byte-identical, symmetric pairs, zero values).
4. State: 'an implementation that does X would pass, because fixture Y has Z'. Optionally confirm with mutation-check-new-tests.
Builds or runs the change: no

### calibrate-style-against-siblings
Back a style or convention finding with a measurement of sibling code (ratio, count, or how other packages handle the same case), excluding generated files.
Why: A convention finding is only as strong as the evidence that it is the repo's norm, not the reviewer's taste.
Steps:
1. Pick 3+ comparable siblings (other packages or modules of the same kind).
2. Measure the property with a script (e.g. blank-line ratio per package, excluding generated dirs; README section scope; how siblings handle the same edge case).
3. Report the PR's number next to the siblings'.
Builds or runs the change: no

### mutation-check-new-tests
Check that a new or cited test guards what it claims by deleting or reverting the guarded code, running the suite, and confirming that test (and only the expected ones) goes red.
Why: A test that stays green with its subject removed proves nothing about the bug it claims to cover.
Steps:
1. Identify the line/arm/attribute the test is meant to pin.
2. Delete or revert it locally (git stash the rest).
3. Run the narrowest test command (`cargo nextest run -p <crate> <filter>`, `forge test --mt`).
4. Report which tests fail; 'all N green' is the finding.
5. Restore.
Builds or runs the change: yes

### run-scripts-as-documented
Run every helper script a PR adds exactly as its usage text documents, at an input that stresses it (two relevant events in one batch, a high-traffic input), before accepting it.
Why: Script bugs (ignored env vars, greedy regex, wrong reference getter) only show on the documented invocation or a non-trivial input.
Steps:
1. Copy the invocation from the script's usage/README verbatim, with only the env vars it documents.
2. Run it once on a trivial input and once on a stressing one (two events in one batch, a high-traffic input).
3. Report the command and the failure or output.
Builds or runs the change: yes

### survey-all-implementors-and-consumers
Back 'first/only/nobody' claims and breaking-change judgements by enumerating every implementation of the trait and every downstream consumer of the type (including sibling repos), not by sampling.
Why: Whether a change is breaking or a precedent depends on who actually implements or serializes the thing.
Steps:
1. `rg 'impl .*<Trait> for'` (or the language's equivalent) across the workspace to count implementations and check the property on each.
2. For wire/serde types, grep downstream repos that depend on this one for where the type is serialized, persisted or replayed.
3. Report the count and the one place that matters.
Builds or runs the change: no

### reproduce-timing-dependent-behaviour
Prove a timing- or scheduling-dependent claim with repeated runs of an unmodified build matching the harness setup that vary the condition (first run vs cached rerun, constrained CPU under load), corroborated by external logs, and pin it with a regression test.
Why: One run of a race shows one outcome, and starvation bugs do not reproduce on a fast idle machine.
Steps:
1. Build release, unmodified, matching the harness setup.
2. Run the same scenario N>=5 times, separating first run from cached reruns.
3. For starvation: `systemd-run --user --scope -p CPUQuota=5% <cmd>` plus load; step the quota to find where it recovers.
4. Corroborate with external logs (e.g. Postgres 'unexpected EOF ... open transaction').
5. Regression test: stall the consumer and assert the connection stays healthy.
Builds or runs the change: yes

### minimal-experiment-for-tool-behaviour
Settle a claim about tool or library behaviour (parsers, test-framework helpers, deploy helpers) with a minimal experiment over edge inputs and report the result table.
Why: Behaviour of test-framework helpers, deploy helpers and parsers is easy to get wrong from docs alone, and a table shows exactly where two approaches diverge.
Steps:
1. Write a throwaway test (Foundry test, Rust test, script) that calls each approach.
2. Feed edge inputs: typed vs string vs hex vs garbage (e.g. [3000], ["3000"], ["0x0bb8"], ["hello"]), or run the same helper under both contexts (`forge test` vs `forge script`).
3. Tabulate input -> output/revert per approach.
Builds or runs the change: yes

### verify-suggested-fix-before-posting
Compile and run the suite on any replacement code a review comment suggests, under the package's pinned toolchain and target, and say so in the comment.
Why: A suggestion that does not compile under the pinned toolchain or breaks the suite is not a fix and costs the author a round trip.
Steps:
1. Apply the suggested diff locally.
2. Build under the package's pinned toolchain and real target (e.g. `cargo check --target wasm32-unknown-unknown` for a wasm package).
3. Run the package's tests and fmt.
4. State in the comment: toolchain, check, test count, fmt clean; call out any toolchain bump the fix needs.
Builds or runs the change: yes

### encoding-measured-from-production-data
Check wire-encoding assumptions by measuring the real byte lengths of stored values in production data and tracing the value through storage to the decoder.
Why: Shows whether fixtures and width checks match production bytes before proposing a stricter decoder.
Steps:
1. Query the production API or a local instance synced from production for the records in question.
2. Tabulate field byte lengths and encodings.
3. Trace the type through storage (its column type) to the decoder.
Builds or runs the change: no

### validate-fix-on-every-identified-instance
When analysis identified a set of affected instances, validate the fix against every one of them, not a sample.
Why: A fix tested on a few cases may miss variants in the rest of the identified set.
Steps:
1. Take the list the PR (or analysis) produced (N items).
2. Ask for, or run, the check over all N; report pass/fail counts and the outliers.
Builds or runs the change: yes

### release-builds-interleaved-benchmarks
Live, timing and performance runs use release builds, and comparative benchmarks alternate slices between both sides under the same external conditions, with warm-up discarded.
Why: Debug timings say nothing about the measured system, and sequential runs compare different conditions.
Steps:
1. Build with optimisations (`cargo build --release`; budget the compile time).
2. For A/B: interleave short slices A,B,A,B over the same window.
3. Drop the warm-up slice.
4. Report per-side distribution, not one number.
Builds or runs the change: yes

### record-how-each-claim-was-confirmed
Every confirmed fact in a review or PR carries how it was confirmed: the command with its pinned inputs, the identifier of the observed event or record, or file:line.
Why: A behaviour seen in one case is a hypothesis for the next; without the method it cannot be re-checked.
Steps: Next to each claim, add the reproducer: the exact command with its pinned inputs (version, timestamp, id), the identifier of the observed event, or the permalink; flag anything not confirmed as inferred.
Builds or runs the change: no

### cross-source-agreement
Check decoding correctness across several independent sources by sending the same request to all of them within a short window; one diverging source flags a source-specific bug.
Why: Agreement across independent wire formats is strong evidence; shared-pipeline bugs are hard to see per source.
Steps:
1. Pick one representative request.
2. Send it to every source within one minute, several samples.
3. Tabulate; investigate any column outside the normal spread.
Builds or runs the change: no

### base-vs-head-differential
Back a claim about what a change does or does not affect by running the same probe or metric on the merge base and on the head, for every affected user of the changed path, and attribute to the change only what differs.
Why: 'No impact' is ambiguous between measured and unchecked, and without a base run a review either blocks the change on inherited defects or misses the one regression it introduces.
Steps: 'No impact on X' or 'this predates the PR' asserted for a shared path without a before/after run. Replies or PR descriptions saying 'did not observe an impact', 'didn't see any difference' or 'should not affect' that name no run, inputs or comparison.
Builds or runs the change: yes

### check-labelled-literals-at-source
Check every literal that docs, examples or fixtures label as a specific external entity (an address, an id) by resolving it at the authoritative source and comparing the name it reports there, plus a format and length check, before accepting the label.
Why: A wrong-but-valid identifier runs fine and teaches readers or tests the wrong thing.
Steps: New or edited identifier literals next to a name or label in README commands, docs pages, example defaults or test fixtures.
Builds or runs the change: no

### size-unsupported-variants-by-usage
Before deciding which variants of an external system an integration must support, measure each unsupported variant's share of real usage and read each variant's actual capabilities from the authoritative source.
Why: Scoping without usage data either over-builds for negligible variants or covers only a minority of the system's real usage.
Steps: 
Builds or runs the change: no

## Tycho and chain methods

### on-chain-history-reconstruction
Decide whether an edge case has happened, can happen, and how often, and pin when on-chain behaviour changed, by reconstructing the contract's on-chain history: implementation/pointer slot writes, creation txs, first-set blocks, operator update txs, and block bisection of the same eth_call to the last good and first bad block.
Why: Severity of an indexing edge case depends on whether the venue's real history hits it.
Steps:
1. List writes to the EIP-1967 slot (0x3608...2bbc) and pointer slots: Etherscan internal txs/logs (Upgraded events) or bisect `cast storage <proxy> <slot> --block N`.
2. Find creation tx (`get_contract_creation`) and the block each dependency slot first became non-zero.
3. Count upgrades/rotations since deployment; check sampled blocks for the sentinel value.
4. Report blocks and tx hashes.
5. To pin when a behaviour changed: find one good block G and one bad block B with the same `cast call ... --block`, bisect on block number until B = G+1, quote both results and inspect the txs in B touching the contract.
Builds or runs the change: no

### state-reads-at-exact-blocks
Back claims about on-chain state and revert behaviour with block-pinned reads (`cast storage`/`call`/`balance`/`nonce`) at every block that matters: the package start block, the block before it, the fixture's pinned block, and the tip (or one stated block); resolve the EIP-1967 implementation at both start block and tip before trusting a cited implementation; for edge inputs call the venue's real entrypoints and capture the revert strings; add state overrides and blockOverrides {number, time} when the claim depends on them, and for lane-stamped pAMM quotes merge every override frame for block N and call at N-1 with number N.
Why: Storage layouts move in upgrades and balances/nonces change across blocks; a claim checked at the wrong block or against the wrong implementation is not evidence.
Steps:
1. `cast storage <proxy> 0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc --block <start>` and `--block latest`.
2. `cast storage/call/balance <addr> ... --block <start-1>`, `<start>`, `latest`.
3. For fork fixtures, `cast nonce <deployer> --block <pinned>` vs latest, and confirm the anvil fork reports the pinned value.
4. Quote command and result per block. Edge inputs: call the venue's real entrypoints (e.g. submit(0), wrap(0), unwrap(0)) with `cast call ... --block <N>` and capture revert strings. Replayed quotes: eth_call with stateOverride = merged override frames of block N and blockOverrides {number: N, time: ts(N)} at block tag N-1 (the number override avoids the staleness revert). Quote block, call and override set.
Builds or runs the change: no

### live-integration-test-run
Validate an integration with a live tycho-integration-test run against a local indexer synced to the tip with only that protocol enabled, over hundreds to a thousand blocks, reporting simulations, failures, simulated-vs-executed slippage and sample sizes.
Why: Range tests at fixed blocks miss live behaviour, and small samples are existence proofs, not statistics.
Steps:
1. Sync a local indexer (sync-integration skill) with only the protocol's extractor.
2. Release build.
3. Run with `--skip-messages-duration 30`, `RUST_LOG=tycho_integration_test=debug,info` (successes log at debug), under `timeout -s KILL <secs>` (it ignores SIGTERM).
4. Never two runs at once.
5. Report blocks, simulations, failures by kind, slippage per protocol, counts; add price-printer spot checks across quote amounts.
Builds or runs the change: yes

### decompile-unverified-bytecode
For unverified contracts, decompile/disassemble the runtime bytecode to recover the called functions, guards, mapping slots and screening rules, and confirm key conclusions with a trace or call.
Why: Without the decompiled logic, names, guards and storage-layout assumptions in the PR can't be checked.
Steps:
1. `cast code <addr> --block <N>` (resolve proxies first).
2. Decompile (Gigahorse / evmole-style / hevm, or Dedaub) to reconstructed Solidity; extract selectors and SLOAD/keccak patterns for mapping base slots.
3. Confirm key points with an eth_call or debug_traceCall.
4. Write findings as reconstructed code with 'inferred'/'traced' labels.
Builds or runs the change: no

### block-diff-sampling
Measure what an operation actually writes and how often state changes by sampling consecutive blocks or sample transactions: state-diff traces for which contracts/slots are written, log counts per block for update frequency.
Why: Claims about which components a swap touches, a filter's false-positive cost, or how stale delta-driven state gets need data, not assumptions.
Steps:
1. Pick a window ending at a stated block (e.g. 300 blocks, or 14 recent blocks).
2. Per block or tx: `debug_traceTransaction` with prestateTracer {diffMode:true} (or trace_replayBlockTransactions stateDiff) and tabulate (contract, slot) writes; or `cast logs --address <addr> --from-block A --to-block B` and count blocks with any log.
3. Report hits/total and which unrelated contracts matched the filter.
Builds or runs the change: no

### state-override-experiments
Test hypotheses about caller-, holder- or environment-dependent behaviour with eth_call/eth_simulateV1 state overrides: crafted code at msg.sender or tx.origin, the VM's own account setup, and transfer sequences with the actual pool as a party, each compared against a plain control.
Why: Inferences from disassembly or wallet-to-wallet probes can be wrong; overrides isolate each rule's effect.
Steps:
1. Control: same call from a plain caller of the same code size.
2. Variant: stateOverride {\<caller>: {code: \<crafted bytes>}} (marker bytes, selectors at offsets, minimal-forwarder shape, address prefix) or at tx.origin; diff prices.
3. To mirror the VM: override token addresses with the VM's TokenProxy code and no storage, call decimals()/balanceOf.
4. For token taxes: eth_simulateV1 sequence pair -> wallet and wallet -> pair with the real v2 pair as holder; replay the analyzer's bytecode with different holders.
Builds or runs the change: no

### resolve-unknown-selectors
Resolve unknown function or error selectors by hashing candidate signatures from the verified ABI, contracts/src and the venue's contracts, then confirm the meaning with an eth_call; don't rely on 4byte alone.
Why: Project-specific selectors are not in public databases, the revert may come from the venue rather than our contract, and a fix through an unnamed getter needs evidence it does what is claimed.
Steps:
1. `cast sig '<candidate signature>'` / `cast sig-event` over error and function signatures from `forge inspect <C> errors`, contracts/src and the venue's verified ABI (`get_contract_abi`).
2. Match against the selector in the trace/revert.
3. Confirm by eth_call (e.g. the getter returns the value the known storage slot holds).
Builds or runs the change: no

### amount-bisection-ground-truth
Establish ground truth for a limit by eth_call bisection over the input amount (largest input that settles, plus a one-raw-unit-more check), across pools x directions x blocks, and score both the current implementation and any proposed replacement against it, with a worked example from on-chain reads.
Why: One example can't show whether a limit error is systematic or which way it fails, and an untested alternative can be as wrong as the code it replaces.
Steps:
1. Grid: >=2 pairs x both directions x \~10 blocks.
2. Per case, bisect amount with `cast call <venue> 'swap/quote(...)' --block N` to the largest input that fills; check +1 raw unit fails.
3. Run the PR's getLimits at the same block; tabulate error %.
4. For a proposed formula, compute it step by step from on-chain reads at a fixed block (state getter, accumulators, balanceOf, exact-output quote) in a table, and score it on the same grid.
Builds or runs the change: no

### chain-derived-test-literals
Derive expected storage keys and values from chain (eth_getStorageAt at the independently computed mapping slot returns the known value) and use those literals in findings and proposed tests.
Why: Gives tests an expectation that doesn't come from the code under test.
Steps:
1. Compute the slot by hand (`cast index <keyType> <key> <baseSlot>`, nested for double mappings).
2. `cast storage <addr> <slot> --block N` and confirm it holds the known pool/value.
3. Put slot and value as hex literals in the suggested test.
Builds or runs the change: no

### precision-against-small-step-reference
Measure numerical error (finite-difference spot price, rounding in quotes) against a reference computed with a step well inside the top level at a fixed block, per pair and direction, and tabulate sign and size.
Why: Bias claims need magnitude and sign per case to judge them against spreads.
Steps:
1. Fix a block.
2. Reference: quote a tiny step well inside the first level (cast call at N), price = out/in.
3. Compare with the PR's spot_price at N per pair and direction.
4. Tabulate error % with sign.
Builds or runs the change: yes

### run-substreams-at-chosen-blocks
Reproduce an indexing suspicion or confirm Firehose data shape by running the package (or a throwaway module) against the live endpoint across the relevant block, with a control range, and compare emitted values to on-chain getters to the wei.
Why: Running the package turns a code-reading suspicion into a demonstrated output and catches wrong assumptions about call-tree shape before a design depends on them.
Steps:
1. `substreams run <spkg/manifest> <module> -s <before-upgrade> -t <after>` with a freshly computed snapshot/seed.
2. Control run on a known-good range.
3. For data shape: a throwaway map module printing call frames at one block.
4. Compare emitted values to `cast call <getter> --block N`.
Builds or runs the change: yes

### sim-vs-live-trace-comparison
Root-cause a simulation/venue disagreement by tracing the call in simulation (--vm-simulation-traces) and reproducing it live with cast state overrides, comparing the two traces frame by frame.
Why: Without frame-level comparison a probe flaw gets misattributed to simulator state and fixed in the wrong layer.
Steps:
1. Re-run the failing simulation with `--vm-simulation-traces`.
2. Reproduce live: `cast call --trace` with the same calldata and overrides at the same block.
3. Diff frames.
4. If the local DB is retention-pruned (mixed storage eras), reproduce at the tip instead.
Builds or runs the change: yes

### fill-replay-for-offchain-venues
Validate an off-chain venue's quotes by replaying real fills: record the stream per block, watch pool-level swap events, reconstruct each fill's amounts from receipt transfers, price them against the snapshot of exactly the fill block, and report median/p95 deviation and coverage gaps.
Why: Without replay against real fills, quote accuracy, unreachable pairs and under-advertised depth are guesses.
Steps:
1. Record the venue stream keyed by target block.
2. Collect fills from pool-level swap events (not only the router's).
3. Amounts from receipt Transfer logs.
4. Price each fill with the snapshot for its block.
5. Report n, median/p95 bps, fills on pairs not streamed, fills above ladder max.
Builds or runs the change: yes

### read-venue-contract-for-update-timing
Pin down when a venue's state changes (per block, per timestamp, per fill) by reading the contract code path that writes it, and combine with how the engine sets block context.
Why: Adapter assumptions about update timing determine how stale a quote can be.
Steps:
1. Read the writer function (verified source, or decompiled if unverified).
2. Find early returns keyed on block.timestamp/number.
3. Check what timestamp the VM simulates at (the indexed block's).
4. State the update trigger.
Builds or runs the change: no

### quote-and-balance-reconciliation
Validate a protocol integration by checking, at pinned blocks, indexed state against the on-chain getters and simulated get_amount_out against the venue's own on-chain quote function to the raw unit, across both directions and different token decimals, on a component whose live state exercises the case under review; for indexed balances and reserves, reconcile every component at one block (the extractor head), selecting the row version valid at that block (valid_from \<= ts(B) \< valid_to) and the getter that exposes the stored quantity.
Why: A run on an idle component passes without touching the condition at issue (e.g. retained protocol fees), and a pass rate alone does not show the numbers are right.
Steps: A validation report that cites only success counts, or tests a freshly created/inactive pool when the concern is state that only accumulates with activity. Population reconciliation: pick block B = extractor head; for every component read the row with valid_from \<= ts(B) \< valid_to (not the latest row) and compare with the getter that exposes the stored quantity (e.g. getReservesOf, not balanceOf of the vault); report matched/total. Temporal-table reads that bypass extraction-state-gateway are acceptable for this one-off check, not for committed tooling.
Builds or runs the change: yes

### gas-measured-as-engine-calls-it
Measure a venue or adapter call's gas the way the engine and router will incur it: standalone at a chosen block (the tip unless the claim is historical), from each caller that will make it (EOA, the TychoRouter address, the adapter) with matching outputs, never from the gasUsed of an inner frame of a historical tx; when calibrating a gas constant from such measurements, sample several tokens and balance transitions and subtract the 21k intrinsic transaction cost when the call runs as an inner call.
Why: Caller-dependent gas (e.g. scanning msg.sender's code) is invisible from a single caller and makes simulated gas wrong by orders of magnitude. Gas from reasoning or one pinned fork block misses what the engine hits at the tip, and inner frames under-measure by the cold-access surcharge already paid by earlier legs. A constant calibrated on one token or including the intrinsic cost under- or over-prices the venue in routing.
Steps:
1. `cast estimate --from <EOA>` and `--from <TychoRouter addr>` (and the adapter) for the same calldata at latest; compare return values with `cast call --from`.
2. If gas differs, `cast run`/debug_traceCall with callTracer from the router to find the frame (EXTCODECOPY/EXTCODESIZE loops).
3. Compare to the VM's trade.gasUsed for the same swap;
1. Code override: stateOverride {\<adapter>: {code: \<runtime bytecode from protocols/testing/fixtures/*.runtime.json>}}.
2. Binary-search `gas` on eth_call (`cast call --gas-limit`) at latest per pool/direction until it stops reverting OOG; compare with the engine's cap.
3. For venue calls: per real fill, debug_traceCall with callTracer at the fill's block, input amount pushed via balance override, read gasUsed.
4. Report per-case numbers.
1. Callers: `cast estimate --from <EOA>` and `--from <TychoRouter addr>` (and the adapter) for the same calldata at latest; confirm the outputs match with `cast call --from`. If gas differs, `cast run`/debug_traceCall with callTracer from the router to find the frame (EXTCODECOPY/EXTCODESIZE loops).
2. Adapter view calls: stateOverride {\<adapter>: {code: \<runtime bytecode from protocols/testing/fixtures/*.runtime.json>}}, binary-search the eth_call gas limit (`cast call --gas-limit`) per pool/direction until it stops reverting OOG; compare with the engine's cap.
3. Venue calls of real fills: per fill, debug_traceCall with callTracer at the fill's block, input amount pushed via balance override, read gasUsed.
4. Calibrating a constant: several tokens, zero/nonzero sender and receiver balance slots; subtract 21,000 when the measured call runs as an inner call.
5. Compare with the VM's trade.gasUsed for the same swap and report per-case numbers.
Builds or runs the change: no
