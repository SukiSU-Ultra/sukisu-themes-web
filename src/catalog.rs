use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::SystemTime;

use anyhow::Result;
use serde_json::{json, Value};

use crate::config::{write_atomic, Paths, CURRENT_FORMAT_VERSION, MIN_FORMAT_VERSION};
use crate::db::ThemeStats;
use crate::models::{
    AuthorRecord, AuthorsFile, CatalogEntry, CatalogFile, JobState, Meta, PendingMeta, StorageUsage,
};
use crate::storage::AssetStore;
use crate::util::{
    author_fingerprint, author_key, file_mtime_seconds, normalize_asset_key, now_iso, quote_query,
    valid_author_hash, valid_submission_id, valid_theme_id,
};

pub const WITHDRAW_RETENTION_DAYS: i64 = 14;

/// Bumped whenever theme metadata changes, so cached indexes can invalidate
/// without rescanning the meta directory on every request.
static META_REVISION: AtomicU64 = AtomicU64::new(1);
/// Bumped on every catalog.json write so parsed catalogs can be cached.
static CATALOG_REVISION: AtomicU64 = AtomicU64::new(1);

struct CachedCatalog {
    revision: u64,
    len: u64,
    modified: Option<SystemTime>,
    catalog: Arc<CatalogFile>,
}

static CATALOG_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedCatalog>>> = OnceLock::new();
/// Serializes read-modify-write cycles on catalog.json.
static CATALOG_WRITE_LOCK: Mutex<()> = Mutex::new(());
/// Keep mutations of one theme ordered without serializing unrelated uploads.
static THEME_LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();

pub fn theme_mutation_lock(id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let locks = THEME_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(id.to_string(), Arc::downgrade(&lock));
    lock
}

fn catalog_write_guard() -> std::sync::MutexGuard<'static, ()> {
    CATALOG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn meta_revision() -> u64 {
    META_REVISION.load(Ordering::Relaxed)
}

fn bump_meta_revision() {
    META_REVISION.fetch_add(1, Ordering::Relaxed);
}

fn bump_catalog_revision() {
    CATALOG_REVISION.fetch_add(1, Ordering::Relaxed);
}

/// In-memory view of `meta/*.json`, used by the user-center preview and
/// discovery paths that previously scanned every meta file per request.
#[derive(Default)]
pub struct MetaIndex {
    by_id: HashMap<String, Meta>,
    theme_for_asset: HashMap<String, String>,
    by_author_hash: HashMap<String, HashSet<String>>,
    by_legacy_name: HashMap<String, HashSet<String>>,
}

impl MetaIndex {
    pub fn theme_by_id(&self, id: &str) -> Option<&Meta> {
        self.by_id.get(id)
    }

    pub fn themes(&self) -> impl Iterator<Item = &Meta> {
        self.by_id.values()
    }

    pub fn theme_for_asset(&self, asset: &str) -> Option<&Meta> {
        self.theme_for_asset
            .get(asset)
            .and_then(|id| self.by_id.get(id))
    }

    /// Theme ids owned by the given author hashes, plus legacy themes whose
    /// empty author hash matched the account username.
    pub fn owned_ids(&self, hashes: &HashSet<String>, username: &str) -> HashSet<String> {
        let mut ids = HashSet::new();
        for hash in hashes {
            if let Some(owned) = self.by_author_hash.get(hash) {
                ids.extend(owned.iter().cloned());
            }
        }
        let username = username.trim().to_ascii_lowercase();
        if !username.is_empty() {
            if let Some(owned) = self.by_legacy_name.get(&username) {
                ids.extend(owned.iter().cloned());
            }
        }
        ids
    }
}

pub fn build_meta_index(paths: &Paths) -> MetaIndex {
    let mut index = MetaIndex::default();
    let Ok(entries) = fs::read_dir(&paths.meta) else {
        return index;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(meta) = read_meta_file(&path) else {
            continue;
        };
        let id = meta.id.clone();
        index.by_id.insert(id.clone(), meta.clone());
        for asset in [&meta.preview]
            .into_iter()
            .chain(meta.screenshots.iter())
            .chain(meta.avatar.iter())
            .filter(|asset| !asset.is_empty())
        {
            index.theme_for_asset.insert(asset.clone(), id.clone());
        }
        let author_hash = meta.author_hash.as_deref().unwrap_or("").trim();
        if author_hash.is_empty() {
            let author = meta.author.trim().to_ascii_lowercase();
            if !author.is_empty() {
                index.by_legacy_name.entry(author).or_default().insert(id);
            }
        } else {
            index
                .by_author_hash
                .entry(author_hash.to_string())
                .or_default()
                .insert(id);
        }
    }
    index
}

