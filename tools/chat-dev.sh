#!/usr/bin/env bash
# Run Idea Chat locally: Postgres, Redis and MinIO in Docker; the web app
# built; the server on http://localhost:8090.
#
#   tools/chat-dev.sh            # build everything and run
#   tools/chat-dev.sh --no-web   # skip rebuilding the web app
#
# Emails (verification codes) are printed in this terminal: look for
# "[mail] to you@example.com — 123456 is your Idea Chat code".
set -euo pipefail
cd "$(dirname "$0")/.."

docker compose -f deploy/chat/docker-compose.yml up -d postgres redis minio
echo "waiting for Postgres…"
until docker compose -f deploy/chat/docker-compose.yml exec -T postgres pg_isready -U postgres -d chat >/dev/null 2>&1; do sleep 1; done

if [[ "${1:-}" != "--no-web" ]]; then
  # web_sys's WebTransport bindings need this cfg (see .cargo/config.toml).
  ( cd crates/chat && RUSTFLAGS=--cfg=web_sys_unstable_apis idealyst build --web )
fi

cargo build -p chat-server

export REDIS_URL=redis://localhost:56379
export AWS_ACCESS_KEY_ID=minio
export AWS_SECRET_ACCESS_KEY=minio-dev-secret
# Fixed in development so sessions survive a restart; production sets its own.
export CALL_SECRET=dev-call-secret-change-me-0123456789
# One name across restarts, so a restart clears the previous run's connections
# (and the calls they were in) at start. The default, hostname-pid, is new
# each run: the old rows linger until the heartbeat sweep, a minute or more,
# and anyone rejoining a call meanwhile is told someone else holds its key.
export CHAT_INSTANCE=${CHAT_INSTANCE:-dev-$(cat /etc/hostname 2>/dev/null || echo local)}
# The relay binds 0.0.0.0 (IPv4), and Chromium dials `localhost` over QUIC
# as ::1 — where nothing listens, so every call fails with "Opening
# handshake failed". The self-signed certificate covers 127.0.0.1 too.
export CALL_RELAY_PUBLIC_URL=${CALL_RELAY_PUBLIC_URL:-https://127.0.0.1:8798/}
export LIVE_SECRET=dev-live-secret-change-me-0123456789
exec ./target/debug/chat-server
