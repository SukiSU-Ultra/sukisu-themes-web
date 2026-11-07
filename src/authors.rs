use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::catalog;
use crate::config::Paths;
use crate::db::{self, Database};
use crate::models::{AuthorsFile, Meta};
use crate::util::{author_fingerprint, author_key, now_iso, random_hex, valid_author_id};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    pub hash: String,
    pub fingerprint: String,
    pub display_name: Option<String>,
    pub user_id: Option<String>,
    pub status: String,
    pub auto_publish: bool,
    pub code: Option<String>,
    pub next_theme_seq: i64,
    pub approved_at: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub auto_revoke_at: Option<String>,
    pub revoke_reason: Option<String>,
    pub restriction_reason: Option<String>,
}

fn author_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Author> {
    Ok(Author {
        hash: row.get(0)?,
        fingerprint: row.get(1)?,
        display_name: row.get(2)?,
        user_id: row.get(3)?,
        status: row.get(4)?,
        auto_publish: row.get::<_, i64>(8)? != 0,
        code: row.get(9)?,
        next_theme_seq: row.get(10)?,
        approved_at: row.get(5)?,
        last_used_at: row.get(6)?,
        created_at: row.get(7)?,
        auto_revoke_at: row.get(11)?,
        revoke_reason: row.get(12)?,
        restriction_reason: row.get(13)?,
    })
}

const AUTHOR_COLUMNS: &str = "hash, fingerprint, display_name, user_id, status, approved_at, last_used_at, created_at, auto_publish, code, next_theme_seq, auto_revoke_at, revoke_reason, restriction_reason";

pub async fn author_by_hash(db: &Database, hash: &str) -> Result<Option<Author>> {
    let hash = hash.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {AUTHOR_COLUMNS} FROM authors.authors WHERE hash = ?1"
        ))?;
        let mut rows = statement.query([hash])?;
        Ok(rows.next()?.map(author_from_row).transpose()?)
    })
    .await
}

pub async fn authors_for_user(db: &Database, user_id: &str) -> Result<Vec<Author>> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {AUTHOR_COLUMNS} FROM authors.authors WHERE user_id = ?1
             ORDER BY CASE status WHEN 'active' THEN 0 WHEN 'pending' THEN 1 ELSE 2 END, created_at DESC"
        ))?;
        let mut rows = statement.query([user_id])?;
        let mut authors = Vec::new();
        while let Some(row) = rows.next()? {
            authors.push(author_from_row(row)?);
        }
        Ok(authors)
    })
    .await
}

/// Every author hash ever tied to the account, including licenses that were
/// revoked or deleted. Theme ownership uses this so published work stays with
/// the account after a license is taken away.
pub async fn author_hashes_for_user(db: &Database, user_id: &str) -> Result<Vec<String>> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT hash FROM authors.authors WHERE user_id = ?1
             UNION
             SELECT author_hash FROM authors.author_owners WHERE user_id = ?1",
        )?;
        let rows = statement.query_map([user_id], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

/// Fills the ownership table from current license rows. Runs at startup so
/// authors created before the table existed keep their theme list.
pub async fn backfill_author_owners(db: &Database) -> Result<usize> {
    db.call(|conn| {
        Ok(conn.execute(
            "INSERT INTO authors.author_owners(author_hash, user_id, display_name, updated_at)
             SELECT hash, user_id, display_name, updated_at FROM authors.authors
             WHERE user_id IS NOT NULL AND TRIM(user_id) <> ''
             ON CONFLICT(author_hash) DO UPDATE SET
                 user_id = excluded.user_id,
                 display_name = COALESCE(excluded.display_name, author_owners.display_name),
                 updated_at = excluded.updated_at",
            [],
        )?)
    })
    .await
}

/// Recovers ownership for licenses that were already deleted: a theme that
/// still exists and was submitted by an account belongs to that account.
pub async fn backfill_author_owners_from_submissions(
    db: &Database,
    index: &catalog::MetaIndex,
) -> Result<usize> {
    let rows: Vec<(String, String)> = db
        .call(|conn| {
            let mut statement = conn.prepare(
                "SELECT DISTINCT theme_id, user_id FROM authors.submissions
                 WHERE user_id IS NOT NULL AND TRIM(user_id) <> ''",
            )?;
            let mut rows = statement.query([])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
            }
            Ok(items)
        })
        .await?;
    let mut owners: HashMap<String, (String, Option<String>)> = HashMap::new();
    for (theme_id, user_id) in rows {
        let Some(meta) = index.theme_by_id(&theme_id) else {
            continue;
        };
        let Some(hash) = meta
            .author_hash
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        owners
            .entry(hash.to_string())
            .or_insert((user_id, Some(meta.author.clone())));
    }
    if owners.is_empty() {
        return Ok(0);
    }
    let now = now_iso();
    db.call(move |conn| {
        let transaction = conn.transaction()?;
        let mut inserted = 0usize;
        for (hash, (user_id, name)) in owners {
            inserted += transaction.execute(
                "INSERT OR IGNORE INTO authors.author_owners
                    (author_hash, user_id, display_name, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![hash, user_id, name, now],
            )?;
        }
        transaction.commit()?;
        Ok(inserted)
    })
    .await
}

pub async fn active_author_for_user(db: &Database, user_id: &str) -> Result<Option<Author>> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {AUTHOR_COLUMNS} FROM authors.authors
             WHERE user_id = ?1 AND status = 'active'
             ORDER BY approved_at DESC, created_at DESC LIMIT 1"
        ))?;
        let mut rows = statement.query([user_id])?;
        Ok(rows.next()?.map(author_from_row).transpose()?)
    })
    .await
}

