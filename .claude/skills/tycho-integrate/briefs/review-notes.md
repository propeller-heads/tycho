In the execution layer, compare the router test's measured gas with the spec's
`gas.swap_gas_used` and the simulation's gas estimate: an estimate more than 25% off the
measurement is a finding (rule id `tycho-integrate`, severity suggestion). Change no code and post
nothing.

Headless session notes (this session cannot ask for permission):

- The Write and Edit tools are refused under `.claude/reviews/` ("a sensitive file"), the review
  report and sidecar included. Write those with `python3` through Bash (`Path(...).write_text(...)`);
  the guard allows exactly `.claude/reviews/` (and the temp dir).
- There may be no Grep or Glob tool, in a subagent too: search with `rg` or `grep` and `find`
  through Bash. Tell every subagent you start both points in its brief.
- Run the deterministic checks (`run_checks.py`, step 12) as the skill says; "no Bash" is not a
  reason to report them as not run. If a command is denied, say which and why under the parts that
  did not run.

Notes from the orchestrator (a person's decisions to check the code against, and anything else):

{{extra}}

End your final message with the report's path on its own line.
