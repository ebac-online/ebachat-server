//! Custom emoji: listing, search, creation (images resized as Go does), deletion, images, and the emoji caches
//! (by id, and by name with not-found entries kept a minute).

// Based on Mattermost server/channels/api4/emoji.go, app/emoji.go, store/sqlstore/emoji_store.go and
// store/localcachelayer/emoji_layer.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::app::Query;
use axum::extract::{Multipart, Path, State as St};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, Body, State, is_valid_id, new_id, now_ms};
use crate::db::{self, Table};
use crate::hub::Broadcast;
use crate::model::{Emoji, Session};
use crate::perm;

const MAX_FILE_SIZE: usize = 1 << 19;
const MAX_WIDTH: u32 = 128;
const MAX_HEIGHT: u32 = 128;
const MAX_ORIGINAL: u32 = 1028;
const MAX_GIF_FRAMES: usize = 70;
const NOT_FOUND_TTL: Duration = Duration::from_secs(60);

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/emoji", post(create).get(list))
        .route("/api/v4/emoji/names", post(by_names))
        .route("/api/v4/emoji/search", post(search))
        .route("/api/v4/emoji/autocomplete", get(autocomplete))
        .route("/api/v4/emoji/{emoji_id}", get(get_one).delete(delete))
        .route("/api/v4/emoji/name/{emoji_name}", get(by_name))
        .route("/api/v4/emoji/{emoji_id}/image", get(image))
}

/// Names of the system emoji (`model.SystemEmojis`).
pub fn is_system_emoji(name: &str) -> bool {
    static NAMES: OnceLock<HashSet<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| include_str!("../data/system_emojis.txt").lines().collect()).contains(name)
}

/// `GetSystemEmojiId`: the code point sequence a system emoji's image is named after.
pub fn system_emoji_code(name: &str) -> Option<&'static str> {
    static CODES: OnceLock<std::collections::HashMap<&'static str, &'static str>> = OnceLock::new();
    CODES
        .get_or_init(|| include_str!("../data/emoji_codes.txt").lines().filter_map(|l| l.split_once(' ')).collect())
        .get(name)
        .copied()
}

/// `IsValidEmojiName`.
pub(crate) fn check_name(name: &str) -> ApiResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'+');
    if !ok {
        return Err(AppError::bad_request("model.emoji.name.app_error", ""));
    }
    if is_system_emoji(name) {
        return Err(AppError::bad_request("model.emoji.system_emoji_name.app_error", ""));
    }
    Ok(())
}

fn disabled() -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "api.emoji.disabled.app_error", "")
}

impl App {
    fn emoji_on(&self) -> ApiResult<()> {
        if self.cfg.bool("ServiceSettings.EnableCustomEmoji") { Ok(()) } else { Err(disabled()) }
    }

    fn cache_emoji(&self, e: &Arc<Emoji>) {
        self.caches.emoji_by_id.put(e.id.clone(), Some(e.clone()));
        self.caches.emoji_by_name.put(e.name.clone(), Some(e.clone()));
    }

    /// `EmojiStore.Get` through the id cache.
    pub async fn emoji(&self, id: &str) -> ApiResult<Option<Arc<Emoji>>> {
        if let Some(Some(e)) = self.caches.emoji_by_id.get(id) {
            return Ok(Some(e));
        }
        let e: Option<Emoji> = db::one(&self.db, "id = $1 AND deleteat = 0", id).await?;
        Ok(e.map(|e| {
            let e = Arc::new(e);
            self.cache_emoji(&e);
            e
        }))
    }

    /// `GetEmojiStaticURL`: a system emoji's image under `/static/emoji`, a custom one's under `/api/v4/emoji`,
    /// below the site's subpath.
    pub async fn emoji_static_url(&self, name: &str) -> Option<String> {
        let sub = crate::web::subpath(self.cfg.str("ServiceSettings.SiteURL"));
        let sub = sub.trim_end_matches('/');
        if let Some(code) = system_emoji_code(name) {
            return Some(format!("{sub}/static/emoji/{code}.png"));
        }
        match self.emoji_by_name(name).await {
            Ok(Some(e)) => Some(format!("{sub}/api/v4/emoji/{}/image", e.id)),
            _ => None,
        }
    }

