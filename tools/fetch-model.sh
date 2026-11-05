#!/usr/bin/env bash
# Put the DeepFilterNet 3 model where the web client can fetch it.
#
# Client-side suppression needs the model in the browser, and it is deliberately
# NOT compiled into the wasm binary — 8 MB on every first visit, including the
# visitors who never open a voice room, to pay for a feature that is off by
# default. So the app fetches it at first join instead, and this puts it
# somewhere to fetch from.
#
# The file is NOT committed. It already exists on any machine that has built the
# workspace: `denoise` depends on DeepFilterNet by git rev, so cargo's checkout
# has it, and copying from there guarantees the model matches the inference
# engine that was compiled against it. Downloading a "current" one from upstream
# would not.
#
#   ./tools/fetch-model.sh            # copy into crates/hearth/assets/
#   ./tools/fetch-model.sh quic-call  # or into another app crate's assets/
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="$root/crates/${1:-hearth}/assets"
name="DeepFilterNet3_onnx.tar.gz"

# The rev is pinned in the SDK's manifest; matching on the package directory
# name rather than the rev keeps this working across a bump.
src="$(find "${CARGO_HOME:-$HOME/.cargo}/git/checkouts" \
        -path '*deepfilternet*' -name "$name" 2>/dev/null | head -1)"

if [ -z "$src" ]; then
  echo "fetch-model: no $name in the cargo git checkouts." >&2
  echo "             Build something that uses \`denoise\` first:" >&2
  echo "               cargo build -p voice-server --bin dfn-bench" >&2
  exit 1
fi

mkdir -p "$dest"
cp "$src" "$dest/$name"
echo "fetch-model: $dest/$name ($(du -h "$dest/$name" | cut -f1))"
echo "             served at /assets/$name — see voice_client::suppress::MODEL_PATH"
