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
            "SELECT COUNT(*) FROM authors.authors WHERE hash = ?1",
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
            "INSERT OR IGNORE INTO social.user_follows(follower_id, author_hash, created_at)
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
            "DELETE FROM social.user_follows WHERE follower_id = ?1 AND author_hash = ?2",
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
             FROM social.user_follows f
             LEFT JOIN authors.authors a ON a.hash = f.author_hash
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
            "SELECT COUNT(*) FROM social.user_follows WHERE follower_id = ?1",
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
             FROM social.user_follows f
             JOIN authors.authors a ON a.hash = f.author_hash
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
            "SELECT COUNT(*) FROM social.user_follows WHERE author_hash = ?1",
            [author_hash],
            |row| row.get(0),
        )?;
        Ok(count)
    })
    .await
}

/// One row per account that owns at least one active author identity, with the
/// distinct follower count across every identity (a follower following two
/// identities of the same account is counted once) and the identity hashes.
#[derive(Debug, Clone)]
pub struct LeaderboardUser {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub avatar: Option<String>,
    pub author_hashes: Vec<String>,
    pub followers: i64,
}

pub async fn leaderboard_users(db: &Database) -> anyhow::Result<Vec<LeaderboardUser>> {
    db.call(|conn| {
        let mut statement = conn.prepare(
            "SELECT u.id, u.username, u.display_name, u.avatar,
                    GROUP_CONCAT(DISTINCT a.hash),
                    COUNT(DISTINCT f.follower_id)
             FROM users u
             JOIN authors.authors a ON a.user_id = u.id AND a.status = 'active'
             LEFT JOIN social.user_follows f ON f.author_hash = a.hash
             WHERE u.status NOT IN ('rejected', 'disabled')
             GROUP BY u.id, u.username, u.display_name, u.avatar",
        )?;
        let rows = statement.query_map([], |row| {
            let hashes: Option<String> = row.get(4)?;
            Ok(LeaderboardUser {
                user_id: row.get(0)?,
                username: row.get(1)?,
                display_name: row.get(2)?,
                avatar: row.get(3)?,
                author_hashes: hashes
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect(),
                followers: row.get(5)?,
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

/// Active author identities owned by rankable accounts; used to attribute
/// theme heat (from the catalog) back to the account leaderboard rows.
#[derive(Debug, Clone)]
pub struct LeaderboardAuthor {
    pub hash: String,
    pub display_name: Option<String>,
    pub created_at: String,
}

pub async fn leaderboard_authors(db: &Database) -> anyhow::Result<Vec<LeaderboardAuthor>> {
    db.call(|conn| {
        let mut statement = conn.prepare(
            "SELECT a.hash, a.display_name, a.created_at
             FROM authors.authors a
             JOIN users u ON u.id = a.user_id
             WHERE a.status = 'active' AND u.status NOT IN ('rejected', 'disabled')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(LeaderboardAuthor {
                hash: row.get(0)?,
                display_name: row.get(1)?,
                created_at: row.get(2)?,
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
                 FROM social.user_follows f
                 JOIN users u ON u.id = f.follower_id
                 LEFT JOIN authors.authors a ON a.hash = f.author_hash
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
            let total: i64 = conn.query_row("SELECT COUNT(*) FROM social.user_follows", [], |row| row.get(0))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn leaderboard_counts_followers_once_per_account_with_multiple_identities() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open_test(directory.path())
            .await
            .unwrap();
        db.call(|conn| {
            for id in ["alice", "bob", "carol", "gone"] {
                conn.execute(
                    "INSERT INTO users(id, username, display_name, status, created_at, updated_at)
                     VALUES (?1, ?1, ?1, ?2, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                    params![id, if id == "gone" { "rejected" } else { "active" }],
                )?;
            }
            // Alice owns two active identities plus one revoked one; the
            // revoked hash must not appear, and carol following both of
            // Alice's identities still counts as one follower.
            conn.execute(
                "INSERT INTO authors.authors(hash, fingerprint, display_name, user_id, status, created_at, updated_at)
                 VALUES ('h1', 'f1', 'Alice One', 'alice', 'active', '2026-01-02T00:00:00Z', '2026-01-02T00:00:00Z'),
                        ('h2', 'f2', NULL, 'alice', 'active', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
                        ('h3', 'f3', 'Alice Gone', 'alice', 'revoked', '2026-01-03T00:00:00Z', '2026-01-03T00:00:00Z'),
                        ('h4', 'f4', 'Bob', 'bob', 'active', '2026-01-04T00:00:00Z', '2026-01-04T00:00:00Z'),
                        ('h5', 'f5', 'Gone Author', 'gone', 'active', '2026-01-05T00:00:00Z', '2026-01-05T00:00:00Z')",
                [],
            )?;
            conn.execute(
                "INSERT INTO social.user_follows(follower_id, author_hash, created_at)
                 VALUES ('bob', 'h1', '2026-01-05T00:00:00Z'),
                        ('carol', 'h1', '2026-01-05T00:00:00Z'),
                        ('carol', 'h2', '2026-01-06T00:00:00Z'),
                        ('alice', 'h5', '2026-01-06T00:00:00Z')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let mut users = leaderboard_users(&db).await.unwrap();
        users.sort_by(|left, right| left.user_id.cmp(&right.user_id));
        assert_eq!(users.len(), 2, "rejected owners must be excluded");
        let alice = users.iter().find(|user| user.user_id == "alice").unwrap();
        assert_eq!(alice.followers, 2, "bob + carol, deduped across identities");
        let mut alice_hashes = alice.author_hashes.clone();
        alice_hashes.sort();
        assert_eq!(alice_hashes, vec!["h1".to_string(), "h2".to_string()]);
        let bob = users.iter().find(|user| user.user_id == "bob").unwrap();
        assert_eq!(bob.followers, 0);

        let mut hashes: Vec<String> = leaderboard_authors(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|author| author.hash)
            .collect();
        hashes.sort();
        assert_eq!(
            hashes,
            vec!["h1".to_string(), "h2".to_string(), "h4".to_string()],
            "revoked identities and rejected owners are not rankable"
        );
    }
}
