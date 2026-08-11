#!/bin/bash
# Fetch verified source from Etherscan for contracts missing on Sourcify.
# Looks for an API key in env or ~/.env-style files WITHOUT printing it.
set -uo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
cd "$dir/vyper-refs"
addr="$1"
label="$2"
key="${ETHERSCAN_API_KEY:-}"
if [ -z "$key" ] && [ -f "$HOME/.claude/etherscan.key" ]; then
  key=$(cat "$HOME/.claude/etherscan.key")
fi
for f in "$HOME/Projects/propeller-heads/rebalancer-bot/.env" "$HOME/Projects/propeller-heads/tycho/.env"; do
  if [ -z "$key" ] && [ -f "$f" ]; then
    key=$(grep -m1 '^ETHERSCAN_API_KEY=' "$f" | cut -d= -f2- || true)
  fi
done
url="https://api.etherscan.io/v2/api?chainid=1&module=contract&action=getsourcecode&address=$addr"
if [ -n "$key" ]; then
  url="$url&apikey=$key"
  echo "using an API key (not printed)"
else
  echo "no API key found, trying keyless"
fi
curl -s "$url" -o "$label.etherscan.json"
python3 - "$label" <<'EOF'
import json, sys
label = sys.argv[1]
d = json.load(open(label + '.etherscan.json'))
res = d.get('result')
if not isinstance(res, list):
    print('FAILED:', str(d)[:200])
    sys.exit(1)
item = res[0]
src = item.get('SourceCode', '')
if not src:
    print('NO SOURCE:', str(item)[:200])
    sys.exit(1)
open(label + '.vy', 'w').write(src)
print('wrote', label + '.vy', len(src), 'compiler', item.get('CompilerVersion'))
EOF
