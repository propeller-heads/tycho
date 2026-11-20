---
name: tycho-integrate
description: Take a DEX integration request (protocol, chains, one anchor) and deliver it as one reviewed, tested PR, running unattended between three gates. Researches the venue on chain into an evidence-backed spec, records the design decisions, implements indexing, simulation and execution in rounds that /tycho-review checks, runs the final gates and opens the PR on approval. Use when the user says "integrate <protocol>", "tycho-integrate", "automate this integration", or asks for the status of or to resume an integration run.
---

# tycho-integrate

```
/tycho-integrate <protocol>             start a run, or resume the run that exists
/tycho-integrate <protocol> --status    stage, rounds, spend, open items
/tycho-integrate <protocol> --recheck   proxies of an open PR: implementation slots vs the spec
/tycho-integrate <protocol> --pr-review <PR>   answer the person's blind review of the PR
```

This session **orchestrates** and decides nothing the scripts or a person decide. Every agent
(research, falsify, decide, each implement round, each review) runs as a headless `claude -p`
through `run_agent.py`, which caps it at the remaining budget and records its cost. The ledger
(`state.py`) is the single source of the run's state: resume, status and the stop rules replay it.
Rules come from `.claude/knowledge/`; this skill restates none of them.

In the commands below, `S` is `python3 -I .claude/skills/tycho-integrate/scripts`, `P` is the
protocol name as given, and `RUN` is `.claude/worktrees/integrations/<slug>/` (`state.py P init` prints it). It sits
under `.claude/worktrees/` because a headless agent may not write anywhere else under `.claude/`.
Run everything from the repo root. Never build in the person's checkout: code changes happen in the
run's worktree only.

## What a person decides

A closed list. Anything else, the agents and the scripts resolve.

1. Two rules contradict, or a rule contradicts a precedent on main (`rule-conflict`).
2. A design question with no precedent (`no-precedent`).
3. A trade-off with a quantified cost: an `under_cap` limit, an `execution_tolerance` or `tvl_only`
   (`quantified-tradeoff`).
4. A spec unknown the chain cannot answer: the team's intent, off-chain behaviour (`spec-unknown`).
5. Anything outside the repo: deployment, secrets, permissions, other teams' registries
   (`outside-repo`).
6. Relaxing a test: a skip flag, a wider tolerance, a removed assertion (`relax-test`).
7. A loop that stopped without converging (`not-converged`).

Plus the three gates: the spec (Gate 1), open decisions (Gate 2), push and PR (Gate 3). Ask with
`AskUserQuestion`, each question standing on its own: what was found, the options, their cost.
Every question to a person is a ledger entry: `S/state.py P ask --class <class> --question '…'`
opens it (escalations from `next` open their own) and prints its id `Q<n>`; record the answer with
`S/state.py P decide --id Q<n> --choice '<answer>'` (with `--grant-rounds` or `--grant-usd` when
the person grants them).

## Resume

On every invocation, after `--status`/`--recheck`/`--pr-review` dispatch: if `RUN/ledger.jsonl`
exists, `S/state.py P resume` names the stage to continue; pick up there. A run stopped by a
budget, a crash or a compaction loses nothing the ledger holds. Before each stage append
`{"t":"stage","stage":"<stage>","status":"started"}` and after it `"done"`
(`S/state.py P append '<json>'`).

## Stages

### 1. Environment (`envcheck`)

`S/envcheck.py --chains <chains>` (after intake, `--protocol P`). For each `MISS` line ask the
person for exactly that item (one question listing all of them), then rerun until it passes. Do
not start work with a check failing.

`debug_storageRangeAt` is not required. Its `info` line per chain says whether DCI is available for
the integration there (`dci-only-where-rpc-serves-storage-range`, `.claude/knowledge/indexing.md`
§ DCI): a node without it means no DCI on that chain, and the harness cannot seed
`initialized_accounts` through it. `--protocol P` writes the result to `RUN/env.json`; research,
the decision record and the implementer design for it, and Gate 1 shows it.

### 2. Intake (`intake`)

Ask until `RUN/intake.json` has the required fields: `protocol`, `chains`, and one anchor (a
contract, docs or a swap tx). Then one open question, "anything else you know?", with an optional
checklist: source repo and commit, known quirks, admin powers, a contact at the protocol, a similar
venue already integrated. No grilling: research verifies every value as a lead.

```json
{"protocol": "Foo Swap v2", "chains": ["ethereum"],
 "anchor": {"contracts": ["0x…"], "docs": "https://…", "swap_txs": ["0x…"]},
 "source": {"repo": "…", "commit": "…"}, "kind_hint": "…", "scope": {"markets": "all"},
 "offchain": {"credentials_env": []}, "known_quirks": [], "contacts": "", "notes": ""}
```