    /// `EmojiStore.GetByName` through the name cache.
    pub async fn emoji_by_name(&self, name: &str) -> ApiResult<Option<Arc<Emoji>>> {
        if let Some(Some(e)) = self.caches.emoji_by_name.get(name) {
            return Ok(Some(e));
        }
        let e: Option<Emoji> = db::one(&self.db, "name = $1 AND deleteat = 0", name).await?;
        Ok(e.map(|e| {
            let e = Arc::new(e);
            self.cache_emoji(&e);
            e
        }))
    }

    /// `EmojiStore.GetMultipleByName` through the emoji caches: the custom emoji of these names that exist;
    /// names found missing are remembered for a minute.
    pub async fn emojis_by_names(&self, names: &[String]) -> ApiResult<Vec<Arc<Emoji>>> {
        let mut out = vec![];
        let mut rest = vec![];
        for n in names {
            match self.caches.emoji_by_name.get(n.as_str()) {
                Some(Some(e)) => out.push(e),
                Some(None) => {}
                None => rest.push(n.clone()),
            }
        }
        if !rest.is_empty() {
            let sql = format!("{} WHERE name = ANY($1) AND deleteat = 0", Emoji::SELECT);
            let found: Vec<Emoji> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&rest).fetch_all(&self.db).await?;
            let names: HashSet<String> = found.iter().map(|e| e.name.clone()).collect();
            for e in found {
                let e = Arc::new(e);
                self.cache_emoji(&e);
                out.push(e);
            }
            for n in rest.into_iter().filter(|n| !names.contains(n) && n.len() <= 64) {
                self.caches.emoji_by_name.put_for(n, None, NOT_FOUND_TTL);
            }
        }
        Ok(out)
    }

    /// `confirmEmojiExists`: a system emoji, or a custom one.
    pub async fn emoji_exists(&self, name: &str) -> ApiResult<bool> {
        if is_system_emoji(name) {
            return Ok(true);
        }
        if check_name(name).is_err() {
            return Ok(false);
        }
        Ok(self.emoji_by_name(name).await?.is_some())
    }

    /// A permission held system-wide or in any of the user's teams (emoji are not team objects).
    async fn perm_anywhere(&self, s: &Session, p: &str) -> bool {
        if self.has_perm(s, p).await {
            return true;
        }
        for tm in &s.team_members {
            if self.has_team_perm(s, &tm.team_id, p).await {
                return true;
            }
        }
        false
    }
}

fn no_result() -> AppError {
    AppError::not_found("app.emoji.get.no_result", "")
}

// ---------- reads ----------

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListQuery {
    page: i64,
    per_page: Option<i64>,
    sort: String,
}

