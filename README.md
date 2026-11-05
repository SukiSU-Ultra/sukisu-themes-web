# SukiSU 主题镜像

Rust 编写的主题镜像、投稿审核和账号系统，提供 Android 管理器接口、管理后台和用户中心。

## 功能

- 主题目录 `catalog.json`；上传前本地校验、服务端校验、图片压缩、SHA-256。
- 每个主题记录累计下载量与热度；目录、管理后台和管理器均可按热度/下载量排序。
- 用户中心「发现主题」随机预览其他作者的主题（每次 5 个），支持「随便看看」换一批和按作者筛选。
- 自适应主题（格式 v3）：识别亮/暗两套外观，自动添加 `adaptive` 标签，目录、管理后台和用户中心预览同步显示对应标签。
- 静态外观覆盖（格式 v4）：校验主题包内的 `appearance`（文字/配色 token、锐化等图像效果），只接受十六进制颜色和有限范围的数字。
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

- 应用内置 gzip/zstd 压缩，覆盖 HTML、JS、JSON、SVG 和 `catalog.json`；图片、主题包、206 响应和已压缩内容会自动跳过。压缩率、节省字节和按内容类型统计在管理后台「性能」页实时显示。
- 管理后台「性能」页可以调整压缩开关/编码/最小字节、下载缓冲、上传并发、统计落库间隔、页面轮询间隔、工作线程/阻塞线程和数据库连接池；部分配置保存后需要重启。
- 下载流默认使用 64 KiB 缓冲（可调 16 KiB–1 MiB），避免 4 KiB 小读放大；主题令牌和命名令牌有 5 秒短缓存，减少热点下载路径的磁盘/DB 查询。
- SQLite 使用 WAL，并默认开 4 个连接组成的小连接池；可用 `THEME_DB_POOL_SIZE` 调整（1-32）。读写仍由 SQLite 串行化，但读请求不再互相阻塞。
- SQLite 连接使用 `synchronous=NORMAL`、内存临时表和 8 MB 页面缓存，适合以读为主的主题镜像。
- 下载计数先写入内存缓冲区，按 `PERF_FLUSH_INTERVAL_SECS`（默认 3 秒）批量落库并批量更新 `catalog.json`，不会每个请求都写数据库或重写目录文件。因此 `/health`、`/api/status` 和统计接口的下载数可能延迟约 3 秒。
- 已发布主题的预览图、截图、作者信息和主题索引缓存在内存中；目录 `catalog.json` 也缓存解析结果，按 mtime/写入版本失效。
- 登录会话有 5 秒正向缓存，避免每个已登录请求都查询 SQLite；登出和会话撤销会立即清理缓存，禁用账号最多延迟 5 秒生效。
- SQLite 使用 `synchronous=NORMAL`：断电时可能丢失最后少量已提交事务，但数据库完整性不受影响。下载计数在崩溃时最多丢失约 3 秒缓冲。
- `catalog.json`、HTML/JS 使用弱 `ETag` + `304`；主题包、预览图和截图按版本号返回长缓存头，重复访问可直接命中浏览器缓存（令牌保护资源使用 `private`，不进入共享缓存）。
- 高流量部署建议：`account.db` 放在本地 SSD，不要放 NFS/SMB；主题资源可以用 NAS/S3，但数据库必须本地；根据 CPU 和内存调整 `THEME_DB_POOL_SIZE`；系统文件描述符上限至少 4096。


## 管理后台

- 「接入信息」与「API 接口」合并为「接入与 API」，统一展示接入地址、请求头、接口列表和调用示例。
- 「存储与配额」与「存储位置」合并为「存储与配额」，在同一页面内展示配额、目录、后端、NAS/OpenList 来源和导入。
- 每个页面顶部都有「功能说明」卡片，说明这是什么、完整能力和使用步骤。
- 用户与作者页使用统一的列表布局，支持按关键词、角色/状态筛选；用户详情改为独立的二级页面 `#user/<id>`，不再滚动到本页底部。
- 管理员操作日志以中文展示操作、目标和详情，支持按用户、操作和 IP 筛选；旧日志的 IP 为空。
- 主题预览页只读：不再提供编辑标签或删除入口；需要在「上传主题 → 主题维护」中操作。
- 性能页新增实时下载/上传速度、活跃请求、上传任务和当前正在读取的资源；顶栏也显示实时速度。