pub async fn create_author(
    db: &Database,
    hash: &str,
    fingerprint: &str,
    display_name: &str,
    user_id: Option<&str>,
    status: &str,
) -> Result<Author> {
    let hash = hash.to_string();
    let insert_hash = hash.clone();
    let fingerprint = fingerprint.to_string();
    let display_name = display_name.trim().to_string();
    let user_id = user_id.map(str::to_string);
    let status = status.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO authors.authors(hash, fingerprint, display_name, user_id, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(hash) DO UPDATE SET
                 display_name = COALESCE(NULLIF(excluded.display_name, ''), authors.display_name),
                 user_id = COALESCE(authors.user_id, excluded.user_id),
                 updated_at = excluded.updated_at",
            params![insert_hash, fingerprint, display_name, user_id, status, now],
        )?;
        if let Some(owner) = user_id.as_deref() {
            conn.execute(
                "INSERT INTO authors.author_owners(author_hash, user_id, display_name, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(author_hash) DO UPDATE SET
                     user_id = excluded.user_id,
                     display_name = COALESCE(excluded.display_name, author_owners.display_name),
                     updated_at = excluded.updated_at",
                params![insert_hash, owner, display_name, now],
            )?;
        }
        Ok(())
    })
    .await?;
    let _ = ensure_author_code(db, &hash).await;
    author_by_hash(db, &hash)
        .await?
        .ok_or_else(|| anyhow::anyhow!("作者写入失败"))
}

pub async fn set_author_status(
    db: &Database,
    hash: &str,
    status: &str,
    actor: Option<&str>,
) -> Result<()> {
    set_author_status_with_reason(db, hash, status, actor, None).await
}

pub async fn set_author_status_with_reason(
    db: &Database,
    hash: &str,
    status: &str,
    actor: Option<&str>,
    reason: Option<&str>,
) -> Result<()> {
    let hash = hash.to_string();
    let status = status.to_string();
    let actor = actor.map(str::to_string);
    let reason = reason.map(str::to_string);
    let now = now_iso();
    db.call(move |conn| {
        let approved_at = if status == "active" {
            Some(now.clone())
        } else {
            None
        };
        conn.execute(
            "UPDATE authors.authors
             SET status = ?2, approved_at = COALESCE(?3, approved_at),
                 approved_by = COALESCE(?4, approved_by), updated_at = ?5,
                 auto_revoke_at = CASE WHEN ?2 = 'active' THEN auto_revoke_at ELSE NULL END,
                 revoke_reason = CASE
                     WHEN ?2 IN ('revoked', 'retired') THEN COALESCE(?6, revoke_reason)
                     ELSE NULL
                 END
             WHERE hash = ?1",
            params![hash, status, approved_at, actor, now, reason],
        )?;
        if status == "active" {
            conn.execute(
                "INSERT INTO authors.author_owners(author_hash, user_id, display_name, updated_at)
                 SELECT hash, user_id, display_name, ?2 FROM authors.authors
                 WHERE hash = ?1 AND user_id IS NOT NULL AND TRIM(user_id) <> ''
                 ON CONFLICT(author_hash) DO UPDATE SET
                     user_id = excluded.user_id,
                     display_name = COALESCE(excluded.display_name, author_owners.display_name),
                     updated_at = excluded.updated_at",
                params![hash, now],
            )?;
        }
        Ok(())
    })
    .await
}

pub async fn delete_author(db: &Database, hash: &str) -> Result<usize> {
    let hash = hash.to_string();
    db.call(move |conn| {
        // Follow relationships, comments and favorites belong to the account
        // and to the theme, not to the license record. Keep them when the
        // license is removed so they survive a later re-licensing.
        conn.execute(
            "DELETE FROM authors.author_restrictions WHERE author_hash = ?1",
            [&hash],
        )?;
        Ok(conn.execute("DELETE FROM authors.authors WHERE hash = ?1", [&hash])?)
    })
    .await
}

