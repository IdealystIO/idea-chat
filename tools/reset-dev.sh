#!/usr/bin/env bash
# A fresh instance for the test suites, on their OWN database and port.
#
# Separate from the shared instance on purpose, and this is not tidiness. The
# claim flow works exactly once, so every end-to-end run needs a database with
# no accounts in it — and for a long time that database was `hearth_dev`, the
# one the shared instance runs on. Every test run therefore deleted whoever was
# using it, and restarted their server without the public origin in the CSRF
# allowlist, which presents as "I can't connect" rather than as anything to do
# with tests.
#
#   tests            :8091   hearth_test    wiped freely
#   shared instance  :8090   hearth_dev     never touched by this script
#
# HEARTH_ORIGINS is deliberately NOT set: the server derives every spelling of
# loopback from the port it bound, and that derivation is itself under test
# (auth-flows checks all three of localhost, 127.0.0.1 and [::1]). Setting it
# here to two of the three made the suite fail on the one I forgot.
set -u
cd "$(dirname "$0")/.."

PORT=${HEARTH_TEST_PORT:-8091}
DB=${HEARTH_TEST_DB:-hearth_test}
PIDFILE=/tmp/hearth-test-server.pid

# Kill only OUR server. `pkill -x server` would take the shared instance with
# it, which is exactly the class of mistake this file exists to stop.
if [ -f "$PIDFILE" ]; then
  kill -9 "$(cat "$PIDFILE")" 2>/dev/null
  rm -f "$PIDFILE"
fi
sleep 0.5

docker exec idea-chat-postgres-1 psql -U postgres \
  -c "DROP DATABASE IF EXISTS $DB" -c "CREATE DATABASE $DB" >/dev/null

DATABASE_URL=postgres://postgres:dev@localhost:55432/$DB \
  HEARTH_BIND=127.0.0.1:$PORT REDIS_URL=redis://localhost:6399 \
  VOICE_DIRECTOR_URL=${VOICE_DIRECTOR_URL:-http://127.0.0.1:9100} \
  VOICE_SHARED_SECRET=${VOICE_SHARED_SECRET:-a-shared-secret-for-local-testing} \
  ./target/debug/server > /tmp/hearth-server.log 2>&1 &
echo $! > "$PIDFILE"

for _ in $(seq 1 30); do
  grep -q 'Claim it' /tmp/hearth-server.log && { echo "hearth: fresh test instance on :$PORT"; exit 0; }
  sleep 0.3
done
echo "hearth: test server did not start"; cat /tmp/hearth-server.log; exit 1
