use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::accounts::{self, SessionInfo, User};
use crate::api::{json_body, ApiError, AppState};
use crate::auth;
use crate::authors;
use crate::mailer;

type ApiResult<T> = Result<T, ApiError>;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users", get(users))
        .route("/users/{id}", get(user_detail))
        .route("/users/{id}/approve", post(user_approve))
        .route("/users/{id}/reject", post(user_reject))
        .route("/users/{id}/disable", post(user_disable))
        .route("/users/{id}/enable", post(user_enable))
        .route("/users/{id}/delete", post(user_delete))
        .route("/users/{id}/reset-password", post(user_reset_password))
        .route("/users/{id}/role", post(user_role))
        .route("/users/{id}/author/bind", post(user_author_bind))
        .route("/users/{id}/author/reset", post(user_author_reset))
        .route("/author-requests", get(author_requests))
        .route("/author-requests/bulk", post(author_request_bulk))
        .route(
            "/author-requests/{id}/approve",
            post(author_request_approve),
        )
        .route("/author-requests/{id}/reject", post(author_request_reject))
        .route("/audit", get(audit))
        .route("/username", post(set_username))
        .route("/theme-id/next", get(next_theme_id))
}

async fn ensure_super_admin_target_allowed(
    state: &AppState,
    actor: &SessionInfo,
    target_id: &str,
) -> ApiResult<()> {
    let target = accounts::user_by_id(&state.db, target_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    if target.is_super_admin
        && !accounts::is_super_admin(&state.db, &actor.user_id)
            .await
            .map_err(ApiError::internal)?
    {
        return Err(ApiError::forbidden("只有超级管理员可以操作超级管理员账号"));
    }
    Ok(())
}

async fn author_avatar_url(state: &AppState, author: &authors::Author) -> Option<String> {
    let key = crate::catalog::author_avatar_key(
        &state.store,
        &state.paths,
        &author.hash,
        author.display_name.as_deref(),
    )
    .await?;
    Some(format!(
        "/admin/api/preview?path={}",
        crate::util::quote_query(&key)
    ))
}

fn user_json(user: &User) -> Value {
    json!({
        "id": user.id,
        "username": user.username,
        "displayName": user.display_name,
        "email": user.email,
        "avatar": user.avatar,
        "role": user.role,
        "isSuperAdmin": user.is_super_admin,
        "status": user.status,
        "emailVerified": user.email_verified,
        "rejectionReason": user.rejection_reason,
        "createdAt": user.created_at,
        "approvedAt": user.approved_at,
        "lastLoginAt": user.last_login_at,
    })
}

#[derive(Deserialize)]
struct UserQuery {
    status: Option<String>,
    q: Option<String>,
}

async fn users(
    State(state): State<AppState>,
    Query(query): Query<UserQuery>,
) -> ApiResult<Json<Value>> {
    let status = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "all");
    let list = accounts::list_users(&state.db, status, query.q.as_deref().unwrap_or(""))
        .await
        .map_err(ApiError::internal)?;
    let pending = accounts::count_pending_users(&state.db)
        .await
        .map_err(ApiError::internal)?;
    let total = accounts::count_users(&state.db)
        .await
        .map_err(ApiError::internal)?;
    let author_pending = authors::count_pending_author_requests_split(&state.db, &state.paths)
        .await
        .map_err(ApiError::internal)?;
    let disabled = accounts::count_disabled_users(&state.db)
        .await
        .map_err(ApiError::internal)?;
    let mut users = Vec::new();
    for user in &list {
        let mut value = user_json(user);
        if let Ok(authors) = authors::authors_for_user(&state.db, &user.id).await {
            if let Some(author) = authors
                .iter()
                .find(|author| author.status == "active")
                .or_else(|| authors.iter().find(|author| author.status == "pending"))
            {
                if let Some(object) = value.as_object_mut() {
                    object.insert("authorName".to_string(), json!(author.display_name));
                    object.insert("authorStatus".to_string(), json!(author.status));
                    object.insert("authorHash".to_string(), json!(author.hash));
                    if let Some(url) = author_avatar_url(&state, author).await {
                        object.insert("authorAvatarUrl".to_string(), json!(url));
                    }
                }
            }
        }
        users.push(value);
    }
    Ok(Json(json!({
        "ok": true,
        "users": users,
        "count": list.len(),
        "counts": {
            "total": total,
            "pending": pending,
            "authorRequests": author_pending.0 + author_pending.1,
            "submissionAuthorRequests": author_pending.1,
            "disabled": disabled,
        }
    })))
}