pub async fn delete_authors_for_user(db: &Database, user_id: &str) -> Result<usize> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let mut statement =
            conn.prepare("SELECT hash FROM authors.authors WHERE user_id = ?1")?;
        let hashes = statement
            .query_map([&user_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for hash in &hashes {
            conn.execute(
                "DELETE FROM social.user_follows WHERE author_hash = ?1",
                [hash],
            )?;
            conn.execute(
                "DELETE FROM authors.author_restrictions WHERE author_hash = ?1",
                [hash],
            )?;
        }
        conn.execute(
            "DELETE FROM social.user_follows WHERE follower_id = ?1",
            [&user_id],
        )?;
        conn.execute(
            "DELETE FROM social.theme_comment_mentions WHERE user_id = ?1",
            [&user_id],
        )?;
        conn.execute(
            "DELETE FROM social.theme_comments WHERE user_id = ?1",
            [&user_id],
        )?;
        conn.execute(
            "DELETE FROM authors.author_requests WHERE user_id = ?1",
            [&user_id],
        )?;
        conn.execute(
            "DELETE FROM authors.author_reset_history WHERE user_id = ?1",
            [&user_id],
        )?;
        conn.execute(
            "DELETE FROM authors.submissions WHERE user_id = ?1",
            [&user_id],
        )?;
        conn.execute(
            "DELETE FROM authors.author_owners WHERE user_id = ?1",
            [&user_id],
        )?;
        Ok(conn.execute(
            "DELETE FROM authors.authors WHERE user_id = ?1",
            [&user_id],
        )?)
    })
    .await
}

pub async fn bind_author_to_user(
    db: &Database,
    hash: &str,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<()> {
    let hash = hash.to_string();
    let user_id = user_id.to_string();
    let display_name = display_name.map(str::to_string);
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors.authors
             SET user_id = ?2,
                 display_name = COALESCE(?3, display_name),
                 updated_at = ?4
             WHERE hash = ?1",
            params![hash, user_id, display_name, now],
        )?;
        conn.execute(
            "INSERT INTO authors.author_owners(author_hash, user_id, display_name, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(author_hash) DO UPDATE SET
                 user_id = excluded.user_id,
                 display_name = COALESCE(excluded.display_name, author_owners.display_name),
                 updated_at = excluded.updated_at",
            params![hash, user_id, display_name, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn ensure_author_code(db: &Database, hash: &str) -> Result<(String, i64)> {
    let hash = hash.to_string();
    db.call(move |conn| {
        let transaction = conn.transaction()?;
        let row = transaction
            .query_row(
                "SELECT code, next_theme_seq FROM authors.authors WHERE hash = ?1",
                [&hash],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((code, seq)) = row else {
            return Err(tokio_rusqlite::Error::Rusqlite(rusqlite::Error::QueryReturnedNoRows));
        };
        let code = match code.filter(|value| !value.trim().is_empty()) {
            Some(code) => code,
            None => {
                let max: i64 = transaction.query_row(
                    "SELECT COALESCE(MAX(CAST(code AS INTEGER)), 11009) FROM authors.authors WHERE code GLOB '[0-9]*'",
                    [],
                    |row| row.get(0),
                )?;
                let code = (max + 1).max(11010).to_string();
                transaction.execute(
                    "UPDATE authors.authors SET code = ?2, updated_at = ?3 WHERE hash = ?1",
                    params![hash, code, now_iso()],
                )?;
                code
            }
        };
        transaction.commit()?;
        Ok((code, seq))
    })
    .await
}

pub async fn peek_next_theme_id(db: &Database, hash: &str) -> Result<Option<String>> {
    let (code, seq) = ensure_author_code(db, hash).await?;
    Ok(Some(format!("{code}-{seq:02}")))
}

pub async fn allocate_theme_id(db: &Database, hash: &str) -> Result<String> {
    let hash = hash.to_string();
    let (code, seq) = db
        .call(move |conn| {
            let transaction = conn.transaction()?;
            let row = transaction
                .query_row(
                    "SELECT code, next_theme_seq FROM authors.authors WHERE hash = ?1",
                    [&hash],
                    |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            let Some((code, seq)) = row else {
                return Err(tokio_rusqlite::Error::Rusqlite(
                    rusqlite::Error::QueryReturnedNoRows,
                ));
            };
            let code = match code.filter(|value| !value.trim().is_empty()) {
                Some(code) => code,
                None => {
                    let max: i64 = transaction.query_row(
                        "SELECT COALESCE(MAX(CAST(code AS INTEGER)), 11009) FROM authors.authors WHERE code GLOB '[0-9]*'",
                        [],
                        |row| row.get(0),
                    )?;
                    (max + 1).max(11010).to_string()
                }
            };
            transaction.execute(
                "UPDATE authors.authors SET code = ?2, next_theme_seq = ?3, updated_at = ?4 WHERE hash = ?1",
                params![hash, &code, seq + 1, now_iso()],
            )?;
            transaction.commit()?;
            Ok((code, seq))
        })
        .await?;
    Ok(format!("{code}-{seq:02}"))
}

pub async fn allocate_unique_theme_id(db: &Database, paths: &Paths, hash: &str) -> Result<String> {
    for _ in 0..100 {
        let id = allocate_theme_id(db, hash).await?;
        if crate::catalog::find_theme_meta(paths, &id).is_none()
            && !crate::catalog::pending_submissions(paths)
                .iter()
                .any(|item| item.get("id").and_then(|value| value.as_str()) == Some(id.as_str()))
        {
            return Ok(id);
        }
    }
    anyhow::bail!("无法分配不冲突的主题 ID")
}

pub async fn set_auto_publish(db: &Database, hash: &str, enabled: bool) -> Result<()> {
    let hash = hash.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors.authors SET auto_publish = ?2, updated_at = ?3 WHERE hash = ?1",
            params![hash, if enabled { 1 } else { 0 }, now],
        )?;
        Ok(())
    })
    .await
}

pub const AUTHOR_GRACE_DAYS: i64 = 10;

pub fn author_grace_deadline(days: i64) -> String {
    (Utc::now() + ChronoDuration::days(days)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub async fn set_auto_revoke_at(db: &Database, hash: &str, at: Option<&str>) -> Result<()> {
    let hash = hash.to_string();
    let at = at.map(str::to_string);
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors.authors SET auto_revoke_at = ?2, updated_at = ?3 WHERE hash = ?1",
            params![hash, at, now],
        )?;
        Ok(())
    })
    .await
}

/// Hashes that a single account may no longer manage: retired licenses left
/// behind by a reset, plus any license still tied to a pending author request.
pub async fn locked_author_hashes(db: &Database, user_id: &str) -> Result<HashSet<String>> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let mut locked = HashSet::new();
        let mut statement = conn.prepare(
            "SELECT old_hash FROM authors.author_reset_history WHERE user_id = ?1
             UNION
             SELECT author_hash FROM authors.author_requests
             WHERE user_id = ?1 AND status = 'pending' AND author_hash IS NOT NULL",
        )?;
        let mut rows = statement.query([user_id])?;
        while let Some(row) = rows.next()? {
            locked.insert(row.get::<_, String>(0)?);
        }
        Ok(locked)
    })
    .await
}

