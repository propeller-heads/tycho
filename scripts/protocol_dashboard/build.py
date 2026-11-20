#!/usr/bin/env python3
"""Build the protocol integration dashboard: one HTML page, one tab per chain.

Sources:
- GitHub project (org project with Status, Protocol and Chains fields): the items, their
  manual stage and their linked pull requests.
- A checkout of main: executor addresses and which layers mention the protocol.
Cards cover the project's items and, marked as untracked, every other protocol that main or an
indexer knows on a chain, so the board also shows what waits for dev and what waits for prod.
- The Tycho indexers (optional): which protocol systems the prod (`tycho-<chain>-beta`) and dev
  (`tycho-<chain>-dev`) indexers serve, and for Uniswap V4 hooks, whether any pool carries the
  hook's `hook_identifier`.

Standard library only. Environment:
  GH_TOKEN          token that can read the org project and its pull requests (required)
  PROJECT_OWNER     default propeller-heads
  PROJECT_NUMBER    default 4
  MAIN_DIR          checkout of main, default "."
  TYCHO_API_KEY     indexer API key; without it the In Dev / In Prod checks are skipped
  TYCHO_MIN_TVL_PROD, TYCHO_MIN_TVL_DEV
                    tvl_gt for the hook pool queries, default 0.1 and 1: the smallest values
                    the key's plan accepts, in the chain's native token
  TYCHO_HOST_<ENV>_<CHAIN>
                    overrides one indexer host, e.g. TYCHO_HOST_DEV_ARBITRUM
  OUT_DIR           default "out"
"""

from __future__ import annotations

import datetime as dt
import json
import os
import re
import socket
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

STAGES = [
    "To do",
    "Planned",
    "In progress",
    "In review",
    "Merged",
    "Executor deployed",
    "In Dev",
    "In Prod",
]
RANK = {s.lower(): i for i, s in enumerate(STAGES)}
GRAVEYARD = "graveyard"
HOOK_SYSTEMS = ("uniswap_v4_hooks", "uniswap_v4")
# Protocol systems without an exact executor entry use their family's, as
# `SwapEncoderRegistry::get_encoder` resolves them. `rfq:aqua0` reuses the Uniswap V4 executor.
EXECUTOR_FAMILIES = {
    "uniswap_v4_hooks": "uniswap_v4",
    "pancakeswap_infinity_cl_hooks": "pancakeswap_infinity_cl",
    "rfq:aqua0": "uniswap_v4",
}
BIN_POOL_MANAGER_CHAINS = {"base", "bsc"}  # SwapEncoderRegistry::has_bin_pool_manager
# RFQ venues and pAMM book feeds quote off-chain, so no indexer serves them: an executor for the
# chain on main means they are live in dev and prod.
OFFCHAIN_PREFIXES = ("rfq:", "pamm:", "propamm:")
# Other executor entries that no indexer serves: fallbacks and helpers. The indexers can't say
# whether these are live, so they get no untracked card and tracked ones show "can't tell".
NOT_INDEXED_PREFIXES = ("fallback", "pricelevelstream")
NOT_INDEXED = {"native_wrapper", "subsidy"}

PROJECT_QUERY = """
query($owner: String!, $number: Int!, $after: String) {
  organization(login: $owner) {
    projectV2(number: $number) {
      title
      url
      items(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          isArchived
          status: fieldValueByName(name: "Status") {
            ... on ProjectV2ItemFieldSingleSelectValue { name }
          }
          protocol: fieldValueByName(name: "Protocol") {
            ... on ProjectV2ItemFieldTextValue { text }
          }
          chains: fieldValueByName(name: "Chains") {
            ... on ProjectV2ItemFieldTextValue { text }
          }
          content {
            __typename
            ... on DraftIssue { title }
            ... on Issue {
              title url number state
              closedByPullRequestsReferences(first: 10, includeClosedPrs: true) {
                nodes { ...pr }
              }
            }
            ... on PullRequest { ...pr }
          }
        }
      }
    }
  }
}
fragment pr on PullRequest {
  title url number state isDraft mergedAt
  author { login }
}
"""

def log(msg: str) -> None:
    print(msg, file=sys.stderr)


def warn(msg: str) -> None:
    # Shows as an annotation on the workflow run.
    print(f"::warning::{msg}", file=sys.stderr)


def post_json(url: str, body: dict, headers: dict) -> dict:
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", **headers},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=60) as resp:
        return json.load(resp)


