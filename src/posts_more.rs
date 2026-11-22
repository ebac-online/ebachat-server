//! The post routes outside EBAChat's use: ephemeral posts from integrations, burn-on-read, rewriting with an
//! AI agent, moving threads, scheduled posts, acknowledgements; resumable uploads; the file store test. Those
//! behind a license, a feature flag or a plugin answer as a server without them does.

// Based on Mattermost server/channels/api4/post.go, api4/scheduled_post.go, api4/upload.go, api4/system.go
// (testFileStore), app/post.go (RevealPost, BurnPost, RewriteMessage) and app/upload.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use axum::extract::{Path, State as St};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::{ApiResult, App, AppError, Auth, State, is_valid_id, new_id, now_ms};
use crate::file_api;
use crate::model::{FileInfo, Post, Session};
use crate::perm;

const CREATE_POST_EPHEMERAL: &str = "create_post_ephemeral";
const TEST_S3: &str = "test_s3";
const BURN_ON_READ: &str = "burn_on_read";

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/posts/ephemeral", post(create_ephemeral))
        .route("/api/v4/posts/rewrite", post(rewrite))
        .route("/api/v4/posts/{post_id}/burn", delete(burn_post))
        .route("/api/v4/posts/{post_id}/reveal", get(reveal_post))
        .route("/api/v4/posts/{post_id}/move", post(move_thread))
        .route("/api/v4/posts/schedule", post(scheduled_posts))
        .route("/api/v4/posts/schedule/{scheduled_post_id}", put(scheduled_posts).delete(scheduled_posts))
        .route("/api/v4/posts/scheduled/team/{team_id}", get(scheduled_posts))
        .route("/api/v4/users/{user_id}/posts/{post_id}/ack", post(ack_post).delete(unack_post))
        .route("/api/v4/uploads", post(create_upload))
        .route("/api/v4/uploads/{upload_id}", get(get_upload).post(upload_data))
        .route("/api/v4/file/test", post(test_file_store))
        .route("/api/v4/file/s3_test", post(test_file_store))
}

fn require_post_id(id: &str) -> ApiResult<()> {
    if is_valid_id(id) { Ok(()) } else { Err(AppError::invalid_url_param("post_id")) }
}

// ---------- ephemeral ----------

#[derive(Deserialize, Default)]
#[serde(default)]
struct PostEphemeral {
    user_id: String,
    post: Option<Post>,
}

/// `createEphemeralPost`: an integration's ephemeral post to one user.
async fn create_ephemeral(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Response> {
    let r: PostEphemeral =
        serde_json::from_slice(&body).map_err(|e| AppError::invalid_param("body").detail(e.to_string()))?;
    if r.user_id.is_empty() {
        return Err(AppError::invalid_param("user_id"));
    }
    let Some(mut p) = r.post else { return Err(AppError::invalid_param("post")) };
    p.user_id = s.user_id.clone();
    p.create_at = now_ms();
    app.require(&s, CREATE_POST_EPHEMERAL).await?;
    let p = app.send_ephemeral(&r.user_id, p).await;
    let p = app.sanitize_for_user(p, &s.user_id).await;
    Ok((StatusCode::CREATED, Json(p)).into_response())
}

// ---------- burn on read ----------

/// The post, readable by the session, and the session's user a member of its channel; `where_` names the
/// handler's own error for a non-member.
async fn bor_post(app: &App, s: &Session, post_id: &str, not_member: &str) -> ApiResult<Post> {
    let (p, _) = app.post_if_authorized(s, post_id, false).await?;
    if app.channel_member(&p.channel_id, &s.user_id).await?.is_none() {
        return Err(AppError::new(StatusCode::FORBIDDEN, not_member, ""));
    }
    Ok(p)
}

/// `burnPost`. Burn-on-read posts need an Enterprise Advanced license to be made, so no post here is one.
async fn burn_post(St(app): St<State>, Auth(s): Auth, Path(post_id): Path<String>) -> ApiResult<Json<Value>> {
    require_post_id(&post_id)?;
    let p = bor_post(&app, &s, &post_id, "api.post.burn_post.user_not_in_channel.app_error").await?;
    if p.r#type != BURN_ON_READ {
        return Err(AppError::bad_request("app.burn_post.not_burn_on_read.app_error", ""));
    }
    Err(AppError::not_implemented("Burning a post"))
}