/// Start (or cancel) the first-license grace period based on whether the
/// license already has a published theme.
pub async fn schedule_grace_if_needed(db: &Database, paths: &Paths, hash: &str) -> Result<()> {
    if has_published_theme(paths, hash) {
        set_auto_revoke_at(db, hash, None).await
    } else {
        let deadline = author_grace_deadline(AUTHOR_GRACE_DAYS);
        set_auto_revoke_at(db, hash, Some(&deadline)).await
    }
}

/// A theme was just published under this license, so the first-license grace
/// period no longer applies. Clear the deadline immediately instead of waiting
/// for the hourly cleanup; otherwise the author page keeps showing a bogus
/// "auto revoke at" date for up to 10 days.
pub async fn clear_auto_revoke_for_published(db: &Database, hash: &str) -> Result<()> {
    if hash.trim().is_empty() {
        return Ok(());
    }
    set_auto_revoke_at(db, hash, None).await
}

/// Any published theme under this license counts as an approved work.
pub fn has_published_theme(paths: &Paths, hash: &str) -> bool {
    let Ok(entries) = fs::read_dir(&paths.meta) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(meta) = catalog::read_meta_file(&path) else {
            continue;
        };
        if meta.author_hash.as_deref() == Some(hash) {
            return true;
        }
    }
    false
}

/// Revoke active licenses whose 10-day grace period expired without an
/// approved theme. Authors with any published theme keep their license, and any
/// not-yet-expired deadline is cleared as soon as a theme exists.
pub async fn revoke_inactive_authors(db: &Database, paths: &Paths) -> Result<usize> {
    let candidates: Vec<(String, Option<String>, Option<String>)> = db
        .call(|conn| {
            let mut statement = conn.prepare(
                "SELECT hash, display_name, auto_revoke_at FROM authors.authors
                 WHERE status = 'active' AND auto_revoke_at IS NOT NULL
                 ORDER BY auto_revoke_at ASC
                 LIMIT 500",
            )?;
            let mut rows = statement.query([])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push((row.get(0)?, row.get(1)?, row.get(2)?));
            }
            Ok(items)
        })
        .await?;
    let now = now_iso();
    let mut revoked = 0;
    for (hash, name, deadline) in candidates {
        if has_published_theme(paths, &hash) {
            let _ = set_auto_revoke_at(db, &hash, None).await;
            continue;
        }
        if deadline
            .as_deref()
            .map(|value| value > now.as_str())
            .unwrap_or(false)
        {
            continue;
        }
        let reason = "长期未发布主题，系统自动撤销";
        set_author_status_with_reason(db, &hash, "revoked", Some("system"), Some(reason)).await?;
        let _ = set_auto_publish(db, &hash, false).await;
        let detail = match name {
            Some(name) => format!("{name}; {reason}"),
            None => reason.to_string(),
        };
        let _ = crate::accounts::audit(
            db,
            Some("system"),
            "author.auto_revoke",
            Some("author"),
            Some(&hash),
            Some(&detail),
        )
        .await;
        revoked += 1;
    }
    Ok(revoked)
}

