use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum::extract::Multipart;
use serde_json::json;

use crate::catalog;
use crate::config::{
    Paths, StorageSettings, AVATAR_MAX_EDGE, IMAGE_EXTENSIONS, MAX_IMAGE_INPUT_BYTES,
    MAX_PACKAGE_BYTES, MAX_SCREENSHOTS, PREVIEW_MAX_EDGE, SCREENSHOT_MAX_EDGE,
};
use crate::images;
use crate::models::{FilePart, Meta, PackageInfo, PendingMeta, UploadForm};
use crate::package::validate_package;
use crate::storage::AssetStore;
use crate::util::{
    content_type_for, dir_size, file_extension, normalize_asset_key, now_iso, sha256_hex,
    valid_theme_id,
};

#[derive(Debug)]
pub enum PublishError {
    Bad(String),
    Conflict(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for PublishError {
    fn from(value: anyhow::Error) -> Self {
        PublishError::Internal(value)
    }
}

impl From<std::io::Error> for PublishError {
    fn from(value: std::io::Error) -> Self {
        PublishError::Internal(value.into())
    }
}

pub struct PreparedUpload {
    pub id: String,
    pub version: u32,
    pub format_version: u32,
    pub name: String,
    pub author: String,
    pub description: String,
    pub tags: Vec<String>,
    pub license: String,
    pub min_manager_version: i64,
    pub changelog: String,
    pub package: FilePart,
    pub package_info: PackageInfo,
    pub preview: FilePart,
    pub avatar: Option<FilePart>,
    pub screenshots: Vec<FilePart>,
}

impl PreparedUpload {
    pub fn new_size(&self) -> u64 {
        let mut total = self.package.data.len() as u64 + self.preview.data.len() as u64;
        if let Some(avatar) = &self.avatar {
            total += avatar.data.len() as u64;
        }
        total += self
            .screenshots
            .iter()
            .map(|file| file.data.len() as u64)
            .sum::<u64>();
        total
    }
}

pub async fn reuse_author_avatar(
    store: &AssetStore,
    paths: &Paths,
    author_hash: &str,
    author_name: Option<&str>,
    avatar: &mut Option<FilePart>,
) {
    if avatar.is_some() {
        return;
    }
    let Some(key) = catalog::author_avatar_key(store, paths, author_hash, author_name).await else {
        return;
    };
    let extension = Path::new(&key)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{}", value.to_ascii_lowercase()))
        .unwrap_or_default();
    if !crate::config::IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        return;
    }
    let Ok(data) = store.read(&key).await else {
        return;
    };
    *avatar = Some(FilePart {
        filename: format!("avatar{extension}"),
        ext: extension,
        data,
    });
}

pub async fn persist_author_avatar(
    store: &AssetStore,
    author_hash: &str,
    avatar: &FilePart,
) -> Result<()> {
    let key = format!("avatars/authors/{author_hash}{}", avatar.ext);
    store
        .put(&key, &avatar.data, &content_type_for(&key))
        .await?;
    for extension in crate::config::IMAGE_EXTENSIONS {
        if extension != avatar.ext.as_str() {
            let _ = store
                .delete(&format!("avatars/authors/{author_hash}{extension}"))
                .await;
        }
    }
    Ok(())
}

async fn read_field_bytes(field: axum::extract::multipart::Field<'_>) -> Result<Vec<u8>> {
    let bytes = field.bytes().await.context("读取上传文件失败")?;
    Ok(bytes.to_vec())
}

pub async fn parse_multipart(mut multipart: Multipart) -> Result<UploadForm, String> {
    let mut id = String::new();
    let mut version = String::new();
    let mut format_version = None;
    let mut name = String::new();
    let mut author = String::new();
    let mut author_id: Option<String> = None;
    let mut owner_hash: Option<String> = None;
    let mut description = String::new();
    let mut tags = String::new();
    let mut license = String::new();
    let mut min_manager_version = String::new();
    let mut changelog = String::new();
    let mut upload_id: Option<String> = None;
    let mut package: Option<FilePart> = None;
    let mut preview: Option<FilePart> = None;
    let mut avatar: Option<FilePart> = None;
    let mut screenshots = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| error.to_string())?
    {
        let field_name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(|name| name.to_string());
        match field_name.as_str() {
            "package" | "preview" | "avatar" | "screenshots" => {
                let Some(filename) = filename else {
                    continue;
                };
                if filename.is_empty() {
                    continue;
                }
                let data = read_field_bytes(field)
                    .await
                    .map_err(|error| error.to_string())?;
                if data.is_empty() {
                    continue;
                }
                let ext = file_extension(&filename);
                match field_name.as_str() {
                    "package" => {
                        if ext != ".sukisu" {
                            return Err(format!("{filename} 的文件类型不支持"));
                        }
                        if data.len() > MAX_PACKAGE_BYTES {
                            return Err(format!("{filename} 超过大小限制"));
                        }
                        package = Some(FilePart {
                            filename,
                            ext,
                            data,
                        });
                    }
                    "preview" | "avatar" | "screenshots" => {
                        if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                            return Err(format!("{filename} 的文件类型不支持"));
                        }
                        if data.len() > MAX_IMAGE_INPUT_BYTES {
                            return Err(format!(
                                "{filename} 超过原始大小限制（最大 12 MB，服务器会自动压缩）"
                            ));
                        }
                        let part = FilePart {
                            filename,
                            ext,
                            data,
                        };
                        match field_name.as_str() {
                            "preview" => preview = Some(part),
                            "avatar" => avatar = Some(part),
                            _ => {
                                if screenshots.len() >= MAX_SCREENSHOTS {
                                    return Err(format!("最多上传 {MAX_SCREENSHOTS} 张截图"));
                                }
                                screenshots.push(part);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                let value = field.text().await.map_err(|error| error.to_string())?;
                match field_name.as_str() {
                    "id" => id = value,
                    "version" => version = value,
                    "formatVersion" => format_version = Some(value),
                    "name" => name = value,
                    "author" => author = value,
                    "authorId" => author_id = Some(value),
                    "ownerHash" => owner_hash = Some(value),
                    "description" => description = value,
                    "tags" => tags = value,
                    "license" => license = value,
                    "minManagerVersion" => min_manager_version = value,
                    "changelog" => changelog = value,
                    "uploadId" => upload_id = Some(value.trim().to_string()),
                    _ => {}
                }
            }
        }
    }

    let upload_id = upload_id.filter(|value| !value.is_empty());
    let package = match package {
        Some(package) => package,
        None if upload_id.is_some() => FilePart {
            filename: "resumable.sukisu".to_string(),
            ext: ".sukisu".to_string(),
            data: Vec::new(),
        },
        None => return Err("package 文件不能为空".to_string()),
    };
    let preview = preview.ok_or_else(|| "preview 文件不能为空".to_string())?;
    Ok(UploadForm {
        id,
        version,
        format_version,
        name,
        author,
        author_id,
        owner_hash,
        description,
        tags,
        license,
        min_manager_version,
        changelog,
        upload_id,
        package,
        preview,
        avatar,
        screenshots,
    })
}

fn split_tags(value: &str) -> Vec<String> {
    value
        .split(|ch| ch == ',' || ch == '，' || ch == '\n')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

pub const ADAPTIVE_TAG: &str = "adaptive";

/// Tag aliases accepted from older clients and translated to the canonical catalog key.
fn canonical_tag(tag: &str) -> String {
    let normalized = tag.trim().to_lowercase();
    match normalized.as_str() {
        "adaptive" | "auto" | "automatic" | "lightdark" | "light/dark" | "自适应"
        | "自适应主题" | "自适应主题包" => ADAPTIVE_TAG.to_string(),
        _ => tag.trim().to_string(),
    }
}

/// Collapses adaptive aliases and makes sure a package detected as adaptive carries its tag,
/// so catalog clients and previews can badge it without inspecting the package again.
fn normalize_tags(tags: Vec<String>, adaptive: bool) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::with_capacity(tags.len() + 1);
    for tag in tags {
        let canonical = canonical_tag(&tag);
        if canonical.is_empty() {
            continue;
        }
        if normalized
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&canonical) || existing == &canonical)
        {
            continue;
        }
        normalized.push(canonical);
    }
    if adaptive {
        if let Some(index) = normalized
            .iter()
            .position(|tag| tag.eq_ignore_ascii_case(ADAPTIVE_TAG))
        {
            normalized.remove(index);
        } else if normalized.len() >= 10 {
            normalized.pop();
        }
        normalized.insert(0, ADAPTIVE_TAG.to_string());
    }
    normalized.truncate(10);
    normalized
}

pub async fn prepare_upload(form: UploadForm) -> Result<PreparedUpload, String> {
    let theme_id = form.id.trim().to_string();
    if !valid_theme_id(&theme_id) {
        return Err("主题 ID 只能使用小写字母、数字和 -".to_string());
    }
    let version = form
        .version
        .trim()
        .parse::<u32>()
        .map_err(|_| "版本号必须是正整数".to_string())?;
    if !(1..=1_000_000).contains(&version) {
        return Err("版本号必须是 1-1000000 的整数".to_string());
    }
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Err("主题名称不能为空".to_string());
    }
    let author = form.author.trim().to_string();
    if name.chars().count() > 100 || author.chars().count() > 100 {
        return Err("名称或作者过长".to_string());
    }
    let description = form.description.trim().to_string();
    if description.chars().count() > 2000 {
        return Err("描述过长".to_string());
    }
    let changelog = form.changelog.trim().to_string();
    if changelog.chars().count() > 4000 {
        return Err("更新说明过长".to_string());
    }
    let license = form.license.trim().to_string();
    if license.chars().count() > 100 {
        return Err("许可证名称过长".to_string());
    }
    let min_manager_version = form
        .min_manager_version
        .trim()
        .parse::<i64>()
        .map_err(|_| "最低管理器版本必须是整数".to_string())?;
    if min_manager_version < 0 {
        return Err("最低管理器版本必须是非负整数".to_string());
    }
    let mut tags = split_tags(&form.tags);
    if tags.len() > 10 || tags.iter().any(|tag| tag.chars().count() > 24) {
        return Err("标签过多或过长".to_string());
    }
    if form.screenshots.len() > MAX_SCREENSHOTS {
        return Err(format!("最多上传 {MAX_SCREENSHOTS} 张截图"));
    }

