# SukiSU Theme Mirror — Agent Notes

## Production deployment

- SSH: `root@ssh.neko.shirkneko.top` (Armbian, `linux-aarch64`), key `~/.ssh/id_ed25519`.
- Public entry: `https://themes.neko.shirkneko.top:8443/` → Lucky TLS → Caddy `127.0.0.1:8080` → app `127.0.0.1:8090`.
- The SSH key has a passphrase. Never write the passphrase, tokens, or `store.env` contents into this repo. Ask the user to unlock the key, or use a session-only `SUKISU_SSH_PASS` with an askpass helper.
- Windows OpenSSH multiplexing (`ControlMaster` / `ControlPath`) is broken against this host (`mux_master_process_new_session: failed to receive fd 0`). Do not rely on it. Use one SSH invocation per operation, or stream a tar/script over stdin.
- Build: `powershell -ExecutionPolicy Bypass -File scripts/build-aarch64.ps1`
  - Output: `target/aarch64-unknown-linux-musl/release/sukisu-theme-web`
- Service: `sukisu-theme-web.service`
  - Unit: `/etc/systemd/system/sukisu-theme-web.service`
  - Binary: `/opt/sukisu-theme-web/sukisu-theme-web-rs`
  - Listens on `127.0.0.1:8090`
  - `THEME_STATIC_DIR` is not set; HTML/CSS/JS/locales/logo are embedded in the binary. Any static change requires a rebuild and binary replacement.
- Storage locations: `/etc/sukisu-theme-web/sources.json` (`THEME_SOURCES_FILE` override), mode 0600.
  - Types: `local` (including OS-mounted NAS) and `alist` (OpenList/AList v3-compatible API, read/write).
  - Sources default to read-only import; set `writable` to use them as overflow locations. Each writable location has `quotaBytes` and `priority`.
  - Primary storage is always first. When full, new themes go to the next enabled writable location; existing themes stay at their original location and are resolved by `storageId`/`_loc` key prefix.
  - Import copies the `.sukisu` package, preview, optional avatar/screenshots into the selected primary/overflow location and publishes through the existing upload pipeline.
  - AList must already have the cloud drive/share mounted; do not implement share-link scraping or rate-limit bypass.
  - Back up `sources.json` together with the storage config; it contains the AList token.
- Caddy: `/etc/caddy/Caddyfile`
  - Keep `@ui_assets path /i18n.js /locales/*` + `handle @ui_assets` before the token gate, otherwise language assets return 401.
  - Root `/logo.svg` is still token-protected; pages use `/admin/logo.svg` or `/account/logo.svg`. Add any new public asset to `@ui_assets`.
  - `/admin*` is proxied with the prefix stripped; `/account*` is proxied unchanged.
  - Validate with the running Caddy environment (`/proc/<MainPID>/environ`) before `systemctl reload caddy`.
- Backups: `/root/sukisu-deploy-backup/<timestamp>` (binary) and `/root/caddy-deploy-backup/Caddyfile.<timestamp>`.
- Full runbook, verification commands, and rollback: `docs/DEPLOYMENT.md`.

## Product notes

- UI languages: Chinese (source in HTML) + English (`static/locales/en.json`); switcher picks the browser language on first visit and stores the choice in `localStorage.sukisu-lang`.
- Native file-input labels (`选择文件` / `未选择任何文件`) come from the browser and are intentionally not translated.
- Mention syntax supports Unicode usernames (e.g. `樱檩殇雪@coolapk`) across GitHub, Gitee, Bilibili, Coolapk, Telegram, Weibo, Xiaohongshu, XDA, YouTube, Afdian, X, Zhihu, Threads, Douyin.
- Storage locations: primary local/S3 is the first write target; writable NAS/OpenList/AList sources are overflow targets. Existing themes stay on their original location; `Meta.storageId` and `_loc` key segments resolve them.
