# SukiSU Theme Mirror — Agent Notes

## Production deployment

- SSH: `root@ssh.neko.shirkneko.top` (Armbian, `linux-aarch64`), key `~/.ssh/id_ed25519`.
- Public entry: `https://themes.neko.shirkneko.top:8443/` → Lucky TLS → app `127.0.0.1:8080`. No Caddy in the chain.
- The SSH key has a passphrase. Never write the passphrase, tokens, or `store.env` contents into this repo. Ask the user to unlock the key, or use a session-only `SUKISU_SSH_PASS` with an askpass helper.
- Windows OpenSSH multiplexing (`ControlMaster` / `ControlPath`) is broken against this host (`mux_master_process_new_session: failed to receive fd 0`). Do not rely on it. Use one SSH invocation per operation, or stream a tar/script over stdin.
- Build: `powershell -ExecutionPolicy Bypass -File scripts/build-aarch64.ps1`
  - Output: `target/aarch64-unknown-linux-musl/release/sukisu-theme-web`
- Service: `sukisu-theme-web.service`
  - Unit: `/etc/systemd/system/sukisu-theme-web.service`
  - Binary: `/opt/sukisu-theme-web/sukisu-theme-web-rs`
  - Listens on `127.0.0.1:8080` (override with `ADMIN_PORT` in `network.env`).
  - `THEME_STATIC_DIR` is not set; HTML/CSS/JS/locales/logo are embedded in the binary. Any static change requires a rebuild and binary replacement.
- Storage locations: `/etc/sukisu-theme-web/sources.json` (`THEME_SOURCES_FILE` override), mode 0600.
  - Types: `local` (including OS-mounted NAS) and `alist` (OpenList/AList v3-compatible API, read/write).
  - Sources default to read-only import; set `writable` to use them as overflow locations. Each writable location has `quotaBytes` and `priority`.
  - Primary storage is always first. When full, new themes go to the next enabled writable location; existing themes stay at their original location and are resolved by `storageId`/`_loc` key prefix.
  - Import copies the `.sukisu` package, preview, optional avatar/screenshots into the selected primary/overflow location and publishes through the existing upload pipeline.
  - AList must already have the cloud drive/share mounted; do not implement share-link scraping or rate-limit bypass.
  - Back up `sources.json` together with the storage config; it contains the AList token.
- Built-in HTTP routing replaces the old Caddy layer: the app serves embedded UI assets, strips `/admin`, redirects `/` to `/account/login`, adds CORS/nosniff headers, and enforces `X-Sukisu-Theme-Token` for `/themes/*`, `/previews/*`, `/screenshots/*`, `/avatars/*` and `/catalog.json`. `/i18n.js` and `/locales/*` stay public.
- Backups: `/root/sukisu-deploy-backup/<timestamp>` (binary).
- Full runbook, verification commands, and rollback: `docs/DEPLOYMENT.md`.
- Mirror redirect configuration and verification: `docs/MIRRORS.md`.

## Auth and follow notes

- `account.env` holds `GITHUB_CLIENT_ID`, `GITHUB_CLIENT_SECRET`, `GITHUB_CALLBACK_URL` for GitHub OAuth. GitHub accounts may start without a password; users can set one and bind an email from the account center.
- Web user/admin sessions default to 30 days. The admin-only "网页会话时长" control on the Auth page writes `WEB_SESSION_TTL_SECS` (1-365 days) to `account.env`; App tokens stay at 30 days and existing sessions keep their old expiry until re-login.
- One-time key (device login) uses `device_login_codes`: the manager polls `account/api/auth/device/poll`, and the web page `account/device?code=...` must be confirmed while signed in. Codes expire after 10 minutes and are single-use.
- Follow relations live in `user_follows` (follower user → author hash) and are exposed through `account/api/follows` plus the admin `api/follows` page. Catalog entries now expose `authorHash`; rebuild `catalog.json` after deploying so old entries gain it.
- `user_preferences.show_r18_set` marks an explicit web R18 choice; a consent-only row leaves it unset. The manager pulls `/account/api/preferences` after sign-in, applies the web value once the device-local agreement exists, and only uploads its local value when the server has no explicit choice.

## Maintenance mode

- `MAINTENANCE=1` in `config.env` is the source of truth; `AppState.maintenance` mirrors it and `api/maintenance` toggles it, rebuilds `catalog.json` and writes an audit entry.
- The maintenance middleware allows the admin console (`/api*`, `/admin*`), health, setup, the login/recovery pages and their auth API paths, plus valid admin cookies/user sessions. Everything else gets a 503 maintenance page or JSON.
- `catalog.json` stays reachable and carries `"maintenance": true`, so managers can stop pulling themes. Theme package assets are served by the app and follow the same maintenance rules.