async fn list(St(app): St<State>, _: Auth, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Emoji>>> {
    app.emoji_on()?;
    if !q.sort.is_empty() && q.sort != "name" {
        return Err(AppError::invalid_url_param("sort"));
    }
    let per = q.per_page.unwrap_or(60).clamp(0, 200);
    let order = if q.sort == "name" { "ORDER BY Name" } else { "" };
    let sql = format!("{} WHERE DeleteAt = 0 {order} LIMIT $1 OFFSET $2", Emoji::SELECT);
    let list: Vec<Emoji> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(per).bind(per * q.page.max(0)).fetch_all(&app.db).await?;
    Ok(Json(list))
}

async fn get_one(St(app): St<State>, _: Auth, Path(id): Path<String>) -> ApiResult<Json<Emoji>> {
    if !is_valid_id(&id) {
        return Err(AppError::invalid_url_param("emoji_id"));
    }
    app.emoji_on()?;
    let e = app.emoji(&id).await?.ok_or_else(no_result)?;
    Ok(Json((*e).clone()))
}

async fn by_name(St(app): St<State>, _: Auth, Path(name): Path<String>) -> ApiResult<Json<Emoji>> {
    if check_name(&name).is_err() && !is_system_emoji(&name) {
        return Err(AppError::invalid_url_param("emoji_name"));
    }
    app.emoji_on()?;
    let e =
        app.emoji_by_name(&name).await?.ok_or_else(|| AppError::not_found("app.emoji.get_by_name.no_result", ""))?;
    Ok(Json((*e).clone()))
}

async fn by_names(St(app): St<State>, _: Auth, body: axum::body::Bytes) -> ApiResult<Json<Vec<Emoji>>> {
    let mut names = crate::posts::sorted_array_from_json(&body)?;
    if names.is_empty() {
        return Err(AppError::invalid_param("names"));
    }
    app.emoji_on()?;
    if names.len() > 200 {
        let mut e = AppError::bad_request("api.emoji.get_multiple_by_name_too_many.request_error", "");
        if let Some(m) = crate::i18n::tr(&e.id, &[("MaxNames", "200")]) {
            e.message = m;
        }
        return Err(e);
    }
    names.retain(|n| !is_system_emoji(n));
    if names.is_empty() {
        return Ok(Json(vec![]));
    }
    let found = app.emojis_by_names(&names).await?;
    Ok(Json(found.iter().map(|e| (**e).clone()).collect()))
}

/// `SqlEmojiStore.Search`.
async fn search_emoji(app: &App, name: &str, prefix: bool, limit: i64) -> ApiResult<Vec<Emoji>> {
    app.emoji_on()?;
    let mut term = String::new();
    for c in name.chars().filter(|c| *c != '\\') {
        if c == '%' || c == '_' {
            term.push('\\');
        }
        term.push(c);
    }
    let pattern = if prefix { format!("{term}%") } else { format!("%{term}%") };
    let sql = format!("{} WHERE DeleteAt = 0 AND Name LIKE $1 ORDER BY Name LIMIT $2", Emoji::SELECT);
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(pattern).bind(limit).fetch_all(&app.db).await?)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EmojiSearch {
    term: String,
    prefix_only: bool,
}

async fn search(St(app): St<State>, _: Auth, Body(q): Body<EmojiSearch>) -> ApiResult<Json<Vec<Emoji>>> {
    if q.term.is_empty() {
        return Err(AppError::invalid_param("term"));
    }
    Ok(Json(search_emoji(&app, &q.term, q.prefix_only, 200).await?))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AutocompleteQuery {
    name: String,
}

async fn autocomplete(St(app): St<State>, _: Auth, Query(q): Query<AutocompleteQuery>) -> ApiResult<Json<Vec<Emoji>>> {
    if q.name.is_empty() {
        return Err(AppError::invalid_url_param("name"));
    }
    Ok(Json(search_emoji(&app, &q.name, true, 100).await?))
}

/// `getEmojiImage`: `emoji/{id}/image`, its type sniffed.
async fn image(St(app): St<State>, _: Auth, Path(id): Path<String>) -> ApiResult<Response> {
    if !is_valid_id(&id) {
        return Err(AppError::invalid_url_param("emoji_id"));
    }
    app.emoji_on()?;
    app.emoji(&id).await?.ok_or_else(no_result)?;
    let data = app
        .files
        .read(&format!("emoji/{id}/image"))
        .await
        .map_err(|e| AppError::not_found("api.emoji.get_image.read.app_error", "").detail(e.to_string()))?;
    let kind = match image::guess_format(&data) {
        Ok(image::ImageFormat::Png) => "png",
        Ok(image::ImageFormat::Jpeg) => "jpeg",
        Ok(image::ImageFormat::Gif) => "gif",
        Ok(image::ImageFormat::Bmp) => "bmp",
        _ => return Err(AppError::internal("api.emoji.get_image.decode.app_error", "unknown image type")),
    };
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&format!("image/{kind}")).unwrap());
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=2592000, private"));
    Ok(r)
}

// ---------- writes ----------

