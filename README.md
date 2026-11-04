# SukiSU 主题镜像

Rust 编写的主题镜像、投稿审核和账号系统，提供 Android 管理器接口、管理后台和用户中心。

## 功能

- 主题目录 `catalog.json`；上传前本地校验、服务端校验、图片压缩、SHA-256。
- 每个主题记录累计下载量与热度；目录、管理后台和管理器均可按热度/下载量排序。
- 用户中心「发现主题」随机预览其他作者的主题（每次 5 个），支持「随便看看」换一批和按作者筛选。
- 自适应主题（格式 v3）：识别亮/暗两套外观，自动添加 `adaptive` 标签，目录、管理后台和用户中心预览同步显示对应标签。
- 投稿审核、作者许可自动发布、管理员人工审核；撤回主题保留 14 天。
- 统一登录、邮箱验证、密码重置、GitHub OAuth、作者身份绑定/重置。
- 本地磁盘或 S3 兼容存储；支持配额、存储迁移、网络和认证配置。
- 主存储之外可添加多个存储位置：本地/NAS、OpenList/AList；可只读导入，也可标记为可写，在主存储满后按优先级作为溢出位置。
- 站点维护模式：管理后台一键开启后，除管理员外的网页访问显示维护页，管理器主题商店也会进入维护状态。
- 用户中心「内容偏好」保存 R18 显示选择；管理器登录后优先采用网页设置，本机免责声明仍需在每台设备上单独同意。
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
| `/health` | 健康检查 JSON（数据库、存储、目录、下载统计） |
| `/catalog.json` | 主题目录（需 `X-Sukisu-Theme-Token`） |
| `/themes/*`、`/previews/*`、`/screenshots/*`、`/avatars/*` | 资源文件（需访问令牌） |
| `/submit` | 管理器投稿（由服务端校验令牌） |

## 下载统计、热度与健康检查

- 下载量在主题包实际下发时累加；断点续传的后续 `Range` 请求不会重复计数。
- 热度 = 累计下载量 + 近 7 天下载量 × 10（`HEAT_RECENT_WEIGHT`）。
- `catalog.json` 的每个主题包含 `downloads` 和 `heat` 字段，管理器可直接显示数字标签并按二者排序。
- 统计保存在 SQLite 的 `theme_stats`、`theme_download_days` 表；服务启动和每小时刷新目录中的热度。
- `GET /health`（或 `/healthz`）返回健康状态、运行时间、数据库延迟、存储、目录和待审核统计：

  ```json
  {
    "ok": true,
    "status": "ok",
    "version": "1.0.0",
    "time": "2026-11-03T12:00:00Z",
    "uptimeSeconds": 3600,
    "checks": {
      "database": { "ok": true, "latencyMs": 1 },
      "storage": { "ok": true, "backend": "local", "diskFree": 1 },
      "catalog": { "ok": true, "themes": 12, "generatedAt": "..." },
      "pending": { "count": 0, "bytes": 0 }
    },
    "stats": { "downloads": 123, "recentDownloads": 45 }
  }
  ```

  `status` 为 `ok` / `degraded`（目录为空）/ `down`（数据库或存储不可用，HTTP 503）。
- 管理后台 `GET /api/status` 提供 `stats.downloads`、`stats.recentDownloads`、`stats.topThemes`；`GET /api/stats` 提供热度排行和近 14 天下载趋势。
- 管理后台主题列表支持 `GET /api/themes?sort=hot|downloads|updated|name|size&q=...&tag=...&limit=...&offset=...`。
- 用户中心 `GET /account/api/discover?limit=5&exclude=id1,id2&author=...` 返回随机主题预览；`GET /account/api/themes/{id}/download` 下载并计数。

## 主题留言与消息

留言按主题保存在 SQLite（`theme_comments` 表），支持任意 UTF-8 文本和表情。

