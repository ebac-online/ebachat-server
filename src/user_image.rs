//! Profile images: stored ones (`users/{id}/profile.png`), the default drawn from the initial, uploads.

// Based on Mattermost server/channels/api4/user.go (getProfileImage, setProfileImage, ...), app/user.go
// (AdjustImage, SetProfileImageFromFile) and app/users/profile_picture.go (createProfileImage),
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use axum::extract::{Multipart, Path, State as St};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use image::{ImageFormat, Rgba, RgbaImage};
use serde_json::Value;

use crate::app::{ApiResult, App, AppError, Auth, State, now_ms};
use crate::model::{Show, User};
use crate::preferences::user_param;
use crate::users::{missing_account, ok};

const SIZE: u32 = 128;
static FONT: &[u8] = include_bytes!("../data/fonts/nunito-bold.ttf");
static BOT_ICON: &[u8] = include_bytes!("../data/bot_default_icon.png");

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/users/{user_id}/image", get(get_image).post(set_image).delete(set_default))
        .route("/api/v4/users/{user_id}/image/default", get(get_default))
}

fn profile_path(user_id: &str) -> String {
    format!("users/{user_id}/profile.png")
}

/// FNV-1a, 32 bits.
fn fnv32a(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// `createProfileImage`: the first letter of the username, white on a colour picked by the user id.
pub fn initials_image(username: &str, user_id: &str) -> Vec<u8> {
    const COLORS: [[u8; 3]; 26] = [
        [197, 8, 126],
        [227, 207, 18],
        [28, 181, 105],
        [35, 188, 224],
        [116, 49, 196],
        [197, 8, 126],
        [197, 19, 19],
        [250, 134, 6],
        [227, 207, 18],
        [123, 201, 71],
        [28, 181, 105],
        [35, 188, 224],
        [116, 49, 196],
        [197, 8, 126],
        [197, 19, 19],
        [250, 134, 6],
        [227, 207, 18],
        [123, 201, 71],
        [28, 181, 105],
        [35, 188, 224],
        [116, 49, 196],
        [197, 8, 126],
        [197, 19, 19],
        [250, 134, 6],
        [227, 207, 18],
        [123, 201, 71],
    ];
    let c = COLORS[(fnv32a(user_id) as usize) % COLORS.len()];
    let mut img = RgbaImage::from_pixel(SIZE, SIZE, Rgba([c[0], c[1], c[2], 255]));
    let initial = username.to_uppercase().chars().next().unwrap_or('?');
    static F: OnceLock<FontRef<'static>> = OnceLock::new();
    let font = F.get_or_init(|| FontRef::try_from_slice(FONT).expect("initials font"));
    // Go's truetype face: 64 pt at 72 dpi, i.e. 64 px per em.
    let scale = font.pt_to_px_scale(64.0).unwrap_or(PxScale::from(64.0));
    let scaled = font.as_scaled(scale);
    let id = font.glyph_id(initial);
    let advance = scaled.h_advance(id);
    // Go: the origin at x = (128 - advance) / 2 and the baseline at (128 + glyph height) / 2, which centres
    // the glyph's ink vertically.
    let x = (SIZE as f32 - advance) / 2.0;
    let glyph = id.with_scale_and_position(scale, ab_glyph::point(x, 0.0));
    if let Some(outline) = font.outline_glyph(glyph) {
        let b = outline.px_bounds();
        let top = ((SIZE as f32 - b.height()) / 2.0).round() as i32;
        outline.draw(|gx, gy, cov| {
            let (px, py) = (b.min.x as i32 + gx as i32, top + gy as i32);
            if px < 0 || py < 0 || px >= SIZE as i32 || py >= SIZE as i32 {
                return;
            }
            let p = img.get_pixel_mut(px as u32, py as u32);
            for c in p.0.iter_mut().take(3) {
                *c = (*c as f32 * (1.0 - cov) + 255.0 * cov).round() as u8;
            }
        });
    }
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).expect("png");
    out.into_inner()
}

impl App {
    /// `GetDefaultProfileImage`.
    pub fn default_profile_image(&self, u: &User) -> Vec<u8> {
        if u.is_bot { BOT_ICON.to_vec() } else { initials_image(&u.username, &u.id) }
    }

    /// `UserService.GetProfileImage`: the stored image, or the default (stored for next time when the user never
    /// set one); the bool says the read failed.
    pub async fn profile_image(&self, u: &User) -> (Vec<u8>, bool) {
        let path = profile_path(&u.id);
        match self.files.read(&path).await {
            Ok(b) => (b.to_vec(), false),
            Err(_) => {
                let img = self.default_profile_image(u);
                if u.last_picture_update == 0 {
                    let _ = self.files.write(&path, img.clone().into()).await;
                }
                (img, true)
            }
        }
    }

