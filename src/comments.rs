//! Per-theme comments and the private user/author message threads they form.
//!
//! A top-level comment is either public or visible only to the theme author
//! (`visibility = "author"`). Replies inherit their thread's visibility, so a
//! private user/author conversation never leaks into the public list. Admins
//! can read and moderate everything.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use axum::extract::{Extension, Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use regex::Regex;
use rusqlite::{params, params_from_iter, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::accounts::{self, SessionInfo};
use crate::api::{json_body, ApiError, AppState};
use crate::authors;
use crate::catalog;
use crate::db::{self, Database};
use crate::util::{now_iso, random_hex, valid_theme_id};

type ApiResult<T> = Result<T, ApiError>;

const META_KEY: &str = "comment_retention_days";
pub const DEFAULT_RETENTION_DAYS: i64 = 30;
pub const MAX_RETENTION_DAYS: i64 = 3650;
const MAX_COMMENT_CHARS: usize = 2000;
const MAX_COMMENT_BYTES: usize = 8000;
const MAX_MESSAGE_RECORDS: i64 = 5000;

const COMMENT_COLUMNS: &str = "c.id, c.theme_id, c.user_id, u.username, u.display_name, u.role,
     c.parent_id, c.visibility, c.content, c.created_at, u.avatar";

#[derive(Clone, Debug)]
struct CommentRecord {
    id: String,
    theme_id: String,
    user_id: String,
    username: String,
    display_name: String,
    user_role: String,
    parent_id: Option<String>,
    visibility: String,
    content: String,
    created_at: String,
    avatar: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ThemeInfo {
    pub id: String,
    pub name: String,
    pub author: String,
    pub author_hash: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ThemeRole {
    pub is_admin: bool,
    pub is_author: bool,
}

#[derive(Clone, Debug)]
struct CommentThread {
    comment: CommentRecord,
    replies: Vec<CommentRecord>,
}

/// Routes mounted under both `/api` and `/admin/api` behind the admin guard.
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/comments", get(admin_list))
        .route("/comments/delete", post(admin_delete))
        .route("/comments/clear", post(admin_clear))
        .route(
            "/comments/settings",
            get(admin_settings_get).post(admin_settings_save),
        )
}

/// Validate a comment body while keeping emoji and other non-ASCII text intact.
pub fn validate_content(raw: &str) -> ApiResult<String> {
    let content = raw.trim();
    if content.is_empty() {
        return Err(ApiError::bad("评论内容不能为空"));
    }
    if content.chars().count() > MAX_COMMENT_CHARS {
        return Err(ApiError::bad(format!(
            "评论最多 {MAX_COMMENT_CHARS} 个字符"
        )));
    }
    if content.len() > MAX_COMMENT_BYTES {
        return Err(ApiError::bad("评论内容过长"));
    }
    if content
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err(ApiError::bad("评论包含不支持的控制字符"));
    }
    Ok(content.to_string())
}

fn normalize_visibility(value: &str) -> &'static str {
    match value.trim().to_ascii_lowercase().as_str() {
        "author" | "private" | "owner" => "author",
        _ => "public",
    }
}

fn mention_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"@([\p{L}\p{M}\p{N}._-]{3,32})").expect("valid mention regex"))
}

const MENTION_PLATFORMS: &[&str] = &[
    "github",
    "gitee",
    "bilibili",
    "coolapk",
    "telegram",
    "weibo",
    "xiaohongshu",
    "xda",
    "youtube",
    "afdian",
    "x",
    "twitter",
    "zhihu",
    "threads",
    "douyin",
];

fn fenced_code_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```.*?```").expect("valid fenced code regex"))
}

fn inline_code_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`\n]*`").expect("valid inline code regex"))
}

/// Mentions inside code samples are examples, not notifications.
fn strip_code_for_mentions(content: &str) -> String {
    let without_fenced = fenced_code_regex().replace_all(content, " ");
    inline_code_regex()
        .replace_all(&without_fenced, " ")
        .into_owned()
}

/// Collect local `@username` mentions without confusing emails, URLs or
/// `username@github` platform mentions for local accounts.
fn extract_local_mentions(content: &str) -> Vec<String> {
    let content = strip_code_for_mentions(content);
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for capture in mention_regex().captures_iter(&content) {
        let Some(full) = capture.get(0) else {
            continue;
        };
        let Some(name) = capture.get(1) else {
            continue;
        };
        let boundary_ok = match content[..full.start()].chars().next_back() {
            None => true,
            Some(previous) => {
                previous.is_whitespace()
                    || matches!(
                        previous,
                        '(' | '（'
                            | '['
                            | '【'
                            | ','
                            | ';'
                            | ':'
                            | '!'
                            | '?'
                            | '，'
                            | '。'
                            | '；'
                            | '：'
                            | '！'
                            | '？'
                            | '、'
                            | '>'
                            | '<'
                            | '“'
                            | '"'
                            | '\''
                    )
            }
        };
        if !boundary_ok {
            continue;
        }
        let name = name.as_str();
        if name.chars().count() < 3 {
            continue;
        }
        if MENTION_PLATFORMS
            .iter()
            .any(|platform| name.eq_ignore_ascii_case(platform))
        {
            continue;
        }
        if seen.insert(name.to_lowercase()) {
            names.push(name.to_string());
            if names.len() >= 10 {
                break;
            }
        }
    }
    names
}

async fn resolve_mention_users(db: &Database, names: &[String]) -> Vec<(String, String)> {
    let names = names.to_vec();
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT id, username FROM users
             WHERE status = 'active' AND username = ?1 COLLATE NOCASE",
        )?;
        let mut users = Vec::new();
        for name in names {
            let trimmed = name
                .trim_end_matches(|ch| matches!(ch, '.' | '-' | '_'))
                .to_string();
            let mut candidates = vec![name];
            if trimmed != candidates[0] && trimmed.chars().count() >= 3 {
                candidates.push(trimmed);
            }
            for candidate in candidates {
                let found = statement
                    .query_row(params![candidate], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .optional()?;
                if let Some(found) = found {
                    users.push(found);
                    break;
                }
            }
        }
        Ok(users)
    })
    .await
    .unwrap_or_default()
}