- 用户中心 / 管理器：
  - `GET /account/api/themes/{id}/comments` 返回当前用户可见的留言线程。
  - `POST /account/api/themes/{id}/comments`，请求体 `{content, visibility, parentId}`；`visibility` 为 `public` 或 `author`，回复会继承顶层留言的可见性。
  - `POST /account/api/comments/delete`，请求体 `{id}`；留言者、主题作者和管理员可删除，删除顶层留言会级联删除回复。
  - `POST /account/api/comments/clear`，请求体 `{themeId?}`；普通用户清空自己参与的留言，主题作者清空该主题全部留言，管理员不传 `themeId` 时清空全部。
  - `GET /account/api/messages` 返回按主题分组的消息；普通用户看到自己参与的线程和被公开提及的线程，主题作者看到自己主题下的全部留言，管理员看到全部。
  - `GET /account/api/users/search?q=` 返回最多 10 个活跃账号（仅用户名、显示名、角色、是否作者），用于 `@` 提及选择器。
  - `GET /account/api/users/{id}/avatar` 是公开头像接口，只返回该用户保存的头像资源；评论 JSON 的 `avatarUrl` 指向它。
  - `GET /account/api/themes/{id}/comments` 允许匿名读取公开评论（私密留言仍只对留言者、作者和管理员可见）；发表评论、回复和删除仍需登录。
- 评论内容支持 Markdown（粗体、斜体、行内代码、围栏代码块、链接）和表情；`@username` 会通知本站账号，`username@github` / `@bilibili` 等平台提及会渲染成对应平台链接。
- 公开评论中的 `@username` 会在被提及用户的消息页显示“提到了你”；私密留言不会产生站内提及通知，避免泄露私密线程。
- 管理后台：
  - `GET /api/comments?theme=&q=&visibility=&offset=&limit=` 列出全部留言。
  - `POST /api/comments/delete`、`POST /api/comments/clear`（`{themeId?}`）。
  - `GET/POST /api/comments/settings`，请求体 `{retentionDays}`；`0` 表示永久保留，默认 30 天，最大 3650 天。
- 私密留言（`visibility=author`）仅留言者、主题作者和管理员可见；公开留言对所有已登录用户可见。
- 自动清理在服务启动时及之后每小时执行一次，按 `createdAt` 删除超过保留天数的留言；永久删除主题时也会清理该主题的留言。

## 性能与并发

- SQLite 使用 WAL，并默认开 4 个连接组成的小连接池；可用 `THEME_DB_POOL_SIZE` 调整（1-32）。读写仍由 SQLite 串行化，但读请求不再互相阻塞。
- SQLite 连接使用 `synchronous=NORMAL`、内存临时表和 8 MB 页面缓存，适合以读为主的主题镜像。
- 下载计数先写入内存缓冲区，每 3 秒批量落库并批量更新 `catalog.json`，不会每个请求都写数据库或重写目录文件。因此 `/health`、`/api/status` 和统计接口的下载数可能延迟约 3 秒。
- 已发布主题的预览图、截图、作者信息和主题索引缓存在内存中；目录 `catalog.json` 也缓存解析结果，按 mtime/写入版本失效。
- 登录会话有 5 秒正向缓存，避免每个已登录请求都查询 SQLite；登出和会话撤销会立即清理缓存，禁用账号最多延迟 5 秒生效。
- SQLite 使用 `synchronous=NORMAL`：断电时可能丢失最后少量已提交事务，但数据库完整性不受影响。下载计数在崩溃时最多丢失约 3 秒缓冲。
- `catalog.json`、HTML/JS 使用 `ETag` + `304`；主题包、预览图和截图按版本号返回长缓存头，重复访问可直接命中浏览器缓存（令牌保护资源使用 `private`，不进入共享缓存）。
- 高流量部署建议：Caddy 保持 `encode zstd gzip`；`account.db` 放在本地 SSD，不要放 NFS/SMB；主题资源可以用 NAS/S3，但数据库必须本地；根据 CPU 和内存调整 `THEME_DB_POOL_SIZE`；系统文件描述符上限至少 4096。

## 上传进度与并发

