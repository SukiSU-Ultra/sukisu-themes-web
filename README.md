# SukiSU 主题镜像

Rust 编写的主题镜像、投稿审核和账号系统，提供 Android 管理器接口、管理后台和用户中心。

## 功能

- 主题目录 `catalog.json`；上传前本地校验、服务端校验、图片压缩、SHA-256。
- 投稿审核、作者许可自动发布、管理员人工审核；撤回主题保留 14 天。
- 统一登录、邮箱验证、密码重置、GitHub OAuth、作者身份绑定/重置。
- 本地磁盘或 S3 兼容存储；支持配额、存储迁移、网络和认证配置。
- 中文 / English，首次按浏览器语言选择，之后保存在 `localStorage.sukisu-lang`。
- 主题描述支持 `用户名@平台` 提及，用户名支持中文/Unicode；支持 GitHub、Gitee、Bilibili、酷安、Telegram、微博、小红书、XDA、YouTube、爱发电、X、知乎、Threads、抖音。

## 入口

| 地址 | 用途 |
| --- | --- |
| `/` | 跳转统一登录/注册 |
| `/account/login` | 登录 / 注册 / 找回密码 |
| `/account/` | 用户中心 |
| `/admin/` | 管理后台 |
| `/admin/setup` | 首次初始化管理员 |
| `/health` | 健康检查（Caddy 返回 `ok`） |
| `/catalog.json` | 主题目录（需 `X-Sukisu-Theme-Token`） |
| `/themes/*`、`/previews/*`、`/screenshots/*`、`/avatars/*` | 资源文件（需访问令牌） |
| `/submit` | 管理器投稿（由服务端校验令牌） |

## 依赖

### 构建

- Rust 1.85+。
- Linux 原生构建：`build-essential`、`pkg-config`、`ca-certificates`。
- Debian/Armbian 安装示例：

  ```bash
  sudo apt update
  sudo apt install -y build-essential pkg-config ca-certificates curl caddy
  ```

  如果发行版仓库没有 Caddy，按 Caddy 官方文档安装。
- 交叉编译 `aarch64-unknown-linux-musl`：
  - `rustup target add aarch64-unknown-linux-musl`
  - Zig 0.16（`scripts/zigcc-aarch64-musl.bat`、`scripts/ar-aarch64-musl.bat` 会调用 `zig cc` / `zig ar`）
- 前端是原生 HTML/CSS/JS，不需要 Node.js/npm。

### 运行

- Linux + systemd；Caddy/Nginx/Lucky 等反向代理负责 TLS 和路由。
- SQLite 已静态编译，不需要单独安装数据库；也不需要 OpenSSL 开发包。
- 可选：SMTP 服务、GitHub OAuth App、S3 兼容对象存储（AWS S3 / R2 / MinIO / B2）。

## 快速开始

### 本地运行

```bash
cargo build --release
mkdir -p dev-data
cat > dev-storage.env <<EOF
THEME_STORAGE_ROOT=$PWD/dev-data
THEME_STORAGE_LIMIT_BYTES=1258291200
EOF
THEME_STORAGE_ENV="$PWD/dev-storage.env" THEME_DB_PATH="$PWD/dev-account.db" \
  ADMIN_BIND=127.0.0.1 ADMIN_PORT=8090 ./target/release/sukisu-theme-web
```

打开 `http://127.0.0.1:8090/admin/setup` 创建管理员并设置存储配额。

Windows 本地开发（先创建 `dev-storage.env`，内容见下方）：

```powershell
$env:THEME_STORAGE_ENV="$PWD\dev-storage.env"
$env:THEME_DB_PATH="$PWD\dev-account.db"
$env:THEME_STATIC_DIR="$PWD\static"
$env:THEME_DISABLE_SYSTEMCTL="1"
$env:ADMIN_BIND="127.0.0.1"
$env:ADMIN_PORT="8090"
cargo run
```

`dev-storage.env`：

```ini
THEME_STORAGE_ROOT=C:/path/to/sukisu-data
THEME_STORAGE_LIMIT_BYTES=1258291200
```

开发开关：

- `THEME_STATIC_DIR=static`：直接从磁盘读取 HTML/JS/语言文件，改完刷新即可，不用重编译。
- `THEME_DEV_MODE=1`：邮件链接写入日志，不真正发信。
- `THEME_DISABLE_SYSTEMCTL=1`：不调用 systemctl/caddy（非 systemd 环境）。

### 交叉编译 aarch64

```powershell
rustup target add aarch64-unknown-linux-musl
# 确保 zig 0.16 在 PATH
powershell -ExecutionPolicy Bypass -File scripts/build-aarch64.ps1
```

产物：

```text
target/aarch64-unknown-linux-musl/release/sukisu-theme-web
```

## 配置

默认配置目录：

