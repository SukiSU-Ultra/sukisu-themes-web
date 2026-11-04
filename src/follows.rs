use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{ApiError, AppState};
use crate::db::Database;
use crate::util::now_iso;

type ApiResult<T> = Result<T, ApiError>;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FollowedAuthor {
    pub author_hash: String,
    pub display_name: String,
}

pub fn admin_router() -> Router<AppState> {
    Router::new().route("/follows", get(admin_list))
}

pub async fn author_exists(db: &Database, author_hash: &str) -> anyhow::Result<bool> {
    let author_hash = author_hash.trim().to_string();
    db.call(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM authors WHERE hash = ?1",
            [author_hash],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    })
    .await
}

/// Inserts the relation if it does not exist. Returns `true` when a new row was created.
pub async fn follow(
    db: &Database,
    follower_id: &str,
    author_hash: &str,
) -> anyhow::Result<bool> {
    let follower_id = follower_id.to_string();
    let author_hash = author_hash.trim().to_string();
    let created_at = now_iso();
    db.call(move |conn| {
        let changed = conn.execute(
            "INSERT OR IGNORE INTO user_follows(follower_id, author_hash, created_at)
             VALUES (?1, ?2, ?3)",
            params![follower_id, author_hash, created_at],
        )?;
        Ok(changed > 0)
    })
    .await
}

pub async fn unfollow(
    db: &Database,
    follower_id: &str,
    author_hash: &str,
) -> anyhow::Result<bool> {
    let follower_id = follower_id.to_string();
    let author_hash = author_hash.trim().to_string();
    db.call(move |conn| {
        let changed = conn.execute(
            "DELETE FROM user_follows WHERE follower_id = ?1 AND author_hash = ?2",
            params![follower_id, author_hash],
        )?;
        Ok(changed > 0)
    })
    .await
}

pub async fn following_authors(
    db: &Database,
    follower_id: &str,
) -> anyhow::Result<Vec<FollowedAuthor>> {
    let follower_id = follower_id.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT f.author_hash, COALESCE(a.display_name, '')
             FROM user_follows f
             LEFT JOIN authors a ON a.hash = f.author_hash
             WHERE f.follower_id = ?1
             ORDER BY f.created_at DESC",
        )?;
        let rows = statement.query_map([follower_id], |row| {
            Ok(FollowedAuthor {
                author_hash: row.get(0)?,
                display_name: row.get(1)?,
            })
        })?;
        let mut values = Vec::new();
        for row in rows {
            values.push(row?);
        }
        Ok(values)
    })
    .await
}

pub async fn following_count(db: &Database, user_id: &str) -> anyhow::Result<i64> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM user_follows WHERE follower_id = ?1",
            [user_id],
            |row| row.get(0),
        )?;
        Ok(count)
    })
    .await
}

/// Number of distinct users following any active author identity owned by `user_id`.
pub async fn follower_count(db: &Database, user_id: &str) -> anyhow::Result<i64> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT f.follower_id)
             FROM user_follows f
             JOIN authors a ON a.hash = f.author_hash
             WHERE a.user_id = ?1 AND a.status = 'active'",
            [user_id],
            |row| row.get(0),
        )?;
        Ok(count)
    })
    .await
}

pub async fn author_follower_count(db: &Database, author_hash: &str) -> anyhow::Result<i64> {
    let author_hash = author_hash.to_string();
    db.call(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM user_follows WHERE author_hash = ?1",
            [author_hash],
            |row| row.get(0),
        )?;
        Ok(count)
    })
    .await
}

#[derive(Deserialize)]
struct AdminListQuery {
    #[serde(default)]
    q: String,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn admin_list(
    State(state): State<AppState>,
    Query(query): Query<AdminListQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    let offset = query.offset.unwrap_or(0).max(0);
    let filter = if query.q.trim().is_empty() { 0i64 } else { 1i64 };
    let pattern = format!("%{}%", query.q.trim().to_lowercase());
    let rows = state
        .db
        .call(move |conn| {
            let mut statement = conn.prepare(
                "SELECT f.follower_id,
                        u.username,
                        u.display_name,
                        f.author_hash,
                        COALESCE(a.display_name, ''),
                        COALESCE(a.user_id, ''),
                        COALESCE(ou.username, ''),
                        f.created_at
                 FROM user_follows f
                 JOIN users u ON u.id = f.follower_id
                 LEFT JOIN authors a ON a.hash = f.author_hash
                 LEFT JOIN users ou ON ou.id = a.user_id
                 WHERE ?1 = 0
                    OR lower(u.username) LIKE ?2
                    OR lower(u.display_name) LIKE ?2
                    OR lower(COALESCE(a.display_name, '')) LIKE ?2
                    OR lower(COALESCE(ou.username, '')) LIKE ?2
                 ORDER BY f.created_at DESC
                 LIMIT ?3 OFFSET ?4",
            )?;
            let rows = statement.query_map(params![filter, pattern, limit, offset], |row| {
                Ok(json!({
                    "followerId": row.get::<_, String>(0)?,
                    "followerUsername": row.get::<_, String>(1)?,
                    "followerDisplayName": row.get::<_, String>(2)?,
                    "authorHash": row.get::<_, String>(3)?,
                    "authorName": row.get::<_, String>(4)?,
                    "ownerId": row.get::<_, String>(5)?,
                    "ownerUsername": row.get::<_, String>(6)?,
                    "createdAt": row.get::<_, String>(7)?,
                }))
            })?;
            let mut values = Vec::new();
            for row in rows {
                values.push(row?);
            }
            let total: i64 = conn.query_row("SELECT COUNT(*) FROM user_follows", [], |row| row.get(0))?;
            Ok((values, total))
        })
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(json!({
        "ok": true,
        "follows": rows.0,
        "total": rows.1,
    })))
}