# ── GitHub project ──────────────────────────────────────────────────────────


def fetch_project(token: str, owner: str, number: int) -> tuple[dict, list[dict]]:
    items: list[dict] = []
    after = None
    meta: dict = {}
    while True:
        resp = post_json(
            "https://api.github.com/graphql",
            {"query": PROJECT_QUERY, "variables": {"owner": owner, "number": number, "after": after}},
            {"Authorization": f"Bearer {token}"},
        )
        if resp.get("errors"):
            raise RuntimeError(f"GitHub GraphQL errors: {json.dumps(resp['errors'])}")
        project = (resp.get("data") or {}).get("organization", {}).get("projectV2")
        if project is None:
            raise RuntimeError(
                f"Project {owner}/{number} not visible to this token; it needs "
                "organization projects: read."
            )
        meta = {"title": project["title"], "url": project["url"]}
        page = project["items"]
        items.extend(n for n in page["nodes"] if n and not n["isArchived"])
        if not page["pageInfo"]["hasNextPage"]:
            return meta, items
        after = page["pageInfo"]["endCursor"]


def split_list(value: str | None) -> list[str]:
    return [v.strip().lower() for v in (value or "").split(",") if v.strip()]


def pr_view(pr: dict) -> dict:
    if pr.get("mergedAt"):
        state = "merged"
    elif pr["state"] == "CLOSED":
        state = "closed"
    elif pr.get("isDraft"):
        state = "draft"
    else:
        state = "open"
    return {
        "number": pr["number"],
        "url": pr["url"],
        "title": pr["title"],
        "state": state,
        "author": (pr.get("author") or {}).get("login"),
        "mergedAt": pr.get("mergedAt"),
    }


def item_view(node: dict) -> dict:
    content = node.get("content") or {}
    kind = content.get("__typename", "DraftIssue")
    prs = []
    if kind == "PullRequest":
        prs = [pr_view(content)]
    elif kind == "Issue":
        prs = [pr_view(p) for p in content["closedByPullRequestsReferences"]["nodes"]]
    return {
        "kind": kind,
        "title": content.get("title", "(untitled)"),
        "url": content.get("url"),
        "status": ((node.get("status") or {}).get("name") or "").strip(),
        "protocols": split_list((node.get("protocol") or {}).get("text")),
        "chains": split_list((node.get("chains") or {}).get("text")),
        "prs": prs,
    }


# ── Protocol keys ───────────────────────────────────────────────────────────


def split_key(protocol: str) -> tuple[str, str | None]:
    """`uniswap_v4_hooks:nftx_v1` -> (`uniswap_v4_hooks`, `nftx_v1`); others unchanged."""
    for system in HOOK_SYSTEMS:
        if protocol.startswith(system + ":"):
            return system, protocol[len(system) + 1 :]
    return protocol, None


# ── Main checkout ───────────────────────────────────────────────────────────


def manifest_chain(substreams_yaml_path: str) -> str:
    """The chain of an integration test, read the way `tycho_test::expectations::package_chain`
    reads it: `substreams.yaml` is Ethereum, otherwise the manifest name's first word."""
    name = Path(substreams_yaml_path).stem
    name = name.removeprefix("test-")
    return "ethereum" if name == "substreams" else name.split("-", 1)[0].lower()


@dataclass
class MainRepo:
    executors: dict = field(default_factory=dict)
    executor_deployments: dict = field(default_factory=dict)
    tests: list[tuple[str, str]] = field(default_factory=list)  # (chain, yaml text)
    test_systems: dict[str, set[str]] = field(default_factory=dict)  # chain -> protocol systems

    @classmethod
    def load(cls, root: Path) -> "MainRepo":
        config = root / "crates/tycho-execution/config"
        repo = cls(
            executors=json.loads((config / "executor_addresses.json").read_text()),
            executor_deployments=json.loads((config / "executor_deployments.json").read_text()),
        )
        manifest = re.compile(r"""^substreams_yaml_path:\s*["']?([^"'\s#]+)""", re.M)
        declared = re.compile(r"""^protocol_system:\s*["']?([^"'\s#]+)""", re.M)
        for path in (root / "protocols/substreams").rglob("integration_test*.tycho.yaml"):
            if "template" in path.parent.name:
                continue
            text = path.read_text(errors="replace")
            if m := manifest.search(text):
                chain = manifest_chain(m.group(1))
                repo.tests.append((chain, text))
                repo.test_systems.setdefault(chain, set()).update(declared.findall(text))
        return repo

    def executor_key(self, chain: str, system: str) -> str:
        entries = self.executors.get(chain) or {}
        if system in entries:
            return system
        if system == "pancakeswap_infinity_bin" and chain in BIN_POOL_MANAGER_CHAINS:
            return "pancakeswap_infinity_cl"
        return EXECUTOR_FAMILIES.get(system, system)

    def executor(self, chain: str, system: str) -> str | None:
        return (self.executors.get(chain) or {}).get(self.executor_key(chain, system))

    def layers(self, chain: str, system: str, hook: str | None) -> dict[str, bool]:
        """What main already has for this chain. Hints shown on the card, not stage gates."""
        declares = re.compile(rf"""^protocol_system:\s*["']?{re.escape(system)}["']?\s*$""", re.M)
        test = any(
            c == chain and declares.search(t) and (hook is None or hook in t) for c, t in self.tests
        )
        key = self.executor_key(chain, system)
        deployment = key in (self.executor_deployments.get(chain) or {})
        return {"test": test, "deployment": deployment}