/// `revealPost`: as `burnPost`, behind the `BurnOnRead` feature flag (on in 12.1).
async fn reveal_post(
    St(app): St<State>,
    Auth(s): Auth,
    headers: HeaderMap,
    Path(post_id): Path<String>,
) -> ApiResult<Json<Value>> {
    // A cookie session must say it is the web app's XHR.
    if !headers.contains_key(header::AUTHORIZATION)
        && headers.get("X-Requested-With").and_then(|v| v.to_str().ok()) != Some("XMLHttpRequest")
    {
        return Err(AppError::new(StatusCode::FORBIDDEN, "api.post.reveal_post.invalid_request.app_error", ""));
    }
    require_post_id(&post_id)?;
    if !app.cfg.feature("BurnOnRead") {
        return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.post.reveal_post.disabled.app_error", ""));
    }
    let p = bor_post(&app, &s, &post_id, "api.post.reveal_post.user_not_in_channel.app_error").await?;
    if p.user_id == s.user_id {
        return Err(AppError::bad_request("api.post.reveal_post.cannot_reveal_own_post.app_error", ""));
    }
    if p.r#type != BURN_ON_READ {
        return Err(AppError::bad_request("app.reveal_post.not_burn_on_read.app_error", ""));
    }
    Err(AppError::not_implemented("Revealing a post"))
}

// ---------- rewrite ----------

#[derive(Deserialize, Default)]
#[serde(default)]
struct RewriteRequest {
    agent_id: String,
    message: String,
    action: String,
    custom_prompt: String,
    root_id: String,
}

/// `rewriteMessage`: through the AI plugin's bridge, which this server does not have.
async fn rewrite(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    let r: RewriteRequest =
        serde_json::from_slice(&body).map_err(|e| AppError::invalid_param("request_body").detail(e.to_string()))?;
    if !is_valid_id(&r.agent_id) {
        return Err(AppError::invalid_param("agent_id"));
    }
    if !r.root_id.is_empty() && !is_valid_id(&r.root_id) {
        return Err(AppError::invalid_param("root_id"));
    }
    if !r.root_id.is_empty() {
        // buildThreadContextForRewrite reads the thread as the session may.
        app.post_if_authorized(&s, &r.root_id, false).await?;
    }
    let known = ["custom", "shorten", "elaborate", "improve_writing", "fix_spelling", "simplify", "summarize"];
    if !r.message.is_empty() && !known.contains(&r.action.as_str()) {
        return Err(AppError::bad_request("app.post.rewrite.invalid_action", "")
            .detail(format!("invalid action: {}", r.action)));
    }
    let _ = (r.custom_prompt, s);
    Err(AppError::internal("app.post.rewrite.agent_call_failed", "the AI plugin is not available"))
}

// ---------- licensed or behind flags ----------

/// `moveThread`: needs the `MoveThreadsEnabled` flag and a license.
async fn move_thread(_: Auth, Path(post_id): Path<String>) -> ApiResult<Json<Value>> {
    require_post_id(&post_id)?;
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.post.move_thread.disabled.app_error", ""))
}

/// `requireScheduledPostsEnabled`, first in every scheduled post route: the setting, then the license.
async fn scheduled_posts(St(app): St<State>, _: Auth) -> AppError {
    if app.cfg.get("ServiceSettings.ScheduledPosts").as_bool() == Some(false) {
        return AppError::bad_request("api.scheduled_posts.feature_disabled", "");
    }
    AppError::bad_request("api.scheduled_posts.license_error", "")
}

/// `acknowledgePost`: its license error is untranslated.
async fn ack_post(_: Auth) -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "<untranslated>", "<untranslated>")
}

async fn unack_post(_: Auth) -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "license_error.feature_unavailable", "")
}

// ---------- uploads ----------

/// `model.UploadSession`, in Go's order; `Path` never leaves the server.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
struct UploadSession {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    create_at: i64,
    user_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    channel_id: String,
    filename: String,
    #[serde(skip)]
    path: String,
    file_size: i64,
    file_offset: i64,
    remote_id: String,
    req_file_id: String,
}

const ATTACHMENT: &str = "attachment";
const IMPORT: &str = "import";
/// `model.IncompleteUploadSuffix`.
const INCOMPLETE: &str = ".tmp";
/// `minFirstPartSize`.
const MIN_FIRST_PART: i64 = 5 * 1024 * 1024;

