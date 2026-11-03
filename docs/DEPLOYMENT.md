# SukiSU Theme Mirror 部署手册

## 目标环境

| 项目 | 值 |
| --- | --- |
| SSH | `root@ssh.neko.shirkneko.top` |
| 架构 | `linux-aarch64`（Armbian） |
| 公网入口 | `https://themes.neko.shirkneko.top:8443/` |
| 链路 | Lucky TLS → Caddy `127.0.0.1:8080` → app `127.0.0.1:8090` |
| systemd | `sukisu-theme-web.service` |
| 二进制 | `/opt/sukisu-theme-web/sukisu-theme-web-rs` |
| 导入来源 | `/etc/sukisu-theme-web/sources.json`（`THEME_SOURCES_FILE`） |
| Caddyfile | `/etc/caddy/Caddyfile` |
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

## Caddy 配置

新版语言资源必须能绕过主题 token 校验。`/etc/caddy/Caddyfile` 中，
在 token 校验之前保留：

```caddy
# Shared UI assets (language runtime and locale dictionaries).
@ui_assets path /i18n.js /locales/*
handle @ui_assets {
	reverse_proxy 127.0.0.1:8090
}
```

不要移除；否则 `/i18n.js` 和 `/locales/en.json` 会返回 401，页面 HTML 是新版但语言切换不出现。

Caddy 校验需要运行中进程的环境变量：

```bash
PID=$(systemctl show -p MainPID --value caddy)
while IFS= read -r -d '' entry; do
  case "$entry" in
    THEME_STORE_TOKEN=*|THEME_STORAGE_ROOT=*) export "$entry" ;;
  esac
done < "/proc/$PID/environ"

caddy validate --config /etc/caddy/Caddyfile
systemctl reload caddy
```

修改前备份：

```bash
cp -a /etc/caddy/Caddyfile /root/caddy-deploy-backup/Caddyfile.$(date +%Y%m%d-%H%M%S)
```

## 部署后验证

服务端：

```bash
systemctl status sukisu-theme-web.service --no-pager -l
ss -lntp | grep 8090
journalctl -u sukisu-theme-web.service -n 30 --no-pager
```

本机端口（当前构建的参考字节数）：

```bash
for p in / /account/login /i18n.js /locales/en.json /logo.svg; do
  curl -sS -o /dev/null -w "$p %{http_code} %{size_download}\n" "http://127.0.0.1:8090$p"
done
# / 200 182710
# /account/login 200 36894
# /i18n.js 200 13757
# /locales/en.json 200 78490
# /logo.svg 200 189417
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

Caddy：

```bash
cp -a /root/caddy-deploy-backup/Caddyfile.<timestamp> /etc/caddy/Caddyfile
systemctl reload caddy
```

## 常见问题

- **页面 HTML 是新版，但没有语言下拉框**：检查 `/i18n.js` 和 `/locales/en.json` 是否 401；需要在 Caddy 中保留 `@ui_assets` 放行。
- **`ssh -M` 后执行命令报 mux / fd 错误**：Windows OpenSSH 多路复用不可用，改用单次 SSH 或 stdin 流式上传。
- **远端执行脚本报 `$'\r'` 或 `set: pipefail: 无效的选项名`**：脚本是 CRLF。用 `sed -i 's/\r$//'` 转成 LF 再上传。
- **`caddy validate` 报 `root` 参数数量错误**：校验进程没有 `THEME_STORAGE_ROOT`。从运行中的 Caddy `/proc/<pid>/environ` 读取后再校验。
- **修改了 HTML/JS 但线上没变**：静态资源在二进制内部，必须重新构建 `scripts/build-aarch64.ps1` 并替换二进制；不是改远端 static 目录。
- **浏览器“选择文件 / 未选择任何文件”仍是中文**：这是浏览器原生控件文案，跟随浏览器/系统语言，不受页面 i18n 控制；按需求保持原生。
