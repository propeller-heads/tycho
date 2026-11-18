# Architecture: layers and the contract between them

How tycho's layers depend on each other and what each one promises the next. An implementation
session reads this file with the knowledge file of the layer it works in; a review applies the
contract checks below to every change that touches a protocol in any layer, or a shared type or
registry. The architecture itself is described in `.claude/CODEBASE.md`.

## Contract checks

The layers are decoupled as code but share a contract: simulation consumes what the indexer emits
(attribute names and byte formats and, for VM protocols, which contracts and slots are tracked),
and execution consumes `ProtocolComponent` and optionally the simulation state. A reviewer who sees
one layer cannot find a break in that contract, so these checks look across them.

1. **Emit ↔ consume.** For every pair the change touches, the producer and the consumer agree:
   attribute names and their byte formats; component id ↔ adapter `poolId` ↔ executor; the
   `protocol_system` across the integration test yaml, `extractors.yaml`, the encoder registry and
   the executor configs.
2. **Each layer stands alone.** The indexer has no simulation knowledge; simulation works from any
   source that supplies the documented attributes (Substreams, snapshot feeds, overrides); execution
   needs only `ProtocolComponent` plus the optional state, with no new `ProtocolSim` downcasts; the
   crate dependency directions hold.
3. **Trace each direction.** For each protocol whose flow the change touches across an interface,
   follow one trade per direction: chain state → indexed attributes and balances → snapshot and
   delta decode → quote and returned state → limits and price → encoder → executor and router →
   receipt. At each step record the units, conversions, checks and state changes, and report where
   two steps disagree.
4. **Shared types and registries.** For a change to a shared type, field or registry entry, follow
   it from every place that writes it to every place that reads it, instead of a trace per protocol.

A protocol is matched across layers by its `protocol_system`. In a registry or config that lists
many protocols, only the entries the change edits count as touched.

## Rules

Each rule is a record: its id, which review findings cite; the rule; why it matters; how to spot a
violation in a diff (Spot); the kinds of files it applies to; and the tools that check it, in part
or in full (`.claude/skills/tycho-review/SKILL.md` lists the file kinds).

<!-- pieces -->

### contract-emitted-signal-has-consumer
Every attribute, flag or pause state the indexer emits has a consumer that reads and honours it, shipped in the same change or already present; an attribute nothing reads is not emitted. 'The same change' is the integration as integration-lands-as-one-change (`integration-lands-as-one-change`) defines it: its one PR, or every PR of an external integration reviewed together, when those PRs state their merge order and the spkg is not released before the consumer merges.
Why: A signal nobody reads does nothing downstream while looking like a fix, and dead attributes are output every later change must keep in sync.
Spot: New attribute names, status values or pause reasons emitted by indexing with no reader in tycho-client or tycho-simulation; static attributes copied from a sibling fork that no decoder reads.
Applies: substreams, rust, rust-test · look first at lines matching `attribute|attr|pause|paused|status|static|flag`
Checked by: repo-script `attribute-has-consumer`

### contract-delta-transition-forward-compatible
delta_transition ignores attributes it does not recognise instead of erroring; that every emitted attribute is consumed is asserted in a test, not enforced at runtime.
Why: A delta_transition error becomes a fatal stream decode error, so one newer package emitting an extra attribute stops the stream for every protocol on it, and no package can add an attribute until all clients upgrade.
Spot: A delta_transition arm that returns an error for attribute names outside a known list, often with a hand-copied list of decoder-injected names (block_number/block_timestamp).
Applies: rust, rust-test · look first at lines matching `delta_transition|attribute|unknown|MissingAttribute`
Checked by: ast-grep `rust-delta-transition-injected-names`; ast-grep `rust-delta-transition-rejects-unknown`

