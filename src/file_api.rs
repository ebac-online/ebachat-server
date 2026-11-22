//! Files: uploads (thumbnails, previews, mini previews), downloads with ranges, public links, search, the file
//! infos of posts through the FileInfo cache, and link metadata for post embeds.

// Based on Mattermost server/channels/api4/file.go, api4/image.go, app/file.go, app/post.go (attachFilesToPost,
// GetFileInfosForPost), store/sqlstore/file_info_store.go, store/localcachelayer/file_info_layer.go and
// platform/shared/web/files.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, OnceLock};

use crate::app::Query;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State as St};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::{ApiResult, App, AppError, Auth, BusyAuth, State, is_valid_id, new_id, now_ms};
use crate::model::channel::{DIRECT, GROUP, OPEN, OPEN_BOARD};
use crate::model::{Base64, Channel, FileInfo, Session};
use crate::search_params::{self, SearchParams};
use crate::{db, images, perm};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/files", post(upload))
        .route("/api/v4/files/search", post(search_all_teams))
        .route("/api/v4/files/{file_id}", get(get_file))
        .route("/api/v4/files/{file_id}/thumbnail", get(get_thumbnail))
        .route("/api/v4/files/{file_id}/preview", get(get_preview))
        .route("/api/v4/files/{file_id}/link", get(get_link))
        .route("/api/v4/files/{file_id}/info", get(get_info))
        .route("/files/{file_id}/public", get(get_public))
        .route("/api/v4/teams/{team_id}/files/search", post(search_in_team))
        .route("/api/v4/posts/{post_id}/files/info", get(post_file_infos))
        .route("/api/v4/image", get(get_image))
}

/// What a link in a message gives the post's metadata: an OpenGraph preview, or an image's size.
#[derive(Clone, Debug, Default)]
pub struct LinkMeta {
    /// Go's `opengraph.OpenGraph` after `TruncateOpenGraph`.
    pub opengraph: Option<crate::opengraph::OpenGraph>,
    /// `PostImage`: `{"width", "height", "format", "frame_count"}`.
    pub image: Option<crate::opengraph::PostImage>,
    /// A permalink's preview (built by the posts code, kept in the same link cache).
    pub permalink: Option<crate::model::post::PreviewPost>,
    /// The URL it was fetched for (the link cache's check against hash collisions).
    pub url: String,
}

const NO_TEAM: &str = "noteam";
const BOOKMARK_OWNER: &str = "bookmark";
const THUMBNAIL_TYPE: &str = "image/jpeg";

// ---------- the FileInfo store ----------

/// `SqlFileInfoStore.queryFields`: NULL channel, content and remote id read as "".
fn select() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| {
        let cols: Vec<String> = <FileInfo as db::Table>::COLUMNS
            .iter()
            .map(|c| match *c {
                "channelid" | "content" | "remoteid" => format!("COALESCE({c}, '') AS {c}"),
                c => c.to_string(),
            })
            .collect();
        format!("SELECT {} FROM FileInfo", cols.join(", "))
    })
}

fn get_info_error() -> AppError {
    AppError::not_found("app.file_info.get.app_error", "Unable to get the file info.")
}