pub fn withdrawal_expires_at(withdrawn_at: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(withdrawn_at)
        .ok()
        .map(|time| {
            (time + chrono::Duration::days(WITHDRAW_RETENTION_DAYS))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
}

pub fn withdrawal_expired(withdrawn_at: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(withdrawn_at)
        .map(|time| time + chrono::Duration::days(WITHDRAW_RETENTION_DAYS) <= chrono::Utc::now())
        .unwrap_or(true)
}

pub fn read_meta_file(path: &Path) -> Option<Meta> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Older metadata predates the explicit `adaptive` flag; the canonical tag still marks it.
pub fn tags_mark_adaptive(tags: &[String]) -> bool {
    tags.iter().any(|tag| {
        matches!(
            tag.trim().to_lowercase().as_str(),
            "adaptive"
                | "auto"
                | "automatic"
                | "lightdark"
                | "light/dark"
                | "自适应"
                | "自适应主题"
                | "自适应主题包"
        )
    })
}

/// Tags that mark a theme as adult content. Kept in sync with the manager's fallback matcher.
pub fn is_r18_tag(tag: &str) -> bool {
    matches!(
        tag.trim().to_lowercase().as_str(),
        "r18" | "r-18" | "18+" | "nsfw" | "adult" | "成人" | "成人向" | "限制级"
    )
}

pub fn tags_mark_r18(tags: &[String]) -> bool {
    tags.iter().any(|tag| is_r18_tag(tag))
}

fn meta_is_r18(meta: &Meta) -> bool {
    tags_mark_r18(&meta.tags)
}

fn meta_is_adaptive(meta: &Meta) -> bool {
    meta.adaptive || tags_mark_adaptive(&meta.tags)
}

pub fn write_meta(path: &Path, meta: &Meta) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(meta)?;
    data.push(b'\n');
    write_atomic(path, &data, 0o644)?;
    bump_meta_revision();
    Ok(())
}

fn history_dir(paths: &Paths, id: &str) -> Option<PathBuf> {
    valid_theme_id(id).then(|| paths.root.join("version-history").join(id))
}

/// Preserve the published metadata before replacing its assets. Existing
/// archives are immutable so a repeated publish cannot rewrite history.
pub fn archive_version(paths: &Paths, meta: &Meta) -> Result<()> {
    let Some(dir) = history_dir(paths, &meta.id) else {
        anyhow::bail!("invalid theme id");
    };
    let path = dir.join(format!("v{}.json", meta.version));
    if !path.exists() {
        let mut data = serde_json::to_vec_pretty(meta)?;
        data.push(b'\n');
        write_atomic(&path, &data, 0o644)?;
    }
    Ok(())
}

pub fn theme_versions(paths: &Paths, id: &str) -> Vec<Meta> {
    let mut versions = Vec::new();
    if let Some((_, current)) = find_theme_meta(paths, id) {
        versions.push(current);
    }
    if let Some(dir) = history_dir(paths, id) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|ext| ext.to_str()) == Some("json") {
                    if let Some(meta) = read_meta_file(&entry.path()) {
                        if meta.id == id && !versions.iter().any(|item| item.version == meta.version) {
                            versions.push(meta);
                        }
                    }
                }
            }
        }
    }
    versions.sort_by(|left, right| right.version.cmp(&left.version));
    versions
}

pub async fn rollback_version(store: &AssetStore, paths: &Paths, id: &str, version: i64, expected: &Meta) -> Result<Meta> {
    let lock = theme_mutation_lock(id);
    let _guard = lock.lock().await;
    let Some((path, current)) = find_theme_meta(paths, id) else {
        anyhow::bail!("theme not found");
    };
    if current.version != expected.version
        || current.author_hash != expected.author_hash
        || current.author != expected.author
        || current.withdrawn_at != expected.withdrawn_at
        || current.package != expected.package
    {
        anyhow::bail!("theme changed during rollback");
    }
    if version >= current.version || current.version == i64::MAX {
        anyhow::bail!("rollback target must be an earlier version");
    }
    let Some(previous) = theme_versions(paths, id).into_iter().find(|item| item.version == version) else {
        anyhow::bail!("version not found");
    };
    if !store.exists(&previous.package).await || !store.exists(&previous.preview).await {
        anyhow::bail!("archived assets are missing");
    }
    archive_version(paths, &current)?;
    let mut restored = previous;
    restored.version = current.version + 1;
    // A rollback restores release content, never an obsolete owner identity.
    restored.author_hash = current.author_hash.clone();
    restored.author = current.author.clone();
    restored.updated_at = now_iso();
    restored.changelog = format!("回退至 v{version}");
    restored.withdrawn_at = None;
    write_meta(&path, &restored)?;
    if let Err(error) = rebuild_catalog(store, paths).await {
        if let Err(restore_error) = write_meta(&path, &current) {
            tracing::error!(?restore_error, theme_id = %id, "failed to restore theme metadata after rollback");
        }
        let _ = rebuild_catalog(store, paths).await;
        return Err(error);
    }
    Ok(restored)
}

