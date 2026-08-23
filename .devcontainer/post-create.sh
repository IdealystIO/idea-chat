#!/usr/bin/env bash
# One-time setup. Idempotent: safe to re-run after a container rebuild.
set -euo pipefail

echo "==> wasm target (the web client builds to wasm32)"
rustup target add wasm32-unknown-unknown

echo "==> wasm-bindgen-cli"
# Must match the wasm-bindgen crate version the framework pins, or the CLI
# refuses the module with a version-mismatch error that reads like a corrupt
# build. Resolve it from the lockfile rather than guessing.
WB_VERSION="$(grep -A1 '^name = "wasm-bindgen"$' /workspaces/idealyst-native/Cargo.lock \
  | grep '^version' | head -1 | cut -d'"' -f2 || true)"
if [ -n "${WB_VERSION}" ]; then
  echo "    pinning wasm-bindgen-cli ${WB_VERSION}"
  cargo install wasm-bindgen-cli --version "${WB_VERSION}" --locked || true
else
  echo "    could not read the pinned version; installing latest"
  cargo install wasm-bindgen-cli --locked || true
fi

echo "==> idealyst CLI (from the sibling checkout, not crates.io)"
cargo install --path /workspaces/idealyst-native/crates/tools/cli --locked || true

echo "==> node deps for the browser harnesses"
if [ -f tools/package.json ]; then
  (cd tools && npm install --no-audit --no-fund)
fi

cat <<'NOTE'

Ready.

  cargo test                       # 80 tests; postgres + redis are already up
  docker compose up -d             # run the fleet (director + 3 nodes)
  cd crates/voice-client && idealyst dev --web --local

The coordination sidecars are reachable as:
  POSTGRES_URL=postgres://postgres:dev@postgres:5432/voice
  REDIS_URL=redis://redis:6379

NOTE
