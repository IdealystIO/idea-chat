#!/usr/bin/env bash
# Start a local voice fleet that REQUIRES room tickets — the configuration a
# real deployment runs, and the one `voice-flows.js` tests against.
#
# Deliberately separate from the docker fleet on :8787-:8789, which comes up
# without a secret for the standalone loopback demo. Two fleets rather than one
# because "admits anyone" and "checks tickets" are different systems, and a
# test that silently ran against the open one would prove nothing.
set -u
cd "$(dirname "$0")/.."

SECRET=${VOICE_SHARED_SECRET:-a-shared-secret-for-local-testing}
DIRECTOR_PORT=${DIRECTOR_PORT:-9100}
NODE_PORT=${NODE_PORT:-8797}
COORD_DB=${COORD_DB:-voice_local}

cargo build -p voice-server --bins -q || exit 1

# Its own coordination database: the docker fleet's nodes are registered in
# `voice`, and a director sharing that store would happily place a room on a
# node that is not checking tickets.
docker exec idea-chat-postgres-1 psql -U postgres \
  -c "SELECT 1 FROM pg_database WHERE datname='$COORD_DB'" 2>/dev/null | grep -q 1 || \
  docker exec idea-chat-postgres-1 psql -U postgres -c "CREATE DATABASE $COORD_DB" >/dev/null

for p in $(pgrep -x voice-director) $(pgrep -x voice-node); do kill "$p" 2>/dev/null; done
sleep 1

export COORD_BACKEND=postgres
export POSTGRES_URL="postgres://postgres:dev@localhost:55432/$COORD_DB"
export VOICE_SHARED_SECRET="$SECRET"
# Server-side noise suppression, permitted. Off by default in the binary — a
# node should relay unless someone decided otherwise — so the local fleet turns
# it on explicitly, which is also what makes `suppression-flows.js` able to
# test the granted path AND the refused one (start without this to get the
# refusal).
export VOICE_DENOISE=${VOICE_DENOISE:-on}
# WebTransport, on a UDP port beside the node's TCP one. Opt-in per node, so
# the local fleet turns it on explicitly — and `voice-flows.js` then asserts a
# browser can actually reach it, which is the one claim a native test cannot
# make.
export VOICE_WEBTRANSPORT=${VOICE_WEBTRANSPORT:-8798}

PORT=$DIRECTOR_PORT nohup ./target/debug/voice-director > /tmp/voice-director-local.log 2>&1 &
sleep 2
PORT=$NODE_PORT NODE_ID=node-local PUBLIC_URL="ws://127.0.0.1:$NODE_PORT" \
  nohup ./target/debug/voice-node > /tmp/voice-node-local.log 2>&1 &

for _ in $(seq 1 30); do
  if curl -fsS -m 1 "http://127.0.0.1:$DIRECTOR_PORT/health" >/dev/null 2>&1 &&
     curl -fsS -m 1 "http://127.0.0.1:$NODE_PORT/health" >/dev/null 2>&1; then
    echo "voice: director :$DIRECTOR_PORT, node :$NODE_PORT — tickets required"
    exit 0
  fi
  sleep 0.3
done
echo "voice: fleet did not start"; tail -5 /tmp/voice-node-local.log; exit 1