### chain-specific-values-not-global
Chain-specific addresses and parameters are never global constants in any layer: Rust branches on Chain (constants named per chain), Solidity takes them as constructor immutables or per-chain subclasses, substreams take them as params, and multi-chain artifacts are not named after one chain.
Why: A global constant is silently wrong on every other chain the protocol deploys to, and the error surfaces only at rollout.
Spot: New `const *_ADDRESS` without a chain in its name or a Chain match; Solidity `constant address` for a deployment address; substreams modules with literal addresses; directory names fixing a chain for a multi-chain deployment.
Applies: substreams, rust, solidity, config, script
Checked by: ast-grep `rust-chainless-address-const`; ast-grep `sol-address-literal-constant`

### contract-manual-updates-mark-every-change
An indexer that marks updated components explicitly (manual_updates) marks every component it sends any change for, balance-only changes included, or does not use manual_updates.
Why: The VM applies balances only inside update_pool_state, which a manual_updates component runs only with an update marker, so balance-only deltas are silently discarded and pools simulate against stale balances.
Spot: `manual_updates` in static attributes together with a balance loop (aggregate_balances_changes, fan-out of shared-holder balances) that never calls mark_component_as_updated; markers only on storage-write paths.
Applies: substreams, rust · look first at lines matching `manual_updates|mark_component_as_updated|balance|updated`
Checked by: repo-script `manual-updates-marks-balances`

### contract-attribute-name-and-format
An attribute is emitted and read under exactly the same name and byte format by the indexing package and by both simulation paths (snapshot decoder and delta_transition), and an attribute the simulation already understands is reused before a new one is invented.
Why: Attributes are matched by exact string and byte layout; a name mismatch freezes the value at its snapshot without any error, and a format mismatch fails at decode time in production.
Spot: New `Attribute { name: "..." }` strings: grep the simulation crate for each; differing case styles (camelCase vs snake_case) between emit and `.get("...")` sites; changed byte encodings of existing attributes.
Applies: substreams, rust, rust-test · look first at lines matching `attribute|attr|get\(|name|static`
Checked by: repo-script `attribute-has-consumer`

### contract-emit-decomposed-values
The indexer emits decomposed, semantically named values rather than raw packed storage words, so the storage layout stays inside the indexing package.
Why: Raw words push the packing rule and slot layout to every consumer, duplicate the unpacking on both sides, and turn an upgrade that relocates slots into a cross-layer change.
Spot: Attribute names like `total_and_external_shares` or `*_and_*` holding a whole 32-byte slot; an unpack helper implemented both in the substreams package and in tycho-simulation.
Applies: substreams, rust · look first at lines matching `attribute|attr|slot|unpack|_and_|>>|<<|shift|mask|split`
Checked by: ast-grep `rust-packed-attribute-name`; repo-script `attribute-has-consumer`

### standalone-execution-input-is-common-data
Execution's input is tycho-common data: a Swap built from a ProtocolComponent and Tokens, with protocol parameters read from static_attributes or user_data; a ProtocolSim state is attached only where encoding needs a quote at encode time.
Why: If an encoder needs simulation state for ordinary pools, execution can no longer be used without running simulation.
Spot: Encoders calling swap.protocol_state() for protocols that are not quoted at encode time; encoders requiring values that are neither in static_attributes nor user_data.
Applies: rust, rust-test · look first at lines matching `protocol_state|static_attributes|user_data|ProtocolComponent|swap|encod`
Checked by: ast-grep `rust-quote-at-encode-blocks-on-quote`

### standalone-quoting-without-feed
The part of an off-chain integration that execution needs at encode time (states and the client that fetches a binding quote) depends on no feed or transport machinery, and feed errors and quoting errors are separate types with no conversion between them.
Why: Execution would otherwise inherit streaming and polling dependencies it never uses, and the quoting half could not move to a shared crate.
Spot: Quote clients or states importing feed loops, sockets or pollers; From impls between a feed error enum and a quote error enum.
Applies: rust, cargo
Checked by: ast-grep `rust-quote-feed-one-trait`; ast-grep `rust-state-imports-feed`

