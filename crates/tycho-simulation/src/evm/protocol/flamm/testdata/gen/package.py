# Copyright (c) 2026 Everlong Labs Limited
"""Copies the recorded snapshots (out/grids/<block>.json) gzipped into the tycho-simulation flamm testdata
(../snapshots/<block>.json.gz, deterministic gzip) and prints the digest table for tests/fixtures.rs and
testdata/README.md.

Usage: python3 package.py <block>..."""
import gzip
import hashlib
import io
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(os.path.dirname(HERE), "snapshots")


def gz_bytes(raw):
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", mtime=0) as f:
        f.write(raw)
    return buf.getvalue()


def main(blocks):
    os.makedirs(OUT, exist_ok=True)
    rows = []
    for b in blocks:
        raw = open(os.path.join(HERE, "out", "grids", "%s.json" % b), "rb").read()
        stored = gz_bytes(raw)
        name = "%s.json.gz" % b
        with open(os.path.join(OUT, name), "wb") as f:
            f.write(stored)
        rows.append((name, hashlib.sha256(stored).hexdigest(), hashlib.sha256(raw).hexdigest(), len(stored)))
    for name in sorted(os.listdir(OUT)):
        if name.endswith(".json.gz") and not any(r[0] == name for r in rows):
            stored = open(os.path.join(OUT, name), "rb").read()
            raw = gzip.decompress(stored)
            rows.append((name, hashlib.sha256(stored).hexdigest(), hashlib.sha256(raw).hexdigest(), len(stored)))
    rows.sort()
    for name, s, u, n in rows:
        print("    (\"snapshots/%s\", \"%s\")," % (name, s))
    print()
    for name, s, u, n in rows:
        print("| `snapshots/%s` | %d | `%s` | `%s` |" % (name, n, s, u))


if __name__ == "__main__":
    main(sys.argv[1:])