impl App {
    /// `FileInfoStore.Get`: not deleted, not cached.
    pub async fn file_info(&self, id: &str) -> ApiResult<Option<FileInfo>> {
        let sql = format!("{} WHERE Id = $1 AND DeleteAt = 0", select());
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(id).fetch_optional(&self.db).await?)
    }

    /// `FileInfoStore.GetByIds` through the cache (`{id}_{includeDeleted}`), newest first like the SQL, as Go's
    /// `fileInfoWithChannelID` gives them (no `archived`).
    pub async fn file_infos_by_ids(&self, ids: &[String], include_deleted: bool) -> ApiResult<Vec<FileInfo>> {
        let mut out = vec![];
        let mut fetch = vec![];
        for id in ids {
            match self.caches.file_infos.get(&format!("{id}_{include_deleted}")) {
                Some(v) => out.extend(v.iter().cloned()),
                None => fetch.push(id.clone()),
            }
        }
        if !fetch.is_empty() {
            let mut sql = format!("{} WHERE Id = ANY($1)", select());
            if !include_deleted {
                sql += " AND DeleteAt = 0";
            }
            sql += " ORDER BY CreateAt DESC";
            let found: Vec<FileInfo> =
                sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&fetch).fetch_all(&self.db).await?;
            for mut f in found {
                f.archived = false;
                self.caches.file_infos.put(format!("{}_{include_deleted}", f.id), Arc::new(vec![f.clone()]));
                out.push(f);
            }
        }
        Ok(out)
    }

    /// `FileInfoStore.GetForPost` through the file info cache (`{post}` / `{post}_deleted`, only when found).
    pub async fn file_infos_for_post(&self, post_id: &str, include_deleted: bool) -> ApiResult<Arc<Vec<FileInfo>>> {
        let key = if include_deleted { format!("{post_id}_deleted") } else { post_id.to_string() };
        if let Some(v) = self.caches.file_infos.get(&key) {
            return Ok(v);
        }
        let mut sql = format!("{} WHERE PostId = $1", select());
        if !include_deleted {
            sql += " AND DeleteAt = 0";
        }
        sql += " ORDER BY CreateAt";
        let found: Vec<FileInfo> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(post_id).fetch_all(&self.db).await?;
        let found = Arc::new(found);
        if !found.is_empty() {
            self.caches.file_infos.put(key, found.clone());
        }
        Ok(found)
    }

    /// `App.GetFileInfosForPost`: the post's `file_ids` through the by-id cache, in the post's order, with mini
    /// previews made for images that have none. The posts code builds `metadata.files` with it. The released 12.1.0
    /// binary skips `orderFileInfosByID`: its order is `GetByIds`' newest first with cache hits interleaved, so the
    /// attachments come back in an order that changes from one read to the next; the post's own order is kept here.
    pub async fn file_infos_for_ids(&self, ids: &[String], include_deleted: bool) -> ApiResult<Vec<FileInfo>> {
        let infos = self.file_infos_by_ids(ids, include_deleted).await?;
        let mut by_id: HashMap<String, FileInfo> = infos.into_iter().map(|f| (f.id.clone(), f)).collect();
        let mut ordered: Vec<FileInfo> = ids.iter().filter_map(|id| by_id.remove(id)).collect();
        for f in ordered.iter_mut() {
            self.ensure_mini_preview(f).await;
        }
        Ok(ordered)
    }

    /// `InvalidateFileInfosForPostCache`, both variants, and the files' own entries.
    pub fn invalidate_file_infos(&self, post_id: &str, file_ids: &[String]) {
        self.caches.file_infos.remove(post_id);
        self.caches.file_infos.remove(&format!("{post_id}_deleted"));
        for id in file_ids {
            self.caches.file_infos.remove(&format!("{id}_true"));
            self.caches.file_infos.remove(&format!("{id}_false"));
        }
    }

    /// `attachFileIDsToPost`: each of the uploader's (or `nouser`'s) unattached files gets the post and channel;
    /// returns the ids attached. Updating the post's `file_ids` when some failed is the caller's (`Overwrite`).
    pub async fn attach_files_to_post(
        &self,
        post_id: &str,
        channel_id: &str,
        user_id: &str,
        file_ids: &[String],
    ) -> ApiResult<Vec<String>> {
        let mut attached = vec![];
        for id in file_ids {
            let r = sqlx::query(
                "UPDATE FileInfo SET PostId = $1, ChannelId = $2
                 WHERE Id = $3 AND PostId = '' AND (CreatorId = $4 OR CreatorId = 'nouser')",
            )
            .bind(post_id)
            .bind(channel_id)
            .bind(id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
            if r.rows_affected() > 0 {
                attached.push(id.clone());
            }
        }
        self.invalidate_file_infos(post_id, file_ids);
        Ok(attached)
    }

    /// `generateMiniPreview`: an image without one gets it from the file, saved (`Upsert`).
    async fn ensure_mini_preview(&self, f: &mut FileInfo) {
        if !f.mime_type.starts_with("image") || f.mime_type == "image/svg+xml" || f.mini_preview.is_some() {
            return;
        }
        // Go tries again on every read; an image that cannot be decoded is not read whole again and again here.
        static FAILED: LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = LazyLock::new(Default::default);
        if FAILED.lock().unwrap().contains(&f.id) {
            return;
        }
        let failed = || {
            let mut s = FAILED.lock().unwrap();
            if s.len() > 10_000 {
                s.clear();
            }
            s.insert(f.id.clone());
        };
        let Ok(data) = self.files.read(&f.path).await else {
            failed();
            return;
        };
        let mini = tokio::task::spawn_blocking(move || {
            images::decode_upright(&data).and_then(|(img, _)| images::mini_preview(&img))
        })
        .await
        .ok()
        .flatten();
        let Some(mini) = mini else {
            failed();
            return;
        };
        f.mini_preview = Some(Base64(mini));
        if sqlx::query("UPDATE FileInfo SET MiniPreview = $1 WHERE Id = $2")
            .bind(&f.mini_preview)
            .bind(&f.id)
            .execute(&self.db)
            .await
            .is_ok()
        {
            // Go invalidates the post's entry only; the file's own entries would keep serving it without one.
            self.invalidate_file_infos(&f.post_id, std::slice::from_ref(&f.id));
        }
    }

    /// `getLinkMetadata` for a new post now. The posts code should prefer [`App::link_metadata_at`] with the
    /// post's `create_at` and whether it is new (old posts read the `LinkMetadata` table).
    pub async fn link_metadata(&self, url: &str) -> Option<Arc<LinkMeta>> {
        self.link_metadata_at(url, now_ms(), true).await
    }

    /// `HasPermissionToReadChannel`: read_channel_content, or a public channel's read_public_channel.
    pub async fn can_read_channel(&self, s: &Session, c: &Channel) -> ApiResult<(bool, bool)> {
        let (ok, member) = self.channel_perm(s, &c.id, perm::READ_CHANNEL_CONTENT).await?;
        if ok {
            return Ok((true, member));
        }
        if (c.r#type == OPEN || c.r#type == OPEN_BOARD) && !self.cfg.bool("ComplianceSettings.Enable") {
            return Ok((self.has_team_perm(s, &c.team_id, perm::READ_PUBLIC_CHANNEL).await, false));
        }
        Ok((false, false))
    }

    /// What every file read checks: the file's channel exists, and its creator or a reader of it asks.
    async fn check_file_access(&self, s: &Session, f: &FileInfo) -> ApiResult<()> {
        let channel = self.channel_or_404(&f.channel_id).await?;
        let (ok, _) = self.can_read_channel(s, &channel).await?;
        if f.creator_id == BOOKMARK_OWNER {
            if !ok {
                return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
            }
        } else if f.creator_id != s.user_id && !ok {
            return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
        }
        Ok(())
    }

    /// `App.GetFileInfo`: the info, with a mini preview made if missing.
    async fn readable_file_info(&self, s: &Session, id: &str) -> ApiResult<FileInfo> {
        if !is_valid_id(id) {
            return Err(AppError::invalid_url_param("file_id"));
        }
        let mut f = self.file_info(id).await?.ok_or_else(get_info_error)?;
        self.ensure_mini_preview(&mut f).await;
        self.check_file_access(s, &f).await?;
        Ok(f)
    }

    /// `CheckIfChannelIsRestrictedDM`: with `RestrictDirectMessage` = team, a DM or GM whose people share no team.
    pub(crate) async fn restricted_dm(&self, c: &Channel) -> ApiResult<bool> {
        if self.cfg.str("TeamSettings.RestrictDirectMessage") != "team" || (c.r#type != DIRECT && c.r#type != GROUP) {
            return Ok(false);
        }
        let common: Option<(String,)> = sqlx::query_as(
            "SELECT t.Id FROM Teams t JOIN TeamMembers tm ON tm.TeamId = t.Id
             WHERE t.DeleteAt = 0 AND tm.DeleteAt = 0
               AND tm.UserId IN (SELECT UserId FROM ChannelMembers WHERE ChannelId = $1)
             GROUP BY t.Id
             HAVING COUNT(DISTINCT tm.UserId) = (SELECT COUNT(*) FROM ChannelMembers WHERE ChannelId = $1)
             LIMIT 1",
        )
        .bind(&c.id)
        .fetch_optional(&self.db)
        .await?;
        Ok(common.is_none())
    }
}

// ---------- uploads ----------

fn read_request(detail: impl Into<String>) -> AppError {
    AppError::bad_request(
        "api.file.upload_file.read_request.app_error",
        "Unable to upload file(s). Error reading or parsing request data.",
    )
    .detail(detail)
}

fn too_large(name: &str, len: i64, limit: i64) -> AppError {
    AppError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "api.file.upload_file.too_large_detailed.app_error",
        &format!("Unable to upload file {name}. {len} bytes exceeds the maximum allowed {limit} bytes."),
    )
}

fn wrong_client_ids(ids: usize, files: usize) -> AppError {
    AppError::bad_request(
        "api.file.upload_file.incorrect_number_of_client_ids.app_error",
        &format!("Unable to upload file(s). Have {ids} client_ids for {files} files."),
    )
}

/// `mime.TypeByExtension`: Go's built-in table, then the usual system types; text types get `charset=utf-8`.
pub fn mime_type(ext_with_dot: &str) -> String {
    let e = ext_with_dot.to_lowercase();
    let builtin = match e.as_str() {
        ".avif" => "image/avif",
        ".css" => "text/css; charset=utf-8",
        ".gif" => "image/gif",
        ".htm" | ".html" => "text/html; charset=utf-8",
        ".jpeg" | ".jpg" => "image/jpeg",
        ".js" | ".mjs" => "text/javascript; charset=utf-8",
        ".json" => "application/json",
        ".pdf" => "application/pdf",
        ".png" => "image/png",
        ".svg" => "image/svg+xml",
        ".wasm" => "application/wasm",
        ".webp" => "image/webp",
        ".xml" => "text/xml; charset=utf-8",
        _ => "",
    };
    if !builtin.is_empty() {
        return builtin.into();
    }
    let Some(ext) = e.strip_prefix('.') else { return String::new() };
    match mime_guess::from_ext(ext).first() {
        Some(m) if m.type_() == "text" => format!("{}; charset=utf-8", m.essence_str()),
        Some(m) => m.essence_str().to_string(),
        None => String::new(),
    }
}

/// `filepath.Base` (Unix).
fn base(p: &str) -> String {
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        return if p.is_empty() { ".".into() } else { "/".into() };
    }
    t.rsplit('/').next().unwrap_or(t).to_string()
}

/// `UploadFileTask`.
pub(crate) struct Upload {
    pub name: String,
    pub channel_id: String,
    pub user_id: String,
    pub timestamp: i64,
    pub content_length: i64,
    /// `TeamId`: `noteam` from the API, the post's team from bulk import.
    pub team_id: String,
    /// `ExtractContent` (with the config's): always from the API, the import job's `extract_content` from bulk
    /// import.
    pub extract: Option<bool>,
}

impl Upload {
    fn prefix(&self, file_id: &str) -> String {
        if self.user_id == BOOKMARK_OWNER {
            return format!("{BOOKMARK_OWNER}/teams/{NO_TEAM}/channels/{}/{file_id}/", self.channel_id);
        }
        // `Timestamp.Format("20060102")` in the server's time zone (`TZ`).
        let day = chrono::DateTime::from_timestamp_millis(self.timestamp)
            .map(|t| t.with_timezone(&chrono::Local).format("%Y%m%d").to_string())
            .unwrap_or_default();
        format!("{day}/teams/{}/channels/{}/users/{}/{file_id}/", self.team_id, self.channel_id, self.user_id)
    }
}