### standalone-heavy-stacks-behind-features
Optional heavy stacks stay behind their crate features (execution's alloy/reqwest behind evm; simulation's revm and off-chain feeds), so `cargo check --workspace --no-default-features` keeps building.
Why: Ungated uses force the full EVM/HTTP stack on consumers who only want the models, and CI fails the no-default-features check.
Spot: alloy/revm/reqwest uses without #[cfg(feature = ...)] in modules compiled without the feature.
Applies: rust, cargo · look first at lines matching `alloy|revm|reqwest|feature|cfg|evm|default-features`
Checked by: repo-script `feature-off-heavy-deps`; ci `cargo check --workspace --no-default-features --locked (already in CI)` (full)

### contract-offchain-emits-simulate-ready-states
Data that is not indexed or block-anchored (off-chain books, quotes) is emitted as simulate-ready states; it is not serialised into the indexer's attribute/wire format only to be decoded again in the same process, and it does not impersonate a block header.
Why: The round trip costs CPU and memory per update and drops configuration the wire format does not carry, and a timestamp posing as a block number broke a consumer.
Spot: Off-chain sources producing component-with-state/attribute bytes for the indexer stream decoder; json_serialize into attribute maps followed by a decoder in the same crate; header types carrying timestamps in block-number fields.
Applies: rust, rust-test · look first at lines matching `serializ|json|attribute|header|timestamp|decod|rfq|off_?chain|quote|state`
Checked by: ast-grep `rust-offchain-json-into-attributes`

### contract-protocol-system-registered-everywhere
A protocol_system string (vm:\<name>, rfq:\<name>, bare) is identical, and registered, in every place that dispatches on it: Substreams integration yaml, extractors.yaml, the simulation decoder registration (with a decoder test), the stream builder's EXCHANGES_REQUIRING_FILTER when the protocol has a client-side filter in filters.rs, the swap-encoder registry, the config JSONs, the executor deploy script, the gas-estimator lists and the integration-test stream processors; gas-estimator entries mirror the executor's `getTransferData` per direction.
Why: A missing registration or a typo fails silently: no pools or no calldata, an executor never deployed, gas mis-estimated (skewing solver profitability), or a protocol the integration test never exercises.
Spot: New or renamed protocol_system strings, or new evm/protocol/\<x>/ modules; grep shows the string in fewer than all of the expected places or no register_decoder call. Integration PRs adding an encoder/executor or RFQ client without touching deploy-executors.js, gas_estimator.rs (PROTOCOLS_OUTPUT_TO_ROUTER, PROTOCOLS_NEEDING_APPROVAL) or crates/tycho-integration-test stream processors. A new or changed filter function in crates/tycho-simulation/src/evm/protocol/filters.rs without a matching EXCHANGES_REQUIRING_FILTER entry in evm/stream.rs.
Applies: substreams, rust, rust-test, config, script, integration-test, solidity
Checked by: repo-script `gas-estimator-mirrors-transfer-data`; repo-script `protocol-system-registry`; pr-script `protocol-system-registered-everywhere`

### contract-vm-component-id-hex
A VM component id is valid hex of at most 32 bytes from which the adapter can recover what it needs, because tycho-simulation hex-decodes it into the adapter's bytes32 poolId.
Why: An id that is not hex, is too long, or lacks the pool's identity cannot be passed to the adapter.
Spot: New VM substreams packages choosing component ids that are not hex or exceed 32 bytes; adapters that need data the id does not carry.
Applies: substreams, solidity
Checked by: repo-script `vm-component-id-hex`