`S/state.py P init` first. Intake is never edited afterwards; anything learned later is a spec
amendment. Run `envcheck.py --protocol P` again now that the chains and credentials are known.

### 3. Worktree (`worktree`)

```bash
S/worktree.py --protocol P
```

It cuts `.claude/worktrees/integrate-<slug>` on branch `integrate/<slug>` from `origin/main`, with
its submodules, and links the git-ignored `.env` files from the person's checkout, which is never
touched. Record the stage in the ledger (`{"t":"stage","stage":"worktree","status":"done"}`).

**Classify.** Before research, sort the venue into one of four cases:

```bash
S/prompt.py --protocol P --brief classify --out RUN/prompts/classify.md
S/run_agent.py --protocol P --label classify --role researcher --agent tycho-researcher \
  --cwd .claude/worktrees/integrate-<slug> --prompt-file RUN/prompts/classify.md --out RUN/out/classify.md
```

Read `RUN/route.json` and merge it into `intake.json` as `route` (`kind`, `layers`, `sibling`).
This is the one write to intake after the person's answers, and it records the classification,
not a fact. `integrated`: stop and tell the person what exists. `partial` and `fork`: the run
builds only `route.layers`. `new`: all three.

### 4. Research (`research`)

```bash
S/prompt.py --protocol P --brief research --out RUN/prompts/research.md
S/run_agent.py --protocol P --label research --role researcher --agent tycho-researcher \
  --cwd <worktree> --prompt-file RUN/prompts/research.md --out RUN/out/research.md
S/spec_render.py --protocol P --check
```

A failing `--check` lists the facts to settle: send them back with the `amend` brief
(`--extra-file` naming each) once; still failing, the critical unknowns are decisions of class
`spec-unknown` for the person. `questions_for_person` in the agent's summary are too.

### 5. Falsify (`falsify`)

Same call with `--brief falsify --label falsify`. For each `refuted` fact in `RUN/falsify.json`,
one `amend` call (`--extra-file` with the falsifier's results) redoes it. Anything still
contested becomes `contested` with an open risk. Then `spec_render.py --check` again.

### 6. Gate 1: the spec (`gate1`)

Show the person `RUN/SPEC.md` (its path and a summary: route, contracts, how a swap works,
the critical facts, open risks, which harness gates are off, and per chain whether DCI is
available and which dependencies need it). Ask the simulation kind as a question of its own:
native, hybrid or vm as research chose it, the other kinds with their costs from the kind fact, or
not worth integrating (the run stops: `{"t":"stage","stage":"gate1","status":"stopped"}`). Then
ask: approve the spec, or correct it. A different kind or any correction goes to the `amend` brief
and the spec is rendered and shown again. On approval:

```bash
S/state.py P append '{"t":"gate","gate":1,"status":"approved","facts":[<every fact id>]}'
```

The listed facts are signed: an amendment to one of them goes back to the person.

### 7. Decision record (`decide`, `decide-review`)

```bash
S/prompt.py --protocol P --brief decide --out RUN/prompts/decide.md
S/run_agent.py --protocol P --label decide --role researcher --agent tycho-researcher --cwd <worktree> --prompt-file RUN/prompts/decide.md --out RUN/out/decide.md
```

Validate it (`python3 -I -c` with `schema.validate_decisions`, or read it against the brief's
shape) and fix gaps by sending them back. Then the review of the record, from a fresh context:

```bash
S/prompt.py --protocol P --brief review-decisions --out RUN/prompts/review-decisions.md
S/run_agent.py --protocol P --label review-decisions --role reviewer --cwd <worktree> --prompt-file RUN/prompts/review-decisions.md --out RUN/out/review-decisions.md
```

Findings in `RUN/review-decisions.json` go back to `decide` once (`--extra-file`); its
`must_be_open` decisions become open.

### 8. Gate 2: open decisions (`gate2`)

Only when `decisions.json` has open decisions. Ask the person each one (question, options, cost,
the rules and the sibling involved); write the answer into `decisions.json` (`choice`, `open:
false`, `"by": "person"`), and record it in the ledger (`state.py ask --class <its class> --record
D<n> --question '…'`, then `state.py decide`). No open decision: mark the stage done without
asking.

### 9. Build, per layer in order (`build:indexing`, `build:simulation`, `build:execution`)

Only the route's layers, in this order; each loop must stop `clean` before the next starts. One
round of layer `L`, number `N` (`N` = rounds so far + 1):

1. **Implement.**
   ```bash
   S/prompt.py --protocol P --brief implement --layer L --round N --out RUN/prompts/implement-L-N.md
   BASE=$(git -C <worktree> rev-parse HEAD)
   S/run_agent.py --protocol P --label implement:L:N --role implementer --agent tycho-implementer \
     --layer L --cwd <worktree> --prompt-file RUN/prompts/implement-L-N.md --out RUN/out/implement-L-N.md
   ```
   Exit 3 means no budget is left: go to step 6 (`next` escalates).
2. **Answers.** Take the JSON at the end of `RUN/out/implement-L-N.md`, save it as
   `RUN/rounds/L-N/answers.json`, and `S/state.py P answers --layer L --round N --file …`. Missing
   answers are reported and stay open; they are never dropped. `facts_learned` go to the spec as
   amendments (below).
3. **Guards.** `S/state.py P check-guards --worktree <worktree> --from $BASE --layer L --round N --record`.
4. **Check and record.** `S/layer_checks.py --protocol P --layer L --round N`, then
   `S/state.py P round --layer L --head $(git -C <worktree> rev-parse HEAD) --checks green|red
   --checks-file RUN/rounds/L-N/checks.json`. The protocol-testing step runs `range --report`;
   `checks.json` carries each failed check of that report (`state.components`, `balances`,
   `sim.ladder`, `exec.ladder`, `limits.*` …) per component and direction, and the implementer's
   next round sees them in its findings file.
5. **Review.**
   ```bash
   S/state.py P answers-file --layer L --out RUN/rounds/L-N/existing.md
   S/prompt.py --protocol P --brief review --layer L --round N \
     --set existing=RUN/rounds/L-N/existing.md --set since=" --since <previous round's report>" \
     --out RUN/prompts/review-L-N.md     # since= empty on the layer's first round
   S/run_agent.py --protocol P --label review:L:N --role reviewer --layer L --cwd . \
     --prompt-file RUN/prompts/review-L-N.md --out RUN/out/review-L-N.md
   ```
   The report's path is the last line of the output; the sidecar is beside it
   (`<report>.findings.json`). Copy both into `RUN/reviews/`.
   `S/state.py P ingest-review --layer L --round N --sidecar <sidecar>`.
6. **Next.** `S/state.py P next --layer L` returns:
   - `continue`: the next round;
   - `clean`: mark `build:L` done and start the next layer;
   - `closed`: a person ended this layer's loop (answered a round or dollar cap without granting more, or
     any escalation with `decide --close`): mark `build:L` done and start the next layer, as for `clean`; its open findings go
     in the PR and the `layers-clean` gate passes it, naming the decision;
   - `escalate`: ask the person the decision it opened (its `decision` id; `state.py P status`
     shows the question), with what the ledger shows: the open findings, the disputes and both
     sides' evidence, the spend. Record the answer with `decide`, then continue the loop. The
     answer resets progress, oscillation and dispute counting for the layer from that round on.