## API 实时使用监控

管理后台「性能 → 接口监控」（`#monitor`）实时展示正在处理的 API 请求，并自动保存完整连接日志。

- 监控范围：`/api`、`/admin/api`、`/account/api`、`/submit`、`/catalog.json`、`/health`，以及 `/themes/*`、`/previews/*`、`/screenshots/*`、`/avatars/*` 主题资源下载；后台/用户中心 HTML、JS、语言包等静态页面不在其中。
- 每条连接显示接口/功能、使用者（管理员、登录用户、命名主题令牌或未登录）、来源 IP、开始时间和持续时间；管理器下载会显示令牌名称（主令牌显示为「主令牌」）。
- 来源 IP 依次取 `X-Forwarded-For`、`X-Real-IP`，最后回退到 TCP 对端地址；显示 `127.0.0.1`/`::1` 说明反向代理没有转发客户端 IP，需要在 Lucky 中启用 `X-Forwarded-For`。不再用 `local` 代表真实请求。
- 只要请求携带有效登录会话（Cookie、`Authorization: Bearer`、`X-Sukisu-User-Token`、`X-Sukisu-Account-Token`、`X-Sukisu-Session` 或 `X-User-Token`），监控就优先显示账号完整信息：用户 ID、用户名、显示名、角色、账号状态、邮箱验证状态和 IP；即使同时带主题令牌也不会只显示令牌名。
- 管理器下载主题时，如果客户端只发送主题令牌而不带账号会话，服务端无法知道是哪个登录账号，只能显示令牌名；管理器需要在 `/themes/*` 请求上同时携带账号 token 或用户 Cookie。GitHub OAuth 登录的用户 Cookie 已改为 `/`，确保浏览器下载主题时也会携带。
- 实时连接每 1 秒刷新，表格同时展示正在处理和最近完成的连接（最近 100 条），短暂请求不会因为轮询间隔而消失；最近完成的连接可直接查看日志详情。
- 可在「实时连接」上方设置隐藏规则：按 IP（精确或 `*` 通配）或用户/令牌名称隐藏记录；规则保存在当前浏览器，并同时作用于实时连接、日志列表和导出。隐藏只影响显示/导出，不会删除数据库中的日志。
- 可设置追踪规则：指定 IP（精确或 `*` 通配）或用户/令牌名称，命中的活跃连接、最近完成连接和日志会高亮并显示「追踪」标记；勾选「只看追踪」后，实时表和日志列表只显示命中项，导出也只包含命中项。追踪规则同样保存在当前浏览器。隐藏规则优先于追踪规则。
- 日志详情中的「追踪此 IP / 追踪此用户/令牌」按钮可以直接把当前连接加入追踪。
- 「IP 协议」筛选支持只看 IPv4 或 IPv6 连接，并同时作用于实时表、日志和导出。

## IPv4 / IPv6 通道

管理后台「网络」页新增两个协议族设置：

