#!/usr/bin/env bash
# Install the gigahorse decompiler into INSTALL_DIR, at a pinned commit.
#
# usage: gigahorse_setup.sh INSTALL_DIR
#
# macOS (Homebrew): needs `brew install souffle boost z3`, git, uv and Xcode command line tools.
# Builds nothing system-wide: souffle is copied into INSTALL_DIR so that its compile script can
# be fixed there, and the compiler shims in ../gigahorse/macos-shims are used only by this
# script and gigahorse_run.py.
# Linux: needs souffle 2.4.1, boost, z3 and uv (see the upstream README), or on amd64 the
# upstream Docker image.
set -euo pipefail

readonly REPO_URL=https://github.com/nevillegrech/gigahorse-toolchain.git
readonly PINNED_COMMIT=1b06cef82f0a29790e87f576eba6d6d793170d70

skill_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly skill_dir

fail() {
  echo "gigahorse_setup: $*" >&2
  exit 1
}

require_commands() {
  local name
  for name in "$@"; do
    command -v "$name" >/dev/null || fail "'$name' not found on PATH; install it first"
  done
}

check_macos_dependencies() {
  require_commands brew git uv xcrun make
  local missing=()
  local formula
  for formula in souffle boost z3; do
    brew list --versions "$formula" >/dev/null || missing+=("$formula")
  done
  if [ ${#missing[@]} -gt 0 ]; then
    echo "missing Homebrew packages; install them, then run this script again:" >&2
    echo "  brew install ${missing[*]}" >&2
    exit 1
  fi
}

check_linux_dependencies() {
  require_commands git uv make
  if ! command -v souffle >/dev/null; then
    echo "souffle not found. Either install the dependencies (souffle 2.4.1, libboost-all-dev," >&2
    echo "libz3-dev) as in docker/dependencies/souffle24/Dockerfile of $REPO_URL," >&2
    if [ "$(uname -m)" = "x86_64" ]; then
      echo "or run the upstream image ghcr.io/nevillegrech/gigahorse-toolchain:latest by hand" >&2
      echo "(README, 'Installing Gigahorse via docker'); gigahorse_run.py needs a native one." >&2
    fi
    exit 1
  fi
}

clone_pinned() {
  local repo=$1
  if [ ! -d "$repo/.git" ]; then
    git clone --quiet "$REPO_URL" "$repo"
  fi
  git -C "$repo" fetch --quiet origin "$PINNED_COMMIT"
  git -C "$repo" checkout --quiet "$PINNED_COMMIT"
  git -C "$repo" submodule update --quiet --init --recursive
}

# Homebrew's souffle-compile.py names the SDK of the machine that built the bottle and links
# with -ld_classic, which current Xcode linkers reject. souffle looks for the script next to
# its own binary, so the binary is copied next to a fixed script.
install_macos_souffle() {
  local dest=$1
  local brew_souffle sdk
  brew_souffle=$(brew --prefix souffle)
  sdk=$(xcrun --show-sdk-path)
  mkdir -p "$dest/bin"
  cp -f "$brew_souffle/bin/souffle" "$dest/bin/souffle"
  ln -sfn "$brew_souffle/include" "$dest/include"
  sed -E \
    -e "s#/Library/Developer/CommandLineTools/SDKs/MacOSX[0-9.]*\.sdk#$sdk#g" \
    -e "s# ?-Wl,-ld_classic##g; s# ?-ld_classic##g" \
    -e "s#\"includes\": \"#\"includes\": \"-I$brew_souffle/include/souffle #" \
    "$brew_souffle/bin/souffle-compile.py" >"$dest/bin/souffle-compile.py"
  chmod +x "$dest/bin/souffle-compile.py"
}

main() {
  [ $# -eq 1 ] || fail "usage: gigahorse_setup.sh INSTALL_DIR"
  mkdir -p "$1"
  local install_dir
  install_dir=$(cd "$1" && pwd)
  local repo="$install_dir/gigahorse-toolchain"
  case "$install_dir/" in
  "$skill_dir"/*)
    rmdir "$install_dir" 2>/dev/null || true
    fail "install outside the skill directory ($skill_dir)"
    ;;
  esac
  if [ -n "$(ls -A "$install_dir")" ] && [ ! -d "$repo" ]; then
    fail "$install_dir is not empty and holds no gigahorse-toolchain; pick a new directory"
  fi

  case "$(uname -s)" in
  Darwin) check_macos_dependencies ;;
  Linux) check_linux_dependencies ;;
  *) fail "unsupported OS $(uname -s)" ;;
  esac

  clone_pinned "$repo"
  local souffle_bin
  if [ "$(uname -s)" = "Darwin" ]; then
    git -C "$repo" apply --reverse --check "$skill_dir/gigahorse/runners-macos.patch" 2>/dev/null ||
      git -C "$repo" apply "$skill_dir/gigahorse/runners-macos.patch"
    install_macos_souffle "$install_dir/souffle"
    souffle_bin="$install_dir/souffle/bin/souffle"
    export HOMEBREW_PREFIX PATH
    HOMEBREW_PREFIX=$(brew --prefix)
    PATH="$skill_dir/gigahorse/macos-shims:$PATH"
  else
    souffle_bin=$(command -v souffle)
  fi
  local word_size
  word_size=$("$souffle_bin" --version | sed -n 3p | cut -c12,13)
  local build_log="$install_dir/souffle-addon-build.log"
  make -C "$repo/souffle-addon" WORD_SIZE="$word_size" libsoufflenum.so >"$build_log" 2>&1 ||
    fail "building souffle-addon failed; see $build_log"

  mkdir -p "$repo/myclients"
  cp -f "$skill_dir/gigahorse/storage_dump.dl" "$repo/myclients/storage_dump.dl"
  (cd "$repo" && uv sync --quiet --frozen --no-dev)
  "$souffle_bin" --version | sed -n 2p
  echo "installed gigahorse $PINNED_COMMIT in $install_dir"
  echo "the first gigahorse_run.py compiles the Datalog analyses once (3-5 minutes)"
}

main "$@"
