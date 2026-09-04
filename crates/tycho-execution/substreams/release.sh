#!/bin/bash
# Builds one .spkg per chain and uploads it to S3, where the sink containers fetch it from.
#
# Forked from protocols/substreams/release.sh and follows the same rules:
#   * the version comes from tycho-router-trades/Cargo.toml
#   * a release needs a git tag `tycho-router-trades-<semver>` on HEAD whose version matches, and
#     a clean tree. Releases are immutable: S3 rejects a key that exists
#   * without such a tag the run publishes a pre-release keyed by the commit, which may be
#     overwritten
#
# The module hash of every package is printed. It is the identity the sink cursors are keyed by,
# so a diff there is the thing to check before pinning a new version in helm.
#
#   ./release.sh                # every chain
#   ./release.sh ethereum bsc   # only these
set -euo pipefail

cd "$(dirname "$0")"
package=tycho-router-trades

current_tag=$(git describe --tags --exact-match HEAD 2>/dev/null || true)
cargo_version=$(cargo pkgid -p "$package" | cut -d# -f2 | cut -d: -f2)

if [ -n "$current_tag" ]; then
	if [[ ! $current_tag =~ ^${package}-([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
		echo "Error: tag '$current_tag' is not ${package}-<semver>." >&2
		exit 1
	fi
	version="v${BASH_REMATCH[1]}"
	if [[ "v$cargo_version" != "$version" ]]; then
		echo "Error: Cargo version v${cargo_version} does not match tag version ${version}." >&2
		exit 1
	fi
	if [ -n "$(git status --porcelain)" ]; then
		echo "Error: the repository is dirty. Commit or stash your changes." >&2
		exit 1
	fi
else
	version="pre.$(git rev-parse --short HEAD)"
	echo "No ${package}-<semver> tag on HEAD, publishing pre-release $version"
fi

chains=("$@")
if [ ${#chains[@]} -eq 0 ]; then
	for manifest in "$package"/chains/*.yaml; do
		chains+=("$(basename "$manifest" .yaml)")
	done
fi

# --locked enforces the committed Cargo.lock and the build runs in this workspace so its own
# rust-toolchain.toml applies. Both are needed for a reproducible wasm, and the wasm is part of
# the module hash.
cargo build --locked --target wasm32-unknown-unknown --release --target-dir target
mkdir -p target/spkg

REPOSITORY=${REPOSITORY:-"s3://repo.propellerheads-propellerheads/substreams"}

for chain in "${chains[@]}"; do
	manifest="$package/chains/$chain.yaml"
	if [ ! -f "$manifest" ]; then
		echo "Error: no manifest for chain '$chain' at $manifest." >&2
		exit 1
	fi

	spkg="target/spkg/$chain-$version.spkg"
	repository_path="$REPOSITORY/$package/$chain-$version.spkg"
	bucket_and_key="${repository_path#s3://}"

	echo "------------------------------------------------------"
	substreams pack "$manifest" -o "$spkg"
	echo "module hash: $(substreams info "$spkg" | awk '/^Name: db_out$/{found=1} found && /^Hash:/{print $2; exit}')"

	if [[ "$version" == pre.* ]]; then
		aws s3 cp "$spkg" "$repository_path"
	else
		if ! aws s3api put-object \
			--bucket "${bucket_and_key%%/*}" \
			--key "${bucket_and_key#*/}" \
			--body "$spkg" \
			--if-none-match '*' >/dev/null; then
			echo "Error: upload rejected. A PreconditionFailed error above means $repository_path already exists — releases are immutable, bump the package version instead." >&2
			exit 1
		fi
	fi

	echo "RELEASED SUBSTREAMS PACKAGE: '$repository_path'"
done
