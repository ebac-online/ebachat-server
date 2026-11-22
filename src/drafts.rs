//! Drafts synced between a user's devices.

// Based on Mattermost server/channels/api4/drafts.go, app/draft.go and store/sqlstore/draft_store.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use axum::extract::{Path, State as St};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, Body, State, is_valid_id, now_ms};
use crate::db::{self, Table};
use crate::hub::Broadcast;
use crate::model::{Draft, FileInfo, PostMetadata, Session};
use crate::perm;
use crate::users::ok;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/drafts", post(upsert_draft))
        .route("/api/v4/users/{user_id}/teams/{team_id}/drafts", get(get_drafts))
        .route("/api/v4/users/{user_id}/channels/{channel_id}/drafts", delete(delete_draft))
        .route("/api/v4/users/{user_id}/channels/{channel_id}/drafts/{thread_id}", delete(delete_draft_in_thread))
}

fn disabled() -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "api.drafts.disabled.app_error", "Drafts are disabled.")
}

fn connection_id(h: &HeaderMap) -> String {
    h.get("Connection-Id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

impl App {
    /// `prepareDraftWithFileInfos`: the user's unattached files of the draft, as its metadata.
    async fn draft_with_files(&self, mut d: Draft) -> Draft {
        if d.file_ids.0.is_empty() {
            d.metadata = Some(serde_json::json!({}));
            return d;
        }
        let sql = format!("{} WHERE Id = ANY($1) AND DeleteAt = 0", FileInfo::SELECT);
        let infos: Vec<FileInfo> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&d.file_ids.0).fetch_all(&self.db).await.unwrap_or_default();
        let files: Vec<FileInfo> =
            infos.into_iter().filter(|f| f.post_id.is_empty() && f.creator_id == d.user_id).collect();
        let md = PostMetadata { files, ..Default::default() };
        d.metadata = Some(serde_json::to_value(md).unwrap());
        d
    }
}

async fn upsert_draft(
    St(app): St<State>,
    Auth(s): Auth,
    headers: HeaderMap,
    Body(d): Body<Draft>,
) -> ApiResult<Response> {
    if !app.cfg.bool("ServiceSettings.AllowSyncedDrafts") {
        return Err(disabled());
    }
    let mut d = d;
    d.delete_at = 0;
    d.user_id = s.user_id.clone();
    crate::posts::check_create_post(&app, &s, &d.channel_id).await?;
    // UpsertDraft
    let c = app.channel(&d.channel_id).await?.ok_or_else(|| AppError::invalid_param("draft.channel_id"))?;
    if c.delete_at != 0 {
        return Err(AppError::bad_request(
            "api.draft.create_draft.can_not_draft_to_deleted.error",
            "Can not create a draft in a deleted channel.",
        ));
    }
    if d.message.is_empty() {
        sqlx::query("DELETE FROM Drafts WHERE UserId = $1 AND ChannelId = $2 AND RootId = $3")
            .bind(&d.user_id)
            .bind(&d.channel_id)
            .bind(&d.root_id)
            .execute(&app.db)
            .await?;
        return Ok((StatusCode::CREATED, Json(Value::Null)).into_response());
    }
    // Draft.PreSave
    let now = now_ms();
    if d.create_at == 0 {
        d.create_at = now;
    }
    d.update_at = now;
    let mut seen = std::collections::HashSet::new();
    d.file_ids.0.retain(|f| seen.insert(f.clone()));
    if d.message.chars().count() > app.max_post_size().await as usize {
        return Err(AppError::bad_request(
            "model.draft.is_valid.message_length.app_error",
            "Invalid Message property length.",
        ));
    }
    if !is_valid_id(&d.channel_id) {
        return Err(AppError::bad_request("model.draft.is_valid.channel_id.app_error", "Invalid channel id."));
    }
    if !(d.root_id.is_empty() || is_valid_id(&d.root_id)) {
        return Err(AppError::bad_request("model.draft.is_valid.root_id.app_error", "Invalid root id."));
    }
    sqlx::query(
        "INSERT INTO Drafts (CreateAt, UpdateAt, DeleteAt, UserId, ChannelId, RootId, Message, Props, FileIds, Priority, Type)
         VALUES ($1, $2, 0, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (UserId, ChannelId, RootId) DO UPDATE SET UpdateAt = $2, Message = $6, Props = $7, FileIds = $8,
             Priority = $9, Type = $10, DeleteAt = 0",
    )
    .bind(d.create_at)
    .bind(d.update_at)
    .bind(&d.user_id)
    .bind(&d.channel_id)
    .bind(&d.root_id)
    .bind(&d.message)
    .bind(&d.props)
    .bind(&d.file_ids)
    .bind(&d.priority)
    .bind(&d.r#type)
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.draft.save.app_error", e.to_string()))?;
    let d = app.draft_with_files(d).await;
    app.hub.publish(
        "draft_created",
        json!({"draft": serde_json::to_string(&d).unwrap()}),
        Broadcast {
            channel_id: d.channel_id.clone(),
            user_id: d.user_id.clone(),
            omit_connection_id: connection_id(&headers),
            ..Default::default()
        },
    );
    Ok((StatusCode::CREATED, Json(d)).into_response())
}

async fn get_drafts(
    St(app): St<State>,
    Auth(s): Auth,
    Path((_user_id, team_id)): Path<(String, String)>,
) -> ApiResult<Json<Option<Vec<Draft>>>> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    if !app.cfg.bool("ServiceSettings.AllowSyncedDrafts") {
        return Err(disabled());
    }
    if !app.has_team_perm(&s, &team_id, perm::VIEW_TEAM).await {
        return Err(AppError::forbidden_perm(perm::CREATE_POST));
    }
    let sql = format!(
        "SELECT {} FROM Drafts d JOIN ChannelMembers cm ON cm.ChannelId = d.ChannelId JOIN Channels c ON c.Id = d.ChannelId
         WHERE d.DeleteAt = 0 AND d.UserId = $1 AND cm.UserId = $1 AND (c.TeamId = $2 OR c.TeamId = '')
         ORDER BY d.UpdateAt DESC",
        db::cols::<Draft>("d")
    );
    let drafts: Vec<Draft> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&s.user_id)
        .bind(&team_id)
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("app.draft.get_drafts.app_error", e.to_string()))?;
    let mut out = Vec::with_capacity(drafts.len());
    for d in drafts {
        out.push(app.draft_with_files(d).await);
    }
    // A nil slice in Go: `null` when there are none.
    Ok(Json((!out.is_empty()).then_some(out)))
}