async fn user_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let user = accounts::user_by_id(&state.db, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    let profile_authors = authors::authors_for_user(&state.db, &id)
        .await
        .map_err(ApiError::internal)?;
    let submissions = accounts::user_submissions(&state.db, &id)
        .await
        .map_err(ApiError::internal)?;
    let author_requests = authors::author_requests(&state.db, None, Some(&id))
        .await
        .map_err(ApiError::internal)?;
    let mut author_values = Vec::with_capacity(profile_authors.len());
    for author in &profile_authors {
        let mut value = serde_json::to_value(author).unwrap_or_default();
        if let Some(url) = author_avatar_url(&state, author).await {
            if let Some(object) = value.as_object_mut() {
                object.insert("avatarUrl".to_string(), json!(url));
            }
        }
        author_values.push(value);
    }
    let hashes: Vec<String> = profile_authors
        .iter()
        .map(|author| author.hash.clone())
        .collect();
    let mut theme_count = 0;
    if let Ok(entries) = std::fs::read_dir(&state.paths.meta) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(meta) = crate::catalog::read_meta_file(&path) {
                if meta
                    .author_hash
                    .as_ref()
                    .map(|hash| hashes.contains(hash))
                    .unwrap_or(false)
                {
                    theme_count += 1;
                }
            }
        }
    }
    Ok(Json(json!({
        "ok": true,
        "user": user_json(&user),
        "authors": author_values,
        "submissions": submissions,
        "authorRequests": author_requests,
        "themeCount": theme_count,
    })))
}

async fn user_approve(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let user = accounts::user_by_id(&state.db, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    if user.status == "disabled" {
        return Err(ApiError::conflict("请先启用该账号"));
    }
    accounts::set_status(&state.db, &id, "active", None, Some(&actor.user_id))
        .await
        .map_err(ApiError::internal)?;
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "user.approve",
        Some("user"),
        Some(&id),
        None,
    )
    .await
    .ok();
    if let Some(email) = user.email {
        mailer::queue_notice(
            &state.paths,
            &email,
            "SukiSU 主题镜像账号已通过审核",
            "你的账号已经通过管理员审核，现在可以登录用户中心、绑定作者身份并投稿。",
        );
    }
    Ok(Json(json!({"ok": true, "message": "账号已通过审核"})))
}

async fn user_reject(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let reason = value
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !(1..=500).contains(&reason.chars().count()) {
        return Err(ApiError::bad("请填写 1-500 字的拒绝原因"));
    }
    let user = accounts::user_by_id(&state.db, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    ensure_super_admin_target_allowed(&state, &actor, &id).await?;
    accounts::set_status(
        &state.db,
        &id,
        "rejected",
        Some(&reason),
        Some(&actor.user_id),
    )
    .await
    .map_err(ApiError::internal)?;
    state.sessions.revoke_user(&id).await;
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "user.reject",
        Some("user"),
        Some(&id),
        Some(&reason),
    )
    .await
    .ok();
    if let Some(email) = user.email {
        let message = if reason.is_empty() {
            "账号审核未通过。".to_string()
        } else {
            format!("账号审核未通过：{reason}")
        };
        mailer::queue_notice(
            &state.paths,
            &email,
            "SukiSU 主题镜像账号审核结果",
            &message,
        );
    }
    Ok(Json(json!({"ok": true, "message": "账号已拒绝"})))
}

