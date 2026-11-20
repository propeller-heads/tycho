#!/usr/bin/env python3
"""Splits the gated branch of a tycho-integrate run into stacked PR branches, and proves the split
is the gated tree.

    split.py --protocol P --branch B --onto <ref> --paths <path>... [--message M] [--dir D]
        a new worktree D (default .claude/worktrees/<B with / as ->) on branch B cut from <ref>,
        holding the run branch's version of --paths in one commit
    split.py --protocol P --verify --top B
        the run branch's tree against B's: exit 0 when they are identical, else the differing files

Split bottom up: the first branch is cut from the PR base, each next one from the one below it, and
the last one (`--top`) must equal the run branch, so the layers the gates and the final review checked
are exactly what the PRs carry. Nothing is pushed.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402


def git(*args, cwd=None, check=True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=check)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    p.add_argument("--branch")
    p.add_argument("--onto")
    p.add_argument("--paths", nargs="+")
    p.add_argument("--message")
    p.add_argument("--dir")
    p.add_argument("--verify", action="store_true")
    p.add_argument("--top")
    args = p.parse_args()

    repo, run_branch = schema.repo_root(), schema.branch(args.protocol)
    if args.verify:
        if not args.top:
            sys.exit("--verify needs --top <branch>")
        diff = git("diff", "--name-only", run_branch, args.top, cwd=repo).stdout.split()
        print(json.dumps({"identical": not diff, "differs": diff}))
        sys.exit(1 if diff else 0)

    if not (args.branch and args.onto and args.paths):
        sys.exit("--branch, --onto and --paths are required")
    tree = Path(args.dir) if args.dir else repo / ".claude" / "worktrees" / args.branch.replace("/", "-")
    git("worktree", "add", "-b", args.branch, str(tree), args.onto, cwd=repo)
    git("checkout", run_branch, "--", *args.paths, cwd=tree)
    git("submodule", "update", "--init", "--recursive", cwd=tree, check=False)
    git("commit", "-m", args.message or f"{args.protocol}: {args.branch}", cwd=tree)
    print(json.dumps({"path": str(tree), "branch": args.branch, "head": git("rev-parse", "HEAD", cwd=tree).stdout.strip()}))


if __name__ == "__main__":
    main()
