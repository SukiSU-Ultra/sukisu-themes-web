use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

pub const BASE_DIR: &str = "/opt/sukisu-theme-web";
pub const DEFAULT_STORAGE_ROOT: &str = "/srv/sukisu-themes";

pub const STORAGE_LIMIT_BYTES: u64 = 1200 * 1024 * 1024;
pub const MIN_STORAGE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_STORAGE_LIMIT_BYTES: u64 = 10 * 1024 * 1024 * 1024 * 1024;
pub const DISK_RESERVE_BYTES: u64 = 300 * 1024 * 1024;
pub const MAX_PACKAGE_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_IMAGE_INPUT_BYTES: usize = 12 * 1024 * 1024;
pub const PREVIEW_MAX_EDGE: u32 = 1280;
pub const AVATAR_MAX_EDGE: u32 = 256;
pub const SCREENSHOT_MAX_EDGE: u32 = 1080;
pub const WEBP_QUALITY: u8 = 82;
pub const WEBP_SKIP_BYTES: usize = 512 * 1024;
pub const MAX_SCREENSHOTS: usize = 6;
pub const MAX_PENDING_SUBMISSIONS: usize = 35;
pub const MAX_SUBMISSIONS_PER_HOUR: usize = 200;
pub const MAX_BODY_BYTES: usize = 8 * 1024;
pub const MAX_UPLOAD_BODY_BYTES: usize = 256 * 1024 * 1024;
pub const MIN_FORMAT_VERSION: u32 = 1;
pub const CURRENT_FORMAT_VERSION: u32 = 2;

pub const TOKEN_HEADER: &str = "X-Sukisu-Theme-Token";
pub const CSRF_HEADER: &str = "X-Requested-With";
pub const CSRF_VALUE: &str = "SukiSUThemeAdmin";
pub const PACKAGE_MAGIC: &[u8] = b"SKTH";
pub const PACKAGE_KEY: [u8; 32] = [
    0x41, 0xDC, 0x17, 0x4E, 0x50, 0x96, 0xB4, 0xEE, 0x93, 0x31, 0x1C, 0xC8, 0x92, 0xB1, 0x32, 0xE2,
    0x19, 0x61, 0x01, 0x2B, 0x14, 0xCF, 0x21, 0x79, 0x54, 0x0F, 0x20, 0x3C, 0xA7, 0x2A, 0xB9, 0xC1,
];

pub const ASSET_DIRS: [&str; 4] = ["themes", "previews", "screenshots", "avatars"];
pub const ASSET_PREFIXES: [&str; 4] = ["themes/", "previews/", "screenshots/", "avatars/"];
pub const IMAGE_EXTENSIONS: [&str; 4] = [".webp", ".png", ".jpg", ".jpeg"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Local,
    S3,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Local => "local",
            Backend::S3 => "s3",
        }
    }
}

#[derive(Clone, Debug)]
pub struct StorageSettings {
    pub root: PathBuf,
    pub backend: Backend,
    pub limit_bytes: u64,
    pub s3_endpoint: String,
    pub s3_region: String,
    pub s3_bucket: String,
    pub s3_prefix: String,
    pub s3_access_key: String,
    pub s3_secret_key: String,
    pub s3_path_style: bool,
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
    pub meta: PathBuf,
    pub pending: PathBuf,
    pub uploads: PathBuf,
    pub catalog: PathBuf,
    pub static_dir: PathBuf,
    pub authors_file: PathBuf,
    pub token_env: PathBuf,
    pub admin_env: PathBuf,
    pub admin_password_file: PathBuf,
    pub admin_config: PathBuf,
    pub storage_env: PathBuf,
    pub account_env: PathBuf,
    pub network_env: PathBuf,
    pub sources_file: PathBuf,
    pub caddyfile: PathBuf,
    pub db: PathBuf,
}

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var(name)
        .ok()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(default))
}

