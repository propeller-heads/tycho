#!/usr/bin/env bash
#
# Tests the package/version/namespace resolution of release.sh.
#
# The whole substreams tree is copied into a throwaway git repository under $TMPDIR and the
# script is run there with RELEASE_DRY_RUN=1, so no tag, commit or build ever touches the real
# checkout and nothing is uploaded. Set KEEP_WORKDIR=1 to inspect the fixture after a run.

set -euo pipefail

SUBSTREAMS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly SUBSTREAMS_DIR

readonly BUCKET="s3://repo.propellerheads-propellerheads/substreams"
# The namespace release.sh must use for the nested Uniswap V4 packages.
readonly NESTED_NS="ethereum-uniswap-v4/with-hooks"

work=""
repo=""
head_sha=""
output=""
status=0
passed=0
failed=0

cleanup() {
    # $work is always a fresh mktemp -d created by this script.
    if [ -n "$work" ] && [ -d "$work" ] && [ "${KEEP_WORKDIR:-0}" != "1" ]; then
        rm -rf -- "$work"
    fi
}
trap cleanup EXIT

# Copies the substreams tree into a fresh git repository and commits it, so that release.sh
# sees a clean tree and a real HEAD.
setup_fixture() {
    work=$(mktemp -d "${TMPDIR:-/tmp}/release-sh-test.XXXXXX")
    repo="$work/substreams"
    mkdir -p "$repo"
    tar -cf - -C "$SUBSTREAMS_DIR" --exclude=target --exclude=.git . | tar -xf - -C "$repo"

    git -C "$repo" init --quiet
    git -C "$repo" add --all
    git -C "$repo" \
        -c user.email=test@example.com \
        -c user.name="release.sh test" \
        commit --quiet --message "fixture"
    head_sha=$(git -C "$repo" rev-parse --short HEAD)
}

# Replaces every tag on HEAD with the given ones.
set_head_tags() {
    local tag
    while read -r tag; do
        [ -n "$tag" ] || continue
        git -C "$repo" tag --delete "$tag" >/dev/null
    done < <(git -C "$repo" tag --points-at HEAD)
    for tag in "$@"; do
        git -C "$repo" tag "$tag"
    done
}

package_version() {
    grep -m1 '^version' "$repo/$1/Cargo.toml" | cut -d'"' -f2
}

# Runs release.sh in the fixture repo, capturing output and exit status.
release_sh() {
    status=0
    output=$(cd "$repo" && RELEASE_DRY_RUN=1 ./release.sh "$@" 2>&1) || status=$?
}

report() {
    local outcome="$1" description="$2"
    if [ "$outcome" = "ok" ]; then
        passed=$((passed + 1))
        printf '  ok   %s\n' "$description"
    else
        failed=$((failed + 1))
        printf '  FAIL %s\n' "$description"
    fi
}

dump_output() {
    printf -- '--- release.sh output ---\n%s\n--- end ---\n' "$output"
}

assert_contains() {
    local needle="$1" description="$2"
    if [[ "$output" == *"$needle"* ]]; then
        report ok "$description"
    else
        report fail "$description"
        printf '       expected to find: %s\n' "$needle"
        dump_output
    fi
}

assert_status() {
    local expected="$1" description="$2"
    if [ "$status" -eq "$expected" ]; then
        report ok "$description"
    else
        report fail "$description"
        printf '       expected exit %s, got %s\n' "$expected" "$status"
        dump_output
    fi
}

assert_failed() {
    local description="$1"
    if [ "$status" -ne 0 ]; then
        report ok "$description"
    else
        report fail "$description"
        printf '       expected a non-zero exit\n'
        dump_output
    fi
}

test_prerelease_nested_by_directory() {
    echo "a) pre-release, nested package given as a directory"
    set_head_tags
    release_sh ethereum-uniswap-v4/with-hooks robinhood-uniswap-v4-with-hooks
    assert_status 0 "exits successfully"
    assert_contains "-o ./target/spkg/robinhood-uniswap-v4-with-hooks-pre.$head_sha.spkg" \
        "packs to the manifest-named spkg"
    assert_contains "'$BUCKET/$NESTED_NS/robinhood-uniswap-v4-with-hooks-pre.$head_sha.spkg'" \
        "uploads under the package directory namespace"
    assert_contains "cd ethereum-uniswap-v4/with-hooks && cargo build" \
        "builds from the package directory"
}

test_prerelease_nested_by_cargo_name() {
    echo "b) pre-release, same package given as its Cargo package name"
    set_head_tags
    release_sh ethereum-uniswap-v4-with-hooks robinhood-uniswap-v4-with-hooks
    assert_status 0 "exits successfully"
    assert_contains "-o ./target/spkg/robinhood-uniswap-v4-with-hooks-pre.$head_sha.spkg" \
        "packs to the same spkg as the directory form"
    assert_contains "'$BUCKET/$NESTED_NS/robinhood-uniswap-v4-with-hooks-pre.$head_sha.spkg'" \
        "uploads to the same object as the directory form"
}

