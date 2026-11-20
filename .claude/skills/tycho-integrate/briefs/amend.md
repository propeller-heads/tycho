# Amend the spec of {{protocol}}

You are the research agent of the tycho-integrate run for **{{protocol}}**. The spec at
`{{run}}/spec.json` needs changes. They come from one of: a person's corrections at Gate 1, facts
the falsifier refuted, or something learned later (a person, a contract upgrade, a fee change).

Run directory: `{{run}}` (write here only). Worktree (read only): `{{worktree}}`.

What changed, and why:

{{extra}}

## How

- Establish each change with evidence, as research does (`.claude/knowledge/verification.md`
  § Tycho and chain methods). A person's statement about the protocol team's intent or off-chain
  behaviour is recorded as such, with the person as its source; anything the chain can answer is
  checked on chain.
- A refuted fact: replace the claim with what holds, with the falsifier's evidence and yours. A
  fact you still cannot settle becomes `contested` and goes into `open_risks`.
- Before Gate 1 (corrections, refutations), edit the facts in place.
- After Gate 1, never edit a signed fact in place: append to `amendments`
  (`{"n": <next>, "fact_id": "S3", "old": "…", "new": "…", "evidence": "…", "source": "person | chain"}`)
  and update the fact's claim and evidence. Each amendment is one fact.
- An amendment of the spec's `kind_fact` changes `kind` (and `vm_reads`) with it: the kind picks
  the checks every layer runs.

End your final message with:

```json
{"changed": ["S3"], "amendments": [{"fact": "S3", "evidence": "…", "new": "…", "source": "chain"}], "still_contested": ["S…"]}
```