pub async fn touch_author(db: &Database, hash: &str) -> Result<()> {
    let hash = hash.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors.authors SET last_used_at = ?2, updated_at = ?2 WHERE hash = ?1",
            params![hash, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn update_author_name(db: &Database, hash: &str, name: &str) -> Result<()> {
    let hash = hash.to_string();
    let name = name.trim().to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors.authors SET display_name = ?2, updated_at = ?3 WHERE hash = ?1",
            params![hash, name, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn list_authors(db: &Database) -> Result<Vec<serde_json::Value>> {
    db.call(|conn| {
        let mut statement = conn.prepare(
            "SELECT a.hash, a.fingerprint, a.display_name, a.user_id, a.status,
                    a.approved_at, a.last_used_at, u.username, u.display_name,
                    a.code, a.next_theme_seq, a.auto_revoke_at, u.role, u.is_super_admin,
                    a.revoke_reason, a.restriction_reason
             FROM authors.authors a LEFT JOIN users u ON u.id = a.user_id
             ORDER BY CASE a.status
                 WHEN 'pending' THEN 0 WHEN 'active' THEN 1 ELSE 2 END,
                 a.approved_at DESC, a.created_at DESC
             LIMIT 500",
        )?;
        let mut rows = statement.query([])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(json!({
                "hash": row.get::<_, String>(0)?,
                "fingerprint": row.get::<_, String>(1)?,
                "name": row.get::<_, Option<String>>(2)?,
                "userId": row.get::<_, Option<String>>(3)?,
                "status": row.get::<_, String>(4)?,
                "approvedAt": row.get::<_, Option<String>>(5)?,
                "lastUsedAt": row.get::<_, Option<String>>(6)?,
                "username": row.get::<_, Option<String>>(7)?,
                "userDisplayName": row.get::<_, Option<String>>(8)?,
                "code": row.get::<_, Option<String>>(9)?,
                "nextThemeSeq": row.get::<_, i64>(10)?,
                "autoRevokeAt": row.get::<_, Option<String>>(11)?,
                "ownerRole": row.get::<_, Option<String>>(12)?,
                "ownerSuperAdmin": row.get::<_, Option<i64>>(13)?.unwrap_or(0) != 0,
                "revokeReason": row.get::<_, Option<String>>(14)?,
                "restrictionReason": row.get::<_, Option<String>>(15)?,
            }));
        }
        Ok(items)
    })
    .await
}

pub const AUTHOR_FEATURES: &[&str] = &["upload", "comment", "auto_publish"];

fn normalize_features(features: &HashSet<String>) -> String {
    AUTHOR_FEATURES
        .iter()
        .filter(|feature| features.contains(**feature))
        .copied()
        .collect::<Vec<_>>()
        .join(",")
}

pub async fn restrictions_for(db: &Database, hash: &str) -> Result<HashSet<String>> {
    let hash = hash.to_string();
    db.call(move |conn| {
        let value: Option<String> = conn
            .query_row(
                "SELECT features FROM authors.author_restrictions WHERE author_hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect())
    })
    .await
}

pub async fn all_restrictions(db: &Database) -> Result<HashMap<String, HashSet<String>>> {
    db.call(move |conn| {
        let mut statement =
            conn.prepare("SELECT author_hash, features FROM authors.author_restrictions")?;
        let mut rows = statement.query([])?;
        let mut items = HashMap::new();
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let features: String = row.get(1)?;
            let values: HashSet<String> = features
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect();
            items.insert(hash, values);
        }
        Ok(items)
    })
    .await
}

pub async fn set_restrictions(
    db: &Database,
    hash: &str,
    features: &HashSet<String>,
    actor: &str,
    reason: Option<&str>,
) -> Result<()> {
    let hash = hash.to_string();
    let value = normalize_features(features);
    let actor = actor.to_string();
    let reason = if value.is_empty() {
        None
    } else {
        reason.map(str::to_string)
    };
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO authors.author_restrictions(author_hash, features, updated_at, updated_by)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(author_hash) DO UPDATE SET
                 features = excluded.features,
                 updated_at = excluded.updated_at,
                 updated_by = excluded.updated_by",
            params![hash, value, now, actor],
        )?;
        conn.execute(
            "UPDATE authors.authors SET restriction_reason = ?2, updated_at = ?3 WHERE hash = ?1",
            params![hash, reason, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn has_restriction(db: &Database, hash: &str, feature: &str) -> Result<bool> {
    Ok(restrictions_for(db, hash).await?.contains(feature))
}

/// Pairs of (submitter user id, author hash) for themes currently in review.
/// A pending author request matching one of these is reviewed together with the
/// theme submission instead of appearing as a separate item.
pub fn pending_submission_author_pairs(paths: &Paths) -> HashSet<(String, String)> {
    catalog::pending_submissions(paths)
        .into_iter()
        .filter_map(|item| {
            let user_id = item
                .get("submitterUserId")
                .and_then(Value::as_str)?
                .trim();
            let hash = item.get("authorHash").and_then(Value::as_str)?.trim();
            if user_id.is_empty() || hash.is_empty() {
                return None;
            }
            Some((user_id.to_string(), hash.to_ascii_lowercase()))
        })
        .collect()
}

fn covered_request_id(request: &Value, pairs: &HashSet<(String, String)>) -> Option<String> {
    if request.get("status").and_then(Value::as_str) != Some("pending") {
        return None;
    }
    let kind = request.get("kind").and_then(Value::as_str).unwrap_or("");
    if !matches!(kind, "new" | "bind") {
        return None;
    }
    let id = request.get("id").and_then(Value::as_str)?;
    let user_id = request.get("userId").and_then(Value::as_str)?;
    let hash = request.get("authorHash").and_then(Value::as_str)?;
    pairs
        .contains(&(user_id.to_string(), hash.to_ascii_lowercase()))
        .then(|| id.to_string())
}

/// Ids of pending author requests already merged into a pending theme
/// submission. Approving that submission also approves the license, so these
/// requests must not be reviewed or counted separately.
pub fn covered_pending_request_ids(paths: &Paths, requests: &[Value]) -> HashSet<String> {
    let pairs = pending_submission_author_pairs(paths);
    requests
        .iter()
        .filter_map(|request| covered_request_id(request, &pairs))
        .collect()
}

/// Approve the pending author request(s) that belong to the account and license
/// of an approved theme submission. This is what lets a first-time author get
/// their license together with their first published theme.
pub async fn approve_pending_requests_for_submission(
    db: &Database,
    user_id: &str,
    hash: &str,
    actor: &str,
) -> Result<Vec<String>> {
    if user_id.trim().is_empty() || hash.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut approved = Vec::new();
    for request in author_requests(db, Some("pending"), Some(user_id)).await? {
        let kind = request.get("kind").and_then(Value::as_str).unwrap_or("");
        if !matches!(kind, "new" | "bind") {
            continue;
        }
        let matches = request
            .get("authorHash")
            .and_then(Value::as_str)
            .map(|value| value.eq_ignore_ascii_case(hash))
            .unwrap_or(false);
        if !matches {
            continue;
        }
        let Some(id) = request.get("id").and_then(Value::as_str) else {
            continue;
        };
        review_author_request(db, id, "approved", actor, None, Some(hash)).await?;
        approved.push(id.to_string());
    }
    Ok(approved)
}

/// Close pending author requests that are already satisfied: the license is
/// active and bound to the same account. Older theme approvals activated the
/// license but could leave the author application pending forever.
pub async fn reconcile_stale_author_requests(db: &Database) -> Result<Vec<(String, String)>> {
    let now = now_iso();
    let updated = db
        .call(move |conn| {
            let transaction = conn.transaction()?;
            let rows = {
                let mut statement = transaction.prepare(
                    "SELECT r.id, r.author_hash
                     FROM authors.author_requests r
                     JOIN authors.authors a ON a.hash = r.author_hash
                     WHERE r.status = 'pending'
                       AND r.kind IN ('new', 'bind')
                       AND r.author_hash IS NOT NULL
                       AND a.status = 'active'
                       AND a.user_id = r.user_id",
                )?;
                // Bind the rows before the block ends so the prepared statement is
                // not dropped while the mapped rows still borrow it.
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            let mut updated = Vec::new();
            for (id, hash) in rows {
                transaction.execute(
                    "UPDATE authors.author_requests
                     SET status = 'approved', reviewed_at = ?2, reviewed_by = 'system',
                         reject_reason = NULL, generated_author_hash = ?3
                     WHERE id = ?1 AND status = 'pending'",
                    params![&id, &now, &hash],
                )?;
                updated.push((id, hash));
            }
            transaction.commit()?;
            Ok(updated)
        })
        .await?;
    for (id, _) in &updated {
        let _ = crate::accounts::audit(
            db,
            Some("system"),
            "author.request.approve",
            Some("author_request"),
            Some(id),
            Some("自动清理：作者许可已生效"),
        )
        .await;
    }
    Ok(updated)
}

pub async fn count_pending_author_requests(db: &Database, paths: &Paths) -> Result<i64> {
    let pairs = pending_submission_author_pairs(paths);
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT user_id, author_hash, kind FROM authors.author_requests WHERE status = 'pending'",
        )?;
        let mut rows = statement.query([])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let user_id: String = row.get(0)?;
            let kind: String = row.get(2)?;
            let covered = matches!(kind.as_str(), "new" | "bind")
                && row
                    .get::<_, Option<String>>(1)?
                    .map(|hash| pairs.contains(&(user_id, hash.to_ascii_lowercase())))
                    .unwrap_or(false);
            if !covered {
                count += 1;
            }
        }
        Ok(count)
    })
    .await
}

pub async fn author_requests(
    db: &Database,
    status: Option<&str>,
    user_id: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    let status = status.map(str::to_string);
    let user_id = user_id.map(str::to_string);
    db.call(move |conn| {
        let mut sql = String::from(
            "SELECT r.id, r.kind, r.status, r.author_hash, r.display_name,
                    r.created_at, r.reviewed_at, r.reviewed_by, r.reject_reason,
                    u.id, u.username, u.display_name, a.status,
                    r.note, a.revoke_reason,
                    (SELECT prev.revoke_reason FROM authors.authors prev
                     WHERE prev.user_id = r.user_id AND prev.status IN ('revoked', 'retired')
                     ORDER BY prev.updated_at DESC LIMIT 1)
             FROM authors.author_requests r
             JOIN users u ON u.id = r.user_id
             LEFT JOIN authors.authors a ON a.hash = r.author_hash
             WHERE 1 = 1",
        );
        let mut values: Vec<String> = Vec::new();
        if let Some(status) = status {
            sql.push_str(" AND r.status = ?");
            values.push(status);
        }
        if let Some(user_id) = user_id {
            sql.push_str(" AND r.user_id = ?");
            values.push(user_id);
        }
        sql.push_str(" ORDER BY r.created_at DESC LIMIT 300");
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(values.iter()))?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(json!({
                "id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "authorHash": row.get::<_, Option<String>>(3)?,
                "displayName": row.get::<_, Option<String>>(4)?,
                "createdAt": row.get::<_, String>(5)?,
                "reviewedAt": row.get::<_, Option<String>>(6)?,
                "reviewedBy": row.get::<_, Option<String>>(7)?,
                "rejectReason": row.get::<_, Option<String>>(8)?,
                "userId": row.get::<_, String>(9)?,
                "username": row.get::<_, String>(10)?,
                "userDisplayName": row.get::<_, String>(11)?,
                "authorStatus": row.get::<_, Option<String>>(12)?,
                "note": row.get::<_, Option<String>>(13)?,
                "revokeReason": row.get::<_, Option<String>>(14)?,
                "lastRevokeReason": row.get::<_, Option<String>>(15)?,
            }));
        }
        Ok(items)
    })
    .await
}

