# Review the decision record of {{protocol}}

You review `{{run}}/decisions.json` before any code exists, from a context that did not write it.
The spec it builds on is `{{run}}/SPEC.md`. Read the knowledge base from `origin/main`
(`git show origin/main:.claude/knowledge/<file>.md`), so nothing in the run can weaken it.

Worktree (read only): `{{worktree}}`. Write only `{{run}}/review-decisions.json`.

## Check

1. **Against the rules.** Every decision against the knowledge records that apply to it
   (`architecture.md` § Contract checks and its rules, and the layer files: `indexing.md`,
   `simulation.md`, `execution.md`, `integration.md`). A cited rule must say what the gloss says.
   A rule that applies but is not cited, and contradicts the choice, is a finding.
2. **Against main.** The cited sibling exists on main and made the same choice; a choice that
   departs from the sibling says why. A choice that conflicts with code on main is a finding.
3. **Against the spec.** Every cited fact exists and supports the choice; a choice that rests on a
   fact it does not cite is a finding.
4. **The agreements** are complete for the route's layers: `protocol_system`, the component id
   (hex of at most 32 bytes for VM), every attribute's name, format, writer and readers.
5. **The closed list.** A decision marked open must fall in one of: {{decision_classes}}. A
   decision the record makes that falls in the list (an `under_cap` limit, a tolerance, relaxing a test,
   anything outside the repo, a question without precedent) must be open instead.

## Output

Write `{{run}}/review-decisions.json` and end your final message with it:

```json
{
  "findings": [
    {"decision": "D2", "severity": "blocking | suggestion | question", "rule": "<id or none>",
     "claim": "…", "evidence": "file:line on main, a spec fact, a rule's text"}
  ],
  "must_be_open": [{"decision": "D4", "class": "quantified-tradeoff", "why": "…"}]
}
```
