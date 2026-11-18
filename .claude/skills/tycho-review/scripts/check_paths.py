#!/usr/bin/env python3
"""Check the path table (`.claude/knowledge/paths.toml`) against the tracked files, for CI.

Fails when a tracked file is in more than one layer, when a tracked `.rs` or `.sol` file is in no
layer, or when a pattern matches no tracked file (an exclusion: none of the layer's files). A moved
or new directory must be placed in the table, or `/tycho-review` checks it as `general` without
saying so.

usage: check_paths.py        (anywhere in the repo; exit 1 on any problem)
"""

import os
import subprocess
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from plan_passes import PATHS, glob_rx, in_layer, layer_patterns  # noqa: E402

CODE = (".rs", ".sol")


def main():
    root = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True)
    os.chdir(root.stdout.strip())
    layers = tomllib.loads(Path(PATHS).read_text())["layer"]
    files = subprocess.run(["git", "ls-files"], capture_output=True, text=True, check=True).stdout.split()

    problems = []
    for layer in layers:
        included = layer_patterns([p for p in layer["paths"] if not p.startswith("!")])
        for p in layer["paths"]:
            if p.startswith("!"):
                rx = glob_rx(p[1:])
                hit = any(rx.fullmatch(f) and in_layer(f, included) for f in files)
            else:
                rx = glob_rx(p)
                hit = any(rx.fullmatch(f) for f in files)
            if not hit:
                problems.append(f"{PATHS}: layer {layer['name']}: `{p}` matches no tracked file")

    patterns = [(layer["name"], layer_patterns(layer["paths"])) for layer in layers]
    for f in files:
        names = [name for name, pats in patterns if in_layer(f, pats)]
        if len(names) > 1:
            problems.append(f"{f}: in more than one layer of {PATHS}: {', '.join(names)}")
        elif not names and f.endswith(CODE):
            problems.append(f"{f}: in no layer of {PATHS}")

    for p in problems:
        print(p)
    if problems:
        sys.exit(1)
    print(f"{PATHS}: {len(layers)} layers; every pattern matches, no file is in two layers, "
          f"every {'/'.join(CODE)} file is in one")


if __name__ == "__main__":
    main()