/// The name cut to `max` bytes, its extension kept.
fn fit_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_string();
    }
    let ext = name.rfind('.').map(|i| &name[i..]).filter(|e| e.len() <= 64).unwrap_or("");
    let mut end = max - ext.len();
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", &name[..end])
}

/// Room on disk for `_preview.jpg` and the store's `#N` staging suffix within one 255-byte path component.
const DISK_NAME_MAX: usize = 230;

/// `UploadFileX`: streamed to the store (images kept in memory to process), then the info saved.
pub(crate) async fn upload_file_x<S, E>(app: &App, t: Upload, mut input: S) -> ApiResult<FileInfo>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let max = app.cfg.i64("FileSettings.MaxFileSize");
    if app.cfg.str("FileSettings.DriverName").is_empty() {
        return Err(AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "api.file.upload_file.storage.app_error",
            "Unable to upload file. Image storage is not configured.",
        ));
    }
    if t.content_length > max {
        return Err(too_large(&t.name, t.content_length, max));
    }
    let name = fit_name(&t.name, 255);
    let disk_name = fit_name(&t.name, DISK_NAME_MAX);
    let ext = name.rfind('.').map(|i| &name[i..]).unwrap_or("");
    let mut f = FileInfo {
        id: new_id(),
        creator_id: t.user_id.clone(),
        channel_id: t.channel_id.clone(),
        create_at: t.timestamp,
        name: name.clone(),
        extension: ext.trim_start_matches('.').to_lowercase(),
        mime_type: mime_type(ext),
        ..Default::default()
    };
    f.path = format!("{}{}", t.prefix(&f.id), disk_name);
    let limit = if t.content_length > 0 { t.content_length } else { max };
    let is_image = f.mime_type.starts_with("image");

    let mut w = app
        .files
        .writer(&f.path)
        .await
        .map_err(|e| AppError::internal("api.file.upload_file.storage.app_error", e.to_string()))?;
    let mut written: i64 = 0;
    let mut kept: Vec<u8> = vec![];
    while let Some(chunk) = input.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                w.abort().await;
                return Err(read_request(e.to_string()));
            }
        };
        let take = chunk.len().min((limit + 1 - written).max(0) as usize);
        let part = &chunk[..take];
        if let Err(e) = w.write(part).await {
            w.abort().await;
            return Err(AppError::internal("api.file.upload_file.storage.app_error", e.to_string()));
        }
        if is_image {
            kept.extend_from_slice(part);
        }
        written += take as i64;
        if written > limit {
            break;
        }
    }
    if let Err(e) = w.finish().await {
        return Err(AppError::internal("api.file.upload_file.storage.app_error", e.to_string()));
    }
    if written > max {
        let _ = app.files.remove(&f.path).await;
        return Err(too_large(&t.name, t.content_length, max));
    }
    f.size = written;

    let prefix = t.prefix(&f.id);
    if is_image && let Err(e) = process_image(app, &mut f, &disk_name, &prefix, kept).await {
        let _ = app.files.remove(&f.path).await;
        return Err(e);
    }
    f.update_at = f.create_at;
    f.remote_id = Some(String::new());
    save_file_info(app, &f).await?;
    // UploadFileX: the text extracted in the background (`FileSettings.ExtractContent`).
    if app.cfg.bool("FileSettings.ExtractContent")
        && t.extract.unwrap_or(true)
        && let Some(a) = app.hub.app()
    {
        let (id, name, path, mime, post) =
            (f.id.clone(), f.name.clone(), f.path.clone(), f.mime_type.clone(), f.post_id.clone());
        tokio::spawn(async move {
            let _ = a.extract_content_from_file_info(&id, &name, &path, &mime, &post).await;
        });
    }
    Ok(f)
}

/// `SqlFileInfoStore.Save`.
pub(crate) async fn save_file_info(app: &App, f: &FileInfo) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO FileInfo (Id, CreatorId, PostId, ChannelId, CreateAt, UpdateAt, DeleteAt, Path, ThumbnailPath,
             PreviewPath, Name, Extension, Size, MimeType, Width, Height, HasPreviewImage, MiniPreview, Content, RemoteId)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)",
    )
    .bind(&f.id)
    .bind(&f.creator_id)
    .bind(&f.post_id)
    .bind(&f.channel_id)
    .bind(f.create_at)
    .bind(f.update_at)
    .bind(f.delete_at)
    .bind(&f.path)
    .bind(&f.thumbnail_path)
    .bind(&f.preview_path)
    .bind(&f.name)
    .bind(&f.extension)
    .bind(f.size)
    .bind(&f.mime_type)
    .bind(f.width)
    .bind(f.height)
    .bind(f.has_preview_image)
    .bind(&f.mini_preview)
    .bind(&f.content)
    .bind(&f.remote_id)
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.file_info.save.app_error", e.to_string()))?;
    Ok(())
}

/// `preprocessImage` + `postprocessImage`: size, resolution limit, orientation, and the thumbnail, preview and
/// mini preview.
pub(crate) async fn process_image(
    app: &App,
    f: &mut FileInfo,
    name: &str,
    prefix: &str,
    data: Vec<u8>,
) -> ApiResult<()> {
    if f.mime_type == "image/svg+xml" {
        if let Some((w, h)) = images::svg_size(&data) {
            (f.width, f.height) = (w as i32, h as i32);
        }
        f.has_preview_image = false;
        return Ok(());
    }
    let Some(h) = images::header(&data) else { return Ok(()) };
    (f.width, f.height) = (h.width as i32, h.height as i32);
    let max_res = app.cfg.i64("FileSettings.MaxImageResolution");
    if h.width as i64 * h.height as i64 > max_res {
        return Err(AppError::bad_request(
            "api.file.upload_file.large_image_detailed.app_error",
            &format!("{} dimensions ({} by {} pixels) exceed the limits.", name, h.width, h.height),
        ));
    }
    f.has_preview_image = true;
    let stem = &name[..name.rfind('.').unwrap_or(name.len())];
    let ext = if f.mime_type == "image/png" { "png" } else { "jpg" };
    f.preview_path = format!("{prefix}{stem}_preview.{ext}");
    f.thumbnail_path = format!("{prefix}{stem}_thumb.{ext}");
    if h.quarter_turn {
        std::mem::swap(&mut f.width, &mut f.height);
    }
    let gif = f.mime_type == "image/gif";
    let made = tokio::task::spawn_blocking(move || {
        let (img, format) = images::decode_upright(&data)?;
        let enc = |i: &image::DynamicImage| {
            if format == image::ImageFormat::Png { images::png(i) } else { images::jpeg(i) }
        };
        Some((enc(&images::thumbnail(&img)), enc(&images::preview(&img)), images::mini_preview(&img)))
    })
    .await
    .ok()
    .flatten();
    let Some((thumb, preview, mini)) = made else { return Ok(()) };
    if gif {
        // A GIF that decodes is shown as itself: no preview image, but the files are made all the same.
        f.has_preview_image = false;
    }
    if let Some(b) = thumb {
        let _ = app.files.write(&f.thumbnail_path, b.into()).await;
    }
    if let Some(b) = preview {
        let _ = app.files.write(&f.preview_path, b.into()).await;
    }
    f.mini_preview = mini.map(Base64);
    Ok(())
}

#[derive(Serialize)]
struct UploadResponse {
    file_infos: Vec<FileInfo>,
    client_ids: Option<Vec<String>>,
}