/// `createEmoji`: multipart `emoji` (JSON) and `image`.
async fn create(
    St(app): St<State>,
    Auth(s): Auth,
    form: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> ApiResult<Json<Emoji>> {
    app.emoji_on()?;
    let parse_err = |e: String| AppError::bad_request("api.emoji.create.parse.app_error", "").detail(e);
    // `ParseMultipartForm` fails before the permission check: not a multipart body.
    let mut form = form.map_err(|e| parse_err(e.body_text()))?;
    if !app.perm_anywhere(&s, perm::CREATE_EMOJIS).await {
        return Err(AppError::forbidden_perm(perm::CREATE_EMOJIS));
    }
    let mut emoji_json = None;
    let mut image: Option<(String, Vec<u8>)> = None;
    while let Some(field) = form.next_field().await.map_err(|e| parse_err(e.to_string()))? {
        match field.name() {
            Some("emoji") => emoji_json = Some(field.text().await.map_err(|e| parse_err(e.to_string()))?),
            Some("image") if image.is_none() => {
                let name = field.file_name().unwrap_or("").to_string();
                let data = field.bytes().await.map_err(|e| parse_err(e.to_string()))?;
                if data.len() > MAX_FILE_SIZE {
                    return Err(AppError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "api.emoji.create.too_large.app_error",
                        "",
                    ));
                }
                image = Some((name, data.to_vec()));
            }
            _ => {}
        }
    }
    let mut emoji: Emoji =
        emoji_json.and_then(|j| serde_json::from_str(&j).ok()).ok_or_else(|| AppError::invalid_param("emoji"))?;
    // PreSave, IsValid
    emoji.id = new_id();
    emoji.name = emoji.name.to_lowercase();
    emoji.create_at = now_ms();
    emoji.update_at = emoji.create_at;
    emoji.delete_at = 0;
    if emoji.creator_id.len() > 26 {
        return Err(AppError::bad_request("model.emoji.user_id.app_error", ""));
    }
    check_name(&emoji.name)?;
    if emoji.creator_id != s.user_id {
        return Err(AppError::new(StatusCode::FORBIDDEN, "api.emoji.create.other_user.app_error", ""));
    }
    if app.emoji_by_name(&emoji.name).await?.is_some() {
        return Err(AppError::bad_request("api.emoji.create.duplicate.app_error", ""));
    }
    let Some((_, data)) = image else { return Err(AppError::invalid_param("createEmoji")) };
    let stored = emoji_image(&data)?;
    app.files
        .write(&format!("emoji/{}/image", emoji.id), stored.into())
        .await
        .map_err(|e| AppError::internal("api.file.upload_file.storage.app_error", e.to_string()))?;
    db::insert(&app.db, &emoji)
        .await
        .map_err(|e| AppError::internal("app.emoji.create.internal_error", e.to_string()))?;
    // LocalCacheEmojiStore.Save: a cached "not found" for the name goes.
    app.caches.emoji_by_name.remove(emoji.name.as_str());
    app.hub.publish("emoji_added", json!({ "emoji": serde_json::to_string(&emoji).unwrap() }), Broadcast::default());
    Ok(Json(emoji))
}

/// `uploadEmojiImage`: limits checked; small images kept as they are, bigger ones fitted into 128×128 (PNG, or
/// GIF for GIFs).
fn emoji_image(data: &[u8]) -> ApiResult<Vec<u8>> {
    let bad = |e: String| AppError::bad_request("api.emoji.upload.image.app_error", "").detail(e);
    let format = image::guess_format(data).map_err(|e| bad(e.to_string()))?;
    let reader = image::ImageReader::with_format(std::io::Cursor::new(data), format);
    let (w, h) = reader.into_dimensions().map_err(|e| bad(e.to_string()))?;
    if w > MAX_ORIGINAL || h > MAX_ORIGINAL {
        let mut e = AppError::bad_request("api.emoji.upload.large_image.too_large.app_error", "");
        if let Some(m) = crate::i18n::tr(&e.id, &[("MaxWidth", "1028"), ("MaxHeight", "1028")]) {
            e.message = m;
        }
        return Err(e);
    }
    let gif = format == image::ImageFormat::Gif;
    if gif {
        use image::AnimationDecoder;
        let frames = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(data))
            .map_err(|e| bad(e.to_string()))?
            .into_frames()
            .count();
        if frames > MAX_GIF_FRAMES {
            let mut e = AppError::bad_request("api.emoji.upload.too_many_frames.app_error", "");
            if let Some(m) = crate::i18n::tr(&e.id, &[("MaxFrames", "70")]) {
                e.message = m;
            }
            return Err(e);
        }
    }
    if w <= MAX_WIDTH && h <= MAX_HEIGHT {
        return Ok(data.to_vec());
    }
    let decode_err = |e: String| AppError::bad_request("api.emoji.upload.large_image.decode_error", "").detail(e);
    let mut out = std::io::Cursor::new(Vec::new());
    if gif {
        use image::AnimationDecoder;
        let frames = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(data))
            .map_err(|e| decode_err(e.to_string()))?
            .into_frames()
            .collect_frames()
            .map_err(|e| decode_err(e.to_string()))?;
        let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite).map_err(|e| decode_err(e.to_string()))?;
        for f in frames {
            let delay = f.delay();
            let img = image::DynamicImage::ImageRgba8(f.into_buffer()).resize(
                MAX_WIDTH,
                MAX_HEIGHT,
                image::imageops::FilterType::Lanczos3,
            );
            enc.encode_frame(image::Frame::from_parts(img.to_rgba8(), 0, 0, delay)).map_err(|e| {
                AppError::bad_request("api.emoji.upload.large_image.gif_encode_error", "").detail(e.to_string())
            })?;
        }
        drop(enc);
    } else {
        let img = image::load_from_memory(data).map_err(|e| decode_err(e.to_string()))?;
        img.resize(MAX_WIDTH, MAX_HEIGHT, image::imageops::FilterType::Lanczos3)
            .write_to(&mut out, image::ImageFormat::Png)
            .map_err(|e| {
                AppError::bad_request("api.emoji.upload.large_image.encode_error", "").detail(e.to_string())
            })?;
    }
    Ok(out.into_inner())
}

