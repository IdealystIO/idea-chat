#!/usr/bin/env bash
# Run the Sigil demo: start a sigil-host, run three devices against it, stop
# the host. Ports are out of the way of a running Idea Chat.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build -q -p sigil-host -p sigil-demo

export SIGIL_BIND=127.0.0.1:18091 SIGIL_RELAY_BIND=127.0.0.1:14434
export SIGIL_RELAY_URL=https://127.0.0.1:14434/ SIGIL_NAME=localhost
log=$(mktemp)
./target/debug/sigil-host >"$log" 2>&1 &
host=$!
trap 'kill $host 2>/dev/null || true; rm -f "$log"' EXIT
for _ in $(seq 1 50); do
    curl -s -o /dev/null "http://$SIGIL_BIND/" && break
    sleep 0.1
done

./target/debug/sigil-demo "http://$SIGIL_BIND"