pub async fn has_pending_request(db: &Database, user_id: &str) -> Result<bool> {
    Ok(!author_requests(db, Some("pending"), Some(user_id))
        .await?
        .is_empty())
}

/// Re-point the user's most recently updated published theme at a newly
/// approved author license. This keeps the latest work on the new license
/// while older themes stay with their original author hash.
pub async fn bind_latest_theme(
    db: &Database,
    paths: &Paths,
    user_id: &str,
    new_hash: &str,
    display_name: Option<&str>,
) -> Result<Option<String>> {
    let Some(user) = crate::accounts::user_by_id(db, user_id).await? else {
        return Ok(None);
    };
    let mut hashes: HashSet<String> = authors_for_user(db, user_id)
        .await?
        .into_iter()
        .map(|author| author.hash)
        .collect();
    hashes.insert(new_hash.to_string());
    let Ok(entries) = fs::read_dir(&paths.meta) else {
        return Ok(None);
    };
    let mut best: Option<(String, PathBuf, Meta)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(meta) = catalog::read_meta_file(&path) else {
            continue;
        };
        if meta.withdrawn_at.is_some() {
            continue;
        }
        let owned = match meta
            .author_hash
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            Some(hash) => hashes.contains(hash),
            None => {
                !meta.author.trim().is_empty()
                    && meta
                        .author
                        .trim()
                        .eq_ignore_ascii_case(user.username.trim())
            }
        };
        if !owned {
            continue;
        }
        let stamp = meta.updated_at.clone();
        if best
            .as_ref()
            .map(|(current, _, _)| stamp >= *current)
            .unwrap_or(true)
        {
            best = Some((stamp, path, meta));
        }
    }
    let Some((_, path, mut meta)) = best else {
        return Ok(None);
    };
    if meta.author_hash.as_deref() == Some(new_hash) {
        return Ok(Some(meta.id));
    }
    meta.author_hash = Some(new_hash.to_string());
    if let Some(name) = display_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        meta.author = name.to_string();
    }
    catalog::write_meta(&path, &meta)?;
    Ok(Some(meta.id))
}