The rounds' review findings are the review's: never edit, merge or drop one. The implementer sees
the ledger's findings (the existing file), never review transcripts.

### 10. Final gates (`final`)

1. **Fresh full review**, from a context that has seen none of the loop:
   ```bash
   S/prompt.py --protocol P --brief final-review --out RUN/prompts/final-review.md
   S/run_agent.py --protocol P --label final-review --role reviewer --cwd . --prompt-file RUN/prompts/final-review.md --out RUN/out/final-review.md
   S/state.py P ingest-review --layer auto --round <next round of each layer> --sidecar <sidecar>
   ```
   (`--layer auto` assigns each finding a layer by its file, from `.claude/knowledge/paths.toml`.)
   A layer with a new `blocking` finding gets more rounds (step 9, with `next --extra-rounds` up to
   `budgets.toml` `final.extra_rounds`), then this stage runs again; past that, it escalates.
   Findings below blocking stay in the ledger and the PR lists them.
2. **Gates.** `S/final_gates.py --protocol P --round <attempt>`: every layer clean, all checks once
   more (with the mutation score once G10 lands), one test per mutation surface (G5), proxies
   unchanged since the spec (a beacon proxy's implementation is read through its beacon), the branch
   containing the base's tip (`base-current`: the base moves while the run builds; a failure means
   rebase, rerun the layer checks and these gates), sibling PRs that predict the same test executor address, the new
   executor's runtime bytecode fixture registered so protocol-testing executes through it (no
   `skip_execution` in the package), and the strict yaml (`harness: strict`, a `balance_lens`, every
   direction's limit declared). A failing gate: fix it with an
   implement round of its layer when it is code, an amendment when a proxy was upgraded (below),
   a person's decision otherwise. `off` gates are listed, not failed.

**Splitting the PR.** When a person decides to split the branch into stacked PRs, cut them from the
gated branch, never by hand: `S/split.py --protocol P --branch <b> --onto <ref> --paths …` for each
PR bottom up (the first onto the PR base, each next onto the one below), then `S/split.py --protocol
P --verify --top <top branch>`, which must report `identical`: the stack then is the tree the gates
and the final review checked.

### 11. Gate 3: push and PR (`gate3`, `pr`)

```bash
S/pr_body.py --protocol P --shadow
```

Show the person `RUN/PR.md` (read any `STALE:` line `pr_body.py` printed: the description names
something the branch no longer has, or the gates ran at an older commit), the final gates and
`RUN/PR-spend.md`, and ask for approval to push the
branch and open the PR. Only on approval:

```bash
git -C <worktree> push -u origin integrate/<slug>
gh pr create --head integrate/<slug> --base main --title "feat: integrate <protocol> as <protocol_system>" --body-file <PR.md plus this session's attribution lines>
```

Shadow mode is the default until the escape rate earns more (the person decides when to drop
`--shadow`): the decisions and the ledger are held back in `RUN/PR-held.md` until the person has
reviewed the PR blind. Then offer to schedule `--recheck` with `/loop` while the PR is open.

## Spec amendments

Anything learned after Gate 1 (a person, an implementer's `facts_learned`, a contract upgrade
found by the final gates or `--recheck`) becomes an amendment, never an intake edit:

1. `amend` brief with what changed (`--extra-file`); the researcher appends it to `amendments`
   with evidence. `spec_render.py --check`.
2. `S/state.py P amend --fact S<n> --evidence '…' --new '…' --source chain|person`. It names the
   decisions that cite the fact and the layers those decisions shaped.
3. `needs_person: true` (the fact was signed at Gate 1): ask the person before anything reruns.
4. Reopened decisions go back through `decide` for those decisions; each affected layer gets
   `{"t":"stage","stage":"build:L","status":"started"}` and more rounds, in layer order.

## `--status`

`S/state.py P status`: stage to resume at, gates, spend per stage and total against the cap, per
layer rounds, spend, open findings by severity, open counts after each review and stop reason,
open decisions, amendments, escapes.

## `--recheck`

`S/final_gates.py --protocol P --recheck`. An upgraded implementation is a spec amendment (above)
with the affected rounds rerun; tell the person, since the PR is open.

## `--pr-review <PR>`

The person's blind review of a shadow-mode PR. Fetch its comments
(`gh api --paginate repos/{owner}/{repo}/pulls/<PR>/comments` and the reviews), and for each
comment:

1. Append an `escape` event: `{"t":"escape","comment":"<url>","severity":"blocking|suggestion|nit",
   "cause":"<one of schema.ESCAPE_CAUSES>"}`. Propose severity and cause, and confirm them with the
   person in one question for all comments. Each escape is fixed at its cause: a knowledge record
   for `missing-knowledge`/`no-rule`/`rule-judged-ok`, a test for `test-gap`; propose those
   changes to the person as follow-ups, never inside this PR.
2. Turn the comments into a sidecar (`{"findings": [{"rule": "human-review", "file", "lines",
   "claim", "severity"}]}`) and `ingest-review --layer auto`; the affected layers run rounds as in
   step 9. Ask before every push. Then post `RUN/PR-held.md` as a PR comment, with approval.

## Budgets

`budgets.toml`: per layer rounds and dollars, per integration dollars, extra final rounds. The
caps are placeholders until shadow runs measure spend (`--status` shows it per stage).

## Files

| Path | What |
|---|---|
| `briefs/*.md` | each agent's job; rendered into its prompt by `prompt.py` |
| `scripts/schema.py` | the shapes of intake, spec, decisions and ledger events; paths; guards |
| `scripts/state.py` | ledger, stop rules, review ingestion, guards, amendments, status, resume |
| `scripts/run_agent.py` | the one headless `claude -p` wrapper: budget cap, cost into the ledger |
| `scripts/guard.py` | `PreToolUse` hook of the run's agents (`settings/<role>.json`) |
| `scripts/envcheck.py` | the environment check |
| `scripts/harness_caps.py` | which hardening gaps (G1–G13) the harness supports |
| `scripts/layer_checks.py`, `checks.toml` | each layer's checks, run in the worktree |
| `scripts/spec_render.py` | `spec.json` → `SPEC.md`, with `--check` |
| `scripts/final_gates.py` | final gates, `--recheck` |
| `scripts/worktree.py` | the run's worktree, on `origin/main` plus the strict test suite while it is unmerged |
| `scripts/pr_body.py` | the PR description (`--shadow` holds the ledger back) |
| `scripts/test_state.py` | tests of the ledger, stop rules, guards and spec validation |
| `.claude/agents/tycho-researcher.md`, `tycho-implementer.md` | the agents' definitions |
| `RUN/` (gitignored) | `intake.json`, `spec.json`, `SPEC.md`, `decisions.json`, `ledger.jsonl`, `rounds/`, `reviews/`, `prompts/`, `out/`, `final_gates.json`, `PR.md` |
