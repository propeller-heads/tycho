#!/usr/bin/env python3
"""spec.json -> SPEC.md: the document a person signs at Gate 1 and `/tycho-review --spec` reads.

    spec_render.py --protocol P [--check]

`--check` validates the spec first and exits 2 on any error, naming each: a critical fact without
evidence stops the run. SPEC.md is generated, never edited, so the person and the pipeline read
the same facts.
"""

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402

EXPLORERS = {
    "ethereum": "https://etherscan.io",
    "base": "https://basescan.org",
    "arbitrum": "https://arbiscan.io",
    "unichain": "https://uniscan.xyz",
    "bsc": "https://bscscan.com",
    "polygon": "https://polygonscan.com",
}

SECTION_TITLES = {
    "summary": "1. Summary",
    "contracts": "2. Contracts",
    "swap": "3. How a swap works",
    "state": "4. What a quote depends on",
    "components": "5. Components",
    "balances": "6. Balances",
    "quotes": "7. How quotes are checked",
    "differences": "8. Differences, other implementations, risks",
}


def link(chain: str, kind: str, ref: str) -> str:
    base = EXPLORERS.get(chain)
    if kind == "tx" and base:
        return f"[tx {ref[:10]}…]({base}/tx/{ref})"
    if kind == "block" and base:
        return f"[block {ref}]({base}/block/{ref})"
    if kind == "address" and base:
        return f"[{ref}]({base}/address/{ref})"
    if kind == "command":
        return f"`{ref}`"
    return ref


def evidence_text(fact: dict, chain: str) -> str:
    evs = fact.get("evidence") or []
    if not evs:
        return ""
    parts = []
    for ev in evs:
        text = link(ev.get("chain", chain), ev["kind"], ev["ref"])
        if ev.get("note"):
            text += f" ({ev['note']})"
        parts.append(text)
    return " — evidence: " + "; ".join(parts)


def fact_line(fact: dict, chain: str) -> str:
    status = fact.get("status", "verified")
    prefix = {"unknown": "**Unknown:** ", "contested": "**Contested:** "}.get(status, "")
    crit = " *(critical)*" if fact.get("critical") else ""
    return f"- **{fact['id']}**{crit} {prefix}{fact['claim']}{evidence_text(fact, chain)}"


def table(headers: list, rows: list) -> list:
    if not rows:
        return ["*None.*", ""]
    out = ["| " + " | ".join(headers) + " |", "|" + "---|" * len(headers)]
    for row in rows:
        out.append("| " + " | ".join(str(c).replace("|", "\\|").replace("\n", " ") for c in row) + " |")
    out.append("")
    return out