/// `uploadFileStream`: a multipart form, or the raw body with `?channel_id=&filename=`.
async fn upload(
    St(app): St<State>,
    Auth(s): Auth,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    if !app.cfg.bool("FileSettings.EnableFileAttachments") {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.file.attachments.disabled.app_error",
            "File attachments have been disabled on this server.",
        ));
    }
    // Go's `r.ContentLength`: -1 for a chunked body, 0 for none.
    let chunked = headers.get(header::TRANSFER_ENCODING).is_some();
    let content_length: i64 = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(if chunked { -1 } else { 0 });
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let mut q = q;
    let mut body = body;
    if ct.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case("application/x-www-form-urlencoded")) {
        // `r.ParseForm` reads such a body as a form (only `client_id` is taken from it); the upload is then empty.
        let raw = axum::body::to_bytes(body, 10 << 20).await.map_err(|e| read_request(e.to_string()))?;
        for (k, v) in parse_form(&raw).ok_or_else(|| read_request("invalid form body"))? {
            if k == "client_id" {
                q.insert(k, v);
            }
        }
        body = Body::empty();
    }
    if content_length == 0 {
        return Err(read_request("Content-Length should not be 0"));
    }
    let is_multipart = ct.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case("multipart/form-data"));
    let resp = if is_multipart {
        let boundary = multer::parse_boundary(ct).map_err(|e| read_request(e.to_string()))?;
        upload_multipart(&app, &s, q.get("channel_id").cloned().unwrap_or_default(), &q, boundary, body).await?
    } else {
        upload_simple(&app, &s, &q, content_length, body).await?
    };
    Ok((StatusCode::CREATED, axum::Json(resp)).into_response())
}

/// `url.ParseQuery`: None where Go fails (a `;`, a bad `%` escape).
fn parse_form(raw: &[u8]) -> Option<Vec<(String, String)>> {
    fn unescape(s: &str) -> Option<String> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'%' => {
                    let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                    out.push(u8::from_str_radix(h, 16).ok()?);
                    i += 3;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }
    let text = String::from_utf8_lossy(raw);
    let mut out = vec![];
    for kv in text.split('&').filter(|kv| !kv.is_empty()) {
        if kv.contains(';') {
            return None;
        }
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        out.push((unescape(k)?, unescape(v)?));
    }
    Some(out)
}

fn bookmark(q: &HashMap<String, String>) -> bool {
    q.get(BOOKMARK_OWNER).is_some_and(|v| matches!(v.as_str(), "1" | "t" | "T" | "true" | "TRUE" | "True"))
}

/// `checkUploadFileAttachmentPermission`, the channel, and the restricted-DM check.
async fn check_upload_channel(app: &App, s: &Session, channel_id: &str, restricted_check: bool) -> ApiResult<()> {
    if !is_valid_id(channel_id) {
        return Err(AppError::invalid_url_param("channel_id"));
    }
    if !app.channel_perm(s, channel_id, perm::UPLOAD_FILE).await?.0 {
        return Err(AppError::forbidden_perm(perm::UPLOAD_FILE));
    }
    let c = app.channel(channel_id).await?.ok_or_else(|| {
        AppError::bad_request("api.file.upload_file.get_channel.app_error", "Cannot upload file to restricted DM")
    })?;
    if restricted_check && app.restricted_dm(&c).await? {
        return Err(AppError::bad_request(
            "api.file.upload_file.restricted_dm.error",
            "Cannot upload file to restricted DM",
        ));
    }
    Ok(())
}

async fn upload_simple(
    app: &App,
    s: &Session,
    q: &HashMap<String, String>,
    content_length: i64,
    body: Body,
) -> ApiResult<UploadResponse> {
    let channel_id = q.get("channel_id").cloned().unwrap_or_default();
    if !is_valid_id(&channel_id) {
        return Err(AppError::invalid_url_param("channel_id"));
    }
    let filename = q.get("filename").cloned().unwrap_or_default();
    if filename.is_empty() {
        return Err(AppError::invalid_url_param("filename"));
    }
    check_upload_channel(app, s, &channel_id, true).await?;
    let client_id = q.get("client_id").cloned().unwrap_or_default();
    let user_id = if bookmark(q) { BOOKMARK_OWNER.to_string() } else { s.user_id.clone() };
    let t = Upload {
        name: base(&filename),
        channel_id: base(&channel_id),
        user_id,
        timestamp: now_ms(),
        content_length,
        team_id: NO_TEAM.into(),
        extract: None,
    };
    let info = upload_file_x(app, t, body.into_data_stream()).await?;
    Ok(UploadResponse { file_infos: vec![info], client_ids: (!client_id.is_empty()).then(|| vec![client_id]) })
}

/// `uploadFileMultipart` (fields before files, streamed) and `uploadFileMultipartLegacy` (files before the
/// channel id, read whole first).
async fn upload_multipart(
    app: &App,
    s: &Session,
    mut channel_id: String,
    q: &HashMap<String, String>,
    boundary: String,
    body: Body,
) -> ApiResult<UploadResponse> {
    const MAX_FORM_VALUE: usize = 10 * 1024;
    let max = app.cfg.i64("FileSettings.MaxFileSize");
    let user_id = if bookmark(q) { BOOKMARK_OWNER.to_string() } else { s.user_id.clone() };
    let timestamp = now_ms();
    let mut mp = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut client_ids: Vec<String> = vec![];
    let mut expect_client_ids = true;
    let mut infos = vec![];
    let mut resp_ids = vec![];
    let mut checked = false;
    // Legacy: files that came before the channel id, kept in memory.
    let mut legacy: Option<Vec<(String, Bytes)>> = None;
    while let Some(field) = mp.next_field().await.map_err(|e| read_request(e.to_string()))? {
        let Some(formname) = field.name().filter(|n| !n.is_empty()).map(str::to_string) else { continue };
        let filename = field.file_name().unwrap_or("").to_string();
        if filename.is_empty() {
            let mut v = field.bytes().await.map_err(|e| {
                AppError::bad_request(
                    "api.file.upload_file.read_form_value.app_error",
                    &format!("Unable to upload file(s). Error reading the value for {formname}."),
                )
                .detail(e.to_string())
            })?;
            v.truncate(MAX_FORM_VALUE);
            let v = String::from_utf8_lossy(&v).into_owned();
            match formname.as_str() {
                "channel_id" => {
                    if !channel_id.is_empty() && channel_id != v {
                        return Err(AppError::bad_request(
                            "api.file.upload_file.multiple_channel_ids.app_error",
                            "Unable to upload file(s). Multiple conflicting channel_ids.",
                        ));
                    }
                    if !v.is_empty() {
                        channel_id = v;
                    }
                }
                "client_ids" if legacy.is_some() => client_ids.push(v),
                "client_ids" => {
                    if !expect_client_ids {
                        return Err(AppError::invalid_param("client_ids"));
                    }
                    client_ids.push(v);
                }
                _ if legacy.is_some() => {}
                other => return Err(AppError::invalid_param(other)),
            }
            continue;
        }
        if channel_id.is_empty() || legacy.is_some() {
            let data = field.bytes().await.map_err(|e| read_request(e.to_string()))?;
            if data.len() as i64 > max {
                return Err(AppError::internal(
                    "api.file.upload_file.read_request.app_error",
                    "multipart: message too large",
                ));
            }
            if formname == "files" {
                legacy.get_or_insert_with(Vec::new).push((filename, data));
            } else {
                legacy.get_or_insert_with(Vec::new);
            }
            continue;
        }
        if !checked {
            check_upload_channel(app, s, &channel_id, true).await?;
            checked = true;
        }
        if infos.is_empty() && client_ids.is_empty() {
            expect_client_ids = false;
        }
        let client_id = if expect_client_ids {
            match client_ids.get(infos.len()) {
                Some(c) => c.clone(),
                None => return Err(AppError::invalid_param("client_ids")),
            }
        } else {
            String::new()
        };
        let t = Upload {
            name: base(&filename),
            channel_id: base(&channel_id),
            user_id: user_id.clone(),
            timestamp,
            content_length: -1,
            team_id: NO_TEAM.into(),
            extract: None,
        };
        infos.push(upload_file_x(app, t, field).await?);
        if expect_client_ids {
            resp_ids.push(client_id);
        }
    }
    if let Some(files) = legacy {
        if channel_id.is_empty() {
            return Err(AppError::invalid_param("channel_id"));
        }
        check_upload_channel(app, s, &channel_id, false).await?;
        if !client_ids.is_empty() && client_ids.len() != files.len() {
            return Err(wrong_client_ids(client_ids.len(), files.len()));
        }
        for (i, (name, data)) in files.into_iter().enumerate() {
            let t = Upload {
                name: base(&name),
                channel_id: base(&channel_id),
                user_id: user_id.clone(),
                timestamp,
                content_length: -1,
                team_id: NO_TEAM.into(),
                extract: None,
            };
            let one = futures_util::stream::iter([Ok::<_, std::convert::Infallible>(data)]);
            infos.push(upload_file_x(app, t, one).await?);
            if let Some(c) = client_ids.get(i).filter(|c| !c.is_empty()) {
                resp_ids.push(c.clone());
            }
        }
        return Ok(UploadResponse { file_infos: infos, client_ids: Some(resp_ids) });
    }
    if expect_client_ids && client_ids.len() != infos.len() {
        return Err(wrong_client_ids(client_ids.len(), infos.len()));
    }
    Ok(UploadResponse { file_infos: infos, client_ids: Some(resp_ids) })
}

