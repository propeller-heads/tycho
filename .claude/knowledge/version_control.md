# Version Control Tasks

## Workflow

### Before committing or pushing

1. Re-read your changes for unnecessary complexity and unclear naming
2. Format: `cargo +nightly fmt --all` (and `forge fmt` from `crates/tycho-execution/contracts/` if you touched Solidity)
3. Run only the tests that cover what you changed, e.g. `cargo nextest run -p tycho-indexer` or
   `forge test --match-test testSwapSingle`. Do not run the whole workspace.
4. Run the `sync-docs` skill if you changed behaviour that the docs describe.

Do **not** run the full CI pipeline (`run-ci` skill) on every push. Run it when you are about to
open or update a PR for review, or when a change is wide enough that scoped tests do not cover it
(workspace-wide refactors, `Cargo.toml`/`Cargo.lock`, DTO/RPC types, DB migrations, release prep).

### Commits

- Imperative mood, ≤72 char subject line, one logical change per commit
- Use `feat:` prefix for public interface changes (new endpoints, trait changes, wire type changes)
- Never commit secrets, API keys, or credentials
- Never push directly to main — use feature branches and PRs

### Pull requests

- Describe what the code does now — not discarded approaches or alternatives
- Use plain, factual language. Avoid: critical, crucial, essential, significant, comprehensive, robust, elegant
- Keep the description concise.