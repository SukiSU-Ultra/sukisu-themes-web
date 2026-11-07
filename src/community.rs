//! Account-facing catalog, drafts and community workflows.

use std::collections::HashSet;

use axum::extract::{Extension, Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::account_api::{discover_theme_json, require_active, theme_owned_by_user, user_show_r18};
use crate::accounts::{self, SessionInfo};
use crate::api::{ApiError, AppState, json_body};
use crate::db::Database;
use crate::util::{now_iso, random_hex, valid_theme_id};

type ApiResult<T> = Result<T, ApiError>;

pub fn account_router() -> Router<AppState> {
    Router::new()
        .route("/gallery", get(gallery))
        .route("/favorites", get(favorites))
        .route("/favorites/toggle", post(toggle_favorite))
        .route("/history", get(theme_history))
        .route("/history/view", post(record_theme_view))
        .route("/history/download", post(record_theme_download))
        .route("/history/delete", post(delete_theme_history))
        .route("/updates", get(updates))
        .route("/updates/read", post(read_update))
        .route("/drafts", get(drafts).post(save_draft))
        .route("/drafts/delete", post(delete_draft))
        .route("/themes/{id}/versions", get(versions))
        .route("/themes/{id}/rollback", post(rollback))
        .route("/author/dashboard", get(author_dashboard))
        .route("/reports", get(my_reports).post(create_report))
        .route("/appeals", get(my_appeals).post(create_appeal))
}

pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/community/reports", get(admin_reports))
        .route("/community/reports/resolve", post(resolve_report))
        .route("/community/appeals", get(admin_appeals))
        .route("/community/appeals/resolve", post(resolve_appeal))
        .route("/mirrors/health", get(mirror_health))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GalleryQuery {
    q: Option<String>,
    tag: Option<String>,
    sort: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

async fn favorite_ids(db: &Database, user_id: &str) -> anyhow::Result<HashSet<String>> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let mut statement =
            conn.prepare("SELECT theme_id FROM social.theme_favorites WHERE user_id = ?1")?;
        let rows = statement.query_map([user_id], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<HashSet<_>>>()?)
    })
    .await
}

async fn gallery(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Query(query): Query<GalleryQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(24).clamp(1, 60);
    let offset = query.offset.unwrap_or(0).min(10_000);
    let show_r18 = user_show_r18(&state, &session.user_id).await?;
    let favorite_ids = favorite_ids(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let stats = state.theme_stats();
    let Some(catalog) = crate::catalog::read_catalog(&state.paths) else {
        return Ok(Json(
            json!({"ok": true, "themes": [], "total": 0, "offset": offset, "limit": limit}),
        ));
    };
    let search = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    let tag = query.tag.as_deref().unwrap_or("").trim().to_lowercase();
    let mut themes: Vec<_> = catalog
        .themes
        .iter()
        .filter(|theme| show_r18 || !(theme.r18 || crate::catalog::tags_mark_r18(&theme.tags)))
        .filter(|theme| {
            search.is_empty()
                || [
                    theme.name.as_str(),
                    theme.author.as_str(),
                    theme.id.as_str(),
                    theme.description.as_str(),
                ]
                .iter()
                .any(|value| value.to_lowercase().contains(&search))
        })
        .filter(|theme| {
            tag.is_empty() || theme.tags.iter().any(|value| value.to_lowercase() == tag)
        })
        .collect();
    match query.sort.as_deref() {
        Some("hot") => themes.sort_by(|a, b| {
            stats
                .get(&b.id)
                .map(|item| item.heat)
                .unwrap_or(0)
                .cmp(&stats.get(&a.id).map(|item| item.heat).unwrap_or(0))
                .then_with(|| a.id.cmp(&b.id))
        }),
        Some("downloads") => themes.sort_by(|a, b| {
            stats
                .get(&b.id)
                .map(|item| item.downloads)
                .unwrap_or(0)
                .cmp(&stats.get(&a.id).map(|item| item.downloads).unwrap_or(0))
                .then_with(|| a.id.cmp(&b.id))
        }),
        Some("name") => themes.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id))),
        _ => themes.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        }),
    }
    let total = themes.len();
    let themes: Vec<Value> = themes
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|theme| {
            let mut item = discover_theme_json(theme, stats.as_ref());
            item["favorite"] = json!(favorite_ids.contains(&theme.id));
            item
        })
        .collect();
    Ok(Json(
        json!({"ok": true, "themes": themes, "total": total, "offset": offset, "limit": limit}),
    ))
}