- **监听协议**（`ADMIN_IP_FAMILY=auto|ipv4|ipv6`）：校验 `ADMIN_BIND` 与所选协议族一致；选择 IPv6 时把监听地址改为 `::1`/`::`，Lucky 上游必须同步改为 `[::1]:端口`，否则公网入口会中断。
- **出站协议**（`OUTBOUND_IP_FAMILY=auto|ipv4|ipv6`）：为 reqwest 客户端安装 A/AAAA 过滤 DNS 解析器，作用于镜像探测、AList、GitHub 等出站请求，重启后生效。`auto` 保持系统解析顺序。
- S3 使用 AWS SDK 自带的 HTTP 栈，SMTP 使用 lettre 的传输层，当前不强制协议族；如必须固定，可在对应 endpoint/host 直接填写 IPv4 或 IPv6 地址。
- 接口监控 `ipFamily=ipv4|ipv6` 可筛选客户端地址族；页面上的「IP 协议」下拉同时过滤实时连接、日志和导出。
- 请求与响应内容以可读格式保存：请求参数、响应正文、请求/响应头（含 Cookie）、User-Agent、状态码和耗时。正文默认最多保留 128 KiB，超出部分标记截断；所有活跃连接共享 32 MiB 抓取缓存，避免慢速大请求占用过多内存。
- 「断开」会立即取消正在执行的请求，并撤销该连接携带的管理员/用户会话；操作写入管理员操作日志（`api.abort`）。
- 日志支持按路径/功能、用户、IP、身份、状态和方法筛选，支持单条/批量删除、清空和导出 NDJSON 文件；删除/清空/导出同样写入管理员操作日志。
- 日志保存在 `audit.db` 的 `api_request_logs` 表，默认最多保留 2000000 条（`API_MONITOR_RETENTION`，0 为不自动清理）。
- 配置项（`config.env`）：`API_MONITOR_ENABLED`（默认 1）、`API_MONITOR_BODY_LIMIT`（默认 131072，范围 16 KiB–4 MiB）、`API_MONITOR_CAPTURE_BUDGET`（默认 33554432，范围 1–512 MiB）、`API_MONITOR_RETENTION`（默认 2000000）。

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
- 同一路径的冷启动探测带 single-flight：并发客户端共享一次探测，不会同时把每台镜像都打一遍。
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
  sudo apt install -y build-essential pkg-config ca-certificates curl
  ```
- 交叉编译 `aarch64-unknown-linux-musl`：
  - `rustup target add aarch64-unknown-linux-musl`
  - Zig 0.16（`scripts/zigcc-aarch64-musl.bat`、`scripts/ar-aarch64-musl.bat` 会调用 `zig cc` / `zig ar`）
- 前端是原生 HTML/CSS/JS，不需要 Node.js/npm。

### 运行

- Linux + systemd；Lucky/Nginx 等反向代理可选，仅用于 TLS 和统一入口。
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
  ./target/release/sukisu-theme-web migrate-db
THEME_STORAGE_ENV="$PWD/dev-storage.env" THEME_DB_PATH="$PWD/dev-account.db" \
  ADMIN_BIND=127.0.0.1 ADMIN_PORT=8080 ./target/release/sukisu-theme-web
```

首次运行 `migrate-db` 会在 `dev-account.db` 同级创建 `db-split/` 空库；已有单库时会复制数据。之后正常启动会拒绝单库布局并提示先迁移。

打开 `http://127.0.0.1:8080/admin/setup` 创建管理员并设置存储配额。

Windows 本地开发（先创建 `dev-storage.env`，内容见下方）：

```powershell
$env:THEME_STORAGE_ENV="$PWD\dev-storage.env"
$env:THEME_DB_PATH="$PWD\dev-account.db"
$env:THEME_STATIC_DIR="$PWD\static"
$env:THEME_DISABLE_SYSTEMCTL="1"
$env:ADMIN_BIND="127.0.0.1"
$env:ADMIN_PORT="8080"
cargo run -- migrate-db
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
- `THEME_DISABLE_SYSTEMCTL=1`：不调用 systemctl（非 systemd 环境）。

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
| `/etc/sukisu-themes/store.env` | `THEME_STORE_TOKEN`（资源访问主令牌，兼容保留；多令牌存 SQLite） |
| `/etc/sukisu-theme-web/admin.env` | 可选的预置管理员（`ADMIN_USERNAME` / `ADMIN_PASSWORD_HASH`） |
| `/etc/sukisu-theme-web/config.env` | `PUBLIC_URL`、`ADMIN_URL` |
| `/etc/sukisu-theme-web/network.env` | `ADMIN_BIND`、`ADMIN_PORT`、`PUBLIC_DOMAINS` |
| `/etc/sukisu-theme-web/account.env` | `ACCOUNT_URL`、SMTP、GitHub OAuth |
| `/etc/sukisu-theme-web/sources.json` | 导入/可写存储位置（本地/NAS、OpenList/AList Token、配额） |
| `/var/lib/sukisu-theme-web/db-split/` | SQLite 分库（account/authors/social/stats/audit） |
| `/var/lib/sukisu-theme-web/account.db` | 迁移前的单库快照（仅回滚用） |

可以用 `THEME_STORAGE_ENV`、`THEME_DB_PATH`、`THEME_DB_DIR`、`THEME_ADMIN_ENV`、`THEME_ADMIN_CONFIG`、`THEME_NETWORK_ENV`、`THEME_ACCOUNT_ENV`、`THEME_STATIC_DIR` 覆盖路径；`THEME_TOKEN_ENV`、`THEME_AUTHORS_FILE`、`THEME_ADMIN_PASSWORD_FILE`、`THEME_SOURCES_FILE` 分别覆盖令牌、作者、密码和导入/可写来源文件。`THEME_DB_POOL_SIZE`（默认 4，范围 1-32）控制 SQLite 连接池大小。

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

管理器配置和应用必须使用同一个令牌。

`THEME_STORE_TOKEN` 是全权限主令牌，用于兼容现有客户端；修改它会要求所有客户端同步更新。管理后台「设置 → 访问令牌」可以为每台管理器或每个镜像签发独立命名令牌：权限分为下载（`/catalog.json` 与资源文件）和上传（`/submit`），支持有效期、启用/停用和单独删除。令牌值只在创建时完整显示一次，数据库里仅存 SHA-256 哈希（随 `account.db` 一起备份）。

### `network.env` / `config.env`

```ini
# network.env
ADMIN_BIND=127.0.0.1
ADMIN_PORT=8080
# auto | ipv4 | ipv6；监听协议为 ipv6 时 Lucky 上游需写 [::1]:8080
ADMIN_IP_FAMILY=auto
# auto | ipv4 | ipv6；作用于镜像探测、AList、GitHub 等 reqwest 出站请求
OUTBOUND_IP_FAMILY=auto
PUBLIC_DOMAINS=themes.example.com

