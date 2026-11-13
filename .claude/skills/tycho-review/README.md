# tycho-review

For the people who run and maintain the skill. Claude reads `SKILL.md`; this file is not loaded
into a review.

## Reviewing a change from outside the team

A change can alter the reviewer itself: its `.claude/skills/`, its `CLAUDE.md` files and the
knowledge files its rules live in, all of which Claude Code loads once a session works on files
there. The skill guards against that for a PR from a fork (base-branch copy of the skill, git
objects only, nothing built, see "Trust" in `SKILL.md`). Three controls are outside its reach and
rest with whoever starts the session:

- **Start in a checkout of the base branch.** A session started inside the change's worktree has
  loaded that worktree's skill and `CLAUDE.md` files before the run begins.
- **Review it through its PR.** Only a PR target is detected as a fork. A fork's branch checked out
  locally and reviewed as a branch, a range or the working changes is reviewed as trusted: built,
  run and with its own copy of the skill.
- **Use the default permission mode,** where every command outside the allowlist needs approval. In
  auto or bypass mode, the rule that nothing from the change is built rests on the model's
  instructions alone.

A trusted change that edits the skill or its knowledge and is reviewed from its own head is reviewed
by its own edited copy: its author is trusted, and the review exercises the edit.

## Measuring a change to the skill

A review run varies from one run to the next, so a change to `SKILL.md`, the planning script or the
knowledge is judged on runs, not on reading it.

- **That it still works,** after any change to the run: `/tycho-review` on one target of each kind,
  each of which must end with a report. A PR at `full`: findings in the report's format, a coverage
  table with no file missing, at least one execution verifier result with real evidence, and the
  run's worktree removed. A range, a path (one module), and the working changes, which must leave
  the checkout unchanged (same diff and status). A PR from a fork, which must be detected as
  untrusted and never built.
- **What it finds,** after a change meant to find more: blind runs. Take PRs whose review comments
  the knowledge was not distilled from, write down the reviewers' points before reading any report,
  and review each PR as a range at the commit its review was made on, with GitHub lookups
  disallowed, so the run sees none of the review. Score each point found, partly found or missed,
  and triage the findings no reviewer raised: a correct finding nobody made counts for the skill,
  not against it. For an open PR, a reviewer reviews it first and keeps the review unposted. Runs
  on the PRs a change was tuned on show whether the change fires, not the skill's recall.

## Why it is built this way

- **One skill for every layer.** The layers are decoupled as code but share a contract: simulation
  consumes what a Substreams package emits (attribute names and byte formats, and for VM protocols
  which contracts and slots are tracked), and execution consumes `ProtocolComponent` and optionally
  the state. The costliest bugs sit on that contract, such as a slot the package doesn't watch so
  the simulation goes stale, and a reviewer of one layer cannot see them.
- **A pass is one chunk of the diff against every record that applies to it.** The limit is the
  size of the diff, not the number of checks. On a 2,400-line chunk, an agent given 40, 80, 160 or
  all 320 applicable records returned a verdict for every one, showed no position effect, and found
  more real problems the more records it had, at about 1.8 times the tokens of the smallest list; a
  held-out PR whose review shaped no record gave the same result. Chunks therefore hold at most
  about 6,000 diff lines, cut by layer and directory, and every pass records a verdict per record,
  so a skipped check shows as missing.
- **Records are routed by file kind, not filtered by keyword.** Keyword filters cut only 4–9% of the
  records on large changes and missed violations the diff text does not show, so a record's
  keywords are a hint of where to look first.
- **Knowledge comes from the base,** so a change cannot weaken its own review by editing the
  knowledge or the path table.
- **No summary step.** Every finding reaches the report as its own entry; consolidation merges only
  findings that make the same claim. Anthropic's
  [pr-review-toolkit](https://github.com/anthropics/claude-code/tree/main/plugins/pr-review-toolkit),
  run on two of the team's reviewed PRs, dropped findings its subagents had made when it summarised
  them.
- **Implementation sessions load little.** A session loads the knowledge of the layer it touches
  and the architecture; the general and language rules reach its code through the lints and a
  review of its own diff. Anthropic's
  [Claude Code best practices](https://code.claude.com/docs/en/best-practices) keep `CLAUDE.md` to
  what applies broadly; HumanLayer's
  [Writing a good CLAUDE.md](https://www.humanlayer.dev/blog/writing-a-good-claude-md) leaves style
  to linters, reading [arXiv 2507.11538](https://arxiv.org/abs/2507.11538) as frontier models
  following about 150–200 instructions with reasonable consistency; and
  [Evaluating AGENTS.md](https://arxiv.org/abs/2602.11988) (ICLR 2026) found repository context
  files raise inference cost by over 20% without generally improving task success.

The run borrows from four review tools:
- Claude Code's [Code Review](https://code.claude.com/docs/en/code-review): for a change that can't
  be trusted, the review instructions come from the base branch, never from the change;
- Test Double's Han and its
  [code-review skill](https://github.com/testdouble/han/blob/main/han-coding/docs/skills/code-review.md):
  the change's intent as untrusted data, pre-existing findings in a list of their own, the
  contradiction check, the failure scenario behind a finding, and a refutation that needs evidence
  of its own;
- Matt Pocock's
  [code-review skill](https://github.com/mattpocock/skills/blob/main/skills/engineering/code-review/SKILL.md):
  the spec pass and its three questions;
- Alibaba's [open-code-review](https://github.com/alibaba/open-code-review): findings anchored by the
  code they quote, and every changed file accounted for.
