#!/usr/bin/env python3
"""Creates the run's worktree.

    worktree.py --protocol P          create it (idempotent), print JSON {path, branch, base}

The worktree is `origin/main` on branch `integrate/<slug>`, in `.claude/worktrees/integrate-<slug>`,
with its submodules. The git-ignored `.env` files the tests read are linked from the person's
checkout, which is never touched.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402

ENV_FILES = ("docker/.env", ".env", "protocols/testing/.env")


def git(*args, cwd=None, check=True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=check)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    args = p.parse_args()

    repo = schema.repo_root()
    tree = schema.worktree_dir(args.protocol)
    branch = schema.branch(args.protocol)
    git("fetch", "origin", cwd=repo)
    if not tree.exists():
        git("worktree", "add", "-b", branch, str(tree), f"origin/{schema.BASE}", cwd=repo)
    # Every time, not only on creation: a failed update (no network) is retried by the rerun
    # instead of leaving the worktree without forge's `lib/`.
    git("submodule", "update", "--init", "--recursive", cwd=tree)

    for rel in ENV_FILES:
        src, dst = repo / rel, tree / rel
        if src.exists() and not dst.exists():
            dst.parent.mkdir(parents=True, exist_ok=True)
            dst.symlink_to(src)

    print(json.dumps({"path": str(tree), "branch": branch, "base": schema.BASE}))


if __name__ == "__main__":
    main()