pub fn find_theme_meta(paths: &Paths, theme_id: &str) -> Option<(PathBuf, Meta)> {
    let entries = fs::read_dir(&paths.meta).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if let Some(meta) = read_meta_file(&path) {
            if meta.id == theme_id {
                return Some((path, meta));
            }
        }
    }
    None
}

pub fn read_catalog(paths: &Paths) -> Option<Arc<CatalogFile>> {
    let metadata = fs::metadata(&paths.catalog).ok()?;
    let len = metadata.len();
    let modified = metadata.modified().ok();
    let revision = CATALOG_REVISION.load(Ordering::Relaxed);
    let cache = CATALOG_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(entry) = guard.get(&paths.catalog) {
            if entry.revision == revision && entry.len == len && entry.modified == modified {
                return Some(entry.catalog.clone());
            }
        }
    }
    let text = fs::read_to_string(&paths.catalog).ok()?;
    let catalog = Arc::new(serde_json::from_str::<CatalogFile>(&text).ok()?);
    if let Ok(mut guard) = cache.lock() {
        // Bound test/dev churn; production normally has one storage root.
        if guard.len() > 8 {
            guard.clear();
        }
        guard.insert(
            paths.catalog.clone(),
            CachedCatalog {
                revision,
                len,
                modified,
                catalog: catalog.clone(),
            },
        );
    }
    Some(catalog)
}

pub async fn meta_asset_size(store: &AssetStore, meta: &Meta) -> u64 {
    if let Some(size) = meta.asset_size {
        return size;
    }
    let mut total = 0u64;
    for key in meta.asset_keys() {
        total = total.saturating_add(store.size(&key).await);
    }
    total
}

async fn build_catalog_entry(
    store: &AssetStore,
    meta: &Meta,
    stats: &HashMap<String, ThemeStats>,
) -> Option<CatalogEntry> {
    if meta.withdrawn_at.is_some() {
        return None;
    }
    if !valid_theme_id(&meta.id) {
        return None;
    }
    if normalize_asset_key(&meta.package).is_none() || normalize_asset_key(&meta.preview).is_none()
    {
        return None;
    }
    if !store.exists(&meta.package).await || !store.exists(&meta.preview).await {
        return None;
    }
    let mut screenshots = Vec::new();
    for relative in &meta.screenshots {
        if normalize_asset_key(relative).is_some() && store.exists(relative).await {
            screenshots.push(relative.clone());
        }
    }
    let mut sha256 = meta.sha256.clone().unwrap_or_default();
    if sha256.is_empty() {
        sha256 = store.sha256(&meta.package).await.unwrap_or_default();
    }
    let mut entry = CatalogEntry {
        id: meta.id.clone(),
        version: meta.version.max(0) as u32,
        format_version: if meta.format_version == 0 {
            MIN_FORMAT_VERSION
        } else {
            meta.format_version
        },
        min_manager_version: meta.min_manager_version,
        name: meta.name.clone(),
        author: meta.author.clone(),
        author_hash: meta.author_hash.clone().filter(|hash| valid_author_hash(hash)),
        description: meta.description.clone(),
        tags: meta.tags.clone(),
        adaptive: meta_is_adaptive(meta),
        r18: meta_is_r18(meta),
        license: meta.license.clone(),
        size: store.size(&meta.package).await,
        sha256,
        path: meta.package.clone(),
        preview_path: meta.preview.clone(),
        avatar_path: None,
        screenshot_paths: screenshots,
        updated_at: meta.updated_at.clone(),
        changelog: meta.changelog.clone(),
        downloads: stats.get(&meta.id).map(|item| item.downloads).unwrap_or(0),
        heat: stats.get(&meta.id).map(|item| item.heat).unwrap_or(0),
    };
    if let Some(avatar) = &meta.avatar {
        if normalize_asset_key(avatar).is_some() && store.exists(avatar).await {
            entry.avatar_path = Some(avatar.clone());
        }
    }
    Some(entry)
}

pub async fn author_avatar_key(
    store: &AssetStore,
    paths: &Paths,
    author_hash: &str,
    author_name: Option<&str>,
) -> Option<String> {
    for extension in crate::config::IMAGE_EXTENSIONS {
        let key = format!("avatars/authors/{author_hash}{extension}");
        if store.exists(&key).await {
            return Some(key);
        }
    }
    latest_published_author_avatar(store, paths, author_hash, author_name).await
}

async fn latest_published_author_avatar(
    store: &AssetStore,
    paths: &Paths,
    author_hash: &str,
    author_name: Option<&str>,
) -> Option<String> {
    let mut candidates: Vec<(u8, String, String)> = Vec::new();
    let entries = fs::read_dir(&paths.meta).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(meta) = read_meta_file(&path) else {
            continue;
        };
        let hash_matches = meta.author_hash.as_deref() == Some(author_hash);
        let legacy_name_matches = meta
            .author_hash
            .as_deref()
            .map(str::is_empty)
            .unwrap_or(true)
            && author_name
                .is_some_and(|name| !name.is_empty() && meta.author.eq_ignore_ascii_case(name));
        if !hash_matches && !legacy_name_matches {
            continue;
        }
        let Some(avatar) = meta.avatar.as_deref().filter(|value| !value.is_empty()) else {
            continue;
        };
        candidates.push((
            if hash_matches { 0 } else { 1 },
            meta.updated_at,
            avatar.to_string(),
        ));
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0).then(right.1.cmp(&left.1)));
    for (_, _, key) in candidates {
        if store.exists(&key).await {
            return Some(key);
        }
    }
    None
}

