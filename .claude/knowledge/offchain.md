# Off-chain venues

Rules for venues priced off-chain (RFQ, books, price-level streams) on the snapshot feed layer in
`crates/tycho-simulation/src/{snapshot_feed,book,rfq,pamm,price_level_stream}/`. Their states are
`ProtocolSim` implementations, so `simulation.md` applies to them as well.

## Rules

Each rule is a record: its id, which review findings cite; the rule; why it matters; how to spot a
violation in a diff (Spot); the kinds of files it applies to; and the tools that check it, in part
or in full (`.claude/skills/tycho-review/SKILL.md` lists the file kinds).

<!-- pieces -->

### offchain-chain-support-resolved-once
Chain support and chain-dependent configuration are resolved once at build time: when the venue's API takes a chain id, the venue's own refusal decides support (and ends the feed); when our request must name the chain, keep our own list, refuse other chains at build, and hold the list to the venue with ignored live tests; an unsupported chain is never retried.
Why: Per-poll resolution panics or retries forever on an unsupported chain, and a hard-coded list drifts from what the venue serves and refuses chains it prices.
Spot: chain.id() or URL formatting inside the poll or quote path; a hard-coded chain match in a client whose API takes a numeric chain id; retries on a chain refusal.
Applies: rust, rust-test · look first at lines matching `chain|url|network|id\(\)`
Checked by: ast-grep `rust-offchain-chain-id-in-request-path`

### offchain-component-identity
An off-chain component's id is stable across updates and carries its identity in the clear: protocol system, the canonically ordered pair, and the maker when quotes are per maker, so it corresponds to exactly what one binding quote can fill; never derived from response ordering or a hash.
Why: Ids from response order make components churn between polls for the same pair, one component spanning several makers simulates depth no single quote can fill, and a bare venue name collides with an on-chain integration of the same venue.
Spot: Component ids built from tokens in API-response order; a map keyed by pair that keeps whichever maker iterated last; keccak256 of a formatted id string; ids without the protocol_system prefix.
Applies: rust, rust-test · look first at lines matching `id|component|pair|maker|keccak|hash|format!|protocol_system`
Checked by: ast-grep `rust-offchain-hashed-component-id`

### offchain-failure-classification
Every venue failure is classified by whether another attempt could answer differently. At the HTTP layer the response's meaning decides, not its cosmetics: an error envelope returned with HTTP 200 is an error, a 5xx is retried whether or not its body parses, and a success=false flag is handled in one place with one verdict. At the error layer, a failure is fatal only when no retry can help (unsupported chain, unbuildable request); a declined size, a missing quote, a rejected credential or a rate limit is recoverable or reported as what it is; and conversions into simulation errors keep the classification and the venue's reason.
Why: Blanket fatal classification ends healthy feeds and stops simulation of healthy books over one trade; misfiled 'connection' errors trigger pointless retries; flattened errors hide the venue's verdict.
Spot: Any HTTP/socket error mapped to a fatal variant; documented venue decline codes mapped to fatal; a blanket From conversion mapping every variant to FatalError; error messages that drop the status code or body. `if !status.is_success()` branches that return instead of retrying; `response.json::<Quote>()` without first checking for an error shape; the same success flag checked in caller and callee.
Applies: rust, rust-test · look first at lines matching `http|status|response|reqwest|5\d\d|error|err|fatal|recoverable|retry|success|json|from<|declin|rate`
Checked by: ast-grep `rust-blanket-from-fatal`

### offchain-quote-response-validated
A binding quote's response is validated against the request and against what the executor needs (tokens, amount, expiry, recipient, payable value, calldata length) before it is encoded.
Why: Malformed or expired venue calldata otherwise fails only on chain, a quote for a different trade goes through, and stored-but-unread fields suggest validation that never happens.
Spot: Hex-decoded calldata inserted into quote data without a length check; responses used without comparing them to the request; signature/deadline fields stored but never read by the encoder.
Applies: rust, rust-test · look first at lines matching `quote|calldata|signature|deadline|expir|recipient|encode|hex|response|payable|value`

### offchain-partial-response-parsing
A poll built from several entries or sub-requests (entries of one response, or one request per pool) drops only the failing ones, logging the count and first reason, and publishes the rest; when nothing succeeds it is an error that counts against the failure budget, never an empty snapshot reported as success.
Why: All-or-nothing parsing withdraws every pair on one bad entry, while unconditional dropping lets a wire-format change that breaks every entry silently withdraw all books.
Spot: `.collect::<Result<Vec<_>, _>>()?` over a response's pairs or levels; a filter_map discarding parse failures with no warn and no empty-result check. A per-item fetch loop that `warn!`s and `continue`s on error, then returns `Ok` with whatever is left, with no check that anything succeeded.
Applies: rust, rust-test · look first at lines matching `collect|filter_map|continue|warn|poll|fetch|for\s|ok\(|vec|parse|entr|level|pair`
Checked by: ast-grep `rust-offchain-collect-result-bail`; ast-grep `offchain-per-item-failures-swallowed`