- 管理后台和用户中心都使用服务端的 4 MB 分片会话（`/uploads/init`、`/uploads/{id}/{index}`），最多 4 路并发；每个分片用 XHR 的 `upload.onprogress` 上报实时字节数，不再等整块 4 MB 完成才跳一次。
- 进度区显示百分比、已传/总字节、实时速度和预计剩余时间；总量是“主题包 + 预览图 + 头像 + 截图”的真实字节，预览图阶段的进度会继续推进，不再固定按 95% 切换。速度按最近 2.5 秒的滑动窗口计算。
- 断点续传恢复时，服务端已收到的分片计入初始进度；单个分片失败最多重试 2 次，仍失败则保留已完成分片，重新提交可继续。
- 管理器使用相同的分片会话，并对主题包做并行分片上传；最终表单（预览图、截图等）继续使用带进度的 multipart 请求。

## 镜像选择与 302 跳转

主题包下载可以交给其他 HTTP 镜像，管理器不需要 WebRTC、P2P 库或任何额外改动。服务端仍是权威主题源；只有在配置了健康镜像时，才会把首次下载重定向过去。

- 配置文件：`THEME_MIRRORS_FILE`（默认 `/etc/sukisu-theme-web/mirrors.json`），每 30 秒自动重载，无需重启。
- 仅对 `themes/...` 且**不带 `Range`** 的请求尝试镜像；断点续传始终留在本服务器，避免不同源的文件混在一起。
- 选择顺序：按 `priority` 从小到大；同优先级之间轮询，避免所有下载压到同一台镜像；对候选镜像的同一路径做 `HEAD`（不支持 `HEAD` 时用 `Range: bytes=0-0`）探测，成功才重定向；4 秒总预算，全部失败则本地直接下载。
- 探测结果缓存：成功 60 秒、失败 20 秒，避免每个分片/重试都重复探测。
- 302 的 `Location` 带 `sukisu_mirror=1`；如果镜像误配成本服务器，第二次请求会识别该标记并本地服务，不会循环重定向。
- 统计在 302 时计数一次；自指回退不会再计数。
- 镜像必须提供相同的 `themes/<id>-v<version>.sukisu` 路径，并且文件与本服务器字节一致。管理器会跨域转发 `X-Sukisu-Theme-Token` 请求头，因此镜像要么接受同一个 token，要么公开可读。配置里的 `token` 只用于服务端探测，不会写入重定向 URL。
- 镜像必须是公网可达的 HTTP/HTTPS 地址，不能是 AList/网盘的私有 API；可以是另一台部署了本服务并同步了主题的服务器。
- 示例：

  ```json
  {
    "mirrors": [
      {
        "name": "edge-a",
        "baseUrl": "https://a.example.com/",
        "token": "",
        "priority": 10,
        "enabled": true
      },
      {
        "name": "edge-b",
        "baseUrl": "https://b.example.com/",
        "token": "",
        "priority": 20,
        "enabled": true
      }
    ]
  }
  ```

- 验证：

  ```bash
  curl -sI -H "X-Sukisu-Theme-Token: $TOKEN" \
    "https://<域名>/themes/<id>-v1.sukisu" | grep -iE 'HTTP|location'
  ```

  返回 `302` + 镜像地址说明选择成功；返回 `200` 说明没有可用镜像，正在本地服务。
- 不需要开放额外端口，也不需要任何 P2P/WebSocket 路由。

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
| `/etc/sukisu-theme-web/sources.json` | 导入/可写存储位置（本地/NAS、OpenList/AList Token、配额） |
| `/var/lib/sukisu-theme-web/account.db` | SQLite 账号数据库 |

