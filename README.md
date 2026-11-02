# SukiSU 主题镜像管理服务（Rust 版）

这是主题镜像站管理后台的 Rust 重写版，替代原来的 Python `app.py`，监听同一个 `127.0.0.1:8090`，对 Caddy 和现有管理后台页面保持接口兼容。

仓库：`sukisu-themes`

## 为什么用 Rust

- 主题包、预览图、截图、头像的读写和 Range 下载不再经过 Python 线程模型。
- 静态 musl 二进制，单文件部署，无 Python 运行时依赖（原 Python 版仍需 `python3`、Pillow、cryptography、boto3）。
- 图像解码/缩放、ZIP 校验、AES-GCM 解密、S3 请求都在 Tokio 异步运行时中执行，重 CPU 任务放到阻塞线程池。
- 保留原有配置文件、数据目录、catalog 格式和管理后台页面，切换成本低。

## 功能

- 主题目录 `/catalog.json` 的生成与重建。
- 投稿 `/submit`：multipart 解析、主题包 AES-GCM + ZIP 校验、图片缩放压缩、作者许可自动发布。
- 待审核队列：批准、拒绝、作者指纹。
- 作者头像：`avatar` 上传、`avatarPath` 目录字段、更新时保留旧头像。
- 本地磁盘 / S3 兼容对象存储两种资源后端。
- 资源下载支持 Range、公开路径仍由 Caddy 做令牌校验。
- 存储目录切换、后端切换、复制迁移、进度查询。
- 管理 API：状态、主题、待审核、作者、令牌、密码、catalog 测试/重建、Caddy reload。
- 管理后台 `index.html` 与 `logo.svg` 已通过 `include_str!` / `include_bytes!` 编译进二进制，生产环境不需要额外的静态目录。
- S3 兼容：AWS S3、Cloudflare R2、MinIO、Backblaze B2 等。

## 代码结构

```text
sukisu-themes/
├── Cargo.toml
├── .cargo/config.toml          # aarch64-musl 使用 rust-lld
├── scripts/
│   ├── build-aarch64.ps1       # Windows 交叉编译入口
│   ├── zigcc-aarch64-musl.bat  # Zig 作为 C 交叉编译器
│   └── ar-aarch64-musl.bat     # Zig ar
├── src/
│   ├── main.rs                 # 启动、配置、监听 127.0.0.1:8090
│   ├── api.rs                  # Axum 路由与全部 HTTP 处理
│   ├── config.rs               # env 文件、路径、限制、存储配置
│   ├── models.rs               # catalog/meta/pending/作者/任务结构
│   ├── storage.rs              # Local / S3 AssetStore、Range、迁移
│   ├── catalog.rs              # meta、catalog、pending、作者、用量
│   ├── uploads.rs              # multipart、上传校验、发布、pending
│   ├── images.rs               # 图片解码/EXIF/缩放/JPEG 编码
│   ├── package.rs              # .sukisu AES-GCM + ZIP 校验
│   ├── jobs.rs                 # 存储迁移任务与进度
│   ├── system.rs               # systemctl / caddy / 延迟重启
│   └── util.rs                 # 时间、哈希、Range、路径校验
├── static/
│   ├── index.html              # 管理后台页面（编译时内嵌）
│   └── logo.svg                # Logo（编译时内嵌）
└── README.md
```

## 本机编译与测试（Windows）

要求：

- Rust 1.85+（本机使用 1.97.1）
- `rustup target add aarch64-unknown-linux-musl`
- 交叉编译到 ARM 时需要 Zig（本机使用 0.16.0）和 `scripts/` 下的两个包装脚本

```bash
cd sukisu-themes
cargo test
cargo build
```

单元测试覆盖：

- 主题包校验（正确包、篡改包）
- 配置读写与存储前缀
- Range 解析
- 图片缩放并编码为 JPEG
- 本地资源 Range 读取
- Axum 通配路由

本地运行（不碰服务器配置）：

```powershell
$env:THEME_STORAGE_ENV="C:\temp\theme-test\storage.env"
$env:THEME_TOKEN_ENV="C:\temp\theme-test\store.env"
$env:THEME_ADMIN_ENV="C:\temp\theme-test\admin.env"
$env:THEME_ADMIN_CONFIG="C:\temp\theme-test\config.env"
$env:THEME_AUTHORS_FILE="C:\temp\theme-test\authors.json"
$env:THEME_STATIC_DIR="C:\temp\theme-test\static"
$env:THEME_DISABLE_SYSTEMCTL="1"
$env:ADMIN_PORT="8099"
cargo run
```