pub async fn create_author_request(
    db: &Database,
    user_id: &str,
    kind: &str,
    author_hash: Option<&str>,
    display_name: &str,
    note: Option<&str>,
) -> Result<String> {
    let id = random_hex(12);
    let user_id = user_id.to_string();
    let kind = kind.to_string();
    let author_hash = author_hash.map(str::to_string);
    let display_name = display_name.trim().to_string();
    let note = note.map(str::to_string);
    let now = now_iso();
    let request_id = id.clone();
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO authors.author_requests
             (id, user_id, kind, author_hash, display_name, status, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
            params![
                request_id,
                user_id,
                kind,
                author_hash,
                display_name,
                note,
                now
            ],
        )?;
        Ok(())
    })
    .await?;
    Ok(id)
}

pub struct AuthorRequestRecord {
    pub id: String,
    pub user_id: String,
    pub kind: String,
    pub author_hash: Option<String>,
    pub display_name: Option<String>,
    pub status: String,
    pub generated_author_hash: Option<String>,
}

pub async fn author_request_by_id(db: &Database, id: &str) -> Result<Option<AuthorRequestRecord>> {
    let id = id.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT id, user_id, kind, author_hash, display_name, status, generated_author_hash
             FROM authors.author_requests WHERE id = ?1",
        )?;
        let mut rows = statement.query([id])?;
        Ok(rows.next()?.map(|row| AuthorRequestRecord {
            id: row.get(0).unwrap_or_default(),
            user_id: row.get(1).unwrap_or_default(),
            kind: row.get(2).unwrap_or_default(),
            author_hash: row.get(3).unwrap_or_default(),
            display_name: row.get(4).unwrap_or_default(),
            status: row.get(5).unwrap_or_default(),
            generated_author_hash: row.get(6).unwrap_or_default(),
        }))
    })
    .await
}

