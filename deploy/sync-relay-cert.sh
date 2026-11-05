#!/usr/bin/env bash
# Give the call relay inside hearth the site's own certificate.
#
# Caddy obtains and renews the certificate for this host, and keeps it where
# only Caddy can read it. The relay (in hearth's server, as `admin`) needs the
# same certificate for its QUIC listener — a CA certificate, so browsers dial it
# with no hash to pin and none of the two-week limit pinned ones carry.
#
# So this copies it across when it changes and restarts hearth to load it. Run
# as root by hearth-relay-cert.service, daily (hearth-relay-cert.timer) and on
# every deploy. Prints what it did; exits 0 when there is nothing to do yet.
set -euo pipefail

FQDN=${1:?usage: sync-relay-cert FQDN}
STORE=/var/lib/caddy/.local/share/caddy/certificates
DEST=/opt/hearth/tls

# Whichever issuer Caddy used (Let's Encrypt, or ZeroSSL as its fallback):
# the newest certificate for this name.
crt=$(ls -t "$STORE"/*/"$FQDN"/"$FQDN".crt 2>/dev/null | head -1 || true)
if [ -z "$crt" ]; then
  echo "no certificate for $FQDN from Caddy yet — the relay stays self-signed"
  exit 0
fi
key="${crt%.crt}.key"

install -d -o admin -g admin -m 0700 "$DEST"
if cmp -s "$crt" "$DEST/fullchain.pem" && cmp -s "$key" "$DEST/privkey.pem"; then
  echo "relay certificate is current"
  exit 0
fi
install -o admin -g admin -m 0600 "$crt" "$DEST/fullchain.pem"
install -o admin -g admin -m 0600 "$key" "$DEST/privkey.pem"
echo "relay certificate updated from $crt"
if systemctl is-active --quiet hearth; then
  systemctl restart hearth
  echo "hearth restarted to load it"
fi