`THEME_DISABLE_SYSTEMCTL=1` 会跳过 `systemctl` 调用，方便本机测试。

测试目录中至少需要：

```text
storage.env       # THEME_STORAGE_ROOT=...、ASSET_BACKEND=local
store.env         # THEME_STORE_TOKEN=...
config.env        # PUBLIC_URL / ADMIN_URL
admin.env         # 可留空
authors.json      # {"version":1,"authors":{}}
static/           # 管理后台 index.html
state/            # 数据目录
```

## 交叉编译 Linux ARM64

本机已安装的 target：

```bash
rustup target list --installed
# aarch64-unknown-linux-musl
```

Windows 一键构建：

```powershell
cd sukisu-themes
powershell -ExecutionPolicy Bypass -File scripts/build-aarch64.ps1
```

产物：

```text
target/aarch64-unknown-linux-musl/release/sukisu-theme-admin
# ELF 64-bit LSB executable, ARM aarch64, statically linked, stripped
# 约 12 MB
```

脚本做的事：

1. `CC_aarch64_unknown_linux_musl` 指向 `zigcc-aarch64-musl.bat`
2. `AR_aarch64_unknown_linux_musl` 指向 `ar-aarch64-musl.bat`
3. `cargo build --release --target aarch64-unknown-linux-musl`
4. `.cargo/config.toml` 指定 `linker = "rust-lld"`

也可以在 aarch64 Linux 机器上直接构建：

```bash
cargo build --release
```

## 部署到服务器

当前服务器：

- 管理服务：`sukisu-theme-admin.service`
- 程序目录：`/opt/sukisu-theme-admin/`
- 程序目录：`/opt/sukisu-theme-admin/`
- Rust 二进制：`/opt/sukisu-theme-admin/sukisu-theme-admin-rs`（管理后台已内嵌）
- 数据目录：`/srv/sukisu-themes`
- 旧 Python 版、静态目录和备份已删除
- Caddy：`127.0.0.1:8080`，`/admin*` 和 `/submit` 反代到 `127.0.0.1:8090`

### 1. 上传并安装二进制

```bash
scp target/aarch64-unknown-linux-musl/release/sukisu-theme-admin user@host:/tmp/
ssh user@host
sudo install -m 755 -o root -g root /tmp/sukisu-theme-admin /opt/sukisu-theme-admin/sukisu-theme-admin-rs
```

### 2. 切换 systemd

```bash
sudo cp -a /etc/systemd/system/sukisu-theme-admin.service \
  /etc/systemd/system/sukisu-theme-admin.service.bak-$(date +%Y%m%d-%H%M%S)

sudo sed -i 's|^ExecStart=.*|ExecStart=/opt/sukisu-theme-admin/sukisu-theme-admin-rs|' \
  /etc/systemd/system/sukisu-theme-admin.service

# 可选：日志级别
sudo sed -i '/^Environment=ADMIN_PORT=/a Environment=RUST_LOG=info' \
  /etc/systemd/system/sukisu-theme-admin.service

sudo systemctl daemon-reload
sudo systemctl restart sukisu-theme-admin
systemctl status sukisu-theme-admin --no-pager
```

日志：

```bash
sudo journalctl -u sukisu-theme-admin -f
# 期望看到：
# sukisu-theme-admin listening on 127.0.0.1:8090
```

### 3. 验证

```bash
TOKEN=$(sudo sed -n 's/^THEME_STORE_TOKEN=//p' /etc/sukisu-themes/store.env | head -1)

curl -H "X-Sukisu-Theme-Token: $TOKEN" http://127.0.0.1:8080/catalog.json
curl -o /dev/null -w '%{http_code}\n' -H "X-Sukisu-Theme-Token: $TOKEN" \
  http://127.0.0.1:8080/previews/1-v1.webp
curl -o /dev/null -w '%{http_code}\n' -H "X-Sukisu-Theme-Token: $TOKEN" \
  -H 'Range: bytes=0-99' http://127.0.0.1:8080/themes/1-v1.sukisu
curl http://127.0.0.1:8090/api/status
curl http://127.0.0.1:8090/api/storage
```

浏览器打开 `https://themes.neko.shirkneko.top:8443/admin/`，确认概览、主题、待审核、存储页面正常。

管理后台页面和 `logo.svg` 已经编译进二进制；旧的 `static/` 目录已删除。需要临时替换页面时，可以设置 `THEME_STATIC_DIR` 指向一个目录，服务会优先读取该目录下的文件。

### 4. 升级与回滚

