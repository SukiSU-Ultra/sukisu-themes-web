#!/usr/bin/env bash
# Upload only the static UI (HTML/JS/locales/logo) to the production host.
# The service reads THEME_STATIC_DIR on every request, so no rebuild or
# restart is needed after this script. Use Ctrl+F5 if the browser cached JS.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
HOST=${SUKISU_HOST:-root@ssh.neko.shirkneko.top}
KEY=${SUKISU_SSH_KEY:-$HOME/.ssh/id_ed25519}
REMOTE=${SUKISU_STATIC_DIR:-/opt/sukisu-theme-web}

if [ ! -d "$ROOT/static" ]; then
  echo "static/ not found under $ROOT" >&2
  exit 1
fi

echo "uploading static/ to $HOST:$REMOTE/static"
tar -czf - -C "$ROOT" static \
  | ssh -T -i "$KEY" "$HOST" "tar -xzf - -C '$REMOTE' --no-same-owner && echo 'static updated (no restart required; Ctrl+F5 if needed)'"
