#!/bin/bash
# Recover the archive RPC URL from the running fee-arb bot's args WITHOUT printing it.
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
pid=$(pgrep -f 'livebot-ethereum.jsonl' | head -1)
if [ -z "$pid" ]; then
  echo "NO BOT PROCESS FOUND"
  exit 1
fi
rpc=$(ps -ww -o args= -p "$pid" | awk '{for(i=1;i<=NF;i++) if($i=="--rpc-url") print $(i+1)}' | head -1)
if [ -z "$rpc" ]; then
  echo "NO RPC ARG FOUND"
  exit 1
fi
printf '%s' "$rpc" > "$dir/.rpc-url"
chmod 600 "$dir/.rpc-url"
echo "RPC recovered to .rpc-url (not printed)"