/// Record local mentions for public comments. Private threads are skipped:
/// their mention targets are not allowed to see the thread.
async fn record_mentions(
    db: &Database,
    comment_id: &str,
    content: &str,
    visibility: &str,
    mentioner_id: &str,
    excluded: &HashSet<String>,
) {
    if visibility != "public" {
        return;
    }
    let names = extract_local_mentions(content);
    if names.is_empty() {
        return;
    }
    let users = resolve_mention_users(db, &names).await;
    if users.is_empty() {
        return;
    }
    let now = now_iso();
    let comment_id = comment_id.to_string();
    let mentioner_id = mentioner_id.to_string();
    let excluded = excluded.clone();
    let _ = db
        .call(move |conn| {
            let mut statement = conn.prepare(
                "INSERT OR IGNORE INTO social.theme_comment_mentions(comment_id, user_id, created_at)
                 VALUES (?1, ?2, ?3)",
            )?;
            let mut seen = HashSet::new();
            for (user_id, _) in users {
                if user_id == mentioner_id || excluded.contains(&user_id) {
                    continue;
                }
                if !seen.insert(user_id.clone()) {
                    continue;
                }
                statement.execute(params![comment_id, user_id, now])?;
            }
            Ok(())
        })
        .await;
}

async fn mentioned_comment_rows(
    db: &Database,
    user_id: &str,
    theme_id: Option<&str>,
) -> Vec<(String, String, Option<String>)> {
    let user_id = user_id.to_string();
    let theme_id = theme_id.map(str::to_string);
    db.call(move |conn| {
        let (sql, values): (&str, Vec<String>) = match &theme_id {
            Some(theme_id) => (
                "SELECT m.comment_id, c.theme_id, c.parent_id
                 FROM social.theme_comment_mentions m
                 JOIN social.theme_comments c ON c.id = m.comment_id
                 WHERE m.user_id = ?1 AND c.theme_id = ?2",
                vec![user_id, theme_id.clone()],
            ),
            None => (
                "SELECT m.comment_id, c.theme_id, c.parent_id
                 FROM social.theme_comment_mentions m
                 JOIN social.theme_comments c ON c.id = m.comment_id
                 WHERE m.user_id = ?1",
                vec![user_id],
            ),
        };
        let mut statement = conn.prepare(sql)?;
        let rows = statement.query_map(params_from_iter(values.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    })
    .await
    .unwrap_or_default()
}

async fn theme_infos(state: &AppState) -> HashMap<String, ThemeInfo> {
    let index = state.meta_index().await;
    let mut themes: HashMap<String, ThemeInfo> = HashMap::new();
    for meta in index.themes() {
        themes.insert(
            meta.id.clone(),
            ThemeInfo {
                id: meta.id.clone(),
                name: meta.name.clone(),
                author: meta.author.clone(),
                author_hash: meta.author_hash.clone().unwrap_or_default(),
            },
        );
    }
    if let Some(catalog) = catalog::read_catalog(&state.paths) {
        for entry in &catalog.themes {
            let slot = themes.entry(entry.id.clone()).or_insert_with(|| ThemeInfo {
                id: entry.id.clone(),
                name: entry.name.clone(),
                author: entry.author.clone(),
                author_hash: String::new(),
            });
            if slot.name.is_empty() {
                slot.name = entry.name.clone();
            }
            if slot.author.is_empty() {
                slot.author = entry.author.clone();
            }
        }
    }
    themes
}

async fn theme_info(state: &AppState, theme_id: &str) -> Option<ThemeInfo> {
    let infos = theme_infos(state).await;
    infos.get(theme_id).cloned()
}

async fn theme_role(state: &AppState, session: &SessionInfo, theme: &ThemeInfo) -> ThemeRole {
    let is_admin = session.role == "admin";
    let mut is_author = is_admin;
    if !is_author && !theme.author_hash.is_empty() {
        if let Ok(list) = authors::authors_for_user(&state.db, &session.user_id).await {
            is_author = list.iter().any(|author| author.hash == theme.author_hash);
        }
    }
    if !is_author && !theme.author.is_empty() {
        is_author = theme
            .author
            .trim()
            .eq_ignore_ascii_case(session.username.trim());
    }
    ThemeRole {
        is_admin,
        is_author,
    }
}

async fn owned_theme_ids(state: &AppState, session: &SessionInfo) -> HashSet<String> {
    if session.role == "admin" {
        return HashSet::new();
    }
    let Ok(list) = authors::authors_for_user(&state.db, &session.user_id).await else {
        return HashSet::new();
    };
    let hashes: HashSet<String> = list.into_iter().map(|author| author.hash).collect();
    let index = state.meta_index().await;
    index.owned_ids(&hashes, &session.username)
}

async fn theme_author_user_ids(db: &Database, hash: &str) -> HashSet<String> {
    if hash.is_empty() {
        return HashSet::new();
    }
    let hash = hash.to_string();
    db.call(move |conn| {
        let mut statement =
            conn.prepare("SELECT user_id FROM authors.authors WHERE hash = ?1 AND user_id IS NOT NULL")?;
        let mut rows = statement.query(params![hash])?;
        let mut ids = HashSet::new();
        while let Some(row) = rows.next()? {
            ids.insert(row.get::<_, String>(0)?);
        }
        Ok(ids)
    })
    .await
    .unwrap_or_default()
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommentRecord> {
    Ok(CommentRecord {
        id: row.get(0)?,
        theme_id: row.get(1)?,
        user_id: row.get(2)?,
        username: row.get(3)?,
        display_name: row.get(4)?,
        user_role: row.get(5)?,
        parent_id: row.get(6)?,
        visibility: row.get(7)?,
        content: row.get(8)?,
        created_at: row.get(9)?,
        avatar: row.get(10)?,
    })
}

async fn theme_records(db: &Database, theme_id: &str, limit: i64) -> ApiResult<Vec<CommentRecord>> {
    let theme_id = theme_id.to_string();
    db.call(move |conn| {
        let sql = format!(
            "SELECT {COMMENT_COLUMNS} FROM social.theme_comments c
             JOIN users u ON u.id = c.user_id
             WHERE c.theme_id = ?1
             ORDER BY c.created_at ASC, c.id ASC
             LIMIT ?2"
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(params![theme_id, limit], record_from_row)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    })
    .await
    .map_err(ApiError::internal)
}

async fn recent_records(db: &Database) -> ApiResult<Vec<CommentRecord>> {
    db.call(move |conn| {
        let sql = format!(
            "SELECT {COMMENT_COLUMNS} FROM social.theme_comments c
             JOIN users u ON u.id = c.user_id
             ORDER BY c.created_at DESC, c.id DESC
             LIMIT ?1"
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(params![MAX_MESSAGE_RECORDS], record_from_row)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    })
    .await
    .map_err(ApiError::internal)
}

async fn fetch_record(db: &Database, id: &str) -> ApiResult<Option<CommentRecord>> {
    let id = id.to_string();
    db.call(move |conn| {
        let sql = format!(
            "SELECT {COMMENT_COLUMNS} FROM social.theme_comments c
             JOIN users u ON u.id = c.user_id
             WHERE c.id = ?1"
        );
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![id])?;
        Ok(rows.next()?.map(record_from_row).transpose()?)
    })
    .await
    .map_err(ApiError::internal)
}

/// Fold the flat rows into top-level threads and drop threads the viewer may
/// not read. Replies never outlive their root because of the foreign key.
fn build_threads(
    records: Vec<CommentRecord>,
    viewer: &SessionInfo,
    role: ThemeRole,
) -> Vec<CommentThread> {
    let can_see_all = role.is_admin || role.is_author;
    let mut replies: HashMap<String, Vec<CommentRecord>> = HashMap::new();
    let mut roots = Vec::new();
    for record in records {
        if let Some(parent) = record.parent_id.clone() {
            replies.entry(parent).or_default().push(record);
        } else {
            roots.push(record);
        }
    }
    roots
        .into_iter()
        .filter(|root| can_see_all || root.visibility == "public" || root.user_id == viewer.user_id)
        .map(|root| CommentThread {
            replies: replies.remove(&root.id).unwrap_or_default(),
            comment: root,
        })
        .collect()
}

fn comment_json(
    record: &CommentRecord,
    theme: Option<&ThemeInfo>,
    viewer: &SessionInfo,
    role: ThemeRole,
    author_user_ids: &HashSet<String>,
    mention_ids: &HashSet<String>,
) -> Value {
    let can_moderate = role.is_admin || role.is_author;
    let mine = record.user_id == viewer.user_id;
    json!({
        "id": record.id,
        "themeId": record.theme_id,
        "themeName": theme.map(|theme| theme.name.clone()).unwrap_or_default(),
        "themeAuthor": theme.map(|theme| theme.author.clone()).unwrap_or_default(),
        "userId": record.user_id,
        "username": record.username,
        "displayName": record.display_name,
        "parentId": record.parent_id,
        "visibility": record.visibility,
        "content": record.content,
        "createdAt": record.created_at,
        "avatarUrl": record
            .avatar
            .as_deref()
            .filter(|value| value.starts_with("avatars/users/"))
            .map(|_| format!("/account/api/users/{}/avatar", record.user_id))
            .unwrap_or_default(),
        "isThemeAuthor": author_user_ids.contains(&record.user_id),
        "isAdmin": record.user_role == "admin",
        "mine": mine,
        "canDelete": can_moderate || mine,
        "canReply": (role.is_admin || !viewer.user_id.is_empty())
            && (can_moderate || mine || record.visibility == "public"),
        "mentionsMe": mention_ids.contains(&record.id),
    })
}

fn threads_json(
    threads: Vec<CommentThread>,
    theme: Option<&ThemeInfo>,
    viewer: &SessionInfo,
    role: ThemeRole,
    author_user_ids: &HashSet<String>,
    mention_ids: &HashSet<String>,
) -> Vec<Value> {
    threads
        .into_iter()
        .map(|thread| {
            let mut value = comment_json(
                &thread.comment,
                theme,
                viewer,
                role,
                author_user_ids,
                mention_ids,
            );
            let replies: Vec<Value> = thread
                .replies
                .iter()
                .map(|reply| comment_json(reply, theme, viewer, role, author_user_ids, mention_ids))
                .collect();
            if let Some(object) = value.as_object_mut() {
                object.insert("replies".to_string(), Value::Array(replies));
            }
            value
        })
        .collect()
}

pub async fn retention_days(db: &Database) -> i64 {
    let loaded = db
        .call(|conn| {
            let value = db::get_meta(conn, META_KEY)?;
            Ok(value.and_then(|value| value.parse::<i64>().ok()))
        })
        .await
        .ok()
        .flatten();
    loaded
        .unwrap_or(DEFAULT_RETENTION_DAYS)
        .clamp(0, MAX_RETENTION_DAYS)
}

async fn save_retention_days(db: &Database, days: i64) -> ApiResult<i64> {
    let days = days.clamp(0, MAX_RETENTION_DAYS);
    let value = days.to_string();
    db.call(move |conn| {
        db::set_meta(conn, META_KEY, &value)?;
        Ok(())
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(days)
}

async fn insert_record(
    db: &Database,
    theme: &ThemeInfo,
    session: &SessionInfo,
    content: String,
    visibility: &'static str,
    parent_id: Option<String>,
    can_moderate: bool,
) -> ApiResult<CommentRecord> {
    let id = format!("cmt_{}", random_hex(12));
    let theme_id = theme.id.clone();
    let user_id = session.user_id.clone();
    let now = now_iso();
    let comment_id = id.clone();
    let result: Result<(), String> = db
        .call(move |conn| {
            let mut parent_id = parent_id;
            let mut visibility = visibility.to_string();
            if let Some(parent) = parent_id.clone() {
                let parent_row: Option<(String, Option<String>, String, String)> = conn
                    .query_row(
                        "SELECT theme_id, parent_id, visibility, user_id
                         FROM social.theme_comments WHERE id = ?1",
                        params![parent],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                let Some((parent_theme, root_parent, parent_visibility, parent_user)) = parent_row
                else {
                    return Ok(Err("回复的评论不存在".to_string()));
                };
                if parent_theme != theme_id {
                    return Ok(Err("回复的评论不属于该主题".to_string()));
                }
                // Reply-to-reply is flattened onto the root thread, so the
                // private-thread owner check must look at the root.
                let (root_id, root_user) = match root_parent {
                    Some(root) => {
                        let root_user: Option<String> = conn
                            .query_row(
                                "SELECT user_id FROM social.theme_comments WHERE id = ?1",
                                params![root],
                                |row| row.get(0),
                            )
                            .optional()?;
                        let Some(root_user) = root_user else {
                            return Ok(Err("原始评论不存在".to_string()));
                        };
                        (root, root_user)
                    }
                    None => (parent.clone(), parent_user),
                };
                if parent_visibility == "author" && root_user != user_id && !can_moderate {
                    return Ok(Err("无权回复该私密留言".to_string()));
                }
                parent_id = Some(root_id);
                visibility = parent_visibility;
            }
            conn.execute(
                "INSERT INTO social.theme_comments
                 (id, theme_id, user_id, parent_id, visibility, content, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![comment_id, theme_id, user_id, parent_id, visibility, content, now],
            )?;
            Ok(Ok(()))
        })
        .await
        .map_err(ApiError::internal)?;
    result.map_err(ApiError::bad)?;
    fetch_record(db, &id)
        .await?
        .ok_or_else(|| ApiError::internal("评论写入后无法读取"))
}

/// Delete a single thread/reply. `owns_theme` lets the theme author moderate
/// every comment on their own theme; admins pass `owns_theme = true` too.
async fn delete_record(db: &Database, id: &str) -> ApiResult<usize> {
    let id = id.to_string();
    db.call(move |conn| {
        let deleted = conn.execute("DELETE FROM social.theme_comments WHERE id = ?1", params![id])?;
        Ok(deleted)
    })
    .await
    .map_err(ApiError::internal)
}

pub async fn clear_theme(db: &Database, theme_id: &str) -> ApiResult<usize> {
    let theme_id = theme_id.to_string();
    db.call(move |conn| {
        let deleted = conn.execute(
            "DELETE FROM social.theme_comments WHERE theme_id = ?1",
            params![theme_id],
        )?;
        Ok(deleted)
    })
    .await
    .map_err(ApiError::internal)
}

async fn clear_all(db: &Database) -> ApiResult<usize> {
    db.call(|conn| {
        let deleted = conn.execute("DELETE FROM social.theme_comments", [])?;
        Ok(deleted)
    })
    .await
    .map_err(ApiError::internal)
}

async fn clear_themes(db: &Database, theme_ids: &HashSet<String>) -> ApiResult<usize> {
    if theme_ids.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = theme_ids.iter().cloned().collect();
    db.call(move |conn| {
        let placeholders = std::iter::repeat("?")
            .take(ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM social.theme_comments WHERE theme_id IN ({placeholders})");
        let deleted = conn.execute(&sql, params_from_iter(ids.iter()))?;
        Ok(deleted)
    })
    .await
    .map_err(ApiError::internal)
}

async fn clear_user_messages(
    db: &Database,
    user_id: &str,
    theme_id: Option<&str>,
) -> ApiResult<usize> {
    let user_id = user_id.to_string();
    let theme_id = theme_id.map(str::to_string);
    db.call(move |conn| {
        let deleted = match theme_id {
            Some(theme_id) => conn.execute(
                "DELETE FROM social.theme_comments
                 WHERE theme_id = ?1 AND (
                     user_id = ?2 OR parent_id IN (
                         SELECT id FROM social.theme_comments WHERE theme_id = ?1 AND user_id = ?2
                     )
                 )",
                params![theme_id, user_id],
            )?,
            None => conn.execute(
                "DELETE FROM social.theme_comments
                 WHERE user_id = ?1 OR parent_id IN (
                     SELECT id FROM social.theme_comments WHERE user_id = ?1
                 )",
                params![user_id],
            )?,
        };
        Ok(deleted)
    })
    .await
    .map_err(ApiError::internal)
}

pub async fn purge_expired(db: &Database) -> anyhow::Result<usize> {
    let days = retention_days(db).await;
    if days <= 0 {
        return Ok(0);
    }
    let cutoff =
        (Utc::now() - ChronoDuration::days(days)).to_rfc3339_opts(SecondsFormat::Secs, true);
    db.call(move |conn| {
        let deleted = conn.execute(
            "DELETE FROM social.theme_comments WHERE created_at < ?1",
            params![cutoff],
        )?;
        Ok(deleted)
    })
    .await
    .map_err(|error| anyhow::anyhow!("comment cleanup failed: {error}"))
}

// ── Account API handlers (mounted under /account/api) ──────────────────────

#[derive(Deserialize)]
pub struct MessageQuery {
    limit: Option<i64>,
}

pub async fn account_list(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Path(theme_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !valid_theme_id(&theme_id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    let theme = theme_info(&state, &theme_id)
        .await
        .ok_or_else(|| ApiError::not_found("主题不存在"))?;
    let role = theme_role(&state, &session, &theme).await;
    let records = theme_records(&state.db, &theme_id, 2000).await?;
    let threads = build_threads(records, &session, role);
    let author_ids = theme_author_user_ids(&state.db, &theme.author_hash).await;
    let mention_ids: HashSet<String> =
        mentioned_comment_rows(&state.db, &session.user_id, Some(&theme_id))
            .await
            .into_iter()
            .map(|(comment_id, _, _)| comment_id)
            .collect();
    let comments = threads_json(
        threads,
        Some(&theme),
        &session,
        role,
        &author_ids,
        &mention_ids,
    );
    let retention = retention_days(&state.db).await;
    Ok(Json(json!({
        "ok": true,
        "theme": {
            "id": theme.id,
            "name": theme.name,
            "author": theme.author,
        },
        "comments": comments,
        "isThemeAuthor": role.is_author,
        "isAdmin": role.is_admin,
        "retentionDays": retention,
    })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CreateCommentBody {
    #[serde(default)]
    content: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    parent_id: Option<String>,
}

pub async fn account_create(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Path(theme_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    if !valid_theme_id(&theme_id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    if !crate::account_api::rate_allowed(&state, &format!("comment:{}", session.user_id), 30, 60) {
        return Err(ApiError::too_many("留言过于频繁，请稍后再试"));
    }
    if session.role != "admin" {
        if let Some(author) = authors::active_author_for_user(&state.db, &session.user_id)
            .await
            .map_err(ApiError::internal)?
        {
            if authors::has_restriction(&state.db, &author.hash, "comment")
                .await
                .map_err(ApiError::internal)?
            {
                return Err(ApiError::forbidden("该作者身份已被限制评论"));
            }
        }
    }
    let value = json_body(body).await?;
    let body: CreateCommentBody =
        serde_json::from_value(value).map_err(|_| ApiError::bad("评论参数无效"))?;
    let theme = theme_info(&state, &theme_id)
        .await
        .ok_or_else(|| ApiError::not_found("主题不存在"))?;
    let role = theme_role(&state, &session, &theme).await;
    let content = validate_content(&body.content)?;
    let parent_id = body
        .parent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(parent) = &parent_id {
        let record = fetch_record(&state.db, parent)
            .await?
            .ok_or_else(|| ApiError::not_found("回复的评论不存在"))?;
        if record.theme_id != theme_id {
            return Err(ApiError::bad("回复的评论不属于该主题"));
        }
        if record.visibility == "author"
            && !(role.is_admin || role.is_author || record.user_id == session.user_id)
        {
            return Err(ApiError::forbidden("无权回复该私密留言"));
        }
    }
    let visibility = normalize_visibility(&body.visibility);
    let record = insert_record(
        &state.db,
        &theme,
        &session,
        content,
        visibility,
        parent_id,
        role.is_admin || role.is_author,
    )
    .await?;
    let author_ids = theme_author_user_ids(&state.db, &theme.author_hash).await;
    record_mentions(
        &state.db,
        &record.id,
        &record.content,
        &record.visibility,
        &session.user_id,
        &HashSet::new(),
    )
    .await;
    let value = comment_json(
        &record,
        Some(&theme),
        &session,
        role,
        &author_ids,
        &HashSet::new(),
    );
    Ok(Json(json!({"ok": true, "comment": value})))
}

#[derive(Deserialize, Default)]
struct DeleteBody {
    #[serde(default)]
    id: String,
}

pub async fn account_delete(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: DeleteBody = serde_json::from_value(value).map_err(|_| ApiError::bad("参数无效"))?;
    if body.id.trim().is_empty() {
        return Err(ApiError::bad("缺少评论 ID"));
    }
    let record = fetch_record(&state.db, &body.id)
        .await?
        .ok_or_else(|| ApiError::not_found("评论不存在"))?;
    let theme = theme_info(&state, &record.theme_id)
        .await
        .ok_or_else(|| ApiError::not_found("主题不存在"))?;
    let role = theme_role(&state, &session, &theme).await;
    if !(role.is_admin || role.is_author || record.user_id == session.user_id) {
        return Err(ApiError::forbidden("无权删除该评论"));
    }
    let deleted = delete_record(&state.db, &record.id).await?;
    Ok(Json(json!({
        "ok": true,
        "deleted": deleted,
        "themeId": record.theme_id,
    })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ClearBody {
    #[serde(default)]
    theme_id: Option<String>,
}

pub async fn account_clear(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: ClearBody = serde_json::from_value(value).unwrap_or_default();
    let theme_id = body
        .theme_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let deleted = if session.role == "admin" {
        match theme_id {
            Some(theme_id) => clear_theme(&state.db, theme_id).await?,
            None => clear_all(&state.db).await?,
        }
    } else if let Some(theme_id) = theme_id {
        let Some(theme) = theme_info(&state, theme_id).await else {
            return Err(ApiError::not_found("主题不存在"));
        };
        let role = theme_role(&state, &session, &theme).await;
        if role.is_author {
            clear_theme(&state.db, theme_id).await?
        } else {
            clear_user_messages(&state.db, &session.user_id, Some(theme_id)).await?
        }
    } else {
        let owned = owned_theme_ids(&state, &session).await;
        if owned.is_empty() {
            clear_user_messages(&state.db, &session.user_id, None).await?
        } else {
            clear_themes(&state.db, &owned).await?
        }
    };
    Ok(Json(json!({"ok": true, "deleted": deleted})))
}

pub async fn account_messages(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Query(query): Query<MessageQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500) as usize;
    let records = recent_records(&state.db).await?;
    let themes = theme_infos(&state).await;
    let is_admin = session.role == "admin";
    let owned = owned_theme_ids(&state, &session).await;

    // A regular user sees complete threads they participate in; an author
    // sees everything on their themes; an admin sees everything.
    let root_of: HashMap<String, String> = records
        .iter()
        .map(|record| {
            (
                record.id.clone(),
                record
                    .parent_id
                    .clone()
                    .unwrap_or_else(|| record.id.clone()),
            )
        })
        .collect();
    let mut participated: HashSet<String> = records
        .iter()
        .filter(|record| record.user_id == session.user_id)
        .filter_map(|record| root_of.get(&record.id).cloned())
        .collect();
    let mention_rows = mentioned_comment_rows(&state.db, &session.user_id, None).await;
    let mention_ids: HashSet<String> = mention_rows
        .iter()
        .map(|(comment_id, _, _)| comment_id.clone())
        .collect();
    let mentioned_themes: HashSet<String> = mention_rows
        .iter()
        .map(|(_, theme_id, _)| theme_id.clone())
        .collect();
    for (comment_id, _, parent_id) in &mention_rows {
        participated.insert(parent_id.clone().unwrap_or_else(|| comment_id.clone()));
    }

    let mut visible: Vec<CommentRecord> = Vec::new();
    for record in records {
        let root = root_of.get(&record.id).cloned().unwrap_or_default();
        if is_admin || owned.contains(&record.theme_id) || participated.contains(&root) {
            visible.push(record);
        }
    }

    let mut by_theme: HashMap<String, Vec<CommentRecord>> = HashMap::new();
    for record in visible {
        by_theme
            .entry(record.theme_id.clone())
            .or_default()
            .push(record);
    }

    let mut conversations: Vec<Value> = Vec::with_capacity(by_theme.len());
    for (theme_id, records) in by_theme {
        let theme = themes.get(&theme_id).cloned().unwrap_or(ThemeInfo {
            id: theme_id.clone(),
            ..Default::default()
        });
        let role = if is_admin {
            ThemeRole {
                is_admin: true,
                is_author: true,
            }
        } else {
            ThemeRole {
                is_admin: false,
                is_author: owned.contains(&theme_id),
            }
        };
        let author_ids = theme_author_user_ids(&state.db, &theme.author_hash).await;
        let mut threads = build_threads(records, &session, role);
        threads.sort_by(|left, right| right.comment.created_at.cmp(&left.comment.created_at));
        let last_at = threads
            .iter()
            .flat_map(|thread| {
                std::iter::once(thread.comment.created_at.clone())
                    .chain(thread.replies.iter().map(|reply| reply.created_at.clone()))
            })
            .max()
            .unwrap_or_default();
        conversations.push(json!({
            "themeId": theme.id,
            "themeName": theme.name,
            "themeAuthor": theme.author,
            "isThemeAuthor": role.is_author,
            "mentioned": mentioned_themes.contains(&theme_id),
            "lastAt": last_at,
            "comments": threads_json(
                threads,
                Some(&theme),
                &session,
                role,
                &author_ids,
                &mention_ids,
            ),
        }));
    }
    conversations.sort_by(|left, right| {
        let left = left.get("lastAt").and_then(Value::as_str).unwrap_or("");
        let right = right.get("lastAt").and_then(Value::as_str).unwrap_or("");
        right.cmp(left)
    });
    conversations.truncate(limit);
    let retention = retention_days(&state.db).await;
    Ok(Json(json!({
        "ok": true,
        "conversations": conversations,
        "retentionDays": retention,
        "isAdmin": is_admin,
    })))
}

// ── Admin API handlers ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AdminListQuery {
    theme: Option<String>,
    q: Option<String>,
    user: Option<String>,
    visibility: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub async fn admin_list(
    State(state): State<AppState>,
    Query(query): Query<AdminListQuery>,
) -> ApiResult<Json<Value>> {
    let theme_id = query
        .theme
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let needle = query
        .q
        .as_deref()
        .map(str::trim)
        .map(str::to_lowercase)
        .filter(|value| !value.is_empty());
    let visibility = query
        .visibility
        .as_deref()
        .map(str::trim)
        .filter(|value| matches!(*value, "public" | "author"));
    let user_needle = query
        .user
        .as_deref()
        .map(str::trim)
        .map(str::to_lowercase)
        .filter(|value| !value.is_empty());
    let records = match theme_id {
        Some(theme_id) => theme_records(&state.db, theme_id, 4000).await?,
        None => recent_records(&state.db).await?,
    };
    let themes = theme_infos(&state).await;
    let viewer = SessionInfo {
        user_id: String::new(),
        username: String::new(),
        display_name: String::new(),
        role: "admin".to_string(),
        status: "active".to_string(),
        email_verified: true,
        scope: "admin".to_string(),
    };
    let admin_role = ThemeRole {
        is_admin: true,
        is_author: true,
    };

    let mut filtered: Vec<CommentRecord> = records
        .into_iter()
        .filter(|record| {
            if let Some(visibility) = visibility {
                if record.visibility != visibility {
                    return false;
                }
            }
            if let Some(user) = &user_needle {
                if !record.username.to_lowercase().contains(user)
                    && !record.display_name.to_lowercase().contains(user)
                    && !record.user_id.to_lowercase().contains(user)
                {
                    return false;
                }
            }
            if let Some(needle) = &needle {
                let theme = themes.get(&record.theme_id);
                let haystack = format!(
                    "{} {} {} {} {} {}",
                    record.content.to_lowercase(),
                    record.username.to_lowercase(),
                    record.display_name.to_lowercase(),
                    record.theme_id.to_lowercase(),
                    theme
                        .map(|value| value.name.to_lowercase())
                        .unwrap_or_default(),
                    theme
                        .map(|value| value.author.to_lowercase())
                        .unwrap_or_default(),
                );
                if !haystack.contains(needle) {
                    return false;
                }
            }
            true
        })
        .collect();
    filtered.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    let total = filtered.len();
    let offset = query.offset.unwrap_or(0).min(total);
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let page: Vec<Value> = filtered
        .iter()
        .skip(offset)
        .take(limit)
        .map(|record| {
            comment_json(
                record,
                themes.get(&record.theme_id),
                &viewer,
                admin_role,
                &HashSet::new(),
                &HashSet::new(),
            )
        })
        .collect();

    // The theme dialog needs thread context, so also expose grouped threads
    // when the caller filters to a single theme.
    let threads = match theme_id {
        Some(theme_id) => {
            let records = theme_records(&state.db, theme_id, 4000).await?;
            let theme = themes.get(theme_id);
            let author_ids = theme
                .map(|theme| theme.author_hash.clone())
                .unwrap_or_default();
            let author_ids = theme_author_user_ids(&state.db, &author_ids).await;
            threads_json(
                build_threads(records, &viewer, admin_role),
                theme,
                &viewer,
                admin_role,
                &author_ids,
                &HashSet::new(),
            )
        }
        None => Vec::new(),
    };
    let retention = retention_days(&state.db).await;
    Ok(Json(json!({
        "ok": true,
        "comments": page,
        "threads": threads,
        "total": total,
        "offset": offset,
        "limit": limit,
        "retentionDays": retention,
    })))
}

pub async fn admin_delete(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: DeleteBody = serde_json::from_value(value).map_err(|_| ApiError::bad("参数无效"))?;
    if body.id.trim().is_empty() {
        return Err(ApiError::bad("缺少评论 ID"));
    }
    let deleted = delete_record(&state.db, &body.id).await?;
    if deleted == 0 {
        return Err(ApiError::not_found("评论不存在"));
    }
    let _ = accounts::audit(
        &state.db,
        Some(&session.user_id),
        "comment.delete",
        Some("comment"),
        Some(&body.id),
        None,
    )
    .await;
    Ok(Json(json!({"ok": true, "deleted": deleted})))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AdminClearBody {
    #[serde(default)]
    theme_id: Option<String>,
}

pub async fn admin_clear(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: AdminClearBody = serde_json::from_value(value).unwrap_or_default();
    let theme_id = body
        .theme_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let deleted = match theme_id {
        Some(theme_id) => clear_theme(&state.db, theme_id).await?,
        None => clear_all(&state.db).await?,
    };
    let detail = theme_id.map(|value| format!("theme={value}"));
    let _ = accounts::audit(
        &state.db,
        Some(&session.user_id),
        "comment.clear",
        theme_id.map(|_| "theme").or(Some("all")),
        theme_id,
        detail.as_deref(),
    )
    .await;
    Ok(Json(json!({"ok": true, "deleted": deleted})))
}

pub async fn admin_settings_get(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let days = retention_days(&state.db).await;
    Ok(Json(json!({
        "ok": true,
        "retentionDays": days,
        "defaultRetentionDays": DEFAULT_RETENTION_DAYS,
        "maxRetentionDays": MAX_RETENTION_DAYS,
    })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SettingsBody {
    #[serde(default)]
    retention_days: i64,
}

pub async fn admin_settings_save(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: SettingsBody =
        serde_json::from_value(value).map_err(|_| ApiError::bad("参数无效"))?;
    if body.retention_days < 0 {
        return Err(ApiError::bad("保留天数不能为负数"));
    }
    let days = save_retention_days(&state.db, body.retention_days).await?;
    let detail = format!("retentionDays={days}");
    let _ = accounts::audit(
        &state.db,
        Some(&session.user_id),
        "comment.settings",
        Some("comment"),
        None,
        Some(&detail),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "retentionDays": days,
        "defaultRetentionDays": DEFAULT_RETENTION_DAYS,
        "maxRetentionDays": MAX_RETENTION_DAYS,
    })))
}

/// Public avatar endpoint used by comments and the @mention picker. It accepts a user id or
/// username and only serves the stored avatar asset, never other profile fields.
pub async fn user_avatar(
    State(state): State<AppState>,
    Path(identifier): Path<String>,
) -> ApiResult<Response> {
    if identifier.is_empty()
        || identifier.len() > 64
        || !identifier
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(ApiError::bad("无效的用户标识"));
    }
    // Comments pass the user id; the mention picker only knows the public username.
    let user = match accounts::user_by_id(&state.db, &identifier)
        .await
        .map_err(ApiError::internal)?
    {
        Some(user) => Some(user),
        None => accounts::user_by_login(&state.db, &identifier)
            .await
            .map_err(ApiError::internal)?,
    }
    .ok_or_else(|| ApiError::not_found("头像不存在"))?;
    let key = user
        .avatar
        .filter(|value| value.starts_with("avatars/users/"))
        .ok_or_else(|| ApiError::not_found("头像不存在"))?;
    let object = state
        .store
        .serve(&key, None)
        .await
        .map_err(|_| ApiError::not_found("头像不存在"))?;
    crate::api::serve_object_response(
        object,
        "public, max-age=300",
        state.download_buffer_bytes(),
    )
}

#[derive(Deserialize)]
pub struct UserSearchQuery {
    q: Option<String>,
}

/// Lightweight account search for the `@mention` picker. Only public profile
/// fields are returned; email, status and identifiers stay private.
pub async fn account_user_search(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Query(query): Query<UserSearchQuery>,
) -> ApiResult<Json<Value>> {
    let text = query.q.as_deref().unwrap_or("").trim();
    if text.is_empty() {
        return Ok(Json(json!({"ok": true, "users": []})));
    }
    if !crate::account_api::rate_allowed(
        &state,
        &format!("user-search:{}", session.user_id),
        60,
        60,
    ) {
        return Err(ApiError::too_many("搜索过于频繁，请稍后再试"));
    }
    let escaped = text
        .chars()
        .take(40)
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%{escaped}%");
    let users = state
        .db
        .call(move |conn| {
            let mut statement = conn.prepare(
                "SELECT u.username, u.display_name, u.role,
                        EXISTS(
                            SELECT 1 FROM authors.authors a
                            WHERE a.user_id = u.id AND a.status = 'active'
                        ) AS is_author,
                        u.avatar
                 FROM users u
                 WHERE u.status = 'active'
                   AND (u.username LIKE ?1 ESCAPE '\\'
                        OR u.display_name LIKE ?1 ESCAPE '\\')
                 ORDER BY u.username COLLATE NOCASE
                 LIMIT 10",
            )?;
            let rows = statement.query_map(params![pattern], |row| {
                let username: String = row.get(0)?;
                let display_name: String = row.get(1)?;
                let role: String = row.get(2)?;
                let is_author: i64 = row.get(3)?;
                let avatar_url = row
                    .get::<_, Option<String>>(4)?
                    .filter(|value| value.starts_with("avatars/users/"))
                    .map(|_| format!("/account/api/users/{username}/avatar"))
                    .unwrap_or_default();
                Ok(json!({
                    "username": username,
                    "displayName": display_name,
                    "role": role,
                    "isAuthor": is_author != 0,
                    "avatarUrl": avatar_url,
                }))
            })?;
            let mut items = Vec::new();
            for row in rows {
                items.push(row?);
            }
            Ok(items)
        })
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "users": users})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, username: &str, role: &str) -> SessionInfo {
        SessionInfo {
            user_id: id.to_string(),
            username: username.to_string(),
            display_name: username.to_string(),
            role: role.to_string(),
            status: "active".to_string(),
            email_verified: true,
            scope: "user".to_string(),
        }
    }

    fn theme(id: &str) -> ThemeInfo {
        ThemeInfo {
            id: id.to_string(),
            name: "Test theme".to_string(),
            author: "alice".to_string(),
            author_hash: "hash".to_string(),
        }
    }

    async fn seed_user(db: &Database, id: &str, username: &str) {
        let id = id.to_string();
        let username = username.to_string();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO users(id, username, display_name, role, status, email_verified,
                                   created_at, updated_at)
                 VALUES (?1, ?2, ?2, 'user', 'active', 1, '2026-01-01T00:00:00Z',
                         '2026-01-01T00:00:00Z')",
                params![id, username],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    #[test]
    fn extracts_local_mentions_without_platform_or_email_false_positives() {
        assert_eq!(
            extract_local_mentions("hi @alice @bob@github @github foo@example.com @alice"),
            vec!["alice".to_string(), "bob".to_string()]
        );
        assert_eq!(
            extract_local_mentions("早上好 @樱檩殇雪，看看这个"),
            vec!["樱檩殇雪".to_string()]
        );
        assert!(extract_local_mentions("mail me at a@b.com").is_empty());
        assert_eq!(
            extract_local_mentions("`@carol` https://x.com/@carol @carol\n```\n@carol\n```"),
            vec!["carol".to_string()]
        );
        assert_eq!(
            extract_local_mentions("你好，@carol"),
            vec!["carol".to_string()]
        );
    }

    #[tokio::test]
    async fn public_mentions_are_recorded_and_private_mentions_are_not() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open_test(directory.path())
            .await
            .unwrap();
        seed_user(&db, "u1", "alice").await;
        seed_user(&db, "u2", "bob").await;
        seed_user(&db, "u3", "carol").await;
        let theme = theme("11010-01");

        let public = insert_record(
            &db,
            &theme,
            &session("u2", "bob", "user"),
            "hello @carol".to_string(),
            "public",
            None,
            false,
        )
        .await
        .unwrap();
        record_mentions(
            &db,
            &public.id,
            &public.content,
            "public",
            "u2",
            &HashSet::new(),
        )
        .await;
        assert_eq!(mentioned_comment_rows(&db, "u3", None).await.len(), 1);

        let private = insert_record(
            &db,
            &theme,
            &session("u2", "bob", "user"),
            "secret @carol".to_string(),
            "author",
            None,
            false,
        )
        .await
        .unwrap();
        record_mentions(
            &db,
            &private.id,
            &private.content,
            "author",
            "u2",
            &HashSet::new(),
        )
        .await;
        assert_eq!(mentioned_comment_rows(&db, "u3", None).await.len(), 1);
    }

    #[test]
    fn rejects_empty_and_control_comments_but_keeps_emoji() {
        assert!(validate_content("   ").is_err());
        assert!(validate_content("hello\u{0}world").is_err());
        assert_eq!(validate_content(" 你好 🎨👨‍👩‍👧  ").unwrap(), "你好 🎨👨‍👩‍👧");
        assert!(validate_content(&"a".repeat(MAX_COMMENT_CHARS + 1)).is_err());
    }

    #[tokio::test]
    async fn private_threads_are_hidden_from_other_users() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open_test(directory.path())
            .await
            .unwrap();
        seed_user(&db, "u1", "alice").await;
        seed_user(&db, "u2", "bob").await;
        seed_user(&db, "u3", "carol").await;
        let theme = theme("11010-01");

        insert_record(
            &db,
            &theme,
            &session("u1", "alice", "user"),
            "public hello".to_string(),
            "public",
            None,
            false,
        )
        .await
        .unwrap();
        insert_record(
            &db,
            &theme,
            &session("u2", "bob", "user"),
            "secret 🎨".to_string(),
            "author",
            None,
            false,
        )
        .await
        .unwrap();

        let records = theme_records(&db, "11010-01", 100).await.unwrap();
        let viewer = session("u3", "carol", "user");
        let visible = build_threads(records, &viewer, ThemeRole::default());
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].comment.content, "public hello");

        let records = theme_records(&db, "11010-01", 100).await.unwrap();
        let author = ThemeRole {
            is_admin: false,
            is_author: true,
        };
        assert_eq!(build_threads(records, &viewer, author).len(), 2);
    }

    #[tokio::test]
    async fn replies_inherit_visibility_and_flatten_to_root() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open_test(directory.path())
            .await
            .unwrap();
        seed_user(&db, "u1", "alice").await;
        seed_user(&db, "u2", "bob").await;
        seed_user(&db, "u3", "carol").await;
        let theme = theme("11010-01");

        let root = insert_record(
            &db,
            &theme,
            &session("u2", "bob", "user"),
            "secret".to_string(),
            "author",
            None,
            false,
        )
        .await
        .unwrap();
        let reply = insert_record(
            &db,
            &theme,
            &session("u1", "alice", "user"),
            "reply".to_string(),
            "public",
            Some(root.id.clone()),
            true,
        )
        .await
        .unwrap();
        assert_eq!(reply.visibility, "author");
        assert_eq!(reply.parent_id.as_deref(), Some(root.id.as_str()));

        // Replying to a reply attaches to the root thread.
        let nested = insert_record(
            &db,
            &theme,
            &session("u2", "bob", "user"),
            "thanks".to_string(),
            "public",
            Some(reply.id.clone()),
            false,
        )
        .await
        .unwrap();
        assert_eq!(nested.parent_id.as_deref(), Some(root.id.as_str()));

        // Carol cannot reply into Bob's private thread.
        let denied = insert_record(
            &db,
            &theme,
            &session("u3", "carol", "user"),
            "intrude".to_string(),
            "public",
            Some(root.id.clone()),
            false,
        )
        .await;
        assert!(denied.is_err());
    }

    #[tokio::test]
    async fn retention_purges_old_comments() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open_test(directory.path())
            .await
            .unwrap();
        seed_user(&db, "u1", "alice").await;
        db.call(|conn| {
            conn.execute(
                "INSERT INTO social.theme_comments
                 (id, theme_id, user_id, parent_id, visibility, content, created_at, updated_at)
                 VALUES ('old', 't', 'u1', NULL, 'public', 'old', '2000-01-01T00:00:00Z',
                         '2000-01-01T00:00:00Z')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        save_retention_days(&db, 30).await.unwrap();
        let deleted = purge_expired(&db).await.unwrap();
        assert_eq!(deleted, 1);
        assert!(theme_records(&db, "t", 10).await.unwrap().is_empty());
    }
}