/// `deleteEmoji`: soft delete, image moved aside, reactions with it removed.
async fn delete(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    if !is_valid_id(&id) {
        return Err(AppError::invalid_url_param("emoji_id"));
    }
    app.emoji_on()?;
    let e = app.emoji(&id).await?.ok_or_else(no_result)?;
    if !app.perm_anywhere(&s, perm::DELETE_EMOJIS).await {
        return Err(AppError::forbidden_perm(perm::DELETE_EMOJIS));
    }
    if s.user_id != e.creator_id && !app.perm_anywhere(&s, perm::DELETE_OTHERS_EMOJIS).await {
        return Err(AppError::forbidden_perm(perm::DELETE_OTHERS_EMOJIS));
    }
    let now = now_ms();
    let n = sqlx::query("UPDATE Emoji SET DeleteAt = $1, UpdateAt = $1 WHERE Id = $2 AND DeleteAt = 0")
        .bind(now)
        .bind(&e.id)
        .execute(&app.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("app.emoji.delete.no_results", ""));
    }
    app.caches.emoji_by_id.remove(e.id.as_str());
    app.caches.emoji_by_name.remove(e.name.as_str());
    let from = format!("emoji/{}/image", e.id);
    if let Ok(data) = app.files.read(&from).await {
        let _ = app.files.write(&format!("emoji/{}/image_deleted", e.id), data).await;
        let _ = app.files.remove(&from).await;
    }
    // deleteReactionsForEmoji: SqlReactionStore.DeleteAllWithEmojiName.
    let posts: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT PostId FROM Reactions WHERE EmojiName = $1 AND DeleteAt = 0")
            .bind(&e.name)
            .fetch_all(&app.db)
            .await?;
    sqlx::query("UPDATE Reactions SET UpdateAt = $1, DeleteAt = $1 WHERE EmojiName = $2 AND DeleteAt = 0")
        .bind(now)
        .bind(&e.name)
        .execute(&app.db)
        .await?;
    for (p,) in posts {
        app.caches.reactions.remove(p.as_str());
        let _ = sqlx::query(
            "UPDATE Posts SET HasReactions = (SELECT COUNT(0) > 0 FROM Reactions WHERE PostId = $1 AND DeleteAt = 0),
                 UpdateAt = $2 WHERE Id = $1",
        )
        .bind(&p)
        .bind(now)
        .execute(&app.db)
        .await;
    }
    Ok(crate::users::ok())
}

#[cfg(test)]
mod tests {
    #[test]
    fn system_emoji_codes() {
        assert_eq!(super::system_emoji_code("ghost"), Some("1f47b"));
        assert_eq!(super::system_emoji_code("+1"), Some("1f44d"));
        assert_eq!(super::system_emoji_code("no_such_emoji"), None);
    }
}
