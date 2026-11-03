# Libraries: general Rust crates

Which crate to use for a need, so a change does not hand-roll what a dependency already does. One
section per crate; each row gives the need, what to use (crate and path), the hand-rolled pattern
it replaces and how to spot that pattern in a diff. No table is complete: before accepting a new
helper, type or loop, search the workspace and its direct dependencies for an existing equivalent.

<!-- pieces: rendered from candidates.json by candidates/knowledge.py; edit the pieces there, not below -->

## Rules

### u256-checked-arithmetic
alloy/ruint U256 arithmetic operators (+, -, *) wrap silently; arithmetic on untrusted or unbounded values uses checked operations.
Why: A wrapped multiplication yields a silently wrong result instead of an error (e.g. when a caller passes U256::MAX as an upper bound).
Spot: Raw `*`/`+` on U256 in amount or limit computations, especially with values like U256::MAX or caller-supplied amounts.
Applies: rust, rust-test, substreams · look first at lines matching `u256|u128|uint|\+|-|\*|max|amount|wrapping|checked`

## alloy

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| alloy-typed-primitives | Parse, hold and write addresses, 32-byte words and hashes with length checking, including compile-time constants. | alloy_primitives::{Address, B256, b256!, keccak256}; Address::from_word, B256::with_last_byte, Address::repeat_byte (const fn) — serde_qs Option\<Address> parsing already in protocols/substreams/ethereum-ekubo-v3/src/params.rs; dependency in protocols/substreams/ethereum-ekubo-v3/Cargo.toml and ethereum-sky/Cargo.toml (alloy-primitives 1.4.1, feature serde). Note Address Display is EIP-55 checksummed; use format!("{addr:#x}") for lowercase keys; compile-time constants via alloy::primitives::{address!, b256!, hex!} typed as Address/B256 (crates/tycho-execution/src/encoding/evm/constants.rs:41). Only in crates that already depend on alloy-primitives; others use the byte and hash types their lower-level dependency provides (605, 606). | Vec\<u8> + hex::serde + ensure!(len == 20), slot(n) building a fresh 32-byte Vec, address() padding any-length words, keccak-hash for registry slots; `[u8; 20]` byte-array literals, `Bytes::from_str("0x...").unwrap()`, `hex::decode(...)` of fixed constants. | `Vec<u8>` addresses with `hex::serde` and `ensure!(len == 20)`; helpers like `slot(n)`/`address(word)` building or padding byte vectors; a separate keccak crate for mapping slots. `const X: [u8; 20] = [0xae, ...]`; `Bytes::from_str("0x...").unwrap()` or `hex::decode` for fixed constants outside tests. | rust, rust-test, substreams |
| alloy-sol-macro | Encoding or decoding contract calls, events or packed data | alloy::sol! with SolCall::abi_encode / SolValue::abi_encode_packed (crates/tycho-ethereum/src/erc20.rs, crates/tycho-execution/src/encoding/evm/approvals/permit2.rs) | hand-written 4-byte selectors, manual 32-byte padding and Vec concatenation | Manual selector bytes, manual left-padding to 32 bytes, concatenating encoded fields by hand. | rust, rust-test, substreams |

## enum_delegate

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| enum-delegate | implement a trait on an enum by forwarding to each variant | enum_delegate::implement (crates/tycho-simulation/Cargo.toml enum_delegate 0.2.0; used in crates/tycho-simulation/src/evm/protocol/ekubo_v3/state.rs:50) | hand-written match self { ... } forwarding in each trait method | `impl Trait for Enum { fn m(&self) { match self { A(p) => p.m(), B(p) => p.m(), ... } } }`. | rust, rust-test, substreams |

## reqwest

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| shared-http-client | one reqwest client reused across components and requests | a `std::sync::LazyLock<reqwest::Client>` static or a client passed in at construction (crates/tycho-simulation/src/rfq/protocols/metric/client.rs:39, native/client.rs:39) | `Client::new()` per request or per component | `Client::new()` / `Client::builder().build()` inside a per-request function or a per-component constructor; a `OnceLock<Client>` field on each instance. | rust, rust-test, substreams |

## rstest

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| rstest-tables-named-cases | table-driven cases over one test body | rstest `#[rstest]` + `#[case::name(..)]` (dev-dependency across the workspace crates; e.g. crates/tycho-simulation/src/evm/protocol/rocketpool/decoder.rs:163) | separate hand-written tests per case, or unnamed `#[case(..)]` | Several near-identical `#[test]` fns; rejection branches without a test; `#[case(...)]` without a `::name`. | rust-test, substreams |

## ruint

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| ruint-div-ceil | ceiling division of U256 values | ruint::Uint::div_ceil (ruint 1.20.0, src/div.rs:67; via alloy U256) | (numerator + divisor - U256::ONE) / divisor | `numerator + d - U256::ONE` followed by `/ d`. | rust, rust-test, substreams |
| ruint-try-from-be-slice | decode a big-endian value of known width without panicking | ruint::Uint::\<BITS, LIMBS>::try_from_be_slice (ruint 1.20.0, src/bytes.rs:239), e.g. alloy_primitives::aliases::{U32, U128} | U256::from_be_slice(bytes).to::\<u32>() | `U256::from_be_slice(bytes).to::<u32>()` on decoded wire values. | rust, rust-test, substreams |

## serde

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| serde-rename-all | map snake_case Rust fields to a camelCase API | serde #[serde(rename_all = "camelCase")] | #[serde(rename = "toWrap")] on every field | Many `#[serde(rename = "camelCase")]` attributes on one struct. | rust, rust-test, substreams |

## strum

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| strum-enum-strings | Converting an enum to and from its string name | strum derives (EnumString, IntoStaticStr, EnumIter, Display) with #[strum(serialize_all = ...)] (crates/tycho-execution/src/encoding/evm/swap_encoder/fallback.rs:35) | hand-written match tables of variant names | fn as_str/from_str with a match listing every variant's string. | rust, rust-test, substreams |

## thiserror

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| thiserror | A new error type | thiserror::Error derive (workspace dependency; crates/tycho-common/src/simulation/errors.rs) | manual Display and std::error::Error impls | impl std::fmt::Display + impl std::error::Error written by hand for new error enums. | rust, rust-test, substreams |

## tokio

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| tokio-watch-latest-value | Publishing the newest complete snapshot to readers | tokio::sync::watch::channel (crates/tycho-simulation/src/evm/stream.rs:630) | bounded mpsc queues of full snapshots | mpsc channels carrying full snapshots where only the newest matters. | rust, rust-test, substreams |
| tokio-notify-enable-before-check | Block until a condition another task signals with `Notify::notify_waiters`. | `let n = notify.notified(); tokio::pin!(n); n.as_mut().enable(); if !cond() { n.await }` | `if !cond() { notify.notified().await }` | `if !cond() { notify.notified().await }`, or a `notified()` future created after the condition check. | rust, rust-test, substreams |

## tokio-retry

| Id | Need | Use | Instead of | Spot | Applies |
|---|---|---|---|---|---|
| tokio-retry-backoff | Exponential backoff for retries or restarts | tokio_retry::strategy::ExponentialBackoff with max_delay | hand-rolled `base * 2u64.pow(n)` with manual caps | Manual exponent arithmetic and min() caps for a backoff in a crate whose Cargo.toml has tokio-retry. | rust, rust-test, substreams, cargo |
