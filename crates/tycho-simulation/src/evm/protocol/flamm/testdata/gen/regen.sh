#!/bin/sh
# Copyright (c) 2026 Everlong Labs Limited
# Re-packs the end-to-end fixtures from the fetched stages (out/stages/, fetch.py + morpho_events.py),
# regenerates the stream fixture through the base-flamm package's own test and prints the digests to pin in
# tests/fixtures.rs and the two testdata READMEs. Run from anywhere; needs cargo and python3 with pycryptodome.
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../../../../../.." && pwd)
SIM="$REPO/crates/tycho-simulation/src/evm/protocol/flamm"
python3 "$HERE/pack.py"
cd "$REPO/protocols/substreams"
FLAMM_E2E_WRITE="$SIM/testdata/snapshots/e2e_stream.json.gz" cargo test -p base-flamm e2e_stream_fixture_is_the_package_output
cargo test -p base-flamm e2e_stream_fixture_is_the_package_output
cd "$SIM/testdata/snapshots"
for f in e2e_stream.json.gz e2e_grids.json.gz; do
  printf '%s stored %s uncompressed %s\n' "$f" "$(shasum -a 256 "$f" | cut -d' ' -f1)" "$(gunzip -c "$f" | shasum -a 256 | cut -d' ' -f1)"
done