test_tagged_nested() {
    echo "c) tagged release of the nested package"
    local version
    version=$(package_version ethereum-uniswap-v4/with-hooks)
    set_head_tags "ethereum-uniswap-v4-with-hooks-$version"
    release_sh ethereum-uniswap-v4/with-hooks robinhood-uniswap-v4-with-hooks
    assert_status 0 "exits successfully"
    assert_contains "-o ./target/spkg/robinhood-uniswap-v4-with-hooks-v$version.spkg" \
        "packs the tag version"
    assert_contains "'$BUCKET/$NESTED_NS/robinhood-uniswap-v4-with-hooks-v$version.spkg'" \
        "uploads to the same namespace as the pre-release"
    assert_contains "--key substreams/$NESTED_NS/robinhood-uniswap-v4-with-hooks-v$version.spkg" \
        "uploads with a conditional write key"
    assert_contains "--if-none-match" "keeps releases immutable"
}

test_tagged_version_mismatch() {
    echo "d) tag version that does not match Cargo.toml"
    set_head_tags "ethereum-uniswap-v4-with-hooks-99.0.0"
    release_sh ethereum-uniswap-v4/with-hooks robinhood-uniswap-v4-with-hooks
    assert_failed "refuses to release"
    assert_contains "does not match tag version" "explains the mismatch"
}

test_prerelease_flat() {
    echo "e) pre-release of a flat package"
    set_head_tags
    release_sh ethereum-uniswap-v2 ethereum-pancakeswap-v2
    assert_status 0 "exits successfully"
    assert_contains "-o ./target/spkg/ethereum-pancakeswap-v2-pre.$head_sha.spkg" \
        "packs to the manifest-named spkg"
    assert_contains "'$BUCKET/ethereum-uniswap-v2/ethereum-pancakeswap-v2-pre.$head_sha.spkg'" \
        "keeps the flat package namespace"
}

test_tagged_flat() {
    echo "f) tagged release of a flat package keeps its published namespace"
    local version
    version=$(package_version ethereum-uniswap-v2)
    set_head_tags "ethereum-uniswap-v2-$version"
    release_sh ethereum-uniswap-v2 ethereum-pancakeswap-v2
    assert_status 0 "exits successfully"
    assert_contains "'$BUCKET/ethereum-uniswap-v2/ethereum-pancakeswap-v2-v$version.spkg'" \
        "uploads where the existing Helm values point"
}

test_flat_package_with_only_substreams_yaml() {
    echo "g) flat package whose only manifest is substreams.yaml"
    local version
    version=$(package_version ethereum-fluid)

    # ethereum-fluid is the one flat package whose Cargo name (ethereum-fluid_v1) differs from
    # its directory, so it pins the rule that the artifact is named after the directory.
    set_head_tags
    release_sh ethereum-fluid
    assert_status 0 "exits successfully"
    assert_contains "Cargo package:      ethereum-fluid_v1" "resolves the Cargo package name"
    assert_contains "-o ./target/spkg/ethereum-fluid-pre.$head_sha.spkg" \
        "packs under the directory name, not the Cargo name"
    assert_contains "'$BUCKET/ethereum-fluid/ethereum-fluid-pre.$head_sha.spkg'" \
        "keeps the published pre-release key"

    set_head_tags "ethereum-fluid_v1-$version"
    release_sh ethereum-fluid
    assert_status 0 "exits successfully when tagged with the Cargo package name"
    assert_contains "-o ./target/spkg/ethereum-fluid-v$version.spkg" \
        "packs the tag version under the directory name"
    assert_contains "'$BUCKET/ethereum-fluid/ethereum-fluid-v$version.spkg'" \
        "keeps the published release key"
}

test_unrelated_tag_is_ignored() {
    echo "h) an unrelated repository tag on HEAD does not start a release"
    set_head_tags "0.410.0" "ethereum-uniswap-v2-0.0.1"
    release_sh ethereum-uniswap-v4/with-hooks robinhood-uniswap-v4-with-hooks
    assert_status 0 "exits successfully"
    assert_contains "robinhood-uniswap-v4-with-hooks-pre.$head_sha.spkg" \
        "falls back to a pre-release"
}

test_unknown_package() {
    echo "i) unknown package"
    set_head_tags
    release_sh ethereum-uniswap-v4/no-such-crate robinhood-uniswap-v4-with-hooks
    assert_failed "refuses to release"
    assert_contains "Unknown package" "names the bad input"
}

test_dry_run_leaves_no_artifacts() {
    echo "j) the dry run leaves the fixture clean"
    local dirty
    dirty=$(git -C "$repo" status --porcelain)
    if [ -z "$dirty" ]; then
        report ok "no files created or modified"
    else
        report fail "no files created or modified"
        printf '       git status:\n%s\n' "$dirty"
    fi
}

main() {
    setup_fixture
    echo "Fixture: $repo (HEAD $head_sha)"
    echo

    test_prerelease_nested_by_directory
    test_prerelease_nested_by_cargo_name
    test_tagged_nested
    test_tagged_version_mismatch
    test_prerelease_flat
    test_tagged_flat
    test_flat_package_with_only_substreams_yaml
    test_unrelated_tag_is_ignored
    test_unknown_package
    test_dry_run_leaves_no_artifacts

    echo
    echo "passed: $passed, failed: $failed"
    [ "$failed" -eq 0 ]
}

main "$@"