旧 Python 版、静态目录、unit 备份和 Caddy 备份已全部删除，当前只保留 Rust 二进制。

升级前如需保留回滚点，先复制当前二进制：

```bash
sudo cp -a /opt/sukisu-theme-admin/sukisu-theme-admin-rs \
  /root/sukisu-theme-admin-rs.previous
```

替换新二进制后重启：

```bash
sudo install -m 755 -o root -g root \
  target/aarch64-unknown-linux-musl/release/sukisu-theme-admin \
  /opt/sukisu-theme-admin/sukisu-theme-admin-rs
sudo systemctl restart sukisu-theme-admin
```

回滚时把 `/root/sukisu-theme-admin-rs.previous` 覆盖回去并重启即可。

Caddy 不需要改动：它只负责 `127.0.0.1:8090` 的反代和资源路径转发。

## 配置文件

Rust 版读取以下配置文件，路径与旧版一致：

| 文件 | 用途 |
| --- | --- |
| `/etc/sukisu-theme-admin/storage.env` | 存储目录、本地/S3 后端、S3 参数 |
| `/etc/sukisu-themes/store.env` | `THEME_STORE_TOKEN` |
| `/etc/sukisu-theme-admin/admin.env` | `ADMIN_PASSWORD_HASH` |
| `/etc/sukisu-theme-admin/config.env` | `PUBLIC_URL`、`ADMIN_URL` |
| `/etc/sukisu-theme-admin/authors.json` | 作者许可 |

本机测试可以用环境变量覆盖路径：

```text
THEME_STORAGE_ENV
THEME_TOKEN_ENV
THEME_ADMIN_ENV
THEME_ADMIN_PASSWORD_FILE
THEME_ADMIN_CONFIG
THEME_AUTHORS_FILE
THEME_STATIC_DIR
ADMIN_PORT
THEME_DISABLE_SYSTEMCTL
```

`storage.env` 字段：

```ini
THEME_STORAGE_ROOT=/srv/sukisu-themes
ASSET_BACKEND=local          # 或 s3
S3_ENDPOINT=
S3_REGION=us-east-1
S3_BUCKET=
S3_PREFIX=
S3_ACCESS_KEY=
S3_SECRET_KEY=
S3_PATH_STYLE=true
```

## 与旧 Python 版的差异（历史参考）

### 图片格式

为了在纯 Rust 中完成缩放并保持交叉编译简单，**需要重新编码的图片会输出 JPEG（质量 82，必要时降为 60）**，而不是 Python 版的 lossy WebP。

- 管理器上传的小尺寸 WebP（≤512 KB 且最长边未超限）会原样保留 `.webp`。
- 其他 PNG/JPEG/大 WebP 会输出 `.jpg`。
- 已存在的 `.webp` 资源继续按原文件服务，catalog 不变。
- 客户端和后台页面都能显示 JPEG，无需修改。

### 图像尺寸校验

使用 `image` crate 解码并检查最多 4000 万像素、最长边 20000px；会应用 EXIF 方向（JPEG/PNG 等）。

### 主题包校验

与旧版一致：

- 4 字节 `SKTH` magic
- 12 字节 nonce
- AES-256-GCM 解密
- ZIP 条目数 ≤32、解压总大小 ≤128 MB
- `theme.json` ≤64 KB、版本 1/2
- `hasBackground` / `hasCardBackground` 需要对应文件
- `card_sticker_*` 最多 5 个

### S3

使用 AWS SDK for Rust（`aws-sdk-s3`，rustls），支持自定义 endpoint / path-style。资源仍然由 Rust 服务代理返回，所以桶可以保持私有，Caddy 前面的令牌校验不变。

### 系统调用

- `systemctl is-active`
- `systemctl reload caddy`
- `caddy hash-password`
- 延迟重启使用 `systemd-run --on-active ...`，避免服务重启时子进程被同一 cgroup 杀掉

## API