// ---------- downloads ----------

const UNSAFE_TYPES: [&str; 6] = [
    "application/javascript",
    "application/ecmascript",
    "text/javascript",
    "text/ecmascript",
    "application/x-javascript",
    "text/html",
];
const MEDIA_TYPES: [&str; 11] = [
    "image/jpeg",
    "image/png",
    "image/bmp",
    "image/gif",
    "image/tiff",
    "image/webp",
    "video/avi",
    "video/mpeg",
    "video/mp4",
    "audio/mpeg",
    "audio/wav",
];

/// `url.PathEscape`.
fn path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'$'
            | b'&'
            | b'+'
            | b':'
            | b'='
            | b'@' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `web.setHeaders`.
pub(crate) fn file_headers(h: &mut HeaderMap, content_type: &str, force_download: bool, filename: &str) {
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=86400"));
    }
    h.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    let ct = if content_type.is_empty() {
        "application/octet-stream"
    } else if UNSAFE_TYPES.iter().any(|u| content_type.starts_with(u)) {
        "text/plain"
    } else {
        content_type
    };
    if let Ok(v) = HeaderValue::from_str(ct) {
        h.insert(header::CONTENT_TYPE, v);
    }
    let download = force_download || !MEDIA_TYPES.iter().any(|m| ct.starts_with(m));
    let name = path_escape(filename);
    let kind = if download { "attachment" } else { "inline" };
    if let Ok(v) = HeaderValue::from_str(&format!("{kind};filename=\"{name}\"; filename*=UTF-8''{name}")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert("X-Frame-Options", HeaderValue::from_static("DENY"));
    h.insert("Content-Security-Policy", HeaderValue::from_static("Frame-ancestors 'none'"));
}

/// An HTTP date (RFC 1123).
fn http_date(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = crate::users::civil_from_days(days);
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WD[days.rem_euclid(7) as usize],
        d,
        MON[(m - 1) as usize],
        y,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// An HTTP date back to unix seconds (RFC 1123 only).
fn parse_http_date(s: &str) -> Option<i64> {
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let p: Vec<&str> = s.split_whitespace().collect();
    let [_, d, mon, y, t, _] = p[..] else { return None };
    let m = MON.iter().position(|x| *x == mon)? as u32 + 1;
    let hms: Vec<i64> = t.split(':').filter_map(|x| x.parse().ok()).collect();
    let [hh, mm, ss] = hms[..] else { return None };
    Some(search_params::days_from_civil(y.parse().ok()?, m, d.parse().ok()?) * 86400 + hh * 3600 + mm * 60 + ss)
}

/// One `bytes=` range against `size`: None for none or several (the whole file then), Err for unsatisfiable.
fn parse_range(h: &str, size: u64) -> Result<Option<std::ops::Range<u64>>, ()> {
    let Some(spec) = h.strip_prefix("bytes=") else { return Ok(None) };
    if spec.contains(',') {
        return Ok(None);
    }
    let (a, b) = spec.trim().split_once('-').ok_or(())?;
    let r = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        size.saturating_sub(n)..size
    } else {
        let start: u64 = a.parse().map_err(|_| ())?;
        if start >= size {
            return Err(());
        }
        let end = if b.is_empty() { size - 1 } else { b.parse::<u64>().map_err(|_| ())?.min(size - 1) };
        if end < start {
            return Err(());
        }
        start..end + 1
    };
    Ok(Some(r))
}

/// `web.WriteFileResponse` with `http.ServeContent`: Last-Modified and If-Modified-Since, single ranges, HEAD.
#[allow(clippy::too_many_arguments)]
async fn serve_file(
    app: &App,
    path: &str,
    filename: &str,
    content_type: &str,
    update_at: i64,
    force_download: bool,
    method: &Method,
    req: &HeaderMap,
    extra: Option<(header::HeaderName, &'static str)>,
) -> ApiResult<Response> {
    let mut h = HeaderMap::new();
    if let Some((k, v)) = extra {
        h.insert(k, HeaderValue::from_static(v));
    }
    file_headers(&mut h, content_type, force_download, filename);
    let not_found = |e: anyhow::Error| {
        AppError::not_found("api.file.file_reader.app_error", "Unable to get a file reader.").detail(e.to_string())
    };
    let size = app.files.size(path).await.map_err(not_found)?;
    let modified = update_at / 1000;
    if update_at > 0 {
        h.insert(header::LAST_MODIFIED, HeaderValue::from_str(&http_date(modified)).unwrap());
        if let Some(since) = req.get(header::IF_MODIFIED_SINCE).and_then(|v| v.to_str().ok()).and_then(parse_http_date)
            && modified <= since
        {
            h.remove(header::CONTENT_TYPE);
            return Ok((StatusCode::NOT_MODIFIED, h).into_response());
        }
    }
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let if_range_ok = req
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| parse_http_date(v).is_some_and(|t| modified <= t));
    let range = match req.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(r) if if_range_ok && size > 0 => match parse_range(r, size) {
            Ok(r) => r,
            Err(()) => {
                h.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{size}")).unwrap());
                return Ok((StatusCode::RANGE_NOT_SATISFIABLE, h, "invalid range: failed to overlap").into_response());
            }
        },
        _ => None,
    };
    let (status, len) = match &range {
        Some(r) => {
            h.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {}-{}/{size}", r.start, r.end - 1)).unwrap(),
            );
            (StatusCode::PARTIAL_CONTENT, r.end - r.start)
        }
        None => (StatusCode::OK, size),
    };
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if method == Method::HEAD {
        return Ok((status, h).into_response());
    }
    let reading = app.files.stream(path, range).await.map_err(not_found)?;
    let body = Body::from_stream(reading.stream.map_err(std::io::Error::other));
    Ok((status, h, body).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DownloadQuery {
    download: String,
    h: String,
}

fn truthy(s: &str) -> bool {
    matches!(s, "1" | "t" | "T" | "true" | "TRUE" | "True")
}

/// `getFile`: the file, read through `GetByIds` from the database (not the cache), deleted ones refused.
async fn get_file(
    St(app): St<State>,
    crate::app::AuthTrusted(s): crate::app::AuthTrusted,
    Path(id): Path<String>,
    Query(q): Query<DownloadQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !is_valid_id(&id) {
        return Err(AppError::invalid_url_param("file_id"));
    }
    let sql = format!("{} WHERE Id = $1", select());
    let f: FileInfo = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&id)
        .fetch_optional(&app.db)
        .await?
        .ok_or_else(|| AppError::not_found("api.file.get_file_info.app_error", "Failed to get file info."))?;
    app.channel_or_404(&f.channel_id).await?;
    if f.delete_at != 0 {
        return Err(AppError::not_found("api.file.get_file_info.app_error", "Failed to get file info."));
    }
    app.check_file_access(&s, &f).await?;
    serve_file(&app, &f.path, &f.name, &f.mime_type, f.update_at, truthy(&q.download), &method, &headers, None).await
}

