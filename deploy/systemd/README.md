# Units

One process, plain systemd: `hearth`, which is the API, the web bundle and the
voice relay. Postgres and Redis are containers; this is not, because there is no
image to build and wrapping one binary to run one process on one box buys
nothing.

`hearth.env` is written by `ship.sh` on first deploy and is never in git — it
holds the shared secret that signs voice grants. Each deploy adds the relay's
settings to it if they are missing.

## Voice

The call relay (`call-relay`) runs inside `hearth` on UDP 8798, which is open in
the security group. Browsers dial it directly over WebTransport — it cannot be
reverse-proxied — at `https://<host>:8798/`, with a grant from `voice_ticket`.

It needs a certificate browsers trust without pinning, and the box already has
one: Caddy's, for the same name. `hearth-relay-cert.service` copies it to
`/opt/hearth/tls` (root, because Caddy's store is Caddy's alone) and restarts
`hearth` when it changes; `hearth-relay-cert.timer` runs it daily, and `ship.sh`
runs it on every deploy. Until Caddy has a certificate the relay serves a
self-signed one, and `voice_ticket` hands out its hash for browsers to pin — a
stopgap browsers accept for about two weeks.

`CALL_RELAY_THROTTLE=on` in `hearth.env` turns on per-member throttling: the
relay watches each member's download and holds back video layers, then
cameras, from anyone whose connection cannot take everything, so their call
stays steady instead of freezing. It costs the server a little work per
datagram per member, so it is off unless set.

`voice-node.service` and `voice-director.service` are the previous voice fleet.
`ship.sh` no longer installs them and disables them where they are still
running.