### contract-native-token-zero-address
Tycho-level models represent the native token as address(0) (Chain::native_token); the router's 0xEeee marker appears only in execution's encoders, and a protocol's own native sentinel stays inside that protocol's simulation or encoder.
Why: Mixing the markers across layers makes components, tokens and calldata disagree about which address means native.
Spot: 0xEeee... literals in indexing, tycho-common or shared simulation code; address(0)->0xEeee conversion outside swap encoders; encoders hardcoding the literal instead of ROUTER_ETH_ADDRESS.
Applies: substreams, rust, solidity · look first at lines matching `eeee|native|address\(0\)|zero|ROUTER_ETH_ADDRESS|0x0{40}|eth`
Checked by: ast-grep `rust-native-sentinel-literal`; ast-grep `rust-router-eth-address-outside-encoders`

### contract-dto-mirrored-in-python-client
Source: `.claude/knowledge/python.md` § DTO Sync with Rust; `crates/tycho-common/CLAUDE.md` § Client Sync
Spot: Diffs to crates/tycho-common/src/dto.rs, Chain variants or tycho-indexer services/rpc.rs without matching tycho-client-py/python/tycho_indexer_client/dto.py changes.
Applies: rust, python
Checked by: repo-script `chain-enum-mirrored`; pr-script `dto-mirrored-in-python-client`

### per-protocol-failure-isolation
Source: `.claude/CODEBASE.md` § Key Architectural Patterns › Extractor-per-protocol
Spot: Single loops or queues serving several protocols where one's backpressure blocks the rest; error paths that pause or drop every component sharing an entrypoint or contract.
Applies: rust