async fn user_disable(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let reason = value
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !(1..=500).contains(&reason.chars().count()) {
        return Err(ApiError::bad("请填写 1-500 字的禁用原因"));
    }
    if id == actor.user_id {
        return Err(ApiError::bad("不能禁用当前登录账号"));
    }
    ensure_super_admin_target_allowed(&state, &actor, &id).await?;
    accounts::set_status(&state.db, &id, "disabled", Some(&reason), Some(&actor.user_id))
        .await
        .map_err(ApiError::internal)?;
    state.sessions.revoke_user(&id).await;
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "user.disable",
        Some("user"),
        Some(&id),
        Some(&reason),
    )
    .await
    .ok();
    Ok(Json(json!({"ok": true, "message": "账号已禁用"})))
}

async fn user_enable(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    accounts::set_status(&state.db, &id, "active", None, Some(&actor.user_id))
        .await
        .map_err(ApiError::internal)?;
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "user.enable",
        Some("user"),
        Some(&id),
        None,
    )
    .await
    .ok();
    Ok(Json(json!({"ok": true, "message": "账号已启用"})))
}

async fn user_delete(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let reason = value
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !(1..=500).contains(&reason.chars().count()) {
        return Err(ApiError::bad("请填写 1-500 字的删除原因"));
    }
    if id == actor.user_id {
        return Err(ApiError::bad("不能删除当前登录账号"));
    }
    let user = accounts::user_by_id(&state.db, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    if user.role == "admin" {
        return Err(ApiError::bad("管理员账号不能删除，请先取消管理员身份"));
    }
    if user.status != "disabled" && user.status != "rejected" {
        return Err(ApiError::bad("只有已禁用或已拒绝的账号可以删除"));
    }
    state.sessions.revoke_user(&id).await;
    if let Some(avatar) = user.avatar.as_deref().filter(|value| !value.is_empty()) {
        let _ = state.store.delete(avatar).await;
    }
    let removed_authors = authors::delete_authors_for_user(&state.db, &id)
        .await
        .map_err(ApiError::internal)?;
    accounts::delete_user(&state.db, &id)
        .await
        .map_err(ApiError::internal)?;
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "user.delete",
        Some("user"),
        Some(&id),
        Some(&format!(
            "{}; authors={removed_authors}; reason={reason}",
            user.username
        )),
    )
    .await
    .ok();
    Ok(Json(json!({"ok": true, "message": "账号已永久删除"})))
}

async fn user_reset_password(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !mailer::configured(&state.paths) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "邮件服务尚未配置，无法发送重置邮件",
        ));
    }
    ensure_super_admin_target_allowed(&state, &actor, &id).await?;
    let user = accounts::user_by_id(&state.db, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    let Some(email) = user.email.clone() else {
        return Err(ApiError::bad("该用户没有绑定邮箱"));
    };
    let token = accounts::issue_email_token(
        &state.db,
        Some(&id),
        &email,
        "reset_password",
        accounts::PASSWORD_RESET_TTL_SECS,
    )
    .await
    .map_err(ApiError::internal)?;
    mailer::send_password_reset(&state.paths, &email, &token)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, error.to_string()))?;
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "user.reset_password",
        Some("user"),
        Some(&id),
        None,
    )
    .await
    .ok();
    Ok(Json(json!({"ok": true, "message": "重置邮件已发送"})))
}

async fn user_role(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let role = value
        .get("role")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !matches!(role.as_str(), "user" | "admin") {
        return Err(ApiError::bad("角色只能是 user 或 admin"));
    }
    if id == actor.user_id && role != "admin" {
        return Err(ApiError::bad("不能取消自己的管理员角色"));
    }
    let target = accounts::user_by_id(&state.db, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    let actor_super = accounts::is_super_admin(&state.db, &actor.user_id)
        .await
        .map_err(ApiError::internal)?;
    if target.is_super_admin && !actor_super {
        return Err(ApiError::forbidden("只有超级管理员可以修改超级管理员"));
    }
    if target.is_super_admin && role != "admin" {
        return Err(ApiError::bad("不能取消超级管理员的管理员角色"));
    }
    accounts::set_role(&state.db, &id, &role)
        .await
        .map_err(ApiError::internal)?;
    // Role changes must take effect immediately; a cached admin session must
    // not outlive a demotion (or keep an old role after promotion).
    state.sessions.revoke_user(&id).await;
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "user.role",
        Some("user"),
        Some(&id),
        Some(&role),
    )
    .await
    .ok();
    Ok(Json(json!({"ok": true, "message": "角色已更新"})))
}

