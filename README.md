# SukiSU 主题镜像

Rust 实现的主题镜像、投稿审核和账号系统，包含 Web 管理后台、用户中心和 Android 管理器接口。

## 功能

### 主题与投稿
- 生成主题目录 `catalog.json`，支持搜索、排序、预览图和截图。
- 主题包上传前本地校验，服务器再次校验 AES-GCM 和 ZIP 内容。
- 预览图、作者头像和截图自动压缩；支持 JPG、PNG、WebP、GIF。
- 支持更新已有主题；新主题自动分配 ID，格式为 `11010-01`、`11010-02`。
- 投稿进入待审核队列，管理员批准后发布；满足作者许可时可自动发布。
- 撤回主题保留 14 天，期间只有作者可以预览和下载，并可重新发布；到期自动永久删除。
- 网页和管理器显示上传进度、校验、待审核、已发布或已拒绝的完整状态。
- 每个主题包生成 SHA-256；网页详情展示，管理器下载时自动校验，不一致拒绝安装。
- 许可证选择、推荐标签、自定义标签、最低管理器版本。
- 主题描述支持 `用户名@github` 用户提及。

### 账号与作者
- 统一登录入口：管理员和用户使用同一个登录页，登录后按账号权限进入管理后台或用户中心。
- 用户注册、邮箱验证、密码重置、GitHub OAuth；邮箱验证后即可使用，不再等待账号审核。
- 用户资料、头像、密码、邮箱管理。
- 作者身份申请、绑定、复用、重置，管理员审核。
- 管理员可禁用、启用用户，重置密码，调整角色，查看操作日志。
- 账号、会话、作者、投稿和审计记录保存在 SQLite。

### 存储与网络
- 本地磁盘或 S3 兼容对象存储（AWS S3、Cloudflare R2、MinIO、Backblaze B2 等）。
- 可调整存储目录、存储后端和存储配额。
- 支持存储迁移和进度查询。
- 后台可修改监听地址、端口、公开域名和公开 URL，自动更新 Caddy 上游，失败自动回滚。

### 管理器
- 主题商店浏览、搜索、排序、下载和详情预览。
- 支持多个主题镜像源。
- 未登录可浏览和下载，投稿需要登录账号。
- 登录后使用账号作者身份投稿，服务器自动分配主题 ID。
- 已有作者头像时从服务器加载，不再显示头像选择器；新投稿自动复用。
- 预览图、头像、截图在本地统一转成 WebP 后上传，JPG 也可直接选择。
- 上传时显示本地校验、上传百分比、服务器校验和审核/发布结果。
- 主题详情显示 SHA-256；下载时校验文件大小和 SHA-256。
- 镜像源管理只显示目录、主题包、预览图、头像和健康检查等只读接口，不显示管理后台地址或管理 API。

## 使用

### 入口

| 地址 | 用途 |
| --- | --- |
| `/` | 跳转到统一登录/注册 |
| `/account/login` | 统一登录/注册 |
| `/account/` | 用户中心 |
| `/admin/` | 管理后台（管理员） |
| `/admin/setup` | 首次初始化 |

首次启动打开 `/admin/setup`，创建管理员账号并设置初始存储配额。初始化完成后该页面自动关闭。

### 管理后台

- **总览**：服务状态、主题数量、待审核数量、存储用量。
- **账号**：用户管理、作者申请、作者绑定/重置、操作日志。
- **主题**：上传、更新、审核、拒绝、删除、重建目录；点击主题卡片查看详情和完整 SHA-256。
- **系统**：认证配置、网络配置、存储配置、访问令牌、管理员密码。

管理员侧栏提供用户中心入口；管理员进入用户中心后也会看到“管理后台”按钮，两边使用同一个登录状态。

认证页可以配置 SMTP 和 GitHub OAuth，保存后立即生效。密码和 Secret 只写不回显，可发送测试邮件、检查 GitHub 参数。

网络页保存后会自动重启服务并 reload Caddy；新端口健康检查失败时会恢复旧配置。

### 用户中心