    // Move the package into the blocking validator and return it afterwards. Cloning a
    // package of up to 128 MB only to read theme.json doubles peak memory and copy time.
    let package = form.package;
    let (package, package_info) = tokio::task::spawn_blocking(move || {
        let info = validate_package(&package.data).map_err(|error| error.to_string())?;
        Ok::<_, String>((package, info))
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    let format_version = match form.format_version.as_deref() {
        Some(value) if !value.trim().is_empty() => {
            let declared = value
                .trim()
                .parse::<u32>()
                .map_err(|_| "主题包格式版本无效".to_string())?;
            if !(crate::config::MIN_FORMAT_VERSION..=crate::config::CURRENT_FORMAT_VERSION)
                .contains(&declared)
            {
                return Err("不支持的主题包格式版本".to_string());
            }
            if package_info.deep && declared != package_info.package_version {
                return Err("主题包格式版本与内容不一致".to_string());
            }
            declared
        }
        _ => package_info.package_version,
    };
    if package_info.adaptive
        && tags.len() >= 10
        && !tags.iter().any(|tag| canonical_tag(tag) == ADAPTIVE_TAG)
    {
        return Err("自适应主题会自动添加 adaptive 标签，请将标签减少到 9 个".to_string());
    }
    tags = normalize_tags(tags, package_info.adaptive);
    if !package_info.adaptive {
        // A stale tag from an earlier package selection must not make a plain theme look adaptive.
        tags.retain(|tag| canonical_tag(tag) != ADAPTIVE_TAG);
    }

    // Start every image optimization before awaiting any of them: preview, avatar and up
    // to six screenshots no longer serialize the post-upload verification phase.
    let preview_input_ext = form.preview.ext.clone();
    let preview_data = form.preview.data;
    let preview_task = tokio::task::spawn_blocking(move || {
        images::optimize_image(preview_data, &preview_input_ext, PREVIEW_MAX_EDGE)
            .map(|(bytes, ext)| FilePart {
                filename: "preview".to_string(),
                ext,
                data: bytes,
            })
            .map_err(|error| error.to_string())
    });

    let avatar_task = form.avatar.map(|avatar| {
        let input_ext = avatar.ext.clone();
        let data = avatar.data;
        tokio::task::spawn_blocking(move || {
            images::optimize_image(data, &input_ext, AVATAR_MAX_EDGE)
                .map(|(bytes, ext)| FilePart {
                    filename: "avatar".to_string(),
                    ext,
                    data: bytes,
                })
                .map_err(|error| error.to_string())
        })
    });

    let screenshot_tasks: Vec<_> = form
        .screenshots
        .into_iter()
        .enumerate()
        .map(|(index, screenshot)| {
            let input_ext = screenshot.ext.clone();
            let data = screenshot.data;
            tokio::task::spawn_blocking(move || {
                images::optimize_image(data, &input_ext, SCREENSHOT_MAX_EDGE)
                    .map(|(bytes, ext)| FilePart {
                        filename: format!("screenshot-{:02}", index + 1),
                        ext,
                        data: bytes,
                    })
                    .map_err(|error| error.to_string())
            })
        })
        .collect();

    let screenshots = futures::future::join_all(screenshot_tasks)
        .await
        .into_iter()
        .map(|result| {
            result
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, String>>()?;
    let preview = preview_task
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    let avatar = match avatar_task {
        Some(task) => Some(
            task.await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?,
        ),
        None => None,
    };

    Ok(PreparedUpload {
        id: theme_id,
        version,
        format_version,
        name,
        author,
        description,
        tags,
        license,
        min_manager_version,
        changelog,
        package,
        package_info,
        preview,
        avatar,
        screenshots,
    })
}

pub fn load_pending_upload(
    _paths: &Paths,
    directory: &Path,
    meta: &PendingMeta,
) -> Result<PreparedUpload, String> {
    let theme_id = meta.id.trim().to_string();
    if !valid_theme_id(&theme_id) {
        return Err("待审核主题 ID 无效".to_string());
    }
    if meta.version < 1 {
        return Err("待审核主题版本无效".to_string());
    }
    let package_path = directory.join(&meta.package);
    let preview_path = directory.join(&meta.preview);
    if !package_path.is_file() {
        return Err("待审核主题包缺失".to_string());
    }
    if !preview_path.is_file() {
        return Err("待审核预览图缺失".to_string());
    }
    let package_data = fs::read(&package_path).map_err(|error| error.to_string())?;
    let package_info = validate_package(&package_data).map_err(|error| error.to_string())?;
    let preview_data = fs::read(&preview_path).map_err(|error| error.to_string())?;
    let preview_ext = crate::util::path_extension(&preview_path);
    images::validate_image(&preview_data, &preview_ext).map_err(|error| error.to_string())?;

    let mut screenshots = Vec::new();
    for relative in &meta.screenshots {
        let target = directory.join(relative);
        if !target.is_file() {
            return Err("待审核截图缺失".to_string());
        }
        let data = fs::read(&target).map_err(|error| error.to_string())?;
        let ext = crate::util::path_extension(&target);
        images::validate_image(&data, &ext).map_err(|error| error.to_string())?;
        screenshots.push(FilePart {
            filename: target
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("screenshot")
                .to_string(),
            ext,
            data,
        });
    }
    let avatar = match &meta.avatar {
        Some(relative) => {
            let target = directory.join(relative);
            if target.is_file() {
                let data = fs::read(&target).map_err(|error| error.to_string())?;
                let ext = crate::util::path_extension(&target);
                images::validate_image(&data, &ext).map_err(|error| error.to_string())?;
                Some(FilePart {
                    filename: "avatar".to_string(),
                    ext,
                    data,
                })
            } else {
                None
            }
        }
        None => None,
    };
    let mut tags = normalize_tags(meta.tags.clone(), package_info.adaptive);
    if !package_info.adaptive {
        tags.retain(|tag| canonical_tag(tag) != ADAPTIVE_TAG);
    }
    Ok(PreparedUpload {
        id: theme_id,
        version: meta.version.max(0) as u32,
        format_version: if meta.format_version == 0 {
            crate::config::MIN_FORMAT_VERSION
        } else {
            meta.format_version
        },
        name: meta.name.trim().to_string(),
        author: meta.author.trim().to_string(),
        description: meta.description.trim().to_string(),
        tags,
        license: meta.license.trim().to_string(),
        min_manager_version: meta.min_manager_version,
        changelog: meta.changelog.trim().to_string(),
        package: FilePart {
            filename: package_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("package.sukisu")
                .to_string(),
            ext: crate::util::path_extension(&package_path),
            data: package_data,
        },
        package_info,
        preview: FilePart {
            filename: "preview".to_string(),
            ext: preview_ext,
            data: preview_data,
        },
        avatar,
        screenshots,
    })
}

pub async fn publish_theme(
    store: &AssetStore,
    _settings: &StorageSettings,
    paths: &Paths,
    limit_bytes: u64,
    prepared: PreparedUpload,
    author_hash: Option<String>,
    _reclaim_bytes: u64,
    old_meta: Option<(PathBuf, Meta)>,
) -> Result<Vec<crate::models::CatalogEntry>, PublishError> {
    let lock = catalog::theme_mutation_lock(&prepared.id);
    let _guard = lock.lock().await;
    let latest = catalog::find_theme_meta(paths, &prepared.id);
    let unchanged = match (&old_meta, &latest) {
        (None, None) => true,
        (Some((_, seen)), Some((_, current))) => {
            seen.version == current.version
                && seen.author_hash == current.author_hash
                && seen.author == current.author
                && seen.withdrawn_at == current.withdrawn_at
                && seen.package == current.package
        }
        _ => false,
    };
    if !unchanged {
        return Err(PublishError::Conflict("主题在上传期间发生变化，请刷新后重试".to_string()));
    }
    if latest.as_ref().is_some_and(|(_, meta)| meta.version >= prepared.version as i64) {
        return Err(PublishError::Conflict("该主题已有同版本或更高版本，请提升版本号".to_string()));
    }
    let old_meta = latest;
    let stem = format!("{}-v{}", prepared.id, prepared.version);
    let new_size = prepared.new_size();
    let location = store
        .choose_location(new_size, paths, limit_bytes)
        .await
        .map_err(|error| PublishError::Bad(error.to_string()))?;
    let keys = AssetKeys::new_at(&stem, &prepared, location.as_deref());

    let keep_old_avatar = if prepared.avatar.is_none() {
        match old_meta.as_ref().and_then(|(_, old)| old.avatar.as_ref()) {
            Some(avatar) if store.exists(avatar).await => Some(avatar.clone()),
            _ => None,
        }
    } else { None };

    let mut items: Vec<(&str, &[u8])> = vec![
        (keys.package.as_str(), prepared.package.data.as_slice()),
        (keys.preview.as_str(), prepared.preview.data.as_slice()),
    ];
    if let (Some(avatar), Some(key)) = (&prepared.avatar, &keys.avatar) {
        items.push((key.as_str(), avatar.data.as_slice()));
    }
    for (screenshot, key) in prepared.screenshots.iter().zip(keys.screenshots.iter()) {
        items.push((key.as_str(), screenshot.data.as_slice()));
    }
    let uploaded_keys: Vec<String> = items.iter().map(|(key, _)| (*key).to_string()).collect();
    let location_id = location.as_deref();
    for chunk in items.chunks(4) {
        let uploads = chunk.iter().map(|&(key, data)| {
            let store = &store;
            async move {
                let content_type = content_type_for(key);
                store
                    .put_at(location_id, key, data, &content_type)
                    .await?;
                Ok::<String, anyhow::Error>(key.to_string())
            }
        });
        if let Some(error) = futures::future::join_all(uploads).await.into_iter().find_map(Result::err) {
            for key in &uploaded_keys { let _ = store.delete(key).await; }
            return Err(PublishError::Internal(error));
        }
    }
    if let Some((_, old)) = &old_meta {
        if let Err(error) = catalog::archive_version(paths, old) {
            for key in &uploaded_keys { let _ = store.delete(key).await; }
            return Err(PublishError::Internal(error));
        }
    }

    let mut meta = Meta {
        id: prepared.id.clone(),
        version: prepared.version as i64,
        name: prepared.name.clone(),
        author: prepared.author.clone(),
        description: prepared.description.clone(),
        tags: prepared.tags.clone(),
        adaptive: prepared.package_info.adaptive,
        license: prepared.license.clone(),
        min_manager_version: prepared.min_manager_version,
        format_version: prepared.format_version,
        package: keys.package.clone(),
        preview: keys.preview.clone(),
        avatar: keys.avatar.clone().or_else(|| keep_old_avatar.clone()),
        screenshots: keys.screenshots.clone(),
        updated_at: now_iso(),
        changelog: prepared.changelog.clone(),
        withdrawn_at: None,
        asset_size: Some(new_size),
        sha256: Some(sha256_hex(&prepared.package.data)),
        author_hash: author_hash.clone(),
        storage_id: location.clone(),
        extra: Default::default(),
    };
    if let Some(avatar) = &keep_old_avatar {
        meta.asset_size = Some(new_size + store.size(avatar).await);
    }
    let meta_file = paths.meta.join(format!("{}.json", prepared.id));
    if let Err(error) = catalog::write_meta(&meta_file, &meta) {
        for key in &uploaded_keys { let _ = store.delete(key).await; }
        return Err(PublishError::Internal(error));
    }
    if let Some((old_file, _)) = &old_meta {
        if old_file != &meta_file {
            let _ = fs::remove_file(old_file);
        }
    }
    let themes = match catalog::rebuild_catalog(store, paths).await {
        Ok(themes) => themes,
        Err(error) => {
            if let Some((old_file, old)) = &old_meta {
                if old_file != &meta_file { let _ = fs::remove_file(&meta_file); }
                if let Err(restore_error) = catalog::write_meta(old_file, old) {
                    tracing::error!(?restore_error, theme_id = %prepared.id, "failed to restore theme metadata");
                }
            } else {
                let _ = fs::remove_file(&meta_file);
            }
            let _ = catalog::rebuild_catalog(store, paths).await;
            for key in &uploaded_keys { let _ = store.delete(key).await; }
            return Err(PublishError::Internal(error));
        }
    };
    Ok(themes)
}

pub struct AssetKeys {
    pub package: String,
    pub preview: String,
    pub avatar: Option<String>,
    pub screenshots: Vec<String>,
}

impl AssetKeys {
    fn new(stem: &str, prepared: &PreparedUpload) -> Self {
        Self::new_at(stem, prepared, None)
    }

    fn new_at(stem: &str, prepared: &PreparedUpload, location: Option<&str>) -> Self {
        let avatar = prepared
            .avatar
            .as_ref()
            .map(|avatar| prefix_key(format!("avatars/{stem}{}", avatar.ext), location));
        let screenshots = prepared
            .screenshots
            .iter()
            .enumerate()
            .map(|(index, screenshot)| {
                prefix_key(
                    format!("screenshots/{stem}-{:02}{}", index + 1, screenshot.ext),
                    location,
                )
            })
            .collect();
        Self {
            package: prefix_key(format!("themes/{stem}{}", prepared.package.ext), location),
            preview: prefix_key(format!("previews/{stem}{}", prepared.preview.ext), location),
            avatar,
            screenshots,
        }
    }
}

fn prefix_key(key: String, location: Option<&str>) -> String {
    let Some(location) = location else {
        return key;
    };
    match key.split_once('/') {
        Some((directory, rest)) => format!("{directory}/_loc/{location}/{rest}"),
        None => key,
    }
}

pub fn write_pending(
    paths: &Paths,
    prepared: &PreparedUpload,
    author_hash: &str,
    author_fingerprint: &str,
    remote: &str,
    submission: &str,
    submitter_user_id: Option<&str>,
) -> Result<PathBuf, String> {
    let directory = paths.pending.join(submission);
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let package_name = "package.sukisu".to_string();
    let preview_name = format!("preview{}", prepared.preview.ext);
    let avatar_name = prepared
        .avatar
        .as_ref()
        .map(|avatar| format!("avatar{}", avatar.ext));
    let mut screenshot_names = Vec::new();
    let result = (|| -> Result<(), String> {
        fs::write(directory.join(&package_name), &prepared.package.data)
            .map_err(|error| error.to_string())?;
        fs::write(directory.join(&preview_name), &prepared.preview.data)
            .map_err(|error| error.to_string())?;
        if let (Some(avatar), Some(name)) = (&prepared.avatar, &avatar_name) {
            fs::write(directory.join(name), &avatar.data).map_err(|error| error.to_string())?;
        }
        for (index, screenshot) in prepared.screenshots.iter().enumerate() {
            let name = format!("screenshot-{:02}{}", index + 1, screenshot.ext);
            fs::write(directory.join(&name), &screenshot.data)
                .map_err(|error| error.to_string())?;
            screenshot_names.push(name);
        }
        let meta = json!({
            "submissionId": submission,
            "id": prepared.id,
            "version": prepared.version,
            "name": prepared.name,
            "author": prepared.author,
            "description": prepared.description,
            "tags": prepared.tags,
            "adaptive": prepared.package_info.adaptive,
            "license": prepared.license,
            "minManagerVersion": prepared.min_manager_version,
            "changelog": prepared.changelog,
            "package": package_name,
            "preview": preview_name,
            "avatar": avatar_name,
            "screenshots": screenshot_names,
            "submittedAt": now_iso(),
            "remote": remote,
            "authorHash": author_hash,
            "authorFingerprint": author_fingerprint,
            "submitterUserId": submitter_user_id.unwrap_or(""),
            "formatVersion": prepared.format_version,
            "sha256": sha256_hex(&prepared.package.data),
            "validation": prepared.package_info.to_json(),
        });
        let mut data = serde_json::to_vec_pretty(&meta).map_err(|error| error.to_string())?;
        data.push(b'\n');
        crate::config::write_atomic(&directory.join("meta.json"), &data, 0o644)
            .map_err(|error| error.to_string())?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&directory);
        return Err(error);
    }
    Ok(directory)
}

pub fn pending_directory_size(directory: &Path) -> u64 {
    dir_size(directory)
}

pub fn normalize_key_or_err(key: &str) -> Result<String, String> {
    normalize_asset_key(key).ok_or_else(|| "无效的资源路径".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_adaptive_tag_aliases() {
        let tags = normalize_tags(
            vec![
                "自适应主题".to_string(),
                "pink".to_string(),
                "Adaptive".to_string(),
            ],
            true,
        );
        assert_eq!(tags, vec!["adaptive".to_string(), "pink".to_string()]);
    }

    #[test]
    fn adds_adaptive_tag_without_exceeding_the_limit() {
        let mut tags: Vec<String> = (0..10).map(|index| format!("tag{index}")).collect();
        tags[9] = "自适应".to_string();
        let tags = normalize_tags(tags, true);
        assert_eq!(tags.len(), 10);
        assert_eq!(tags.first().map(String::as_str), Some("adaptive"));
        assert_eq!(tags.iter().filter(|tag| *tag == "adaptive").count(), 1);
    }
}