async fn user_author_bind(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let hash = value
        .get("authorHash")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if hash.len() != 64 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(ApiError::bad("作者 hash 无效"));
    }
    let author = authors::author_by_hash(&state.db, &hash)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("作者不存在"))?;
    if let Some(owner) = author.user_id {
        if owner != id {
            return Err(ApiError::conflict("该作者已绑定其他用户"));
        }
    }
    authors::bind_author_to_user(&state.db, &hash, &id, None)
        .await
        .map_err(ApiError::internal)?;
    authors::set_author_status(&state.db, &hash, "active", Some(&actor.user_id))
        .await
        .map_err(ApiError::internal)?;
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "author.bind",
        Some("author"),
        Some(&hash),
        Some(&id),
    )
    .await
    .ok();
    Ok(Json(json!({"ok": true, "message": "作者已绑定"})))
}

async fn user_author_reset(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let target = accounts::user_by_id(&state.db, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    let actor_super = accounts::is_super_admin(&state.db, &actor.user_id)
        .await
        .map_err(ApiError::internal)?;
    if target.is_super_admin && !actor_super {
        return Err(ApiError::forbidden(
            "只有超级管理员可以重置超级管理员的作者许可",
        ));
    }
    let (raw_id, author) = authors::reset_author(&state.db, &id, &actor.user_id)
        .await
        .map_err(ApiError::internal)?;
    if let Ok(Some(theme_id)) = authors::bind_latest_theme(
        &state.db,
        &state.paths,
        &id,
        &author.hash,
        author.display_name.as_deref(),
    )
    .await
    {
        if let Err(error) = crate::catalog::rebuild_catalog(&state.store, &state.paths).await {
            tracing::warn!("author theme rebind catalog rebuild failed: {error:#}");
        }
        state.refresh_withdrawn_assets();
        let _ = accounts::audit(
            &state.db,
            Some(&actor.user_id),
            "author.bind_latest_theme",
            Some("theme"),
            Some(&theme_id),
            Some(&author.hash),
        )
        .await;
    }
    if let Err(error) =
        authors::schedule_grace_if_needed(&state.db, &state.paths, &author.hash).await
    {
        tracing::warn!("author grace scheduling failed: {error:#}");
    }
    let _ = crate::uploads::inherit_author_avatar(
        &state.store,
        &state.paths,
        &state.db,
        &id,
        &author.hash,
        author.display_name.as_deref(),
    )
    .await;
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "author.reset",
        Some("author"),
        Some(&author.hash),
        Some(&id),
    )
    .await
    .ok();
    Ok(Json(json!({
        "ok": true,
        "authorId": raw_id,
        "author": author,
        "message": "新作者 ID 只显示一次，请转交给用户",
    })))
}

#[derive(Deserialize)]
struct RequestQuery {
    status: Option<String>,
}

async fn author_requests(
    State(state): State<AppState>,
    Query(query): Query<RequestQuery>,
) -> ApiResult<Json<Value>> {
    let status = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "all");
    let mut requests = authors::author_requests(&state.db, status, None)
        .await
        .map_err(ApiError::internal)?;
    // First-time author requests are reviewed together with their pending
    // theme submission, so they must not show up as a second review item.
    let covered = authors::covered_pending_request_ids(&state.paths, &requests);
    requests.retain(|request| {
        !request
            .get("id")
            .and_then(Value::as_str)
            .map(|id| covered.contains(id))
            .unwrap_or(false)
    });
    for request in &mut requests {
        let Some(hash) = request.get("authorHash").and_then(Value::as_str) else {
            continue;
        };
        let name = request.get("displayName").and_then(Value::as_str);
        if let Some(key) =
            crate::catalog::author_avatar_key(&state.store, &state.paths, hash, name).await
        {
            if let Some(object) = request.as_object_mut() {
                object.insert(
                    "avatarUrl".to_string(),
                    json!(format!(
                        "/admin/api/preview?path={}",
                        crate::util::quote_query(&key)
                    )),
                );
            }
        }
    }
    Ok(Json(json!({"ok": true, "requests": requests})))
}