pub async fn rebuild_catalog(store: &AssetStore, paths: &Paths) -> Result<Vec<CatalogEntry>> {
    fs::create_dir_all(&paths.meta)?;
    let mut metas: Vec<Meta> = Vec::new();
    if let Ok(entries) = fs::read_dir(&paths.meta) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect();
        files.sort();
        for path in files {
            if let Some(meta) = read_meta_file(&path) {
                metas.push(meta);
            }
        }
    }
    let stats = crate::db::load_theme_stats(&paths.stats_db);
    let mut themes = Vec::new();
    for meta in metas {
        if let Some(entry) = build_catalog_entry(store, &meta, &stats).await {
            themes.push(entry);
        }
    }
    themes.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    let _guard = catalog_write_guard();
    // A stats flush may have completed while this rebuild scanned assets.
    let latest_stats = crate::db::load_theme_stats(&paths.db);
    for theme in &mut themes {
        if let Some(current) = latest_stats.get(&theme.id).copied() {
            theme.downloads = current.downloads;
            theme.heat = current.heat;
        }
    }
    let catalog = CatalogFile {
        schema: 1,
        format_version: CURRENT_FORMAT_VERSION,
        min_format_version: MIN_FORMAT_VERSION,
        supported_formats: (MIN_FORMAT_VERSION..=CURRENT_FORMAT_VERSION).collect(),
        generated_at: Some(now_iso()),
        maintenance: crate::config::maintenance_enabled(paths),
        themes: themes.clone(),
    };
    // Machine-read catalog: compact JSON keeps transfer and parse costs down.
    let data = serde_json::to_vec(&catalog)?;
    write_atomic(&paths.catalog, &data, 0o644)?;
    bump_catalog_revision();
    bump_meta_revision();
    Ok(themes)
}

/// Update counters for a batch of themes inside the published catalog.
///
/// Download flushes call this once per batch, so a busy store does not rewrite
/// catalog.json for every package request.
pub fn update_catalog_stats_batch(
    paths: &Paths,
    stats: &HashMap<String, ThemeStats>,
) -> Result<bool> {
    if stats.is_empty() {
        return Ok(false);
    }
    let _guard = catalog_write_guard();
    let Some(catalog) = read_catalog(paths) else {
        return Ok(false);
    };
    let mut catalog = (*catalog).clone();
    let mut changed = false;
    for entry in &mut catalog.themes {
        let Some(current) = stats.get(&entry.id).copied() else {
            continue;
        };
        if entry.downloads != current.downloads || entry.heat != current.heat {
            entry.downloads = current.downloads;
            entry.heat = current.heat;
            changed = true;
        }
    }
    if changed {
        let data = serde_json::to_vec(&catalog)?;
        write_atomic(&paths.catalog, &data, 0o644)?;
        bump_catalog_revision();
    }
    Ok(changed)
}

/// Update one theme's counters; kept for callers that do not batch.
pub fn update_catalog_stats(paths: &Paths, theme_id: &str, stats: ThemeStats) -> Result<bool> {
    let mut batch = HashMap::new();
    batch.insert(theme_id.to_string(), stats);
    update_catalog_stats_batch(paths, &batch)
}

/// Refresh every counter in the published catalog from the stats database.
/// This is cheap compared with a full rebuild and lets heat decay hourly.
pub fn refresh_catalog_stats(paths: &Paths) -> Result<bool> {
    let _guard = catalog_write_guard();
    let Some(catalog) = read_catalog(paths) else {
        return Ok(false);
    };
    let mut catalog = (*catalog).clone();
    let stats = crate::db::load_theme_stats(&paths.stats_db);
    let mut changed = false;
    for theme in &mut catalog.themes {
        let current = stats.get(&theme.id).copied().unwrap_or_default();
        if theme.downloads != current.downloads || theme.heat != current.heat {
            theme.downloads = current.downloads;
            theme.heat = current.heat;
            changed = true;
        }
    }
    if changed {
        let data = serde_json::to_vec(&catalog)?;
        write_atomic(&paths.catalog, &data, 0o644)?;
        bump_catalog_revision();
    }
    Ok(changed)
}

/// Extract the theme id from a package asset key such as
/// `themes/11010-01-v3.sukisu`.
pub fn theme_id_from_package_path(path: &str) -> Option<String> {
    let file = path.strip_prefix("themes/")?;
    let stem = file.strip_suffix(".sukisu")?;
    let (id, version) = stem.rsplit_once("-v")?;
    if version.parse::<i64>().is_err() || !crate::util::valid_theme_id(id) {
        return None;
    }
    Some(id.to_string())
}

