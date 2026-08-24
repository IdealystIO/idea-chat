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

step "1/6  build — client (wasm target check) and server"
cargo check -p hearth --target wasm32-unknown-unknown -q 2>&1 | tail -3
verdict $?
cargo check -p hearth --features server -q 2>&1 | tail -3
verdict $?

step "2/6  unit tests — client model, ids, invite codes"
cargo test -p hearth --lib -q 2>&1 | tail -3
verdict $?

step "3/6  data layer — 41 tests against real Postgres"
HEARTH_TEST_DATABASE_URL="$TEST_DB" cargo test -p hearth-db -q 2>&1 | tail -4
verdict $?

step "4/6  auth flows — 34 assertions over HTTP"
cargo build -p hearth --bin server --features server -q || exit 1
bash tools/reset-dev.sh >/dev/null || exit 1
bash tools/auth-flows.sh | tail -1
verdict $?

step "5/6  message flows — 17 assertions over HTTP"
bash tools/message-flows.sh | tail -1
verdict $?

step "6/6  the whole app — 27 assertions, two browsers"
if [ ! -d crates/hearth/dist/web/pkg ]; then
  echo "  no wasm bundle — run: (cd crates/hearth && idealyst build --web)"
  FAILED=1
else
  bash tools/reset-dev.sh >/dev/null || exit 1
  node tools/hearth-e2e.js | tail -1
  verdict $?
fi

printf '\n'
if [ "$FAILED" = 0 ]; then
  echo "Everything passed. The server is still running on :8090 — open it."
else
  echo "Something failed above."
fi
exit "$FAILED"