# config.env
PUBLIC_URL=https://themes.example.com/
ADMIN_URL=https://themes.example.com/admin/
# 站点维护开关；通常由管理后台「站点维护」卡片修改
MAINTENANCE=0
# 性能配置；通常由管理后台「性能」页修改
PERF_COMPRESSION_ENABLED=1
PERF_COMPRESSION_GZIP=1
PERF_COMPRESSION_ZSTD=1
PERF_COMPRESSION_MIN_BYTES=256
PERF_COMPRESSION_QUALITY=default
PERF_DOWNLOAD_BUFFER_BYTES=65536
PERF_UPLOAD_CONCURRENCY=4
PERF_FLUSH_INTERVAL_SECS=3
PERF_POLL_INTERVAL_SECS=30
PERF_WORKER_THREADS=0
PERF_BLOCKING_THREADS=64
PERF_DB_POOL_SIZE=4
# API 实时使用监控；默认开启，单请求正文上限 128 KiB，活跃连接共享 32 MiB 抓取缓存
API_MONITOR_ENABLED=1
API_MONITOR_BODY_LIMIT=131072
API_MONITOR_CAPTURE_BUDGET=33554432
API_MONITOR_RETENTION=2000000
```

### `account.env`

```ini
ACCOUNT_URL=https://themes.example.com/account/
GITHUB_CALLBACK_URL=https://themes.example.com/account/api/auth/github/callback
GITHUB_CLIENT_ID=
GITHUB_CLIENT_SECRET=
# Web 用户中心/管理后台会话时长（秒），也可在管理后台「认证」页修改；默认 30 天
WEB_SESSION_TTL_SECS=2592000
SMTP_HOST=
SMTP_PORT=587
SMTP_USERNAME=
SMTP_PASSWORD=
SMTP_FROM=
SMTP_TLS=starttls
```

未配置 SMTP 时无法注册邮箱账号；未配置 GitHub 时 GitHub 登录按钮隐藏。管理员也可以登录后在「认证」页修改这些配置。GitHub 登录需要在 GitHub 创建 OAuth App，并把回调地址登记为 `GITHUB_CALLBACK_URL`。

### 数据库布局（分库）

当前版本运行在分库布局：`account.db`（主库）+ `authors.db` + `social.db` + `stats.db` + `audit.db`，默认位于 `/var/lib/sukisu-theme-web/db-split/`。生产环境可通过 `config.env` 的 `THEME_DB_DIR` 指向其他磁盘；本机部署在 `/data/sda1/sukisu-theme-web/db-split/`（根分区只有 6GB），备份必须包含该目录。

- 进程仍是同一个连接池；每条连接通过 SQLite `ATTACH` 挂载 5 个文件，所以跨库 JOIN 仍然可用。
- SQLite 每个文件仍只有一个写者，但账号、作者、评论/关注、下载统计、审计的写可以落在不同文件上并行。
- 跨库外键和跨库事务不可用；删除用户/作者由应用按固定顺序清理，并每小时运行孤儿修复。
- `config.env` 中的 `THEME_DB_LAYOUT=split` 和 `THEME_DB_DIR` 记录布局与目录。
- 新安装或从单库升级前，先停止服务并运行：

```bash
/opt/sukisu-theme-web/sukisu-theme-web-rs migrate-db
```

  首次安装（没有旧 `account.db`）会创建空的 5 个库；从单库升级会复制旧表并保留旧 `account.db` 作为回滚快照。迁移期间不要启动服务。
- 回滚：停止服务，恢复旧二进制和迁移前的 `account.db`，删除 `config.env` 中的 `THEME_DB_LAYOUT` / `THEME_DB_DIR`，再启动。迁移后的新写入会丢失，必须以备份为准。
- 管理后台「性能」页会显示 5 个库的文件大小、WAL、表和行数。

### 站点维护模式

管理后台首页的「站点维护」卡片可以一键开启或关闭维护。状态保存在 `config.env` 的 `MAINTENANCE`，重启后仍然有效。

- 管理员（管理后台会话或 `role=admin` 的账号会话）不受影响，可以继续使用管理后台、账号中心和关闭维护。
- 普通访客的页面请求返回 `503` 维护页；账号中心 API 返回 `503` JSON（`maintenance: true`）。
- 健康检查、登录/找回密码页面及其所需的认证 API 保持可用，避免管理员被锁在维护模式之外。
- `/catalog.json` 保持可读，并写入 `"maintenance": true`；管理器读取后显示维护提示，不再展示主题列表。
- 维护期间主题包资源由应用统一返回维护页，不再有绕过应用的静态文件服务。

### 存储位置、溢出与导入

管理后台「存储位置」页支持配置多个来源，默认为只读导入；也可以标记为可写：

- **主存储**：仍然是上传/发布的第一选择，配额在「存储」页设置。
- **可写来源**：填写独立配额和优先级。主存储达到配额或磁盘剩余阈值后，新主题按优先级写入下一个可用位置；旧主题保留原位置继续服务。
- **本地 / NAS**：填写服务器上的绝对目录；作为可写位置时，会在该根目录下创建 `themes/`、`previews/`、`screenshots/`、`avatars/`。NAS 需先在系统挂载 SMB/NFS。
- **OpenList / AList**：支持目录浏览、上传、建目录、删除和下载（含 Range）；云盘分享仍需先在 AList/OpenList 中挂载。应用不实现分享链接破解或限速绕过。
- **导入**：浏览来源目录，选择 `.sukisu` 包、预览图和可选头像/截图，填写元数据后直接发布；导入同样参与溢出选路。
- 每个主题在 `meta` 中记录 `storageId`，资源 key 使用 `_loc/<sourceId>` 段；删除、撤回、预览和下载会按 key 解析回原位置。
- `sources.json` 保存来源、Token 和配额（权限 0600）；备份时需要包含该文件。

## 部署（Linux + systemd + Lucky）

### 1. 安装二进制和目录

```bash
install -d /opt/sukisu-theme-web /srv/sukisu-themes \
  /etc/sukisu-theme-web /etc/sukisu-themes /var/lib/sukisu-theme-web