### offchain-venue-representations-checked
Venue-specific representations (native-token alias, opt-in query flags) are checked against the venue docs and Tycho's convention (native token = zero address); normalisation for a value no consumer can pass is removed.
Why: A missing opt-in flag silently drops whole books (e.g. native-token books), and dead normalisation code and its tests pin a representation that cannot occur.
Spot: Address-alias constants like 0xEeee... with normalize functions; API requests lacking documented optional params that gate data the integration expects.
Applies: rust, config · look first at lines matching `0xeeee|native|eth|alias|normali|query|param|flag|zero|address`
Checked by: ast-grep `rust-native-sentinel-literal`

### offchain-override-heuristics-scoped
Heuristics that extract values from streamed state overrides are scoped to the contract and slots whose layout they assume, or replaced by a deterministic source.
Why: A plausibility check over every overridden slot picks up unrelated writes and resolves a wrong timestamp, defeating the staleness guard.
Spot: A function scanning all overridden storage slots for 'plausible' values (timestamps, prices) without filtering by contract address or slot position.
Applies: rust, rust-test · look first at lines matching `override|slot|storage|plausible|scan`

### offchain-binding-quote-pinned-to-simulated-book
A binding quote is requested from the same book the state simulated: the request names the maker/book and disables fallback to others.
Why: Otherwise the venue answers from a maker the route was never priced on, and execution diverges from simulation.
Spot: Quote requests without a maker identifier where books are per maker.
Applies: rust, rust-test · look first at lines matching `maker|book|fallback|request|quote|rfq`

### offchain-quote-within-served-range
A price-level state quotes only inside the range the venue serves: it never extrapolates below the smallest or beyond the largest level, it rounds an advertised minimum so it stays fillable (down to atomic units, not up), and it refuses amounts below a venue floor once that floor is measured.
Why: Venues enforce minimums and ladders can be broken, so extrapolated quotes revert or are wildly wrong, and an unenforced floor makes routes simulate clean and fail at encode time.
Spot: Linear scaling of the first level below it or of the last level beyond it; ceil/round-up on minimum amounts; venues with known refusal floors that get_amount_out does not apply.
Applies: rust, rust-test · look first at lines matching `level|min|max|extrapol|ceil|round|floor|scale|amount|refus`
Checked by: ast-grep `rust-offchain-minimum-rounded-up`

### offchain-freshness-failures-classified
Execution validation and metrics classify quote-freshness failures (stale oracle or lane for the target block) separately from real reverts.
Why: Mixing designed stale-skips into revert counts makes healthy venues look broken and hides genuine reverts.
Spot: Execution validation that counts all reverts alike for off-chain priced venues; no handling for venue staleness error selectors.
Applies: rust, rust-test, integration-test, script · look first at lines matching `revert|stale|fresh|valid|metric|selector|error|oracle|lane|execut`

### offchain-feed-decoupled-from-consumer
A feed owns its connection in a dedicated task and publishes its latest complete snapshot through a latest-value channel; the consumer's pace never decides when the socket or poll runs, and there is no per-update queue or backpressure onto the connection.
Why: A socket polled only when the consumer polls misses the venue's keepalive deadline and is closed; a queue replays stale updates after a stall, while a latest value wakes a stalled consumer to the newest book only.
Spot: Generator-style streams that read the socket inside the consumer-facing stream; socket reads in the task that applies updates under locks; bounded mpsc between feed and consumer for absolute snapshots; tx.send().await in the reader loop.
Applies: rust, rust-test · look first at lines matching `spawn|task|stream|socket|ws|watch|mpsc|channel|send|recv|poll|select!|lock|connect|feed`
Checked by: ast-grep `rust-offchain-bounded-feed-channel`; ast-grep `rust-offchain-io-in-consumer-stream`

### offchain-complete-snapshots
A feed publishes complete snapshots; additions and removals are the consumer's bookkeeping, and an unchanged component is announced once and refreshed afterwards, never re-announced as new on every update.
Why: Producer-computed diffs need exactly-once delivery, which forces a queue, and re-announcing the whole book costs CPU downstream on every update.
Spot: Feeds computing removed/new components against what they sent before; every snapshot state reported as a new pair.
Applies: rust, rust-test · look first at lines matching `new|remov|snapshot|add|updat|component|pair|previous|sent|known`

### offchain-shared-transport-lifecycle
Transport and lifecycle behaviour (connect/reconnect, poll loop, failure counting, backoff, common config, TVL filter, component construction, HTTP send/status/parse) is written once and drives every venue; per-venue code is limited to its configuration, decoding its data into states, and its API client.
Why: Copies drift: a behavioural fix lands in one copy and not in the others.
Spot: A new venue adding its own reconnect/poll/backoff loop, retry helper, or its own config struct.
Applies: rust, rust-test