## Product notes

- UI languages: Chinese (source in HTML) + English (`static/locales/en.json`); switcher picks the browser language on first visit and stores the choice in `localStorage.sukisu-lang`.
- Native file-input labels (`选择文件` / `未选择任何文件`) come from the browser and are intentionally not translated.
- Mention syntax supports Unicode usernames (e.g. `樱檩殇雪@coolapk`) across GitHub, Gitee, Bilibili, Coolapk, Telegram, Weibo, Xiaohongshu, XDA, YouTube, Afdian, X, Zhihu, Threads, Douyin.
- Storage locations: primary local/S3 is the first write target; writable NAS/OpenList/AList sources are overflow targets. Existing themes stay on their original location; `Meta.storageId` and `_loc` key segments resolve them.
- Multiple theme access tokens: `src/theme_tokens.rs` stores named tokens (SHA-256 hashed) in the `theme_tokens` table with scopes `read` (catalog/assets) and `upload` (`/submit`), expiry, enable/disable and last-used (throttled write). The `store.env` `THEME_STORE_TOKEN` remains the full-access master fallback. Admin UI lives on the settings page (`/admin/api/tokens*`); plaintext is shown once at creation. `status.tokenSet` is true when either master or live tokens exist.
- Database layout is split: `account.db` (main) + `authors.db` + `social.db` + `stats.db` + `audit.db` under `/var/lib/sukisu-theme-web/db-split/`. Each pooled connection ATTACHes the four side files, so cross-file JOINs still work. SQL must qualify moved tables (`authors.authors`, `social.user_follows`, `stats.theme_stats`, `audit.audit_log`). Runtime refuses to start in single layout; run the binary with `migrate-db` while the service is stopped. Legacy `account.db` is kept untouched for rollback.
- Cross-file foreign keys/transactions are not available. `authors::delete_authors_for_user` and `accounts::delete_user` perform ordered cleanup; `Database::repair_orphans()` runs at startup and hourly.
- Backups must include `db-split/*.db*`; `account.db` is only the pre-migration snapshot.
- Admin UI pages are merged: `#access` contains access info + API endpoints, `#storage` contains quota/back-end + storage locations/import. `#user/<id>` is a standalone user detail page; theme preview (`#themes`) is read-only (tag/delete actions live on #upload).
- Every page renders a `PAGE_GUIDES` "what/capabilities/how" card. Audit entries are rendered with Chinese action/target labels and support actor/action/IP filters (`audit_log.ip` is filled from the request context task-local).
- Performance page and top bar poll `/admin/api/performance/live` for download/upload speed, active requests/uploads and active asset paths. Performance settings live in `config.env` as `PERF_*`; compression enabled/min bytes, download buffer, flush interval and poll interval are hot, while codecs/quality, upload concurrency, worker/blocking threads and DB pool need a restart.
- The app compresses text/JSON/JS/SVG with gzip/zstd; images, `.sukisu` packages, 206 responses and already-encoded bodies are skipped. Static ETags are weak (`W/"..."`).

## Mirror notes

- `src/mirrors.rs` selects optional HTTP mirrors for fresh package downloads and answers `302 Location: <mirror>/themes/...?sukisu_mirror=1`. Managers stay plain HTTP clients; there is no WebRTC/P2P dependency in the app.
- Config: `THEME_MIRRORS_FILE` (default `/etc/sukisu-theme-web/mirrors.json`), reloaded every 30 seconds. Entries: `name`, `baseUrl`, `token`, `priority`, `enabled`.
- Only `themes/*` requests without a `Range` header are redirected. Resumed downloads stay local so a partial file never mixes origins. Mirrors must expose the same paths and byte-identical files.
- A candidate must pass a `HEAD` probe (falling back to `Range: bytes=0-0`) before it is used; 4-second total budget, then local serving. Success is cached 60s, failure 20s. Concurrent cold requests for the same path share one probe via an in-flight lock.
- S3 full downloads issue a single GET (no separate HEAD); Range requests still HEAD for total size. Local asset writes use async file IO; publish uploads run at most four in parallel.
- The `sukisu_mirror=1` marker prevents a self-misconfigured mirror from redirecting in a loop; self-mirror fallbacks do not double-count downloads.
- The manager forwards `X-Sukisu-Theme-Token` across the redirect. A mirror must accept the same token or be publicly readable; `token` in `mirrors.json` is only for the server-side probe.
- Uploads still use up to 4 concurrent resumable chunks (manager streams 128 KB reads, flushes every 256 KB); admin/account progress covers package + preview/avatar/screenshot bytes from per-chunk XHR progress.
- The Android manager no longer depends on `io.github.webrtc-sdk:android`; keep it out of the APK to avoid ~30 MB of native libraries.
