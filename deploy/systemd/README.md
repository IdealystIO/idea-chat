# Units

Three processes, plain systemd. Postgres and Redis are containers; these are
not, because there is no image to build and wrapping one binary to run one
process on one box buys nothing.

`hearth.env` and `voice.env` are written by `ship.sh` on first deploy and are
never in git — they hold the shared secret that mints room tickets.

## Two variables that look redundant and are not

`PUBLIC_URL` is `ws://127.0.0.1:8797`. `VOICE_PUBLIC_HOST` is the public
hostname. Both describe "where is this node", and they genuinely differ:

- The **WebSocket** is reverse-proxied by Hearth, so `PUBLIC_URL` must name an
  address *Hearth* can reach. On one box that is loopback.
- **WebTransport** cannot be proxied at all, so the browser dials the node
  directly and the published address must be one the *browser* can reach.

Set them the same and one of the two transports breaks — quietly, because the
client falls back to the WebSocket and the call still works.