### offchain-timeouts-and-backoff
Every request to a venue has a timeout, and repeated failures back off (resetting on recovery without a catch-up burst); an invalid cadence such as a zero interval can't be configured (`invalid-range-inputs-typed`).
Why: Otherwise a dead or rate-limiting API is hammered forever, a hung request stalls the feed, and a zero interval panics.
Spot: HTTP requests without a timeout; fixed-interval polling with no failure counter; polling loops that catch up missed ticks.
Applies: rust, rust-test · look first at lines matching `timeout|interval|poll|backoff|retry|sleep|tick|http|reqwest|request|client|send|duration|fail`
Checked by: ast-grep `rust-offchain-send-without-timeout`

### offchain-credentials-held-by-client
Credentials and endpoints are constructor arguments held only by the client that talks to the venue; library code reads no environment, and states and data sources never hold a credential.
Why: Environment reads hide configuration and get silently re-read elsewhere; credentials in Debug output or serialized state leak into logs and recordings.
Spot: std::env::var in library code; #[derive(Debug)] or Serialize on structs holding keys; credentials stored on states or sources.
Applies: rust, rust-test · look first at lines matching `env|key|secret|token|credential|auth|password|debug|serial|endpoint|url|header|bearer`
Checked by: ast-grep `rust-credential-debug-derive`; ast-grep `rust-credential-serialized`

### offchain-withdraw-stale-observable-end
Stale data is withdrawn and termination is observable: a published value is taken away once older than a maximum age or when the feed ends, and the end carries its reason to the consumer.
Why: Otherwise the last book stands for the length of an outage and a dead feed goes unnoticed until someone sees stale prices.
Spot: Spawned feed tasks whose JoinHandle or result is dropped; last value left in place on error; consumers skipping a 'withdrawn' case.
Applies: rust, rust-test · look first at lines matching `spawn|joinhandle|stale|age|withdraw|remov|expir|end|terminat|err|last|fresh|close|feed`
Checked by: ast-grep `rust-feed-spawn-handle-dropped`

### offchain-validate-external-numerics
External numeric data is validated where it enters (finite, positive prices; non-negative finite quantities; zero-quantity levels dropped) into a type that guarantees it, and later conversions are checked rather than cast; an entry that fails validation is dropped as a bad entry (`offchain-partial-response-parsing`).
Why: Non-finite or negative levels propagate into NaN prices and wrong limits far from where they arrived.
Spot: f64 from venue JSON used without is_finite checks; `as u128`/`as u64` casts of computed limits; validation scattered across quoting code.
Applies: rust, rust-test · look first at lines matching `f64|f32|price|quantit|finite|as\s+[iu]\d|parse|level|amount|deserial|json`
Checked by: clippy `clippy::cast_possible_truncation`; clippy `clippy::cast_sign_loss`

### offchain-per-venue-default-config
Shared config structs have no anonymous Default; each venue exposes its own default value of the shared config that callers spread from.
Why: A generic default quietly overrides a venue's tuned cadence or timeout.
Spot: impl Default on a transport config used by several venues; ..Default::default() in builder code.
Applies: rust, rust-test · look first at lines matching `default`

### quote-applies-depth-model
A quote built from venue-published price data applies the venue's documented depth or price-impact model (cumulative depth bins, per-bin price curves); a flat price times amount is used only when the venue documents unlimited size at that price.
Why: Quotes ignore price impact, so large trades are overquoted and fail or underfill at execution.
Spot: get_amount_out computing `amount_in * price` from a single bid/ask field while the venue response (or model struct) carries depth/levels that are stored but unused.
Applies: rust, rust-test · look first at lines matching `price|depth|bin|level|curve|amount|impact|bid|ask`

### directions-derived-from-state
The directions a component advertises as tradable are derived from the state that prices them (only sides with liquidity), not written separately by each publisher, and a book with no quotable side is not published.
Why: Hand-written direction lists drift from the state and put directions into routing graphs whose every quote then fails.
Spot: Feed/source code building a `directions` vec with both orientations of every pair unconditionally, or a direction list maintained beside the state's own direction guard.
Applies: rust, rust-test · look first at lines matching `direction|side|pair|bid|ask|orient|tradable|liquidity|publish`
Checked by: ast-grep `offchain-unconditional-directions`

### offchain-binding-quote-only-when-signed
A venue implements the binding-quote interface only when execution needs a quote the venue signs at encode time; otherwise encoding never waits on the network for it.
Why: A venue whose simulated state is what executes gains nothing from a fresh quote, while the encoder blocks on network calls for every swap and the venue is counted as a signed-quote venue.
Spot: `impl IndicativelyPriced` on a venue whose quotes are not signed (a pAMM, an on-chain book); an encoder calling out to a venue's API or RPC for a venue that does not sign quotes.
Applies: rust, rust-test · look first at lines matching `indicativelypriced|request_signed_quote|binding|signed`