### protocol-logic-in-protocol-module
Protocol-specific logic (filters, exclusivity or capability checks, per-protocol state hooks, address constants, which directions a component trades) lives in that protocol's module or is emitted by its indexing package as an attribute; shared code, consumers and encoders reach it only through a trait hook with a default implementation (defined in tycho-common where execution needs it) or through the indexed data, never by matching on protocol_system strings or downcasting to concrete types.
Why: Protocol branches in shared code grow with every integration, couple every protocol to one protocol's concepts and are missed when the protocol changes; matching on a system name misses the same venue published under another name.
Spot: A shared module that names a protocol (a `UniswapV4State` downcast or match in the decoder, an Ekubo-only constant exported at crate level, one protocol's filter body in filters.rs, a static attribute inserted by the client decoder); consumer code branching on `component.protocol_system == X::PROTOCOL_SYSTEM` to decide directions or capabilities. as_any().downcast_ref::\<...State>() in crates/tycho-execution/src, or imports of simulation state types in encoders (a downcast there also makes tycho-execution depend on tycho-simulation).
Applies: rust, substreams, solidity
Checked by: ast-grep `rust-execution-no-downcast`; ast-grep `shared-code-branches-on-protocol`

### static-values-read-from-static-map
Values fixed for a component's lifetime are emitted once as static attributes and read by the simulation decoder from the component's static attributes; they are not duplicated into the mutable state attributes, and the decoder never expects them in the state attribute map or in deltas.
Why: Duplicated immutable data can diverge from the static copy, and a decoder that looks for a static value in the state map fails to decode every component.
Spot: A Substreams mapper copying static_attributes into attributes (with_attributes on the same keys), or a decoder reading pool/token addresses via the state-attributes map passed to decode/delta_transition.
Applies: substreams, rust · look first at lines matching `static|attribute|attr`

### execution-tests-share-no-files
tycho-execution tests share no files with tycho-simulation: Foundry tests and fs_permissions never read simulation's test assets, and cross-layer data reaches executor tests only through calldata.txt written by the Rust encoder tests in tycho-execution.
Why: A file-level dependency couples the two crates' test suites and their release cycles as much as a crate dependency would, without the build ever showing it.
Spot: `vm.readFile("../../tycho-simulation/...")` or a foundry.toml fs_permissions entry pointing outside crates/tycho-execution.
Applies: solidity-test, config, rust-test · look first at lines matching `readFile|fs_permissions|tycho-simulation|\.\./|read_file|path|calldata`
Checked by: ast-grep `sol-test-reads-outside-execution`

### one-chain-per-component
New indexer, storage and cache components hold exactly one chain, because every deployment runs one chain per process: they take a Chain, not a slice of chains or maps keyed by chain.
Why: Per-chain maps that must stay in sync add loops, guards and failure modes that no production build exercises.
Spot: A new struct with HashMap\<Chain, ..> fields or a constructor taking &[Chain], next to builders that call single_chain().
Applies: rust, rust-test · look first at lines matching `chain`
Checked by: ast-grep `multi-chain-indexer-component`

### workspace-features-target-neutral
Workspace-level dependency entries carry only target-neutral features, so tycho-common still compiles for wasm32; features that need OS facilities (uuid's v4/fast-rng through getrandom) are enabled in the Cargo.toml of each crate that uses them.
Why: An OS-dependent feature added to a workspace entry reaches tycho-common through feature unification and breaks its WASM build.
Spot: A feature added to a `[workspace.dependencies]` entry that pulls getrandom, std-only I/O or a native TLS/crypto backend, or tycho-common enabling such a feature outside dev-dependencies.
Applies: cargo · look first at lines matching `feature|getrandom|uuid|tls|rustls|openssl|native|crypto|std|workspace`

### new-chain-in-every-list
A change that adds a chain (a Chain variant, or the chain in one list of supported chains) adds it explicitly everywhere the repo enumerates chains: an explicit arm in every per-chain match table (which by exhaustive-matches, 258, has no default arm; an existing table that still has one gets the explicit arm, and its default arm is reported as pre-existing) (client timing, quickstart default tokens, TVL thresholds, executor lists), CI deployment targets, hosted-endpoint docs, executor deployment config and scripts, and per-chain tests.
Why: Chains missing from one list are silently undeployable, undocumented or untested.
Spot: A diff adding one chain to a workflow option list, docs table or deployment JSON while sibling lists in the repo lack it. A PR adding a Chain variant without touching every `match` on Chain with a `_ =>` arm (grep the workspace).
Applies: any
Checked by: ast-grep `chain-match-wildcard-arm`; clippy `clippy::wildcard_enum_match_arm`; clippy `clippy::match_wildcard_for_single_variants`

### crate-layering
Each crate depends only on the layers below it: tycho-common is a leaf with no workspace dependencies, and dependencies only some consumers need are optional behind features; tycho-execution depends on no workspace crate but tycho-common; tycho-simulation never depends on tycho-indexer, tycho-storage or tycho-protobuf, takes tycho-client's feed types and tycho-ethereum for byte conversion only, and has tycho-execution as a dev-dependency only; Substreams packages depend only on tycho-substreams and the substreams crates and reach the indexer only through the protobuf messages.
Why: Execution must be usable by solvers that bring their own pricing; any extra layer drags simulation or indexing dependencies into every encoder user. Simulation must run from any feed (or a recording) without indexer, database or Substreams machinery. Indexing logic must build to wasm and evolve independently of indexer internals. Every crate, execution and client included, depends on tycho-common, so a heavy unconditional dependency lands in all of them.
Spot: Additions to crates/tycho-execution/Cargo.toml [dependencies] naming tycho-*; `use tycho_simulation`/`tycho_ethereum`/`tycho_client` in crates/tycho-execution/src; New tycho-indexer/tycho-storage/tycho-protobuf/diesel deps in crates/tycho-simulation/Cargo.toml; tycho-execution moving from [dev-dependencies] to [dependencies]; new tycho_ethereum uses beyond BytesCodec; Path dependencies from protocols/substreams/* onto crates/*; package code assuming indexer behaviour not expressed in the protobuf; Non-optional additions to crates/tycho-common/Cargo.toml such as database, EVM, HTTP or runtime crates
Applies: cargo, rust, substreams
Checked by: cargo-deny `deny-workspace-layers`; ast-grep `rust-simulation-tycho-ethereum-scope`; cargo-deny `deny-substreams-workspace-crates`; repo-script `feature-off-heavy-deps`