async fn get_thumbnail(
    St(app): St<State>,
    crate::app::AuthTrusted(s): crate::app::AuthTrusted,
    Path(id): Path<String>,
    Query(q): Query<DownloadQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let f = app.readable_file_info(&s, &id).await?;
    if f.thumbnail_path.is_empty() {
        return Err(AppError::bad_request(
            "api.file.get_file_thumbnail.no_thumbnail.app_error",
            "File doesn't have a thumbnail image.",
        )
        .detail(format!("file_id={}", f.id)));
    }
    serve_file(
        &app,
        &f.thumbnail_path,
        &f.name,
        THUMBNAIL_TYPE,
        f.update_at,
        truthy(&q.download),
        &method,
        &headers,
        None,
    )
    .await
}

async fn get_preview(
    St(app): St<State>,
    crate::app::AuthTrusted(s): crate::app::AuthTrusted,
    Path(id): Path<String>,
    Query(q): Query<DownloadQuery>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let f = app.readable_file_info(&s, &id).await?;
    if f.preview_path.is_empty() {
        return Err(AppError::bad_request(
            "api.file.get_file_preview.no_preview.app_error",
            "File doesn't have a preview image.",
        )
        .detail(format!("file_id={}", f.id)));
    }
    serve_file(
        &app,
        &f.preview_path,
        &f.name,
        THUMBNAIL_TYPE,
        f.update_at,
        truthy(&q.download),
        &method,
        &headers,
        None,
    )
    .await
}

async fn get_info(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Response> {
    let f = app.readable_file_info(&s, &id).await?;
    let mut r = axum::Json(f).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=2592000, private"));
    Ok(r)
}

/// `GeneratePublicLinkHash`: SHA-256 of the salt and the id, URL-safe base64 without padding.
pub fn public_link_hash(file_id: &str, salt: &str) -> String {
    use base64::Engine;
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(file_id.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize())
}

/// `GetSiteURLHeader`: the configured site URL, or the one the request came to.
fn site_url(app: &App, req: &HeaderMap) -> String {
    let site = app.cfg.str("ServiceSettings.SiteURL").trim_end_matches('/');
    if !site.is_empty() {
        return site.into();
    }
    let host = req.get("x-forwarded-host").or(req.get(header::HOST)).and_then(|v| v.to_str().ok()).unwrap_or("");
    let proto = req.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("http");
    format!("{proto}://{host}")
}

async fn get_link(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !is_valid_id(&id) {
        return Err(AppError::invalid_url_param("file_id"));
    }
    if !app.cfg.bool("FileSettings.EnablePublicLink") {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.file.get_public_link.disabled.app_error",
            "Public links have been disabled.",
        ));
    }
    let f = app.readable_file_info(&s, &id).await?;
    if f.post_id.is_empty() && f.creator_id != BOOKMARK_OWNER {
        return Err(AppError::bad_request(
            "api.file.get_public_link.no_post.app_error",
            "Unable to get public link for file. File must be attached to a post that can be read by the current user.",
        )
        .detail(format!("file_id={}", f.id)));
    }
    let hash = public_link_hash(&f.id, app.cfg.str("FileSettings.PublicLinkSalt"));
    let link = format!("{}/files/{}/public?h={hash}", site_url(&app, &headers), f.id);
    Ok(axum::Json(BTreeMap::from([("link", link)])).into_response())
}

/// `getPublicFile`: with the link's hash, no session.
/// `getPublicFile`, on the web router (`/files/{file_id:[A-Za-z0-9]+}/public`): its errors are the web error page.
/// Go renders the page for a bad hash twice, once in the handler and again from `handleContextError`, so the
/// body holds two pages; here it is rendered once.
async fn get_public(
    St(app): St<State>,
    Path(id): Path<String>,
    Query(q): Query<DownloadQuery>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    // An id outside the route's pattern is no route: the web client's page.
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return crate::web::root(&app).await;
    }
    match public_file(&app, &id, &q, &method, &headers).await {
        Ok(r) => r,
        Err(e) => crate::web::web_error(&app, &headers, e).await,
    }
}

