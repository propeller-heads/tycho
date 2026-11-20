#!/usr/bin/env python3
"""Splits the gated branch of a tycho-integrate run into stacked PR branches, and proves the split
is the gated tree.

    split.py --protocol P --branch B --onto <ref> --paths <path>... [--dir D]
        a new worktree D (default .claude/worktrees/<B with / as ->) on branch B cut from <ref>,
        carrying the run branch's commits restricted to --paths, in order
    split.py --protocol P --verify --top B
        the run branch's tree against B's: exit 0 when they are identical, else the differing files

Each run commit that touches --paths is replayed with its message and author, limited to those
paths, so a verbatim-copy commit stays before the commit that adapts it
(`copy-commit-before-adaptation`). A final commit aligns the paths with the run branch if anything
the replay could not carry (a merge) is left.

Split bottom up: the first branch is cut from the PR base, each next one from the one below it, and
the last one (`--top`) must equal the run branch, so the layers the gates and the final review checked
are exactly what the PRs carry. A failed split removes its branch and worktree, so it can simply be
run again. Nothing is pushed.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import schema  # noqa: E402

git = schema.git


def replay(tree: Path, run_branch: str, onto: str, paths: list) -> list:
    """Applies each run commit's change to `paths` in order; the new commits' ids."""
    shas = git("rev-list", "--reverse", "--no-merges", f"{onto}..{run_branch}", "--", *paths, cwd=tree).stdout.split()
    made = []
    for sha in shas:
        patch = git("diff", "--binary", f"{sha}^", sha, "--", *paths, cwd=tree).stdout
        if not patch.strip():
            continue
        subprocess.run(["git", "apply", "--index", "--3way", "-"], input=patch, text=True, cwd=tree, check=True, capture_output=True)
        if git("diff", "--cached", "--quiet", cwd=tree, check=False).returncode == 0:
            continue
        git("commit", "-q", "--no-verify", "-C", sha, cwd=tree)
        made.append(git("rev-parse", "HEAD", cwd=tree).stdout.strip())
    return made


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--protocol", required=True)
    p.add_argument("--branch")
    p.add_argument("--onto")
    p.add_argument("--paths", nargs="+")
    p.add_argument("--message", help="the subject of the aligning commit, when one is needed")
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
    if git("rev-parse", "--verify", "-q", f"refs/heads/{args.branch}", cwd=repo, check=False).returncode == 0:
        sys.exit(f"{args.branch} exists already; remove its worktree and branch to split again")
    try:
        git("worktree", "add", "-b", args.branch, str(tree), args.onto, cwd=repo)
        made = replay(tree, run_branch, args.onto, args.paths)
        git("checkout", run_branch, "--", *args.paths, cwd=tree)
        if git("diff", "--cached", "--quiet", cwd=tree, check=False).returncode != 0:
            git("commit", "-q", "-m", args.message or f"chore({args.protocol}): align {args.branch} with the gated branch", cwd=tree)
            made.append(git("rev-parse", "HEAD", cwd=tree).stdout.strip())
        git("submodule", "update", "--init", "--recursive", cwd=tree, check=False)
    except subprocess.CalledProcessError as e:
        git("worktree", "remove", "--force", str(tree), cwd=repo, check=False)
        git("branch", "-D", args.branch, cwd=repo, check=False)
        sys.exit(f"split of {args.branch} failed and was undone: {' '.join(e.cmd)}: {(e.stderr or '').strip()}")
    print(json.dumps({"path": str(tree), "branch": args.branch, "commits": len(made), "head": git("rev-parse", "HEAD", cwd=tree).stdout.strip()}))


if __name__ == "__main__":
    main()
