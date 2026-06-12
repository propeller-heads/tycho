# Contributing guidelines

## Local Development

### Changing Rust Code

Please make sure that the following commands pass if you have changed the code:

```sh
cargo check --all
cargo test --all --all-features
cargo +nightly fmt -- --check
cargo +nightly clippy --workspace --all-features --all-targets -- -D warnings
```

We are using the stable toolchain for building and testing, but the nightly toolchain for formatting and linting, as it allows us to use the latest features of `rustfmt` and `clippy`.

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

#### Setup <a href="#setup" id="setup"></a>

Install foudryup and foundry

```bash
curl -L https://foundry.paradigm.xyz | bash
foundryup
```

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

To run locally, simply install Slither in your conda env and run it inside the contracts directory.

```bash
conda create --name tycho-execution python=3.10
conda activate tycho-execution

pip install slither-analyzer
cd crates/tycho-execution/contracts
slither .
```

## Creating a Pull Request

We use <a href="https://www.conventionalcommits.org/en/v1.0.0/" target="_blank" rel="noopener noreferrer">conventional commits</a> as our convention for formatting commit messages and PR titles.