async fn author_request_approve(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    approve_author_request_record(&state, &actor, &id).await?;
    Ok(Json(json!({"ok": true, "message": "申请已通过"})))
}

async fn approve_author_request_record(
    state: &AppState,
    actor: &SessionInfo,
    id: &str,
) -> ApiResult<()> {
    let request = authors::author_request_by_id(&state.db, id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("申请不存在"))?;
    if request.status != "pending" {
        return Err(ApiError::conflict("该申请已处理"));
    }
    if request.kind != "reset" && request.author_hash.is_none() {
        return Err(ApiError::bad("申请缺少作者 hash"));
    }
    let activated = authors::approve_author_request_atomic(&state.db, id, &actor.user_id)
        .await
        .map_err(ApiError::internal)?;
    if !activated {
        return Err(ApiError::conflict("该申请已处理"));
    }
    if request.kind != "reset" {
        let hash = request.author_hash.as_deref().unwrap_or_default();
        if request.kind == "new" {
            match authors::bind_latest_theme(
                &state.db,
                &state.paths,
                &request.user_id,
                hash,
                request.display_name.as_deref(),
            )
            .await
            {
                Ok(Some(theme_id)) => {
                    if let Err(error) =
                        crate::catalog::rebuild_catalog(&state.store, &state.paths).await
                    {
                        tracing::warn!("author theme rebind catalog rebuild failed: {error:#}");
                    }
                    state.refresh_withdrawn_assets();
                    if let Err(error) = accounts::audit(
                        &state.db,
                        Some(&actor.user_id),
                        "author.bind_latest_theme",
                        Some("theme"),
                        Some(&theme_id),
                        Some(hash),
                    )
                    .await
                    {
                        tracing::warn!("author bind audit failed: {error:#}");
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!("author theme rebind failed: {error:#}");
                }
            }
        }
        if let Err(error) = authors::schedule_grace_if_needed(&state.db, &state.paths, hash).await {
            tracing::warn!("author grace scheduling failed: {error:#}");
        }
        // Returns the inherited avatar path, or None when there is nothing to copy.
        if let Some(avatar) = crate::uploads::inherit_author_avatar(
            &state.store,
            &state.paths,
            &state.db,
            &request.user_id,
            hash,
            request.display_name.as_deref(),
        )
        .await
        {
            tracing::info!(%avatar, "author avatar inherited on approval");
        }
    }
    if let Err(error) = accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "author.request.approve",
        Some("author_request"),
        Some(&id),
        None,
    )
    .await
    {
        tracing::warn!("author request audit failed: {error:#}");
    }
    if let Ok(Some(user)) = accounts::user_by_id(&state.db, &request.user_id).await {
        if let Some(email) = user.email {
            let message = if request.kind == "reset" {
                "你的作者 ID 重置申请已通过，请登录用户中心生成新的作者 ID。"
            } else {
                "你的作者身份申请已通过，现在可以使用账号投稿。"
            };
            mailer::queue_notice(
                &state.paths,
                &email,
                "SukiSU 作者申请审核结果",
                message,
            );
        }
    }
    Ok(())
}

async fn author_request_reject(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let reason = value
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    reject_author_request_record(&state, &actor, &id, &reason).await?;
    Ok(Json(json!({"ok": true, "message": "申请已拒绝"})))
}

async fn reject_author_request_record(
    state: &AppState,
    actor: &SessionInfo,
    id: &str,
    reason: &str,
) -> ApiResult<()> {
    let reason = reason.trim().to_string();
    if !(1..=500).contains(&reason.chars().count()) {
        return Err(ApiError::bad("请填写 1-500 字的拒绝原因"));
    }
    let request = authors::author_request_by_id(&state.db, id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("申请不存在"))?;
    if request.status != "pending" {
        return Err(ApiError::conflict("该申请已处理"));
    }
    let rejected =
        authors::reject_author_request_atomic(&state.db, id, &actor.user_id, &reason)
            .await
            .map_err(ApiError::internal)?;
    if !rejected {
        return Err(ApiError::conflict("该申请已处理"));
    }
    if let Err(error) = accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "author.request.reject",
        Some("author_request"),
        Some(&id),
        Some(&reason),
    )
    .await
    {
        tracing::warn!("author request reject audit failed: {error:#}");
    }
    if let Ok(Some(user)) = accounts::user_by_id(&state.db, &request.user_id).await {
        if let Some(email) = user.email {
            let message = if reason.is_empty() {
                "你的作者申请未通过。".to_string()
            } else {
                format!("你的作者申请未通过：{reason}")
            };
            mailer::queue_notice(
                &state.paths,
                &email,
                "SukiSU 作者申请审核结果",
                &message,
            );
        }
    }
    Ok(())
}