async fn favorites(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let ids = favorite_ids(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let show_r18 = user_show_r18(&state, &session.user_id).await?;
    let stats = state.theme_stats();
    let themes = crate::catalog::read_catalog(&state.paths)
        .map(|catalog| {
            catalog
                .themes
                .iter()
                .filter(|theme| {
                    ids.contains(&theme.id)
                        && (show_r18 || !(theme.r18 || crate::catalog::tags_mark_r18(&theme.tags)))
                })
                .map(|theme| discover_theme_json(theme, stats.as_ref()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Json(json!({"ok": true, "themes": themes, "ids": ids})))
}

async fn toggle_favorite(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    let value = json_body(body).await?;
    let id = value.get("themeId").and_then(Value::as_str).unwrap_or("");
    let favorite = value
        .get("favorite")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad("缺少收藏状态"))?;
    if !valid_theme_id(id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    if favorite {
        let catalog = crate::catalog::read_catalog(&state.paths)
            .ok_or_else(|| ApiError::not_found("主题不存在"))?;
        let theme = catalog
            .themes
            .iter()
            .find(|theme| theme.id == id)
            .ok_or_else(|| ApiError::not_found("主题不存在"))?;
        if (theme.r18 || crate::catalog::tags_mark_r18(&theme.tags))
            && !user_show_r18(&state, &session.user_id).await?
        {
            return Err(ApiError::forbidden("请先开启 R18 内容偏好"));
        }
    }
    let id = id.to_string();
    let user_id = session.user_id.clone();
    state.db.call(move |conn| {
        if favorite {
            conn.execute("INSERT OR IGNORE INTO social.theme_favorites(user_id, theme_id, created_at) VALUES (?1, ?2, ?3)", params![user_id, id, now_iso()])?;
        } else {
            conn.execute("DELETE FROM social.theme_favorites WHERE user_id = ?1 AND theme_id = ?2", params![user_id, id])?;
        }
        Ok(())
    }).await.map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "favorite": favorite})))
}

/// Called after a package is published, whether approval was automatic or manual.
pub async fn record_release(
    db: &Database,
    author_hash: &str,
    theme_id: &str,
    name: &str,
    version: i64,
) -> anyhow::Result<()> {
    if author_hash.is_empty() || !valid_theme_id(theme_id) {
        return Ok(());
    }
    let (author_hash, theme_id, name) = (
        author_hash.to_string(),
        theme_id.to_string(),
        name.to_string(),
    );
    db.call(move |conn| {
        let id = format!("{theme_id}:{version}");
        conn.execute("INSERT OR IGNORE INTO social.theme_updates(id, theme_id, author_hash, name, version, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, theme_id, author_hash, name, version, now_iso()])?;
        Ok(())
    }).await
}

pub async fn remove_theme_activity(db: &Database, theme_id: &str) -> anyhow::Result<()> {
    let theme_id = theme_id.to_string();
    db.call(move |conn| {
        let transaction = conn.transaction()?;
        transaction.execute("DELETE FROM social.theme_update_reads WHERE update_id IN (SELECT id FROM social.theme_updates WHERE theme_id = ?1)", [&theme_id])?;
        transaction.execute("DELETE FROM social.theme_updates WHERE theme_id = ?1", [&theme_id])?;
        transaction.execute("DELETE FROM social.theme_favorites WHERE theme_id = ?1", [&theme_id])?;
        transaction.execute("DELETE FROM social.theme_history WHERE theme_id = ?1", [&theme_id])?;
        transaction.commit()?;
        Ok(())
    }).await
}

async fn updates(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let user_id = session.user_id.clone();
    let rows: Vec<(String, String, String, i64, String, bool)> = state.db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT u.id, u.theme_id, u.name, u.version, u.created_at, r.update_id IS NOT NULL
             FROM social.theme_updates u JOIN social.user_follows f ON f.author_hash = u.author_hash
             LEFT JOIN social.theme_update_reads r ON r.update_id = u.id AND r.user_id = f.follower_id
             WHERE f.follower_id = ?1 AND u.created_at >= f.created_at ORDER BY u.created_at DESC LIMIT 100")?;
        let rows = statement.query_map([user_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get::<_, i64>(5)? != 0)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await.map_err(ApiError::internal)?;
    let show_r18 = user_show_r18(&state, &session.user_id).await?;
    let catalog = crate::catalog::read_catalog(&state.paths);
    let items: Vec<Value> = rows.into_iter().filter_map(|(id, theme_id, name, version, created_at, read)| {
        let theme = catalog.as_ref()?.themes.iter().find(|item| item.id == theme_id)?;
        if !show_r18 && (theme.r18 || crate::catalog::tags_mark_r18(&theme.tags)) { return None; }
        Some(json!({"id": id, "themeId": theme_id, "name": name, "version": version, "createdAt": created_at, "read": read}))
    }).collect();
    let unread = items.iter().filter(|item| item["read"] == false).count();
    Ok(Json(
        json!({"ok": true, "updates": items, "unread": unread}),
    ))
}

async fn read_update(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    if id.len() > 96
        || !id
            .rsplit_once(':')
            .is_some_and(|(theme, version)| valid_theme_id(theme) && version.parse::<i64>().is_ok())
    {
        return Err(ApiError::bad("无效的更新标识"));
    }
    let id = id.to_string();
    let user_id = session.user_id.clone();
    state.db.call(move |conn| {
        conn.execute("INSERT OR IGNORE INTO social.theme_update_reads(user_id, update_id, read_at)
            SELECT ?1, u.id, ?3 FROM social.theme_updates u JOIN social.user_follows f ON f.author_hash = u.author_hash
            WHERE u.id = ?2 AND f.follower_id = ?1 AND u.created_at >= f.created_at", params![user_id, id, now_iso()])?;
        Ok(())
    }).await.map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true})))
}