install -m 0755 target/aarch64-unknown-linux-musl/release/sukisu-theme-web \
  /opt/sukisu-theme-web/sukisu-theme-web-rs
```

生产环境通过 `/etc/systemd/system/sukisu-theme-web.service.d/static.conf` 设置 `THEME_STATIC_DIR=/opt/sukisu-theme-web/static`：静态文件优先从磁盘读取，缺失时回退到二进制内嵌副本。**只改 HTML/JS/语言包时不需要重新编译或重启**：

```bash
bash scripts/hot-static.sh
```

脚本会把整个 `static/`（约 200KB 压缩后）传到服务器，服务在下次请求时直接读取新文件。浏览器有缓存时按 `Ctrl+F5`。

Rust 后端改动仍需构建并替换二进制，可用 xz 压缩上传脚本（自动备份并在启动失败时回滚）：

```bash
powershell -ExecutionPolicy Bypass -File scripts/build-aarch64.ps1
bash scripts/deploy-binary.sh
```

`deploy-binary.sh` 会上传 xz 压缩后的二进制并重启服务；静态文件不受影响，继续由磁盘目录提供。

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

服务默认以 root 运行，因为需要写存储目录、SQLite 和 `/etc/sukisu-theme-web`，并调用 `systemctl`。

```bash
systemctl daemon-reload
systemctl enable --now sukisu-theme-web
```

### 4. 直接监听与 Lucky

服务内置静态资源、令牌门禁、`/admin` 前缀处理和根路径跳转，默认直接监听 `127.0.0.1:8080`。前置 Lucky 只需终止 TLS 并把站点反代到该地址：

- Lucky 目标填写 `http://127.0.0.1:8080`；保留原始 Host 和路径，不需要改写 `/admin`。
- 根路径 `/` 返回 `302` 到 `/account/login`，管理后台在 `/admin/`。
- `/themes/*`、`/previews/*`、`/screenshots/*`、`/avatars/*`、`/catalog.json` 校验 `X-Sukisu-Theme-Token`（下载权限）；`/submit` 需要上传权限；主令牌两者皆可。`/i18n.js`、`/locales/*` 公开。
- 如果 Lucky 与服务不在同一台机器，或需要直接对外，可把 `ADMIN_BIND` 改为 `0.0.0.0` 并自行配置防火墙。
- 修改端口或监听地址后，管理服务会重启；管理后台「网络」页会自动处理并回滚失败配置。

