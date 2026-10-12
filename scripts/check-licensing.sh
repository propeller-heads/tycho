#!/usr/bin/env bash
# Fails if first-party packages drift from the Fynd License 1.1 (see LICENSING.md).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

errors=0
fail() {
    echo "error: $*" >&2
    errors=$((errors + 1))
}

expected_title="# Fynd License 1.1"
if [[ "$(head -n 1 LICENSE.md)" != "$expected_title" ]]; then
    fail "LICENSE.md must start with '$expected_title'"
fi

# Every package in the main workspace must point at LICENSE.md and declare no SPDX license.
while IFS=$'\t' read -r name license license_file; do
    if [[ "$license" != "null" ]]; then
        fail "crate $name declares license = \"$license\"; use license-file.workspace = true"
    fi
    if [[ "$license_file" != */LICENSE.md ]]; then
        fail "crate $name must set license-file to LICENSE.md (found: $license_file)"
    fi
done < <(cargo metadata --no-deps --format-version 1 |
    jq -r '.packages[] | [.name, (.license // "null"), (.license_file // "null")] | @tsv')

# Published substreams crates live in a separate WASM workspace.
for manifest in protocols/substreams/crates/tycho-substreams/Cargo.toml \
    protocols/substreams/crates/substreams-helper/Cargo.toml; do
    if grep -qE '^license[[:space:]]*=' "$manifest"; then
        fail "$manifest declares an SPDX license; use license-file"
    fi
    if ! grep -qE '^license-file[[:space:]]*=[[:space:]]*"(\.\./)+LICENSE\.md"' "$manifest"; then
        fail "$manifest must set license-file to LICENSE.md"
    fi
done

# Exclude this script, which contains the pattern itself.
busl_pattern='SPDX-License-Identifier: BUSL-1.1'
if git grep -q "$busl_pattern" -- ':!scripts/check-licensing.sh'; then
    git grep -l "$busl_pattern" -- ':!scripts/check-licensing.sh' >&2
    fail "files above still carry BUSL-1.1; use LicenseRef-Fynd-License-1.1"
fi

if grep -q 'MIT' crates/tycho-client-py/pyproject.toml; then
    fail "crates/tycho-client-py/pyproject.toml still references MIT"
fi

if ((errors > 0)); then
    echo "$errors licensing check(s) failed. See LICENSING.md." >&2
    exit 1
fi
echo "Licensing checks passed."