/// The uploads being written to, as Go's `uploadLockMap`.
static UPLOADING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn attachments_disabled() -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "api.file.attachments.disabled.app_error", "")
}

fn is_system_admin(s: &Session) -> bool {
    s.roles.split_whitespace().any(|r| r == perm::SYSTEM_ADMIN)
}

/// `filepath.Base`.
fn base(p: &str) -> String {
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        return if p.is_empty() { ".".into() } else { "/".into() };
    }
    t.rsplit('/').next().unwrap_or(t).to_string()
}

/// `checkUploadFileAttachmentPermission`.
async fn check_upload_perm(app: &App, s: &Session, channel_id: &str) -> ApiResult<()> {
    if !app.channel_perm(s, channel_id, perm::UPLOAD_FILE).await?.0 {
        return Err(AppError::forbidden_perm(perm::UPLOAD_FILE));
    }
    Ok(())
}

/// An `UploadSessions` row, its columns in `UploadSession`'s order (with `Path`).
type SessionRow = (String, String, i64, String, String, String, String, i64, i64, String, String);

impl App {
    async fn upload_session(&self, id: &str) -> ApiResult<UploadSession> {
        let row: Option<SessionRow> = sqlx::query_as(
            "SELECT Id, COALESCE(Type::text, ''), COALESCE(CreateAt, 0), COALESCE(UserId, ''), COALESCE(ChannelId, ''),
                    COALESCE(Filename, ''), COALESCE(Path, ''), COALESCE(FileSize, 0), COALESCE(FileOffset, 0),
                    COALESCE(RemoteId, ''), COALESCE(ReqFileId, '')
             FROM UploadSessions WHERE Id = $1",
        )
        .bind(id)
        .fetch_optional(&self.db)
        .await
        .map_err(|e| AppError::internal("app.upload.get.app_error", e.to_string()))?;
        let Some(r) = row else { return Err(AppError::not_found("app.upload.get.app_error", "")) };
        Ok(UploadSession {
            id: r.0,
            kind: r.1,
            create_at: r.2,
            user_id: r.3,
            channel_id: r.4,
            filename: r.5,
            path: r.6,
            file_size: r.7,
            file_offset: r.8,
            remote_id: r.9,
            req_file_id: r.10,
        })
    }
}

