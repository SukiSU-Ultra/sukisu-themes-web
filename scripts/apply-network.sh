#!/bin/sh
set -eu

NEW_PORT="${1:?new port required}"
OLD_PORT="${2:?old port required}"
BACKUP="${3:?backup directory required}"
HEALTH_HOST="${4:-127.0.0.1}"
CONFIG_DIR="/etc/sukisu-theme-web"

sleep 2

healthy=0
systemctl restart sukisu-theme-web.service || true
i=0
while [ "$i" -lt 30 ]; do
    if curl -fsS "http://${HEALTH_HOST}:${NEW_PORT}/api/session" >/dev/null 2>&1; then
        healthy=1
        break
    fi
    i=$((i + 1))
    sleep 1
done

if [ "$healthy" -eq 1 ]; then
    rm -rf "$BACKUP"
    exit 0
fi

# Roll back the config and restart if the service cannot serve the new port.
if [ -d "$BACKUP" ]; then
    for name in network.env config.env account.env; do
        if [ -f "$BACKUP/$name" ]; then
            cp "$BACKUP/$name" "$CONFIG_DIR/$name"
            chown root:root "$CONFIG_DIR/$name" 2>/dev/null || true
            chmod 600 "$CONFIG_DIR/$name" 2>/dev/null || true
        fi
    done
fi

systemctl restart sukisu-theme-web.service || true

# Give the old service a moment to come back.
sleep 1
if curl -fsS "http://${HEALTH_HOST}:${OLD_PORT}/api/session" >/dev/null 2>&1; then
    exit 1
fi
exit 1