# ── Indexers ────────────────────────────────────────────────────────────────

ENVS = ("dev", "prod")


class Indexers:
    """What the dev and prod Tycho indexers serve. Queried lazily and cached.

    A chain with no indexer host (DNS fails) counts as "not running"; any other failure leaves
    the answer unknown (None) and is reported in `problems`.
    """

    def __init__(self, api_key: str, min_tvl: dict[str, float]):
        self.api_key = api_key
        self.min_tvl = min_tvl
        self.problems: list[str] = []
        self._systems: dict[tuple[str, str], set[str] | None] = {}
        self._hooks: dict[tuple[str, str, str], set[str] | None] = {}

    @staticmethod
    def host(env: str, chain: str) -> str:
        if override := os.environ.get(f"TYCHO_HOST_{env.upper()}_{chain.upper()}"):
            return override
        suffix = "beta" if env == "prod" else "dev"
        name = f"tycho-{suffix}" if chain == "ethereum" else f"tycho-{chain}-{suffix}"
        return f"{name}.propellerheads.xyz"

    def _post(self, env: str, chain: str, endpoint: str, body: dict) -> dict:
        """POST with up to three attempts: the indexers drop the odd TLS connection."""
        for attempt in range(3):
            try:
                return post_json(
                    f"https://{self.host(env, chain)}/v1/{endpoint}",
                    {"chain": chain, **body},
                    {"authorization": self.api_key},
                )
            except urllib.error.HTTPError as err:
                if err.code < 500 or attempt == 2:
                    raise
            except (urllib.error.URLError, TimeoutError, ConnectionError) as err:
                if self._no_host(err) or attempt == 2:
                    raise
            time.sleep(2 * (attempt + 1))
        raise AssertionError("unreachable")

    def _failed(self, env: str, chain: str, what: str, err: Exception) -> None:
        if isinstance(err, urllib.error.HTTPError):
            detail = f"HTTP {err.code}: {err.read().decode(errors='replace')[:200]}"
        else:
            detail = str(err)
        self.problems.append(f"{env} {chain} {what}: {detail}")

    @staticmethod
    def _no_host(err: Exception) -> bool:
        return (
            isinstance(err, urllib.error.URLError)
            and not isinstance(err, urllib.error.HTTPError)
            and isinstance(err.reason, socket.gaierror)
        )

    def systems(self, env: str, chain: str) -> set[str] | None:
        key = (env, chain)
        if key not in self._systems:
            try:
                found: set[str] = set()
                page = 0
                while True:
                    resp = self._post(env, chain, "protocol_systems",
                                      {"pagination": {"page": page, "page_size": 100}})
                    found.update(resp["protocol_systems"])
                    page += 1
                    if page * 100 >= resp["pagination"]["total"]:
                        break
                self._systems[key] = found
            except (urllib.error.URLError, TimeoutError, ConnectionError, KeyError, ValueError) as err:
                if self._no_host(err):
                    self._systems[key] = set()
                else:
                    self._failed(env, chain, "protocol_systems", err)
                    self._systems[key] = None
        return self._systems[key]

    def runs(self, env: str, chain: str, system: str) -> bool | None:
        systems = self.systems(env, chain)
        return None if systems is None else system in systems

    def load_hooks(self, env: str, chain: str, system: str, wanted: set[str]) -> None:
        """Page through `system`'s pools above the TVL floor until every wanted hook is seen."""
        key = (env, chain, system)
        if key in self._hooks:
            return
        if not self.runs(env, chain, system):
            self._hooks[key] = set() if self.runs(env, chain, system) is False else None
            return
        found: set[str] = set()
        try:
            page = 0
            while not wanted <= found:
                resp = self._post(env, chain, "protocol_components", {
                    "protocol_system": system,
                    "tvl_gt": self.min_tvl[env],
                    "pagination": {"page": page, "page_size": 500},
                })
                for comp in resp["protocol_components"]:
                    raw = (comp.get("static_attributes") or {}).get("hook_identifier")
                    if raw:
                        found.add(decode_hex_text(raw))
                page += 1
                if page * 500 >= resp["pagination"]["total"]:
                    break
            self._hooks[key] = found
        except (urllib.error.URLError, TimeoutError, ConnectionError, KeyError, ValueError) as err:
            self._failed(env, chain, f"{system} pools", err)
            self._hooks[key] = None

    def has_hook(self, env: str, chain: str, system: str, hook: str) -> bool | None:
        found = self._hooks.get((env, chain, system))
        return None if found is None else hook in found