/// `createUpload`: a session to send a file's bytes in parts.
async fn create_upload(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Response> {
    if !app.cfg.bool("FileSettings.EnableFileAttachments") {
        return Err(attachments_disabled());
    }
    let mut us: UploadSession =
        serde_json::from_slice(&body).map_err(|e| AppError::invalid_param("upload").detail(e.to_string()))?;
    us.remote_id.clear();
    us.req_file_id.clear();
    us.filename = base(&us.filename);
    if us.kind == IMPORT {
        if !is_system_admin(&s) {
            return Err(AppError::forbidden_perm(perm::MANAGE_SYSTEM));
        }
        let import = app.cfg.str("ImportSettings.Directory").trim_end_matches('/').to_string();
        let plugins = app.cfg.str("PluginSettings.Directory").trim_end_matches('/').to_string();
        if !import.is_empty() && import == plugins {
            return Err(AppError::new(StatusCode::FORBIDDEN, "api.upload.create.directory_conflict.app_error", ""));
        }
    } else {
        check_upload_perm(&app, &s, &us.channel_id).await?;
        us.kind = ATTACHMENT.into();
    }
    us.id = new_id();
    us.user_id = s.user_id.clone();
    if us.file_size > app.cfg.i64("FileSettings.MaxFileSize") {
        return Err(AppError::new(StatusCode::PAYLOAD_TOO_LARGE, "api.upload.create.upload_too_large.app_error", "")
            .with_param("channelId", &us.channel_id));
    }

    // CreateUploadSession
    us.file_offset = 0;
    us.create_at = now_ms();
    if us.kind == ATTACHMENT {
        let day = chrono::DateTime::from_timestamp_millis(us.create_at)
            .map(|t| t.with_timezone(&chrono::Local).format("%Y%m%d").to_string())
            .unwrap_or_default();
        us.path = format!(
            "{day}/teams/noteam/channels/{}/users/{}/{}/{}",
            us.channel_id,
            us.user_id,
            us.id,
            base(&us.filename)
        );
    } else {
        let dir = app.cfg.str("ImportSettings.Directory").trim_end_matches('/').to_string();
        // filepath.Join: `./import` becomes `import`.
        us.path = crate::files::clean(&format!("{dir}/{}_{}", us.id, base(&us.filename)));
    }
    let invalid = |id: &str| Err(AppError::bad_request(id, ""));
    if us.kind == ATTACHMENT && !is_valid_id(&us.channel_id) {
        return invalid("model.upload_session.is_valid.channel_id.app_error");
    }
    if us.filename.is_empty() {
        return invalid("model.upload_session.is_valid.filename.app_error");
    }
    if us.file_size <= 0 {
        return invalid("model.upload_session.is_valid.file_size.app_error");
    }
    if us.kind == ATTACHMENT {
        let bad = |id: &str| AppError::bad_request(id, "").with_param("channelId", &us.channel_id);
        let c = app
            .channel(&us.channel_id)
            .await?
            .ok_or_else(|| bad("app.upload.create.incorrect_channel_id.app_error"))?;
        if c.delete_at != 0 {
            return Err(bad("app.upload.create.cannot_upload_to_deleted_channel.app_error"));
        }
        if app.restricted_dm(&c).await? {
            return Err(AppError::bad_request("app.upload.create.cannot_upload_to_restricted_dm.error", ""));
        }
    }
    sqlx::query(
        "INSERT INTO UploadSessions (Id, Type, CreateAt, UserId, ChannelId, Filename, Path, FileSize, FileOffset,
             RemoteId, ReqFileId)
         VALUES ($1, $2::upload_session_type, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(&us.id)
    .bind(&us.kind)
    .bind(us.create_at)
    .bind(&us.user_id)
    .bind(&us.channel_id)
    .bind(&us.filename)
    .bind(&us.path)
    .bind(us.file_size)
    .bind(us.file_offset)
    .bind(&us.remote_id)
    .bind(&us.req_file_id)
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.upload.create.save.app_error", e.to_string()))?;
    Ok((StatusCode::CREATED, Json(us)).into_response())
}

/// `getUpload`: the uploader's own, or any for a system admin.
async fn get_upload(
    St(app): St<State>,
    Auth(s): Auth,
    Path(upload_id): Path<String>,
) -> ApiResult<Json<UploadSession>> {
    if !is_valid_id(&upload_id) {
        return Err(AppError::invalid_url_param("upload_id"));
    }
    let us = app.upload_session(&upload_id).await?;
    if us.user_id != s.user_id && !is_system_admin(&s) {
        return Err(AppError::new(StatusCode::FORBIDDEN, "api.upload.get_upload.forbidden.app_error", ""));
    }
    Ok(Json(us))
}

/// `uploadData`: the next part of the file, raw or the first part of a multipart body. 204 until the file is
/// whole, then its `FileInfo`.
async fn upload_data(
    St(app): St<State>,
    Auth(s): Auth,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> ApiResult<Response> {
    if !app.cfg.bool("FileSettings.EnableFileAttachments") {
        return Err(attachments_disabled());
    }
    if !is_valid_id(&upload_id) {
        return Err(AppError::invalid_url_param("upload_id"));
    }
    let us = app.upload_session(&upload_id).await?;
    if us.kind == IMPORT {
        if !is_system_admin(&s) {
            return Err(AppError::forbidden_perm(perm::MANAGE_SYSTEM));
        }
    } else {
        if us.user_id != s.user_id {
            return Err(AppError::forbidden_perm(perm::UPLOAD_FILE));
        }
        check_upload_perm(&app, &s, &us.channel_id).await?;
    }

    // doUploadData: the part, at most what is left of the file.
    let left = (us.file_size - us.file_offset).max(0) as usize;
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let data: Vec<u8> = if ct.starts_with("multipart/") {
        let boundary = multer::parse_boundary(ct).map_err(|e| {
            AppError::bad_request("api.upload.upload_data.invalid_content_type", "").detail(e.to_string())
        })?;
        let mut mp = multer::Multipart::new(body.into_data_stream(), boundary);
        let multipart_error = |e: String| AppError::bad_request("api.upload.upload_data.multipart_error", "").detail(e);
        let mut field = mp
            .next_field()
            .await
            .map_err(|e| multipart_error(e.to_string()))?
            .ok_or_else(|| multipart_error("EOF".into()))?;
        read_limited(&mut field, left).await.map_err(multipart_error)?
    } else {
        let len: Option<usize> =
            headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
        if len.is_some_and(|l| l > left) {
            return Err(AppError::bad_request("api.upload.upload_data.invalid_content_length", ""));
        }
        let bytes = axum::body::to_bytes(body, left.saturating_add(1)).await.map_err(|e| {
            AppError::bad_request("api.upload.upload_data.invalid_content_length", "").detail(e.to_string())
        })?;
        bytes[..bytes.len().min(left)].to_vec()
    };

    // UploadData, one writer at a time.
    if !UPLOADING.lock().unwrap().insert(us.id.clone()) {
        return Err(AppError::bad_request("app.upload.upload_data.concurrent.app_error", ""));
    }
    let r = write_part(&app, us, data).await;
    UPLOADING.lock().unwrap().remove(&upload_id);
    match r? {
        None => Ok(StatusCode::NO_CONTENT.into_response()),
        Some(info) => Ok(Json(info).into_response()),
    }
}

async fn read_limited(field: &mut multer::Field<'_>, max: usize) -> Result<Vec<u8>, String> {
    let mut out = vec![];
    while let Some(chunk) = field.chunk().await.map_err(|e| e.to_string())? {
        let take = chunk.len().min(max - out.len());
        out.extend_from_slice(&chunk[..take]);
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

/// The part written (the first creates the file, the next ones append); when the file is whole, its info made
/// and saved as an upload's, and the session deleted.
async fn write_part(app: &App, mut us: UploadSession, data: Vec<u8>) -> ApiResult<Option<FileInfo>> {
    let stored = app.upload_session(&us.id).await?;
    if stored.file_offset != us.file_offset {
        return Err(
            AppError::bad_request("app.upload.upload_data.concurrent.app_error", "").detail("FileOffset mismatch")
        );
    }
    let upload_path = if us.kind == IMPORT { format!("{}{INCOMPLETE}", us.path) } else { us.path.clone() };
    let written = data.len() as i64;
    let storage = |e: anyhow::Error| AppError::internal("api.file.write_file.app_error", e.to_string());
    if us.file_offset == 0 {
        if written < MIN_FIRST_PART && written != us.file_size {
            return Err(AppError::bad_request("app.upload.upload_data.first_part_too_small.app_error", "")
                .with_param("Size", &MIN_FIRST_PART.to_string()));
        }
        app.files.write(&upload_path, data.into()).await.map_err(storage)?;
    } else if us.file_offset < us.file_size {
        // AppendFile: the store has no append; the file so far and the part, written again.
        let mut all = app.files.read(&upload_path).await.map_err(storage)?.to_vec();
        all.extend_from_slice(&data);
        app.files.write(&upload_path, all.into()).await.map_err(storage)?;
    }
    if written > 0 {
        us.file_offset += written;
        sqlx::query("UPDATE UploadSessions SET FileOffset = $1 WHERE Id = $2")
            .bind(us.file_offset)
            .bind(&us.id)
            .execute(&app.db)
            .await
            .map_err(|e| AppError::internal("app.upload.upload_data.update.app_error", e.to_string()))?;
    }
    if us.file_offset != us.file_size {
        return Ok(None);
    }

    // genFileInfoFromReader
    let name = sanitize_filename(&us.filename);
    if name.is_empty() {
        return Err(AppError::bad_request("app.upload.gen_file_info.invalid_filename.app_error", ""));
    }
    let ext = name.rfind('.').map(|i| name[i..].to_lowercase()).unwrap_or_default();
    let mut info = FileInfo {
        id: if us.req_file_id.is_empty() { new_id() } else { us.req_file_id.clone() },
        creator_id: us.user_id.clone(),
        channel_id: us.channel_id.clone(),
        create_at: now_ms(),
        name: name.clone(),
        extension: ext.trim_start_matches('.').to_string(),
        mime_type: file_api::mime_type(&ext),
        size: us.file_size,
        path: us.path.clone(),
        remote_id: Some(us.remote_id.clone()),
        ..Default::default()
    };
    info.update_at = info.create_at;
    // Go's UploadData makes the preview and thumbnail but never sets `HasPreviewImage`, so clients never show
    // them; fixed here: the image is processed as a plain upload's (`has_preview_image` set, mini preview).
    if info.mime_type.starts_with("image") {
        let bytes = app.files.read(&upload_path).await.map_err(storage)?.to_vec();
        let dir = &us.path[..us.path.rfind('/').map(|i| i + 1).unwrap_or(0)];
        if let Err(e) = file_api::process_image(app, &mut info, &name, dir, bytes).await {
            return Err(if e.id == "api.file.upload_file.large_image_detailed.app_error" {
                AppError::bad_request("app.upload.upload_data.large_image.app_error", "").with_params(&[
                    ("Filename", &us.filename),
                    ("Width", &info.width.to_string()),
                    ("Height", &info.height.to_string()),
                ])
            } else {
                e
            });
        }
    }
    if us.kind == IMPORT {
        let bytes = app.files.read(&upload_path).await.map_err(storage)?;
        app.files
            .write(&us.path, bytes)
            .await
            .map_err(|e| AppError::internal("app.upload.upload_data.move_file.app_error", e.to_string()))?;
        let _ = app.files.remove(&upload_path).await;
    }
    file_api::save_file_info(app, &info).await?;
    // UploadData: the text extracted in the background (`FileSettings.ExtractContent`), for an import upload too.
    if app.cfg.bool("FileSettings.ExtractContent")
        && let Some(a) = app.hub.app()
    {
        let (id, name, path, mime) = (info.id.clone(), info.name.clone(), info.path.clone(), info.mime_type.clone());
        tokio::spawn(async move {
            let _ = a.extract_content_from_file_info(&id, &name, &path, &mime, "").await;
        });
    }
    let _ = sqlx::query("DELETE FROM UploadSessions WHERE Id = $1").bind(&us.id).execute(&app.db).await;
    Ok(Some(info))
}

/// `model.SanitizeFilename`: the base name, without path separators or control characters.
fn sanitize_filename(name: &str) -> String {
    let b = base(name);
    let b: String = b.chars().filter(|c| !c.is_control() && *c != '/' && *c != '\\').collect();
    if b == "." || b == ".." { String::new() } else { b }
}

// ---------- file store test ----------

/// `testFileStore` (`/file/test` and `/file/s3_test`): the given file settings, or the server's, tried by
/// writing and removing a test file.
async fn test_file_store(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    let given: Option<Map<String, Value>> = serde_json::from_slice::<Option<Map<String, Value>>>(&body).ok().flatten();
    app.require(&s, TEST_S3).await?;
    let server = app.cfg.get("FileSettings").as_object().cloned().unwrap_or_default();
    let mut fs = match &given {
        None => server.clone(),
        Some(cfg) => cfg.get("FileSettings").and_then(Value::as_object).cloned().unwrap_or_default(),
    };
    // checkHasNilFields: every setting the server has must be given.
    if server.keys().any(|k| fs.get(k).is_none_or(Value::is_null)) {
        return Err(AppError::bad_request("api.file.test_connection_settings_nil.app_error", ""));
    }
    let str_of = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let driver = str_of(&fs, "DriverName");
    match driver.as_str() {
        "local" => {}
        "amazons3" => {
            if str_of(&fs, "AmazonS3Bucket").is_empty() {
                return Err(AppError::bad_request("api.admin.test_s3.missing_s3_bucket", ""));
            }
        }
        "azure" => {
            if str_of(&fs, "AzureStorageAccount").is_empty() || str_of(&fs, "AzureContainer").is_empty() {
                return Err(AppError::bad_request("api.admin.test_azure.missing_azure_field", ""));
            }
        }
        _ => {
            return Err(AppError::bad_request("api.file.test_connection_unsupported_driver.app_error", "")
                .with_param("Driver", &driver));
        }
    }
    // config.Desanitize: the secrets the admin console sends back masked.
    for k in ["AmazonS3SecretAccessKey", "AzureAccessKey", "PublicLinkSalt"] {
        if fs.get(k).and_then(Value::as_str) == Some("********************************")
            && let Some(v) = server.get(k)
        {
            fs.insert(k.into(), v.clone());
        }
    }
    if driver == "azure" {
        return Err(AppError::internal("api.file.no_driver.app_error", "the azure driver is not available"));
    }
    let cfg = crate::config::Config::from_value(json!({ "FileSettings": fs }));
    let store = crate::files::FileStore::new(&cfg)
        .map_err(|e| AppError::internal("api.file.no_driver.app_error", e.to_string()))?;
    let test = |e: anyhow::Error| AppError::internal("api.file.test_connection.app_error", e.to_string());
    store.write("testfile", bytes::Bytes::from_static(b"testingwrite")).await.map_err(test)?;
    let _ = store.remove("testfile").await;
    Ok(crate::channels_api::status_ok())
}
