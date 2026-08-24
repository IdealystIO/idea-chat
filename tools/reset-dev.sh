#!/usr/bin/env bash
# Drop and recreate the dev database, then restart the server on a fresh one.
# The claim flow works once, so every end-to-end run needs a clean instance.
set -u
for p in $(pgrep -x server); do kill -9 "$p" 2>/dev/null; done
sleep 1
docker exec idea-chat-postgres-1 psql -U postgres \
  -c "DROP DATABASE IF EXISTS hearth_dev" -c "CREATE DATABASE hearth_dev" >/dev/null
DATABASE_URL=postgres://postgres:dev@localhost:55432/hearth_dev \
  HEARTH_BIND=127.0.0.1:8090 REDIS_URL=redis://localhost:6399 \
  ./target/debug/server > /tmp/hearth-server.log 2>&1 &
for _ in $(seq 1 30); do
  grep -q 'Claim it' /tmp/hearth-server.log && { echo "hearth: fresh instance on :8090"; exit 0; }
  sleep 0.3
done
echo "hearth: server did not start"; cat /tmp/hearth-server.log; exit 1
