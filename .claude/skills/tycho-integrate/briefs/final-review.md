Run the `tycho-review` skill (Skill tool) with exactly these arguments:

```
{{branch}} full --spec {{run}}/SPEC.md
```

This is the final review of the tycho-integrate run for **{{protocol}}**: a full review of the
whole branch from a context that has seen none of the loop. Write the JSON sidecar beside the
report as the skill's Output section describes. Compare the router test's measured gas with the
spec's `gas.swap_gas_used` and the simulation's gas estimate: an estimate more than 25% off the
measurement is a finding (rule id `tycho-integrate`, severity suggestion). Change no code and post
nothing.

Headless session notes (this session cannot ask for permission):

- The Write and Edit tools are refused under `.claude/reviews/` ("a sensitive file"), the review
  report and sidecar included. Write those with `python3` through Bash (`Path(...).write_text(...)`);
  the guard allows exactly `.claude/reviews/` and the run directory, which Write reaches directly.
- There may be no Grep or Glob tool, in a subagent too: search with `rg` or `grep` and `find`
  through Bash. Tell every subagent you start both points in its brief.
- Run the deterministic checks (`run_checks.py`, step 12) as the skill says; "no Bash" is not a
  reason to report them as not run. If a command is denied, say which and why under the parts that
  did not run.

End your final message with the report's path on its own line.