def is_offchain(system: str) -> bool:
    return system.startswith(OFFCHAIN_PREFIXES)


def is_indexed(system: str) -> bool:
    return (
        not is_offchain(system)
        and system not in NOT_INDEXED
        and not system.startswith(NOT_INDEXED_PREFIXES)
    )


def is_tracked_kind(system: str) -> bool:
    """Whether the dashboard can say where this system is live: indexed, or an off-chain venue."""
    return is_offchain(system) or is_indexed(system)


def decode_hex_text(value: str) -> str:
    try:
        return bytes.fromhex(value.removeprefix("0x")).decode()
    except ValueError:
        return value


# ── Cards ───────────────────────────────────────────────────────────────────


def build_cards(items: list[dict], repo: MainRepo, indexers: Indexers | None) -> tuple[list[dict], list[dict]]:
    groups: dict[tuple[str, str], list[dict]] = {}
    unassigned = []
    for item in items:
        if not item["protocols"] or not item["chains"]:
            unassigned.append(item)
            continue
        for chain in item["chains"]:
            for protocol in item["protocols"]:
                groups.setdefault((chain, protocol), []).append(item)

    if indexers is not None:
        wanted: dict[tuple[str, str], set[str]] = {}
        for chain, protocol in groups:
            system, hook = split_key(protocol)
            if hook:
                wanted.setdefault((chain, system), set()).add(hook)
        for (chain, system), hooks in sorted(wanted.items()):
            for env in ENVS:
                indexers.load_hooks(env, chain, system, hooks)

    cards = []
    for (chain, protocol), group in sorted(groups.items()):
        statuses = [i["status"].lower() for i in group]
        live = [s for s in statuses if s != GRAVEYARD]
        if not live:
            cards.append({"chain": chain, "protocol": protocol, "stage": "Graveyard", "graveyard": True,
                          "items": group, "prs": [], "checks": {}, "layers": {}, "flags": []})
            continue
        manual = max((RANK.get(s, -1) for s in live), default=-1)

        prs = {}
        for item in group:
            if item["status"].lower() == GRAVEYARD:
                continue
            for pr in item["prs"]:
                prs[pr["url"]] = pr
        prs = sorted(prs.values(), key=lambda p: p["number"])
        relevant = [p for p in prs if p["state"] != "closed"]

        system, hook = split_key(protocol)
        executor = repo.executor(chain, system)
        checks: dict[str, bool | None] = {
            "In review": any(p["state"] == "open" for p in relevant),
            "Merged": bool(relevant) and all(p["state"] == "merged" for p in relevant),
            "Executor deployed": executor is not None,
        }
        for env, name in (("dev", "In Dev"), ("prod", "In Prod")):
            if is_offchain(system):
                # Live once its code is merged and the chain has its executor. Some off-chain
                # venues reuse an existing executor (rfq:aqua0), so the executor alone isn't enough.
                checks[name] = executor is not None and checks["Merged"]
            elif indexers is None or not is_indexed(system):
                checks[name] = None  # not checked, or not something an indexer serves
            elif hook:
                # One extractor serves every hook; a hook is live once a pool carries its tag.
                checks[name] = indexers.has_hook(env, chain, system, hook)
            else:
                checks[name] = indexers.runs(env, chain, system)

        # The stages are a sequence: a later check only counts once the earlier ones pass. An
        # existing executor (hooks, a chain that already has the family) or a branch deployed to
        # dev doesn't move an unmerged protocol forward; the raw checks still show on the card.
        if checks["In review"]:
            derived = RANK["in review"]
        elif any(p["state"] == "draft" for p in relevant):
            derived = RANK["in progress"]
        else:
            derived = -1
        if checks["Merged"]:
            derived = RANK["merged"]
            if checks["Executor deployed"]:
                derived = RANK["executor deployed"]
                if checks["In Dev"]:
                    derived = RANK["in dev"]
                if checks["In Prod"]:
                    derived = RANK["in prod"]
        stage = max(manual, derived)

        flags = []
        running = [n for n in ("In Dev", "In Prod") if checks.get(n)]
        if running and not checks["Merged"]:
            # Usually a change to a protocol that is already live, or a branch deployed to dev.
            flags.append(f"{system} already live ({', '.join(running)}) while PRs are unmerged")
        if manual >= RANK["merged"] and relevant and not checks["Merged"]:
            flags.append(f"Project says {STAGES[manual]}, but not all PRs are merged")
        if manual >= RANK["executor deployed"] and not executor:
            flags.append(f"Project says {STAGES[manual]}, but main has no {chain} executor for {system}")
        if indexers is not None:
            for env, name in (("dev", "In Dev"), ("prod", "In Prod")):
                if manual >= RANK[name.lower()] and checks[name] is False:
                    what = f"a {hook} pool above {indexers.min_tvl[env]} TVL" if hook else system
                    flags.append(f"Project says {STAGES[manual]}, but the {env} indexer on {chain} has no {what}")
                    break
        if derived > manual and manual >= 0:
            flags.append(f"Project status {STAGES[manual]} is behind the evidence ({STAGES[derived]})")

        cards.append({
            "chain": chain,
            "protocol": protocol,
            "system": system,
            "hook": hook,
            "stage": STAGES[stage] if stage >= 0 else "To do",
            "manual": STAGES[manual] if manual >= 0 else None,
            "derived": STAGES[derived] if derived >= 0 else None,
            "checks": checks,
            "executor": executor,
            "layers": repo.layers(chain, system, hook),
            "prs": prs,
            "items": [{"title": i["title"], "url": i["url"], "kind": i["kind"], "status": i["status"]} for i in group],
            "flags": flags,
            "graveyard": False,
            "tracked": True,
        })

    cards.extend(untracked_cards(cards, repo, indexers))
    return cards, unassigned