async fn author_request_bulk(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !matches!(action.as_str(), "approve" | "reject") {
        return Err(ApiError::bad("未知的批量操作"));
    }
    let ids: Vec<String> = value
        .get("ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut unique = Vec::with_capacity(ids.len());
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if seen.insert(id.clone()) {
            unique.push(id);
        }
    }
    if unique.is_empty() {
        return Err(ApiError::bad("没有选择要处理的申请"));
    }
    if unique.len() > 200 {
        return Err(ApiError::bad("一次最多处理 200 个申请"));
    }
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if action == "reject" && !(1..=500).contains(&reason.chars().count()) {
        return Err(ApiError::bad("请填写 1-500 字的拒绝原因"));
    }
    let mut approved = 0usize;
    let mut rejected = 0usize;
    let mut failed = Vec::new();
    for id in unique {
        let result = if action == "approve" {
            approve_author_request_record(&state, &actor, &id).await
        } else {
            reject_author_request_record(&state, &actor, &id, &reason).await
        };
        match result {
            Ok(()) => {
                if action == "approve" {
                    approved += 1;
                } else {
                    rejected += 1;
                }
            }
            Err(error) => failed.push(json!({"id": id, "error": error.detail()})),
        }
    }
    Ok(Json(json!({
        "ok": true,
        "approved": approved,
        "rejected": rejected,
        "failed": failed,
    })))
}

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    actor: Option<String>,
    action: Option<String>,
    ip: Option<String>,
}

async fn audit(
    State(state): State<AppState>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(20).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let (log, total) = accounts::audit_log(
        &state.db,
        limit,
        offset,
        query.actor.as_deref(),
        query.action.as_deref(),
        query.ip.as_deref(),
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "items": log,
        "total": total,
        "offset": offset,
        "limit": limit,
    })))
}

#[derive(Deserialize)]
struct NextThemeIdQuery {
    #[serde(rename = "authorHash")]
    author_hash: String,
}

async fn next_theme_id(
    State(state): State<AppState>,
    Query(query): Query<NextThemeIdQuery>,
) -> ApiResult<Json<Value>> {
    let hash = query.author_hash.trim().to_ascii_lowercase();
    if hash.len() != 64 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(ApiError::bad("作者 hash 无效"));
    }
    let author = authors::author_by_hash(&state.db, &hash)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("作者不存在"))?;
    let id = authors::peek_next_theme_id(&state.db, &hash)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::conflict("无法分配主题 ID"))?;
    let author = authors::author_by_hash(&state.db, &hash)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or(author);
    Ok(Json(json!({"ok": true, "id": id, "author": author})))
}

async fn set_username(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let username = value
        .get("username")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !auth::valid_admin_username(&username) {
        return Err(ApiError::bad("用户名需要 3-32 位字母、数字或 . _ -"));
    }
    let user = accounts::user_by_id(&state.db, &actor.user_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    accounts::update_profile(
        &state.db,
        &actor.user_id,
        &username,
        &user.display_name,
        None,
    )
    .await
    .map_err(|error| ApiError::conflict(error.to_string()))?;
    if user.role == "admin" {
        let _ = crate::config::set_env_key(&state.paths.admin_env, "ADMIN_USERNAME", &username);
    }
    accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "admin.username",
        Some("user"),
        Some(&actor.user_id),
        Some(&username),
    )
    .await
    .ok();
    Ok(Json(
        json!({"ok": true, "message": "用户名已更新", "username": username}),
    ))
}