pub async fn remove_theme_files(
    store: &AssetStore,
    paths: &Paths,
    meta_file: Option<&Path>,
    meta: &Meta,
) -> Result<()> {
    let lock = theme_mutation_lock(&meta.id);
    let _guard = lock.lock().await;
    let Some((latest_path, latest)) = find_theme_meta(paths, &meta.id) else {
        anyhow::bail!("theme changed during deletion");
    };
    if meta_file.is_some_and(|path| path != latest_path)
        || latest.version != meta.version
        || latest.updated_at != meta.updated_at
        || latest.withdrawn_at != meta.withdrawn_at
        || latest.package != meta.package
        || latest.preview != meta.preview
        || latest.author_hash != meta.author_hash
    {
        anyhow::bail!("theme changed during deletion");
    }
    let mut keys: HashSet<String> = theme_versions(paths, &meta.id)
        .into_iter().flat_map(|version| version.asset_keys()).collect();
    keys.extend(meta.asset_keys());
    for key in keys {
        let _ = store.delete(&key).await;
    }
    if let Some(dir) = history_dir(paths, &meta.id) {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|ext| ext.to_str()) == Some("json") {
                    let _ = fs::remove_file(entry.path());
                }
            }
            let _ = fs::remove_dir(&dir);
        }
    }
    if let Some(path) = meta_file {
        let _ = fs::remove_file(path);
    }
    bump_meta_revision();
    Ok(())
}

pub fn withdrawn_asset_keys(paths: &Paths) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    let Ok(entries) = fs::read_dir(&paths.meta) else {
        return keys;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(meta) = read_meta_file(&path) else {
            continue;
        };
        if meta.withdrawn_at.is_some() {
            keys.extend(meta.asset_keys());
        }
    }
    keys
}

pub async fn purge_expired_withdrawn(store: &AssetStore, paths: &Paths) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    let Ok(entries) = fs::read_dir(&paths.meta) else {
        return Ok(removed);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(meta) = read_meta_file(&path) else {
            continue;
        };
        let Some(withdrawn_at) = meta.withdrawn_at.as_deref() else {
            continue;
        };
        if !withdrawal_expired(withdrawn_at) {
            continue;
        }
        remove_theme_files(store, paths, Some(&path), &meta).await?;
        removed.push(meta.id);
    }
    if !removed.is_empty() {
        rebuild_catalog(store, paths).await?;
    }
    Ok(removed)
}

pub fn pending_dir(paths: &Paths, submission: &str) -> Option<PathBuf> {
    if !valid_submission_id(submission) {
        return None;
    }
    let target = paths.pending.join(submission);
    if target.starts_with(&paths.pending) {
        Some(target)
    } else {
        None
    }
}

