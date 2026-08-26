#!/usr/bin/env bash
# Build everything here, put it on the box, restart.
#
#   bash deploy/ship.sh
#
# Nothing compiles on the server. It has no idealyst checkout — the manifests
# reach for it by path — and no reason to spend an hour on a t3.small doing what
# this machine does in minutes.
#
# The binaries are built in a `rust:1-bookworm` container rather than natively,
# because this machine's glibc is far newer than the AMI's and anything linked
# here simply will not start there. That is not a portability nicety; it is the
# difference between a service that runs and one that exits instantly with a
# message about a missing symbol.
set -euo pipefail
cd "$(dirname "$0")/.."

TF=deploy/terraform
TERRAFORM=${TERRAFORM:-terraform}
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

host=$("$TERRAFORM" -chdir="$TF" output -raw public_ip)
name=$("$TERRAFORM" -chdir="$TF" output -raw hostname)
key="$TF/id_ed25519"
ssh_opts=(-i "$key" -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null)

echo "==> building server binaries (in a bookworm container)"
docker volume create idea-chat-deploy-cargo  >/dev/null
docker volume create idea-chat-deploy-target >/dev/null
docker run --rm \
  -v "$PWD:/w/idea-chat:ro" \
  -v "$(cd .. && pwd)/idealyst-native:/w/idealyst-native:ro" \
  -v idea-chat-deploy-cargo:/cargo \
  -v idea-chat-deploy-target:/target \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -w /w/idea-chat rust:1-bookworm \
  bash -c 'cargo build --release -p hearth --bin server --features server \
        && cargo build --release -p voice-server --bin voice-node --bin voice-director'

mkdir -p "$STAGE/bin"
docker run --rm -v idea-chat-deploy-target:/target -v "$STAGE:/out" debian:bookworm-slim \
  cp /target/release/server /target/release/voice-node /target/release/voice-director /out/bin/

echo "==> building the web bundle (release, with brotli siblings)"
( cd crates/hearth && idealyst build --web --release --out-dir "$STAGE/web" )

echo "==> uploading"
# Made here rather than in bootstrap, because bootstrap runs once and this runs
# every deploy: a layout change should not require rebuilding the machine.
ssh "${ssh_opts[@]}" "admin@$host" \
  'mkdir -p /opt/hearth/bin /opt/hearth/dist/web /opt/hearth/assets /tmp/units'

# The model is 8 MB and changes never; skipped unless it is missing up there.
rsync -az --delete -e "ssh ${ssh_opts[*]}" "$STAGE/bin/" "admin@$host:/opt/hearth/bin/"
rsync -az --delete -e "ssh ${ssh_opts[*]}" "$STAGE/web/" "admin@$host:/opt/hearth/dist/web/"
if [ -f crates/hearth/assets/DeepFilterNet3_onnx.tar.gz ]; then
  rsync -az -e "ssh ${ssh_opts[*]}" crates/hearth/assets/ "admin@$host:/opt/hearth/assets/"
fi
rsync -az -e "ssh ${ssh_opts[*]}" deploy/systemd/*.service "admin@$host:/tmp/units/"

echo "==> installing units and restarting"
ssh "${ssh_opts[@]}" "admin@$host" "HOSTNAME_FQDN='$name' bash -s" <<'REMOTE'
set -euo pipefail
sudo install -m 0644 /tmp/units/*.service /etc/systemd/system/

# Secrets are generated ON the box, once, and never travel. Regenerating them
# would invalidate every session and every room ticket in flight.
if [ ! -f /opt/hearth/voice.env ]; then
  secret=$(head -c 32 /dev/urandom | base64 | tr -d '\n')
  sudo tee /opt/hearth/voice.env >/dev/null <<ENV
VOICE_SHARED_SECRET=$secret
COORD_BACKEND=postgres
POSTGRES_URL=postgres://postgres:hearth@127.0.0.1:5432/voice
VOICE_PUBLIC_HOST=$HOSTNAME_FQDN
ENV
  sudo tee /opt/hearth/hearth.env >/dev/null <<ENV
HEARTH_BIND=127.0.0.1:8080
DATABASE_URL=postgres://postgres:hearth@127.0.0.1:5432/hearth
REDIS_URL=redis://127.0.0.1:6379
VOICE_DIRECTOR_URL=http://127.0.0.1:9100
VOICE_SHARED_SECRET=$secret
HEARTH_ORIGINS=https://$HOSTNAME_FQDN
ENV
  sudo chown admin:admin /opt/hearth/voice.env /opt/hearth/hearth.env
  sudo chmod 0600 /opt/hearth/voice.env /opt/hearth/hearth.env
fi

sudo systemctl daemon-reload
sudo systemctl enable --now voice-director voice-node hearth
sudo systemctl restart voice-director voice-node hearth
sleep 3
systemctl is-active voice-director voice-node hearth
REMOTE

echo
echo "==> https://$name"