pub fn load_paths(settings: &StorageSettings) -> Paths {
    let root = settings.root.clone();
    Paths {
        meta: root.join("meta"),
        pending: root.join("pending"),
        uploads: root.join("uploads"),
        catalog: root.join("catalog.json"),
        root,
        static_dir: env_path("THEME_STATIC_DIR", &format!("{BASE_DIR}/static")),
        authors_file: env_path("THEME_AUTHORS_FILE", "/etc/sukisu-theme-web/authors.json"),
        token_env: env_path("THEME_TOKEN_ENV", "/etc/sukisu-themes/store.env"),
        admin_env: env_path("THEME_ADMIN_ENV", "/etc/sukisu-theme-web/admin.env"),
        admin_password_file: env_path(
            "THEME_ADMIN_PASSWORD_FILE",
            "/etc/sukisu-theme-web/admin-password",
        ),
        admin_config: env_path("THEME_ADMIN_CONFIG", "/etc/sukisu-theme-web/config.env"),
        storage_env: env_path("THEME_STORAGE_ENV", "/etc/sukisu-theme-web/storage.env"),
        account_env: env_path("THEME_ACCOUNT_ENV", "/etc/sukisu-theme-web/account.env"),
        network_env: env_path("THEME_NETWORK_ENV", "/etc/sukisu-theme-web/network.env"),
        sources_file: env_path("THEME_SOURCES_FILE", "/etc/sukisu-theme-web/sources.json"),
        caddyfile: env_path("THEME_CADDYFILE", "/etc/caddy/Caddyfile"),
        db: env_path("THEME_DB_PATH", "/var/lib/sukisu-theme-web/account.db"),
    }
}

pub fn read_env(path: &Path) -> BTreeMap<String, String> {
    let mut data = BTreeMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return data;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        data.insert(key.trim().to_string(), value.trim().to_string());
    }
    data
}

pub fn normalize_prefix(value: &str) -> String {
    let mut prefix = value.trim().trim_start_matches('/').to_string();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    prefix
}

fn parse_bool(value: &str, default: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}

pub fn parse_storage_limit(value: &str) -> u64 {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|bytes| (MIN_STORAGE_LIMIT_BYTES..=MAX_STORAGE_LIMIT_BYTES).contains(bytes))
        .unwrap_or(STORAGE_LIMIT_BYTES)
}

pub fn validate_storage_limit(bytes: u64) -> Result<u64> {
    if bytes < MIN_STORAGE_LIMIT_BYTES {
        bail!("存储配额不能小于 64 MB");
    }
    if bytes > MAX_STORAGE_LIMIT_BYTES {
        bail!("存储配额不能超过 10 TB");
    }
    Ok(bytes)
}

pub fn format_storage_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;
    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    }
}

pub fn read_storage_config(env_path: &Path) -> StorageSettings {
    let stored = read_env(env_path);
    let root_text = stored
        .get("THEME_STORAGE_ROOT")
        .map(String::as_str)
        .unwrap_or("")
        .trim();
    let root = if root_text.is_empty() {
        PathBuf::from(DEFAULT_STORAGE_ROOT)
    } else {
        PathBuf::from(root_text)
    };
    let backend = match stored
        .get("ASSET_BACKEND")
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "local".to_string())
        .as_str()
    {
        "s3" => Backend::S3,
        _ => Backend::Local,
    };
    StorageSettings {
        root,
        backend,
        limit_bytes: parse_storage_limit(
            stored
                .get("THEME_STORAGE_LIMIT_BYTES")
                .map(String::as_str)
                .unwrap_or(""),
        ),
        s3_endpoint: stored.get("S3_ENDPOINT").cloned().unwrap_or_default(),
        s3_region: stored
            .get("S3_REGION")
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "us-east-1".to_string()),
        s3_bucket: stored.get("S3_BUCKET").cloned().unwrap_or_default(),
        s3_prefix: normalize_prefix(stored.get("S3_PREFIX").map(String::as_str).unwrap_or("")),
        s3_access_key: stored.get("S3_ACCESS_KEY").cloned().unwrap_or_default(),
        s3_secret_key: stored.get("S3_SECRET_KEY").cloned().unwrap_or_default(),
        s3_path_style: parse_bool(
            stored
                .get("S3_PATH_STYLE")
                .map(String::as_str)
                .unwrap_or("true"),
            true,
        ),
    }
}

fn set_permissions(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp = path.with_file_name(format!(".{}.{}.tmp", file_name, std::process::id()));
    fs::write(&temp, data)?;
    set_permissions(&temp, mode)?;
    fs::rename(&temp, path)?;
    Ok(())
}

