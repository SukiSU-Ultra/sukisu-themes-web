use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::json;

use crate::config::Paths;
use crate::db::{self, Database};
use crate::models::AuthorsFile;
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
    })
}

const AUTHOR_COLUMNS: &str = "hash, fingerprint, display_name, user_id, status, approved_at, last_used_at, created_at, auto_publish, code, next_theme_seq";

pub async fn author_by_hash(db: &Database, hash: &str) -> Result<Option<Author>> {
    let hash = hash.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {AUTHOR_COLUMNS} FROM authors WHERE hash = ?1"
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
            "SELECT {AUTHOR_COLUMNS} FROM authors WHERE user_id = ?1
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

pub async fn active_author_for_user(db: &Database, user_id: &str) -> Result<Option<Author>> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {AUTHOR_COLUMNS} FROM authors
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
            "INSERT INTO authors(hash, fingerprint, display_name, user_id, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(hash) DO UPDATE SET
                 display_name = COALESCE(NULLIF(excluded.display_name, ''), authors.display_name),
                 user_id = COALESCE(authors.user_id, excluded.user_id),
                 updated_at = excluded.updated_at",
            params![insert_hash, fingerprint, display_name, user_id, status, now],
        )?;
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
    let hash = hash.to_string();
    let status = status.to_string();
    let actor = actor.map(str::to_string);
    let now = now_iso();
    db.call(move |conn| {
        let approved_at = if status == "active" {
            Some(now.clone())
        } else {
            None
        };
        conn.execute(
            "UPDATE authors
             SET status = ?2, approved_at = COALESCE(?3, approved_at),
                 approved_by = COALESCE(?4, approved_by), updated_at = ?5
             WHERE hash = ?1",
            params![hash, status, approved_at, actor, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn delete_author(db: &Database, hash: &str) -> Result<usize> {
    let hash = hash.to_string();
    db.call(move |conn| Ok(conn.execute("DELETE FROM authors WHERE hash = ?1", [&hash])?))
        .await
}

pub async fn delete_authors_for_user(db: &Database, user_id: &str) -> Result<usize> {
    let user_id = user_id.to_string();
    db.call(move |conn| Ok(conn.execute("DELETE FROM authors WHERE user_id = ?1", [&user_id])?))
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
            "UPDATE authors
             SET user_id = ?2,
                 display_name = COALESCE(?3, display_name),
                 updated_at = ?4
             WHERE hash = ?1",
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
                "SELECT code, next_theme_seq FROM authors WHERE hash = ?1",
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
                    "SELECT COALESCE(MAX(CAST(code AS INTEGER)), 11009) FROM authors WHERE code GLOB '[0-9]*'",
                    [],
                    |row| row.get(0),
                )?;
                let code = (max + 1).max(11010).to_string();
                transaction.execute(
                    "UPDATE authors SET code = ?2, updated_at = ?3 WHERE hash = ?1",
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
                    "SELECT code, next_theme_seq FROM authors WHERE hash = ?1",
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
                        "SELECT COALESCE(MAX(CAST(code AS INTEGER)), 11009) FROM authors WHERE code GLOB '[0-9]*'",
                        [],
                        |row| row.get(0),
                    )?;
                    (max + 1).max(11010).to_string()
                }
            };
            transaction.execute(
                "UPDATE authors SET code = ?2, next_theme_seq = ?3, updated_at = ?4 WHERE hash = ?1",
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
            "UPDATE authors SET auto_publish = ?2, updated_at = ?3 WHERE hash = ?1",
            params![hash, if enabled { 1 } else { 0 }, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn touch_author(db: &Database, hash: &str) -> Result<()> {
    let hash = hash.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors SET last_used_at = ?2, updated_at = ?2 WHERE hash = ?1",
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
            "UPDATE authors SET display_name = ?2, updated_at = ?3 WHERE hash = ?1",
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
                    a.code, a.next_theme_seq
             FROM authors a LEFT JOIN users u ON u.id = a.user_id
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
            }));
        }
        Ok(items)
    })
    .await
}

pub async fn count_pending_author_requests(db: &Database) -> Result<i64> {
    db.call(|conn| {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM author_requests WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )?)
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
                    u.id, u.username, u.display_name, a.status
             FROM author_requests r
             JOIN users u ON u.id = r.user_id
             LEFT JOIN authors a ON a.hash = r.author_hash
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
            }));
        }
        Ok(items)
    })
    .await
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
            "INSERT INTO author_requests
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
}

pub async fn author_request_by_id(db: &Database, id: &str) -> Result<Option<AuthorRequestRecord>> {
    let id = id.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT id, user_id, kind, author_hash, display_name, status
             FROM author_requests WHERE id = ?1",
        )?;
        let mut rows = statement.query([id])?;
        Ok(rows.next()?.map(|row| AuthorRequestRecord {
            id: row.get(0).unwrap_or_default(),
            user_id: row.get(1).unwrap_or_default(),
            kind: row.get(2).unwrap_or_default(),
            author_hash: row.get(3).unwrap_or_default(),
            display_name: row.get(4).unwrap_or_default(),
            status: row.get(5).unwrap_or_default(),
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
            "UPDATE author_requests
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
                "INSERT INTO author_reset_history(user_id, old_hash, new_hash, created_at, created_by)
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
                "INSERT INTO authors(hash, fingerprint, display_name, status, approved_at, last_used_at, created_at, updated_at)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allocates_author_codes_and_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.db")).await.unwrap();
        let hash1 = author_key("aaaaaaaaaaaaaaaa");
        let author1 = create_author(
            &db,
            &hash1,
            &author_fingerprint(&hash1),
            "Author One",
            None,
            "active",
        )
        .await
        .unwrap();
        assert_eq!(
            allocate_theme_id(&db, &author1.hash).await.unwrap(),
            "11010-01"
        );
        assert_eq!(
            allocate_theme_id(&db, &author1.hash).await.unwrap(),
            "11010-02"
        );
        let hash2 = author_key("bbbbbbbbbbbbbbbb");
        let author2 = create_author(
            &db,
            &hash2,
            &author_fingerprint(&hash2),
            "Author Two",
            None,
            "active",
        )
        .await
        .unwrap();
        assert_eq!(
            allocate_theme_id(&db, &author2.hash).await.unwrap(),
            "11011-01"
        );
        let loaded = author_by_hash(&db, &author1.hash).await.unwrap().unwrap();
        assert_eq!(loaded.code.as_deref(), Some("11010"));
        assert_eq!(loaded.next_theme_seq, 3);
    }
}
