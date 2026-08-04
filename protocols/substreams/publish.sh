#!/bin/bash

# Manual publish script for substream packages.
# Builds, packs, and publishes a substream package to the public S3 repository.
# Usage: ./publish.sh <package-name> <yaml-name>
#   package-name: The name of the package to build
#   yaml-name: The name of the YAML file (without .yaml extension)
# The version will be automatically fetched from the package's Cargo.toml

# Check if required arguments are provided
if [ -z "$1" ] || [ -z "$2" ]; then
    echo "Error: package name and yaml name are required!"
    echo "Usage: $0 <package-name> <yaml-name>"
    exit 1
fi

package=$1
yaml_name=$2

# Resolve package metadata from Cargo.toml (supports nested package paths)
cargo_toml="./$package/Cargo.toml"
if [[ ! -f "$cargo_toml" ]]; then
    echo "Error: Cargo.toml not found at $cargo_toml"
    exit 1
fi

cargo_package_name=$(awk '
    /^\[package\]/ { in_package=1; next }
    /^\[/ && in_package { in_package=0 }
    in_package && $1 == "name" {
        gsub(/"/, "", $3)
        print $3
        exit
    }
' "$cargo_toml")

cargo_version=$(awk '
    /^\[package\]/ { in_package=1; next }
    /^\[/ && in_package { in_package=0 }
    in_package && $1 == "version" {
        gsub(/"/, "", $3)
        print $3
        exit
    }
' "$cargo_toml")

if [[ -z "$cargo_package_name" || -z "$cargo_version" ]]; then
    echo "Error: Unable to parse package name/version from $cargo_toml"
    exit 1
fi

version="v${cargo_version}"

# Construct the YAML file path
yaml_file="./$package/$yaml_name.yaml"

# Validate that the YAML file exists
if [[ ! -f "$yaml_file" ]]; then
    echo "Error: manifest reader: unable to stat input file $yaml_file: file does not exist."
    exit 1
fi

# Determine the artifact name based on the YAML file name. The manifest name identifies the
# chain and fork being published, which the source directory does not: packing
# robinhood-uniswap-v4-no-hooks.yaml from ethereum-uniswap-v4/no-hooks must not publish under
# "ethereum". Fall back to the Cargo package name, which flattens nested package paths, so no
# "/" ever leaks into the S3 key.
if [ "$yaml_name" = "substreams" ]; then
    version_prefix="$cargo_package_name"
else
    version_prefix="${yaml_name}"
fi

REPOSITORY=${REPOSITORY:-"s3://repo.propellerheads-propellerheads/substreams"}
repository_path="$REPOSITORY/$version_prefix/$version_prefix-$version.spkg"
output_file="./target/spkg/$version_prefix-$version.spkg"

set -e  # Exit the script if any command fails

echo ""
echo "Substreams package: $package"
echo "YAML config: $yaml_file"
echo "Version: $version"
echo "Destination: $repository_path"
echo ""

# Ask for confirmation before proceeding
read -p "Is this information correct? Do you want to proceed? (y/N): " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "Aborted."
    exit 0
fi

set -e  # Exit the script if any command fails

# Build the package (using resolved Cargo package name)
cargo build --target wasm32-unknown-unknown --release -p "$cargo_package_name"

# Create output directory
mkdir -p ./target/spkg/

# Pack and upload the substreams package
substreams pack "$yaml_file" -o "$output_file"

# Check if the file already exists in S3 before uploading
# Extract bucket and key from the S3 path (format: s3://bucket/key)
s3_path="${repository_path#s3://}"
bucket="${s3_path%%/*}"
key="${s3_path#*/}"

if aws s3api head-object --bucket "$bucket" --key "$key" >/dev/null 2>&1; then
    echo "Error: File already exists in S3: $repository_path"
    echo "Upload aborted to prevent overwriting existing file."
    exit 1
fi

aws s3 cp "$output_file" "$repository_path"

echo "------------------------------------------------------"
echo "PUBLISHED SUBSTREAMS PACKAGE: '$repository_path'"
echo "------------------------------------------------------"

