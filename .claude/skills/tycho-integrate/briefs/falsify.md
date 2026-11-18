# Try to break the spec of {{protocol}}

You are a fresh research agent of the tycho-integrate run for **{{protocol}}**. You have not seen
how the spec was written. Your job is to break it: for every claimed invariant and every critical
fact in `{{run}}/spec.json` (rendered as `{{run}}/SPEC.md`), look for the block or the state where
it does not hold.

Run directory: `{{run}}` (write here only). Worktree (read only): `{{worktree}}`.

## How

- **Another block.** A fact established at one block: read it again at other blocks that matter
  (before and after each mutation surface fired, the test range's start and stop, the latest
  block), with `cast` and the block pinned (`state-reads-at-exact-blocks` in
  `.claude/knowledge/verification.md`).
- **Changed state.** On an anvil fork, change what the fact depends on (`state-override-experiments`):
  fire the admin setter, move liquidity, pause, swap near the limit, swap in the other direction.
- **Claims from other implementations** (the spec's `templates`, the protocol's docs or API) are
  exactly the claims to test against the chain.
- **Limits**: the largest input that settles and one raw unit more (`amount-bisection-ground-truth`).
- **Golden vectors**: replay each against the venue's quote function at the block before the swap;
  a mismatch is a refutation of the quote function's fact or of the vector.

Do not edit `spec.json`. Spend your effort on the critical facts and the invariants first.

## Output

Write `{{run}}/falsify.json` and end your final message with the same JSON:

```json
{
  "results": [
    {"fact": "S3", "result": "held | refuted | contested",
     "evidence": [{"kind": "command | tx | block | trace", "ref": "…", "note": "what it shows"}],
     "note": "for refuted: what holds instead; for contested: why neither side is settled"}
  ],
  "new_risks": ["…"]
}
```

`held` needs evidence from a block or state other than the spec's own.