async fn versions(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !valid_theme_id(&id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    let (_, current) = crate::catalog::find_theme_meta(&state.paths, &id)
        .ok_or_else(|| ApiError::not_found("主题不存在"))?;
    let owned = theme_owned_by_user(&state, &session, &current).await?;
    if current.withdrawn_at.is_some() && !owned {
        return Err(ApiError::not_found("主题不存在"));
    }
    if !owned
        && crate::catalog::tags_mark_r18(&current.tags)
        && !user_show_r18(&state, &session.user_id).await?
    {
        return Err(ApiError::forbidden("请先开启 R18 内容偏好"));
    }
    let versions: Vec<Value> = crate::catalog::theme_versions(&state.paths, &id).into_iter()
        .map(|item| json!({"version": item.version, "name": item.name, "changelog": item.changelog, "updatedAt": item.updated_at,
            "minManagerVersion": item.min_manager_version, "formatVersion": item.format_version, "sha256": item.sha256,
            "current": item.version == current.version})).collect();
    Ok(Json(
        json!({"ok": true, "versions": versions, "canRollback": owned}),
    ))
}

async fn rollback(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    if !valid_theme_id(&id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    if state.jobs.is_running() {
        return Err(ApiError::conflict("存储迁移进行中，请稍后再试"));
    }
    let value = json_body(body).await?;
    let version = value
        .get("version")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::bad("无效的版本号"))?;
    let (_, current) = crate::catalog::find_theme_meta(&state.paths, &id)
        .ok_or_else(|| ApiError::not_found("主题不存在"))?;
    if !theme_owned_by_user(&state, &session, &current).await? {
        return Err(ApiError::forbidden("只能回退自己的主题"));
    }
    let restored = crate::catalog::rollback_version(&state.store, &state.paths, &id, version, &current)
        .await
        .map_err(ApiError::internal)?;
    state.refresh_withdrawn_assets();
    if let Some(hash) = restored.author_hash.as_deref() {
        if let Err(error) =
            record_release(&state.db, hash, &id, &restored.name, restored.version).await
        {
            tracing::warn!(?error, "failed to record rollback notification");
        }
    }
    if let Err(error) = accounts::audit(
        &state.db,
        Some(&session.user_id),
        "theme.rollback",
        Some("theme"),
        Some(&id),
        Some(&format!("from v{} to v{}", current.version, version)),
    )
    .await
    {
        tracing::warn!(?error, "failed to audit theme rollback");
    }
    Ok(Json(json!({"ok": true, "version": restored.version})))
}

async fn drafts(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let user_id = session.user_id.clone();
    let drafts: Vec<(String, String, String, String)> = state.db.call(move |conn| {
        let mut statement = conn.prepare("SELECT id, payload, created_at, updated_at FROM authors.submission_drafts WHERE user_id = ?1 ORDER BY updated_at DESC LIMIT 20")?;
        let rows = statement.query_map([user_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await.map_err(ApiError::internal)?;
    let drafts: Vec<Value> = drafts.into_iter().map(|(id, payload, created_at, updated_at)|
        json!({"id": id, "payload": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null), "createdAt": created_at, "updatedAt": updated_at})).collect();
    Ok(Json(json!({"ok": true, "drafts": drafts})))
}

async fn save_draft(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    if body.len() > 16_384 {
        return Err(ApiError::bad("草稿内容不能超过 16 KB"));
    }
    let value = json_body(body).await?;
    let payload = value
        .get("payload")
        .filter(|v| v.is_object())
        .ok_or_else(|| ApiError::bad("草稿内容无效"))?;
    let payload = serde_json::to_string(payload).map_err(ApiError::internal)?;
    if payload.len() > 12_000 {
        return Err(ApiError::bad("草稿内容过长"));
    }
    let requested_id = value.get("id").and_then(Value::as_str).unwrap_or("");
    let is_new = requested_id.is_empty();
    if !is_new && (requested_id.len() != 32 || !requested_id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(ApiError::bad("草稿 ID 无效"));
    }
    let id = if is_new {
        random_hex(16)
    } else {
        requested_id.to_string()
    };
    let user_id = session.user_id.clone();
    let id_for_db = id.clone();
    let saved = state.db.call(move |conn| {
        if is_new {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM authors.submission_drafts WHERE user_id = ?1", [&user_id], |row| row.get(0))?;
            if count >= 20 { return Ok(false); }
            let now = now_iso();
            conn.execute("INSERT INTO authors.submission_drafts(id, user_id, payload, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![id_for_db, user_id, payload, now])?;
            Ok(true)
        } else {
            Ok(conn.execute("UPDATE authors.submission_drafts SET payload = ?3, updated_at = ?4 WHERE id = ?1 AND user_id = ?2",
                params![id_for_db, user_id, payload, now_iso()])? > 0)
        }
    }).await.map_err(ApiError::internal)?;
    if !saved {
        return Err(ApiError::bad(if is_new {
            "最多保存 20 份草稿"
        } else {
            "草稿不存在"
        }));
    }
    Ok(Json(json!({"ok": true, "id": id})))
}

async fn delete_draft(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::bad("草稿 ID 无效"));
    }
    let id = id.to_string();
    let user_id = session.user_id.clone();
    state
        .db
        .call(move |conn| {
            conn.execute(
                "DELETE FROM authors.submission_drafts WHERE id = ?1 AND user_id = ?2",
                params![id, user_id],
            )?;
            Ok(())
        })
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true})))
}

