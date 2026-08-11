#!/bin/bash
# Fetch remaining math + views sources from Sourcify (Etherscan fallback).
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
cd "$dir/vyper-refs"
fetch() {
  local label="$1" addr="$2"
  curl -s "https://sourcify.dev/server/v2/contract/1/$addr?fields=sources" -o "$label.json"
  python3 - "$label" <<'EOF'
import json, sys
label = sys.argv[1]
d = json.load(open(label + '.json'))
srcs = d.get('sources') or {}
for name, src in srcs.items():
    out = label + '_' + name.replace('/', '_')
    open(out, 'w').write(src['content'])
    print(label, '->', out, len(src['content']))
if not srcs:
    print(label, 'MISSING ON SOURCIFY')
EOF
}
fetch twocrypto_math_v210 0x1Fd8Af16DC4BEBd950521308D55d0543b6cDF4A1
fetch twocrypto_math_v010 0x79839c2D74531A8222C0F555865aAc1834e82e51
fetch twocrypto_math_v011 0xBfDdF58Cb6ef84e115fF47c10e49A80B2653EA13
fetch twocrypto_views 0x07CdEBF81977E111B08C126DEFA07818d0045b80
