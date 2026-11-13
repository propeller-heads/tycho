# Contributing guidelines

## Local Development

### Tools

Install <a href="https://rustup.rs/" target="_blank" rel="noopener noreferrer">rustup</a> and <a href="https://mise.jdx.dev/getting-started.html" target="_blank" rel="noopener noreferrer">mise</a>, <a href="https://mise.jdx.dev/getting-started.html#activate-mise" target="_blank" rel="noopener noreferrer">activate it</a> in your shell, and run `mise install` in the repository. mise provides the tools and environment variables CI uses, pinned in `mise.toml`. rustup installs the Rust toolchains that the `rust-toolchain.toml` files pin. The commands on this page assume an activated mise.

### Changing Rust Code

Please make sure that the following commands pass if you have changed the code:

```sh
cargo check --all
cargo test --all --all-features
cargo +"$RUST_NIGHTLY" fmt -- --check
cargo +"$RUST_NIGHTLY" clippy --workspace --all-features --all-targets -- -D warnings
```

Builds and tests use the stable toolchain from `rust-toolchain.toml`. Formatting and linting use the nightly toolchain in `RUST_NIGHTLY`, because `rustfmt.toml` relies on unstable `rustfmt` options.

If you are working in VSCode, we recommend you install the <a href="https://rust-analyzer.github.io/" target="_blank" rel="noopener noreferrer">rust-analyzer</a> extension, and use the following VSCode user settings:

```json
"editor.formatOnSave": true,
"rust-analyzer.rustfmt.extraArgs": ["+nightly"],
"rust-analyzer.check.overrideCommand": [
"cargo",
"+nightly",
"clippy",
"--workspace",
"--all-features",
"--all-targets",
"--message-format=json"
],
"[rust]": {
"editor.defaultFormatter": "rust-lang.rust-analyzer"
}
```

### Changing Solidity code

#### Executors <a href="#running-tests" id="running-tests"></a>

For security purposes, new Executors must have:

* No ERC20 token transfers
* No `delegatecall`s
* Only perform native ETH transfers if this behaviour is safely reflected in `getTransferData` or `getCallbackTransferData`
* **Selector allowlist for caller-controlled calldata**: If `swap()` forwards caller-supplied calldata to an external contract, validate the first 4 bytes against an explicit allowlist of permitted function selectors before making the call. Without this check, an attacker can supply a selector for an arbitrary function on the settlement contract — for example, one that withdraws the router's balance to an attacker-controlled address.

#### Running tests <a href="#running-tests" id="running-tests"></a>

Run from `crates/tycho-execution/contracts/`:

```bash
export ETH_RPC_URL=<url>
forge test
```

#### Code formatting <a href="#code-formatting" id="code-formatting"></a>

Run from `crates/tycho-execution/contracts/`:

```bash
forge fmt
```

#### Assembly

Please **minimize** use of assembly for security reasons.

#### Contract Analysis

We use <a href="https://github.com/crytic/slither" target="_blank" rel="noopener noreferrer">Slither</a> to detect any potential vulnerabilities in our contracts.

To run it locally, run it inside the contracts directory:

```bash
cd crates/tycho-execution/contracts
slither .
```

## Creating a Pull Request

We use <a href="https://www.conventionalcommits.org/en/v1.0.0/" target="_blank" rel="noopener noreferrer">conventional commits</a> as our convention for formatting commit messages and PR titles.
