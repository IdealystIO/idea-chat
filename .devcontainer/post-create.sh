#!/usr/bin/env bash
# One-time setup. Idempotent: safe to re-run after a container rebuild.
set -euo pipefail

echo "==> claim the named volumes"
# Docker creates a fresh named volume root-owned, and postCreate runs as
# `vscode`, so every cargo command would fail with EACCES on the registry
# (and Claude Code / the AWS CLI couldn't write their config).
sudo chown -R "$(id -u):$(id -g)" \
  /usr/local/cargo/registry /workspaces/idea-chat/target \
  "$HOME/.claude" "$HOME/.aws"
chmod 700 "$HOME/.claude" "$HOME/.aws"

echo "==> wasm target (the web client builds to wasm32)"
rustup target add wasm32-unknown-unknown

echo "==> wasm-bindgen-cli"
# Must match the wasm-bindgen crate version the app's bundle is linked against,
# or the CLI refuses the module with a version-mismatch error that reads like a
# corrupt build. Resolve it from the lockfile rather than guessing — OUR
# lockfile: the framework comes from the registry now, so idealyst-native's
# Cargo.lock no longer decides what this build resolves.
WB_VERSION="$(grep -A1 '^name = "wasm-bindgen"$' /workspaces/idea-chat/Cargo.lock \
  | grep '^version' | head -1 | cut -d'"' -f2 || true)"
if [ -n "${WB_VERSION}" ]; then
  echo "    pinning wasm-bindgen-cli ${WB_VERSION}"
  cargo install wasm-bindgen-cli --version "${WB_VERSION}" --locked || true
else
  echo "    could not read the pinned version; installing latest"
  cargo install wasm-bindgen-cli --locked || true
fi

echo "==> binaryen (wasm-opt)"
# `idealyst build --web --release` runs wasm-opt after the split, and fails
# outright without it — so deploy/ship.sh cannot build its bundle here.
# Debug builds never call it, which is why its absence goes unnoticed.
#
# The base image ships with empty apt lists, so `install` alone fails — and
# under `set -e` that used to end the script here, before the idealyst CLI.
if ! command -v wasm-opt >/dev/null; then
  sudo apt-get update
  sudo apt-get install -y binaryen
fi

echo "==> idealyst CLI (from the sibling checkout, not crates.io)"
# After binaryen on purpose: the CLI's build script compiles the Inspector
# front end with wasm-opt, and without it `idealyst inspect` serves no page.
# No `|| true` — without the CLI nothing here builds or runs, so a failed
# install should stop the script where it can be seen.
CLI_SRC=/workspaces/idealyst-native/crates/tools/cli
CLI_VERSION="$(grep -m1 '^version' "$CLI_SRC/Cargo.toml" | cut -d'"' -f2)"
if idealyst --version 2>/dev/null | grep -qx "idealyst ${CLI_VERSION}"; then
  echo "    idealyst ${CLI_VERSION} already installed"
else
  cargo install --path "$CLI_SRC" --locked
fi

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