```ini
# /etc/sukisu-theme-web/network.env
ADMIN_BIND=127.0.0.1
ADMIN_PORT=8080
PUBLIC_DOMAINS=themes.example.com
```

### 5. 初始化

浏览器打开：

```text
https://<域名>/admin/setup
```

创建管理员、选择存储目录和配额。完成后即可登录管理后台。若使用 `admin.env` 预置管理员，直接访问 `/account/login`。

### 6. 验证

```bash
curl -fsS http://127.0.0.1:8080/health
curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/account/login
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
| `/var/lib/sukisu-theme-web/db-split/` | SQLite 分库文件（含 `-wal` / `-shm`） |
| `/var/lib/sukisu-theme-web/account.db` | 迁移前单库快照（回滚用） |
| `/etc/sukisu-theme-web`、`/etc/sukisu-themes` | 配置和访问令牌 |
| `/etc/sukisu-theme-web/sources.json` | 存储位置、配额和 AList Token |

## 多语言

- 语言：`zh-CN`（HTML 原文）、`en`（`static/locales/en.json`）。
- 新增语言：复制 `en.json` 为 `static/locales/<code>.json` 并翻译；在 `static/i18n.js` 的 `LANGUAGES` 注册；在 `src/api.rs` 添加 `include_str!` 常量和 `embedded_static` 分支。
- 静态资源嵌入二进制，新增语言文件后必须重新构建。

## 常见问题

- **页面没有语言下拉框**：应用已公开 `/i18n.js` 和 `/locales/*`；若仍 401，检查 Lucky/Nginx 是否额外加了鉴权。
- **`/account/i18n.js` 404**：正确路径是根路径 `/i18n.js`；不要加 `/account` 前缀。
- **改了 HTML/JS 线上没变化**：确认服务进程有 `THEME_STATIC_DIR=/opt/sukisu-theme-web/static`，并用 `bash scripts/hot-static.sh` 上传；浏览器再按 `Ctrl+F5`。Rust 代码改动仍需替换二进制。
- **页面 502 / 无法访问**：确认服务监听 `127.0.0.1:8080`，Lucky 目标端口一致；`/admin` 前缀由应用内部处理，不需要反代改写。
- **首次打不开初始化页**：确认没有预置 `admin.env`；已有管理员时请走 `/account/login`。
- **浏览器显示旧页面**：`Ctrl + F5` 强制刷新。

## 开发

```bash
cargo check
cargo test
cargo build --release
```

完整生产部署、Lucky 反代、回滚和真实主机信息见 [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)。
