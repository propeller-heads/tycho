#!/bin/bash

# Allows releasing multiple substream packages within the same repo.
# To trigger a release simply create a tag with [package-name]-[semver].
# The script will look for these tags, then infer which package needs to be built and
# released.

# Try to get the tag name associated with the current HEAD commit
current_tag=$(git describe --tags --exact-match HEAD 2>/dev/null)

if [ -n "$current_tag" ]; then
    # If the HEAD is at a tag, extract the prefix and version
    if [[ $current_tag =~ ^([a-zA-Z0-9-]*-)?([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
        # Prefix without the trailing hyphen (if any)
        package="${BASH_REMATCH[1]%?}"
        # Semantic version
        version="v${BASH_REMATCH[2]}"

        cargo_version=$(cargo pkgid -p "$package" | sed -e 's/.*[#@:]//')
        if [[ "v$cargo_version" != "$version" ]]; then
          echo "Error: Cargo version: v${cargo_version} does not match tag version: ${version}!"
          exit 1
        fi
        # Check if the Git repository is dirty
        if [ -n "$(git status --porcelain)" ]; then
          echo "Error: The repository is dirty. Please commit or stash your changes."
          exit 1
        fi
    else
        echo "Error: Current tag ($current_tag) does not match the expected format."
        exit 1
    fi
else
    # If the HEAD is not at a tag, construct the tag name with the pre-release postfix
    if [ -z "$1" ]; then
        echo "Error: package argument is required to create a pre-release!"
        exit 1
    fi
    package=$1

    # Get the short commit hash of the current HEAD
    commit_hash=$(git rev-parse --short HEAD)
    version="pre.${commit_hash}"
fi

chain_name=$(echo "$package" | cut -d'-' -f1)

# The package directory: the argument itself when it names one (nested packages such as
# ethereum-uniswap-v4/no-hooks), otherwise the directory of the Cargo package with that name
# (packages that live below their protocol directory, such as ethereum-ekubo-v3/package).
if [ -d "$package" ]; then
    package_dir="$package"
else
    package_dir=$(realpath --relative-to=. "$(cargo pkgid -p "$package" | sed -e 's|^path+file://||' -e 's|#.*$||')")
fi

# Find all YAML files in the specified package directory if no YAML file input is provided
yaml_files=()
if [ -z "$2" ]; then
    # Check for YAML files in the package directory, filtering by chain name or called substreams.yaml
    yaml_files=($(ls "$package_dir"/*.yaml 2>/dev/null | grep -E "^$package_dir/($chain_name|substreams.yaml)"))
    if [ ${#yaml_files[@]} -eq 0 ]; then
        echo "Error: No YAML files found in the package directory that match the chain name: $chain_name or substreams.yaml."
        exit 1
    fi
else
    yaml_files=("$package_dir/$2.yaml")
fi

set -e  # Exit the script if any command fails
# Build from inside the package directory so rustup picks up the package's own
# rust-toolchain.toml; --locked enforces the committed Cargo.lock. Both are
# required for reproducible wasm builds.
(cd "$package_dir" && cargo build --locked --target wasm32-unknown-unknown --release)
mkdir -p ./target/spkg/

# Loop through each YAML file and build the substreams package
for yaml_file in "${yaml_files[@]}"; do
    # Determine the version prefix based on the YAML file name
    yaml_name=$(basename "$yaml_file" .yaml)
    if [ "$yaml_name" = "buf.gen" ]; then
        continue
    fi
    if [ "$yaml_name" = "substreams" ] && [ ${#yaml_files[@]} -eq 1 ]; then
        version_prefix="$package"
    else
        version_prefix="${yaml_name}"
    fi

    echo "------------------------------------------------------"
    echo "Building substreams package with config: $yaml_file"

    if [[ ! -f "$yaml_file" ]]; then
        echo "Error: manifest reader: unable to stat input file $yaml_file: file does not exist."
        exit 1
    fi

    REPOSITORY=${REPOSITORY:-"s3://repo.propellerheads-propellerheads/substreams"}
    repository_path="$REPOSITORY/$package/$version_prefix-$version.spkg"
    bucket_and_key="${repository_path#s3://}"

    substreams pack "$yaml_file" -o ./target/spkg/$version_prefix-$version.spkg

    if [[ "$version" == pre.* ]]; then
        # Pre-releases are keyed by commit sha and may be rebuilt and overwritten.
        aws s3 cp "./target/spkg/$version_prefix-$version.spkg" "$repository_path"
    else
        # Releases are immutable: the conditional write makes S3 reject the upload
        # atomically if the object already exists. Requires AWS CLI >= 2.17 and
        # only s3:PutObject permission.
        if ! aws s3api put-object \
            --bucket "${bucket_and_key%%/*}" \
            --key "${bucket_and_key#*/}" \
            --body "./target/spkg/$version_prefix-$version.spkg" \
            --if-none-match '*' > /dev/null; then
            echo "Error: upload rejected. A PreconditionFailed error above means $repository_path already exists — releases are immutable, bump the package version instead."
            exit 1
        fi
    fi

    echo "RELEASED SUBSTREAMS PACKAGE: '$repository_path'"
done