def untracked_cards(tracked: list[dict], repo: MainRepo, indexers: Indexers | None) -> list[dict]:
    """A card for every protocol main or an indexer knows on a chain that the project doesn't.

    There are no PRs to follow, so the stage is simply the furthest place it has reached:
    an integration test on main (Merged), an executor on main, the dev indexer, the prod indexer.
    """
    known = {(c["chain"], c["protocol"]) for c in tracked}
    chains = set(repo.executors) | set(repo.test_systems) | {c["chain"] for c in tracked}
    cards = []
    for chain in sorted(chains):
        on_main = repo.test_systems.get(chain, set())
        executors = {k for k in (repo.executors.get(chain) or {}) if is_tracked_kind(k)}
        dev = (indexers.systems("dev", chain) if indexers else None) or set()
        prod = (indexers.systems("prod", chain) if indexers else None) or set()
        for system in sorted(on_main | executors | dev | prod):
            if (chain, system) in known or not is_tracked_kind(system):
                continue
            executor = repo.executor(chain, system)
            if is_offchain(system):
                in_dev = in_prod = executor is not None
            else:
                in_dev = indexers.runs("dev", chain, system) if indexers else None
                in_prod = indexers.runs("prod", chain, system) if indexers else None
            checks: dict[str, bool | None] = {
                "In review": False,
                "Merged": system in on_main,
                "Executor deployed": executor is not None,
                "In Dev": in_dev,
                "In Prod": in_prod,
            }
            reached = [RANK[n.lower()] for n in ("Merged", "Executor deployed", "In Dev", "In Prod") if checks[n]]
            stage = max(reached, default=RANK["merged"])
            flags = []
            if checks["In Prod"] and checks["In Dev"] is False:
                flags.append("In prod but not in dev")
            if (checks["In Dev"] or checks["In Prod"]) and not executor:
                flags.append(f"Indexed, but main has no {chain} executor")
            cards.append({
                "chain": chain,
                "protocol": system,
                "system": system,
                "hook": None,
                "stage": STAGES[stage],
                "manual": None,
                "derived": STAGES[stage],
                "checks": checks,
                "executor": executor,
                "layers": repo.layers(chain, system, None),
                "prs": [],
                "items": [],
                "flags": flags,
                "graveyard": False,
                "tracked": False,
            })
    return cards