可以用 `THEME_STORAGE_ENV`、`THEME_DB_PATH`、`THEME_ADMIN_ENV`、`THEME_ADMIN_CONFIG`、`THEME_NETWORK_ENV`、`THEME_ACCOUNT_ENV`、`THEME_CADDYFILE`、`THEME_STATIC_DIR` 覆盖路径；`THEME_TOKEN_ENV`、`THEME_AUTHORS_FILE`、`THEME_ADMIN_PASSWORD_FILE`、`THEME_SOURCES_FILE` 分别覆盖令牌、作者、密码和导入/可写来源文件。`THEME_DB_POOL_SIZE`（默认 4，范围 1-32）控制 SQLite 连接池大小。

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
# 站点维护开关；通常由管理后台「站点维护」卡片修改
MAINTENANCE=0
```

### `account.env`

```ini
ACCOUNT_URL=https://themes.example.com/account/
GITHUB_CALLBACK_URL=https://themes.example.com/account/api/auth/github/callback
GITHUB_CLIENT_ID=
GITHUB_CLIENT_SECRET=
QQ_APP_ID=
QQ_APP_KEY=
QQ_CALLBACK_URL=https://themes.example.com/account/api/auth/qq/callback
# Web 用户中心/管理后台会话时长（秒），也可在管理后台「认证」页修改；默认 30 天
WEB_SESSION_TTL_SECS=2592000
SMTP_HOST=
SMTP_PORT=587
SMTP_USERNAME=
SMTP_PASSWORD=
SMTP_FROM=
SMTP_TLS=starttls
```

未配置 SMTP 时无法注册邮箱账号；未配置 GitHub/QQ 时对应登录按钮隐藏。管理员也可以登录后在「认证」页修改这些配置。QQ 登录需要在 QQ 互联创建应用，并把回调地址登记为 `QQ_CALLBACK_URL`；QQ 不返回邮箱，账号创建后可在用户中心补充邮箱并设置密码。

### 站点维护模式

管理后台首页的「站点维护」卡片可以一键开启或关闭维护。状态保存在 `config.env` 的 `MAINTENANCE`，重启后仍然有效。

- 管理员（管理后台会话或 `role=admin` 的账号会话）不受影响，可以继续使用管理后台、账号中心和关闭维护。
- 普通访客的页面请求返回 `503` 维护页；账号中心 API 返回 `503` JSON（`maintenance: true`）。
- 健康检查、登录/找回密码页面及其所需的认证 API 保持可用，避免管理员被锁在维护模式之外。
- `/catalog.json` 保持可读，并写入 `"maintenance": true`；管理器读取后显示维护提示，不再展示主题列表。
- 维护期间主题包资源仍可能被 Caddy 直接下发。若需要连资源下载也严格阻断，请把 `/themes/*`、`/previews/*` 等资源路径改为反代到应用（应用会返回维护页），而不是使用 Caddy `file_server`；`/catalog.json` 会继续返回维护标记供管理器识别。

### 存储位置、溢出与导入

管理后台「存储位置」页支持配置多个来源，默认为只读导入；也可以标记为可写：

- **主存储**：仍然是上传/发布的第一选择，配额在「存储」页设置。
- **可写来源**：填写独立配额和优先级。主存储达到配额或磁盘剩余阈值后，新主题按优先级写入下一个可用位置；旧主题保留原位置继续服务。
- **本地 / NAS**：填写服务器上的绝对目录；作为可写位置时，会在该根目录下创建 `themes/`、`previews/`、`screenshots/`、`avatars/`。NAS 需先在系统挂载 SMB/NFS。
- **OpenList / AList**：支持目录浏览、上传、建目录、删除和下载（含 Range）；云盘分享仍需先在 AList/OpenList 中挂载。应用不实现分享链接破解或限速绕过。
- **导入**：浏览来源目录，选择 `.sukisu` 包、预览图和可选头像/截图，填写元数据后直接发布；导入同样参与溢出选路。
- 每个主题在 `meta` 中记录 `storageId`，资源 key 使用 `_loc/<sourceId>` 段；删除、撤回、预览和下载会按 key 解析回原位置。
- `sources.json` 保存来源、Token 和配额（权限 0600）；备份时需要包含该文件。

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
		reverse_proxy 127.0.0.1:8090
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
- 镜像跳转由应用返回普通 `302`；镜像本身是独立的 HTTP(S) 服务，不需要经过本服务器的 Caddy。
- `/health` 反代到 `http://127.0.0.1:8090/health`，返回 JSON 健康状态；数据库或存储不可用时返回 HTTP 503。
- 旧的 Caddy 配置如果仍使用 `respond "ok" 200`，请改成上面的 `reverse_proxy` 才能拿到详细健康信息。

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
| `/etc/sukisu-theme-web/sources.json` | 存储位置、配额和 AList Token |
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
