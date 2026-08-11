#!/bin/bash
# Fetch views + twocrypto pool template + legacy pool sources from Sourcify.
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
rpc="$(cat "$dir/.rpc-url")"
cd "$dir/vyper-refs"

factory=$(cast call --rpc-url "$rpc" 0xf5f5b97624542d72a9e06f04804bf81baa15e2b4 'factory()(address)')
views=$(cast call --rpc-url "$rpc" "$factory" 'views_implementation()(address)')
echo "tricrypto factory=$factory views=$views"

fetch() {
  local label="$1" addr="$2"
  curl -s "https://sourcify.dev/server/v2/contract/1/$addr?fields=sources" -o "$label.json"
  python3 - "$label" <<'EOF'
import json, sys
label = sys.argv[1]
try:
    d = json.load(open(label + '.json'))
    for name, src in d.get('sources', {}).items():
        out = label + '_' + name.replace('/', '_')
        open(out, 'w').write(src['content'])
        print(label, '->', out, len(src['content']))
    if not d.get('sources'):
        print(label, 'NO SOURCES:', json.dumps(d)[:200])
except Exception as e:
    print(label, 'ERROR', e)
EOF
}

fetch tricrypto_views "$views"
fetch twocrypto_pool 0x004C167d27ADa24305b76D80762997Fa6EB8d9B2
fetch tricrypto2_v1_pool 0xd51a44d3fae010294c616388b506acda1bfaae46
fetch crveth_v1_pool 0x8301ae4fc9c624d1d396cbdaa1ed877821d7c511