| 文件 | 用途 |
| --- | --- |
| `/etc/sukisu-theme-web/storage.env` | 存储目录、后端、S3 参数、配额 |
| `/etc/sukisu-themes/store.env` | `THEME_STORE_TOKEN`（资源访问令牌） |
| `/etc/sukisu-theme-web/admin.env` | 可选的预置管理员（`ADMIN_USERNAME` / `ADMIN_PASSWORD_HASH`） |
| `/etc/sukisu-theme-web/config.env` | `PUBLIC_URL`、`ADMIN_URL` |
| `/etc/sukisu-theme-web/network.env` | `ADMIN_BIND`、`ADMIN_PORT`、`PUBLIC_DOMAINS` |
| `/etc/sukisu-theme-web/account.env` | `ACCOUNT_URL`、SMTP、GitHub OAuth |
| `/var/lib/sukisu-theme-web/account.db` | SQLite 账号数据库 |

可以用 `THEME_STORAGE_ENV`、`THEME_DB_PATH`、`THEME_ADMIN_ENV`、`THEME_ADMIN_CONFIG`、`THEME_NETWORK_ENV`、`THEME_ACCOUNT_ENV`、`THEME_CADDYFILE`、`THEME_STATIC_DIR` 覆盖路径；`THEME_TOKEN_ENV`、`THEME_AUTHORS_FILE`、`THEME_ADMIN_PASSWORD_FILE` 分别覆盖令牌、作者和密码文件。

### `storage.env`

```ini
THEME_STORAGE_ROOT=/srv/sukisu-themes
ASSET_BACKEND=local
THEME_STORAGE_LIMIT_BYTES=1258291200

# ASSET_BACKEND=s3 时使用
S3_ENDPOINT=
S3_REGION=us-east-1
S3_BUCKET=
S3_PREFIX=
S3_ACCESS_KEY=
S3_SECRET_KEY=
S3_PATH_STYLE=true
```

### `store.env`

```bash
openssl rand -hex 32
```

```ini
# /etc/sukisu-themes/store.env
THEME_STORE_TOKEN=<上面生成的 64 位十六进制>
```

管理器配置和 Caddy 必须使用同一个令牌。

### `network.env` / `config.env`

```ini
# network.env
ADMIN_BIND=127.0.0.1
ADMIN_PORT=8090
PUBLIC_DOMAINS=themes.example.com

# config.env
PUBLIC_URL=https://themes.example.com/
ADMIN_URL=https://themes.example.com/admin/
```

### `account.env`

```ini
ACCOUNT_URL=https://themes.example.com/account/
GITHUB_CALLBACK_URL=https://themes.example.com/account/api/auth/github/callback
GITHUB_CLIENT_ID=
GITHUB_CLIENT_SECRET=
SMTP_HOST=
SMTP_PORT=587
SMTP_USERNAME=
SMTP_PASSWORD=
SMTP_FROM=
SMTP_TLS=starttls
```

未配置 SMTP 时无法注册邮箱账号；未配置 GitHub 时隐藏 GitHub 登录按钮。管理员也可以登录后在「认证」页修改并测试。

## 部署（Linux + systemd + Caddy）

### 1. 安装二进制和目录

```bash
install -d /opt/sukisu-theme-web /srv/sukisu-themes \
  /etc/sukisu-theme-web /etc/sukisu-themes /var/lib/sukisu-theme-web
install -m 0755 target/aarch64-unknown-linux-musl/release/sukisu-theme-web \
  /opt/sukisu-theme-web/sukisu-theme-web-rs
```

静态资源已嵌入二进制。生产环境不要设置 `THEME_STATIC_DIR`；修改 HTML/JS/语言文件后必须重新构建并替换二进制。

### 2. 写入配置

按上一节创建 `storage.env`、`store.env`、`network.env`、`config.env`；SMTP/GitHub 可选。

### 3. systemd

`/etc/systemd/system/sukisu-theme-web.service`：

```ini
[Unit]
Description=SukiSU theme mirror web console
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/sukisu-theme-web/sukisu-theme-web-rs
Restart=always
RestartSec=3
Environment=RUST_LOG=info
EnvironmentFile=-/etc/sukisu-theme-web/network.env

[Install]
WantedBy=multi-user.target
```

服务默认以 root 运行，因为需要写存储目录、SQLite 和 `/etc/sukisu-theme-web`，并调用 `systemctl`/Caddy。

```bash
systemctl daemon-reload
systemctl enable --now sukisu-theme-web
```

### 4. Caddy

给 Caddy 提供令牌和存储根目录：

`/etc/systemd/system/caddy.service.d/theme-store.conf`：

```ini
[Service]
EnvironmentFile=/etc/sukisu-themes/store.env
EnvironmentFile=/etc/sukisu-theme-web/storage.env
```

`/etc/caddy/Caddyfile`（最小可用配置，假设 TLS 由前置 Lucky/Nginx 终止；如果 Caddy 直接对外，改成域名站点并配置证书）：

