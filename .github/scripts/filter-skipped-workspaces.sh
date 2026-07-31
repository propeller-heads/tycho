#!/usr/bin/env bash
# Reads NUL-delimited file paths from stdin and emits only those whose
# ancestor directories do NOT contain a .cargo-update-skip marker file.
# Usage: find … -print0 | ./filter-skipped-workspaces.sh
set -euo pipefail

while IFS= read -r -d '' path; do
  dir=$(dirname "$path")
  skip=false
  while [[ "$dir" != "." && "$dir" != "/" ]]; do
    if [[ -f "$dir/.cargo-update-skip" ]]; then
      skip=true
      break
    fi
    dir=$(dirname "$dir")
  done
  if [[ "$skip" == false ]]; then
    printf '%s\0' "$path"
  fi
done
