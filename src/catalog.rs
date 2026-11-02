use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::config::{
    write_atomic, Paths, MIN_FORMAT_VERSION, CURRENT_FORMAT_VERSION, STORAGE_LIMIT_BYTES,
};
use crate::models::{
    AuthorRecord, AuthorsFile, CatalogEntry, CatalogFile, JobState, Meta, PendingMeta, StorageUsage,
};
use crate::storage::AssetStore;
use crate::util::{
    author_fingerprint, author_key, file_mtime_seconds, normalize_asset_key, now_iso,
    quote_query, valid_author_hash, valid_submission_id, valid_theme_id,
};

pub fn read_meta_file(path: &Path) -> Option<Meta> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_meta(path: &Path, meta: &Meta) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(meta)?;
    data.push(b'\n');
    write_atomic(path, &data, 0o644)?;
    Ok(())
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

pub fn read_catalog(paths: &Paths) -> Option<CatalogFile> {
    let text = fs::read_to_string(&paths.catalog).ok()?;
    serde_json::from_str(&text).ok()
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

async fn build_catalog_entry(store: &AssetStore, meta: &Meta) -> Option<CatalogEntry> {
    if !valid_theme_id(&meta.id) {
        return None;
    }
    if normalize_asset_key(&meta.package).is_none()
        || normalize_asset_key(&meta.preview).is_none()
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
        description: meta.description.clone(),
        tags: meta.tags.clone(),
        license: meta.license.clone(),
        size: store.size(&meta.package).await,
        sha256,
        path: meta.package.clone(),
        preview_path: meta.preview.clone(),
        avatar_path: None,
        screenshot_paths: screenshots,
        updated_at: meta.updated_at.clone(),
        changelog: meta.changelog.clone(),
    };
    if let Some(avatar) = &meta.avatar {
        if normalize_asset_key(avatar).is_some() && store.exists(avatar).await {
            entry.avatar_path = Some(avatar.clone());
        }
    }
    Some(entry)
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
    let mut themes = Vec::new();
    for meta in metas {
        if let Some(entry) = build_catalog_entry(store, &meta).await {
            themes.push(entry);
        }
    }
    themes.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    let catalog = CatalogFile {
        schema: 1,
        format_version: CURRENT_FORMAT_VERSION,
        min_format_version: MIN_FORMAT_VERSION,
        supported_formats: vec![MIN_FORMAT_VERSION, CURRENT_FORMAT_VERSION],
        generated_at: Some(now_iso()),
        themes: themes.clone(),
    };
    let mut data = serde_json::to_vec_pretty(&catalog)?;
    data.push(b'\n');
    write_atomic(&paths.catalog, &data, 0o644)?;
    Ok(themes)
}

pub async fn remove_theme_files(
    store: &AssetStore,
    meta_file: Option<&Path>,
    meta: &Meta,
) -> Result<()> {
    for key in meta.asset_keys() {
        let _ = store.delete(&key).await;
    }
    if let Some(path) = meta_file {
        let _ = fs::remove_file(path);
    }
    Ok(())
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

pub fn pending_meta(paths: &Paths, directory: &Path) -> Option<PendingMeta> {
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
        let size = package.metadata().map(|metadata| metadata.len()).unwrap_or(0);
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
            "license": meta.license,
            "minManagerVersion": meta.min_manager_version,
            "size": size,
            "submittedAt": meta.submitted_at,
            "authorFingerprint": meta.author_fingerprint,
            "formatVersion": meta.format_version,
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
) -> StorageUsage {
    let mut asset_used = 0u64;
    if let Ok(entries) = fs::read_dir(&paths.meta) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(meta) = read_meta_file(&path) {
                asset_used = asset_used.saturating_add(meta_asset_size(store, &meta).await);
            }
        }
    }
    let pending_used = pending_state(paths).1;
    let used = asset_used.saturating_add(pending_used);
    let disk_free = fs4::available_space(&paths.root).unwrap_or(0);
    StorageUsage {
        limit: STORAGE_LIMIT_BYTES,
        used,
        free: STORAGE_LIMIT_BYTES.saturating_sub(used),
        disk_free,
        assets: asset_used,
        pending: pending_used,
        backend: backend.to_string(),
    }
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

pub async fn admin_themes(paths: &Paths) -> Vec<Value> {
    let Some(catalog) = read_catalog(paths) else {
        return Vec::new();
    };
    catalog
        .themes
        .into_iter()
        .map(|theme| {
            let preview_url = if theme.preview_path.is_empty() {
                String::new()
            } else {
                format!(
                    "/admin/api/preview?path={}",
                    quote_query(&theme.preview_path)
                )
            };
            let avatar_url = theme
                .avatar_path
                .as_ref()
                .filter(|path| !path.is_empty())
                .map(|path| {
                    format!(
                        "/admin/api/preview?path={}",
                        quote_query(path)
                    )
                })
                .unwrap_or_default();
            json!({
                "id": theme.id,
                "version": theme.version,
                "formatVersion": theme.format_version,
                "minManagerVersion": theme.min_manager_version,
                "name": theme.name,
                "author": theme.author,
                "description": theme.description,
                "tags": theme.tags,
                "size": theme.size,
                "license": theme.license,
                "updatedAt": theme.updated_at,
                "previewUrl": preview_url,
                "avatarUrl": avatar_url,
                "path": theme.path,
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
