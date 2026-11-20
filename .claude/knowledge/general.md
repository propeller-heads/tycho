# General rules

Rules that hold for code, tests, docs and changes in any language. Reviews check every change
against them; an implementation session reads this file on request.

## Rules

Each rule is a record: its id, which review findings cite; the rule; why it matters; how to spot a
violation in a diff (Spot); the kinds of files it applies to; and the tools that check it, in part
or in full (`.claude/skills/tycho-review/SKILL.md` lists the file kinds).

<!-- pieces -->

### names-say-what-it-is
Names say what the item holds or does (take the noun from the doc comment when it describes the item better than the name); test names and test modules state the subject and scenario they exercise, adding the expected outcome only when the scenario alone does not tell the test from its siblings, and one test covers one behaviour, never the process that produced them; names in a published manifest are fixed before release. A name also reads right at its call site and signals the shape of what it holds: a method reads as a phrase, a function says what it computes rather than being a prepositional fragment, a boolean or predicate reads as yes/no (is_, has_, should_, can_) like its siblings and never as an imperative action, a function returning an amount or Option is not named like a predicate, and a parameter is not named after a type it does not hold.
Why: Misleading names (a 'write helper' that only performs a read-only lookup, a `*View` type that wraps store reads, `review_regressions` for a test module) make the logic around them read wrong. A name that reads wrong at the call site makes the reader open the definition to learn what the call returns.
Spot: A struct/function whose doc comment uses a different noun than its name; names covering only one of several methods; vague names that fit every parameter (ModuleInputs); test names ruling out a special case the code doesn't have; test modules named after a review, PR or round. A `MAX_*` or `MIN_*` constant derived from an encoding width (`(1 << N) - 1`, `type(uint48).max`) named like a policy limit or a duration (`MAX_DEADLINE` reads as a timeout); name the width or what it bounds (`MAX_ENCODABLE_DEADLINE`). A test name that narrates the outcome as a sentence, or joins two behaviours with 'And' (split it into two tests). Methods named by a bare participle or preposition phrase (`pricing`, `in_x_units`); a parameter named after a domain type but typed as something else (a display label, a string id). A new `fn verb_object(..) -> bool` next to `is_*` siblings; a predicate-shaped name on a function returning an amount.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: ast-grep `test-module-named-after-process`; ast-grep `outcome-sentence-test-name`; ast-grep `prepositional-fn-name`; ast-grep `bool-fn-verb-name`

