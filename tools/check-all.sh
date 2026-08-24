#!/usr/bin/env bash
# Run everything that can be verified, in order, and say what passed.
#
# Needs Postgres and Redis up (`docker compose up -d postgres redis`) and the
# `hearth` database created once:
#   docker exec idea-chat-postgres-1 psql -U postgres -c 'CREATE DATABASE hearth'
#
# Each suite resets the dev instance where it needs a fresh one — the claim
# flow works exactly once, so auth-flows and the browser run cannot share a
# database.
set -u
cd "$(dirname "$0")/.."

TEST_DB=${TEST_DB:-postgres://postgres:dev@localhost:55432/hearth}
FAILED=0
step() { printf '\n\033[1m%s\033[0m\n' "$1"; }
verdict() { if [ "$1" = 0 ]; then echo "  PASS"; else echo "  FAIL"; FAILED=1; fi; }
# Every step below pipes into `tail`, and `$?` after a pipeline is the exit
# status of the LAST command in it — `tail`, which succeeds no matter what the
# suite did. This reported "Everything passed" while five browser checks were
# failing. Always take the status of the command that actually ran the tests.
run() { "$@" 2>&1 | tail -"${TAIL:-3}"; verdict "${PIPESTATUS[0]}"; }

step "1/6  build — client (wasm bundle) and server"
# A real bundle, not `cargo check`. Step 6 loads whatever is in dist/web, so a
# check-only step here means the browser test can pass against stale wasm from
# an earlier build — which is exactly how a broken client once went green.
( cd crates/hearth && idealyst build --web ) 2>&1 | tail -2
verdict "${PIPESTATUS[0]}"
run cargo check -p hearth --features server -q

step "2/6  unit tests — client model, ids, invite codes"
run cargo test -p hearth --lib -q

step "3/6  data layer — 41 tests against real Postgres"
TAIL=4 run env HEARTH_TEST_DATABASE_URL="$TEST_DB" cargo test -p hearth-db -q

step "4/6  auth flows — 34 assertions over HTTP"
cargo build -p hearth --bin server --features server -q || exit 1
bash tools/reset-dev.sh >/dev/null || exit 1
TAIL=1 run bash tools/auth-flows.sh

step "5/6  message flows — 17 assertions over HTTP"
TAIL=1 run bash tools/message-flows.sh

step "6/6  the whole app — 27 assertions, two browsers"
if [ ! -d crates/hearth/dist/web/pkg ]; then
  echo "  no wasm bundle — run: (cd crates/hearth && idealyst build --web)"
  FAILED=1
else
  bash tools/reset-dev.sh >/dev/null || exit 1
  TAIL=1 run node tools/hearth-e2e.js
fi

printf '\n'
if [ "$FAILED" = 0 ]; then
  echo "Everything passed. The server is still running on :8090 — open it."
else
  echo "Something failed above."
fi
exit "$FAILED"