pub fn write_env_map(path: &Path, values: &BTreeMap<String, String>) -> io::Result<()> {
    let mut text = String::new();
    for (key, value) in values {
        text.push_str(key);
        text.push('=');
        text.push_str(value);
        text.push('\n');
    }
    write_atomic(path, text.as_bytes(), 0o600)
}

pub fn set_env_key(path: &Path, key: &str, value: &str) -> io::Result<()> {
    let mut values = read_env(path);
    values.insert(key.to_string(), value.to_string());
    write_env_map(path, &values)
}

pub fn remove_env_key(path: &Path, key: &str) -> io::Result<()> {
    let mut values = read_env(path);
    values.remove(key);
    write_env_map(path, &values)
}

pub fn write_storage_config(path: &Path, settings: &StorageSettings) -> io::Result<()> {
    let mut values = BTreeMap::new();
    values.insert(
        "THEME_STORAGE_ROOT".to_string(),
        settings.root.to_string_lossy().to_string(),
    );
    values.insert(
        "ASSET_BACKEND".to_string(),
        settings.backend.as_str().to_string(),
    );
    values.insert(
        "THEME_STORAGE_LIMIT_BYTES".to_string(),
        settings.limit_bytes.to_string(),
    );
    values.insert("S3_ENDPOINT".to_string(), settings.s3_endpoint.clone());
    values.insert("S3_REGION".to_string(), settings.s3_region.clone());
    values.insert("S3_BUCKET".to_string(), settings.s3_bucket.clone());
    values.insert("S3_PREFIX".to_string(), settings.s3_prefix.clone());
    values.insert("S3_ACCESS_KEY".to_string(), settings.s3_access_key.clone());
    values.insert("S3_SECRET_KEY".to_string(), settings.s3_secret_key.clone());
    values.insert(
        "S3_PATH_STYLE".to_string(),
        if settings.s3_path_style {
            "true"
        } else {
            "false"
        }
        .to_string(),
    );
    let mut text = String::from(
        "# SukiSU theme storage. Managed by the admin console; restart required after edits.\n",
    );
    for (key, value) in values {
        text.push_str(&key);
        text.push('=');
        text.push_str(&value);
        text.push('\n');
    }
    write_atomic(path, text.as_bytes(), 0o600)
}

pub fn ensure_storage_dirs(paths: &Paths, settings: &StorageSettings) -> Result<()> {
    fs::create_dir_all(&paths.root)
        .with_context(|| format!("无法创建存储目录 {}", paths.root.display()))?;
    fs::create_dir_all(&paths.meta)?;
    fs::create_dir_all(&paths.pending)?;
    fs::create_dir_all(&paths.uploads)?;
    if settings.backend == Backend::Local {
        for name in ASSET_DIRS {
            fs::create_dir_all(paths.root.join(name))?;
        }
    }
    Ok(())
}

fn has_parent(path: &Path, parent: &Path) -> bool {
    path.components().count() > parent.components().count() && path.starts_with(parent)
}

pub fn validate_storage_root(value: &str, current_root: Option<&Path>) -> Result<PathBuf> {
    let text = value.trim();
    if text.is_empty() {
        bail!("存储目录不能为空");
    }
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        bail!("存储目录必须是绝对路径");
    }
    let absolute =
        std::path::absolute(&path).with_context(|| format!("无法解析目录 {}", path.display()))?;
    let resolved = absolute.canonicalize().unwrap_or(absolute);
    let denied: Vec<PathBuf> = [
        "/", "/etc", "/usr", "/boot", "/dev", "/proc", "/sys", BASE_DIR,
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    if denied.iter().any(|candidate| candidate == &resolved) {
        bail!("这个目录不允许作为存储目录");
    }
    if resolved.components().count() < 3 {
        bail!("目录层级太浅，请使用更具体的路径");
    }
    let base = PathBuf::from(BASE_DIR);
    if has_parent(&resolved, &base) {
        bail!("存储目录不能放在管理后台程序目录内");
    }
    if let Some(current) = current_root {
        let current = current
            .canonicalize()
            .unwrap_or_else(|_| current.to_path_buf());
        if resolved == current {
            return Ok(resolved);
        }
        if has_parent(&resolved, &current) || has_parent(&current, &resolved) {
            bail!("新目录不能与当前存储目录互相包含");
        }
    }
    fs::create_dir_all(&resolved).with_context(|| format!("目录不可写: {}", resolved.display()))?;
    let probe = resolved.join(format!(".storage-probe-{}", crate::util::random_hex(4)));
    fs::write(&probe, b"ok").with_context(|| format!("目录不可写: {}", resolved.display()))?;
    let _ = fs::remove_file(&probe);
    Ok(resolved)
}

