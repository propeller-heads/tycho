# Coding in Rust

## Commands

During development, use simpler commands to help you iterate:

```bash
cargo check --all-features
cargo test
cargo clippy --all-features
```

After a task is done, run `cargo +"$RUST_NIGHTLY" fmt --all` and the tests that cover what you changed (e.g.
`cargo nextest run -p tycho-indexer`). Save the `/run-ci` skill for when you open a PR or change
something workspace-wide. See `.claude/skills/run-ci/SKILL.md` for the canonical commands.

Then check whether docs need updating with `/sync-docs`.

## Rules

The rules below are what reviews check Rust code against. Formatting is `rustfmt.toml`'s and lints
are the workspace's; a rule here covers what they cannot check. Each rule is a record: its id (which
review findings cite), the rule, why it matters, how to spot a violation (Spot), the files it
applies to and the tools that check it in part or in full.

<!-- pieces: rendered from candidates.json by candidates/knowledge.py; edit the pieces there, not below -->

### no-avoidable-allocation
Code does not pay for work it throws away or repeats: allocating fallbacks go through `unwrap_or_else`, per-item values are computed inside the branch that uses them, fixed hashes and keys are constants, collections are borrowed rather than cloned (preferring a dependency's borrow-taking function over one that forces a clone), and nothing is `collect`ed only to be iterated again.
Why: Hot paths (per event, per request) pay allocations, hex formatting and hashing for values that are discarded or never change.
Spot: `.unwrap_or(x.to_vec())` / `.unwrap_or(cfg.x.clone())`; a value formatted before a branch that uses it in one arm; `iter().cloned().collect()` where references suffice; hash or key builders over constants called in a loop; `.clone()` of a large state only to satisfy an API; `collect()` followed by another iteration; `Vec::contains` inside a loop. `.iter().find(|x| x.id == id)` inside a `for` over a second collection: index it into a map once before the loop.
Applies: rust, rust-test, substreams
Checked by: ast-grep `find-in-loop`; clippy `clippy::or_fun_call`; clippy `clippy::needless_collect`; clippy `clippy::redundant_clone`

### idiom-states-intent
Use the construct that states exactly the intent: `contains_key` when only presence matters, `[a, b, c].contains(&x)` for 'one of these', `else if`/`match` for mutually exclusive branches, and `.last()` rather than a loop that keeps the last element.
Why: A construct that does more than the intent (a fetched-then-discarded value, independent ifs over exclusive cases, a loop over every item) makes the reader check whether the extra matters, and does redundant work.
Spot: `map.get(k)?;` with the value unused; chains of `||` comparisons against constants; consecutive independent `if let Some(x) = try_decode_*(..)` calls on the same input; `for x in xs { last = Some(f(x)?) }`.
Applies: rust, rust-test, substreams
Checked by: ast-grep `or-chain-against-constants`; ast-grep `map-get-presence-only`; ast-grep `keep-last-loop`; clippy `clippy::manual_contains (default)`; clippy `clippy::search_is_some (default)`

### remove-single-use-indirection
Remove single-use indirections: a method or associated fn that does not use self is a free function, a wrapper used once is inlined, and an Arc around a value already behind an Arc is a plain value.
Why: Indirection without reuse adds names a reader must chase and implies sharing or state that does not exist.
Spot: A new `fn x(&self) -> &T { &self.t }` accessor with one caller, associated functions with no self usage, `Arc<AtomicBool>` in a struct only ever held via Arc.
Applies: rust, rust-test, substreams
Checked by: ast-grep `arc-atomic-field`; dylint `arc-inside-arc-held-struct`; clippy `clippy::unused_self`; clippy `clippy::self_only_used_in_recursion`; clippy `clippy::single_call_fn (review only)`

### invariants-at-construction
A type establishes its invariants once, in its constructor, and keeps the fields that carry them private (accessors only where a caller needs them), so methods do not re-validate on every call and callers cannot write fields that desynchronize; every other way to build a value goes through the same checked construction, so a validating type does not also derive a plain `Deserialize` (use `#[serde(try_from = ...)]` or a manual impl, with a malformed-input test that drives the public method that would otherwise panic).
Why: Per-method validation costs every call and can be forgotten in one method; pub fields let callers bypass the constructor's checks or desynchronize redundant data nothing re-validates.
Spot: A `validate_*()` call at the top of several methods of one type; `pub` fields on a type with a validating constructor or holding derived/redundant data; struct literals of such a type outside its module. `#[derive(Deserialize)]` on a struct whose `new`/`from`/`validate_*` enforces bounds, ordering or filtering.
Applies: rust, rust-test, substreams
Checked by: ast-grep `validate-in-several-methods`; dylint `struct-literal-outside-validating-module`; ast-grep `deserialize-bypasses-checked-new`; clippy `clippy::partial_pub_fields`

### required-config-in-constructor
Configuration with no safe default (target network or environment, timing parameters) is a required `new` parameter, not a defaulted field filled in by a later setter or a lone `with_*` builder beside a `new` that takes everything else; optional settings on a struct that may grow fields use builder-style `with_x(self, x) -> Self`, not `at_x(x) -> Self` constructors that read like sum-type variants.
Why: A defaulted environment or timing value silently misconfigures every consumer that forgets the setter and is only caught at runtime (or by a panic path), not at compile time.
Spot: One environment variant (e.g. the production network) used as a default in `new()`, `set_*`/`with_*` setters for values that change behaviour, or a single `with_*` builder on a type whose other fields go through `new`.
Applies: rust, rust-test, substreams
Checked by: ast-grep `lone-with-builder`; ast-grep `at-constructor-on-product`

### typed-deserialization-once
Parse external JSON once into typed structs with derived Deserialize (using the domain types' own serde impls for numbers and ids), not by walking serde_json::Value or re-parsing raw string fields in every method.
Why: Hand-walking `serde_json::Value` turns half a function into property lookups, and string fields re-parsed in several methods duplicate both the parsing and its error handling.
Spot: `serde_json::from_str::<Value>` followed by `.get("...")`/`.as_str()` chains; model fields typed String holding numbers/addresses that are parsed again later.
Applies: rust, rust-test, substreams · look first at lines matching `serde|json|deserializ|value|from_str|parse`
Checked by: ast-grep `json-value-walking`

### param-owned-when-copied
A parameter's form follows what the body does with it: one the body unconditionally copies (`.clone()`, `.to_owned()`, `.to_string()`, `.to_vec()`, `Arc::clone`) is taken owned; where only some paths or some callers need ownership, the caller decides (`String`, `impl Into<String>`, `Cow<'_, T>`, or two functions; likewise `PathBuf`/`Vec<T>`), and `&'static str` labels stay as they are; methods take `self` when they consume or rebuild, `&self` to read, `&mut self` to update in place.
Why: Borrow-then-copy makes every caller that already owns the value pay for a second copy, and hides the cost from the call site.
Spot: `fn f(x: &T)` or `Option<&T>` whose first use of `x` is a copy; `&self` methods that clone self to return a modified copy; callers that clone a value only to pass a reference the callee then copies again. `fn new(name: &str) -> Self { Self { name: name.to_string() } }` called with owned Strings; an unconditional clone at the top of a function whose owned value is used in one branch only.
Applies: rust, rust-test, substreams
Checked by: repo-script `borrow-then-copy`; clippy `clippy::needless_pass_by_ref_mut`; ast-grep `fn-with-ref-param`

### single-pass-over-collection
Visit a collection once: derive per-item values (indices, predicates, unwrapped fields) in the loop that already walks it instead of a second pass that repeats the same unwrap/expect or predicate.
Why: Two passes duplicate the predicate and its expect message, so they can drift, and the sort/pre-check that only existed for the second pass becomes dead weight.
Spot: Two loops over the same Vec with the same `.expect(..)`; a filter function and its caller applying the same predicate; an early return that changes no output.
Applies: rust, rust-test, substreams

### enums-not-bool-flags
Mutually exclusive states and modes are an enum with documented variants (the default named by what it guarantees), not boolean flags, and easily confused domain values get newtypes.
Why: A bool whose `false` needs prose to explain misleads and leaves no room for another mode; flag and primitive soup admits invalid combinations and argument swaps the compiler cannot catch.
Spot: New bool fields or params whose doc explains what `false` means in other terms; structs gaining several bools that encode one state; functions taking several same-typed primitives with different meanings.
Applies: rust, rust-test, substreams
Checked by: clippy `clippy::fn_params_excessive_bools`; clippy `clippy::struct_excessive_bools`; clippy `clippy::option_option`

### match-on-variants-not-ids
Match on enum variants directly rather than on derived numeric ids.
Why: Matching numbers duplicates the enum's own mapping.
Spot: `match x.id() { 1 => ..., 56 => ... }` on an enum that has the variants.
Applies: rust, rust-test, substreams · look first at lines matching `match|=>|id\(`
Checked by: ast-grep `match-on-numeric-id`

### drain-tasks-before-exit
Before exiting on a graceful stream end, await in-flight background tasks (commit handles, spawned writers); returning from `#[tokio::main]` drops the runtime and cancels detached tasks mid-operation.
Why: The last unit of work (here the final DB commit) is lost nondeterministically depending on how fast the stream ends, producing timing-dependent results.
Spot: `tokio::spawn` whose JoinHandle is only awaited when the next item arrives; `Ok(())` returns on an end-of-stream variant without draining pending handles or buffers.
Applies: rust, rust-test, substreams · look first at lines matching `spawn|joinhandle|handle|tokio::main|stream|ok\(\(\)\)|await`

### no-hidden-block-in-place
Do not hide tokio::task::block_in_place or Handle::current behind a method that reads as synchronous and pure; it panics on a current-thread runtime or outside a runtime, so the precondition must be explicit or the work moved to async.
Why: A trait method callers treat as pure now needs a multi-threaded tokio context and panics otherwise.
Spot: block_in_place / Handle::current().block_on inside a non-async trait method or helper reached from sync code.
Applies: rust, rust-test, substreams · look first at lines matching `block_in_place|handle::current|block_on`
Checked by: clippy `clippy::allow_attributes_without_reason`; clippy `clippy::disallowed_methods (default)`

### structured-logging
Log events carry variables as structured tracing fields (durations through the Debug sigil `?d`, not `.as_secs()`), not values interpolated into the message.
Why: Values baked into messages cannot be filtered, `.as_secs()` truncates sub-second durations to 0 and drops the unit, and error-level logs for self-healing conditions page people for nothing.
Spot: `format!`-interpolated ids/amounts in tracing messages; `*_secs = d.as_secs()` fields; `error!` inside a retry loop.
Applies: rust, rust-test, substreams · look first at lines matching `tracing|info!|warn!|error!|debug!|trace!|log::|as_secs|event!`
Checked by: ast-grep `tracing-format-interpolation`; ast-grep `tracing-as-secs-field`; ast-grep `error-log-in-retry-loop`

### one-outer-timeout
Enforce an overall time budget with one outer `tokio::time::timeout` around the whole retry loop; the inner attempt does no elapsed-time arithmetic.
Why: Deriving remaining time at every await point drifts (inconsistent attempt counts in logs, duplicated timeout messages) and sleeps away the budget in dead cases.
Spot: Multiple `Instant::now()`/`elapsed()`/`checked_sub` computations and several nested `timeout(...)` calls inside one request-with-retries function.
Applies: rust, rust-test, substreams · look first at lines matching `timeout|instant|elapsed|retry|attempt|checked_sub|deadline`

### secrets-skipped-in-serialize-debug
Secrets held by serializable or Debug structs are skipped in both (`#[serde(skip_serializing)]`, `#[debug(skip)]`), and a test serializes the struct and asserts the credential is absent.
Why: Deleting one attribute puts API keys into every recording/log with the suite still green.
Spot: A struct with api_key/auth_key/secret fields deriving Serialize or Debug; serde tests comparing field-by-field instead of asserting secrets are absent.
Applies: rust, rust-test, substreams · look first at lines matching `secret|key|token|password|auth|credential|serializ|debug`
Checked by: ast-grep `secret-field-serialized`

### async-tests-no-fixed-sleeps
Async tests must not rely on fixed sleeps; drive the condition to an observable event with a short interval and let an existing deadline report failure (and note that paused virtual time skips real I/O).
Why: Fixed sleeps make tests slow and flaky; start_paused advances time during real TCP handshakes, so the test observes nothing and passes vacuously.
Spot: `tokio::time::sleep(Duration::from_millis(1500))` followed by assertions in a test; `#[tokio::test(start_paused = true)]` on tests with real sockets.
Applies: rust-test, substreams · look first at lines matching `sleep|start_paused|tokio::test|pause|advance`
Checked by: ast-grep `fixed-sleep-in-async-test`; ast-grep `paused-time-with-sockets`

### error-variant-payloads
Public error variants carry payloads that match their name and that consumers can use (a message String when nothing inspects it), not foreign runtime types that also encode states the variant denies.
Why: A `Panicked(JoinError)` variant also represents cancellation, needs docs to explain it away, and pins the runtime crate in the public API.
Spot: Public enum variants wrapping `tokio::task::JoinError`, `Box<dyn Error>`, or other third-party types whose cases are broader than the variant's name.
Applies: rust, rust-test, substreams · look first at lines matching `enum|error|err\b|joinerror|box<dyn`
Checked by: ast-grep `error-variant-foreign-payload`

### no-positional-pairing
Two parallel collections are not paired by position (`zip` over a table declared elsewhere); each pair names both halves explicitly.
Why: Reordering or inserting a row silently mismatches names and widths, and `zip` silently truncates when lengths diverge.
Spot: `TABLE.iter().zip([a, b, c])` where TABLE is a const in another module; arrays of values whose meaning depends on index.
Applies: rust, rust-test, substreams · look first at lines matching `zip|\[\d|iter\(\)|const|static`
Checked by: ast-grep `zip-over-const-table`

### narrowest-visibility
Visibility is the narrowest that current users need: a module whose items are all crate-internal is itself `pub(crate)` rather than `pub`, and `pub(crate)` or `pub` on a module or item needs a caller outside its parent. Items not reachable from the crate root are `pub(crate)` (`private-module-items-pub-crate`).
Why: Widened visibility without a user invites coupling; restating a restriction on every item is noise that a reader has to check for consistency, and it has to be edited item by item when the boundary moves, whereas one module-level restriction is the single place that decides who can reach the code.
Spot: A `pub mod` whose items are all `pub(crate)`; a new `pub(crate) mod`/`pub mod` among private sibling modules; widened visibility with no caller outside the parent.
Applies: rust, rust-test, substreams · look first at lines matching `pub|mod\b`
Checked by: clippy `clippy::redundant_pub_crate`

### param-widest-borrow
A parameter the body only reads takes the widest borrow: &str over &String, &[T] over &Vec\<T>, &Path over &PathBuf.
Why: Narrow borrow types force callers to own a specific container for no benefit.
Spot: &String, &Vec\<T>, &PathBuf, &Box\<T> in function signatures.
Applies: rust, rust-test, substreams · look first at lines matching `&string|&vec|&pathbuf|&box|&mut string|&mut vec`
Checked by: clippy `clippy::ptr_arg (default)` (full); clippy `clippy::borrowed_box (default)` (full); clippy `clippy::needless_pass_by_value` (full); clippy `clippy::needless_pass_by_ref_mut` (full)

### return-what-callers-use
Functions return the type their callers use; they do not return a freshly formatted or allocated representation (e.g. a String id) that every caller converts straight back.
Why: Round-tripping through a representation costs allocations and parse errors at every call site.
Spot: Functions returning String/format!(...) whose callers immediately parse, hex::decode or from_str the result.
Applies: rust, rust-test, substreams
Checked by: dylint `returned-string-reparsed`

### errors-not-swallowed-with-context
Errors are never swallowed and carry context (what operation failed, on what input, what to do next); library code returns Result rather than panicking. An error whose message adds nothing to what the call already says (an integer or slice conversion, a timeout, a closed channel or semaphore, a poisoned lock, a clock before the epoch) is not appended to a message: `map_err` takes a named ignored binding (`|_out_of_range|`, `|_elapsed|`) and the message names the failure itself.
Why: Swallowed or context-free errors make failures invisible or undiagnosable; panics in a library take down the consumer.
Spot: let _ = fallible(), .ok() discarding errors, if let Err(_) = ... {} with no handling, unwrap/expect on runtime inputs in library code, errors without the offending value. `timeout(d, &mut handle).await.is_err()` that inspects only the elapsed case and discards `Ok(Err(e))` and `Ok(Ok(Err(e)))` of an awaited background task. `format!("...: {e}")` on a `TryFromIntError`, `TryFromSliceError`, ruint `FromUintError`, tokio `Elapsed`, channel `RecvError`/`SendError`, `AcquireError`, `PoisonError` or `SystemTimeError`, which only appends text such as "out of range integral type conversion attempted" or "deadline has elapsed".
Applies: rust, rust-test, substreams
Checked by: ast-grep `swallowed-err`; ast-grep `awaited-task-error-ignored`; clippy `clippy::map_err_ignore`; clippy `clippy::let_underscore_must_use`; clippy `clippy::unused_result_ok`; clippy `clippy::match_wild_err_arm`; clippy `clippy::unwrap_in_result`; clippy `clippy::panic_in_result_fn`

### exhaustive-matches
Matches on enums and destructuring of structs owned by the codebase are exhaustive: no `_ =>` arms and no `..` that would silently absorb a new variant or field.
Why: Wildcards turn an added variant or field into silently wrong behaviour instead of a compile error.
Spot: `_ =>` arms or `..` patterns over crate-defined enums/structs in the diff.
Applies: rust, rust-test, substreams · look first at lines matching `_\s*=>|\.\.|match|let\s`
Checked by: ast-grep `tuple-rest-pattern`; dylint `wildcard-over-crate-owned-type`; clippy `clippy::wildcard_enum_match_arm`; clippy `clippy::match_wildcard_for_single_variants`; clippy `clippy::rest_pat_in_fully_bound_structs`; clippy `clippy::rest_pattern_accessible_field (master only)`; clippy `clippy::unnecessary_rest_pattern (master only)`

### let-else-early-return
Early returns use let...else so the happy path stays unindented.
Why: Nested if-let/match pyramids bury the main path.
Spot: if let Some(x) = ... { \<long body> } else { return ... } or match with a single bail-out arm.
Applies: rust, rust-test, substreams · look first at lines matching `if let|match|else|return|\?`
Checked by: ast-grep `if-let-else-return`; clippy `clippy::manual_let_else`; clippy `clippy::redundant_else`; clippy `clippy::needless_continue`

### deterministic-selection
Any choice that affects output (which source, candidate or entry to use) is deterministic; it never depends on HashMap/HashSet iteration order.
Why: Iteration-order choices change between runs and processes, so results are irreproducible and differ per consumer.
Spot: `.iter().next()`, `find` or first-match over a HashMap/HashSet to pick a value that feeds results or ids.
Applies: rust, rust-test, substreams · look first at lines matching `hashmap|hashset|map|set|next\(\)|find|first|keys|values|iter`
Checked by: ast-grep `keys-next-pick`; dylint `hash-iteration-pick`; clippy `clippy::iter_over_hash_type (review only)`

### no-twin-methods-for-api-change
An API change is a plain signature change, not a defaulted twin method (foo_with_env) or a wrapper type introduced only to keep the old signature; an absent context is Option\<&T> with None meaning the old behaviour.
Why: Twin methods and compatibility wrappers double the surface that every implementor must keep consistent.
Spot: New *_with_* / *_v2 methods with a default impl delegating to the old one; new enum/wrapper types whose only purpose is to carry an optional parameter.
Applies: rust, rust-test, substreams · look first at lines matching `_with|_v\d|fn |trait|impl|enum|struct`
Checked by: ast-grep `twin-method-default-delegates`; clippy `clippy::ref_option`; clippy `clippy::ref_option_ref`

### minimal-trait-surface
Trait surfaces are minimal: a method already reachable through another trait does not reappear on a second one, and configuration belongs on constructors, not on trait methods or builders that only forward it.
Why: Duplicate entry points and forwarding setters multiply what each implementor must support and keep in sync.
Spot: Trait methods duplicating another trait's capability; builder setters that only forward to an inner config; config passed per call that never varies.
Applies: rust, rust-test, substreams · look first at lines matching `trait|impl|fn |config|builder|with_`
Checked by: ast-grep `forwarding-builder-setter`

### network-tests-ignored
Source: `crates/tycho-simulation/CLAUDE.md` § Conventions
Spot: New network tests using cfg_attr feature-based ignores or no ignore at all.
Applies: rust-test, substreams
Checked by: ast-grep `rust-network-test-feature-ignore` (full); ast-grep `rust-rpc-test-not-ignored`; clippy `clippy::ignore_without_reason`

### invalid-states-unrepresentable
State that the types can exclude is excluded by the types, not by runtime checks: a value with a required shape is parsed into a type that only holds valid values, fields that are present or absent together form one struct or enum variant, a distinct case is an Option or enum variant rather than an in-band sentinel (an empty string, 0, -1), a case that cannot occur in some configuration is absent from that configuration's type, and a branch for a state the types already exclude is deleted rather than handled.
Why: A runtime check or a defensive arm for a state the types could exclude moves a compile-time guarantee to the first input that hits it, and such an arm tends to drift from the guarantee it was meant to keep.
Spot: `ensure!`/`assert!` re-checking a length, range or format a type could carry (e.g. an address as `Vec<u8>` or `String`); two `Option` fields always `Some`/`None` together; a setter plus panic path for configuration that only some variants read; `unreachable!`/defensive match arms or error branches for states a caller already rules out; sentinel values (0, empty string, -1) standing for a distinct case. Functions returning `String::new()`/`""`/`0` to signify 'none' or a special mode, or maps keyed by such sentinel values. Docs saying 'zero if unknown' or 'use 0 if you don't know' for a numeric field that later feeds a sum, max or average; such a value is required or an Option that makes the aggregate unknown too. `bool.then(|| x).into_iter().collect::<Vec<_>>()` or a `Vec`/set only ever filled from a single `Option`: an at-most-one value stays an `Option`. A `Vec<String>` registry next to HashMap-keyed registries of the same items; a Vec of entries whose uniqueness is checked (or assumed) at runtime.
Applies: rust, rust-test, substreams
Checked by: ast-grep `hex-serde-vec-address`; ast-grep `empty-string-sentinel`; ast-grep `option-collected-into-vec`; ast-grep `zero-if-unknown-doc`; clippy `clippy::unnecessary_wraps`; clippy `clippy::unreachable`

### traits-abstract-over-real-variation
A trait abstracts over behaviour that actually differs between implementors: a required method whose every implementation has the same shape (an accessor, a constructor, `clone_box`/`as_any` boilerplate) is a shared field, a generic type or a blanket impl, not a trait method; provided methods that extend real variation are fine un-overridden, but one whose correctness depends on nobody overriding it, and whose signature does not prevent it, moves to an extension trait with a blanket impl or a free function.
Why: A trait over identical implementations abstracts over nothing and only lets generic code reach into a newtype, at the cost of one copy per implementor; and since Rust cannot seal a single method, a provided method that must not change is a convention any implementor can break for every `dyn` caller without a compiler error. The fix has a cost to weigh: a blanket-implemented extension trait can never be specialised per implementor on stable Rust, so it suits methods whose result could never sensibly differ, not optimisation hooks.
Spot: The same body (field read, field write, `Self { .. }` construction, `Box::new(self.clone())`, `self`) in every impl of a method, test doubles included; provided methods documented 'should not be overridden' or relied on by other methods' invariants; provided methods built only from other trait methods whose result type the implementor could construct differently. Not a finding: provided methods whose return type only the trait's crate can construct, which seals them without an extension trait (`Iterator::map` must return `Map<Self, F>`); extension methods over varying required methods (`map` over `next`), overridable reference implementations (`Iterator::nth`, `fold`), and a defaulted method added to a public trait so existing implementors keep compiling.
Applies: rust, rust-test, substreams · look first at lines matching `trait|impl|fn `
Checked by: ast-grep `clone-box-boilerplate`

### recover-poisoned-lock
A lock guarding data that cannot be left torn (simple get/insert/clear on a map) recovers from poisoning with `PoisonError::into_inner` through one small accessor, rather than `.expect("poisoned")` at every access.
Why: One panic while holding the lock turns every later access on every thread into a panic.
Spot: `.read().expect(..poisoned..)` / `.write().unwrap()` on a lock inside a value shared across threads.
Applies: rust, rust-test, substreams · look first at lines matching `mutex|rwlock|lock\(|read\(\)|write\(\)|poison`
Checked by: ast-grep `poisoned-lock-expect`

### public-docs-reference-public-items
Docs on public items refer only to items visible in the public API; a link or pointer to a crate-private item (for its defaults, rationale or behaviour) is replaced by the stated fact or relation. A public setter states its default by linking a public constant ('Default: [`DEFAULT_X`].'); a literal 'Default: \<value>.' only where the default is not a named constant.
Why: Readers of the published docs are sent to items they cannot see, and intra-doc links to private items do not resolve.
Spot: A `///` doc on a `pub` item that names or links a `pub(crate)`/private function, constant or module ("see default_http_feed_config", "for the feed loops"). A public setter whose default is a crate-private constant with no 'Default: \<value>.' line, or with the line worded differently from sibling setters ('Default 16.'); a setter that restates a value owned by a public constant or function instead of linking it.
Applies: rust, rust-test, substreams · look first at lines matching `///|//!|#\[doc`
Checked by: rustdoc `rustdoc::private_intra_doc_links (24 warnings in CI's cargo doc today; add it to RUSTDOCFLAGS -D)`

### no-whole-crate-reexport
A library does not re-export a whole third-party crate; it re-exports only the specific items its public API needs, after deciding that those items' versions become part of its own compatibility promise.
Why: Re-exporting a crate wholesale makes every bump of it a breaking change for the library.
Spot: `pub use some_dependency;` in lib.rs.
Applies: rust, rust-test, substreams · look first at lines matching `pub\s+use|extern crate`
Checked by: ast-grep `whole-crate-reexport` (full)

### producer-stops-without-receivers
A background producer task stops, or at least logs, once every receiver of its channel is gone, instead of discarding the send error and running forever.
Why: Orphaned tasks keep connections open and consume resources while producing nothing.
Spot: `let _ = sender.send(..)` inside a long-running loop, with a comment that a send error only means no receivers.
Applies: rust, rust-test, substreams · look first at lines matching `send|sender|channel|tx\b|_tx|broadcast|mpsc|watch|spawn|loop`
Checked by: ast-grep `send-error-discarded-in-loop`; clippy `clippy::let_underscore_must_use`

### port-input-assertions-as-errors
Code ported from another implementation mirrors that implementation's input assertions (index range, distinct indices, overflow) as returned errors, so no caller-supplied index or amount reaches unchecked indexing or arithmetic.
Why: An out-of-range index or overflowing input panics the process instead of failing one call.
Spot: Direct `arr[i]` or unchecked `+=` on values derived from caller input in a function that returns Option/Result.
Applies: rust, rust-test, substreams
Checked by: clippy `clippy::indexing_slicing (review only)`

### workspace-pinned-dependencies
A crate that the workspace already pins is declared with `workspace = true`; a member needing a newer version bumps the workspace pin and migrates the others instead of declaring its own version.
Why: Two versions of the same crate (and their transitive stacks, e.g. TLS) bloat builds and make types incompatible across crates.
Spot: A member Cargo.toml with `foo = "x.y"` where the root [workspace.dependencies] pins foo at another version; Cargo.lock gaining a second version of a crate.
Applies: cargo, substreams · look first at lines matching `=|version|workspace`

### runtime-handles-not-serialized
Runtime handles a value holds (HTTP clients, connection pools) are skipped in serialization, and the field's type expresses that a deserialized value lacks them (Option), so using the missing handle fails explicitly.
Why: Serializing a handle duplicates its configuration into every recorded value and gives each deserialized copy its own pool, while still pretending it can act.
Spot: `Arc<Client>` fields on Serialize/Deserialize state structs without `#[serde(skip)]`.
Applies: rust, rust-test, substreams · look first at lines matching `serde|serializ|deserializ|client|pool|arc<|skip`
Checked by: ast-grep `runtime-handle-serialized`

### rustdoc-on-the-item-it-documents
Comments that document an item (field, function, type) use `///` so rustdoc renders them, and a `///` block sits directly above the item it describes, since rustdoc attaches it to whatever item follows.
Why: A `//` field comment never reaches the docs, and a paragraph meant for one test or function placed above another is merged into that item's docs.
Spot: `//` comments directly above pub fields or items whose siblings use `///`; a `///` paragraph above an item that names a different item or describes behaviour the item below does not have.
Applies: rust, rust-test, substreams · look first at lines matching `//`
Checked by: ast-grep `plain-comment-on-pub-field`

### name-dense-closures
A closure called for several inputs that does its own lookup and error construction becomes a named private function whose name says what it returns.
Why: A multi-line closure with nested `ok_or_else(|| ...format!(...))` in the middle of a function makes the main flow hard to follow and cannot be named or tested on its own.
Spot: `let f = |..| { ...ok_or_else(|| Error(format!(...))) };` spanning several lines inside a longer function body, called for each of several inputs.
Applies: rust, rust-test, substreams · look first at lines matching `\|`
Checked by: ast-grep `dense-closure`

### docs-not-tied-to-visibility
Every field of a public type has a `///` line saying what it holds, under a summary noun that covers all fields, and an item keeps its behavioural docs (defaults, when a flag applies) when its visibility narrows.
Why: Callers read the implementation to learn what a field maps onto, a summary noun that excludes a field misleads about it, and the next reader of a now-private item rediscovers from the code what the deleted doc said.
Spot: A `pub` field without `///` on a type reachable from the public API, or a struct doc whose noun does not describe one of its fields; a diff that narrows an item's visibility and in the same hunk deletes or shortens its docs.
Applies: rust, rust-test, substreams · look first at lines matching `pub|///|//!`
Checked by: ast-grep `undocumented-pub-field`

### signature-carries-documented-guarantee
A guarantee a doc states that the signature can express is in the signature (a method documented as not modifying state takes `&self`), or the doc drops it.
Why: With `&mut self` the 'does not modify' promise is only a convention any implementor can break silently.
Spot: A doc saying 'not modified', 'idempotent', 'read-only' or 'pure' on a `&mut self` method.
Applies: rust, rust-test, substreams · look first at lines matching `///|//!|&mut self|#\[doc`
Checked by: ast-grep `mut-self-documented-read-only`; clippy `clippy::needless_pass_by_ref_mut`

### fixed-width-reads-checked-once
Fixed-width fields of a byte buffer are read so the length check and the reads cannot drift apart: one length check up front followed by infallible fixed-size copies, or one fallible accessor that is itself the check, with no per-field `expect` and no `map_err` that can never fire.
Why: A per-field expect panics once someone edits the preceding check, and an unreachable error message suggests a failure mode that does not exist.
Spot: `try_into().expect("N bytes")` after a `len()` check with an 'Infallible:' comment; `try_into().map_err(|_| ...("x should be 32 bytes"))` on a slice whose length a previous check already fixed.
Applies: rust, rust-test, substreams · look first at lines matching `try_into|len\(\)|expect|map_err|\[u8|bytes|slice|from_be|from_le|copy_from`
Checked by: ast-grep `try-into-expect-after-len`

### one-workspace-lockfile
A workspace member does not commit its own Cargo.lock; the workspace root's lockfile is the only one.
Why: Cargo ignores a member's lockfile, so it drifts from the versions actually built, and dependency bumps touch it for nothing.
Spot: A Cargo.lock beside a Cargo.toml whose directory is listed in the root `[workspace] members`.
Applies: cargo, substreams

### waits-select-on-stop
A supervisor or retry loop selects over its stop/control channel during every wait (backoff sleep, rebuild), so a stop request takes effect immediately.
Why: A Stop that arrives during a backoff of up to hours waits until the next rebuild completes, and shutdown works only because another task wins a race.
Spot: tokio::time::sleep(backoff).await (or an awaited rebuild) in a loop that polls control_rx only elsewhere.
Applies: rust, rust-test, substreams · look first at lines matching `sleep|backoff|retry|rebuild|select!|control|stop|shutdown|cancel|loop`
Checked by: ast-grep `sleep-ignores-stop`

### config-denies-unknown-fields
Deserialized configuration rejects keys it does not read (#[serde(deny_unknown_fields)] on config structs and enum variants), so a misspelled or unsupported key is an error rather than a silent no-op.
Why: An operator who sets a key that is silently dropped gets no error and no effect, and the shipped example config documents a setting that does nothing.
Spot: A config struct or tagged-enum variant deriving Deserialize without deny_unknown_fields, or a YAML example with a key no field consumes.
Applies: rust, rust-test, substreams, config, docs · look first at lines matching `deserializ|serde|deny_unknown|config|:`
Checked by: ast-grep `config-without-deny-unknown`

### rust-method-prefix-conventions
Method names follow Rust's conventions for their prefix: `iter_*` returns an iterator, `new` is a cheap, synchronous, infallible constructor (one that does I/O or returns Result is named `create`, `load`, `init` or `try_new`), and `as_`/`to_`/`into_` match their cost and ownership.
Why: A conventional name sets wrong expectations at every call site (a snapshot allocated where an iterator was expected, an RPC call hidden in `new`).
Spot: `fn iter_*` returning a collection, or `async fn new` / `fn new(..) -> Result` that makes network or database calls.
Applies: rust, rust-test, substreams · look first at lines matching `fn\s+(iter_|new|as_|to_|into_)`
Checked by: ast-grep `method-prefix-convention`; clippy `clippy::new_ret_no_self (default)`; clippy `clippy::wrong_self_convention (default)`; clippy `clippy::iter_not_returning_iterator`

### one-public-error-enum-per-module
A module exposes one public error enum covering its failures; a single-variant error enum for one helper becomes a variant of it, and helper-local error types stay private.
Why: A crop of narrow public error enums that can never be extended clutters the API and forces callers to handle several types for one operation.
Spot: A new pub enum with one variant (e.g. TooLong(usize)) or several pub error enums added for one small feature.
Applies: rust, rust-test, substreams · look first at lines matching `enum|error`
Checked by: ast-grep `single-variant-pub-error`

### box-large-variants
A variant with a large payload added to a small, widely copied enum is boxed, referenced by a compact id, or justified, because every value of the enum takes the size of its largest variant.
Why: Every value of the enum, payload-free variants included, grows from one byte to hundreds of bytes, and clippy's large-enum warnings spread to every type holding it.
Spot: A new enum variant carrying a struct with arrays or strings on an enum that derives Copy and appears in many structs and errors.
Applies: rust, rust-test, substreams · look first at lines matching `enum|copy|variant|\(|\{`
Checked by: clippy `clippy::large_enum_variant (default)`

### one-integer-type-per-quantity
A quantity has one integer type across the layers that handle it, chosen by its domain (unsigned for counts), with the conversion to a wire or storage type made once at that boundary.
Why: Casts back and forth between i64 and u64 at every call site hide truncation and sign bugs and make the boundary unclear.
Spot: `as i64` / `as u64` on the same field at several call sites, or a count typed as i64.
Applies: rust, rust-test, substreams · look first at lines matching `\bas\s+[iu](8|16|32|64|128|size)|i64|i32|u64|u32|usize|isize|try_from|try_into`
Checked by: clippy `clippy::cast_sign_loss`; clippy `clippy::cast_possible_wrap`

### applying-input-returns-result
A method that applies external input to internal state returns Result, so the caller learns when the state may no longer be reliable.
Why: A unit-returning apply method must either panic or swallow a bad input, and callers keep using state that silently diverged.
Spot: A trait method like `fn apply(&mut self, input: &..)` with no return type whose input comes from outside the process.
Applies: rust, rust-test, substreams · look first at lines matching `fn |&mut self|apply|trait`
Checked by: ast-grep `apply-without-result`

### private-module-items-pub-crate
Every `pub` item is reachable from the crate root; items inside a private or pub(crate) module are marked pub(crate) themselves, so a new `pub` is new public API.
Why: The item's reach is readable at the item, and in a published crate a `pub` in a diff always means public API.
Spot: pub items inside a restricted module.
Applies: rust, rust-test, substreams · look first at lines matching `pub|mod\b`

### same-concept-same-name-across-layers
Types that are the same concept in two layers (a wire DTO and its domain model) share one name in their own modules, and code that needs both disambiguates by module path (`dto::Foo`, `synchronizer::Foo`) rather than by import aliases (`Foo as ModelFoo`) or name affixes (`FooDto`).
Why: Aliases and affixes vary from file to file, so the same type is spelled differently in different places and grep finds only some of them.
Spot: `use ...::{Foo as DtoFoo}` / `as ModelFoo` / `as FooDto` imports, or a `Dto` suffix added to a type that lives in a `dto` module.
Applies: rust, rust-test, substreams · look first at lines matching `\bas\b|dto|model|use `
Checked by: ast-grep `layer-affix-alias`

### no-private-bounds-on-public-methods
A public type's methods are not gated on a crate-private trait bound (`#[allow(private_bounds)]`); methods external callers need are implemented on the type directly, or the trait is made public.
Why: External users can name the type but cannot call the gated methods, and the private trait adds an abstraction that has a single implementor.
Spot: `#[allow(private_bounds)]` or an `impl<T: PrivateTrait> PublicType<T>` block, or a `pub(crate)` trait used only as a bound on one public struct.
Applies: rust, rust-test, substreams · look first at lines matching `private_bounds|pub\(crate\)|trait|impl<`
Checked by: ast-grep `private-bounds-allowed`

### new-default-method-checked-per-implementor
When a trait gains a provided method with a default answer, every existing implementor is checked against it, and each whose correct answer differs overrides it with a test that fails under the default.
Why: A default that is right for most implementors silently gives the wrong answer for the others and regresses behaviour that worked before the method existed.
Spot: A new provided trait method returning a constant (`false`, `None`) that the PR overrides in fewer implementors than exist, with no per-implementor test.
Applies: rust, rust-test, substreams · look first at lines matching `trait|fn |impl`

### invalid-range-inputs-typed
An input with an invalid range (a tuning value, an optional feature's metadata) takes a type that can only hold valid values: `NonZero*`, or a newtype with a checked constructor and a `const` constructor for literals, so the caller gets the error where the value is built and `build()` stays infallible. Where such a type is overkill, `build()` fails fast. An existing setter that can't change its signature before the next breaking release clamps the breaking range with a warning meanwhile.
Why: A fallible build() for one knob changes every consumer's call site; a warning or a silent clamp lets a misconfiguration run unnoticed; a type puts the check where the value is made, and a const literal is checked at compile time.
Spot: A new error enum and Result on build() whose only variant range-checks one setter; a setter that silently clamps or ignores out-of-range values; warn-and-continue on invalid configuration; a Duration or integer setter whose zero value panics or disables the feature; tokio::time::interval fed an unchecked configurable Duration (it panics on zero).
Applies: rust, rust-test, substreams · look first at lines matching `with_|set_|fn |build|duration|nonzero|interval|clamp|min\(|max\(|config|warn`

### builder-collections-into-iterator
A builder method that takes a collection accepts `impl IntoIterator<Item = (K, V)>` with `K: Into<String>, V: Into<String>` (or the relevant targets) and extends the existing entries, so one method serves single entries, literals and any collection.
Why: Taking a concrete map forces callers to build that map type and to choose between replacing and adding.
Spot: A `with_x(mut self, x: BTreeMap<String, String>)`/`HashMap` setter that replaces the field wholesale.
Applies: rust, rust-test, substreams · look first at lines matching `with_|map|vec|iter|fn `
Checked by: ast-grep `builder-takes-concrete-collection`

### handle-only-the-expected-error
Code that stops, skips or falls back on an error handles only the condition it expects ('does not apply', 'end of range reached'), expressed as `Ok(None)` or a dedicated variant, and propagates every other error, or logs it with its context before any fallback; distinct meanings are never collapsed into one `None`, `break`, `continue` or fallback arm.
Why: A real bug in the fast path is erased whenever the fallback succeeds, and replaced by an unrelated fallback error when it fails. A blanket break/continue on any Err silently truncates results for real bugs and malformed state.
Spot: A match that maps several error variants of different meaning (unsupported vs math failure) to one `Ok(None)`/`None` arm that triggers a fallback; `let Ok(x) = f(..) else { break };`, `if let Err(_) = .. { continue }` or `.ok()?` inside a loop over entries. `let Ok(x) = f(..) else { break };`, `if let Err(_) = .. { continue }` or `.ok()?` inside a loop; a match mapping several error variants of different meaning (unsupported vs math failure) to one `Ok(None)`/`None` arm that triggers a fallback.
Applies: rust, rust-test, substreams
Checked by: ast-grep `fallback-collapses-errors`; ast-grep `loop-stops-on-any-error`

### test-code-in-module-tests
Test code lives in the `mod tests` of the module it tests: unit tests of one module's behaviour go there, not in the crate's `tests/` directory (which holds only end-to-end tests of the public API), and test-only imports, wrappers and fixtures used by one module's tests sit inside that `mod tests` rather than at module level behind `#[cfg(test)]`.
Why: Readers of production code have to work out which items exist only for tests. Unit tests in `tests/` are found far from the code they check and can only use public items.
Spot: `#[cfg(test)] use ...;` or `#[cfg(test)] fn ...` outside the tests module; A new file under `crates/*/tests/` that tests one module's builder validation. `#[cfg(test)] use ...;` or `#[cfg(test)] fn ...` outside the tests module and used by it alone; a new file under `crates/*/tests/` that tests one module's internals (builder validation).
Applies: rust, rust-test, substreams · look first at lines matching `cfg\(test\)|mod tests|#\[test|tokio::test|rstest|use |fn `
Checked by: ast-grep `cfg-test-outside-tests-mod`; ast-grep `cfg-test-file-modules`