async fn public_file(
    app: &App,
    id: &str,
    q: &DownloadQuery,
    method: &Method,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    if !is_valid_id(id) {
        return Err(AppError::invalid_url_param("file_id"));
    }
    if !app.cfg.bool("FileSettings.EnablePublicLink") {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.file.get_public_link.disabled.app_error",
            "Public links have been disabled.",
        ));
    }
    let f = app.file_info(id).await?.ok_or_else(get_info_error)?;
    let want = public_link_hash(&f.id, app.cfg.str("FileSettings.PublicLinkSalt"));
    if q.h.is_empty() || !constant_eq(q.h.as_bytes(), want.as_bytes()) {
        return Err(AppError::bad_request(
            "api.file.get_file.public_invalid.app_error",
            "The public link does not appear to be valid.",
        ));
    }
    serve_file(app, &f.path, &f.name, &f.mime_type, f.update_at, false, method, headers, None).await
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `getFileInfosForPost`: the post's files (`GetFileInfosForPost`, by-id cache), `include_deleted` for admins.
async fn post_file_infos(
    St(app): St<State>,
    Auth(s): Auth,
    Path(post_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<axum::Json<Vec<FileInfo>>> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    let post: Option<(String, crate::model::StringArray, i64)> =
        sqlx::query_as("SELECT COALESCE(ChannelId, ''), FileIds, COALESCE(DeleteAt, 0) FROM Posts WHERE Id = $1")
            .bind(&post_id)
            .fetch_optional(&app.db)
            .await?;
    let readable = match &post {
        Some((cid, _, _)) => match app.channel(cid).await? {
            Some(c) => app.can_read_channel(&s, &c).await?.0,
            None => app.has_perm(&s, perm::READ_CHANNEL_CONTENT).await,
        },
        None => app.has_perm(&s, perm::READ_CHANNEL_CONTENT).await,
    };
    if !readable {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    let include_deleted = q.get("include_deleted").is_some_and(|v| truthy(v));
    if include_deleted && !app.has_perm(&s, perm::MANAGE_SYSTEM).await {
        return Err(AppError::forbidden_perm(perm::MANAGE_SYSTEM));
    }
    let Some((_, file_ids, delete_at)) = post.filter(|p| include_deleted || p.2 == 0) else {
        return Err(AppError::not_found("app.post.get.app_error", "Unable to get the post."));
    };
    let _ = delete_at;
    Ok(axum::Json(app.file_infos_for_ids(&file_ids.0, include_deleted).await?))
}

// ---------- search ----------

#[derive(Deserialize)]
struct SearchBody {
    terms: Option<String>,
    is_or_search: Option<bool>,
    time_zone_offset: Option<i64>,
    include_deleted_channels: Option<bool>,
    page: Option<i64>,
    per_page: Option<i64>,
}

/// `model.FileInfoList`.
#[derive(Serialize, Default)]
pub struct FileInfoList {
    pub order: Vec<String>,
    pub file_infos: BTreeMap<String, FileInfo>,
    pub next_file_info_id: String,
    pub prev_file_info_id: String,
    pub first_inaccessible_file_time: i64,
}

async fn search_in_team(
    St(app): St<State>,
    BusyAuth(s): BusyAuth,
    Path(team_id): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    if !app.has_team_perm(&s, &team_id, perm::VIEW_TEAM).await {
        return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
    }
    search_files(&app, &s, &team_id, &body).await
}

async fn search_all_teams(St(app): St<State>, BusyAuth(s): BusyAuth, body: Bytes) -> ApiResult<Response> {
    search_files(&app, &s, "", &body).await
}

async fn search_files(app: &App, s: &Session, team_id: &str, body: &[u8]) -> ApiResult<Response> {
    let b: SearchBody = serde_json::from_slice(body).map_err(|e| {
        AppError::bad_request("api.post.search_files.invalid_body.app_error", "Unable to parse the request body.")
            .detail(e.to_string())
    })?;
    let terms = b.terms.unwrap_or_default();
    if terms.is_empty() {
        return Err(AppError::invalid_param("terms"));
    }
    if !app.cfg.bool("ServiceSettings.EnableFileSearch") {
        return Err(AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "store.sql_file_info.search.disabled",
            "Searching files has been disabled on this server. Please contact your System Administrator.",
        ));
    }
    let include_deleted = b.include_deleted_channels.unwrap_or(false);
    let mut list = Vec::new();
    for mut p in search_params::parse(terms.trim(), b.time_zone_offset.unwrap_or(0)) {
        p.or_terms = b.is_or_search.unwrap_or(false);
        p.include_deleted_channels = include_deleted;
        if p.terms == "*" {
            continue;
        }
        p.in_channels = channel_names_to_ids(app, &p.in_channels, &s.user_id, team_id, include_deleted).await;
        p.excluded_channels =
            channel_names_to_ids(app, &p.excluded_channels, &s.user_id, team_id, include_deleted).await;
        p.from_users = usernames_to_ids(app, &p.from_users).await;
        p.excluded_users = usernames_to_ids(app, &p.excluded_users).await;
        list.push(p);
    }
    let mut result = FileInfoList::default();
    if !list.is_empty() && b.page.unwrap_or(0) <= 0 {
        let _ = b.per_page;
        let found = search_store(app, &list, &s.user_id, team_id).await;
        // FilterFilesByChannelPermissions
        let mut allowed: HashMap<String, bool> = HashMap::new();
        for f in found {
            let ok = match allowed.get(&f.channel_id) {
                Some(ok) => *ok,
                None => {
                    let ok = match app.channel(&f.channel_id).await? {
                        Some(c) => app.can_read_channel(s, &c).await?.0,
                        None => false,
                    };
                    allowed.insert(f.channel_id.clone(), ok);
                    ok
                }
            };
            if ok && !result.file_infos.contains_key(&f.id) {
                result.order.push(f.id.clone());
                result.file_infos.insert(f.id.clone(), f);
            }
        }
    }
    let mut r = axum::Json(result).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-store, must-revalidate"));
    Ok(r)
}

/// `parseAndFetchChannelIdByNameFromInFilter` over a list: names, `@user` (an existing DM) or `@a,b` (an
/// existing group); unknown names stay as they are.
async fn channel_names_to_ids(
    app: &App,
    names: &[String],
    user_id: &str,
    team_id: &str,
    include_deleted: bool,
) -> Vec<String> {
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        let clean = n.trim_start_matches('~');
        // (team, name, include deleted): a DM or group has no team and is never archived away from search.
        let target: Option<(String, String, bool)> = if let Some(users) = clean.strip_prefix('@') {
            let names: Vec<String> = users.split(',').map(str::to_string).collect();
            let ids: Vec<(String,)> = sqlx::query_as("SELECT Id FROM Users WHERE Username = ANY($1)")
                .bind(&names)
                .fetch_all(&app.db)
                .await
                .unwrap_or_default();
            let mut ids: Vec<String> = ids.into_iter().map(|i| i.0).collect();
            if names.len() == 1 {
                ids.first().map(|other| (String::new(), crate::model::channel::direct_name(user_id, other), true))
            } else if ids.len() == names.len() {
                // GetGroupNameFromUserIds: SHA-1 of the sorted ids.
                ids.sort();
                let mut h = sha1::Sha1::new();
                for id in &ids {
                    sha1::Digest::update(&mut h, id.as_bytes());
                }
                let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
                Some((String::new(), hex, true))
            } else {
                None
            }
        } else {
            Some((team_id.to_string(), clean.to_string(), include_deleted))
        };
        let resolved: Option<(String,)> = match target {
            Some((team, name, deleted)) => {
                let mut sql = "SELECT Id FROM Channels WHERE (TeamId = $1 OR TeamId = '') AND Name = $2".to_string();
                if !deleted {
                    sql += " AND DeleteAt = 0";
                }
                sqlx::query_as(sqlx::AssertSqlSafe(sql))
                    .bind(team)
                    .bind(name)
                    .fetch_optional(&app.db)
                    .await
                    .ok()
                    .flatten()
            }
            None => None,
        };
        out.push(resolved.map(|r| r.0).unwrap_or_else(|| n.clone()));
    }
    out
}

/// `convertUserNameToUserIds`: unknown names stay.
async fn usernames_to_ids(app: &App, names: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        let name = n.trim_start_matches('@');
        let id: Option<(String,)> = sqlx::query_as("SELECT Id FROM Users WHERE Username = $1")
            .bind(name)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
        out.push(id.map(|i| i.0).unwrap_or_else(|| n.clone()));
    }
    out
}

/// The server's `default_text_search_config`, as Go reads it once.
pub(crate) async fn text_search_config(app: &App) -> String {
    static C: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    C.get_or_init(|| async {
        let c: String = sqlx::query_scalar("SHOW default_text_search_config")
            .fetch_one(&app.db)
            .await
            .unwrap_or_else(|_| "pg_catalog.english".into());
        let c = c.strip_prefix("pg_catalog.").unwrap_or(&c).to_string();
        if c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') { c } else { "english".into() }
    })
    .await
    .clone()
}