pub fn merge_s3_settings(
    base: &StorageSettings,
    body: &serde_json::Value,
) -> Result<StorageSettings> {
    let mut settings = base.clone();
    if let Some(backend) = body.get("backend").and_then(|value| value.as_str()) {
        settings.backend = match backend.trim().to_ascii_lowercase().as_str() {
            "s3" => Backend::S3,
            "local" => Backend::Local,
            _ => bail!("存储后端只能是 local 或 s3"),
        };
    }
    if let Some(s3) = body.get("s3") {
        let s3 = s3
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("S3 配置格式无效"))?;
        if let Some(value) = s3.get("endpoint").and_then(|value| value.as_str()) {
            settings.s3_endpoint = value.trim().to_string();
        }
        if let Some(value) = s3.get("region").and_then(|value| value.as_str()) {
            let value = value.trim();
            settings.s3_region = if value.is_empty() {
                "us-east-1".to_string()
            } else {
                value.to_string()
            };
        }
        if let Some(value) = s3.get("bucket").and_then(|value| value.as_str()) {
            settings.s3_bucket = value.trim().to_string();
        }
        if let Some(value) = s3.get("prefix").and_then(|value| value.as_str()) {
            settings.s3_prefix = normalize_prefix(value);
        }
        if let Some(value) = s3.get("accessKey").and_then(|value| value.as_str()) {
            let value = value.trim();
            if !value.is_empty() {
                settings.s3_access_key = value.to_string();
            }
        }
        if let Some(value) = s3.get("secretKey").and_then(|value| value.as_str()) {
            let value = value.trim();
            if !value.is_empty() {
                settings.s3_secret_key = value.to_string();
            }
        }
        if let Some(value) = s3.get("pathStyle").and_then(|value| value.as_bool()) {
            settings.s3_path_style = value;
        }
    }
    Ok(settings)
}

