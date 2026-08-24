#!/usr/bin/env bash
# Put this Hearth on the public internet through an ngrok tunnel.
#
#   bash tools/voice-fleet.sh          # relays that check tickets
#   bash tools/share.sh                # or: DOMAIN=you.ngrok.io bash tools/share.sh
#
# The DATABASE IS NOT WIPED. This instance is somebody's account and space; the
# test suites have their own database and their own port precisely so that
# running them cannot delete it. See tools/reset-dev.sh.
#
# Three things have to agree for this to work, and getting any of them wrong
# looks like a different bug:
#
#   1. HEARTH_ORIGINS must contain the PUBLIC origin. The CSRF guard compares
#      against the browser's Origin header, which will be the ngrok host — not
#      the address the server is bound to. Wrong here and every call 403s and
#      the app reports that the server is unreachable.
#   2. The tunnel must terminate TLS (ngrok does). `getUserMedia` only works on
#      a secure context, so voice over a plain-http tunnel fails with a
#      permanently muted microphone and no error worth reading.
#   3. Voice node URLs must be rewritten to the public origin. The gateway
#      already does this from the request's own Host and X-Forwarded-Proto, so
#      it needs no configuration — but it only works because the voice routes
#      are served on this same origin.
set -u
cd "$(dirname "$0")/.."

DOMAIN=${DOMAIN:-nicho.ngrok.io}
PORT=${PORT:-8090}
PUBLIC="https://$DOMAIN"

cargo build -p hearth --bin server --features server -q || exit 1

# Only the shared instance. The test suites run their own server on another
# port with its own database (see reset-dev.sh), and killing that from here
# would make a `check-all` running alongside look like a flaky suite.
if [ -f /tmp/hearth-shared.pid ]; then
  kill "$(cat /tmp/hearth-shared.pid)" 2>/dev/null
  rm -f /tmp/hearth-shared.pid
fi
pkill -x ngrok 2>/dev/null
sleep 1

DATABASE_URL=${DATABASE_URL:-postgres://postgres:dev@localhost:55432/hearth_dev} \
HEARTH_BIND=127.0.0.1:$PORT \
REDIS_URL=${REDIS_URL:-redis://localhost:6399} \
VOICE_DIRECTOR_URL=${VOICE_DIRECTOR_URL:-http://127.0.0.1:9100} \
VOICE_SHARED_SECRET=${VOICE_SHARED_SECRET:-a-shared-secret-for-local-testing} \
HEARTH_ORIGINS="$PUBLIC,http://127.0.0.1:$PORT,http://localhost:$PORT" \
  nohup ./target/debug/server > /tmp/hearth-shared.log 2>&1 &
echo $! > /tmp/hearth-shared.pid

for _ in $(seq 1 40); do
  curl -fsS -m 1 "http://127.0.0.1:$PORT/" >/dev/null 2>&1 && break
  sleep 0.3
done

nohup ngrok http "$PORT" --domain "$DOMAIN" --log stdout > /tmp/ngrok.log 2>&1 &

for _ in $(seq 1 40); do
  if curl -fsS -m 3 "$PUBLIC/" >/dev/null 2>&1; then
    echo "Hearth is public at $PUBLIC"
    grep -E 'Browser origins allowed|voice enabled' /tmp/hearth-shared.log | sed 's/^/  /'
    exit 0
  fi
  sleep 0.5
done
echo "tunnel did not come up"; tail -5 /tmp/ngrok.log; exit 1
