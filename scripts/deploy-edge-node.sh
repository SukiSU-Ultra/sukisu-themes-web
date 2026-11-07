#!/usr/bin/env bash
# Provision one SukiSU edge/site node on a Linux host (native or WSL2).
#
# The node joins an existing EasyTier network so the origin reaches it on a
# stable private address, then publishes the site/edge port through Caddy with a
# self-signed CA that the origin is told to trust. Every step is idempotent: run
# it again after a reboot, an address change or an EasyTier upgrade.
#
# Required environment:
#   NODE_NAME          human label, also used for the certificate CN
#   ET_NET_NAME        EasyTier network name (e.g. zako)
#   ET_NET_SECRET      EasyTier network secret
#   ET_VIRTUAL_IPV4    fixed address inside the network (e.g. 100.0.0.50)
# Optional:
#   ET_PEER            public server URL (default udp://easytier.neko.shirkneko.top:11010)
#   ET_MTU             tunnel MTU (default 1200; larger values stall TLS over WSL NAT)
#   MODE               site | edge (default edge)
#   BINARY             binary already staged on this host (default /opt/sukisu-theme-web-rs)
#   SITE_CONFIG        site.json copied to /etc/sukisu-theme-site (MODE=site only)
#   EDGE_CONFIG        edge.json copied to /etc/sukisu-theme-edge (MODE=edge only)
#   TLS_PORT           public HTTPS port behind Caddy (default 8443)
#   APP_PORT           loopback port of the site/edge process (default 8080)
#   SKIP_EASYTIER=1    keep the existing EasyTier service untouched
#
# Usage (typical WSL2 run):
#   NODE_NAME=win-edge ET_NET_NAME=zako ET_NET_SECRET=... ET_VIRTUAL_IPV4=100.0.0.50 \
#     MODE=site SITE_CONFIG=/mnt/f/other/sukisu-edge/site.json \
#     bash scripts/deploy-edge-node.sh
set -euo pipefail

NODE_NAME=${NODE_NAME:?NODE_NAME is required}
ET_NET_NAME=${ET_NET_NAME:?ET_NET_NAME is required}
ET_NET_SECRET=${ET_NET_SECRET:?ET_NET_SECRET is required}
ET_VIRTUAL_IPV4=${ET_VIRTUAL_IPV4:?ET_VIRTUAL_IPV4 is required}
ET_PEER=${ET_PEER:-udp://easytier.neko.shirkneko.top:11010}
ET_MTU=${ET_MTU:-1200}
MODE=${MODE:-edge}
BINARY=${BINARY:-/opt/sukisu-theme-web-rs}
TLS_PORT=${TLS_PORT:-8443}
APP_PORT=${APP_PORT:-8080}
SKIP_EASYTIER=${SKIP_EASYTIER:-0}

log() { printf '[edge-node] %s\n' "$*"; }
die() { printf '[edge-node] error: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root"
[ -x "$BINARY" ] || die "binary not executable: $BINARY"
case "$MODE" in
  site) [ -n "${SITE_CONFIG:-}" ] && [ -f "$SITE_CONFIG" ] || die "MODE=site needs SITE_CONFIG" ;;
  edge) [ -n "${EDGE_CONFIG:-}" ] && [ -f "$EDGE_CONFIG" ] || die "MODE=edge needs EDGE_CONFIG" ;;
  *) die "MODE must be site or edge" ;;
esac
command -v openssl >/dev/null || die "openssl is required"

# Copy a config into place. Re-running on a live node passes the destination as
# the source, so equal paths must only fix ownership instead of failing.
copy_config() {
  local src=$1 dest=$2 owner=$3
  if [ "$(readlink -f "$src")" = "$(readlink -f "$dest" 2>/dev/null || echo "$dest")" ]; then
    chown "$owner" "$dest"
    chmod 0600 "$dest"
    return
  fi
  install -m 0600 -o "${owner%%:*}" -g "${owner##*:}" "$src" "$dest"
}