1. 注册账号并完成邮箱验证，验证后即可使用账号功能。
2. 申请作者身份，填写作者名称和作者 ID。
3. 管理员批准作者申请后即可上传主题。
5. 上传时显示文件上传进度和服务器校验状态。
6. 在「我的内容」查看投稿记录、完整状态和 SHA-256，审核通过后显示在主题商店。
7. 点击已发布主题可预览图片、查看详情、下载主题包或撤回下架；待审核投稿也可以撤回。
8. 已撤回主题会单独显示，14 天内可重新发布，且只有作者能预览和下载；到期自动删除。

作者 ID 重置后，旧主题仍归属于同一用户。

### Android 管理器

1. 在主题商店设置中填写镜像地址和访问令牌。
2. 打开账号页注册或登录。
3. 进入投稿页，主题 ID 自动获取；更新主题时选择已有主题。
4. 选择主题包、预览图、截图和作者头像。
5. 提交时查看本地校验、上传进度、服务器校验和审核/发布结果。
6. 在投稿页查看最近投稿状态，或到用户中心/管理后台查看完整流程。
7. 已通过的主题可在「我的投稿」中撤回下架；撤回后 14 天内可在「已撤回主题」中重新发布。

预览图/头像/截图会在管理器本地压缩并转换为 WebP，不需要手动转换。已有作者头像会直接使用服务器头像，只在该作者还没有头像时显示选择器。

主题详情页显示 SHA-256；下载时管理器会自动校验文件大小和 SHA-256，不一致会拒绝安装。

### 作者头像

- 作者第一次上传头像后保存到服务器。
- 后续投稿自动复用该头像，管理器不再显示头像选择器。
- 需要更换头像时，在网页端投稿表单重新选择作者头像。

## 运行

### 环境

- Rust 1.85+
- Caddy 或其他反向代理
- Linux / Windows / macOS

### 配置

| 文件 | 用途 |
| --- | --- |
| `/etc/sukisu-theme-web/storage.env` | 存储目录、后端、S3 参数、配额 |
| `/etc/sukisu-themes/store.env` | `THEME_STORE_TOKEN` |
| `/etc/sukisu-theme-web/admin.env` | 管理员用户名和密码哈希 |
| `/etc/sukisu-theme-web/config.env` | 公开地址、管理后台地址 |
| `/etc/sukisu-theme-web/account.env` | SMTP、GitHub OAuth、用户中心地址 |
| `/etc/sukisu-theme-web/network.env` | 监听地址、端口、公开域名 |

`storage.env`：

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

`account.env`：

```ini
ACCOUNT_URL=https://<镜像域名>/account/
GITHUB_CALLBACK_URL=https://<镜像域名>/account/api/auth/github/callback
GITHUB_CLIENT_ID=
GITHUB_CLIENT_SECRET=
SMTP_HOST=
SMTP_PORT=587
SMTP_USERNAME=
SMTP_PASSWORD=
SMTP_FROM=
SMTP_TLS=starttls
```

未配置 SMTP 时无法注册邮箱账号；未配置 GitHub 时用户中心隐藏 GitHub 登录按钮。

### 构建与启动

```bash
cargo build --release
./target/release/sukisu-theme-web
```

ARM64 Linux 静态二进制：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-aarch64.ps1
```

产物：

```text
target/aarch64-unknown-linux-musl/release/sukisu-theme-web
```

用 Caddy/Nginx 将 `/admin*`、`/account*`、`/submit`、`/themes/*`、`/previews/*`、`/screenshots/*`、`/avatars/*` 反代到服务端口。资源请求需要携带 `X-Sukisu-Theme-Token`。

### 数据目录

| 路径 | 内容 |
| --- | --- |
| `/srv/sukisu-themes` | 主题包、预览图、截图、头像、meta、catalog |
| `/var/lib/sukisu-theme-web/account.db` | SQLite 账号数据库 |
| `/etc/sukisu-theme-web` | 服务配置 |
| `/etc/sukisu-themes/store.env` | 主题商店访问令牌 |

备份时同时备份数据目录、SQLite 数据库（含 WAL/SHM）和 `/etc/sukisu-theme-web`、`/etc/sukisu-themes`。