    /// `UpdateDefaultProfileImage`: the default image stored, `LastPictureUpdate` reset to a negative time.
    pub async fn update_default_profile_image(&self, u: &User) -> ApiResult<()> {
        let img = self.default_profile_image(u);
        self.files
            .write(&profile_path(&u.id), img.into())
            .await
            .map_err(|e| AppError::internal("api.file.write_file.app_error", e.to_string()))?;
        sqlx::query("UPDATE Users SET LastPictureUpdate = $1, UpdateAt = $2 WHERE Id = $3")
            .bind(-now_ms())
            .bind(now_ms())
            .bind(&u.id)
            .execute(&self.db)
            .await?;
        self.invalidate_user(&u.id);
        Ok(())
    }
}

fn png(data: Vec<u8>, cache: &str) -> Response {
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_str(cache).unwrap());
    r
}

/// `getProfileImage`: ETag is the picture's update time.
async fn get_image(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let user_id = user_param(&s, &user_id)?;
    if !app.can_see_user(&s, &user_id).await? {
        return Err(AppError::forbidden_perm(crate::perm::VIEW_MEMBERS));
    }
    let u = app.user(&user_id).await?.ok_or_else(missing_account)?;
    let etag = u.last_picture_update.to_string();
    if let Some(r) = crate::teams::not_modified(&headers, &etag) {
        return Ok(r);
    }
    let (img, failed) = app.profile_image(&u).await;
    if failed {
        return Ok(png(img, "max-age=300, private"));
    }
    let mut r = png(img, "max-age=86400, private");
    r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    Ok(r)
}

async fn get_default(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Response> {
    let user_id = user_param(&s, &user_id)?;
    if !app.can_see_user(&s, &user_id).await? {
        return Err(AppError::forbidden_perm(crate::perm::VIEW_MEMBERS));
    }
    let u = app.user(&user_id).await?.ok_or_else(missing_account)?;
    Ok(png(app.default_profile_image(&u), "max-age=86400, private"))
}

/// `setProfileImage`: multipart `image`, made upright and filled into 128×128 PNG.
async fn set_image(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    req: axum::extract::Request,
) -> ApiResult<Json<Value>> {
    use axum::extract::FromRequest;
    let user_id = user_param(&s, &user_id)?;
    if !crate::user_edit::user_or_bot_perm(&app, &s, &user_id).await? {
        return Err(AppError::forbidden_perm(crate::perm::EDIT_OTHER_USERS));
    }
    let length = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<i64>().ok());
    if length.is_some_and(|l| l > app.cfg.i64("FileSettings.MaxFileSize")) {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "api.user.upload_profile_user.too_large.app_error",
            "",
        ));
    }
    let mut form = Multipart::from_request(req, &app)
        .await
        .map_err(|e| AppError::internal("api.user.upload_profile_user.parse.app_error", e.to_string()))?;
    let mut data = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| AppError::internal("api.user.upload_profile_user.parse.app_error", e.to_string()))?
    {
        if field.name() == Some("image") {
            data = Some(
                field
                    .bytes()
                    .await
                    .map_err(|e| AppError::internal("api.user.upload_profile_user.parse.app_error", e.to_string()))?,
            );
            break;
        }
    }
    let data = data.ok_or_else(|| AppError::bad_request("api.user.upload_profile_user.no_file.app_error", ""))?;
    if data.len() as i64 > app.cfg.i64("FileSettings.MaxFileSize") {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "api.user.upload_profile_user.too_large.app_error",
            "",
        ));
    }
    let u = app.user(&user_id).await?.ok_or_else(|| AppError::invalid_url_param("user_id"))?;
    // `SetProfileImageFromMultiPartFile`: `checkImageLimits` reads the dimensions first, so a file that is no image
    // fails as `check_image_limits` here (as `decode` only on the other paths).
    image::ImageReader::new(std::io::Cursor::new(&data))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .ok_or_else(|| AppError::bad_request("api.user.upload_profile_user.check_image_limits.app_error", ""))?;
    app.set_profile_image_from_bytes(&u.id, &data).await?;
    crate::audit::log(&app, Some(&s), "").await;
    Ok(ok())
}