# ---------------------------------------------------------------- EasyTier ---
install_easytier() {
  if [ "$SKIP_EASYTIER" = "1" ]; then
    log "skipping EasyTier (SKIP_EASYTIER=1)"
    return
  fi
  if [ ! -x /opt/easytier/easytier-core ]; then
    local arch asset
    arch=$(uname -m)
    case "$arch" in
      x86_64) asset=x86_64 ;;
      aarch64|arm64) asset=aarch64 ;;
      *) die "unsupported architecture: $arch" ;;
    esac
    command -v unzip >/dev/null || { apt-get update -qq && apt-get install -y -qq unzip; }
    local version=${ET_VERSION:-v2.6.4}
    local url="https://github.com/EasyTier/EasyTier/releases/download/${version}/easytier-linux-${asset}-${version}.zip"
    log "downloading EasyTier $version ($asset)"
    curl -fL --retry 3 -m 300 -o /tmp/easytier.zip "$url"
    rm -rf /opt/easytier && mkdir -p /opt/easytier
    unzip -o -q /tmp/easytier.zip -d /opt/easytier
    # Archives nest the binaries in a per-arch directory.
    if [ -d "/opt/easytier/easytier-linux-${asset}" ]; then
      mv "/opt/easytier/easytier-linux-${asset}"/* /opt/easytier/
      rmdir "/opt/easytier/easytier-linux-${asset}"
    fi
    chmod +x /opt/easytier/easytier-*
    rm -f /tmp/easytier.zip
  fi
  /opt/easytier/easytier-core --version | head -1

  # The web console writes its own config; this node instead pins a plain file.
  cat > /etc/easytier.toml <<EOF
instance_name = "${NODE_NAME}"
hostname = "${NODE_NAME}"
ipv4 = "${ET_VIRTUAL_IPV4}"
mtu = ${ET_MTU}
listeners = ["tcp://0.0.0.0:11010", "udp://0.0.0.0:11010"]
rpc_portal = "127.0.0.1:15888"

[network_identity]
network_name = "${ET_NET_NAME}"
network_secret = "${ET_NET_SECRET}"

[[peer]]
uri = "${ET_PEER}"
EOF
  chmod 600 /etc/easytier.toml

  cat > /etc/systemd/system/easytier.service <<'UNIT'
[Unit]
Description=EasyTier network for the SukiSU edge node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/easytier/easytier-core -c /etc/easytier.toml
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
UNIT

  # NATed tunnels (WSL2 in particular) drop large TLS handshake packets above
  # ~1280 bytes, so the tunnel MTU is pinned after every EasyTier start.
  cat > /usr/local/bin/fix-et-mtu <<SH
#!/bin/sh
# Wait for tun0, then pin MTU. Safe to run when the interface never appears.
i=0
while [ \$i -lt 15 ]; do
  if ip link show tun0 >/dev/null 2>&1; then ip link set tun0 mtu ${ET_MTU} && exit 0; fi
  i=\$((i+1)); sleep 1
done
exit 0
SH
  chmod 755 /usr/local/bin/fix-et-mtu

  cat > /etc/systemd/system/easytier-mtu.service <<'UNIT'
[Unit]
Description=Pin EasyTier tunnel MTU for NATed tunnels
After=easytier.service network-online.target
Wants=easytier.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/bin/fix-et-mtu

[Install]
WantedBy=multi-user.target
UNIT

  systemctl daemon-reload
  systemctl enable easytier.service easytier-mtu.service >/dev/null 2>&1
  systemctl restart easytier.service
  systemctl restart easytier-mtu.service

  local i
  for i in $(seq 1 20); do
    ip -brief addr show tun0 >/dev/null 2>&1 && break
    sleep 1
  done
  ip -brief addr show tun0 || die "tun0 did not appear; check EasyTier credentials"
}

# ------------------------------------------------------------------- Caddy ---
install_caddy() {
  if ! command -v caddy >/dev/null; then
    log "installing Caddy"
    apt-get update -qq
    apt-get install -y -qq curl ca-certificates debian-keyring debian-archive-keyring apt-transport-https gpg
    curl -1sLf "https://dl.cloudsmith.io/public/caddy/stable/gpg.key" \
      | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
    curl -1sLf "https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt" \
      > /etc/apt/sources.list.d/caddy-stable.list
    apt-get update -qq
    apt-get install -y -qq caddy
  fi

  local cert=/etc/caddy/certs/edge.crt key=/etc/caddy/certs/edge.key
  install -d -m 0700 -o caddy -g caddy /etc/caddy/certs
  if [ ! -f "$cert" ] || [ ! -f "$key" ]; then
    log "creating self-signed CA certificate for $ET_VIRTUAL_IPV4"
    # CA:TRUE lets the origin treat this single file as a trust anchor.
    openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
      -keyout "$key" -out "$cert" \
      -subj "/CN=${NODE_NAME}" \
      -addext "basicConstraints=critical,CA:TRUE" \
      -addext "keyUsage=critical,digitalSignature,keyCertSign,keyEncipherment" \
      -addext "subjectAltName=IP:${ET_VIRTUAL_IPV4},IP:127.0.0.1,DNS:${NODE_NAME}" 2>/dev/null
  fi
  chown caddy:caddy "$cert" "$key"
  chmod 644 "$cert"
  chmod 600 "$key"

  cat > /etc/caddy/Caddyfile <<EOF
# Managed by scripts/deploy-edge-node.sh
https://${ET_VIRTUAL_IPV4}:${TLS_PORT} {
	tls ${cert} ${key}
	encode gzip zstd
	reverse_proxy 127.0.0.1:${APP_PORT} {
		header_up X-Forwarded-Proto https
	}
	log {
		output stderr
		format console
	}
}
EOF

  systemctl enable caddy >/dev/null 2>&1
  systemctl restart caddy
  systemctl is-active --quiet caddy || die "caddy failed to start"
  log "certificate fingerprint: $(openssl x509 -in "$cert" -noout -fingerprint -sha256 | cut -d= -f2)"
}

print_origin_hint() {
  local remote=/etc/sukisu-theme-web/certs/edge-ca.pem
  cat <<EOF

Registering this node on the origin:
  1. Add a peer (site mode) or edge node (plain mode) pointing at
     https://${ET_VIRTUAL_IPV4}:${TLS_PORT}
  2. Make the origin trust this certificate. It reads ONE PEM bundle from
     THEME_DISTRIBUTION_CA_FILE (default /etc/sukisu-theme-web/certs/edge-ca.pem),
     so concatenate rather than replacing:
       ssh root@<origin> "cat >> ${remote}" < /etc/caddy/certs/edge.crt
       ssh root@<origin> 'systemctl restart sukisu-theme-site.service'
  3. Remove the certificate with a rewrite if a node is decommissioned:
       sed -i '/BEGIN CERTIFICATE/,/END CERTIFICATE/d' on the origin bundle

Certificate to copy:
  /etc/caddy/certs/edge.crt  (sha256 $(openssl x509 -in /etc/caddy/certs/edge.crt -noout -fingerprint -sha256 | cut -d= -f2))
EOF
}

# ------------------------------------------------------------ site / edge ---
install_app() {
  # BINARY may already be the installed path (re-running on a live node), and a
  # rename keeps a running service executable intact while the file is replaced.
  local target=/opt/sukisu-theme-web-rs
  if [ "$(readlink -f "$BINARY")" != "$(readlink -f "$target")" ]; then
    install -m 0755 "$BINARY" "${target}.new"
    mv -f "${target}.new" "$target"
  fi
  if [ "$MODE" = "site" ]; then
    id -u sukisu-site >/dev/null 2>&1 || useradd --system \
      --home-dir /var/lib/sukisu-theme-site --shell /usr/sbin/nologin sukisu-site
    install -d -m 0700 -o sukisu-site -g sukisu-site /etc/sukisu-theme-site /var/lib/sukisu-theme-site
    copy_config "$SITE_CONFIG" /etc/sukisu-theme-site/site.json sukisu-site:sukisu-site
    cat > /etc/systemd/system/sukisu-theme-site.service <<'UNIT'
[Unit]
Description=SukiSU complete website node
After=network-online.target easytier.service
Wants=network-online.target easytier.service

[Service]
Type=simple
User=sukisu-site
Group=sukisu-site
ExecStart=/opt/sukisu-theme-web-rs site /etc/sukisu-theme-site/site.json
Restart=on-failure
RestartSec=5
KillMode=control-group
TimeoutStopSec=120
Environment=RUST_LOG=info
ExecStartPre=-/usr/local/bin/fix-et-mtu

[Install]
WantedBy=multi-user.target
UNIT
    systemctl disable --now sukisu-theme-edge.service >/dev/null 2>&1 || true
    systemctl daemon-reload
    systemctl enable sukisu-theme-site.service >/dev/null 2>&1
    systemctl restart sukisu-theme-site.service
  else
    id -u sukisu-edge >/dev/null 2>&1 || useradd --system \
      --home-dir /var/cache/sukisu-theme-edge --shell /usr/sbin/nologin sukisu-edge
    install -d -m 0700 -o sukisu-edge -g sukisu-edge /etc/sukisu-theme-edge /var/cache/sukisu-theme-edge
    copy_config "$EDGE_CONFIG" /etc/sukisu-theme-edge/edge.json sukisu-edge:sukisu-edge
    cat > /etc/systemd/system/sukisu-theme-edge.service <<'UNIT'
[Unit]
Description=SukiSU published theme cache edge
After=network-online.target easytier.service
Wants=network-online.target easytier.service

[Service]
Type=simple
User=sukisu-edge
Group=sukisu-edge
ExecStart=/opt/sukisu-theme-web-rs edge /etc/sukisu-theme-edge/edge.json
Restart=always
RestartSec=5
Environment=RUST_LOG=info
ExecStartPre=-/usr/local/bin/fix-et-mtu

[Install]
WantedBy=multi-user.target
UNIT
    systemctl disable --now sukisu-theme-site.service >/dev/null 2>&1 || true
    systemctl daemon-reload
    systemctl enable sukisu-theme-edge.service >/dev/null 2>&1
    systemctl restart sukisu-theme-edge.service
  fi

  local unit="sukisu-theme-${MODE}.service" i
  for i in $(seq 1 25); do
    curl -fsS -m 2 "http://127.0.0.1:${APP_PORT}/health" >/dev/null 2>&1 && break
    curl -fsS -m 2 "http://127.0.0.1:${APP_PORT}/healthz" >/dev/null 2>&1 && break
    sleep 1
  done
  systemctl is-active --quiet "$unit" || {
    journalctl -u "$unit" -n 20 --no-pager || true
    die "$unit is not running"
  }
  log "$unit active"
}

verify() {
  local code
  code=$(curl -sk -m 8 -o /dev/null -w '%{http_code}' "https://${ET_VIRTUAL_IPV4}:${TLS_PORT}/healthz" || true)
  log "https://${ET_VIRTUAL_IPV4}:${TLS_PORT}/healthz -> ${code:-000}"
  [ "$code" = "200" ] || log "warning: expected 200; check Caddy -> ${APP_PORT} and the tunnel"
}

install_easytier
install_caddy
install_app
verify
print_origin_hint