def render(spec: dict, intake: dict, env: dict = None) -> str:
    chain = (spec.get("chains") or ["ethereum"])[0]
    facts = spec.get("facts", [])
    by_section = {s: [f for f in facts if f.get("section") == s] for s in schema.SPEC_SECTIONS}
    route = intake.get("route") or {}
    cov, gas = spec.get("coverage") or {}, spec.get("gas") or {}
    share = f"{100 * cov['served'] / cov['total']:.1f}%" if cov.get("total") else "unknown"
    out = [
        f"# {spec['protocol']}: integration spec",
        "",
        f"Kind: **{spec.get('kind')}** ({spec.get('kind_fact', 'no kind fact')}) · chains: {', '.join(spec.get('chains', []))} · route: **{route.get('kind', 'new')}**"
        + (f" (sibling `{route.get('sibling')}`, layers {', '.join(route.get('layers', []))})" if route.get("sibling") else ""),
        "",
        *([f"Hybrid: the quote reads through the VM: {'; '.join(spec['vm_reads'])}.", ""] if spec.get("vm_reads") else []),
        f"Coverage: **{share}** of the venue's {cov.get('unit', '?')} ({cov.get('served')} of {cov.get('total')}, {cov.get('source', 'no source')})"
        f" · gas of one swap on the venue: **{gas.get('swap_gas_used', 'unknown')}** ({gas.get('evidence', 'no evidence')})"
        + (" · **caller-dependent pricing**: " + ", ".join(q["id"] for q in spec.get("venue_quotes", []) if q.get("caller_fact")) if any(q.get("caller_fact") for q in spec.get("venue_quotes", [])) else ""),
        "",
        "Every fact links its evidence: a transaction, a block or a command anyone can rerun. An "
        "unknown is written as unknown. Generated from `spec.json`; do not edit.",
        "",
    ]

    def section(key: str) -> None:
        out.append(f"## {SECTION_TITLES[key]}")
        out.append("")
        out.extend(fact_line(f, chain) for f in by_section[key])
        if by_section[key]:
            out.append("")

    section("summary")
    if spec.get("scope"):
        out += [f"Scope: {spec['scope']}", ""]

    section("contracts")
    out += table(
        ["chain", "address", "role", "proxy → implementation", "verified source", "deployed at"],
        [
            [
                c.get("chain", chain),
                link(c.get("chain", chain), "address", c.get("address", "")),
                c.get("role", ""),
                f"yes → {c.get('implementation')}" if c.get("proxy") else "no",
                c.get("verified_source", "unknown"),
                c.get("deployment_block", "unknown"),
            ]
            for c in spec.get("contracts", [])
        ],
    )

    section("swap")

    section("state")
    out.append("### Mutation surfaces")
    out.append("")
    out += table(
        ["id", "surface", "who can trigger it", "signal (event or slot)", "fired at blocks", "facts"],
        [
            [m["id"], m["name"], m.get("who", "unknown"), m.get("signal", "unknown"), ", ".join(str(b) for b in m.get("fired_at_blocks", [])) or "never (fork test)", ", ".join(m.get("fact_ids", []))]
            for m in spec.get("mutation_surfaces", [])
        ],
    )

    section("components")
    if spec.get("directions"):
        out.append("### Components that do not trade both ways")
        out.append("")
        out += table(["component", "trades only", "reason"], [[d.get("component", ""), " → ".join(d.get("only", [])), d.get("reason", "")] for d in spec["directions"]])

    if spec.get("limits"):
        out.append("### Limits per direction")
        out.append("")
        out += table(
            ["component", "direction", "kind", "why", "facts"],
            [[l.get("component", ""), l.get("direction", ""), l.get("kind", "") + (f" (rounding {l['rounding']})" if l.get("rounding") else ""), l.get("why", ""), ", ".join(l.get("fact_ids", []))] for l in spec["limits"]],
        )

    out.append("### Dependencies and DCI")
    out.append("")
    dci = (env or {}).get("chains", {})
    out += [f"- {c}: " + ("DCI available" if dci.get(c, {}).get("dci") else f"**no DCI** ({dci.get(c, {}).get('detail', 'not probed')})") for c in spec.get("chains", [])]
    out.append("")
    if spec.get("dependencies"):
        out += table(
            ["component kind", "contract", "role", "address in", "re-pointable", "values", "facts"],
            [[d.get("component_kind", ""), d.get("contract", ""), d.get("role", ""), d.get("address_in", ""), "yes" if d.get("repointable") else "no", d.get("values", ""), ", ".join(d.get("fact_ids", []))] for d in spec["dependencies"]],
        )
    else:
        out += ["No dependencies outside the package's own contracts.", ""]

    section("balances")
    out += table(
        ["component kind", "balance source", "balance lens", "simulation reads it"],
        [[b.get("component_kind", ""), b.get("source", ""), b.get("lens", "") or "**none**", "yes" if b.get("simulation_reads") else "no (TVL only)"] for b in spec.get("balance_source", [])],
    )

    section("quotes")
    qf = spec.get("quote_function") or {}
    if qf:
        out += [
            f"Venue quote function: `{qf.get('signature', '?')}` at {link(qf.get('chain', chain), 'address', qf.get('address', ''))}"
            + (f", checked against {link(qf.get('chain', chain), 'tx', qf['checked_against_tx'])}" if qf.get("checked_against_tx") else ""),
            "",
        ]
    else:
        out += ["Venue quote function: **none recorded**.", ""]
    out.append("### Golden vectors")
    out.append("")
    out += table(
        ["tx", "component", "in", "out", "amount in", "amount out", "note"],
        [[link(chain, "tx", g.get("tx", "")), g.get("component", ""), g.get("token_in", ""), g.get("token_out", ""), g.get("amount_in", ""), g.get("amount_out", ""), g.get("note", "")] for g in spec.get("golden_vectors", [])],
    )
    tr = spec.get("test_range") or {}
    if tr:
        out += [f"Test range: blocks {tr.get('start_block')}–{tr.get('stop_block')} on {tr.get('chain', chain)}. {tr.get('why', '')}", ""]
    fs = spec.get("fork_setup") or {}
    if fs.get("script"):
        out += [f"Fork setup script: `{fs['script']}`. {fs.get('why', '')}", ""]
    if spec.get("proxies"):
        out.append("### Proxies (checked again at the final gates)")
        out.append("")
        out += table(["chain", "proxy", "implementation", "slot"], [[p["chain"], p["proxy"], p["implementation"], p.get("slot", "EIP-1967")] for p in spec["proxies"]])
    if spec.get("venue_quotes"):
        out.append("### Venue quotes (replayed from the Tycho router, and tested against the simulation to the wei)")
        out.append("")
        out += table(["id", "chain", "block", "direction", "amount in", "result", "caller-dependent"],
                     [[q.get("id"), q.get("chain"), q.get("block"), q.get("direction", ""), q.get("amount_in", ""), q.get("result", ""), q.get("caller_fact") or "no"] for q in spec["venue_quotes"]])
    if spec.get("watch"):
        out.append("### Watched on chain (re-read at the final gates and by `--recheck`)")
        out.append("")
        out += table(["chain", "address", "block", "what"], [[w.get("chain"), w.get("address"), w.get("block"), w.get("what") or ("getter " + w["data"][:10] if w.get("data") else "code")] for w in spec["watch"]])
    if spec.get("read_set"):
        out.append("### What a quote reads (prestateTracer)")
        out.append("")
        out += table(["address", "slots", "tracked by"], [[r.get("address"), len(r.get("slots", [])), r.get("tracked_by") or f"untracked: {r.get('why_untracked', '')}"] for r in spec["read_set"]])

    section("differences")
    sib = spec.get("sibling") or {}
    if sib:
        out += [f"Nearest integrated sibling: `{sib.get('protocol_system')}`. {sib.get('why', '')}", ""]
        out.extend(f"- {d}" for d in sib.get("differences", []))
        out.append("")
    if spec.get("templates"):
        out.append("### Other open-source implementations (leads, not evidence)")
        out.append("")
        out += table(["repo", "path", "license", "use"], [[t.get("repo", ""), t.get("path", ""), t.get("license", "unknown"), t.get("use", "")] for t in spec["templates"]])
    out.append("### Open risks")
    out.append("")
    out.extend(f"- {r}" for r in spec.get("open_risks", []) or ["None recorded."])
    out.append("")
    out.append("### Out of scope")
    out.append("")
    out.extend(f"- {r}" for r in spec.get("out_of_scope", []) or ["Nothing recorded."])
    out.append("")
    if spec.get("amendments"):
        out.append("## Amendments")
        out.append("")
        out += table(["#", "fact", "was", "now", "evidence", "source"], [[a.get("n", i + 1), a.get("fact_id", ""), a.get("old", ""), a.get("new", ""), a.get("evidence", ""), a.get("source", "")] for i, a in enumerate(spec["amendments"])])
    return "\n".join(out).rstrip() + "\n"


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    p.add_argument("--check", action="store_true")
    args = p.parse_args()
    run = schema.run_dir(args.protocol)
    spec = schema.load_json(run / "spec.json")
    if args.check:
        errors = schema.validate_spec(spec)
        if errors:
            print("\n".join(errors))
            sys.exit(2)
    intake = schema.load_json(run / "intake.json") if (run / "intake.json").exists() else {}
    env = schema.load_json(run / "env.json") if (run / "env.json").exists() else {}
    (run / "SPEC.md").write_text(render(spec, intake, env))
    print(run / "SPEC.md")


if __name__ == "__main__":
    main()
