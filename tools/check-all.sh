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

step "1/11  build — client (wasm bundle) and server"
# A real bundle, not `cargo check`. Step 6 loads whatever is in dist/web, so a
# check-only step here means the browser test can pass against stale wasm from
# an earlier build — which is exactly how a broken client once went green.
( cd crates/hearth && idealyst build --web ) 2>&1 | tail -2
verdict "${PIPESTATUS[0]}"
run cargo check -p hearth --features server -q

step "2/11  unit tests — client model, ids, invite codes, the DSP, and device I/O"
run cargo test -p hearth --lib -q
# The signal conditioning is testable without a browser and mostly isn't
# tested by one: what a leveller does to a gate, or what a fader does to a
# gain change, is arithmetic. It went unrun here for longer than it should
# have.
# `--features codec` matters: without it the `codec` module is not compiled at
# all, so the encoder/decoder bridge both halves of a call depend on would go
# untested while the step still reported green.
run cargo test -p voice-dsp -q
run cargo test -p voice-proto --features codec -q
# The transport-side crates: the jitter buffer and its concealment, the codec,
# and a real QUIC connection on loopback.
run cargo test -p jitter -p audio-codec -p datagram -q
# Device I/O. Every backend in here is `#[cfg]`-gated to a platform, so what is
# testable on a host is the part that decides cross-platform behaviour: how a
# latency intent becomes a block size, what a pipeline does to a block, and the
# resampler a mixed-rate room depends on.
run cargo test -p audio-io -q
# The relay: room bookkeeping, fan-out over both transports, and a real call
# over QUIC on loopback.
#
# NOT `--lib`. It was, and the integration tests beside it went red the day the
# suppression notice was added — every "did a packet arrive" helper read that
# notice as silence — and stayed red because nothing ran them. A suite that is
# not in this file does not exist.
run cargo test -p voice-server -q

step "3/11  data layer — 41 tests against real Postgres"
TAIL=4 run env HEARTH_TEST_DATABASE_URL="$TEST_DB" cargo test -p hearth-db -q

step "4/11  auth flows — 42 assertions over HTTP"
cargo build -p hearth --bin server --features server -q || exit 1
bash tools/reset-dev.sh >/dev/null || exit 1
TAIL=1 run bash tools/auth-flows.sh

step "5/11  message flows — 17 assertions over HTTP"
TAIL=1 run bash tools/message-flows.sh

step "6/11  the whole app — 39 assertions, two browsers"
if [ ! -d crates/hearth/dist/web/pkg ]; then
  echo "  no wasm bundle — run: (cd crates/hearth && idealyst build --web)"
  FAILED=1
else
  bash tools/reset-dev.sh >/dev/null || exit 1
  TAIL=1 run node tools/hearth-e2e.js
fi

step "7/11  voice — 47 assertions, two browsers and a real relay"
# Skipped rather than failed when the fleet is not up: it is a separate set of
# processes, and a red suite for "you did not start the relay" trains people to
# ignore the colour. The skip is loud and does NOT count as a pass.
if curl -fsS -m 2 http://127.0.0.1:9100/health >/dev/null 2>&1; then
  bash tools/reset-dev.sh >/dev/null || exit 1
  TAIL=1 run node tools/voice-flows.js
else
  echo "  SKIP — no ticket-checking fleet on :9100. Start one with:"
  echo "         bash tools/voice-fleet.sh"
  SKIPPED=1
fi

step "8/11  noise suppression — 22 assertions, both modes, a real relay"
# Needs the same fleet, and needs it to PERMIT server-side suppression —
# tools/voice-fleet.sh sets VOICE_DENOISE=on. A fleet started without it is not
# a failure, it is a different (also supported) configuration, so the suite is
# told which one it is looking at rather than guessing.
if curl -fsS -m 2 http://127.0.0.1:9100/health >/dev/null 2>&1; then
  bash tools/reset-dev.sh >/dev/null || exit 1
  TAIL=1 run node tools/suppression-flows.js
else
  echo "  SKIP — no fleet on :9100. Start one with: bash tools/voice-fleet.sh"
  SKIPPED=1
fi

step "9/11  presence — 8 assertions on the live event stream"
# Judged by what the server SAYS, not what it stores. The database was
# perfectly consistent while the server announced a join on every heartbeat,
# so no assertion about the table could have caught it.
bash tools/reset-dev.sh >/dev/null || exit 1
TAIL=1 run node tools/presence-flows.js

step "10/11  voice activity — 7 assertions against a speech-like fixture"
# Chromium's DEFAULT fake device is a pure tone, which a working suppressor
# removes — so a VAD suite built on it measures the suppressor, not the VAD.
# This one plays tools/fixtures/speech-like.wav, which has a known duty cycle.
if curl -fsS -m 2 http://127.0.0.1:9100/health >/dev/null 2>&1; then
  bash tools/reset-dev.sh >/dev/null || exit 1
  TAIL=1 run node tools/vad-flows.js
else
  echo "  SKIP — no fleet on :9100. Start one with: bash tools/voice-fleet.sh"
  SKIPPED=1
fi

step "11/11  listener volume — 17 assertions, master, per-person, and mute"
# The only stage in the audio chain nobody else can hear. Every silence check
# is paired with a packet count: a volume control implemented by dropping
# packets or closing a sink would pass "it got quieter" and fail "it is still
# connected".
if curl -fsS -m 2 http://127.0.0.1:9100/health >/dev/null 2>&1; then
  bash tools/reset-dev.sh >/dev/null || exit 1
  TAIL=1 run node tools/volume-flows.js
else
  echo "  SKIP — no fleet on :9100. Start one with: bash tools/voice-fleet.sh"
  SKIPPED=1
fi

printf '\n'
if [ "${SKIPPED:-0}" = 1 ] && [ "$FAILED" = 0 ]; then
  echo "Everything that ran passed — but voice was skipped, see above."
elif [ "$FAILED" = 0 ]; then
  echo "Everything passed. The test instance is on :8091; the shared one on :8090 is untouched."
else
  echo "Something failed above."
fi
exit "$FAILED"