# ── Output ──────────────────────────────────────────────────────────────────


def waiting_for(card: dict) -> str | None:
    """"dev" or "prod" when the card's next step is confirmed missing; None otherwise. A check
    that couldn't run (an indexer that didn't answer) doesn't put a card on either list."""
    if card["graveyard"]:
        return None
    if card["stage"] == "Executor deployed" and card["checks"].get("In Dev") is False:
        return "dev"
    if card["stage"] == "In Dev" and card["checks"].get("In Prod") is False:
        return "prod"
    return None


def render(data: dict, template: Path) -> str:
    payload = json.dumps(data, default=sorted).replace("</", "<\\/")
    return template.read_text().replace("/*__DATA__*/null", payload)


def main() -> int:
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token:
        log("GH_TOKEN is not set")
        return 2
    owner = os.environ.get("PROJECT_OWNER", "propeller-heads")
    number = int(os.environ.get("PROJECT_NUMBER", "4"))
    main_dir = Path(os.environ.get("MAIN_DIR", "."))
    out_dir = Path(os.environ.get("OUT_DIR", "out"))

    project, nodes = fetch_project(token, owner, number)
    items = [item_view(n) for n in nodes]
    log(f"project: {len(items)} items")

    repo = MainRepo.load(main_dir)

    sources = {"project": "ok", "main": "ok"}
    indexers = None
    if api_key := os.environ.get("TYCHO_API_KEY"):
        indexers = Indexers(api_key, {
            "prod": float(os.environ.get("TYCHO_MIN_TVL_PROD", "0.1")),
            "dev": float(os.environ.get("TYCHO_MIN_TVL_DEV", "1")),
        })
    else:
        sources["indexers"] = "skipped: no TYCHO_API_KEY"
        warn("TYCHO_API_KEY not set, In Dev / In Prod not checked")

    cards, unassigned = build_cards(items, repo, indexers)
    for card in cards:
        card["waiting"] = waiting_for(card)
    if indexers is not None:
        if indexers.problems:
            sources["indexers"] = f"partial: {len(indexers.problems)} failed queries"
            for problem in indexers.problems:
                warn(f"Indexer query failed: {problem}")
        else:
            sources["indexers"] = "ok"
    main_sha = os.environ.get("MAIN_SHA", "")
    data = {
        "generatedAt": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
        "project": project,
        "mainSha": main_sha,
        "runUrl": os.environ.get("RUN_URL", ""),
        "stages": STAGES,
        "sources": sources,
        "cards": cards,
        "unassigned": unassigned,
    }

    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "status.json").write_text(json.dumps(data, indent=2, default=sorted))
    html = render(data, Path(__file__).with_name("template.html"))
    (out_dir / "index.html").write_text(html)
    log(f"wrote {out_dir / 'index.html'}: {len(cards)} cards, {len(unassigned)} unassigned")

    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        live = [c for c in cards if not c["graveyard"]]
        lines = [
            "### Protocol dashboard",
            "",
            "Download the `protocol-dashboard` artifact from this run and open `index.html`.",
            "",
            "Sources: " + ", ".join(f"{k} {v}" for k, v in sources.items()),
            "",
            "| Stage | Cards |",
            "|---|---|",
            *(f"| {s} | {n} |" for s in STAGES if (n := sum(c["stage"] == s for c in live))),
            "",
            f"{sum(bool(c['flags']) for c in live)} cards flagged, {len(unassigned)} items missing Protocol or Chains.",
            "",
            "**Waiting for dev** (executor on main, not in the dev indexer): "
            + (", ".join(f"{c['chain']}/{c['protocol']}" for c in live if c["waiting"] == "dev") or "none"),
            "",
            "**Waiting for prod** (in dev, not in prod): "
            + (", ".join(f"{c['chain']}/{c['protocol']}" for c in live if c["waiting"] == "prod") or "none"),
        ]
        with open(summary, "a") as fh:
            fh.write("\n".join(lines) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