管理接口（Basic Auth 由 Caddy 负责，写操作需要 `X-Requested-With: SukiSUThemeAdmin`）：

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/status` | 状态、catalog、用量、服务状态 |
| GET | `/api/themes` | 主题列表 |
| GET | `/api/pending` | 待审核列表 |
| GET | `/api/authors` | 作者许可 |
| GET | `/api/preview?path=...` | 预览图 / 截图 / 头像 / pending 图片 |
| GET | `/api/storage` | 存储配置与用量 |
| GET | `/api/storage/job` | 迁移任务进度 |
| POST | `/api/storage/test` | 测试本地目录或 S3 |
| POST | `/api/storage/root` | 修改数据目录并复制 |
| POST | `/api/storage/backend` | 切换后端并迁移 |
| POST | `/api/upload-theme` | 管理员上传 |
| POST | `/api/approve` | 批准待审核 |
| POST | `/api/reject` | 拒绝待审核 |
| POST | `/api/delete-theme` | 删除主题 |
| POST | `/api/author/revoke` | 撤销作者 |
| POST | `/api/sync` | 重建 catalog |
| POST | `/api/test-catalog` | 测试公开目录接口 |
| POST | `/api/reload` | `systemctl reload caddy` |
| POST | `/api/token` | 修改访问令牌 |
| POST | `/api/token/generate` | 生成令牌 |
| POST | `/api/password` | 修改后台密码 |

公开接口（Caddy 令牌校验）：

- `GET /catalog.json`
- `GET /themes/*`、`/previews/*`、`/screenshots/*`、`/avatars/*`（支持 Range）
- `POST /submit`（multipart，可选 `avatar`）
- `GET /health`（Caddy 处理）

## 存储迁移

后台「存储」页面的行为与旧版一致：

- 修改数据目录：目标必须为空；复制 `meta/`、`pending/`、`catalog.json` 和本地资源；成功后写 `storage.env` 并重启管理服务与 Caddy。
- 切换本地/S3：先复制全部资源并校验大小，成功后写配置并重启管理服务。
- 迁移后删除源文件默认关闭。
- 同一时间只允许一个存储任务；迁移期间拒绝投稿、发布、删除。
- catalog 与 meta 始终在本地数据目录，资源可以在桶里。

## 安全

- `storage.env`、`store.env`、`admin.env`、`authors.json` 必须 `0600`、属主 root。
- 管理接口依赖 Caddy Basic Auth；写接口要求 CSRF 头。
- `/submit` 使用常量时间比较令牌。
- 资源路径校验前缀、`..`、反斜杠；存储目录校验绝对路径、系统目录、互相包含、可写性。
- S3 凭证不会通过 API 返回；后台只显示是否已配置。
- 桶保持私有即可，对外只暴露镜像域名和令牌。

## 已验证 / 未验证

已在本机测试：

- 单元测试 9 项通过
- 本地二进制完整流程：提交（含头像）→ 待审核 → 批准 → catalog `avatarPath` → 图片/包下载与 Range → 自动发布 → 删除
- 存储目录迁移任务、配置写入、本地存储测试
- 交叉编译 aarch64-musl 静态二进制

已在服务器验证：

- Rust 二进制在 8090 启动，隐藏 `static/` 目录后仍能返回内嵌的 `index.html`（77,790 字节）和 `logo.svg`（61,506 字节）
- Caddy 公开路径 catalog、预览、截图、主题包、Range、无令牌 401
- `/api/status`、`/api/storage`、后台页面静态资源
- 带头像的真实 `/submit` → 待审核 → 拒绝清理

未在真实 S3 桶上联调：本机只用 AWS SDK 编译验证和模拟流程；部署后可在后台「存储」页填写桶信息并点「测试连接」验证。

## 故障排查

| 现象 | 处理 |
| --- | --- |
| 服务启动失败 | `journalctl -u sukisu-theme-admin -n 100`；确认存储目录可读写、`storage.env` 格式正确 |
| 资源 404 | 确认 Caddy 代理到 8090；Rust 日志 `asset request: ...`；检查资源在本地或桶内 |
| 资源 502 | S3 读取失败：检查 endpoint、凭证、桶权限、网络 |
| 上传返回 400 | 看返回的中文错误；主题包格式、图片尺寸、作者 ID、配额 |
| 上传返回 401 | Caddy/客户端令牌不一致 |
| 存储任务无进度 | `GET /api/storage/job`；确认没有并发任务；查看日志 |
| 切换目录后 Caddy 不启动 | 检查 `storage.env` 的 `THEME_STORAGE_ROOT` 是否存在、可读；`journalctl -u caddy` |
| 回滚 / 升级 | 覆盖 `/opt/sukisu-theme-admin/sukisu-theme-admin-rs` 后 `systemctl restart`；升级前先备份当前二进制 |

## 部署记录

- 2026-11-03 00:22 切换 `sukisu-theme-admin.service` 到 Rust 二进制。
- 2026-11-03：旧 Python 版、`static/`、unit 备份、Caddy 备份和临时文件已全部删除。
- 当前部署目录只保留：
  - `/opt/sukisu-theme-admin/sukisu-theme-admin-rs`
- 管理后台页面与 Logo 已内嵌在二进制中。