### expectations-from-outside-code-under-test
A test's expected values and fixtures come from outside the code under test (a literal, data from the real external system, the upstream's documented example or spec, a recorded response, hand-worked arithmetic), never from the same function, helper, table, builder or constant the implementation uses. Setup inputs (identifiers, addresses, selectors) may reuse the code's public constants; asserted values, and any constant whose correctness the test checks, stay literal.
Why: A wrong but self-consistent implementation (swapped key components, a missed fixed-point scaling) still passes, so the test proves only that the code equals itself.
Spot: `assert_eq!(x, f(..))` (or the language's equivalent) where f or its constants are also used by the implementation; expected bytes or keys built with the package's own concat/encode/format helpers; fixtures keyed with production key or address helpers, or built by the encoder/serializer under test; tests enumerating a lookup table's entries or asserting the struct they just built.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, integration-test, config
Checked by: dylint `expected-value-from-crate-fn`

### fact-lives-once
Each fact lives in one place (the code that defines it or the doc that owns it), and other prose points to it or states the relation instead of restating its values, defaults, enumerations, guarantees or encoded layouts; a producer's doc names the layout's owner (the decoder or the guide) rather than re-illustrating the byte packing.
Why: Restated copies drift on the next retune or added case and become false while the original stays right.
Spot: 'Default: \<value>' or numbers that also appear as literals a few lines away; lists of supported variants, backends or implementors that are also match arms or constants; doc comments or README sections paraphrasing another function's doc or another README; enumerations of config keys defined elsewhere. A docstring with a byte-layout or protocol-data table duplicating one in the decoder's docs or a guide.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs, config
Checked by: repo-script `restated-default-value`; ast-grep `byte-layout-table-in-doc`

### document-load-bearing-oddities
Code that looks like a bug or a removable simplification (a guard, an empty value meaning 'disabled', an override passed on one call only, a deviation from the upstream-recommended approach, a design choice resembling a workaround) carries a comment stating what it prevents or why it is so.
Why: Without the reason, readers can't tell intent from a bug and 'fix' it, e.g. removing a guard against a zero or null value whose absence would bring down the whole pipeline.
Spot: `if x != ..`/`if !zero(..)` filters with no comment; empty-set/None sentinels that change semantics; one call site passing an explicit value the others don't; code diverging from a documented recommendation; documented rules ('do X, not Y') without the reason the alternative fails. One arm of a parallel match carrying the 'why' comment while a sibling arm that departs from the same contract carries none: place the comment so it covers every arm it explains.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### docs-state-what-code-does
Comments, docs, usage text, changelogs, error messages and metric names, labels and descriptions state what the code actually does, including the checks it enforces; every claim placed next to code is checked against that code in every mode it runs in (reruns, cached paths), and a metric claims only what the code determined and covers every path that increments it.
Why: A claim that contradicts the code (an advertised env var that is never read, 'first' that is actually 'last', 'goes stale unnoticed' beside a test that fails CI in exactly that case) sends the next reader or operator down the wrong path.
Spot: Usage blocks or error messages listing options the code doesn't read; 'first'/'only'/'deliberately excluded' next to code doing otherwise; changelogs naming components the system doesn't have; doc sentences describing a consequence ('goes stale unnoticed', 'is ignored') that a test or CI check prevents. Behaviour claims with 'usually', 'guarantees', specific off-by-N numbers justified by one observed run; metric labels like expected/unexpected derived from a proxy check; a counter documented for one case but incremented on several paths.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs

### remove-what-the-change-made-redundant
When a change makes code, dependencies, config, stored or derived state, or tracking redundant or unreachable (including through an invariant of the upstream system), the same PR removes it together with the inputs that only fed it; unused dependencies, dead code kept alive by lint suppressions, and temporary allows whose pending consumer this PR adds go too (what only tests use becomes test-only).
Why: Dead or redundant code keeps being built, maintained and computed, misleads readers about what the package relies on, suggests cases that can't happen, and can hide errors (an ignore rule hiding misplaced files).
Spot: Functions/branches with no remaining callers; an old implementation left beside its replacement; stored state or fields whose only consumer could use an existing value; normalisation between values an upstream invariant guarantees equal; .gitignore/CI entries naming files whose producer the PR deleted. Rust: Cargo.toml entries with no `use` in the crate; `#[allow(dead_code)]` on traits/impls. Exception: cosmetic cleanup in a Substreams package whose spkg is already syncing waits (`defer-cleanup-in-syncing-package`). `#[allow(dead_code)] // consumed by ...` naming the current PR's ticket.
Applies: any
Checked by: ast-grep `allow-dead-code-suppression`; ast-grep `allow-pending-consumer`; clippy `clippy::allow_attributes`; clippy `clippy::collection_is_never_read`; clippy `clippy::no_effect_underscore_binding`; ci `cargo-machete (already in CI)`; ast-grep `allow-dead-code-on-trait-impl`

### single-definition-of-logic
Logic that defines one concept (a predicate, a classification, an enumeration of active entities, a directive syntax, or behaviour several implementations must share) lives in one shared function, not a framework every implementor must satisfy, that every path calls, written in its simplest form. The same holds for a data shape produced in one place and parsed in another (one shared type serialized on both sides) and for a name or key format used in several places (one source); where copies cannot share one definition (different packages or languages, a hand-written list mirroring a schema), a test or a startup check asserts the copies agree and refuses to proceed on a mismatch.
Why: Copies drift (different prefix checks, different key sets, fixes landing in one of five loops) and the behaviour depends on which path ran. Restated shapes and key formats mismatch only at runtime, often without an error.
Spot: The same predicate or key set in a filter and its caller; two resolvers for one directive syntax; the same probe/iteration loop in several helpers or implementations; near-identical functions differing only in a constant; convoluted versions (extra maps, carried indices) of a simple rule. A preview/estimate function and the execution function it predicts each computing the same direction flag, limit or rounding mode inline. Two paths that build the same state (a snapshot path and an incremental path) each parsing the same fields with their own loop; downstream stages re-deriving a classification an upstream stage computed and discarded. Hand-written JSON/struct mirrors on producer and consumer sides; string literals for the same name in several files. Key-value stores: key formats not defined once (constants for fixed keys, one function per prefixed key) and shared by writer and reader, e.g. `format!("user:{}", ..)` in one module and `format!("user:0x{}", ..)` in another, or a checksummed Display rendering of an id next to a lowercase hex encoding; tests spell keys out as literals. 'Must match X in crate Y' or a literal repeated across packages or languages with no comparing test; a cleanup or probe list mirroring tables, columns or variants with no check against the schema's actual definition.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config, sql, integration-test
Checked by: repo-script `store-key-formats`; ast-grep `must-match-copy`; ast-grep `sol-must-match-copy`

### scope-claims-exactly
Comments and docs state the exact scope of what they describe: which part matches, from which point or version on, for which inputs and environments, and what happens for every combination a multi-condition branch handles; no unqualified 'the same as', 'from the start', 'every' or 'always', no description that silently holds for one environment only, and a borrowed term is defined generally, with its source, before the case the code uses.
Why: An overbroad claim is false in the part it doesn't qualify and misleads anyone relying on it there (compiler settings that differ, a value set before the processed range began read as zero).
Spot: Comments or README sentences with 'the same as', 'exactly like', 'built the way it is deployed', 'from the start', 'all', 'every', 'always' about artifacts, configs, signatures or coverage, next to code with a start point, filters or differing settings. A module or README whose code hardcodes one environment while its prose names none; 'when A and B ..., otherwise ...' above code that branches on A alone; a term from an external spec introduced through one example with no source. A security or trust statement ('no input can select a call target') that holds for the component it documents but not for the components it forwards caller-supplied values to.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs
Checked by: ast-grep `sol-broad-security-claim`

### docs-at-the-right-scope
Shared docs (repo- or crate-level CLAUDE.md, architecture docs) hold guidance for the whole component and a package README covers only that package; details of one module or integration go to that module's own code or README, notes for downstream consumers to the consumer-facing docs, and how the PR was verified to the PR description; remaining work goes to filed issues, not to a committed handover or status file.
Why: Module-specific details in shared docs bloat them and go stale without an owner, and content about other layers in a package README duplicates those layers' own docs.
Spot: Additions to a shared CLAUDE.md or architecture doc naming one module's call path, byte layout, type or address; a package README with build/test commands for other packages or descriptions of how other layers behave. A committed handover or notes file listing remaining work, or citing files not in the repo.
Applies: docs

### remove-redundant-tests
Remove a test whose assertions another test at the same layer already makes on the same input, or that asserts nothing about its subject, and keep each module's unit tests on its own logic by building the module's input directly instead of running it through an upstream stage.
Why: Duplicated and subsumed tests cost maintenance without coverage, and tests that go through the upstream filter test the wrong module; a higher-layer test whose cases share one branch today is what catches a later change that treats them differently.
Spot: Two tests building the same input and checking the same error text; a single-case test whose branch a multi-case test covers; test helpers that pass fixtures through an upstream filter; noise writes the first check discards.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, integration-test

### stopgap-workarounds
Before accepting a workaround, check whether the constraint it works around is self-inflicted and fix that instead; a workaround that stays is marked as a stopgap in its doc comment with the condition for removal, and the root fix is filed.
Why: Unmarked workarounds become permanent and get built upon, and workarounds for self-imposed constraints multiply.
Spot: New helpers or magic offsets (+3, stubs, raw queries) justified by another component's defect or an arbitrary pinned value; 'X is needed because Y' where Y is in this repo; a NOTE replacing a TODO without a ticket.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config
Checked by: pr-script `todo-became-note`

### docs-describe-own-item
A docstring or comment describes the item it is attached to and sits next to the code it talks about: what that function does and guarantees, not its callers, guarantees its caller enforces, or another function's internals; trait docs stay implementation-agnostic.
Why: Caller lists and caller-side guarantees go stale or mislead anyone reusing the function from a caller that lacks the enforcement, and implementation details on a trait mislead every other implementor.
Spot: Docstrings with 'called from', 'used in', 'on both X and Y', 'existing'; doc comments explaining why something errors when the function itself just returns a value; comments about a helper's internals at its call site; a trait or interface method doc naming one implementor.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `//|/\*|\*/|^\s*\*|#|"""|'''`
Checked by: pr-script `doc-describes-callers`

### every-assertion-can-fail
Every assertion can fail for a plausible bug in the production code: it checks concrete values the production code computed (not a closure or struct the test defined) and, when the inputs pin the outcome (a fixed seed, a recorded response, a pinned block), asserts the exact value with equality rather than `> 0`, 'increased' or a bound the code under test already enforces on the same call. It is not a round trip through generated code, a property every fixture has, identical expected values or identical inputs for cases or parameters it claims to distinguish, a 'field changed' check in place of the effect, or an outcome a fallback in the code would also produce within the test's wait. Together the assertions cover the whole outcome: every value the function returns (none discarded with a blank destructuring slot), what must stay unchanged as well as what changed, a metric by its full name and label set, and for a gate or throttle both that nothing passes before the release and that the item passes after it.
Why: Vacuous assertions give false confidence: a state transition that dropped its writes, or a key rotation that kept the old key, still passes. A renamed label, a value both deleted and restored, or a max that ignores some inputs passes a partial assertion.
Spot: Only `is_ok`/non-empty/non-zero checks; test bodies defining the logic they assert on; `decode(encode(x)) == x` on generated (e.g. protobuf) types; assertions on a property every fixture item has; byte-identical expected values in a test claiming to distinguish two cases; `assert_ne!(old, new)` on a field rather than the behaviour it drives. `assertGt`/`assert!(x > 0)` on deterministic input; a threshold passed in that equals the asserted bound; the same variable or literal passed for two same-typed parameters (`f(amount, amount)`); a test wait longer than a timeout/abort window in the code under test that produces the same observed effect. A revert expectation with no error selector or reason (Foundry's argument-less `vm.expectRevert()`), which passes for any revert, a broken setup included. Metric tests folding a snapshot by one label with unwrap_or_default, a deletion test without the sibling tests' `updated.is_empty()`, or fixtures that give every input the same value.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, integration-test · look first at lines matching `assert|expect|check|require|should|panic|==|!=|>|<|is_ok|is_err|is_some|is_none|is_empty|wait|sleep|timeout|unwrap|eq`
Checked by: ast-grep `weak-assert`; ast-grep `inequality-only-assert`; ast-grep `sol-inequality-only-assert`; ast-grep `same-arg-twice`; ast-grep `sol-same-arg-twice`; ast-grep `sol-bare-expect-revert`; ast-grep `test-discards-returned-value`

### tests-reach-new-branches
Every new branch, including each rejection and error branch, has a test whose fixture actually reaches it: the fixture contains the key or condition the branch keys on and has the shape the real system produces (full-width packed values, unsorted ordering, several entities in one input, the producer's actual encoding), and every setup step the scenario depends on is asserted to have happened rather than skipped silently by an `if let`.
Why: A test that passes without entering the branch, or only because the code is lenient with a shortcut fixture, leaves the new path and the real decode untested.
Spot: New `if let Some(..) = map.get(..)`, `?` or `return None` branches whose fixtures lack the key or condition; only one half of a fatal/retryable split tested; truncated values where the real input is a wider packed word; fixtures already in the order the code is supposed to establish; rejection cases reached only indirectly; fixtures encoded differently from the real producer (fixed-width padded where the producer writes minimal-length bytes). `if let Some(..) = setup_step() { .. }` in a test where the else case means the scenario was never set up. All fixtures share one ordering of their inputs where the code derives behaviour from that ordering (which of two addresses sorts first); length guards on supplied bytes tested only with an empty payload rather than with a truncated and an over-long one that assert the offending length.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: ast-grep `test-if-let-skips-step`; clippy `clippy::should_panic_without_expect`

### cite-external-source
A comment about external behaviour (an upstream system's types, a library's guarantees, a provider's error codes) is checked against that source and cites where the guarantee comes from: the accessor, the function, or the provider's docs URL; where the external component has no published source or interface, a hand-written interface for it names how it was obtained, and each meaning the code relies on is confirmed with the provider or marked as inferred from observed behaviour.
Why: A wrong premise misleads the next reader even when the conclusion happens to hold, and an uncited error-code mapping can't be verified.
Spot: Comments asserting an external system's types, library behaviour ('tungstenite answers pings while polled') or invariants without pointing at what enforces them; error-code match tables or API-specific behaviour with no docs link. A hand-written interface or struct for an external component with no published source and no comment on its origin; logic that depends on undocumented flags or packed fields.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs

### links-resolve
Links in comments, docs and package metadata resolve publicly to the current version and location of the referenced page or file; dead, gated or outdated links, copied boilerplate URLs, and permalinks into a repository the code has moved out of are fixed or removed.
Why: A 401/404 or superseded link leaves the reader without the reference the text relies on, and a dead boilerplate URL repeated in every manifest hides the real location.
Spot: New URLs into gated doc hosts or deep app links; `url:` fields copied from other manifests that point into another package's or a moved repository's path; links to superseded API versions. URLs of the form github.com/\<org>/\<predecessor-repo>/blob/... in comments, especially comments claiming 'mirrors X'.
Applies: any · look first at lines matching `https?://|www\.|\]\(|href|url|repository|homepage|documentation|github\.com|\.md\b`
Checked by: repo-script `manifest-url-mismatch`; ast-grep `predecessor-repo-link`; ast-grep `sol-predecessor-repo-link`

### no-speculative-code-paths
A branch, retry or feature (older API versions, optional auth) is added only with evidence that it is exercised, while a configuration knob a deployment cannot otherwise reach is legitimate without an in-repo caller. Code lands with its first caller: public types, aliases, test helpers and counters nothing in the PR uses move to the PR that consumes them.
Why: Unneeded branches add untested surface, while a missing knob forces forks or releases once a deployment grows. Unused API in a layer PR cannot be reviewed against real usage.
Spot: Conditional code for API versions, auth or headers with no evidence of need; conversely, a config setter removed only because nothing in the repo calls it. New pub aliases, functions or test helpers with zero callers in the diff; `#[expect(dead_code)]` on new items.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: pr-script `new-item-without-caller`; ast-grep `allow-dead-code-on-trait-impl`

### use-or-drop-deserialized-fields
Fields deserialized from an external response are either used (asserted against the request, or enforced when they bound the response's validity: an expiry, the environment it applies to, a minimum size) or dropped from the model; a validity bound is never dropped as unused.
Why: Unused fields are dead model surface that suggests validation which never happens.
Spot: New model structs with fields never read outside Deserialize; identifiers or amounts echoed back in a response and never compared to the request. A removed or never-modelled expiry, minimum-size or environment field in a response model whose content the code then acts on.
Applies: rust, rust-test, substreams, script, python, solidity · look first at lines matching `deserializ|serde|derive|struct|json|response|model|field|expir|valid|class|interface|type\s|\w+\s*:\s*\S`
Checked by: dylint `deserialized-field-never-read`

### narrow-escape-hatches
An escape-hatch or override parameter exposes only the degree of freedom its use case needs.
Why: A wider override lets callers reintroduce the very mismatch the default was built to prevent.
Spot: Override variants of helpers taking a free-form argument (environment, address, path) when only a narrower one (a version or point in time) is ever needed.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### validate-both-range-bounds
Validation of input against a bounded range checks both bounds.
Why: Checking only one side lets data outside the requested window through, so results can claim to cover a range they don't.
Spot: A guard on the upper bound (`last > to`) with no corresponding lower-bound check, or vice versa.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `<|>|\.\.|range|bound|min|max|clamp|contains|from|start|end|lower|upper|limit`

### missing-input-fails-loudly
A required input that is missing or empty, a value the code itself wrote that fails to decode, and a configuration value that is set but fails to parse each fail loudly instead of falling back to zero, empty or a degraded default; only an unset optional value takes its default. Library code returns an error naming the key (`errors-not-swallowed-with-context`); a panic is right only in build and config paths at startup, in Substreams handlers for violated invariants of the package's own data when no value keeps just the affected entity inert (closed, unquotable or skipped), since a module error halts the extractor for every component (`modules-survive-external-values`), and in tests. A failed eth_call or other provider failure is transient, not a missing input: the handler panics so the block is retried, and never writes an inert value in its place, least of all into a static attribute written once at creation. A real value someone configured, used when an event is out of range (`one-time-event-config-fallback`), is not a degraded default. Input that violates an invariant the computation relies on (non-monotonic steps, unsorted bounds) is an error too, not patched with a fallback value such as the nearest valid neighbour.
Why: A silent fallback turns a misconfiguration or bug into output that looks valid: a zero indistinguishable from a genuinely empty value, an empty set that filters out every item.
Spot: `unwrap_or_default()`, `unwrap_or_else(BigInt::zero)`, `.ok().and_then(..)` on store reads, decodes or required inputs; Option-returning accessors that treat empty as 'not configured'; helpers returning an empty set where 'unknown' is meant. `env::var(X).ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT)` merging 'absent' and 'invalid'. A branch that detects an invariant violation, logs a warning and returns a substitute value (`return lower.clone()`).
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config · look first at lines matching `unwrap|expect|default|\.ok\b|ok\(|or_else|or\(|zero|empty|none|null|nil|env|var|parse|decode|get\(|\?\?|\|\||:-|panic|fallback|missing|optional|option|some|try|catch|except|err|return`
Checked by: ast-grep `store-read-default`; ast-grep `parse-failure-as-default`; ast-grep `warn-and-substitute`

### guard-no-silent-absorb-config-errors
A guard, fallback or name mapping does not silently absorb a class of config error: if it cannot tell the expected case from a misconfiguration, it becomes an explicit allowlist (each value with its reason, every supported member listed) and fails loudly for everything else, rather than a fallthrough that returns a real member or a stub.
Why: A generic 'lookup failed -> substitute a stub' fallback turns typos and identifiers from the wrong environment into baked-in values that only fail in production.
Spot: Code that auto-patches, defaults or stubs when a lookup fails (missing resource, key or file) without checking a known list; log lines like 'planting a stub' or 'using default'. A shell `case` with `*) echo "<real value>"`, a `match` with `_ => Default`, or a lookup with a real-member default over a growing set.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config, ci · look first at lines matching `match|case|=>|default|fallback|stub|else|unwrap_or|or_else|get\(|lookup|map|\*\)|missing|not found|exist|allow|\|\||\?\?`
Checked by: repo-script `fallback-log-lines`; ast-grep `wildcard-to-real-member`; clippy `clippy::wildcard_enum_match_arm`

### check-against-same-quantity
Verification scripts compare a decoded value against the source-of-truth accessor that exposes the same stored quantity, not a derived one that only coincides in some states.
Why: A check against the wrong reference fails on ordinary states or passes for the wrong reason.
Spot: `check` calls comparing a stored field to an accessor that computes a function of it (a current or effective value against the stored maximum).
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, integration-test · look first at lines matching `check|assert|verify|compare|expect|==|eq|diff|match`

### comments-explain-why
Comments explain why, not what; a comment narrating the next line means the code should be clearer, and commented-out code is deleted.
Why: What-comments drift from the code they paraphrase, and commented-out code is dead weight with no owner.
Spot: Comments restating the following statement; '// let x = ...' or other commented-out blocks in the diff.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `//|/\*|\*/|^\s*\*|#|"""|'''`
Checked by: ast-grep `commented-out-code`

### tests-check-behaviour-and-edges
Tests check behaviour, not implementation, and cover edges and errors (empty input, boundaries, malformed data); mocks are used only at slow, non-deterministic or external boundaries.
Why: Implementation-coupled tests break on refactors without catching bugs, and happy-path-only tests miss where bugs live.
Spot: Tests asserting private internals or call counts on in-process collaborators; mocks of pure logic; new parsing/decoding code with no malformed-input or boundary case.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, integration-test

### fixed-paths-not-env-vars
Inputs come from fixed, documented locations (a conventional file path, plus a feature flag when optional) rather than environment-variable indirection, and tools run from the repo (e.g. via cargo run) rather than from something installed on PATH.
Why: Env-var plumbing and PATH tools hide where inputs come from and make builds depend on the developer's shell.
Spot: std::env::var / env! in build.rs or library code for paths; new required env vars; scripts invoking a binary expected on PATH.
Applies: rust, rust-test, substreams, script, python, ci, cargo
Checked by: ast-grep `env-macro-not-manifest-dir`

### counters-registered-at-zero
Prometheus counters and their label sets are registered at zero at startup.
Why: A counter created on first increment is invisible to increase() for that first event, so the first occurrence never alerts.
Spot: New metrics incremented on an error path with no registration at startup.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `counter|metric|prometheus|gauge|histogram|inc\(|increment|register|label`
Checked by: repo-script `counters-registered-at-zero`

### inputs-exactly-what-callee-acts-on
Give a function exactly the inputs it acts on: compute set differences / filtering at the caller and iterate only the needed items, instead of passing broad collections the callee must subtract from.
Why: Callees taking both the full set and the exclusions mix responsibilities and do wasted work on items they then skip.
Spot: Function signatures taking `all: &[T]` plus `covered: &HashSet<T>` (or similar) and computing `all - covered` inside; loops over a global registry when only a subset is needed.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### harness-exercises-its-subjects
A harness or test that exercises nothing (no case ran, or the subjects it is meant to check are missing from what it checked) is not accepted as coverage, unless the change shows that no harness can exercise them; a harness should exercise everything it is meant to check. The harness need not fail its own run.
Why: Vacuous passes hide pools silently skipped by the decoder or tokens never loaded, so a broken integration ships green.
Spot: Range tests with `skip_simulation: false` where nothing shows how many simulations ran; decoder results whose failed/skipped components are only logged; a new test whose cases all skip.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, integration-test, ci, config

### data-clumps-become-a-type
Values that keep travelling together (the same few parameters across several signatures or from one source to a constructor, the same group of fields in several structs) become one type that is passed instead of its members, so constructors take only independent inputs.
Why: Every signature that repeats the group can swap or drop a member, and every invariant of the group (its members are present together, one is derived from another) has to be re-checked at each site instead of once where the type is built.
Spot: The same two or more parameters, in the same or swapped order, in several functions of the change; structs repeating the same field group; a function that takes a group's members one by one only to rebuild the group; a group where removing one member leaves the others meaningless. Constructors with five or more parameters where a subset always comes from the same source together.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: clippy `clippy::too_many_arguments (default)`

### cache-keyed-by-all-dependencies
A cache of a derived value is keyed or invalidated by everything the value depends on (including the current time or epoch and any overrides passed in), not only by the events that change the underlying stored state.
Why: Values that change without a state event (time-dependent schedules or ramps) are served stale indefinitely, and a call with different inputs gets the first caller's answer.
Spot: A read-through cache keyed on a subset of the function's inputs, cleared only on update/delta events, wrapping a computation that also reads the clock, an epoch counter or caller-supplied overrides.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `cache|memo|lru|ttl|invalidat|clear|key|stor|lazy|once|map|hash|static|state|saved|prev|last`

### default-on-with-opt-out
Behaviour that should apply to every consumer ships on by default with an explicit opt-out, and only once what it depends on has shipped; an opt-in method that no caller invokes leaves the feature inert.
Why: Consumers keep the worse behaviour without knowing a better one exists, and the feature's behaviour stays unexercised end to end.
Spot: A new `via_*`/`enable_*`/`--run-*` switch defaulting to off with no in-repo caller turning it on, or a boolean CLI flag with `default_value_t = false` for something the PR describes as the desired behaviour.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config, ci · look first at lines matching `default|enable|disable|via_|--|flag|opt|bool|false|true|feature|switch|run_|with_`
Checked by: ast-grep `default-off-flag`

### derive-dependent-args-in-callee
A value every caller derives the same way from another argument (or from state the callee can reach) is derived inside the callee rather than passed alongside its source.
Why: Repeated derivations at call sites drift, and passing both the source and the derived value lets a caller supply an inconsistent pair with nothing to catch it.
Spot: Every call site passing `f(x)` (or the same `format!`-built string or constant) to one parameter, alongside `x` or something the callee already holds.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### apply-only-newer-sequenced-updates
A consumer of sequenced updates (version, sequence number, block) applies one only if it is newer than the last applied and drops an older or replayed one without touching its state; an idempotent skip at an already-applied version checks that the incoming value equals the stored one and warns if not; and when several streams carry the same events, a lagging stream's backlog cannot move shared state backwards (decisions read only caught-up streams, and an event already applied from another stream is recognised as delayed).
Why: Applying a late or replayed older frame rolls the consumer's state back and announces removals and additions that never happened.
Spot: A `process(frame)` that replaces state from a frame carrying a sequence number with no comparison against the last processed one. `if at <= self.written_at { return; }` with no value comparison; round or tip selection over all queued messages including streams marked delayed.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, sql · look first at lines matching `version|seq|block|height|nonce|epoch|revision|round|tip|frame|ordinal|number|_at|timestamp|delay|lag|stream|update|apply|written|last|process`
Checked by: ast-grep `idempotent-skip-without-compare`

### solver-checks-postcondition
A solver or search reports success only after checking the requested postcondition on the result it returns (the result lies inside the caller's target band); reaching an iteration cap, an input cap or a sentinel value is never treated as convergence.
Why: The caller receives an 'Ok' whose state misses the requested target by orders of magnitude, and the miss is indistinguishable from a real solution.
Spot: Success inferred from a counter differing from its maximum, from an input cap being hit, from the last iterate of a bounded loop, or before a later state change that moves the result; no final in-band check on the returned result before returning Ok.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `solve|search|iter|loop|while|for\b|converge|bisect|binary|newton|max|cap|limit|toleran|band|target|step|attempt|ok\(`

### every-retry-step-reaches-classifier
In a retry or restart loop, every fallible step (building or rebuilding the worker, the request or the connection, reading, decoding) routes its error through the same retry-or-give-up decision with the same backoff and limit; no step returns early with `?` past the failure counter, and only configuration errors fail fast.
Why: A transient failure in the one step that skips the classification ends the loop for good, while the same error class elsewhere is retried.
Spot: `?` on a fallible call inside a retry/reconnect loop (or a `try_stream!` body) that sits before or outside the failure-tracking call; two sibling loops classifying the same error type differently. `return Err(err)` from the build step inside a supervisor's restart loop whose caller only logs the result.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, ci · look first at lines matching `retry|retries|backoff|attempt|reconnect|restart|supervis|loop|try_stream|fail|give_?up|max_|limit|\?|err`
Checked by: ast-grep `early-return-in-retry-loop`

### map-key-carries-full-identity
When entities are collected into a keyed map, the key carries every dimension that distinguishes them, and an insert that finds the key already present is treated as a bug (warn or error), not silently overwritten.
Why: A key missing one distinguishing dimension makes last-insert-wins pick an arbitrary entity, which can change from run to run with unordered iteration, and the others disappear.
Spot: `map.insert(derived_key, value);` inside a loop over entities with the return value ignored, where the key is derived from a subset of the entity's identity (e.g. a group id for data that is per member of the group).
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `insert|map|hash|dict|\]\s*=|set\(|entry|key|collect|mapping`
Checked by: ast-grep `insert-overwrite-ignored`

### snapshot-shared-source-once
A computation whose reads must agree takes one snapshot of each concurrently updated source at the start and uses it for every step: a latest-value source is read once, and database reads that must agree (a parent and child table, a watermark and the rows it covers) run in one repeatable-read snapshot transaction with a statement timeout.
Why: Steps of one computation see different versions of the data (data from one update, metadata from the next), producing inconsistent or spuriously failing results.
Spot: Several `.borrow()`/load calls on the same watch channel, ArcSwap or shared map inside one function or across helpers called from it. A refresh issuing several SELECTs on related tables outside a transaction, or a background read with no statement timeout.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, sql · look first at lines matching `borrow|load|watch|arcswap|arc_swap|read|lock|get|select|query|transaction|snapshot|repeatable|timeout|fetch|latest`
Checked by: ast-grep `repeated-latest-value-read`

### no-local-pin-of-global-setting
Tests do not pin locally a setting the project configures globally (a compiler or VM version, a feature flag).
Why: The local pin silently exempts that test from the next global upgrade, which is exactly when the test should catch breakage.
Spot: A test setup that re-sets a value also present in the project's global config (e.g. `vm.setEvmVersion("osaka")` in a fork modifier while foundry.toml sets evm_version).
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, integration-test, config · look first at lines matching `version|evm|feature|set|pin|config|toolchain|edition|cfg|flag|env`
Checked by: ast-grep `sol-test-pins-evm-version`

### externally-owned-facts-not-hardcoded
Facts owned by an external system that can change without a release of this code (endpoint URLs, lists of supported items, per-item properties such as precision) come from runtime inputs or fetched data with a sensible default, not from hardcoded constants or match tables.
Why: When the external system changes, users are stuck until a new release ships.
Spot: A hardcoded base URL or IP, or a `match id { ... => Some(6) }` style table of per-item properties in client code.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config · look first at lines matching `https?://|url|host|endpoint|\d|match|=>|const|static|list|supported|decimals|precision|ip|case`
Checked by: ast-grep `hardcoded-endpoint-url`

### freshness-of-what-affects-result
The freshness used to report or gate a result is that of the oldest input that can affect that result: not the newest input, and not inputs that cannot affect it.
Why: Taking the newest makes an aggregate look fresh while it serves results from stale parts, and taking an unrelated stale input blocks a result whose own inputs are fresh.
Spot: `.max()` over constituent timestamps to produce an aggregate's age; `min()` over the timestamps of inputs of which only one feeds each result (e.g. one combined timestamp for independently updated halves).
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `max|min|time|stamp|age|fresh|stale|last|updated|block|latest|oldest|newest|_at|lag|ts\b`
Checked by: ast-grep `max-timestamp-as-age`

### startup-read-external-data-refreshes
Data read once at startup from a mutable external source (a remote allowlist, a remote config) either refreshes or documents that a restart is needed to pick up changes, and tests cover both membership directions (added and removed).
Why: A removal after startup keeps routing to an entry that now fails on every call until someone restarts the process.
Spot: A one-shot fetch in a builder/`build()`/first-poll whose result is stored for the life of the process with no TTL, re-read, or doc note.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config · look first at lines matching `fetch|http|get|load|read|build|init|start|remote|allowlist|config|once|lazy|refresh|ttl|reload|url|client|request`

### honour-documented-constraint-inputs
Every implementation of an interface honours every input the interface documents as a constraint, validating the inputs' own consistency (e.g. min \<= max) and rejecting results outside them; destructuring a documented constraint as `_` is a contract violation, not an implementation choice.
Why: Callers that rely on the documented bounds get results outside them (a result of 5e17 when the documented maximum input is 1) with no error.
Spot: A match or destructure that binds documented request fields to `_` (`min_in: _, max_in: _`) in an implementation of a public trait method.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: ast-grep `constraint-field-discarded`

### capability-check-before-call
Code branches on an explicit capability check when the capability is knowable up front, not on catching a recoverable error from an attempted call.
Why: Using an error as the capability signal conflates 'unsupported' with genuine failures of the first call and hides the contract implementors must satisfy.
Spot: A `match` on `Err(RecoverableError(_))` from one method whose arm then calls an optional capability method of the same object.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `err|catch|except|recoverable|match|support|capab|has_|can_|try|unimplemented|not.?supported`
Checked by: ast-grep `capability-by-recoverable-error`

### one-claim-per-sentence
A doc sentence makes one claim; a chain of clauses joined by colons, semicolons or nested dash asides is split into sentences ordered the way the reader needs them.
Why: A reader has to reread a clause chain to find which part is the claim, and a nested aside hides it entirely.
Spot: A doc or comment sentence over about 40 words, with two or more colons or semicolons, or with a dash aside nested inside another aside.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs

### literal-wording-in-docs
Docs describe behaviour with the literal operation ('returns an error', 'fails to parse', 'uses'), not with metaphor, personification or inflated verbs the reader must translate back.
Why: Figurative wording makes every reader decode the claim, and readers can decode it differently, so the doc stops pinning down behaviour.
Spot: Doc verbs that are not operations the code performs ('stands', 'settles', 'reads as', 'tells itself apart'), or marketing verbs such as leverage, utilize, empower.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs
Checked by: ast-grep `figurative-doc-verb`; ast-grep `sol-figurative-doc-verb`

### docs-name-inputs-outputs-failures
A doc names the concrete things its item reads, returns and fails with (the key it reads, each error and its condition, the case that yields nothing) instead of describing them through a relation the reader must resolve; an encoded value's width, byte order and unit are stated where its layout is defined, and other docs point there (`fact-lives-once`).
Why: A caller cannot tell from the doc what to pass, what to handle or how to produce a compatible value, and has to reverse-engineer it from the code.
Spot: Docs on fallible or optional-returning functions that never mention the error or the empty case; an input described as 'the X it is published under' or 'what it was given' without naming the field or key; a value described as 'raw bytes', '(as Bytes)', 'a timestamp' or 'encoded' with no width, byte order or unit.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs
Checked by: clippy `clippy::missing_errors_doc (review only)`

### lead-with-what-it-is
A doc comment opens with what the item is or does (for a type, what a value represents; for a function, what it returns; for a block, what the block achieves), and a rationale, a contrast with a neighbouring concept or an exception follows that statement and never replaces it.
Why: Readers of generated docs and hover text see the first sentence only, and a comment that opens with, or consists of, a justification or an 'X, not Y' contrast leaves them to reconstruct the purpose from the code.
Spot: A first doc sentence starting with 'so', 'because', 'since' or a design justification, or describing another item; several paragraphs of rationale after a summary line; a rewritten block comment that drops a purpose clause ('to ensure ...') for a contrast ('these are A, not B'), or that only explains why some items are skipped. A first sentence that says how a function works instead of what it returns or does for the caller, leaving out the inputs that matter (a signing helper whose doc opens with 'Builds the EIP-712 digest by hand' and never names the key it signs with).
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `//|/\*|\*/|^\s*\*|#|"""|'''`
Checked by: clippy `clippy::too_long_first_doc_paragraph (review only)`

### every-clause-adds-a-fact
Every clause in a doc adds a fact: a clause restating the previous one, the item's name or a neighbouring paragraph is cut, and two paragraphs giving the same reason are merged into one that keeps every fact either carried.
Why: Repetition makes docs long enough that readers skim, and the copies drift apart on the next edit.
Spot: A clause whose deletion loses no information (a tautology such as 'picking an X is picking among them'), two adjacent comments explaining the same reason, or a field doc of more than about six lines repeating a rationale stated nearby.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs

### active-voice-named-actor
Docs use the active voice with a named actor ('`build()` rejects only zero', 'the task polls your socket'), not agentless passives ('only zero is rejected') that hide which component acts.
Why: An agentless passive leaves the reader unable to tell which component does the thing, which is usually the point of the sentence.
Spot: Sentences of the form 'is/are/was + past participle' with no 'by ...', especially about validating, rejecting, dropping, recording or polling.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs · look first at lines matching `\b(is|are|was|were|be|been|being|get|gets|got|gotten)\b`

### prose-uses-code-names
Prose (docs, comments, log and CLI messages) refers to a concept or item by the name the code gives it and uses that one name everywhere; an informal label with no counterpart in the code, or a positional reference ('as above', 'see below'), is replaced by the searchable name, and where two framings of one thing coexist the doc picks one and states the equivalence.
Why: A reader cannot search for a label the code never uses, and two names for one concept read as two concepts, so readers look for a disagreement that is not there or miss one that is.
Spot: A noun phrase in a doc or message that matches no identifier while an identifier for the same concept exists; a doc naming a key or parameters with words that differ from the names at the sites that write or read it; examples importing a different name than the alias the other docs use. 'as above', 'see below', 'the previous function' in doc comments.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs
Checked by: ast-grep `positional-doc-reference`; ast-grep `sol-positional-doc-reference`

### one-name-one-concept
A name or term denotes one concept within its reach: two items visible in one namespace never share a name (private ones included), a local binding or parameter does not reuse a name the surrounding interface or scope gives a different type or item (an imported function included) and the same input carries the same parameter name across sibling modules, a new name does not reuse a term the codebase already uses for a different role (including names that co-occur in one trace, such as two components' error names), and a test double uses the vocabulary of the slot it stands in for.
Why: Readers and search results conflate the two, a later import or re-export collides, and a reader misreads which value goes where.
Spot: A new private type or function named like a public item in the same or parent module; a doc example or test whose local has the name of a documented method's parameter while holding a different type. A new enum, error or mock name whose word already denotes a different role elsewhere in the crate (grep the term). A parameter named like a function the module imports, followed by a fully qualified call to that function. Test names, helper names, comments and diagrams count too: in execution code `wrap`/`unwrap` mean native-token wrapping, so a test or helper that calls another wrapper 'wrapped' collides with it.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: ast-grep `param-shadows-helper`; clippy `clippy::same_name_method`; clippy `clippy::renamed_function_params`

### follow-sibling-form
An item added beside siblings that play the same role follows their form (the same name for the same job, naming pattern, value representation, markup, import style and declaration style) and departs only where its contract differs; tests in one file follow one naming style.
Why: A lone deviation reads as intentional, so readers look for a difference that is not there, counterparts cannot be found by name, and the next contributor copies whichever form they saw first.
Spot: A new flag, list entry, field, import or build-manifest entry whose form differs from every neighbour in the file; helpers with equivalent signatures in sibling modules named differently; a field holding the same kind of identifier as its siblings in another representation; an inline fully qualified path to a module the file already imports; an explicit build-target declaration where sibling targets rely on auto-discovery. Sentence-style outcome test names (`a_response_nothing_reads_in_is_an_error`) next to `test_<subject>_<scenario>` names in the same module. A new venue or integration validated through its own example or script while its siblings go through a shared harness. A test name much longer than, or shaped differently from, its siblings in the same file or contract (`testPayerRevertFillsSwapAndEmitsPaymentFailed` beside `testPausedPayer` and `testReplay`).
Applies: any
Checked by: ast-grep `mixed-test-name-style`; clippy `clippy::absolute_paths (review only)`

### sweep-old-names-after-change
A change that renames, moves, removes or re-scopes an item, or copies a sibling's code, searches the whole repository case-insensitively for the old name, location and scope (identifiers, identifier strings, log and error messages, doc comments and links, guides, config keys, test expectation files) and updates every hit in the same change, including lines outside the diff.
Why: Untouched lines go stale silently: logs and errors name a component under a name it no longer has, docs send readers after deleted or moved code, and string-keyed config or expectation files fail only at runtime after merge.
Spot: A diff that renames a type, module, function, flag or component, removes a parameter or field, moves logic between functions, widens or narrows a supported set (a match arm, an allowlist, an endpoint), or adds a module copied from a sibling, while a search for the old identifier, the old set or the sibling's name still hits comments, messages, docs or data files. A change that moves or clears a setting while a `coalesce(..., default)` or `unwrap_or(default)` elsewhere still reads the old location.
Applies: any

### doc-depth-matches-reader
A doc goes only as deep as its reader needs to act, so a contributor-guide map entry gives what lives where and the invariants not to break, a user-facing doc gives which option to use and when, and mechanism and design argument stay in the code docs they point to.
Why: Readers wade through arguments to find a location or learn internals they never touch, the internals become something users depend on, and the duplicated rationale drifts from the module docs.
Spot: A code-map bullet in a contributor or agent guide that runs over a few lines or argues ('so', 'because', 'that is the point'); a user-facing doc sentence naming internal storage fields, encoded values or implementation steps behind an option the reader only selects.
Applies: docs

### type-logic-lives-on-the-type
Logic about one type's fields lives with that type (a condition callers would spell field by field is a method, a conversion to another type is a conversion function on it), and call sites use it instead of spelling the fields out.
Why: Every call site's copy must be found when the type gains a field, and a missed one treats a non-empty value as empty or drops the new field from the conversion.
Spot: A multi-clause boolean over one struct's fields at a call site outside its implementation; a value of type B built field by field from a value of type A at a call site, where a conversion from A, or from a type containing A, already exists.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### constants-state-their-origin
A magic value (a number in a formula, a tag, a byte prefix, a framing string) is a named constant that is spelled out and self-explanatory ('snapshot-cache-v1', not 'SC1'), with a comment saying where the value comes from (measured how, derived from what, copied from which source); constants that split one figure into parts say what each part corresponds to.
Why: An unexplained value cannot be checked or tuned, copies drift when one is changed, and maintainers cannot tell which part to adjust when the source changes.
Spot: A literal other than 0, 1 or a power of ten inline in arithmetic, especially beside named constants; a formula with unexplained factors; several new constants summed into one estimate, named after external functions, with no comment on what they correspond to or how they were measured. Short uppercase tags or byte prefixes introduced without a name saying what they mean. A function-selector constant with a generic name and no signature next to it.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config · look first at lines matching `\d|const|static|prefix|tag|selector|b\"|0x|immutable|constant|\"|'`
Checked by: ast-grep `unexplained-numeric-const`; ast-grep `sol-selector-constant-signature`; repo-script `sol-selector-comment-matches`; forge-lint `forge-lint literal-instead-of-constant (Foundry >= 1.8.5)`

### error-names-its-cause
An error names its actual cause in both variant and message: conditions with different remedies never share one message, an error from a branch the code considers unreachable names the broken invariant rather than reading like bad input, the variant constructed is the one whose name describes the failure (a catch-all only where none fits), and when handling starts to branch on the variant every producer of that variant is checked to use it for the meaning the handling assumes. A rejection of caller input states the broken rule in the caller's terms, not as a negated formula.
Why: Logs and callers cannot tell a data problem from expected policy, handling that branches on the variant (retry, reject input) does the wrong thing, and readers chase a cause that is not there.
Spot: One error string returned from several sites that guard different conditions; a new rejection branch reusing an existing generic message; a catch-all or fatal variant built in a length or format check on caller input or in the error mapping of a network call; an error mapped into a variant whose name describes a different cause than the wrapped error, often copied from a sibling module. Errors in branches guarded by an earlier non-empty check whose message reads like an input failure; a From/match that starts mapping variants differently with no change at their producers. One trailing error reached when any of several distinct checks fails (a fall-through `Err` after a chain of early `return Ok`/`continue`, or a message of the form 'neither X nor Y'): split it so each failed check names itself.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `err|fail|panic|bail|anyhow|revert|require|raise|throw|except|invalid|unreachable|expect|unwrap|from|variant|=>|\?`

### repeat-the-data-not-the-code
Code that applies one transformation to inputs that differ only in data writes the varying part once as data or alternatives and applies the shared transformation once (a table plus one loop, alternatives yielding the raw value plus one conversion).
Why: Repeated boilerplate hides the data, a one-off difference in one statement is easy to miss, and the shared step gets duplicated or forgotten when an alternative is added.
Spot: Five or more consecutive near-identical statements differing only in a key and a source field; a chain where one alternative converts its result inline while a helper repeats the same conversion for the others.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### name-at-its-actual-generality
A name is exactly as general as what it covers: an abstraction meant to be independent of its source or transport is named for the concept, not its current source; an item serving one implementation carries that implementation's name and lives in its module rather than taking a category name; and when a second kind joins a generically named item, the original is qualified with its kind (a breaking rename to schedule when the name is public).
Why: A category-named item reads as the shared abstraction, invites siblings to bend to it and collides with a later real generic item, and an unqualified name suggests it holds every kind.
Spot: New types, files or constants that serve one provider but carry a category name with no provider prefix, or sit at the top level while only one submodule uses them; a diff adding `<kind>_<x>` beside an existing bare `<x>` of the same family. Module, type or doc names embedding one provider or transport where the abstraction is source-agnostic.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### cost-figures-from-mechanism
A comment that justifies a cost names the mechanism that causes it, and any figure it gives is derived from that mechanism and identical wherever it appears (code comments, docs, change description).
Why: Figures drift apart between places and from reality as the underlying pricing changes, and a too-low figure makes a trade-off read as cheaper than it is.
Spot: The same quantity quoted with different values in two comments or between a comment and the change description; a bare '\~15k extra' cost figure with no stated cause.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs, pr · look first at lines matching `cost|gas|\d|overhead|cheap|expensive|budget|latency|memory|bytes|cycles|price|fee|weight|slow|fast`

### interface-docs-state-method-relations
When one method of an interface makes another unnecessary or changes how it is used, the interface's docs state that relation on both methods.
Why: Implementors cannot tell which methods they must implement meaningfully, and callers probe for the shortcut by waiting for an error instead of checking first.
Spot: Code that calls method B only after method A returned an error or nothing, with no doc on the interface describing the relation.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### measured-constant-states-scope
A measured cost constant states what the measurement includes, so the components modelled separately beside it are not counted twice; each figure is measured, not recalled; and the case that drives the figure is exercised by a test.
Why: A measured delta that already contains a transfer, added on top of a separate transfer term, inflates the estimate, and a remembered figure is never checked again.
Spot: A constant described as a measured delta (router-wrapped minus direct) added next to per-component terms; figures justified by 'I vaguely remember'; a unit test that asserts the total from the comment's own breakdown; a declared test constant for the measured venue that no test uses.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs · look first at lines matching `\d|measur|gas|cost|const|overhead|delta|bench`

### multi-flag-membership
When membership in an index depends on several flags (an entity is listed while any custom setting is active), each path that clears one flag removes the membership only once all flags are clear, and adding a new flag updates every such removal path, with a test for each combination.
Why: Otherwise an entity drops out of the index while it still has an active setting, or stays listed after it has none, so enumeration lies.
Spot: A new `hasCustomX` flag or similar added to a struct whose sibling `removeY` functions call `set.remove` after checking only the flags that existed before.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `has|flag|is_|bool|remove|delete|insert|add|set|enable|active|custom|index|member|contains|bitmap`

### one-descriptor-axis-for-variants
Names for variants of one quantity use one descriptor axis, taken from the vocabulary the codebase already uses (expected/actual/min, before/after fees). No two names in one scope differ by only a short prefix (amountOut/actualOut), and the bare generic name is not used once several variants coexist.
Why: Mixed axes (gross/net beside quoted/actual) and look-alike names make a reader, or an auditor, confuse which amount a line uses, and that is where fee and slippage bugs hide.
Spot: New parameters such as `grossAmountOut`/`quotedAmountOut` next to existing `amountOutBeforeFees`/`expectedAmountOut`; a function with both `amountOut` and `actualOut`; an amount variable without an amount noun (`effectiveMin`).
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### name-guard-parts
A guard built from a compound condition or inline arithmetic assigns the parts to named intermediate values (the floor, the flag) so that the condition reads as the rule it enforces, and needs no explaining comment.
Why: Inequalities with the arithmetic folded in hide which bound is checked, and the explaining comment drifts from the code.
Spot: An `if` whose condition spans several lines of arithmetic or `&&`/`||` clauses, accompanied by a comment that restates it.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `if|require|assert|guard|when|while|unless|&&|\|\||\band\b|\bor\b|[<>]=?|==|!=`
Checked by: ast-grep `commented-compound-guard`; ast-grep `sol-commented-compound-guard`

### public-docs-describe-observable-behaviour
The docs of an external or public function describe its behaviour through the parameters, return values and errors the caller sees; they never name the implementation's local variables, or internal steps the caller cannot observe.
Why: A caller cannot resolve an internal name without reading the code, and the doc goes stale as soon as the body is refactored.
Spot: NatSpec `@dev`/`@notice` or rustdoc on a public item that mentions a local (`effectiveMin`) or explains an internal use ('used for slippage surplus calculation').
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `//|/\*|^\s*\*|^\s*#|\"\"\"|'''`

### fill-input-gaps-only-when-unambiguous
Code that fills in parts missing from a caller's input (inserting steps, defaulting a route) does so only where the completion is unambiguous; otherwise it rejects the input with an error naming the ambiguity.
Why: Guessed repairs fail silently on edge cases (a whole input wrapped where only a share should be), and each fix adds another special case.
Spot: Encoder or normalisation code that inserts or rewrites items in caller-supplied input, with conditions growing per edge case (splits, cycles, remainders); a repair for something the stream or another layer now provides.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### bounded-fan-out
Work that fans out one thread or task per input item is bounded by a fixed worker cap independent of input size, counts nested fan-out levels as a product, and takes the concurrent path only when at least one item actually blocks.
Why: Uncapped per-item spawning at nested levels holds solutions x groups x requests threads for a large batch, and pure-CPU items pay spawn-and-join overhead for nothing.
Spot: A spawn-per-item helper (thread::scope, spawn in a loop, join_all over an unbounded Vec) applied to caller-sized input, especially called from inside another such helper.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `spawn|thread|scope|join|par_|rayon|parallel|concurren|task|worker|pool|gather|promise\.all|buffer_unordered|fan`
Checked by: ast-grep `unbounded-fan-out`

### validate-batch-before-side-effects
Before starting expensive or externally visible work for any item of a batch (network requests, rate-limited or billed API calls), run the cheap side-effect-free validation of every item, so a bad input is rejected before any request goes out.
Why: Validating each item only after the concurrent work completes sends real requests that count against rate limits and produce results nobody uses.
Spot: A batch path that validates inside the per-item closure passed to a parallel map, or after it returns.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### no-single-use-interfaces
An interface or trait whose implementors are only ever used as concrete types, or that has one implementer and one caller in the same repository, is removed and callers use the concrete type; an interface stays when it describes a contract outside the repo or callers are genuinely polymorphic.
Why: The indirection costs a file and a hop for every reader and has to be edited the day the single implementation changes.
Spot: A trait with one remaining method whose implementors sit in named struct fields; a Solidity `I<Contract>.sol` whose only implementer is a contract two directories away.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `trait|impl|interface|abstract|dyn\s|box<|abc|protocol|contract\s`

### fallback-is-observable
When code catches a failure and continues on a fallback path, falls back from a verified check to a weaker one, or demotes a crash to a logged non-fatal path, it records that the fallback ran (an event, log or metric naming the failed component and the values involved, with labels separating the expected cause from unexpected ones), and a demoted crash also gets an alert, so the fallback rate is observable from outside.
Why: A silently caught failure makes a misconfigured or permanently failing primary look the same as a healthy one, and its failure rate cannot be measured.
Spot: `catch {}` or `Err(_) => fallback()` with no emit, log or counter in the catch arm. A removed panic or fatal error replaced by warn! plus a fallback value, with no new metric or one unlabelled counter. A Solidity `catch` or `catch {}` without `(bytes memory reason)` whose event drops the revert data: the cause of an unexpected failure is lost and an operator must replay the transaction; the event should also index the keys its sibling events index.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: ast-grep `silent-fallback`; ast-grep `sol-silent-catch`; ast-grep `swallowed-err`

### zero-address-unset-excluded
Where a sentinel value stands for 'absent' (zero for an unset optional identifier) because the type cannot carry the case, every path that compares or matches against that setting excludes the sentinel the same way, since unset fields in the data it is compared with hold the same value.
Why: A zero immutable matches every record whose field is unset, routing all of them down the feature's path.
Spot: A config placeholder of address(0) for an immutable that is compared with equality against decoded data (`if (pool.extension == signedExclusiveSwap)`). A guard such as `if owner == ZERO { return 0 }` on one path next to equality matching on the same value elsewhere with no such guard; a zero placeholder for a setting that is compared with equality against decoded data.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config · look first at lines matching `zero|0x0|address\(0\)|\b0\b|default|none|null|sentinel|unset|empty|placeholder|max|-1`

### bounded-metric-labels
Metric labels take values only from small bounded sets (component kind, outcome, error class); unbounded identifiers such as addresses, entity ids or user ids go into logs, not labels.
Why: Every distinct label value creates a time series, so per-token labels blow up the metrics backend's cardinality.
Spot: `counter!`/`histogram!` label arrays containing addresses, ids or amounts.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `counter|gauge|histogram|metric|label|prometheus|observe|inc\(`
Checked by: ast-grep `unbounded-metric-label`

### signed-authorization-binds-reuse
A signed off-chain authorization binds every dimension along which it could be reused: who may redeem it, the minimum size of the action it was priced for, expiry and a single-use nonce.
Why: A signature missing one bound can be replayed by another caller, on a tiny trade, after it expired, or several times, each extracting value it was not meant to.
Spot: A signed struct that carries an amount to pay but no redeemer, minimum input size, deadline or nonce check.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `sign|ecrecover|permit|nonce|deadline|expir|eip712|typehash|authori|digest|quote`

### deferred-removal-marks-deprecated
When a change introduces a replacement for an existing mechanism but defers removing it, it marks the old mechanism deprecated with a pointer to the replacement on every item of its family (contract, constants, config keys) and states which one new code uses.
Why: Readers cannot tell which of two near-identical paths to pick, and an unmarked constant of the old family keeps attracting new uses.
Spot: A new contract or encoder that achieves what an existing one does, with no @custom:deprecated / #[deprecated] or selection rule on the old one.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config

### cache-learns-after-commit
An in-memory cache or index that mirrors a database learns of a write only after the transaction that made it commits (a post-commit hook or a poll of committed rows), never from inside the open transaction.
Why: A rollback or a retried batch leaves entries with no row behind them, visible to every reader until the next full reload, and sequence ids consumed by the failed attempt never come back.
Spot: A cache insert/upsert call inside a gateway method that writes through a transaction-scoped connection, or module docs that say the write-through 'runs before the enclosing transaction commits'.
Applies: rust, rust-test, script, python, sql · look first at lines matching `cache|memo|index|transaction|commit|txn|\btx\b|conn|upsert|insert|write.?through|mirror`

### every-cached-field-has-update-path
Every field of a long-lived cache has an update path in every process that enables the cache: a field that only write-through updates is cached only in processes that perform those writes, or the periodic refresh also covers it.
Why: A read-only process keeps that field from its startup load, so a filter on it silently excludes newer entities and the gap grows with uptime.
Spot: A cache enabled in a read-only command (an RPC server) whose refresh query covers only some of the cached fields, or docs that say a field 'converges through write-through alone'.
Applies: rust, rust-test, script, python, docs, sql · look first at lines matching `cache|refresh|write.?through|memo|reload|poll|converge`

### fast-path-parity
A fast path that answers requests in place of a reference path (a cache, an in-memory index, a shortcut for a single-item input beside a batched or threaded path) returns what the reference path returns for every request it accepts, failures included (status, deduplication, ordering, validation of bounds, and the same error naming the same cause for an input that fails), or hands the request to the reference path; each intended difference is listed in the PR, both implementations name each other, and a parity test runs both on the same inputs.
Why: A client gets a 400, a 404, duplicate entries or a different page from the fast path for a request the reference path answers, and the two implementations drift with nothing to catch it.
Spot: A new serve-from-cache or in-memory branch next to an existing SQL/DB path that adds its own rejection (unknown key, missing ids, negative pagination), skips deduplication the DB query gets for free, or reimplements filter semantics without a parity test. A special case for a one-item input that runs inline beside a path that dispatches to workers, where the worker path turns a failure into a generic error (a thread-join error that discards the panic payload) while the inline path reports it directly.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: ast-grep `join-panic-payload-dropped`; clippy `clippy::map_err_ignore`

### paginate-by-unique-order
A paginated query orders by a unique key before LIMIT/OFFSET.
Why: Without ORDER BY the database may change scan order between pages (index order up to one offset, physical order after it), so a client sweeping all pages skips or repeats rows.
Spot: A query builder call with .limit()/.offset() and no .order_by() on a unique column.
Applies: rust, rust-test, script, python, sql · look first at lines matching `limit|offset|paginat|page|order_by|order\s+by|cursor`
Checked by: ast-grep `paginate-without-order`

### watermark-polling-overlap
Incremental polling by a timestamp watermark either re-reads an overlap window or uses a value that follows commit order, because a timestamp set at transaction start (now(), a column default) can become visible after rows stamped later.
Why: With two concurrent writers, a poll that already advanced past a later stamp never reads the rows the slower transaction commits afterwards, and the cache serves their old values until a full reload.
Spot: A refresh query of the form `modified_ts > last_seen` where modified_ts comes from now() or a trigger, with no overlap subtracted and no argument that only one writer runs at a time.
Applies: rust, rust-test, script, python, sql · look first at lines matching `timestamp|_ts|modified|updated_at|now\(|watermark|last_seen|since|poll|refresh|created_at`

### background-task-liveness
A detached background task that keeps derived state fresh exposes its liveness (a last-success timestamp gauge with an alert), bounds each I/O it performs with a timeout, and backs off on repeated failures.
Why: A panic or a hung connection ends or stalls the task silently, and readers keep getting stale data with no signal.
Spot: tokio::spawn of a refresh loop whose JoinHandle is dropped, failures that only log error!, or a DB/HTTP call in the loop with no timeout.
Applies: rust, rust-test, script, python · look first at lines matching `spawn|tokio|task|loop|interval|refresh|background|thread|tick|sleep|timeout`
Checked by: ast-grep `rust-feed-spawn-handle-dropped`; ast-grep `rust-offchain-send-without-timeout`

### migration-lock-plan
A schema migration on a table the hot write path uses states the lock it takes and for how long, and either uses the non-blocking form (CREATE INDEX CONCURRENTLY outside a transaction, plus a check for an INVALID index left by a failed build) or comes with a deploy plan that keeps it clear of the writers.
Why: A plain CREATE INDEX holds a SHARE lock for the whole build and stalls every writer of the table, while a failed concurrent build leaves an INVALID index that `IF NOT EXISTS` then hides on rerun.
Spot: A new up.sql with CREATE INDEX on a large, frequently written table, run in a transaction (no metadata.toml) and with no note on lock or deploy timing.
Applies: sql · look first at lines matching `create\s+(unique\s+)?index|alter\s+table|drop\s+index|add\s+column|add\s+constraint|create\s+table|reindex|lock|concurrently`

### sql-functions-tested
Every stored procedure or SQL function a migration creates is called by a database test, because creating it does not resolve the calls and identifiers in its body.
Why: A typo or an upstream signature change in a nested call surfaces only when the scheduled job first runs in production.
Spot: A migration with CREATE PROCEDURE/FUNCTION and no test that CALLs it.
Applies: sql · look first at lines matching `create\s+(or\s+replace\s+)?(procedure|function)`

### absence-covers-in-flight-data
A conclusion that a value does not exist, drawn from missing it in an in-memory buffer and in the persistent store, covers every stage between the two (an in-flight commit task, a staged but unflushed transaction); data that has left the buffer stays visible to lookups until its write has landed.
Why: A lookup in the window between drain and flush finds nothing in either place and treats existing state as absent, for example deleting values on a rollback instead of restoring them.
Spot: A 'not in buffer and not in DB' branch where the buffer drains entries before a background task commits them, or where commits can sit in a write cache below a batch threshold.
Applies: rust, rust-test, script, python, substreams · look first at lines matching `buffer|cache|pending|flight|flush|commit|stag|queue|batch|\bdb\b|database|storage|store|gateway|persist`

### check-before-mutate
A handler that applies a change checks that it can complete before it mutates anything: it decodes and validates every input and performs every fallible fetch or lookup before the first write to its own or shared state, or confines the failure to the one item and leaves it retryable, so a failure leaves state, cursor and downstream consumers consistent; whether the current callers discard the object on error sets a finding's severity, never whether it is one.
Why: An error after the mutation leaves a half-applied change: the buffer is purged while the cursor is not advanced and no consumer receives a reconciliation event.
Spot: A destructive operation (split_off, purge, clear, remove) followed by a lookup that can still fail and return an error. A decode or validation step (`?`) interleaved with assignments to the state being updated, or an error arm reached after some assignments; a fallible fetch or lookup placed after writes to a store other items share, with no rollback or pending marker.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: ast-grep `mutate-before-fallible`; repo-script `delta-transition-partial-apply`; ast-grep `transition-write`

### log-level-follows-expectedness
The log level of a branch follows how expected its condition is: a state the code's invariants exclude logs at warn or above (or returns an error), and a handled race or routine fallback logs at debug; skipping input the code considers impossible is never logged only at trace.
Why: Unexpected states hidden at trace go unnoticed, while routine conditions logged at error fill the error logs and hide real faults.
Spot: trace!/debug! in a branch commented 'unexpected' or 'should not happen', or error! in a branch that normal operation reaches (a cache fallback, a known race).
Applies: rust, rust-test, substreams, script, python · look first at lines matching `trace|debug|info|warn|error|log|print|console\.|tracing`
Checked by: ast-grep `quiet-log-for-unexpected`; ast-grep `error-log-in-retry-loop`

### lifecycle-events-in-band
Lifecycle events that consumers must order relative to data (restart, reset, end) travel as typed variants on the same channel as the data and reach every subscriber, and each subscriber decides its own reaction; the producer does not special-case one consumer or use a side channel keyed by name strings.
Why: A string-keyed side channel fails silently on a name mismatch and gives no ordering against the data, and a producer that acts on one kind of consumer (clearing WS subscriptions) makes the subscription mechanism unusable for others.
Spot: A second channel (Sender\<String>, reset_tx) next to the data channel, or a supervisor that reaches into one subscriber type's state on restart.
Applies: rust, rust-test, script, python · look first at lines matching `channel|sender|receiver|\btx\b|\brx\b|broadcast|mpsc|subscri|restart|reset|stream|emit|send`

### retry-counts-consecutive-failures
A retry or restart policy counts consecutive failures: its failure count and backoff reset after a healthy run, and the first retry comes within seconds.
Why: Counting failures per process lifetime ratchets the backoff to its cap and lets a retry limit permanently stop a healthy worker after sporadic failures weeks apart, while a minutes-long first delay keeps a worker dark after a momentary blip.
Spot: Retry counters and backoff iterators declared outside the loop and never reset, or a backoff whose first delay is minutes.
Applies: rust, rust-test, script, python, ci · look first at lines matching `retr|backoff|attempt|reconnect|restart|failure|sleep|delay`

### generated-files-via-generator
Generated files change only by rerunning their generator; a diff to a generated file comes with the generator input or version change that produced it.
Why: Hand-edited generated code diverges from its source and is silently overwritten on the next regeneration.
Spot: Changes to files marked generated (codegen, protobuf/tonic output, bindings) with no change to their .proto, schema or generator version.
Applies: any

### deletion-keeps-needed-side-effects
When a change deletes code it made redundant, side effects in the deleted lines that are still needed (a metric emission, a log, a resource release) are kept.
Why: A metric or cleanup disappears with the line next to it, and dashboards or later code silently lose it.
Spot: A deleted block that contained more than the removed concept, for example a gauge update removed together with an unrelated emit_*_metrics() call.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### hot-callers-of-heavier-functions
A change that makes a widely called function do more work checks its callers in loops and per-item paths, and keeps a cheap path for them or hoists the call.
Why: A helper that was a constant-time match becomes an allocation-heavy construction on hot paths (per balance, per entrypoint) without anyone noticing.
Spot: A cheap accessor whose body now builds a larger structure, parses or clones, while grep shows calls inside loops.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### cheap-work-under-shared-locks
A lock shared with a latency-sensitive writer or reader is held only for cheap operations (cloning Arcs, swapping in a prepared value); deep copies, scans and rebuilds run outside it.
Why: A large request or a full reload holding the lock delays the producer or stalls every reader on a runtime worker.
Spot: Deep clones, per-row matching or vector rebuilds between lock()/write() and the guard's drop.
Applies: rust, rust-test, script, python · look first at lines matching `lock|mutex|rwlock|write\(|read\(|guard|arc|semaphore`
Checked by: clippy `clippy::significant_drop_in_scrutinee`; clippy `clippy::significant_drop_tightening (review only)`

### relied-on-properties-documented
A property a caller relies on (one consistent snapshot, an isolation level, an execution order, the meaning of each position in a returned array) is stated in the doc of the function or interface that provides it, the consumer's doc names the assumption, and an assertion checks it where that is cheap. Conversely, a doc neither advertises a usage no caller or test exercises nor promises an incidental property that would then become a contract a test must defend.
Why: Another implementation, or a refactor of this one, drops the property without anything flagging it. An over-specified contract blocks legitimate changes and grows tests that defend behaviour nobody needs.
Spot: A trait whose method doc promises 'all from one consistent read' without saying what the implementation must do to provide it. 'can also be used to ...' or 'share one X across Y to ...' with no caller or test doing it; a docstring listing a negative guarantee ('never X') no caller depends on, paired with a test asserting it.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs

### omitted-defaults-confirmed-with-consumers
Before an API leaves values equal to a default out of its responses (zero storage slots, empty fields), every consumer is confirmed to treat a missing value as that default, and the change is called out as client-visible.
Why: A consumer that distinguishes 'missing' from 'meaningful zero' (a fee set to zero) starts misbehaving.
Spot: A response-building filter that drops zero or empty values.
Applies: rust, rust-test, substreams, script, python · look first at lines matching `filter|skip|zero|empty|default|is_zero|retain|serde|none|omit|null`
Checked by: ast-grep `response-drops-default-values`

### domain-errors-mapped-at-transport
A service layer returns domain errors that say what went wrong, and the transport layer (the HTTP handler) maps them to status codes.
Why: A service that picks HTTP statuses itself wraps unrelated failures to get a code, mislabels server faults as client errors, and imports the handler module that imports it.
Spot: A service function returning the RPC/HTTP error type, or wrapping lock and merge errors in a transport error only to get a 500.
Applies: rust, rust-test, script, python · look first at lines matching `err|status|http|rpc|500|response|actix|axum|handler`
Checked by: ast-grep `service-returns-transport-error`

### reuse-fixture-builders
Tests that share a setup use the existing fixture builder, and repeated scenario construction becomes small helpers, so each test shows only its own data.
Why: Copies of a 20-line mock setup in every test hide what each scenario changes and drift when the shared setup changes.
Spot: A test that inlines a mock or fixture block identical to an existing helper, or repeats the same multi-line message literal in every test.
Applies: rust-test, solidity-test, python, script, integration-test, substreams

### log-messages-share-module-subject
Log messages of one module start with the same subject, so one grep finds all of them.
Why: A message phrased differently from its siblings is missed when operators grep for the component.
Spot: A new log line in a module whose other messages share a prefix that it does not use.
Applies: rust, rust-test, substreams, script, python · look first at lines matching `trace|debug|info|warn|error|log|print|console\.|tracing`

### registry-references-validated-at-creation
A value that refers to an entry of a process-global registry is validated against it when created, and the registry is filled by an explicit fallible step at startup rather than lazily on first access, so later lookups through infallible conversions cannot panic.
Why: A process that did not load the registry panics deep inside a From conversion or accessor instead of failing at startup.
Spot: A OnceLock::get_or_init that loads from disk, or accessors that resolve an id through a global registry and panic when it is missing.
Applies: rust, rust-test, substreams, script, python · look first at lines matching `oncelock|lazy|once_cell|static|global|registry|get_or_init|thread_local|singleton|cache`
Checked by: ast-grep `lazy-global-loads-io`

### explicit-flag-for-weaker-security
A mode that weakens security (disabling TLS, skipping authentication or verification) is enabled only by its own explicit flag and warns when active; it is never inferred from another input being absent.
Why: A user who simply forgets to set a credential silently gets a plaintext or unauthenticated connection to a remote service.
Spot: A security-relevant boolean derived from another value's absence (`let no_tls = api_key.is_none()`, `if key.is_none() { insecure }`), or two independent settings coupled in one condition.
Applies: rust, rust-test, substreams, script, python, config, ci · look first at lines matching `tls|ssl|auth|insecure|verif|api_key|secure|https|cert|skip|disable|token|key|password`
Checked by: ast-grep `insecure-from-absence`

### narrow-justified-suppressions
A suppression of a tool warning (lint allow, advisory ignore) sits on the narrowest item that raises it, never on a whole module or crate by convenience, and carries a comment naming the cause; where the fix is cheap and contained (a private interface), the warning is fixed instead of suppressed.
Why: A broad suppression hides later real warnings of the same kind across the whole scope, and an unexplained one can never be safely removed.
Spot: A new `#![allow(...)]` at the top of a module or crate, an `#[allow]` on a private function whose signature could change, or a new entry in an advisory ignore list without a comment.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config, cargo, ci · look first at lines matching `allow|expect\(|noqa|eslint|ignore|pylint|clippy|deny|advisor|audit|disable|lint|suppress`
Checked by: ast-grep `module-wide-allow`; ast-grep `allow-dead-code-suppression`; clippy `clippy::allow_attributes`; clippy `clippy::allow_attributes_without_reason`

### skips-reported-once-per-operation
A best-effort operation that skips items, or a condition many items hit in one operation, is reported once per operation (one warn line with the count, the total and the affected ids, matching the counter it increments), with per-item detail at debug.
Why: Skips logged only at debug make lossy output look complete, while a warn per item floods logs on every run.
Spot: A loop with `Err(_) => debug!(...)` or `continue` on a failed item inside serialization, conversion or export code, with no aggregate report afterwards.
Applies: rust, rust-test, substreams, script, python · look first at lines matching `continue|skip|err|filter_map|\.ok\(|warn|debug|fail|ignore|except`
Checked by: ast-grep `per-item-skip-no-aggregate`

### runbooks-list-observed-failures
Troubleshooting and operations docs list only failure modes that have been observed, each with how to detect it and a remedy that was tried; anticipated failures are not documented as fact.
Why: Speculative entries send operators chasing symptoms that cannot occur (stale `age` readings from a wrong RPC) and destructive remedies nobody tested (deleting a state cache).
Spot: A troubleshooting entry whose author cannot say when it happened, or whose cause, symptom and remedy are stated without a detection method.
Applies: docs · look first at lines matching `troubleshoot|fail|error|issue|problem|symptom|cause|remedy|fix|workaround|known|if you`

### structured-conditions-not-message-matching
A condition a consumer must branch on is exposed by the producer as a structured code or response type; consumer-side matching on message text is a marked stopgap with the producer-side fix filed.
Why: String matching breaks silently, and falls back to the generic error class, the next time the producer rewords its message.
Spot: `body.contains("...")` or `starts_with` on an error body or message to pick an error variant, or a client-side timeout added for a response the server should always send.
Applies: rust, rust-test, script, python · look first at lines matching `contains|starts_?with|ends_?with|includes|indexof|\bin\b|regex|re\.|message|msg|body|text|timeout|err`
Checked by: ast-grep `error-message-matching`

### reference-shared-config-by-id
Entities on the wire refer to a shared configuration object (a chain, a venue, a token list) by its identifier, and the receiver resolves it from a registry; they do not embed a full copy of the object in every entity.
Why: Embedding repeats the payload on every entity and lets copies drift from the registry, and every client model has to mirror the whole config shape.
Spot: A wire or DTO type whose field changes from an enum or name to a full config struct (`chain: Union[Chain, CustomChainConfig]`), or a serializer that writes a nested object where an id would do.
Applies: rust, rust-test, script, python

### benchmarks-sized-from-production
Synthetic inputs for benchmarks and load tests are sized from the distribution measured in production (typical and maximum), and the PR states those figures next to the chosen sizes.
Why: A benchmark over sizes production never reaches optimises the wrong regime, and reviewers cannot judge it without the real figures.
Spot: A benchmark or load test sweeping a size parameter (book depth, batch size, component count) with no stated link to observed values.
Applies: rust, rust-test, script, python, solidity-test, config · look first at lines matching `bench|criterion|load|stress|perf|throughput|latency|iteration|black_box|sweep|size|depth|count`

### callee-acquires-ordered-resource
When correctness depends on calls happening in an order (acquire a resource only after every fallible step), the function that uses the resource acquires it itself, so no caller can place a fallible step between acquisition and use.
Why: An order kept by convention breaks silently when a later edit adds an early return between the two calls, and the resource leaks.
Spot: A function that takes an already-acquired resource (a connected client plus its task handle) as separate parameters, with every caller and the test repeating acquire-then-call; a comment such as 'connect as late as possible'.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### finished-flag-cleared-on-restart
A flag that marks an object as permanently finished is cleared when the object is restarted, or the restart refuses with an error; a restart never reports success on an object every later call will reject.
Why: A reused client reports a successful reconnect while every later operation fails, with no log line or error telling the caller to build a new one.
Spot: A shared terminal flag (dead, closed, poisoned) set from Drop, close() or an exhausted retry loop, with no reset in connect()/start() and no check there.
Applies: rust, rust-test, script, python · look first at lines matching `dead|close|poison|finish|done|terminat|shutdown|stop|drop|restart|connect|start|exhaust|alive|running|flag|atomicbool`

### gates-apply-only-to-their-action
A delay or gate placed before one action applies only when that action will happen, and inputs that arrive during the wait and carry other information are kept or replayed rather than dropped.
Why: Messages discarded while waiting can carry events nothing else delivers (new or removed components, balance and TVL changes), so the consumer silently diverges.
Spot: A `continue` that discards incoming messages while `Instant::now() < not_before`, in a mode where the delayed action (a snapshot request) may not happen at all.
Applies: rust, rust-test, script, python · look first at lines matching `delay|sleep|wait|not_before|instant|timeout|throttl|debounce|until|interval|deadline|continue|gate|cooldown`

### throttle-caps-the-counted-unit
A throttle meant to stay under an external limit caps the unit the limit counts: first establish whether it is a per-second ceiling or a quota, and for a rate limit gate the items that arrive in bursts, not a coarser unit that only divides total volume.
Why: Sampling by a coarse unit (every Nth block) leaves the instantaneous rate unchanged inside each sampled unit, so a per-second limit still trips.
Spot: A sampling or throttling flag justified as 'to cap RPC usage' that gates on a container (block) while the requests are issued per contained item (each partial update).
Applies: rust, rust-test, script, python, config, substreams · look first at lines matching `throttl|rate|limit|quota|sampl|every|skip|interval|rpc|budget|cap`

### work-bounded-by-present-data
Work per input is bounded by the data actually present (the entries the source system itself visits), never by a range or distance an untrusted party can choose; mirror the source's sparse summary (an upper bitmap level) so a scan skips empty space.
Why: A cheap permissionless action that sets a wide span makes processing cost grow without bound and stalls the whole pipeline for as long as someone pays for it.
Spot: A loop over `low..=high` derived from external or user-supplied values with a store read or host call per step, where the source system jumps directly between non-empty entries.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: ast-grep `loop-over-caller-range`

### refactors-port-tests
A refactor that deletes or rewrites tests ports every test whose behaviour still exists, to the new location or to the integration suite.
Why: A behaviour the old suite pinned becomes untested, and a later regression (a dropped label) changes routing without any failure.
Spot: A diff that removes `#[test]` functions together with the code they lived beside, with no corresponding test added where that code moved.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, integration-test · look first at lines matching `#\[test|#\[tokio::test|#\[rstest|fn\s+test|function\s+test|def\s+test|\bit\(|describe\(|mod\s+tests`

### server-enforces-client-limits
Limits a client enforces on input it sends to a server (sizes, counts, formats) are enforced by the server too, which ignores or rejects oversized input; the client check is a convenience.
Why: Any client that skips or modifies the check (another language, an old version, a hand-written request) can send unbounded input.
Spot: Size or count constants (MAX_ENTRIES, MAX_HEADER_BYTES) added only to a client library for data it sends to a service.
Applies: rust, rust-test, script, python · look first at lines matching `max|limit|size|len|count|bytes|cap`

### one-clock-source
A component that needs the current time takes every clock reading it uses from its caller, or reads all of them itself; it never mixes an injected clock with one read internally.
Why: A half-injected clock makes the component neither deterministic under test nor consistent with itself (wall and monotonic readings taken at different points).
Spot: A constructor or helper that receives an `Instant` parameter but calls `SystemTime::now()`/`Instant::now()` inside.
Applies: rust, rust-test, substreams, script, python, solidity · look first at lines matching `now\(|instant|systemtime|clock|time|chrono|utc|date`
Checked by: ast-grep `injected-and-internal-clock`

### migration-docs-between-released-versions
User-facing migration docs describe the versions users move between; intermediate states that were superseded before users adopted them are dropped.
Why: Documenting transient rules users never ran adds constraints to reason about and confuses the upgrade path.
Spot: A migration guide listing several minor versions' rules (V3.0, V3.1, V3.2) where an intermediate one is only a step in an unreleased sequence.
Applies: docs · look first at lines matching `migrat|upgrade|v\d|version|breaking`

### comments-for-current-readers
Comments, module docs and identifiers are written for a first-time reader of the file with no diff open: no change-relative wording (now, no longer, unchanged, previously, the old/new X, renamed from) or new_/old_/legacy_ names, no PR change summary as a module doc, no negation of or argument against an alternative absent from the code (contrast with something external, such as a spec or an upstream system's behaviour, is fine), and no answer to anticipated review questions; justification against the code's history belongs in the commit message or PR description.
Why: Once merged the earlier version is gone, so the comment or name becomes a puzzle the code cannot answer, and a doc that is really the PR description goes stale immediately. A negation only informs someone who saw the rejected alternative or an earlier draft; everyone else is left asking why it would have been there. Review-facing comments outlive the review and read as noise or as references to a discussion the reader never saw.
Spot: Added comments, docstrings or identifiers containing 'now', 'no longer', 'unchanged', 'still', 'previously', 'old'/'new', 'simplified', 'renamed', 'moved', 'used to'; module-level `//!` docs saying what is unchanged or what 'only differs'; docs referring to code paths absent from the branch; Added comments with 'No ...:', 'rather than', 'instead of', 'we don't', 'deliberately not', or a paragraph arguing for this design against one not in the file; Comments such as 'intentionally ...', 'as discussed', 'to address review', 'this is fine because the previous ...'.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs, config · look first at lines matching `now|longer|unchanged|still|previous|old|new|simplif|renam|moved|used to|legacy|instead|anymore|formerly|replac`
Checked by: pr-script `history-relative-comment`; pr-script `negation-of-absent-alternative`

### guards-fail-closed
A guard that prevents something unsafe fails closed: a failed decode, a missed lookup or an errored check of the guard's input is never read as 'safe' or 'absent', and where the guarded action has an atomic precondition (a conditional write), the guard uses it instead of a separate check whose errors are discarded.
Why: `.ok()` or `None` on the guard's input silently leaves the guarded thing enabled. A separate check that discards its errors fails open, and even when it works it races with a concurrent writer.
Spot: `.and_then(|v| hex::decode(v).ok())` or `unwrap_or_default()` feeding a guard, pause or disable decision; key lookups whose miss means 'no restriction'; Shell guards like `if ! aws s3 ls "$path" >/dev/null 2>&1; then upload` or any check-then-act whose check output and exit status are thrown away.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, ci

### copied-content-rewritten-for-new-site
Anything copied from a sibling file (metadata such as author, owner, tuning or capability fields; comments; error reasons) is rewritten for the new site: the copier can explain each item, values are updated for the new file, jargon reasons are replaced with the actual condition, and fields nothing reads are not presented as having an effect.
Why: Wrong authorship and dead fields mislead readers about who owns the package and what drives the system's behaviour. A copied comment nobody can explain misleads the next reader, and a vague error reason tells nobody what was rejected.
Spot: Author or owner fields matching the file they were copied from; manifest fields that nothing reads described or set as if they were functional; Comments or error strings in a new match arm or entry that are identical to a sibling's; the author answers 'copied from the other executors' when asked about one.
Applies: any
Checked by: pr-script `copied-author-metadata`

### docs-instructions-work-as-written
Instructions and worked examples in a README or guide work when followed verbatim: each command runs from the working directory the document uses, the setup lists every input the program requires (each variable or argument read without a fallback), every option offered works or says what more it needs, and every value in an example (names, ids, numbers, recommended settings) belongs to the scenario it describes.
Why: A path relative to a different directory fails when copy-pasted. A user who follows the README hits a panic on a missing variable, or on an option the text presents as supported. Readers copy examples verbatim, so a value left over from another scenario (another chain's package name or start block) makes their setup quietly wrong.
Spot: New `cd <relative path>` or relative paths in a docs code block whose surrounding commands run from another directory; Environment or argument reads that fail when absent (`expect`/`unwrap` on an env read) in an example's entry point that the README's setup block does not mention; README prose offering a choice where the code fails for some values; An example for scenario X containing names or numbers belonging to scenario Y (an `ethereum-` package in a non-Ethereum example, a start block from another chain), or an example value that contradicts the advice next to it.
Applies: docs, rust, script, python · look first at lines matching `cd\s|\$|env|var\(|getenv|environ|args|```|cargo|npm|pip|export|run|path|\./|--`

### value-change-follows-every-hop
When a value's meaning, unit, scale or type changes, everything along its path changes in the same change: the parameter is renamed at every hop it passes through, names abbreviated from the old type are renamed, no leftover conversion factor or lossy compatibility shim remains, and every comment and test literal names the new meaning or unit.
Why: A stale name makes readers assume the old value and invites off-by-one bugs in later edits. A leftover scaling multiplies an already-converted value, and a shim such as 'downscale, but return at least 1' changes amounts under a legacy interface nobody should rely on. Abbreviations of the old type tell the reader the wrong type and API.
Spot: Call sites passing a newly computed value into a parameter still named after the old source; a rename in one function but not its callers or callees; `* 10_000` or `/ 10_000` left next to a parameter whose type was widened; comments saying 'legacy BPS scale' on uint32 values; accessors that downscale and clamp to 1 so old callers still see 'non-zero'; A changed type annotation on a binding whose name is an abbreviation of the previous type.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python
Checked by: ast-grep `bps-scale-factor`; ast-grep `sol-bps-scale-factor`

### qualifier-suffix-when-type-lacks-it
A constant or variable whose type does not carry a qualifier the value depends on (its unit, the environment it is valid for) names it in a suffix, as its siblings do (`_SECS`, `_BYTES`, `_GAS`, `_MAINNET`); a newtype carrying the unit is the stronger form.
Why: Without the suffix a value for one environment gets used in a test for another and only fails at runtime. Without the unit a reader has to infer from the doc comment or the call site whether 2000 is a count, bytes or milliseconds, and values in different units combine without warning.
Spot: New constants in shared test constants files without the `_PROD`/`_MAINNET`-style suffix their siblings carry; A new integer constant for a time, size, cost or rate quantity named only after the concept, with the unit only in its doc comment, next to siblings that carry a suffix.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config
Checked by: repo-script `chain-suffix-on-test-constants`; ast-grep `unitless-quantity-const`

### error-layers-keep-their-meaning
Each layer an error passes through states only what that layer knows: a reused helper's message is true for every caller, a caller wrapping an error adds the context of its own operation and lets the callee say why it failed, and an error that already carries its classification (a status, a client-vs-server class) propagates unchanged instead of being re-wrapped into a generic variant.
Why: A shared parser reporting one caller's meaning ('invalid user id') misleads when another caller passes a different kind of id. A caller-chosen reason becomes wrong as soon as the callee gains another failure mode, producing misleading messages. A client error (400, 404) from the callee reaches the client as a 500, so it looks like a server fault.
Spot: Shared util functions whose error strings name one specific use; map_err(|e| ..format!("Missing X: {e}")) where the callee can fail for reasons other than a missing X; `.map_err(|err| SomeError::Unknown(format!("...: {err}")))` on a call that already returns the caller's error type.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python · look first at lines matching `err|fail|panic|revert|require|expect|raise|except|bail|anyhow|context|msg|message`
Checked by: ast-grep `guessed-cause-in-wrap`; ast-grep `error-reclassified-unknown`

### slot-holds-what-its-name-says
A parameter or field receives only values of the kind its name and the other users give it: a shared request field means the same thing for every implementation, and a helper is called only with arguments of the kind its parameters name. A value only one caller or implementation needs is produced inside it or travels in its own field or helper, never smuggled through a slot that works only by an implementation detail.
Why: Repurposing a shared field makes it mean different things per implementation, so a later change that restores its common meaning silently reintroduces the bug the repurposing worked around. The call works by accident of an implementation detail and breaks silently when that detail changes, and readers cannot tell what the call means.
Spot: One implementation fills a shared struct field (sender, receiver, recipient) with a synthesized value that the field's name and the other implementations do not describe; A call that passes a computed fragment or placeholder into a parameter named for a different entity, often with a comment explaining why it still works.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### docs-anchor-status
A doc that mentions something whose status changes over time states that status absolutely: an external system's behaviour is anchored to the version, date or block from which it applies (or 'not yet live'), and a defect found in the project's own code says whether and since when it is fixed; no time-relative 'later', 'upcoming', 'new' or bare 'discovered'.
Why: 'The later policy' reads as not yet active while it already applies, so readers underestimate when the documented limitation bites. Without it readers take the defect as still present.
Spot: 'later', 'upcoming', 'new', 'recent' or 'soon' describing an external rollout in a README or doc comment, with no version, date or block; Prose like 'discovered several critical vulnerabilities' with no statement that they were fixed.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, docs · look first at lines matching `later|upcoming|new|recent|soon|not yet|planned|future|currently|now|latest|vulnerab|deprecat|will|today|yet|live`
Checked by: ast-grep `time-relative-external-status`; ast-grep `sol-time-relative-external-status`

### protection-cannot-reach-zero
A protective bound can never be set or rounded to a value that disables it: a floor derived from another value (a minimum from a tolerance, any bound from rounding-down division) is rejected when it reaches zero, and a protection callers apply for the sake of shared infrastructure (restart jitter, backoff, a rate limit) has a floor they cannot configure away, values below it being raised with a warning or rejected.
Why: A 100% tolerance, or rounding on tiny amounts, turns the slippage guard into 'accept any output' without any error. If every client may set the protection to zero, it no longer prevents the stampede it exists for.
Spot: Tolerance parameters accepted up to their full range (e.g. `max_slippage_bps` up to 10_000), or `floor = x * (D - t) / D` with no check that the floor or the resulting minimum is non-zero; A CLI flag or builder setter documented with 'set to 0 to disable' on a mechanism that protects the server rather than the caller.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config · look first at lines matching `slippage|toleran|bps|min|max|floor|ceil|bound|limit|threshold|10_?000|percent|basis|cap|deadline|guard|clamp`
Checked by: ast-grep `rounded-down-floor`; ast-grep `sol-rounded-down-floor`; ast-grep `disable-protection-by-zero`

### change-reaches-every-sibling
A change made to one member of a family of siblings that play the same role (implementations, match arms handling the same kind of outcome, methods of one API family) is made to every member in the same change, or moved into shared code: a guard added for a shared constraint, the metrics recorded per outcome, and a new parallel entry point covering every member it mirrors, with existing hand-rolled equivalents moved onto it.
Why: Siblings without the guard accept inputs that fail later, and the gap only shows once someone routes through them. A branch that records only the aggregate makes its events invisible in the per-item breakdown, which reads as the event not happening. Partial parity forces callers back onto the old API for the missing variant, and hand-rolled copies keep diverging from the new registry.
Spot: A new rejection branch in one of several sibling encoders/handlers; grep the siblings for the same check; A match over results where most arms call a per-item counter (e.g. per protocol) and one arm (Timeout) only increments the total; A new `x_by_name` beside `x`, `x_with_context`, ... that mirrors only some of them; custom registries elsewhere in the repo (integration tests, test crates) left in place.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python

### every-exit-path-cleans-up
Every way a producer or resource holder can stop (an error return, each arm of a select, a retry limit, a rebuild failure, a supervisor's shutdown, cancellation of an asynchronous operation while it waits) performs the same cleanup as the path the change targeted: it releases what it acquired the same way and sends consumers the event that makes them drop state derived from it. Where cancellation cannot run that cleanup, an owner of the resource (a drop guard) does, or the uncovered path is commented at the wait.
Why: A consumer that only sees its receiver close keeps buffered data and merges it into every later answer indefinitely. The path left with a weaker release (a bare close that a reconnecting task never reads) keeps leaking the resource the fix was meant to free. A cancelled future never reaches the Err arm, so the resource leaks on exactly the path a caller under a timeout takes, and readers assume it is covered.
Spot: An early return from a supervisor or producer loop placed before the code that broadcasts the restart/reset event; A cleanup helper called on one arm of a match/select while a sibling arm drops the handle or calls a weaker close; a PR measurement that only covers the happy shutdown; Cleanup written only in an `Err(e) =>` arm after a long `.await` inside a public async fn, with nothing owning the resource across the await.
Applies: rust, rust-test, script, python

### config-docs-serve-the-operator
Configuration docs serve the person setting the system up: a reference table lists required entries before optional ones, each field's doc says what reads it, what it changes and how to choose a value (unit plus a reference point), and an example configuration sets only values the reader must provide or that deliberately differ from the default.
Why: A reader setting up the system has to scan the whole table to find the few values they must provide. Restated defaults hide which lines matter, and they go stale when the default changes. A reader cannot pick a sensible value for a threshold described only as 'TVL gates for component tracking'.
Spot: A docs table of environment variables or config keys with a Required column whose 'Yes' rows are scattered among the optional ones; An example .env or YAML in docs that contains keys set to the value the reference table lists as the default; A config table row or field doc that repeats the field name in other words and gives neither the consumer nor a unit or guidance for choosing a value.
Applies: docs, config

### fixtures-match-the-real-system
Fixtures standing in for an external system's input or output match what that system really produces and accepts: recorded responses are captured from it and re-captured when its shape changes (no placeholder values or invented structure), and hand-picked configuration satisfies the validation the real system applies, so a test cannot pass on input production would never see or would reject.
Why: Tests built on a hand-written fixture keep passing after the real shape diverges, and anyone changing the client against the fixture works from the wrong shape. A test that passes with input the system rejects is exercising a path that never runs in production and hides that the validation is untested.
Spot: Recorded-response fixtures containing placeholder values (0xdeadbeef, round numbers) or a structure (entry count, fields) that differs from a live response; Hand-picked configuration values in a test without a reference to the validation the real system applies to them.
Applies: rust, rust-test, solidity-test, python, script, integration-test, substreams, config

### test-name-form-per-language
Test functions use the repository's name form for their language: `test_<description>` in Rust, `test<Description>` in Solidity (Foundry discovers tests by the `test` prefix).
Why: One form per language keeps tests findable by name and lets tooling filter them; a Foundry function without the `test` prefix silently never runs.
Spot: A Rust `#[test] fn` without the `test_` prefix beside prefixed siblings; a Solidity test function not starting with `test` (it is not run).
Applies: rust-test, solidity-test · look first at lines matching `fn\s|function\s`

### optional-feature-keeps-base-inputs
A change that adds an optional feature (a subsidy, a fee, a hook) keeps processing every input the base accepted: where the feature cannot apply, the input goes through without it, and rejecting a previously accepted input is a stated and tested decision.
Why: A new rejection on the feature's path silently drops inputs that worked before (routes that stop encoding, pools that stop decoding), and nothing in the diff shows the regression.
Spot: A new `Err`, `InvalidInput`, `return None` or revert in an encoder, validator or decoder that fires on an input shape the base handled (cyclical, multi-hop or split routes, a pool type), guarded by the feature being present; compare with the base's tests for that shape.
Applies: rust, solidity, python, substreams

## PR scope and hygiene

### ci-pinned-toolchain
CI builds and tests with the exact toolchain the release pins, not floating channels, and every package builds under the toolchain it pins (lockfile version, per-package rust-toolchain.toml); code that needs a newer compiler or std API bumps the pin in the same change.
Why: With floating channels CI failures are unpredictable and may not reflect the release config, and release builds use the pin, so code needing a newer compiler, or a pin older than the lockfile format, breaks the reproducible build.
Spot: Workflows using `stable`/`nightly` alongside a rust-toolchain.toml pin; per-package toolchain files older than the Cargo.lock version; stale per-package lockfiles. Recently stabilised std APIs (`as_chunks`, `LazyCell` `DerefMut`, ...) in a package whose toolchain pin predates them.
Applies: ci, cargo, rust, rust-test, substreams
Checked by: repo-script `toolchain-consistency`

### shared-code-change-needs-test-and-owner
A change to shared logic that every integration or plugin runs through (a common decoder, dispatcher or pipeline stage) comes with a test pinning the new behaviour and sign-off from that code's owner.
Why: Shared-path changes affect every consumer of that path; without a test the intended behaviour is not pinned.
Spot: Hunks in shared decoder/dispatch code that every integration passes through, without accompanying tests or the owner's review.
Applies: rust, rust-test, substreams, solidity, script, python, pr
Checked by: pr-script `shared-code-without-tests`

### ci-path-filters-match-inputs
CI path filters match only the files the triggered jobs actually read.
Why: Over-broad filters run expensive matrices (and, on fork PRs, environment approvals) for unrelated changes.
Spot: New glob entries like `config/**` in changed-files filters where only one file in that directory feeds the job.
Applies: ci · look first at lines matching `paths|path|filter|\*|changes|files`

### out-of-scope-changes-own-pr
Changes outside the PR's stated purpose go into their own PR: shared harness or tooling edits inside a feature or integration PR, user-visible behaviour changes inside a refactor, and growth on a draft beyond its original scope.
Why: Unrelated changes ride along unreviewed by their owners, block the main change on their discussion, and make reviews and reverts coarse.
Spot: A feature or integration PR that also edits a shared test harness or a CI workflow; a refactor PR that also changes computed results, limits or error classification; edits to files the description never mentions.
Applies: pr, any
Checked by: pr-script `integration-touches-shared`

### split-breaking-changes-by-audience
A large redesign is split into a stack of independently mergeable PRs, and breaking changes with different audiences ship separately: compiler-caught library breaks apart from deployment breaks (renamed identifier strings, ids in configs, dashboards).
Why: Bundling a deployment-facing rename with a library refactor forces every operator to hand-edit configs in the same upgrade and spends the expensive break before the naming is settled.
Spot: One PR that both changes Rust signatures or serialization and renames string identifiers users put in config (name prefixes, id formats, CLI values); PRs above \~5k changed lines mixing a new layer with its consumers.
Applies: pr, any
Checked by: pr-script `oversized-pr`

### only-intended-changes-in-diff
The diff contains only intended changes: lockfile churn and files regenerated as a build side effect (e.g. bindings rewritten by a build script) are reverted, and formatting uses the same formatter version and settings as CI.
Why: Incidental churn obscures the real diff, can smuggle dependency upgrades into a merge, and forces history rewrites to remove it.
Spot: Lockfile changes in packages the PR doesn't otherwise touch; large diffs in generated files unrelated to the change; formatting diffs produced by a different formatter version than CI's.
Applies: any
Checked by: pr-script `lockfile-churn`; pr-script `generated-abi-rewritten`; ci `cargo +"$RUST_NIGHTLY" fmt --check, forge fmt --check (already in CI)`

### docs-updated-with-behaviour
Behaviour changes that the docs describe (agent docs such as CLAUDE.md, architecture docs, docs/, field and function docs) update those docs in the same PR, and every doc enumerating the affected cases gains the new case. Docs that describe what is deployed or available to users (deployed addresses, supported-protocols listings) change at launch, not at merge (`deployment-artifacts-after-launch`). An edit to a behaviour doc keeps every statement the unchanged code still implements; each removed statement is checked against the code.
Why: Agent and developer docs drift and then mislead the next change. Rewriting a list while adding one step silently deletes rules the code still enforces.
Spot: Changes to described behaviour (endpoints, features, conventions, data flow) without doc updates. A new assignment to a documented field or a new allowlist beside existing ones with no edit to the paragraph listing them. A doc hunk that removes sentences or bullets about code paths the diff does not touch.
Applies: rust, rust-test, substreams, solidity, solidity-test, script, python, config, docs, pr

### privileged-roles-documented
A change that adds privileged roles or signing keys documents each one (what it gates, who holds it) in the operations or secrets documentation in the same change.
Why: Operators rotating keys or responding to an incident cannot find which role gates which function.
Spot: New `bytes32 public constant *_ROLE` or signer setters in a diff that touches no ops/secrets doc.
Applies: solidity, rust, script, python, config, ci · look first at lines matching `role|signer|admin|owner|key|privileg|auth|access|grant|secret|multisig`

### author-verifies-assistant-claims
The author can explain every line of a change, and a justification produced by an assistant or another unverified source is checked against the code before it is used to justify the change or written into docs.
Why: Unchecked assistant output lands wrong claims in docs and reasons in reviews, and reviewers end up debugging the assistant instead of reviewing the change.
Spot: Review replies or PR text that cite an assistant ('Claude says this is fine', 'Claude confirmed'), or docs and code the author cannot explain when asked.
Applies: pr, any

### client-sync-changes-run-live
A change to the client's connection, reorg or stream-synchronisation logic is run against a live endpoint for a sustained period before merge, with its fallback paths logged, and the run's counts (blocks, reverts, partial updates, fallbacks fired, errors) reported.
Why: These paths depend on timing and endpoint behaviour that unit tests do not produce, and a regression there stalls every consumer.
Spot: A tycho-client feed/deltas/block_history change whose PR shows only unit tests.
Applies: rust, rust-test, python · look first at lines matching `feed|delta|block_history|reorg|revert|stream|synchron|websocket|\bws\b|connect|snapshot`