pub fn pending_meta(_paths: &Paths, directory: &Path) -> Option<PendingMeta> {
    let text = fs::read_to_string(directory.join("meta.json")).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn pending_state(paths: &Paths) -> (usize, u64) {
    let mut count = 0usize;
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(&paths.pending) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let directory = entry.path();
        if !directory.is_dir() || !directory.join("meta.json").is_file() {
            continue;
        }
        count += 1;
        for file in walkdir::WalkDir::new(&directory)
            .into_iter()
            .flatten()
            .filter(|entry| entry.file_type().is_file())
        {
            if let Ok(metadata) = file.metadata() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    (count, total)
}

pub fn pending_submissions(paths: &Paths) -> Vec<Value> {
    let mut submissions = Vec::new();
    let Ok(entries) = fs::read_dir(&paths.pending) else {
        return submissions;
    };
    for entry in entries.flatten() {
        let directory = entry.path();
        if !directory.is_dir() {
            continue;
        }
        let name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !valid_submission_id(name) {
            continue;
        }
        let Some(meta) = pending_meta(paths, &directory) else {
            continue;
        };
        let package = directory.join(&meta.package);
        let preview = directory.join(&meta.preview);
        if !package.is_file() || !preview.is_file() {
            continue;
        }
        let preview_rel = format!("pending/{}/{}", name, meta.preview);
        let mut avatar_url = String::new();
        if let Some(avatar) = &meta.avatar {
            if directory.join(avatar).is_file() {
                avatar_url = format!(
                    "/admin/api/preview?path={}",
                    quote_query(&format!("pending/{}/{}", name, avatar))
                );
            }
        }
        let size = package
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let deep = meta
            .validation
            .get("deep")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        submissions.push(json!({
            "submissionId": name,
            "id": meta.id,
            "version": meta.version,
            "name": meta.name,
            "author": meta.author,
            "description": meta.description,
            "tags": meta.tags,
            "adaptive": meta.adaptive || tags_mark_adaptive(&meta.tags),
            "license": meta.license,
            "minManagerVersion": meta.min_manager_version,
            "size": size,
            "submittedAt": meta.submitted_at,
            "authorHash": meta.author_hash,
            "authorFingerprint": meta.author_fingerprint,
            "submitterUserId": meta.submitter_user_id,
            "formatVersion": meta.format_version,
            "sha256": meta.sha256,
            "deepValidated": deep,
            "previewUrl": format!("/admin/api/preview?path={}", quote_query(&preview_rel)),
            "avatarUrl": avatar_url,
        }));
    }
    submissions.sort_by(|a, b| {
        b.get("submittedAt")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .cmp(
                a.get("submittedAt")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
            )
    });
    submissions
}

pub fn read_authors(paths: &Paths) -> AuthorsFile {
    let Ok(text) = fs::read_to_string(&paths.authors_file) else {
        return AuthorsFile {
            version: 1,
            authors: BTreeMap::new(),
        };
    };
    serde_json::from_str(&text).unwrap_or(AuthorsFile {
        version: 1,
        authors: BTreeMap::new(),
    })
}

pub fn write_authors(paths: &Paths, authors: &AuthorsFile) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(authors)?;
    data.push(b'\n');
    write_atomic(&paths.authors_file, &data, 0o600)?;
    Ok(())
}

pub fn authors_list(paths: &Paths) -> Vec<Value> {
    let data = read_authors(paths);
    let mut entries: Vec<Value> = data
        .authors
        .iter()
        .map(|(hash, record)| {
            json!({
                "hash": hash,
                "fingerprint": if record.fingerprint.is_empty() { author_fingerprint(hash) } else { record.fingerprint.clone() },
                "approvedAt": record.approved_at,
                "lastUsedAt": record.last_used_at,
            })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.get("approvedAt")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .cmp(
                a.get("approvedAt")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
            )
    });
    entries
}

pub fn grant_author(paths: &Paths, hash: &str) -> Result<()> {
    let mut data = read_authors(paths);
    let now = now_iso();
    let entry = data.authors.entry(hash.to_string()).or_default();
    if entry.fingerprint.is_empty() {
        entry.fingerprint = author_fingerprint(hash);
    }
    if entry.approved_at.is_empty() {
        entry.approved_at = now.clone();
    }
    entry.last_used_at = now;
    write_authors(paths, &data)
}

pub fn touch_author(paths: &Paths, hash: &str) -> Result<()> {
    let mut data = read_authors(paths);
    if let Some(entry) = data.authors.get_mut(hash) {
        entry.last_used_at = now_iso();
        write_authors(paths, &data)?;
    }
    Ok(())
}

pub fn revoke_author(paths: &Paths, hash: &str) -> Result<bool> {
    let mut data = read_authors(paths);
    let removed = data.authors.remove(hash).is_some();
    if removed {
        write_authors(paths, &data)?;
    }
    Ok(removed)
}

pub async fn storage_usage(
    store: &AssetStore,
    paths: &Paths,
    backend: &str,
    limit: u64,
) -> StorageUsage {
    let mut asset_used = 0u64;
    if let Ok(entries) = fs::read_dir(&paths.meta) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(meta) = read_meta_file(&path) {
                if meta.storage_id.is_some() {
                    continue;
                }
                asset_used = asset_used.saturating_add(meta_asset_size(store, &meta).await);
            }
        }
    }
    let pending_used = pending_state(paths).1;
    let used = asset_used.saturating_add(pending_used);
    let disk_free = fs4::available_space(&paths.root).unwrap_or(0);
    StorageUsage {
        limit,
        used,
        free: limit.saturating_sub(used),
        disk_free,
        assets: asset_used,
        pending: pending_used,
        backend: backend.to_string(),
    }
}

pub async fn location_usage(paths: &Paths, location_id: &str) -> u64 {
    let mut used = 0u64;
    if let Ok(entries) = fs::read_dir(&paths.meta) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(meta) = read_meta_file(&path) else {
                continue;
            };
            if meta.storage_id.as_deref() != Some(location_id) {
                continue;
            }
            used = used.saturating_add(meta.asset_size.unwrap_or(0));
        }
    }
    used
}

pub async fn backfill_meta_assets(store: &AssetStore, paths: &Paths) -> Result<()> {
    let Ok(entries) = fs::read_dir(&paths.meta) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(mut meta) = read_meta_file(&path) else {
            continue;
        };
        let mut changed = false;
        if meta.asset_size.is_none() {
            let mut total = 0u64;
            for key in meta.asset_keys() {
                total = total.saturating_add(store.size(&key).await);
            }
            if total > 0 || meta.asset_keys().is_empty() {
                meta.asset_size = Some(total);
                changed = true;
            }
        }
        if meta.sha256.as_deref().unwrap_or("").is_empty() {
            if !meta.package.is_empty() {
                if let Ok(sha) = store.sha256(&meta.package).await {
                    meta.sha256 = Some(sha);
                    changed = true;
                }
            }
        }
        if changed {
            let _ = write_meta(&path, &meta);
        }
    }
    Ok(())
}

