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
  bash -c 'cargo build --release -p hearth --bin server --features server'

# One binary: voice is the call relay inside it (call-relay, on UDP 8798).
mkdir -p "$STAGE/bin"
docker run --rm -v idea-chat-deploy-target:/target -v "$STAGE:/out" debian:bookworm-slim \
  cp /target/release/server /out/bin/
cp deploy/sync-relay-cert.sh "$STAGE/bin/sync-relay-cert"

echo "==> building the web bundle (release, with brotli siblings)"
# RUSTFLAGS because the CLI ignores .cargo/config.toml for this build — see there.
( cd crates/hearth && RUSTFLAGS=--cfg=web_sys_unstable_apis \
    idealyst build --web --release --out-dir "$STAGE/web" )

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
rsync -az -e "ssh ${ssh_opts[*]}" deploy/systemd/hearth*.service deploy/systemd/hearth*.timer "admin@$host:/tmp/units/"

echo "==> installing units and restarting"
ssh "${ssh_opts[@]}" "admin@$host" "HOSTNAME_FQDN='$name' bash -s" <<'REMOTE'
set -euo pipefail
sudo install -m 0644 /tmp/units/hearth* /etc/systemd/system/
echo "HEARTH_FQDN=$HOSTNAME_FQDN" | sudo tee /opt/hearth/fqdn.env >/dev/null

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
VOICE_SHARED_SECRET=$secret
HEARTH_ORIGINS=https://$HOSTNAME_FQDN
ENV
  sudo chown admin:admin /opt/hearth/voice.env /opt/hearth/hearth.env
  sudo chmod 0600 /opt/hearth/voice.env /opt/hearth/hearth.env
fi

# The call relay's settings, added to an existing hearth.env as well as a new
# one: it runs inside hearth now, on UDP 8798 (open in the security group),
# with the site's certificate once Caddy has one (see sync-relay-cert).
set_env() {
  sudo sed -i "/^$1=/d" /opt/hearth/hearth.env
  echo "$1=$2" | sudo tee -a /opt/hearth/hearth.env >/dev/null
}
sudo sed -i '/^VOICE_DIRECTOR_URL=/d' /opt/hearth/hearth.env
set_env CALL_RELAY_BIND 0.0.0.0:8798
set_env CALL_RELAY_HOST "$HOSTNAME_FQDN"
set_env CALL_RELAY_CERT /opt/hearth/tls/fullchain.pem
set_env CALL_RELAY_KEY /opt/hearth/tls/privkey.pem

# The old voice fleet is replaced by the relay in hearth; it also held 8798.
sudo systemctl disable --now voice-node voice-director 2>/dev/null || true

sudo systemctl daemon-reload
sudo /opt/hearth/bin/sync-relay-cert "$HOSTNAME_FQDN"
sudo systemctl enable --now hearth hearth-relay-cert.timer
sudo systemctl restart hearth
sleep 3
systemctl is-active hearth
REMOTE

echo
echo "==> https://$name"