/// `SqlFileInfoStore.Search`: at most 100, newest first; errors give nothing, as Go logs and returns empty.
async fn search_store(app: &App, list: &[SearchParams], user_id: &str, team_id: &str) -> Vec<FileInfo> {
    let cfg = text_search_config(app).await;
    let cols: String = <FileInfo as db::Table>::COLUMNS
        .iter()
        .map(|c| match *c {
            "channelid" | "content" | "remoteid" => format!("COALESCE(FileInfo.{c}, '') AS {c}"),
            c => format!("FileInfo.{c}"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "SELECT {cols} FROM FileInfo LEFT JOIN Channels AS C ON C.Id = FileInfo.ChannelId
         LEFT JOIN ChannelMembers AS CM ON C.Id = CM.ChannelId
         WHERE FileInfo.DeleteAt = 0 AND (FileInfo.CreatorId = 'bookmark' OR FileInfo.PostId != '')
           AND NOT EXISTS (SELECT 1 FROM TemporaryPosts WHERE TemporaryPosts.PostId = FileInfo.PostId)"
    );
    let mut args: Vec<Arg> = vec![];
    let bind = |a: Arg, args: &mut Vec<Arg>| {
        args.push(a);
        format!("${}", args.len())
    };
    if !team_id.is_empty() {
        let p = bind(Arg::S(team_id.into()), &mut args);
        sql += &format!(" AND (C.TeamId = {p} OR C.TeamId = '')");
    }
    for p in list {
        let terms_raw = search_params::remove_non_alphanumeric_unquoted(&p.terms, " ");
        if !p.include_deleted_channels {
            sql += " AND C.DeleteAt = 0";
        }
        if !p.search_without_user_id {
            let x = bind(Arg::S(user_id.into()), &mut args);
            sql += &format!(" AND CM.UserId = {x}");
        }
        let mut list_cond = |col: &str, v: &[String], neg: bool, args: &mut Vec<Arg>| {
            if !v.is_empty() {
                let x = bind(Arg::L(v.to_vec()), args);
                sql += &if neg { format!(" AND NOT ({col} = ANY({x}))") } else { format!(" AND {col} = ANY({x})") };
            }
        };
        list_cond("C.Id", &p.in_channels, false, &mut args);
        list_cond("FileInfo.Extension", &p.extensions, false, &mut args);
        list_cond("FileInfo.Extension", &p.excluded_extensions, true, &mut args);
        list_cond("C.Id", &p.excluded_channels, true, &mut args);
        list_cond("FileInfo.CreatorId", &p.from_users, false, &mut args);
        list_cond("FileInfo.CreatorId", &p.excluded_users, true, &mut args);
        if !p.on_date.is_empty() {
            let (a, b) = p.on_millis();
            sql += &format!(" AND FileInfo.CreateAt BETWEEN {a} AND {b}");
        } else {
            if !p.excluded_date.is_empty() {
                let (a, b) = p.excluded_date_millis();
                sql += &format!(" AND FileInfo.CreateAt NOT BETWEEN {a} AND {b}");
            }
            if !p.after_date.is_empty() {
                sql += &format!(" AND FileInfo.CreateAt >= {}", p.after_millis());
            }
            if !p.before_date.is_empty() {
                sql += &format!(" AND FileInfo.CreateAt <= {}", p.before_millis());
            }
            if !p.excluded_after_date.is_empty() {
                sql += &format!(" AND FileInfo.CreateAt < {}", p.excluded_after_millis());
            }
            if !p.excluded_before_date.is_empty() {
                sql += &format!(" AND FileInfo.CreateAt > {}", p.excluded_before_millis());
            }
        }
        let clean = |s: &str| {
            let mut s = s.to_string();
            for c in ["<", ">", "+", "(", ")", "~", ":", "-"] {
                s = s.replace(c, " ");
            }
            WILDCARD.replace_all(&s, ":* ").into_owned()
        };
        let (terms, excluded) = (clean(&terms_raw), clean(&p.excluded_terms));
        if !(terms.is_empty() && excluded.is_empty()) {
            let exclude = if excluded.is_empty() {
                String::new()
            } else {
                format!(" & !({})", excluded.split_whitespace().collect::<Vec<_>>().join(" | "))
            };
            let sep = if p.or_terms { " | " } else { " & " };
            let q = format!("({}){exclude}", terms.split_whitespace().collect::<Vec<_>>().join(sep));
            let x = bind(Arg::S(q), &mut args);
            sql += &format!(
                " AND (to_tsvector('{cfg}', FileInfo.Name) @@ to_tsquery('{cfg}', {x})
                   OR to_tsvector('{cfg}', Translate(FileInfo.Name, '.,-', '   ')) @@ to_tsquery('{cfg}', {x})
                   OR to_tsvector('{cfg}', FileInfo.Content) @@ to_tsquery('{cfg}', {x}))"
            );
        }
    }
    sql += " ORDER BY FileInfo.CreateAt DESC LIMIT 100";
    let mut q = sqlx::query_as::<_, FileInfo>(sqlx::AssertSqlSafe(sql));
    for a in args {
        q = match a {
            Arg::S(s) => q.bind(s),
            Arg::L(l) => q.bind(l),
        };
    }
    match q.fetch_all(&app.db).await {
        Ok(v) => v
            .into_iter()
            .map(|mut f| {
                f.archived = false;
                f
            })
            .collect(),
        Err(e) => {
            eprintln!("query error searching files: {e}");
            vec![]
        }
    }
}

static WILDCARD: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\*($| )").unwrap());

enum Arg {
    S(String),
    L(Vec<String>),
}

// ---------- the image proxy ----------

/// `getImage`: with the image proxy on, a site image is redirected to and another site's fetched; off, refused.
async fn get_image(St(app): St<State>, _: Auth, Query(q): Query<HashMap<String, String>>) -> ApiResult<Response> {
    let bad = || AppError::bad_request("api.image.get.app_error", "Requested image url cannot be parsed.");
    let raw = q.get("url").cloned().unwrap_or_default();
    let site = reqwest::Url::parse(app.cfg.str("ServiceSettings.SiteURL")).ok();
    let url = match reqwest::Url::parse(&raw) {
        Ok(u) if u.cannot_be_a_base() => return Err(bad()),
        Ok(u) => u,
        // A path on this site.
        Err(_) => site.as_ref().and_then(|s| s.join(&raw).ok()).ok_or_else(bad)?,
    };
    if !app.cfg.bool("ImageProxySettings.Enable") {
        return Err(bad());
    }
    if site.as_ref().and_then(|s| s.host_str()) == url.host_str() {
        return Ok((StatusCode::FOUND, [(header::LOCATION, url.to_string())]).into_response());
    }
    app.proxy_image(&url).await
}

impl App {
    /// The local image proxy: another site's picture, fetched as link previews are, served as an image.
    async fn proxy_image(&self, url: &reqwest::Url) -> ApiResult<Response> {
        let fail = |d: String| {
            AppError::bad_request("api.image.get.app_error", "Requested image url cannot be parsed.").detail(d)
        };
        let (body, ct) = self.fetch_image(url).await.map_err(fail)?;
        let mut h = HeaderMap::new();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=259200, public"));
        h.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
        h.insert(
            "Content-Security-Policy",
            HeaderValue::from_static("default-src 'none'; img-src data:; style-src 'unsafe-inline'"),
        );
        if let Ok(v) = HeaderValue::from_str(&ct) {
            h.insert(header::CONTENT_TYPE, v);
        }
        Ok((StatusCode::OK, h, body).into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_fit() {
        assert_eq!(fit_name("a.txt", 255), "a.txt");
        let long = format!("{}.txt", "ж".repeat(200));
        let f = fit_name(&long, DISK_NAME_MAX);
        assert!(f.len() <= DISK_NAME_MAX && f.ends_with(".txt") && f.starts_with("жж"));
        assert_eq!(fit_name(&"x".repeat(300), 255).len(), 255);
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 10), Ok(Some(0..10)));
        assert_eq!(parse_range("bytes=5-", 10), Ok(Some(5..10)));
        assert_eq!(parse_range("bytes=2-4", 10), Ok(Some(2..5)));
        assert_eq!(parse_range("bytes=-3", 10), Ok(Some(7..10)));
        assert_eq!(parse_range("bytes=2-100", 10), Ok(Some(2..10)));
        assert_eq!(parse_range("bytes=10-", 10), Err(()));
        assert_eq!(parse_range("bytes=0-1,3-4", 10), Ok(None));
    }

    #[test]
    fn names_and_types() {
        assert_eq!(path_escape("a b(1).png"), "a%20b%281%29.png");
        assert_eq!(path_escape("отчёт.pdf"), "%D0%BE%D1%82%D1%87%D1%91%D1%82.pdf");
        assert_eq!(mime_type(".PNG"), "image/png");
        assert_eq!(mime_type(".mp4"), "video/mp4");
        assert_eq!(mime_type(".txt"), "text/plain; charset=utf-8");
        assert_eq!(mime_type(""), "");
        assert_eq!(base("dir/x.png"), "x.png");
        let d = http_date(784111777);
        assert_eq!(d, "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(parse_http_date(&d), Some(784111777));
        assert_eq!(public_link_hash("id", "salt").len(), 43);
    }
}