pub fn catalog_state(paths: &Paths) -> Value {
    let Some(catalog) = read_catalog(paths) else {
        return json!({
            "themes": 0,
            "generatedAt": null,
            "updatedAt": null,
            "missing": true,
        });
    };
    json!({
        "themes": catalog.themes.len(),
        "generatedAt": catalog.generated_at,
        "updatedAt": file_mtime_seconds(&paths.catalog),
        "missing": false,
    })
}

pub async fn admin_themes(paths: &Paths, stats: &HashMap<String, ThemeStats>) -> Vec<Value> {
    let Some(catalog) = read_catalog(paths) else {
        return Vec::new();
    };
    let catalog = (*catalog).clone();
    catalog
        .themes
        .into_iter()
        .map(|theme| {
            let theme_stats = stats.get(&theme.id).copied().unwrap_or_default();
            let preview_url = if theme.preview_path.is_empty() {
                String::new()
            } else {
                format!(
                    "/admin/api/preview?path={}&v={}",
                    quote_query(&theme.preview_path),
                    quote_query(&theme.updated_at)
                )
            };
            let avatar_url = theme
                .avatar_path
                .as_ref()
                .filter(|path| !path.is_empty())
                .map(|path| format!("/admin/api/preview?path={}", quote_query(path)))
                .unwrap_or_default();
            json!({
                "id": theme.id,
                "version": theme.version,
                "formatVersion": theme.format_version,
                "minManagerVersion": theme.min_manager_version,
                "name": theme.name,
                "author": theme.author,
                "authorHash": theme.author_hash,
                "description": theme.description,
                "tags": theme.tags,
                "adaptive": theme.adaptive || tags_mark_adaptive(&theme.tags),
                "r18": theme.r18 || tags_mark_r18(&theme.tags),
                "size": theme.size,
                "license": theme.license,
                "sha256": theme.sha256,
                "verified": !theme.sha256.is_empty(),
                "updatedAt": theme.updated_at,
                "changelog": theme.changelog,
                "previewUrl": preview_url,
                "avatarUrl": avatar_url,
                "path": theme.path,
                "downloads": theme_stats.downloads,
                "heat": theme_stats.heat,
            })
        })
        .collect()
}

pub fn catalog_themes_count(paths: &Paths) -> usize {
    read_catalog(paths)
        .map(|catalog| catalog.themes.len())
        .unwrap_or(0)
}

pub fn job_snapshot_value(job: &JobState) -> Value {
    serde_json::to_value(job).unwrap_or_else(|_| json!({}))
}

pub fn author_hash_for(author_id: &str) -> (String, String) {
    let hash = author_key(author_id);
    let fingerprint = author_fingerprint(&hash);
    (hash, fingerprint)
}

pub fn is_valid_author_hash(value: &str) -> bool {
    valid_author_hash(value)
}