async fn author_dashboard(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let hashes: HashSet<String> =
        crate::authors::author_hashes_for_user(&state.db, &session.user_id)
            .await
            .map_err(ApiError::internal)?
            .into_iter()
            .collect();
    let index = state.meta_index().await;
    let owned = index.owned_ids(&hashes, &session.username);
    let stats = state.theme_stats();
    let mut themes: Vec<Value> = owned.iter().filter_map(|id| {
        let meta = index.theme_by_id(id)?;
        if meta.withdrawn_at.is_some() { return None; }
        let stat = stats.get(id).copied().unwrap_or_default();
        Some(json!({"id": id, "name": meta.name, "downloads": stat.downloads, "heat": stat.heat, "version": meta.version}))
    }).collect();
    themes.sort_by(|a, b| b["downloads"].as_u64().cmp(&a["downloads"].as_u64()));
    let downloads: u64 = themes
        .iter()
        .map(|item| item["downloads"].as_u64().unwrap_or(0))
        .sum();
    let followers = crate::follows::follower_count(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let user_id = session.user_id.clone();
    let (pending, approved): (i64, i64) = state.db.call(move |conn| {
        let pending = conn.query_row("SELECT COUNT(*) FROM authors.submissions WHERE user_id = ?1 AND status = 'pending'", [&user_id], |row| row.get(0))?;
        let approved = conn.query_row("SELECT COUNT(*) FROM authors.submissions WHERE user_id = ?1 AND status = 'approved'", [&user_id], |row| row.get(0))?;
        Ok((pending, approved))
    }).await.map_err(ApiError::internal)?;
    let daily: Vec<(String, String, i64)> = state.db.call(|conn| {
        let mut statement = conn.prepare("SELECT theme_id, day, count FROM stats.theme_download_days WHERE day >= date('now', '-13 days')")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await.map_err(ApiError::internal)?;
    let mut by_day = std::collections::BTreeMap::<String, i64>::new();
    for (id, day, count) in daily {
        if owned.contains(&id) {
            *by_day.entry(day).or_default() += count;
        }
    }
    Ok(Json(
        json!({"ok": true, "themes": themes, "published": themes.len(), "downloads": downloads,
        "followers": followers, "pendingSubmissions": pending, "approvedSubmissions": approved,
        "daily": by_day.into_iter().map(|(day, count)| json!({"day": day, "downloads": count})).collect::<Vec<_>>()}),
    ))
}

async fn my_reports(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let user_id = session.user_id.clone();
    let mut reports: Vec<Value> = state.db.call(move |conn| {
        let mut statement = conn.prepare("SELECT id, theme_id, category, detail, status, resolution, created_at, updated_at
            FROM social.theme_reports WHERE reporter_id = ?1 ORDER BY created_at DESC LIMIT 100")?;
        let rows = statement.query_map([user_id], |row| Ok(json!({"id": row.get::<_, String>(0)?, "themeId": row.get::<_, String>(1)?,
            "category": row.get::<_, String>(2)?, "detail": row.get::<_, String>(3)?, "status": row.get::<_, String>(4)?,
            "resolution": row.get::<_, Option<String>>(5)?, "createdAt": row.get::<_, String>(6)?, "updatedAt": row.get::<_, String>(7)?})))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await.map_err(ApiError::internal)?;
    for report in &mut reports {
        let theme_id = report
            .get("themeId")
            .and_then(Value::as_str)
            .map(str::to_string);
        attach_theme_info(
            &state.paths,
            report,
            theme_id.as_deref(),
            "/account/api/preview",
        );
    }
    Ok(Json(json!({"ok": true, "reports": reports})))
}

async fn create_report(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    if body.len() > 4_096 {
        return Err(ApiError::bad("举报内容过长"));
    }
    let value = json_body(body).await?;
    let theme_id = value.get("themeId").and_then(Value::as_str).unwrap_or("");
    let category = value.get("category").and_then(Value::as_str).unwrap_or("");
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !valid_theme_id(theme_id)
        || !matches!(
            category,
            "copyright" | "malware" | "inappropriate" | "broken" | "other"
        )
        || !(10..=2000).contains(&detail.chars().count())
    {
        return Err(ApiError::bad("请填写有效的主题、类型和 10–2000 字说明"));
    }
    let catalog = crate::catalog::read_catalog(&state.paths)
        .ok_or_else(|| ApiError::not_found("主题不存在"))?;
    let theme = catalog
        .themes
        .iter()
        .find(|item| item.id == theme_id)
        .ok_or_else(|| ApiError::not_found("主题不存在"))?;
    if (theme.r18 || crate::catalog::tags_mark_r18(&theme.tags))
        && !user_show_r18(&state, &session.user_id).await?
    {
        return Err(ApiError::forbidden("请先开启 R18 内容偏好"));
    }
    if !crate::account_api::rate_allowed(&state, &format!("report:{}", session.user_id), 10, 86_400)
    {
        return Err(ApiError::too_many("今日举报次数已达上限"));
    }
    let id = random_hex(16);
    let (id_db, user_id, theme_id, category, detail) = (
        id.clone(),
        session.user_id.clone(),
        theme_id.to_string(),
        category.to_string(),
        detail.to_string(),
    );
    let inserted = state.db.call(move |conn| {
        let now = now_iso();
        Ok(conn.execute("INSERT INTO social.theme_reports(id, reporter_id, theme_id, category, detail, created_at, updated_at)
            SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?6 WHERE NOT EXISTS
            (SELECT 1 FROM social.theme_reports WHERE reporter_id = ?2 AND theme_id = ?3 AND status = 'pending')",
            params![id_db, user_id, theme_id, category, detail, now])? > 0)
    }).await.map_err(ApiError::internal)?;
    if !inserted {
        return Err(ApiError::conflict("该主题已有待处理举报"));
    }
    Ok(Json(json!({"ok": true, "id": id})))
}

async fn my_appeals(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let user_id = session.user_id.clone();
    let mut appeals: Vec<Value> = state.db.call(move |conn| {
        let mut statement = conn.prepare("SELECT a.id, a.target_kind, a.target_id, a.detail, a.status, a.resolution,
            a.created_at, a.updated_at,
            r.theme_id AS report_theme_id,
            s.theme_id AS submission_theme_id,
            s.name AS submission_name
            FROM social.theme_appeals a
            LEFT JOIN social.theme_reports r ON a.target_kind = 'report' AND r.id = a.target_id
            LEFT JOIN authors.submissions s ON a.target_kind = 'submission' AND s.id = a.target_id
            WHERE a.appellant_id = ?1 ORDER BY a.created_at DESC LIMIT 100")?;
        let rows = statement.query_map([user_id], |row| {
            let report_theme_id: Option<String> = row.get(8)?;
            let submission_theme_id: Option<String> = row.get(9)?;
            let submission_name: Option<String> = row.get(10)?;
            let theme_id = report_theme_id.or(submission_theme_id);
            Ok(json!({"id": row.get::<_, String>(0)?, "targetKind": row.get::<_, String>(1)?,
                "targetId": row.get::<_, String>(2)?, "detail": row.get::<_, String>(3)?, "status": row.get::<_, String>(4)?,
                "resolution": row.get::<_, Option<String>>(5)?, "createdAt": row.get::<_, String>(6)?,
                "updatedAt": row.get::<_, String>(7)?, "themeId": theme_id,
                "submissionName": submission_name}))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await.map_err(ApiError::internal)?;
    for appeal in &mut appeals {
        let theme_id = appeal
            .get("themeId")
            .and_then(Value::as_str)
            .map(str::to_string);
        attach_theme_info(
            &state.paths,
            appeal,
            theme_id.as_deref(),
            "/account/api/preview",
        );
        if appeal.get("themeName").is_none() {
            if let Some(name) = appeal
                .get("submissionName")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                if let Some(object) = appeal.as_object_mut() {
                    object.insert("themeName".to_string(), json!(name));
                }
            }
        }
    }
    Ok(Json(json!({"ok": true, "appeals": appeals})))
}

async fn create_appeal(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    if body.len() > 4_096 {
        return Err(ApiError::bad("申诉内容过长"));
    }
    let value = json_body(body).await?;
    let kind = value
        .get("targetKind")
        .and_then(Value::as_str)
        .unwrap_or("");
    let target_id = value.get("targetId").and_then(Value::as_str).unwrap_or("");
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !matches!(kind, "report" | "submission")
        || target_id.len() > 96
        || target_id.is_empty()
        || !(10..=2000).contains(&detail.chars().count())
    {
        return Err(ApiError::bad("请填写有效的申诉对象和 10–2000 字说明"));
    }
    if kind == "report" {
        if target_id.len() != 32 || !target_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ApiError::bad("举报 ID 无效"));
        }
        let target = target_id.to_string();
        let report: Option<(String, String)> = state
            .db
            .call(move |conn| {
                conn.query_row(
                    "SELECT theme_id, status FROM social.theme_reports WHERE id = ?1",
                    [target],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(Into::into)
            })
            .await
            .map_err(ApiError::internal)?;
        let Some((theme_id, status)) = report else {
            return Err(ApiError::not_found("举报不存在"));
        };
        if status != "accepted" {
            return Err(ApiError::bad("仅可申诉已受理的举报"));
        }
        let (_, meta) = crate::catalog::find_theme_meta(&state.paths, &theme_id)
            .ok_or_else(|| ApiError::not_found("主题不存在"))?;
        if !theme_owned_by_user(&state, &session, &meta).await? {
            return Err(ApiError::forbidden("只能申诉自己的主题"));
        }
    } else {
        if !crate::util::valid_submission_id(target_id) {
            return Err(ApiError::bad("投稿 ID 无效"));
        }
        let (target, user_id) = (target_id.to_string(), session.user_id.clone());
        let rejected = state
            .db
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT status FROM authors.submissions WHERE id = ?1 AND user_id = ?2",
                        params![target, user_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .as_deref()
                    == Some("rejected"))
            })
            .await
            .map_err(ApiError::internal)?;
        if !rejected {
            return Err(ApiError::forbidden("仅可申诉自己被驳回的投稿"));
        }
    }
    if !crate::account_api::rate_allowed(&state, &format!("appeal:{}", session.user_id), 10, 86_400)
    {
        return Err(ApiError::too_many("今日申诉次数已达上限"));
    }
    let id = random_hex(16);
    let (id_db, user_id, kind, target_id, detail) = (
        id.clone(),
        session.user_id.clone(),
        kind.to_string(),
        target_id.to_string(),
        detail.to_string(),
    );
    let inserted = state.db.call(move |conn| {
        let now = now_iso();
        Ok(conn.execute("INSERT INTO social.theme_appeals(id, appellant_id, target_kind, target_id, detail, created_at, updated_at)
            SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?6 WHERE NOT EXISTS
            (SELECT 1 FROM social.theme_appeals WHERE appellant_id = ?2 AND target_kind = ?3 AND target_id = ?4 AND status = 'pending')",
            params![id_db, user_id, kind, target_id, detail, now])? > 0)
    }).await.map_err(ApiError::internal)?;
    if !inserted {
        return Err(ApiError::conflict("该对象已有待处理申诉"));
    }
    Ok(Json(json!({"ok": true, "id": id})))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryQuery {
    kind: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryViewBody {
    theme_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteHistoryBody {
    kind: Option<String>,
    #[serde(default)]
    ids: Vec<i64>,
    #[serde(default)]
    all: bool,
}

fn history_kind(value: Option<&str>) -> &'static str {
    if value == Some("download") {
        "download"
    } else {
        "view"
    }
}

pub async fn record_theme_history(
    db: &Database,
    user_id: &str,
    theme_id: &str,
    kind: &str,
) -> anyhow::Result<()> {
    if user_id.is_empty() || !valid_theme_id(theme_id) {
        return Ok(());
    }
    let user_id = user_id.to_string();
    let theme_id = theme_id.to_string();
    let kind = history_kind(Some(kind));
    let created_at = now_iso();
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO social.theme_history(user_id, theme_id, kind, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(user_id, theme_id, kind)
             DO UPDATE SET created_at = excluded.created_at",
            params![user_id, theme_id, kind, created_at],
        )?;
        Ok(())
    })
    .await
}

async fn theme_history(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<Value>> {
    let kind = history_kind(query.kind.as_deref());
    let limit = query.limit.unwrap_or(100).clamp(1, 200);
    let user_id = session.user_id.clone();
    let rows = state
        .db
        .call(move |conn| {
            let mut statement = conn.prepare(
                "SELECT id, theme_id, created_at FROM social.theme_history
                 WHERE user_id = ?1 AND kind = ?2
                 ORDER BY created_at DESC LIMIT ?3",
            )?;
            let rows = statement.query_map(params![user_id, kind, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .map_err(ApiError::internal)?;
    let show_r18 = user_show_r18(&state, &session.user_id).await?;
    let catalog = crate::catalog::read_catalog(&state.paths);
    let mut items = Vec::new();
    for (id, theme_id, created_at) in rows {
        let Some(theme) = catalog
            .as_ref()
            .and_then(|catalog| catalog.themes.iter().find(|theme| theme.id == theme_id))
        else {
            continue;
        };
        if !show_r18 && (theme.r18 || crate::catalog::tags_mark_r18(&theme.tags)) {
            continue;
        }
        let preview = if theme.preview_path.is_empty() {
            String::new()
        } else {
            format!(
                "/account/api/preview?path={}",
                crate::util::quote_query(&theme.preview_path)
            )
        };
        items.push(json!({
            "id": id,
            "themeId": theme.id,
            "kind": kind,
            "createdAt": created_at,
            "themeName": theme.name,
            "themeAuthor": theme.author,
            "themeAuthorHash": theme.author_hash,
            "themeVersion": theme.version,
            "themePreviewUrl": preview,
            "themePreviewPath": theme.preview_path,
            "themeAdaptive": theme.adaptive,
            "r18": theme.r18,
        }));
    }
    Ok(Json(json!({"ok": true, "kind": kind, "items": items})))
}

async fn record_theme_view(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    record_history_entry(&state, session, body, "view").await
}

/// The manager records downloads itself (it fetches packages from a source's
/// asset URL rather than the account download route), so it reports them here.
async fn record_theme_download(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    record_history_entry(&state, session, body, "download").await
}

async fn record_history_entry(
    state: &AppState,
    session: SessionInfo,
    body: Bytes,
    kind: &'static str,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: HistoryViewBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("主题历史格式错误：{error}")))?;
    if !valid_theme_id(&body.theme_id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    let index = state.meta_index().await;
    let Some(meta) = index.theme_by_id(&body.theme_id) else {
        return Err(ApiError::not_found("主题不存在"));
    };
    if meta.withdrawn_at.is_some() {
        return Err(ApiError::not_found("主题不存在"));
    }
    record_theme_history(&state.db, &session.user_id, &meta.id, kind)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true})))
}

async fn delete_theme_history(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: DeleteHistoryBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("主题历史格式错误：{error}")))?;
    if !body.all && body.ids.is_empty() {
        return Err(ApiError::bad("请选择要删除的记录"));
    }
    let kind = history_kind(body.kind.as_deref());
    let ids = body
        .ids
        .into_iter()
        .filter(|id| *id > 0)
        .take(500)
        .collect::<Vec<_>>();
    let user_id = session.user_id.clone();
    let all = body.all;
    let deleted = state
        .db
        .call(move |conn| {
            if all {
                return Ok(conn.execute(
                    "DELETE FROM social.theme_history WHERE user_id = ?1 AND kind = ?2",
                    params![user_id, kind],
                )?);
            }
            let mut deleted = 0;
            for id in ids {
                deleted += conn.execute(
                    "DELETE FROM social.theme_history
                     WHERE user_id = ?1 AND kind = ?2 AND id = ?3",
                    params![user_id, kind, id],
                )?;
            }
            Ok(deleted)
        })
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "deleted": deleted})))
}

/// Add the published theme's display metadata to a report or appeal row so the
/// review UI can show what was reported instead of a bare theme ID.
fn attach_theme_info(
    paths: &crate::config::Paths,
    item: &mut Value,
    theme_id: Option<&str>,
    preview_base: &str,
) {
    let Some(theme_id) = theme_id.filter(|id| !id.is_empty()) else {
        return;
    };
    let (name, author, author_hash, version, preview_path) =
        if let Some(theme) = crate::catalog::read_catalog(paths)
            .and_then(|catalog| catalog.themes.iter().find(|theme| theme.id == theme_id).cloned())
        {
            (
                theme.name,
                theme.author,
                theme.author_hash,
                theme.version as i64,
                theme.preview_path,
            )
        } else if let Some((_, meta)) = crate::catalog::find_theme_meta(paths, theme_id) {
            (
                meta.name,
                meta.author,
                meta.author_hash,
                meta.version,
                meta.preview,
            )
        } else {
            return;
        };
    let preview_url = if preview_path.is_empty() {
        String::new()
    } else {
        format!(
            "{preview_base}?path={}",
            crate::util::quote_query(&preview_path)
        )
    };
    let Some(object) = item.as_object_mut() else {
        return;
    };
    object.insert("themeName".to_string(), json!(name));
    object.insert("themeAuthor".to_string(), json!(author));
    object.insert("themeAuthorHash".to_string(), json!(author_hash));
    object.insert("themeVersion".to_string(), json!(version));
    object.insert("themePreviewUrl".to_string(), json!(preview_url));
}

async fn admin_reports(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let mut reports: Vec<Value> = state.db.call(|conn| {
        let mut statement = conn.prepare("SELECT r.id, r.theme_id, r.category, r.detail, r.status, r.resolution,
            r.created_at, u.username FROM social.theme_reports r LEFT JOIN users u ON u.id = r.reporter_id
            ORDER BY CASE WHEN r.status = 'pending' THEN 0 ELSE 1 END, r.created_at DESC LIMIT 200")?;
        let rows = statement.query_map([], |row| Ok(json!({"id": row.get::<_, String>(0)?, "themeId": row.get::<_, String>(1)?,
            "category": row.get::<_, String>(2)?, "detail": row.get::<_, String>(3)?, "status": row.get::<_, String>(4)?,
            "resolution": row.get::<_, Option<String>>(5)?, "createdAt": row.get::<_, String>(6)?,
            "reporter": row.get::<_, Option<String>>(7)?})))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await.map_err(ApiError::internal)?;
    for report in &mut reports {
        let theme_id = report
            .get("themeId")
            .and_then(Value::as_str)
            .map(str::to_string);
        attach_theme_info(&state.paths, report, theme_id.as_deref(), "/admin/api/preview");
    }
    Ok(Json(json!({"ok": true, "reports": reports})))
}

async fn resolve_report(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    if body.len() > 4_096 {
        return Err(ApiError::bad("处理说明过长"));
    }
    let value = json_body(body).await?;
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    let resolution = value
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if id.len() != 32
        || !id.bytes().all(|b| b.is_ascii_hexdigit())
        || !matches!(status, "accepted" | "dismissed")
        || !(1..=2000).contains(&resolution.chars().count())
    {
        return Err(ApiError::bad("请填写有效处理结果和说明"));
    }
    let (id_db, status_db, resolution, actor_id) = (
        id.to_string(),
        status.to_string(),
        resolution.to_string(),
        actor.user_id.clone(),
    );
    let changed = state.db.call(move |conn| {
        Ok(conn.execute("UPDATE social.theme_reports SET status = ?2, resolution = ?3, moderator_id = ?4, updated_at = ?5 WHERE id = ?1 AND status = 'pending'",
            params![id_db, status_db, resolution, actor_id, now_iso()])? > 0)
    }).await.map_err(ApiError::internal)?;
    if !changed {
        return Err(ApiError::conflict("举报已处理或不存在"));
    }
    if let Err(error) = accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "report.resolve",
        Some("report"),
        Some(id),
        Some(status),
    )
    .await
    {
        tracing::warn!(?error, "failed to audit report resolution");
    }
    Ok(Json(json!({"ok": true})))
}

async fn admin_appeals(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let mut appeals: Vec<Value> = state.db.call(|conn| {
        let mut statement = conn.prepare("SELECT a.id, a.target_kind, a.target_id, a.detail, a.status, a.resolution,
            a.created_at, u.username,
            r.theme_id AS report_theme_id,
            s.theme_id AS submission_theme_id,
            s.name AS submission_name
            FROM social.theme_appeals a LEFT JOIN users u ON u.id = a.appellant_id
            LEFT JOIN social.theme_reports r ON a.target_kind = 'report' AND r.id = a.target_id
            LEFT JOIN authors.submissions s ON a.target_kind = 'submission' AND s.id = a.target_id
            ORDER BY CASE WHEN a.status = 'pending' THEN 0 ELSE 1 END, a.created_at DESC LIMIT 200")?;
        let rows = statement.query_map([], |row| {
            let report_theme_id: Option<String> = row.get(8)?;
            let submission_theme_id: Option<String> = row.get(9)?;
            let submission_name: Option<String> = row.get(10)?;
            let theme_id = report_theme_id.or(submission_theme_id);
            Ok(json!({"id": row.get::<_, String>(0)?, "targetKind": row.get::<_, String>(1)?,
                "targetId": row.get::<_, String>(2)?, "detail": row.get::<_, String>(3)?, "status": row.get::<_, String>(4)?,
                "resolution": row.get::<_, Option<String>>(5)?, "createdAt": row.get::<_, String>(6)?,
                "appellant": row.get::<_, Option<String>>(7)?, "themeId": theme_id,
                "submissionName": submission_name}))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await.map_err(ApiError::internal)?;
    for appeal in &mut appeals {
        let theme_id = appeal
            .get("themeId")
            .and_then(Value::as_str)
            .map(str::to_string);
        attach_theme_info(&state.paths, appeal, theme_id.as_deref(), "/admin/api/preview");
        let missing_name = appeal.get("themeName").is_none();
        let submission_name = appeal
            .get("submissionName")
            .and_then(Value::as_str)
            .map(str::to_string);
        let appellant = appeal
            .get("appellant")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(object) = appeal.as_object_mut() {
            if missing_name {
                if let Some(name) = submission_name {
                    object.insert("themeName".to_string(), json!(name));
                }
            }
            if object.get("themeAuthor").is_none() {
                if let Some(appellant) = appellant {
                    object.insert("themeAuthor".to_string(), json!(appellant));
                }
            }
        }
    }
    Ok(Json(json!({"ok": true, "appeals": appeals})))
}

async fn resolve_appeal(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    if body.len() > 4_096 {
        return Err(ApiError::bad("处理说明过长"));
    }
    let value = json_body(body).await?;
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    let resolution = value
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if id.len() != 32
        || !id.bytes().all(|b| b.is_ascii_hexdigit())
        || !matches!(status, "accepted" | "rejected")
        || !(1..=2000).contains(&resolution.chars().count())
    {
        return Err(ApiError::bad("请填写有效处理结果和说明"));
    }
    let (id_db, status_db, resolution, actor_id) = (
        id.to_string(),
        status.to_string(),
        resolution.to_string(),
        actor.user_id.clone(),
    );
    let changed = state.db.call(move |conn| {
        let transaction = conn.transaction()?;
        let target: Option<(String, String)> = transaction.query_row("SELECT target_kind, target_id FROM social.theme_appeals WHERE id = ?1 AND status = 'pending'",
            [&id_db], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        let Some((kind, target_id)) = target else { return Ok(false); };
        if status_db == "accepted" && kind == "report" {
            if transaction.execute("UPDATE social.theme_reports SET status = 'overturned', updated_at = ?2 WHERE id = ?1 AND status = 'accepted'",
                params![target_id, now_iso()])? == 0 {
                return Ok(false);
            }
        }
        transaction.execute("UPDATE social.theme_appeals SET status = ?2, resolution = ?3, moderator_id = ?4, updated_at = ?5 WHERE id = ?1 AND status = 'pending'",
            params![id_db, status_db, resolution, actor_id, now_iso()])?;
        transaction.commit()?;
        Ok(true)
    }).await.map_err(ApiError::internal)?;
    if !changed {
        return Err(ApiError::conflict("申诉已处理或不存在"));
    }
    if let Err(error) = accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "appeal.resolve",
        Some("appeal"),
        Some(id),
        Some(status),
    )
    .await
    {
        tracing::warn!(?error, "failed to audit appeal resolution");
    }
    if status == "accepted" {
        let detail = format!("作者申诉已受理，举报 ID {id}");
        if let Err(error) = accounts::audit(
            &state.db,
            None,
            "theme.report_appeal_overturned",
            Some("appeal"),
            Some(id),
            Some(&detail),
        )
        .await
        {
            tracing::warn!(?error, "failed to audit overturned report");
        }
    }
    Ok(Json(json!({"ok": true})))
}

async fn mirror_health(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let sample = crate::catalog::read_catalog(&state.paths).and_then(|catalog| {
        catalog
            .themes
            .iter()
            .find(|item| item.path.starts_with("themes/"))
            .map(|item| item.path.clone())
    });
    let mirrors = state.mirrors.health(sample.as_deref()).await;
    Ok(Json(
        json!({"ok": true, "samplePath": sample, "mirrors": mirrors}),
    ))
}
