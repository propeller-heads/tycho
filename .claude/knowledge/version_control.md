# Version Control Tasks

## Workflow

### Before committing or pushing

1. Re-read your changes for unnecessary complexity and unclear naming
2. Format: `cargo +"$RUST_NIGHTLY" fmt --all` (and `forge fmt` from `crates/tycho-execution/contracts/` if you touched Solidity)
3. Run only the tests that cover what you changed, e.g. `cargo nextest run -p tycho-indexer` or
   `forge test --match-test testSwapSingle`. Do not run the whole workspace.
4. Run the `sync-docs` skill if you changed behaviour that the docs describe.

Do **not** run the full CI pipeline (`run-ci` skill) on every push. Run it when you are about to
open or update a PR for review, or when a change is wide enough that scoped tests do not cover it
(workspace-wide refactors, `Cargo.toml`/`Cargo.lock`, DTO/RPC types, DB migrations, release prep).

### Commits

- Imperative mood, ≤72 char subject line, one logical change per commit
- Use `feat:` prefix for public interface changes (new endpoints, trait changes, wire type changes)
- The squash-merged PR title drives the release, so its conventional type matches the change as
  well. A commit message names every removal of an interface item it makes, with the reason
- Never commit secrets, API keys, or credentials
- Never push directly to main — use feature branches and PRs
- Once a branch is under review, address feedback with new commits, not a rebase or force-push
- In a PR reviewed commit by commit, squash out a deletion, stub or rename that a later commit
  reverts; a test that cannot run yet stays `#[ignore]`d with its body intact

### Pull requests

- Describe what the code does now — not discarded approaches or alternatives
- Update factual claims in the description (data sources, vendored files, test counts) with each
  revision
- Use plain, factual language. Avoid: critical, crucial, essential, significant, comprehensive, robust, elegant
- Keep the description concise.

## Rules

Each rule is a record: its id, which review findings cite; the rule; why it matters; how to spot a
violation in a diff (Spot); the kinds of files it applies to; and the tools that check it, in part
or in full (`.claude/skills/tycho-review/SKILL.md` lists the file kinds). The Commits and Pull
requests sections above are the source of the records that point to them.

<!-- pieces -->

### pr-description-matches-code
Source: `.claude/knowledge/version_control.md` § Workflow › Pull requests
Spot: Description claims ('derived from X events', 'N unit tests', 'vendors A and B') that don't match the diff; descriptions not edited after later commits changed the approach; narrated rejected alternatives; 'critical', 'robust', 'comprehensive' and similar.
Applies: pr
Checked by: pr-script `description-intensifiers`

### copy-commit-before-adaptation
Code copied from another module (a fork of upstream logic, another package) and then adapted lands as a verbatim-copy commit before the commits that modify it.
Why: Without the copy commit the adaptation is buried in a large new-file diff and reviewers cannot see which lines differ from the proven original.
Spot: A PR adding a module that is largely a copy of an existing one, with no commit containing the unmodified copy.
Applies: pr, any
Checked by: pr-script `copy-commit-before-adaptation`

### pr-title-type-matches-change
Source: `.claude/knowledge/version_control.md` § Workflow › Commits
Spot: Public API, trait or wire changes under fix:/chore: titles; docs: titles carrying feature changes. A commit typed refactor/perf whose diff deletes a trait or public method its message does not mention.
Applies: pr
Checked by: pr-script `title-type-vs-pub-changes`

### breaking-changes-listed
A PR that breaks public API in a way consumers must act on (removed or renamed items, changed signatures, serialized forms, ids or wire formats) lists every breaking item with its signature, gives a before/after upgrade example, and calls out behaviour changes that compile unchanged, including implicit guarantees consumers of a stream or API rely on (one message per upstream unit, never an empty message); for those it checks each downstream consumer before rollout and coordinates their adjustments. It never asks for a breaking marker or a major bump on tycho's Rust crates: they are 0.x, a `feat` commit already releases the incompatible 0.(x+1), and a `breaking` commit would release 1.0.0. Additive changes (a new enum variant, a new item) need no listing.
Why: Downstream consumers otherwise discover breaks at upgrade time and miss silent behaviour changes.
Spot: PRs removing or renaming pub items, or changing serialized forms or ids, with no breaking-changes section. A decoder or feed change that introduces a new message kind or an empty/status-only update on an existing channel.
Applies: rust, python, script, solidity, substreams, pr
Checked by: pr-script `breaking-section-missing`

### pr-states-purpose-and-task
Every PR has a description of at least one sentence stating what it is for, and a fix PR states the original problem (symptom and cause) before the change. A link to a tracking task is not required, from internal or external contributors.
Why: Without a description the reviewer has to rebuild the PR's purpose from the diff.
Spot: An empty PR body. A PR titled fix: whose body describes only the new behaviour.
Applies: pr

### bug-reachable-before-fix
A bug-fix PR states whether the bug is reachable in a configuration that actually runs: a reachable bug comes with a reproduction (with working credentials for an external API); a latent bug says so and what would make it reachable, and is pinned by a test that fails without the fix. A fix to a component that is about to be replaced is confirmed as still relevant first.
Why: Fixes for unreachable bugs or soon-replaced components cost review time and can change correct behaviour.
Spot: A fix PR whose description cites only docs or a code reading, for a mode used rarely or a component under replacement.
Applies: pr
