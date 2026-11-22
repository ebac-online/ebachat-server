//! Custom profile attributes: the user fields of the `access_control` property group. Without an Enterprise license
//! Go's property service refuses any field or value of that group it finds (`LicenseCheckHook`): reads that find
//! nothing answer empty, anything that reaches a stored field or value is `app.property.license_error`.

// Based on Mattermost server/channels/api4/custom_profile_attributes.go, api4/properties.go (hasTargetAccess) and
// app/properties/license_check.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use crate::app::{ApiResult, App, AppError, Auth, State, decode_json, is_valid_id};
use crate::model::Session;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/custom_profile_attributes/fields", get(list_fields).post(create_field))
        .route("/api/v4/custom_profile_attributes/fields/{field_id}", patch(patch_field).delete(delete_field))
        .route("/api/v4/custom_profile_attributes/values", patch(patch_own_values))
        .route("/api/v4/users/{user_id}/custom_profile_attributes", get(list_values).patch(patch_user_values))
}

/// `LicenseCheckHook`: a stored field or value of the group, without a license.
pub fn license_error() -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        "app.property.license_error",
        "Your license does not support this property group.",
    )
}

/// `GetPropertyGroup(access_control)`.
async fn group(app: &App) -> ApiResult<String> {
    let g: Option<String> = sqlx::query_scalar("SELECT ID FROM PropertyGroups WHERE Name = 'access_control'")
        .fetch_optional(&app.db)
        .await?;
    g.ok_or_else(|| AppError::not_found("app.property_group.get.app_error", ""))
}

fn require_field_id(id: &str) -> ApiResult<()> {
    if is_valid_id(id) { Ok(()) } else { Err(AppError::invalid_url_param("field_id")) }
}

/// `GetPropertyField` of the group: missing is a 404, found is refused.
async fn stored_field(app: &App, group: &str, id: &str) -> AppError {
    let n: Result<i64, _> =
        sqlx::query_scalar("SELECT COUNT(*) FROM PropertyFields WHERE GroupID = $1 AND ID = $2 AND DeleteAt = 0")
            .bind(group)
            .bind(id)
            .fetch_one(&app.db)
            .await;
    match n {
        Err(e) => e.into(),
        Ok(0) => AppError::not_found("app.property.not_found.app_error", "The specified property does not exist."),
        Ok(_) => license_error(),
    }
}

/// `listCPAFields`: the group's user fields, refused when there are any.
async fn list_fields(St(app): St<State>, Auth(_): Auth) -> ApiResult<Json<Value>> {
    let g = group(&app).await?;
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM PropertyFields WHERE GroupID = $1 AND ObjectType = 'user' AND DeleteAt = 0",
    )
    .bind(&g)
    .fetch_one(&app.db)
    .await?;
    if n > 0 {
        return Err(license_error());
    }
    Ok(Json(json!([])))
}

/// `createCPAField`: system admins only, and the license hook refuses the field.
async fn create_field(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    let v: Value = decode_json(&body, "property_field")?;
    if !v.is_object() {
        return Err(AppError::invalid_param("property_field"));
    }
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    group(&app).await?;
    Err(license_error())
}

/// `patchCPAField`.
async fn patch_field(St(app): St<State>, Auth(_): Auth, Path(id): Path<String>, body: Bytes) -> ApiResult<Json<Value>> {
    require_field_id(&id)?;
    let v: Value = decode_json(&body, "property_field_patch")?;
    if !v.is_object() {
        return Err(AppError::invalid_param("property_field_patch"));
    }
    let g = group(&app).await?;
    Err(stored_field(&app, &g, &id).await)
}

/// `deleteCPAField`.
async fn delete_field(St(app): St<State>, Auth(_): Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    require_field_id(&id)?;
    let g = group(&app).await?;
    Err(stored_field(&app, &g, &id).await)
}

/// `listCPAValues`: the user's values, refused when there are any.
async fn list_values(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<Value>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    if user_id != s.user_id && !app.user_can_see(&s, &user_id).await? {
        return Err(AppError::forbidden_perm("view_members"));
    }
    let g = group(&app).await?;
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM PropertyValues WHERE GroupID = $1 AND TargetID = $2 AND TargetType = 'user' AND DeleteAt = 0",
    )
    .bind(&g)
    .bind(&user_id)
    .fetch_one(&app.db)
    .await?;
    if n > 0 {
        return Err(license_error());
    }
    Ok(Json(Value::Object(Map::new())))
}

/// `patchCPAValues`: the session's own values.
async fn patch_own_values(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Json<Value>> {
    let updates: std::collections::BTreeMap<String, Value> = decode_json(&body, "value")?;
    patch_values(&app, &s, &s.user_id.clone(), updates).await
}

/// `patchCPAValuesForUser`.
async fn patch_user_values(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    let updates: std::collections::BTreeMap<String, Value> = decode_json(&body, "value")?;
    patch_values(&app, &s, &user_id, updates).await
}

/// `cpaPatchValues` up to `GetPropertyFields`: every field must exist, and the fields found are refused.
async fn patch_values(
    app: &App,
    s: &Session,
    user_id: &str,
    updates: std::collections::BTreeMap<String, Value>,
) -> ApiResult<Json<Value>> {
    let g = group(app).await?;
    // userTargetAccess (write): oneself, else edit_other_users (or one's own bot), and system admins only by one.
    if user_id != s.user_id {
        let bot_manager = crate::user_edit::user_or_bot_perm(app, s, user_id).await?
            && app.users(&[user_id.to_string()]).await?.iter().any(|u| u.is_bot);
        if !app.has_perm(s, crate::perm::EDIT_OTHER_USERS).await && !bot_manager {
            return Err(AppError::forbidden_perm(crate::perm::EDIT_OTHER_USERS));
        }
        if !app.has_perm(s, crate::perm::MANAGE_SYSTEM).await {
            let target = app.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
            if target.is_system_admin() {
                return Err(AppError::forbidden_perm(crate::perm::MANAGE_SYSTEM));
            }
        }
    }
    if updates.is_empty() {
        return Err(AppError::bad_request("api.property_value.patch.empty_body.app_error", ""));
    }
    if updates.len() > 100 {
        return Err(
            AppError::bad_request("api.property_value.patch.too_many_items.request_error", "").with_param("Max", "100")
        );
    }
    let ids: Vec<String> = updates.keys().cloned().collect();
    if ids.iter().any(|id| !is_valid_id(id)) {
        return Err(AppError::bad_request("api.property_value.patch.invalid_field_id.app_error", ""));
    }
    let found: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM PropertyFields WHERE GroupID = $1 AND ID = ANY($2) AND DeleteAt = 0")
            .bind(&g)
            .bind(&ids)
            .fetch_one(&app.db)
            .await?;
    if (found as usize) < ids.len() {
        // PropertyService.GetPropertyFields: every id must be a field of the group.
        return Err(AppError::not_found("app.property_field.not_found.app_error", ""));
    }
    Err(license_error())
}
