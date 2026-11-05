#!/usr/bin/env bash
# quic-call on a public hostname, through a tunnel that forwards every port to
# this machine (see `relay status`).
#
#   relay up                          # the WireGuard tunnel, first
#   bash tools/quic-call-public.sh    # page on https://$HOST/, relay on $HOST:4433
#
# The page and the relay share one CA certificate, so nobody pastes a hash:
# open https://$HOST/ on each machine and press Join.
#
#   public 443/tcp -> local 8443   Caddy, serving crates/quic-call/dist/web
#   public 4433/udp -> local 4433  call-relay, bound to $BIND
set -euo pipefail

HOST="${HOST:-nicho.idealyst.io}"
CERTS="${CERTS:-$HOME/.config/idealyst-relay/certs}"
# The tunnel address rather than 0.0.0.0, so replies leave through the tunnel
# whatever the socket does about source addresses.
BIND="${BIND:-10.77.0.2}"
WEB_PORT="${WEB_PORT:-8443}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/crates/quic-call/dist/web"

ip -4 addr show | grep -q "inet $BIND/" \
    || { echo "no interface has $BIND — bring the tunnel up first (relay up)" >&2; exit 1; }
[[ -f "$DIST/index.html" ]] \
    || { echo "no bundle at $DIST — run: idealyst build --web --release crates/quic-call" >&2; exit 1; }
# DeepFilterNet runs inline in the page, and a debug wasm is far too slow for it.
[[ -n "$(find "$DIST/pkg" -name '*.br' -print -quit 2>/dev/null)" ]] \
    || echo "warning: $DIST looks like a debug build; DeepFilterNet will not keep up (build with --release)" >&2
[[ -f "$CERTS/live/$HOST/fullchain.pem" ]] \
    || { echo "no certificate at $CERTS/live/$HOST" >&2; exit 1; }

caddyfile="$(mktemp)"
cat >"$caddyfile" <<EOF
{
	auto_https off
	admin off
}
:$WEB_PORT {
	tls /certs/live/$HOST/fullchain.pem /certs/live/$HOST/privkey.pem
	root * /srv/web
	# A release bundle carries .br siblings; serve them where the browser takes br.
	file_server {
		precompressed br
	}
}
EOF

docker rm -f quic-call-web >/dev/null 2>&1 || true
# The parent of the bundle is mounted, not the bundle: `idealyst build` deletes
# and recreates dist/web, and a mount of the old directory would go on serving
# the deleted one (404s) until the container restarted.
# Host networking, not `-p`. The tunnel's policy rule sends everything sourced
# from $BIND into the tunnel, and a published port's NAT puts exactly that
# source on packets bound for the docker bridge — so they go to the relay
# instead of the container, and every request hangs. On the host network Caddy
# is an ordinary local socket, which the rule handles correctly.
docker run -d --name quic-call-web --network host \
    -v "$(dirname "$DIST"):/srv:ro" -v "$CERTS:/certs:ro" -v "$caddyfile:/etc/caddy/Caddyfile:ro" \
    caddy:2.8.4 >/dev/null
echo "page:  https://$HOST/   (Caddy on :$WEB_PORT; stop with: docker rm -f quic-call-web)"

cd "$ROOT"
exec cargo run --release -p call-relay -- "$BIND:4433" \
    --cert "$CERTS/live/$HOST/fullchain.pem" --key "$CERTS/live/$HOST/privkey.pem" --host "$HOST"