impl App {
    /// `checkImageLimits` + `SetProfileImageFromFile`: made upright, filled into 128×128 PNG and stored unless the
    /// same image is there already; `LastPictureUpdate` and `user_updated`.
    pub async fn set_profile_image_from_bytes(&self, user_id: &str, data: &[u8]) -> ApiResult<()> {
        let app = self;
        let decode = |e: String| AppError::bad_request("api.user.upload_profile_user.decode.app_error", "").detail(e);
        let reader = image::ImageReader::new(std::io::Cursor::new(data))
            .with_guessed_format()
            .map_err(|e| decode(e.to_string()))?;
        let (w, h) = reader.into_dimensions().map_err(|e| decode(e.to_string()))?;
        let max_res = app.cfg.i64("FileSettings.MaxImageResolution");
        if max_res > 0 && (w as i64) * (h as i64) > max_res {
            return Err(AppError::bad_request("api.user.upload_profile_user.check_image_limits.app_error", ""));
        }
        let img = image::load_from_memory(data).map_err(|e| decode(e.to_string()))?;
        let img = upright(img, data).resize_to_fill(SIZE, SIZE, image::imageops::FilterType::Lanczos3);
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png)
            .map_err(|e| AppError::internal("api.user.upload_profile_user.encode.app_error", e.to_string()))?;
        let out = out.into_inner();
        let path = profile_path(user_id);
        if let Ok(old) = app.files.read(&path).await
            && old.as_ref() == out.as_slice()
        {
            return Ok(());
        }
        app.files
            .write(&path, out.into())
            .await
            .map_err(|e| AppError::internal("api.user.upload_profile_user.upload_profile.app_error", e.to_string()))?;
        let now = now_ms();
        sqlx::query("UPDATE Users SET LastPictureUpdate = $1, UpdateAt = $1 WHERE Id = $2")
            .bind(now)
            .bind(user_id)
            .execute(&app.db)
            .await?;
        app.publish_user_updated(user_id).await;
        Ok(())
    }
}

/// `MakeImageUpright` from the EXIF orientation of a JPEG.
fn upright(img: image::DynamicImage, data: &[u8]) -> image::DynamicImage {
    match exif_orientation(data) {
        Some(2) => img.fliph(),
        Some(3) => img.rotate180(),
        Some(4) => img.flipv(),
        Some(5) => img.rotate90().fliph(),
        Some(6) => img.rotate90(),
        Some(7) => img.rotate270().fliph(),
        Some(8) => img.rotate270(),
        _ => img,
    }
}

/// The orientation tag (0x0112) of a JPEG's APP1 Exif block.
fn exif_orientation(d: &[u8]) -> Option<u16> {
    if d.len() < 4 || d[0] != 0xFF || d[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 4 <= d.len() {
        if d[i] != 0xFF {
            return None;
        }
        let marker = d[i + 1];
        let len = u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize;
        if marker == 0xE1 && d.get(i + 4..i + 10) == Some(b"Exif\0\0") {
            let t = &d.get(i + 10..i + 2 + len)?;
            let le = t.get(0..2)? == b"II";
            let u16_at = |o: usize| -> Option<u16> {
                let b = t.get(o..o + 2)?;
                Some(if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
            };
            let u32_at = |o: usize| -> Option<u32> {
                let b = t.get(o..o + 4)?;
                Some(if le {
                    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
                } else {
                    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
                })
            };
            let ifd = u32_at(4)? as usize;
            let n = u16_at(ifd)? as usize;
            for k in 0..n {
                let e = ifd + 2 + k * 12;
                if u16_at(e)? == 0x0112 {
                    return u16_at(e + 8);
                }
            }
            return None;
        }
        if marker == 0xDA {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// `setDefaultProfileImage`.
async fn set_default(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    let u = app.user(&user_id).await?.ok_or_else(missing_account)?;
    app.update_default_profile_image(&u).await?;
    // SetDefaultProfileImage: the sanitized profile to everyone.
    if let Ok(Some(u)) = app.user(&user_id).await {
        let mut u = (*u).clone();
        u.sanitize_profile(Show {
            email: app.cfg.bool("PrivacySettings.ShowEmailAddress"),
            fullname: app.cfg.bool("PrivacySettings.ShowFullName"),
            admin: false,
        });
        app.hub.publish("user_updated", serde_json::json!({ "user": u }), crate::hub::Broadcast::default());
    }
    crate::audit::log(&app, Some(&s), "").await;
    Ok(ok())
}

#[cfg(test)]
mod tests {
    #[test]
    fn draws_an_initial() {
        let png = super::initials_image("bench", "zmkn3dkahbf9ubi7is8e7gjjxw");
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (128, 128));
        // Some white pixels (the letter) and the background colour in a corner.
        assert!(img.pixels().any(|p| p.0[..3] == [255, 255, 255]));
        assert_ne!(img.get_pixel(0, 0).0[..3], [255, 255, 255]);
    }
}
