#!/usr/bin/env python3
"""Creates the run's worktree, and says which branch its PR targets.

    worktree.py --protocol P          create it (idempotent), print JSON {path, branch, pr_base, merged}

The worktree is `origin/main` on branch `integrate/<slug>`, in `.claude/worktrees/integrate-<slug>`.
While `schema.HARNESS_BRANCH` (the strict integration-test suite) is not on main, it is merged in,
and the PR targets that branch, so its diff is the integration alone. A merge that conflicts is
aborted and reported (exit 2): the person decides (class outside-repo). The git-ignored `.env`
files the tests read are linked from the person's checkout, which is never touched.
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


def contains(ref: str, ancestor: str, cwd) -> bool:
    return git("merge-base", "--is-ancestor", ancestor, ref, cwd=cwd, check=False).returncode == 0


def finish_with_rerere(tree: Path) -> tuple:
    """Completes a conflicted merge when git rerere (shared by the repository's worktrees) already
    resolved every conflict, as recorded when the suite was last merged with main: (the commit's
    result, the files still conflicted). Anything rerere did not resolve is the person's."""
    unmerged = git("diff", "--name-only", "--diff-filter=U", cwd=tree, check=False).stdout.split()
    remaining = git("rerere", "remaining", cwd=tree, check=False).stdout.split()
    resolved = git("rerere", "status", cwd=tree, check=False).stdout.split()
    failed = subprocess.CompletedProcess([], 1)
    if remaining or (unmerged and not set(unmerged) <= set(resolved)):
        return failed, sorted(set(unmerged) | set(remaining))
    touched = sorted(set(unmerged) | set(resolved))
    if touched and git("grep", "-l", "-e", "^<<<<<<< ", "--", *touched, cwd=tree, check=False).stdout.strip():
        return failed, touched
    if touched:
        git("add", "--", *touched, cwd=tree)
    return git("commit", "--no-edit", cwd=tree, check=False), []


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    args = p.parse_args()

    repo = schema.repo_root()
    tree = schema.worktree_dir(args.protocol)
    branch = schema.branch(args.protocol)
    git("fetch", "origin", cwd=repo)
    if not tree.exists():
        git("worktree", "add", "-b", branch, str(tree), "origin/main", cwd=repo)
        git("submodule", "update", "--init", "--recursive", cwd=tree)

    harness = f"origin/{schema.HARNESS_BRANCH}" if schema.HARNESS_BRANCH else None
    if harness and git("rev-parse", "--verify", "-q", harness, cwd=repo, check=False).returncode != 0:
        sys.exit(f"{harness} does not exist: if stack #1804 merged, set schema.HARNESS_BRANCH to None")
    pending = schema.harness_pending()
    merged = False
    if pending and not contains("HEAD", harness, tree):
        proc = git("merge", "--no-edit", "-m", f"Merge {schema.HARNESS_BRANCH} (the strict integration-test suite)", harness, cwd=tree, check=False)
        if proc.returncode != 0:
            proc, conflicts = finish_with_rerere(tree)
        if proc.returncode != 0:
            git("merge", "--abort", cwd=tree, check=False)
            print(json.dumps({"error": "merge-conflict", "branch": schema.HARNESS_BRANCH, "files": conflicts}))
            sys.exit(2)
        merged = True

    for rel in ENV_FILES:
        src, dst = repo / rel, tree / rel
        if src.exists() and not dst.exists():
            dst.parent.mkdir(parents=True, exist_ok=True)
            dst.symlink_to(src)

    print(json.dumps({
        "path": str(tree),
        "branch": branch,
        "pr_base": schema.pr_base(),
        "merged": merged,
    }))


if __name__ == "__main__":
    main()