pub async fn review_author_request(
    db: &Database,
    id: &str,
    status: &str,
    reviewer: &str,
    reason: Option<&str>,
    generated_hash: Option<&str>,
) -> Result<()> {
    let id = id.to_string();
    let status = status.to_string();
    let reviewer = reviewer.to_string();
    let reason = reason.map(str::to_string);
    let generated_hash = generated_hash.map(str::to_string);
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors.author_requests
             SET status = ?2, reviewed_at = ?3, reviewed_by = ?4,
                 reject_reason = ?5, generated_author_hash = ?6
             WHERE id = ?1",
            params![id, status, now, reviewer, reason, generated_hash],
        )?;
        Ok(())
    })
    .await
}

pub async fn reset_author(db: &Database, user_id: &str, actor: &str) -> Result<(String, Author)> {
    let old = active_author_for_user(db, user_id)
        .await?
        .or(authors_for_user(db, user_id)
            .await?
            .into_iter()
            .find(|author| author.status != "retired"));
    let display_name = old
        .as_ref()
        .and_then(|author| author.display_name.clone())
        .unwrap_or_else(|| "作者".to_string());
    let raw_id = random_hex(24);
    let hash = author_key(&raw_id);
    let fingerprint = author_fingerprint(&hash);
    let author = create_author(
        db,
        &hash,
        &fingerprint,
        &display_name,
        Some(user_id),
        "active",
    )
    .await?;
    set_auto_publish(db, &hash, false).await?;
    if let Some(old) = old {
        set_author_status(db, &old.hash, "retired", Some(actor)).await?;
        let user_id = user_id.to_string();
        let old_hash = old.hash.clone();
        let new_hash = hash.clone();
        let actor = actor.to_string();
        let now = now_iso();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO authors.author_reset_history(user_id, old_hash, new_hash, created_at, created_by)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![user_id, old_hash, new_hash, now, actor],
            )?;
            Ok(())
        })
        .await?;
    }
    Ok((raw_id, author))
}

pub async fn import_legacy_authors(db: &Database, paths: &Paths) -> Result<bool> {
    let imported = db
        .call(|conn| Ok(db::get_meta(conn, "authors_json_imported")?))
        .await?;
    if imported.as_deref() == Some("1") {
        return Ok(false);
    }
    let path = &paths.authors_file;
    if !path.is_file() {
        db.call(|conn| {
            db::set_meta(conn, "authors_json_imported", "1")?;
            Ok(())
        })
        .await?;
        return Ok(false);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("无法读取 {}", path.display()))?;
    let authors: AuthorsFile =
        serde_json::from_str(&text).with_context(|| format!("{} 格式错误", path.display()))?;
    let mut names: HashMap<String, String> = HashMap::new();
    let mut latest = HashMap::<String, String>::new();
    if let Ok(entries) = std::fs::read_dir(&paths.meta) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                continue;
            }
            let Ok(meta) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&meta) else {
                continue;
            };
            let Some(hash) = value.get("authorHash").and_then(|value| value.as_str()) else {
                continue;
            };
            let stamp = value
                .get("updatedAt")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            if latest
                .get(hash)
                .map(|current| stamp.as_str() > current.as_str())
                .unwrap_or(true)
            {
                if let Some(author) = value.get("author").and_then(|value| value.as_str()) {
                    names.insert(hash.to_string(), author.to_string());
                    latest.insert(hash.to_string(), stamp);
                }
            }
        }
    }
    for (hash, record) in authors.authors {
        let name = names.get(&hash).cloned().unwrap_or_default();
        let fingerprint = record.fingerprint;
        let approved_at = record.approved_at;
        let last_used_at = record.last_used_at;
        let now = now_iso();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO authors.authors(hash, fingerprint, display_name, status, approved_at, last_used_at, created_at, updated_at)
                 VALUES (?1, ?2, NULLIF(?3, ''), 'active', NULLIF(?4, ''), NULLIF(?5, ''), ?6, ?6)
                 ON CONFLICT(hash) DO NOTHING",
                params![hash, fingerprint, name, approved_at, last_used_at, now],
            )?;
            Ok(())
        })
        .await?;
    }
    db.call(|conn| {
        db::set_meta(conn, "authors_json_imported", "1")?;
        Ok(())
    })
    .await?;
    Ok(true)
}

pub fn validate_author_id(value: &str) -> bool {
    valid_author_id(value)
}
