# SukiSU Theme Mirror 部署手册

## 目标环境

| 项目 | 值 |
| --- | --- |
| SSH | `root@ssh.neko.shirkneko.top` |
| 架构 | `linux-aarch64`（Armbian） |
| 公网入口 | `https://themes.neko.shirkneko.top:8443/` |
| 链路 | Lucky TLS → app `127.0.0.1:8080`（应用内置路由，无 Caddy） |
| systemd | `sukisu-theme-web.service` |
| 二进制 | `/opt/sukisu-theme-web/sukisu-theme-web-rs` |
| 导入来源 | `/etc/sukisu-theme-web/sources.json`（`THEME_SOURCES_FILE`） |
| 密钥 | `~/.ssh/id_ed25519`（有口令） |

`THEME_STATIC_DIR` 未设置，`index.html`、`account.html`、`i18n.js`、`locales/en.json`、`logo.svg` 等全部通过 `include_str!` / `include_bytes!` 打进二进制。改静态文件也必须重新构建并替换二进制。

## 存储位置与导入

主存储（`storage.env`）始终是第一个写入位置；`sources.json` 保存额外的导入/可写位置：

```json
{
  "version": 1,
  "sources": [
    { "id": "src_...", "name": "NAS", "type": "local", "enabled": true,
      "writable": true, "quotaBytes": 1073741824, "priority": 1,
      "local": { "path": "/mnt/nas/sukisu-themes" } },
    { "id": "src_...", "name": "OpenList", "type": "alist", "enabled": true,
      "writable": false, "quotaBytes": 0, "priority": 0,
      "alist": { "url": "https://alist.example.com", "token": "...", "root": "/sukisu-themes" } }
  ]
}
```

- 主存储达到配额或本地磁盘剩余阈值后，新主题按 `priority` 写入下一个可写来源；旧主题保留原位置，继续从原位置服务。
- 每个可写来源有独立 `quotaBytes`（0 = 不限制）；本地来源还会检查磁盘剩余。主存储配额仍在「存储」页设置。
- `local`：NAS 必须先由系统挂载 SMB/NFS；作为可写位置时，`path` 是存储根，应用会在其下创建 `themes/`、`previews/`、`screenshots/`、`avatars/`。
- `alist`：OpenList/AList v3 兼容 API，支持列表、上传、建目录、删除和下载（含 Range）。云盘分享必须先挂载/保存到 AList/OpenList，应用不解析分享链接、不绕过限速。
- `Meta.storageId` 记录主题所在位置，资源 key 带 `themes/_loc/<sourceId>/...`、`previews/_loc/...` 等；删除/撤回/预览/下载按 key 解析。
- 管理后台「存储位置」页可以添加/编辑/测试/删除来源，浏览目录并选择 `.sukisu`、预览图、可选头像/截图；导入同样参与溢出选路。
- 位置中仍有主题时，删除来源或修改类型/路径会被拒绝；先迁移/删除主题。
- `sources.json` 权限应为 `0600`，包含 AList Token；备份配置目录时一并备份。
- 导入任务与存储迁移共用单任务队列；进行中时不要同时上传或迁移。

## 构建

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-aarch64.ps1
```

产物：

```text
target/aarch64-unknown-linux-musl/release/sukisu-theme-web
```

确认架构（ELF `e_machine=0xB7`）：

```bash
od -An -t x1 -j 18 -N 2 target/aarch64-unknown-linux-musl/release/sukisu-theme-web
# b7 00
```

## 上传与替换

Windows 上不要用 `ssh -M` / `ControlPath`：该主机的 mux 通道会报
`mux_master_process_new_session: failed to receive fd 0 from client`。
用一次 SSH 会话从 stdin 流式上传 tar，并在远端执行脚本。

本地准备：

```bash
STAGE=$(mktemp -d)
cp target/aarch64-unknown-linux-musl/release/sukisu-theme-web "$STAGE/"
cp -r static "$STAGE/static"
# 放入远端 deploy.sh（见下节）
tar -czf deploy.tar.gz -C "$STAGE" .
```

上传并执行：

```bash
ssh -T root@ssh.neko.shirkneko.top '
  set -e
  rm -rf /tmp/sukisu-deploy
  mkdir -p /tmp/sukisu-deploy
  tar -xzf - -C /tmp/sukisu-deploy
  bash /tmp/sukisu-deploy/deploy.sh
' < deploy.tar.gz
```

如果私钥有口令且需要非交互执行，用一次性 askpass（不要把口令写入仓库或脚本）：

```bash
cat > /tmp/askpass.sh <<'SH'
#!/usr/bin/env bash
printf '%s\n' "${SUKISU_SSH_PASS:-}"
SH
chmod +x /tmp/askpass.sh

SUKISU_SSH_PASS='<会话内输入>' \
SSH_ASKPASS=/tmp/askpass.sh \
SSH_ASKPASS_REQUIRE=force \
ssh -T root@ssh.neko.shirkneko.top '...' < deploy.tar.gz
```

更推荐给部署单独配一把免口令、仅允许部署命令的密钥，或在本机启动 SSH Agent 后 `ssh-add`。

## 远端替换步骤

```bash
UNIT=sukisu-theme-web.service
EXEC=$(systemctl show -p ExecStart --value "$UNIT" | sed -n 's/.*path=\([^ ;]*\).*/\1/p' | head -1)
# /opt/sukisu-theme-web/sukisu-theme-web-rs