pub fn author_record(hash: &str) -> AuthorRecord {
    AuthorRecord {
        fingerprint: author_fingerprint(hash),
        approved_at: String::new(),
        last_used_at: String::new(),
        extra: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::storage::{AssetStore, LocalStore};

    fn test_paths(root: &Path) -> Paths {
        Paths {
            root: root.to_path_buf(),
            meta: root.join("meta"),
            pending: root.join("pending"),
            uploads: root.join("uploads"),
            catalog: root.join("catalog.json"),
            static_dir: root.join("static"),
            authors_file: root.join("authors.json"),
            token_env: root.join("token.env"),
            admin_env: root.join("admin.env"),
            admin_password_file: root.join("admin-password"),
            admin_config: root.join("config.env"),
            storage_env: root.join("storage.env"),
            account_env: root.join("account.env"),
            network_env: root.join("network.env"),
            sources_file: root.join("sources.json"),
            legacy_db: root.join("account.db"),
            db_dir: root.join("db-split"),
            db_layout: crate::config::DbLayout::Split,
            db: root.join("db-split/account.db"),
            authors_db: root.join("db-split/authors.db"),
            social_db: root.join("db-split/social.db"),
            stats_db: root.join("db-split/stats.db"),
            audit_db: root.join("db-split/audit.db"),
        }
    }

    #[test]
    fn builds_meta_index_for_assets_and_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("meta")).unwrap();
        std::fs::write(
            root.join("meta/11010-01.json"),
            r#"{
                "id": "11010-01",
                "author": "alice",
                "authorHash": "hash-a",
                "preview": "previews/11010-01-v1.jpg",
                "screenshots": ["screenshots/11010-01-v1-01.jpg"],
                "package": "themes/11010-01-v1.sukisu"
            }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("meta/legacy.json"),
            r#"{
                "id": "legacy",
                "author": "Bob",
                "authorHash": "",
                "preview": "previews/legacy-v1.jpg",
                "package": "themes/legacy-v1.sukisu"
            }"#,
        )
        .unwrap();

        let index = build_meta_index(&test_paths(root));
        assert!(index.theme_for_asset("previews/11010-01-v1.jpg").is_some());
        assert!(index
            .theme_for_asset("screenshots/11010-01-v1-01.jpg")
            .is_some());
        assert!(index.theme_for_asset("previews/missing.jpg").is_none());

        let hashes = HashSet::from(["hash-a".to_string()]);
        assert!(index.owned_ids(&hashes, "someone").contains("11010-01"));
        assert!(index.owned_ids(&HashSet::new(), "bob").contains("legacy"));
        assert!(!index.owned_ids(&HashSet::new(), "alice").contains("legacy"));
    }

    #[test]
    fn parses_package_asset_paths() {
        assert_eq!(
            theme_id_from_package_path("themes/11010-01-v3.sukisu"),
            Some("11010-01".to_string())
        );
        assert_eq!(
            theme_id_from_package_path("themes/foo-v1.sukisu"),
            Some("foo".to_string())
        );
        assert_eq!(theme_id_from_package_path("previews/1-v1.jpg"), None);
        assert_eq!(theme_id_from_package_path("themes/abc.sukisu"), None);
    }

    #[test]
    fn detects_r18_tags() {
        assert!(tags_mark_r18(&["R18".to_string()]));
        assert!(tags_mark_r18(&["nsfw".to_string()]));
        assert!(tags_mark_r18(&["成人向".to_string()]));
        assert!(!tags_mark_r18(&["dark".to_string()]));
        assert!(!tags_mark_r18(&[]));
    }

    #[tokio::test]
    async fn finds_legacy_author_avatar_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("meta")).unwrap();
        std::fs::create_dir_all(root.join("avatars")).unwrap();
        std::fs::write(root.join("avatars/1-v1.jpg"), b"avatar").unwrap();
        std::fs::write(
            root.join("meta/1.json"),
            r#"{
                "id": "1",
                "version": 1,
                "name": "Legacy",
                "author": "shirkneko",
                "package": "themes/1-v1.sukisu",
                "preview": "previews/1-v1.jpg",
                "avatar": "avatars/1-v1.jpg",
                "updatedAt": "2026-11-02T17:31:22Z"
            }"#,
        )
        .unwrap();
        let paths = test_paths(root);
        let store = AssetStore::Local(LocalStore::new(root));
        assert_eq!(
            author_avatar_key(&store, &paths, "missing-hash", Some("shirkneko")).await,
            Some("avatars/1-v1.jpg".to_string())
        );
    }

    #[tokio::test]
    async fn rebuild_catalog_copies_maintenance_flag() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let paths = test_paths(root);
        std::fs::create_dir_all(&paths.meta).unwrap();
        let store = AssetStore::Local(LocalStore::new(root));

        crate::config::set_maintenance_enabled(&paths, true).unwrap();
        rebuild_catalog(&store, &paths).await.unwrap();
        assert!(read_catalog(&paths).unwrap().maintenance);

        crate::config::set_maintenance_enabled(&paths, false).unwrap();
        rebuild_catalog(&store, &paths).await.unwrap();
        assert!(!read_catalog(&paths).unwrap().maintenance);
    }

    #[tokio::test]
    async fn rollback_publishes_new_version_and_preserves_history() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let paths = test_paths(root);
        for name in ["meta", "themes", "previews"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        for version in [1, 2] {
            std::fs::write(root.join(format!("themes/demo-v{version}.sukisu")), b"package").unwrap();
            std::fs::write(root.join(format!("previews/demo-v{version}.jpg")), b"preview").unwrap();
        }
        let first: Meta = serde_json::from_value(json!({
            "id": "demo", "version": 1, "name": "Demo", "author": "Alice",
            "authorHash": "old-license",
            "package": "themes/demo-v1.sukisu", "preview": "previews/demo-v1.jpg"
        })).unwrap();
        let second: Meta = serde_json::from_value(json!({
            "id": "demo", "version": 2, "name": "Demo", "author": "Alice New",
            "authorHash": "current-license",
            "package": "themes/demo-v2.sukisu", "preview": "previews/demo-v2.jpg"
        })).unwrap();
        archive_version(&paths, &first).unwrap();
        write_meta(&paths.meta.join("demo.json"), &second).unwrap();
        let store = AssetStore::Local(LocalStore::new(root));
        rebuild_catalog(&store, &paths).await.unwrap();

        let restored = rollback_version(&store, &paths, "demo", 1, &second).await.unwrap();
        assert_eq!(restored.version, 3);
        assert_eq!(restored.package, first.package);
        assert_eq!(restored.author_hash, second.author_hash);
        assert_eq!(restored.author, second.author);
        assert_eq!(restored.changelog, "回退至 v1");
        assert_eq!(theme_versions(&paths, "demo").iter().map(|meta| meta.version).collect::<Vec<_>>(), vec![3, 2, 1]);
        assert_eq!(read_catalog(&paths).unwrap().themes[0].version, 3);
    }
}