async fn remove_draft(
    app: &App,
    s: &Session,
    headers: &HeaderMap,
    channel_id: &str,
    root_id: &str,
) -> ApiResult<Json<Value>> {
    if !is_valid_id(channel_id) {
        return Err(AppError::invalid_url_param("channel_id"));
    }
    if !app.cfg.bool("ServiceSettings.AllowSyncedDrafts") {
        return Err(disabled());
    }
    let sql = format!("{} WHERE UserId = $1 AND ChannelId = $2 AND RootId = $3 AND DeleteAt = 0", Draft::SELECT);
    let d: Option<Draft> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&s.user_id)
        .bind(channel_id)
        .bind(root_id)
        .fetch_optional(&app.db)
        .await?;
    let Some(d) = d else { return Ok(ok()) };
    sqlx::query("DELETE FROM Drafts WHERE UserId = $1 AND ChannelId = $2 AND RootId = $3")
        .bind(&d.user_id)
        .bind(&d.channel_id)
        .bind(&d.root_id)
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("app.draft.delete.app_error", e.to_string()))?;
    app.hub.publish(
        "draft_deleted",
        json!({"draft": serde_json::to_string(&d).unwrap()}),
        Broadcast {
            channel_id: d.channel_id.clone(),
            user_id: d.user_id.clone(),
            omit_connection_id: connection_id(headers),
            ..Default::default()
        },
    );
    Ok(ok())
}

async fn delete_draft(
    St(app): St<State>,
    Auth(s): Auth,
    headers: HeaderMap,
    Path((_user_id, channel_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    remove_draft(&app, &s, &headers, &channel_id, "").await
}

async fn delete_draft_in_thread(
    St(app): St<State>,
    Auth(s): Auth,
    headers: HeaderMap,
    Path((_user_id, channel_id, thread_id)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    remove_draft(&app, &s, &headers, &channel_id, &thread_id).await
}