BACKUP=/root/sukisu-deploy-backup/$(date +%Y%m%d-%H%M%S)
mkdir -p "$BACKUP"
cp -a "$EXEC" "$BACKUP/$(basename "$EXEC")"

install -m 0755 /tmp/sukisu-deploy/sukisu-theme-web "$EXEC.new"
mv -f "$EXEC.new" "$EXEC"
systemctl restart "$UNIT"
systemctl is-active "$UNIT"
```

若 `systemctl is-active` 不是 `active`，从 `$BACKUP` 恢复旧二进制并重启，不要继续。

## 内置路由与 Lucky

应用直接监听 `127.0.0.1:8080`，内置原先 Caddy 的职责：

- 静态资源与页面由二进制提供，`/i18n.js`、`/locales/*` 公开。
- `/admin*` 在应用内去掉前缀；`/account*` 保持原样。
- `/` 返回 `302` 到 `/account/login`。
- `/themes/*`、`/previews/*`、`/screenshots/*`、`/avatars/*`、`/catalog.json` 校验 `X-Sukisu-Theme-Token`。
- 响应带 `Access-Control-Allow-Origin: *` 和 `X-Content-Type-Options: nosniff`。

Lucky 只需把站点反代到 `http://127.0.0.1:8080`，保留 Host 和路径，不需要改写 `/admin`。

迁移到内置监听时，先确认端口为空并停用 Caddy：

```bash
# 生产环境的 network.env 里如有 ADMIN_PORT=8090，改为 8080
sed -i 's/^ADMIN_PORT=.*/ADMIN_PORT=8080/' /etc/sukisu-theme-web/network.env
# 可选：清理不再使用的 QQ 互联凭据
sed -i '/^QQ_APP_ID=/d;/^QQ_APP_KEY=/d;/^QQ_CALLBACK_URL=/d' /etc/sukisu-theme-web/account.env
systemctl disable --now caddy
ss -lntp | grep ':8080' || true
systemctl restart sukisu-theme-web.service
```

镜像选择（`THEME_MIRRORS_FILE`）不经过 Lucky 配置：302 是普通 HTTP 响应，管理器会自动跟随到独立镜像。部署后确认：

```bash
curl -sI -H "X-Sukisu-Theme-Token: $THEME_STORE_TOKEN" \
  "https://<域名>/themes/<id>-v1.sukisu" | grep -iE 'HTTP|location'
```

- 有健康镜像时返回 `302` + 镜像地址；没有则返回 `200` 并由本服务器服务。
- 镜像必须公网可达、路径结构一致，并接受转发过来的 token 或公开可读。
- 带 `Range` 的续传请求应返回 `200/206`，不应返回 `302`。
- 详见 [`docs/MIRRORS.md`](MIRRORS.md)。

## 部署后验证

服务端：

```bash
systemctl status sukisu-theme-web.service --no-pager -l
ss -lntp | grep 8080
journalctl -u sukisu-theme-web.service -n 30 --no-pager
```

本机端口（当前构建的参考字节数）：

```bash
for p in /account/login /i18n.js /locales/en.json /logo.svg; do
  curl -sS -o /dev/null -w "$p %{http_code}\n" "http://127.0.0.1:8080$p"
done
# 以上均为 200

curl -sS -o /dev/null -w '/ %{http_code}\n' "http://127.0.0.1:8080/"
# / 302
```

公网：

```bash
for p in i18n.js locales/en.json account/login admin/ admin/logo.svg; do
  curl -k -sS -o /dev/null -w "/$p %{http_code} %{size_download}\n" \
    "https://themes.neko.shirkneko.top:8443/$p"
done

# token 门禁仍应生效
curl -k -sS -o /dev/null -w '%{http_code}\n' \
  https://themes.neko.shirkneko.top:8443/themes/does-not-exist
# 401
```

浏览器验证：打开 `https://themes.neko.shirkneko.top:8443/account/login`，
确认右上角语言下拉框存在，能切换中文 / English，logo 为新版。
若仍是旧页面，`Ctrl + F5` 强制刷新。

## 回滚

二进制：

```bash
cp -a /root/sukisu-deploy-backup/<timestamp>/sukisu-theme-web-rs \
  /opt/sukisu-theme-web/sukisu-theme-web-rs
systemctl restart sukisu-theme-web.service
```

## 常见问题

- **页面 HTML 是新版，但没有语言下拉框**：应用已公开 `/i18n.js` 和 `/locales/en.json`；若仍 401，检查 Lucky 是否额外加了鉴权。
- **`ssh -M` 后执行命令报 mux / fd 错误**：Windows OpenSSH 多路复用不可用，改用单次 SSH 或 stdin 流式上传。
- **远端执行脚本报 `$'\r'` 或 `set: pipefail: 无效的选项名`**：脚本是 CRLF。用 `sed -i 's/\r$//'` 转成 LF 再上传。
- **Lucky 502 / 服务无法访问**：确认 `network.env` 的 `ADMIN_PORT` 是 8080，`ss -lntp | grep 8080` 有监听，Lucky 上游指向 `127.0.0.1:8080`。
- **修改了 HTML/JS 但线上没变**：静态资源在二进制内部，必须重新构建 `scripts/build-aarch64.ps1` 并替换二进制；不是改远端 static 目录。
- **浏览器“选择文件 / 未选择任何文件”仍是中文**：这是浏览器原生控件文案，跟随浏览器/系统语言，不受页面 i18n 控制；按需求保持原生。
