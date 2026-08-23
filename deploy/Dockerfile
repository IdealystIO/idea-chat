# Voice fleet image — builds both `voice-node` and `voice-director`.
#
# Build context is the repo root:  docker build -f deploy/Dockerfile .
#
# The web client is deliberately NOT in this image. It is a wasm bundle built by
# the idealyst CLI against the sibling framework checkout, which is a developer
# workflow, not a server artifact.

FROM rust:1-bookworm AS builder
WORKDIR /src

# Swap in the server-only workspace so cargo never looks for `../idealyst-native`.
COPY deploy/Cargo.server.toml ./Cargo.toml
COPY crates/voice-proto  ./crates/voice-proto
COPY crates/voice-coord  ./crates/voice-coord
COPY crates/voice-server ./crates/voice-server

# Tests are excluded from the image build: the fleet tests need live Postgres
# and Redis, so they belong in CI or the devcontainer, not in a layer that has
# neither.
RUN cargo build --release --bin voice-node --bin voice-director

FROM debian:bookworm-slim AS runtime
# tini so signals reach the process: the node's graceful shutdown (drain, then
# deregister) only runs if it actually receives SIGINT/SIGTERM, and PID 1
# without an init does not get default handlers.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini curl \
 && rm -rf /var/lib/apt/lists/*

# Unprivileged: nothing here needs root. Note the account is NOT called
# "voice" — Debian already ships a `voice` group (GID 22) and useradd fails
# on the collision.
RUN groupadd --system --gid 10001 voicefleet \
 && useradd --system --uid 10001 --gid voicefleet --create-home voicefleet
USER voicefleet

COPY --from=builder /src/target/release/voice-node     /usr/local/bin/voice-node
COPY --from=builder /src/target/release/voice-director /usr/local/bin/voice-director

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["voice-node"]
