# chat.idealyst.io

One EC2 instance running the whole thing: the Hearth server, a voice director,
a gateway, and one relay node, with Postgres and Redis in containers beside
them and Caddy terminating TLS.

Not a production topology, and deliberately so. It exists to put the app on a
URL that other people can open, which is the only way to find the things a
loopback test cannot — real network jitter, real microphones, real browsers on
machines nobody controls.

```
terraform init
terraform apply
../ship.sh          # build, upload, restart
```

## What is here and what is not

**Here:** the instance, its security group, an Elastic IP so the address
survives a stop/start, and the DNS record.

**Not here:** the binaries. Nothing compiles on the server — it has neither the
idealyst checkout the manifests reach for by path, nor any reason to spend an
hour on a t3.small doing what this machine does in minutes. `ship.sh` builds in
a `rust:1-bookworm` container (this machine's glibc is far newer than the
AMI's) and uploads the result.

## The UDP port is not optional decoration

A QUIC session cannot be reverse-proxied — it terminates at whoever holds the
certificate — so the voice node needs its own reachable UDP port, and the
security group opens 8798 for exactly that. Without it every call silently
falls back to the WebSocket, which works, and which is how the tunnelled setup
this replaces has always run. See `docs/quic-migration.md`.