```caddyfile
http://:8080 {
	bind 127.0.0.1
	root * {$THEME_STORAGE_ROOT}
	encode zstd gzip

	header {
		Access-Control-Allow-Origin "*"
		X-Content-Type-Options "nosniff"
	}

	handle_path /admin* {
		reverse_proxy 127.0.0.1:8090
	}
	handle /account* {
		reverse_proxy 127.0.0.1:8090
	}
	handle /submit {
		reverse_proxy 127.0.0.1:8090
	}
	handle /health {
		respond "ok" 200
	}

	# 公共 UI 资源，必须在 token 校验之前
	handle /i18n.js {
		reverse_proxy 127.0.0.1:8090
	}
	handle /locales/* {
		reverse_proxy 127.0.0.1:8090
	}

	handle / {
		redir * /account/login 302
	}

	@authorized header X-Sukisu-Theme-Token {$THEME_STORE_TOKEN}
	handle @authorized {
		@assets path /themes/* /previews/* /screenshots/* /avatars/*
		handle @assets {
			reverse_proxy 127.0.0.1:8090
		}
		handle {
			file_server
		}
	}

	handle {
		respond "Unauthorized" 401
	}
}
```

```bash
systemctl daemon-reload

# Caddyfile 使用 {$THEME_STORE_TOKEN} 和 {$THEME_STORAGE_ROOT}，校验前先导出
export THEME_STORE_TOKEN="$(sed -n 's/^THEME_STORE_TOKEN=//p' /etc/sukisu-themes/store.env | tail -1)"
export THEME_STORAGE_ROOT="$(sed -n 's/^THEME_STORAGE_ROOT=//p' /etc/sukisu-theme-web/storage.env | tail -1)"

caddy validate --config /etc/caddy/Caddyfile
systemctl enable --now caddy
systemctl reload caddy
```

如果不用 Caddy，Nginx 等价规则：

- `/admin/` 反代到 `http://127.0.0.1:8090/`（去掉 `/admin` 前缀）。
- `/account/` 反代到 `http://127.0.0.1:8090`（保留前缀）。
- `/i18n.js`、`/locales/*` 无需令牌即可访问。
- `/themes/`、`/previews/`、`/screenshots/`、`/avatars/`、`/catalog.json` 校验 `X-Sukisu-Theme-Token`。
- `/health` 直接返回 `200 ok`。

管理后台的「网络」页会改写 `THEME_CADDYFILE`（默认 `/etc/caddy/Caddyfile`）并重启服务；不使用 Caddy 时可设置 `THEME_DISABLE_SYSTEMCTL=1`，改用其他方式维护代理配置。

### 5. 初始化

浏览器打开：

```text
https://<域名>/admin/setup
```

创建管理员、选择存储目录和配额。完成后即可登录管理后台。若使用 `admin.env` 预置管理员，直接访问 `/account/login`。

### 6. 验证

```bash
curl -fsS http://127.0.0.1:8080/health
curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8090/account/login
curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/i18n.js
curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/themes/missing
```

预期：`ok`、`200`、`200`、`401`。公网再确认登录页、`/i18n.js`、`/locales/en.json` 和 logo 正常。

## 升级与备份

升级：

```bash
BACKUP=/root/sukisu-deploy-backup/$(date +%Y%m%d-%H%M%S)
mkdir -p "$BACKUP"
cp -a /opt/sukisu-theme-web/sukisu-theme-web-rs "$BACKUP/"
install -m 0755 sukisu-theme-web /opt/sukisu-theme-web/sukisu-theme-web-rs
systemctl restart sukisu-theme-web
systemctl is-active sukisu-theme-web
```

失败时恢复 `$BACKUP/sukisu-theme-web-rs` 并重启。

备份：

| 路径 | 内容 |
| --- | --- |
| `/srv/sukisu-themes` | 主题包、预览图、截图、头像、meta、`catalog.json` |
| `/var/lib/sukisu-theme-web/account.db` | SQLite（含 `-wal` / `-shm`） |
| `/etc/sukisu-theme-web`、`/etc/sukisu-themes` | 配置和访问令牌 |
| `/etc/caddy/Caddyfile` | 反向代理配置 |

## 多语言

- 语言：`zh-CN`（HTML 原文）、`en`（`static/locales/en.json`）。
- 新增语言：复制 `en.json` 为 `static/locales/<code>.json` 并翻译；在 `static/i18n.js` 的 `LANGUAGES` 注册；在 `src/api.rs` 添加 `include_str!` 常量和 `embedded_static` 分支。
- 静态资源嵌入二进制，新增语言文件后必须重新构建。

## 常见问题

- **页面没有语言下拉框**：检查 `/i18n.js`、`/locales/en.json` 是否被反向代理 401；Caddy 需保留 `handle /i18n.js` 和 `handle /locales/*`。
- **`/account/i18n.js` 404**：正确路径是根路径 `/i18n.js`；不要加 `/account` 前缀。
- **改了 HTML/JS 线上没变化**：生产环境未设置 `THEME_STATIC_DIR`，资源在二进制里，重新构建并替换二进制。
- **Caddy 启动后 502**：确认服务监听 `127.0.0.1:8090`，`/admin*` 需要去前缀，`/account*` 保留前缀。
- **首次打不开初始化页**：确认没有预置 `admin.env`；已有管理员时请走 `/account/login`。
- **浏览器显示旧页面**：`Ctrl + F5` 强制刷新。

## 开发

```bash
cargo check
cargo test
cargo build --release
```

完整生产部署、Caddy 验证、回滚和真实主机信息见 [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)。