pub fn validate_s3_settings(settings: &StorageSettings) -> Result<()> {
    if settings.backend != Backend::S3 {
        return Ok(());
    }
    if !settings.s3_endpoint.is_empty() {
        let parsed = reqwest::Url::parse(&settings.s3_endpoint)
            .context("S3 Endpoint 必须是 http(s) 地址")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            bail!("S3 Endpoint 必须是 http(s) 地址");
        }
    }
    let bucket = settings.s3_bucket.trim();
    let bucket_ok = bucket.len() >= 3
        && bucket.len() <= 63
        && bucket
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '-')
        && bucket
            .chars()
            .next()
            .map(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
            .unwrap_or(false)
        && bucket
            .chars()
            .last()
            .map(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
            .unwrap_or(false);
    if !bucket_ok {
        bail!("S3 存储桶名称无效");
    }
    if settings.s3_access_key.trim().is_empty() {
        bail!("需要填写 S3 Access Key");
    }
    if settings.s3_secret_key.trim().is_empty() {
        bail!("需要填写 S3 Secret Key");
    }
    if settings.s3_prefix.contains("..") {
        bail!("S3 前缀不能包含 ..");
    }
    Ok(())
}

pub fn path_part_is_safe(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains("..")
        && !value.contains('\\')
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSettings {
    pub bind: String,
    pub port: u16,
    pub domains: Vec<String>,
    pub public_url: String,
    pub admin_url: String,
    pub account_url: String,
    pub github_callback: String,
}

impl NetworkSettings {
    pub fn upstream(&self) -> String {
        let host = match self.bind.as_str() {
            "0.0.0.0" => "127.0.0.1".to_string(),
            "::" => "[::1]".to_string(),
            value if value.contains(':') => format!("[{value}]"),
            value => value.to_string(),
        };
        format!("{host}:{}", self.port)
    }
}

fn normalize_url(value: &str) -> Result<String> {
    let text = value.trim();
    let url = reqwest::Url::parse(text).with_context(|| format!("URL 无效: {text}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("URL 只支持 http/https");
    }
    if url.host_str().is_none() {
        bail!("URL 缺少域名");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("URL 不能包含查询参数或片段");
    }
    let mut url = url;
    if url.path().is_empty() {
        url.set_path("/");
    }
    Ok(url.to_string())
}

fn normalize_domain(value: &str) -> Result<String> {
    let mut text = value.trim().trim_end_matches('/').to_ascii_lowercase();
    if let Some(rest) = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
    {
        text = rest.split('/').next().unwrap_or("").to_string();
    }
    if text.is_empty() || text.contains('/') || text.contains(' ') || text.contains('@') {
        bail!("域名格式无效: {value}");
    }
    if !text
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':' | '[' | ']'))
    {
        bail!("域名格式无效: {value}");
    }
    Ok(text)
}

pub fn manager_version_code(paths: &Paths) -> i64 {
    read_env(&paths.admin_config)
        .get("MANAGER_VERSION_CODE")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(0)
}

pub fn read_network_settings(paths: &Paths) -> NetworkSettings {
    let network = read_env(&paths.network_env);
    let config = read_env(&paths.admin_config);
    let account = read_env(&paths.account_env);
    let bind = network
        .get("ADMIN_BIND")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var("ADMIN_BIND").ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = network
        .get("ADMIN_PORT")
        .and_then(|value| value.trim().parse::<u16>().ok())
        .or_else(|| {
            std::env::var("ADMIN_PORT")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(8090);
    let public_url = config
        .get("PUBLIC_URL")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8080/".to_string());
    let admin_url = config
        .get("ADMIN_URL")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}admin/",
                if public_url.ends_with('/') {
                    public_url.clone()
                } else {
                    format!("{public_url}/")
                }
            )
        });
    let account_url = account
        .get("ACCOUNT_URL")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}account/",
                if public_url.ends_with('/') {
                    public_url.clone()
                } else {
                    format!("{public_url}/")
                }
            )
        });
    let github_callback = account
        .get("GITHUB_CALLBACK_URL")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{}api/auth/github/callback", account_url));
    let domains = network
        .get("PUBLIC_DOMAINS")
        .map(|value| {
            value
                .split([',', '\n'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| {
            let mut values = Vec::new();
            for url in [&public_url, &admin_url, &account_url] {
                if let Ok(parsed) = reqwest::Url::parse(url) {
                    if let Some(host) = parsed.host_str() {
                        let host = match parsed.port() {
                            Some(port) => format!("{host}:{port}"),
                            None => host.to_string(),
                        };
                        if !values.contains(&host) {
                            values.push(host);
                        }
                    }
                }
            }
            values
        });
    NetworkSettings {
        bind,
        port,
        domains,
        public_url,
        admin_url,
        account_url,
        github_callback,
    }
}

pub fn validate_network(
    bind: &str,
    port: u16,
    domains: &[String],
    public_url: &str,
    admin_url: &str,
    account_url: &str,
    github_callback: &str,
) -> Result<NetworkSettings> {
    let bind = bind.trim().to_string();
    bind.parse::<std::net::IpAddr>()
        .with_context(|| format!("监听地址无效: {bind}"))?;
    if port == 0 {
        bail!("端口不能为 0");
    }
    if domains.is_empty() {
        bail!("至少需要一个公开域名");
    }
    let mut normalized_domains = Vec::new();
    for domain in domains {
        let domain = normalize_domain(domain)?;
        if !normalized_domains.contains(&domain) {
            normalized_domains.push(domain);
        }
    }
    let public_url = normalize_url(public_url)?;
    let admin_url = normalize_url(admin_url)?;
    let account_url = normalize_url(account_url)?;
    let github_callback = normalize_url(github_callback)?;
    Ok(NetworkSettings {
        bind,
        port,
        domains: normalized_domains,
        public_url,
        admin_url,
        account_url,
        github_callback,
    })
}

pub fn write_network_settings(paths: &Paths, settings: &NetworkSettings) -> Result<()> {
    let mut values = BTreeMap::new();
    values.insert("ADMIN_BIND".to_string(), settings.bind.clone());
    values.insert("ADMIN_PORT".to_string(), settings.port.to_string());
    values.insert("PUBLIC_DOMAINS".to_string(), settings.domains.join(","));
    write_env_map(&paths.network_env, &values)?;
    set_env_key(&paths.admin_config, "PUBLIC_URL", &settings.public_url)?;
    set_env_key(&paths.admin_config, "ADMIN_URL", &settings.admin_url)?;
    set_env_key(&paths.account_env, "ACCOUNT_URL", &settings.account_url)?;
    set_env_key(
        &paths.account_env,
        "GITHUB_CALLBACK_URL",
        &settings.github_callback,
    )?;
    Ok(())
}

pub fn update_caddy_upstream(path: &Path, old: &str, new: &str) -> Result<bool> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("无法读取 Caddyfile {}", path.display()))?;
    let old_line = format!("reverse_proxy {old}");
    let new_line = format!("reverse_proxy {new}");
    if !text.contains(&old_line) {
        return Ok(false);
    }
    let updated = text.replace(&old_line, &new_line);
    write_atomic(path, updated.as_bytes(), 0o644)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_normalization() {
        assert_eq!(normalize_prefix("foo"), "foo/");
        assert_eq!(normalize_prefix("/foo/"), "foo/");
        assert_eq!(normalize_prefix(""), "");
    }

    #[test]
    fn env_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage.env");
        let settings = StorageSettings {
            root: dir.path().join("data"),
            backend: Backend::S3,
            limit_bytes: 3 * 1024 * 1024 * 1024,
            s3_endpoint: "http://127.0.0.1:9000".to_string(),
            s3_region: "us-east-1".to_string(),
            s3_bucket: "bucket".to_string(),
            s3_prefix: "themes/".to_string(),
            s3_access_key: "key".to_string(),
            s3_secret_key: "secret".to_string(),
            s3_path_style: true,
        };
        write_storage_config(&path, &settings).unwrap();
        let loaded = read_storage_config(&path);
        assert_eq!(loaded.backend, Backend::S3);
        assert_eq!(loaded.s3_bucket, "bucket");
        assert_eq!(loaded.s3_prefix, "themes/");
        assert!(loaded.s3_path_style);
        assert_eq!(loaded.limit_bytes, 3 * 1024 * 1024 * 1024);
    }

    #[test]
    fn network_settings_validation() {
        let settings = validate_network(
            "127.0.0.1",
            8090,
            &[
                "themes.example.com".to_string(),
                "themes.example.com:8443".to_string(),
            ],
            "https://themes.example.com/",
            "https://themes.example.com/admin/",
            "https://themes.example.com/account/",
            "https://themes.example.com/account/api/auth/github/callback",
        )
        .unwrap();
        assert_eq!(settings.upstream(), "127.0.0.1:8090");
        assert_eq!(settings.domains.len(), 2);
        assert!(validate_network(
            "not-an-ip",
            8090,
            &["themes.example.com".to_string()],
            "https://themes.example.com/",
            "https://themes.example.com/admin/",
            "https://themes.example.com/account/",
            "https://themes.example.com/account/api/auth/github/callback",
        )
        .is_err());
        let wildcard = validate_network(
            "0.0.0.0",
            8091,
            &["themes.example.com".to_string()],
            "https://themes.example.com/",
            "https://themes.example.com/admin/",
            "https://themes.example.com/account/",
            "https://themes.example.com/account/api/auth/github/callback",
        )
        .unwrap();
        assert_eq!(wildcard.upstream(), "127.0.0.1:8091");
    }

    #[test]
    fn storage_limit_validation() {
        assert_eq!(parse_storage_limit("2147483648"), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_storage_limit("nope"), STORAGE_LIMIT_BYTES);
        assert!(validate_storage_limit(MIN_STORAGE_LIMIT_BYTES).is_ok());
        assert!(validate_storage_limit(MIN_STORAGE_LIMIT_BYTES - 1).is_err());
        assert!(validate_storage_limit(MAX_STORAGE_LIMIT_BYTES + 1).is_err());
        assert_eq!(format_storage_size(2 * 1024 * 1024 * 1024), "2.00 GB");
    }
}
