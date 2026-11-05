#!/usr/bin/env bash
# Build first with scripts/build-aarch64.ps1, then run this script.
# Uploads the release binary xz-compressed and restarts the service with a
# rollback copy. Static-only changes do not need this; use hot-static.sh.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
HOST=${SUKISU_HOST:-root@ssh.neko.shirkneko.top}
KEY=${SUKISU_SSH_KEY:-$HOME/.ssh/id_ed25519}
BIN=${SUKISU_BIN:-$ROOT/target/aarch64-unknown-linux-musl/release/sukisu-theme-web}

if [ ! -f "$BIN" ]; then
  echo "binary not found: $BIN" >&2
  echo "run: powershell -ExecutionPolicy Bypass -File scripts/build-aarch64.ps1" >&2
  exit 1
fi

echo "uploading $(du -h "$BIN" | awk '{print $1}') binary (xz-compressed) to $HOST"
xz -9 -T0 -c "$BIN" | ssh -T -i "$KEY" "$HOST" '
set -euo pipefail
TMP=$(mktemp)
trap "rm -f \"$TMP\"" EXIT
xz -dc > "$TMP"
chmod 755 "$TMP"
UNIT=sukisu-theme-web.service
EXEC=$(systemctl show -p ExecStart --value "$UNIT" | sed -n "s/.*path=\([^ ;]*\).*/\1/p" | head -1)
[ -n "$EXEC" ] || EXEC=/opt/sukisu-theme-web/sukisu-theme-web-rs
BACKUP=/root/sukisu-deploy-backup/$(date +%Y%m%d-%H%M%S)
mkdir -p "$BACKUP"
cp -a "$EXEC" "$BACKUP/$(basename "$EXEC")"
install -m 0755 "$TMP" "$EXEC.new"
mv -f "$EXEC.new" "$EXEC"
systemctl restart "$UNIT"
for i in $(seq 1 20); do systemctl is-active --quiet "$UNIT" && break; sleep 1; done
if ! systemctl is-active --quiet "$UNIT"; then
  cp -a "$BACKUP/$(basename "$EXEC")" "$EXEC"
  systemctl restart "$UNIT"
  exit 1
fi
HEALTH=0
for i in $(seq 1 10); do
  if curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1; then
    HEALTH=1
    break
  fi
  sleep 1
done
if [ "$HEALTH" != 1 ]; then
  cp -a "$BACKUP/$(basename "$EXEC")" "$EXEC"
  systemctl restart "$UNIT"
  exit 1
fi
echo "deployed: $EXEC"
echo "backup: $BACKUP"
'
