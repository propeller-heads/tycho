Run the `tycho-review` skill (Skill tool) with exactly these arguments:

```
{{branch}} partial --spec {{run}}/SPEC.md --existing {{existing}}{{since}}
```

This review is one round of the tycho-integrate loop for **{{protocol}}**, {{layer}} layer,
round {{round}}. The existing-findings file carries the ledger's open findings, each under its
ledger id with the implementer's answer: report every one's status under that id in the sidecar's
`earlier` list. Write the JSON sidecar beside the report as the skill's Output section describes.
Change no code and post nothing.

Headless session notes (this session cannot ask for permission):

- The Write and Edit tools are refused under `.claude/` ("a sensitive file"), the review report and
  sidecar included. Write them with `python3` through Bash (`Path(...).write_text(...)`); the
  guard allows exactly `.claude/reviews/` and the run directory.
- There may be no Grep or Glob tool, in a subagent too: search with `rg` or `grep` and `find`
  through Bash. Tell every subagent you start both points in its brief.
- Run the deterministic checks (`run_checks.py`, step 12) as the skill says; "no Bash" is not a
  reason to report them as not run. If a command is denied, say which and why under the parts that
  did not run.

End your final message with the report's path on its own line.
