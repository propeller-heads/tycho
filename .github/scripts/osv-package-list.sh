#!/usr/bin/env bash
set -euo pipefail

# Prints the crates.io packages that the workspace builds, in osv-scanner's custom package-list
# format, for `osv-scanner --lockfile=osv-scanner:<file>`. The format is documented at
# https://google.github.io/osv-scanner/supported-languages-and-lockfiles/#custom-lockfiles.
# A package without an `ecosystem` is skipped as unscannable, with only an info-level log line.
#
# osv-scanner reads Cargo.lock as is, and Cargo.lock also lists optional dependencies of our
# dependencies that no feature enables, so a lockfile scan reports crates we never compile
# (google/osv-scanner#654). `cargo tree` resolves features instead.
#
# Usage: osv-package-list.sh >osv-package-list.json

# cargo tree covers normal, build and dev-dependencies (which CI compiles and runs) by default.
tree_args=(
    # Fail instead of updating Cargo.lock.
    --locked
    # Every member, wherever the script runs; from a member's directory cargo tree covers only that
    # member.
    --workspace
    # Dependencies for every platform, not only the host's.
    --target all
    # Everything a downstream user can enable through our crates.
    --all-features
    # One package per line instead of a drawn tree.
    --prefix none
)

to_package_list=$(
    cat <<'EOF'
# Each line of cargo tree's output is `name vX.Y.Z`, then the source unless it is crates.io
# (` (/path)` for local crates such as the workspace members, ` (<url>#<rev>)` for git), then
# ` (proc-macro)` for proc macros and ` (*)` for subtrees already printed.
#
# Only crates.io packages are listed. Local crates are our own code. Any other source is an error:
# labelled crates.io, a git or alternative-registry package would be matched against the advisories
# of the crates.io crate with the same name and version.
def package:
    split(" ")
    | .[2] as $marker
    | if $marker | IN(null, "(proc-macro)", "(*)") then
        {name: .[0], version: .[1][1:], ecosystem: "crates.io"}
      elif $marker | startswith("(/") then
        empty
      else
        error("not a crates.io or local package: \(join(" "))")
      end;

# Blank lines separate the members' trees. A package appears once per place it is reached.
[inputs | select(length > 0) | package]
| unique
| {results: [{packages: map({package: .})}]}
EOF
)

cargo tree "${tree_args[@]}" | jq -R -n "$to_package_list"
