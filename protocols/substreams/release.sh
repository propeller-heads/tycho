#!/bin/bash
#
# Builds and publishes substreams packages (.spkg) to the Tycho registry.
#
# Usage: ./release.sh <package> <config_file>
#
#   package      Cargo package name ("evm-uniswap-v4-with-hooks") or package directory
#                relative to protocols/substreams ("evm-uniswap-v4/with-hooks"). Both
#                forms resolve to the same release.
#   config_file  Manifest name without the .yaml extension ("robinhood-uniswap-v4-with-hooks").
#                Package directories are chain-agnostic, so the manifest is the only thing that
#                says which chain the release is for.
#
# A tag "<cargo-package-name>-<semver>" on HEAD for the requested package produces an
# immutable release; anything else produces a pre-release keyed by the short commit sha. Tags
# for other packages, and the repository's own release tags, are ignored.
#
# Artifacts land at "$REPOSITORY/<package directory>/<manifest name>-<version>.spkg". The
# namespace is the package directory relative to this one, so nested packages get their
# directory path, e.g. "evm-uniswap-v4/with-hooks". Objects already in the registry are never
# moved: spkgs released before a directory rename stay under the old namespace.
#
# RELEASE_DRY_RUN=1 prints the build, pack and upload commands instead of running them. Tag
# and version validation and path resolution still run.

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR
cd "$SCRIPT_DIR"

REPOSITORY=${REPOSITORY:-"s3://repo.propellerheads-propellerheads/substreams"}

die() {
    echo "Error: $*" >&2
    exit 1
}

dry_run() {
    [ "${RELEASE_DRY_RUN:-0}" = "1" ]
}

run() {
    if dry_run; then
        printf 'DRY RUN: %s\n' "$*"
    else
        "$@"
    fi
}

# Prints "<cargo package name>\t<directory>\t<version>" for a package given either as its
# Cargo package name or as its directory relative to this one. The nested Uniswap V4 packages
# are members of this workspace too, so the whole workspace resolves from here.
resolve_package() {
    local input="$1"
    input="${input#./}"
    input="${input%/}"

    command -v jq >/dev/null || die "jq is required to resolve '$input'. Install jq and retry."

    local metadata resolved
    metadata=$(cargo metadata --no-deps --format-version 1)
    resolved=$(jq -r --arg input "$input" '
        .workspace_root as $root
        | .packages[]
        | (.manifest_path | rtrimstr("/Cargo.toml") | ltrimstr($root + "/")) as $dir
        | select(.name == $input or $dir == $input)
        | [.name, $dir, .version]
        | @tsv
    ' <<<"$metadata")

    if [ -z "$resolved" ]; then
        die "Unknown package '$input'. Pass the Cargo package name (e.g." \
            "evm-uniswap-v4-with-hooks) or the package directory (e.g." \
            "evm-uniswap-v4/with-hooks)."
    fi
    if [ "$(wc -l <<<"$resolved")" -ne 1 ]; then
        die "Package '$input' is ambiguous, it matches: $(tr '\n' ' ' <<<"$resolved")"
    fi
    printf '%s\n' "$resolved"
}

# Prints the release tag on HEAD for the given Cargo package, if there is one.
release_tag_for() {
    local want="$1" tag
    while read -r tag; do
        [[ $tag =~ ^(.+)-([0-9]+\.[0-9]+\.[0-9]+)$ ]] || continue
        [ "${BASH_REMATCH[1]}" = "$want" ] || continue
        printf '%s\n' "$tag"
        return 0
    done < <(git tag --points-at HEAD)
    return 0
}

# Copies an spkg to the registry. Reads $version to tell a pre-release from a release.
upload_spkg() {
    local body="$1" destination="$2"

    if [[ "$version" == pre.* ]]; then
        # Pre-releases are keyed by commit sha and may be rebuilt and overwritten.
        run aws s3 cp "$body" "$destination"
        return
    fi

    # Releases are immutable: the conditional write makes S3 reject the upload atomically if
    # the object already exists. Requires AWS CLI >= 2.17 and only s3:PutObject permission.
    local bucket_and_key="${destination#s3://}"
    if dry_run; then
        printf 'DRY RUN: aws s3api put-object --bucket %s --key %s --body %s' \
            "${bucket_and_key%%/*}" "${bucket_and_key#*/}" "$body"
        printf " --if-none-match '*'\n"
        return
    fi
    if ! aws s3api put-object \
        --bucket "${bucket_and_key%%/*}" \
        --key "${bucket_and_key#*/}" \
        --body "$body" \
        --if-none-match '*' >/dev/null; then
        die "upload rejected. A PreconditionFailed error above means $destination already" \
            "exists — releases are immutable, bump the package version instead."
    fi
}

package_input=${1:-}
config_file=${2:-}

if [ -z "$package_input" ] || [ -z "$config_file" ]; then
    die "package and config_file arguments are required. Usage: ./release.sh <package>" \
        "<config_file>"
fi

resolved=$(resolve_package "$package_input")
IFS=$'\t' read -r cargo_package package_dir cargo_version <<<"$resolved"
artifact_namespace="$package_dir"

release_tag=$(release_tag_for "$cargo_package")
if [ -n "$release_tag" ]; then
    tag_version="${release_tag#"$cargo_package"-}"
    if [ "$tag_version" != "$cargo_version" ]; then
        die "Cargo version v$cargo_version does not match tag version v$tag_version (tag" \
            "$release_tag). Bump the package version or fix the tag."
    fi
    if [ -n "$(git status --porcelain)" ]; then
        die "The repository is dirty. Please commit or stash your changes."
    fi
    version="v$cargo_version"
else
    version="pre.$(git rev-parse --short HEAD)"
fi

yaml_file="$package_dir/$config_file.yaml"
if [ ! -f "$yaml_file" ]; then
    die "manifest reader: unable to stat input file $yaml_file: file does not exist."
fi

echo "Cargo package:      $cargo_package"
echo "Package directory:  $package_dir"
echo "Artifact namespace: $artifact_namespace"
echo "Version:            $version"

# Build from inside the package directory so rustup picks up the package's own
# rust-toolchain.toml and cargo uses the lock file of the workspace that owns it; --locked
# enforces that lock. Both are required for reproducible wasm builds.
if dry_run; then
    printf 'DRY RUN: (cd %s && cargo build --locked --target wasm32-unknown-unknown --release)\n' \
        "$package_dir"
else
    (cd "$package_dir" && cargo build --locked --target wasm32-unknown-unknown --release)
fi

run mkdir -p ./target/spkg

echo "------------------------------------------------------"
echo "Building substreams package with config: $yaml_file"

spkg_path="./target/spkg/$config_file-$version.spkg"
repository_path="$REPOSITORY/$artifact_namespace/$config_file-$version.spkg"

run substreams pack "$yaml_file" -o "$spkg_path"
upload_spkg "$spkg_path" "$repository_path"

if dry_run; then
    echo "DRY RUN: would release substreams package: '$repository_path'"
else
    echo "RELEASED SUBSTREAMS PACKAGE: '$repository_path'"
fi
