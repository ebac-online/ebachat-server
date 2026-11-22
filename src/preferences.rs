//! Preferences (`/users/{user_id}/preferences`), and the favorites sidebar category they keep in sync.

// Based on Mattermost server/channels/api4/preference.go, app/preference.go, store/sqlstore/preference_store.go
// and channel_store_categories.go (UpdateSidebarChannelsByPreferences), Copyright (c) 2015-present Mattermost,
// Inc. Licensed under AGPL-3.0.

use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, Body, State, is_valid_id};
use crate::db::Table;
use crate::hub::Broadcast;
use crate::model::{Preference, Session};

pub const FLAGGED_POST: &str = "flagged_post";
pub const FAVORITE_CHANNEL: &str = "favorite_channel";
pub const THEME: &str = "theme";
pub const CUSTOM_STATUS: &str = "custom_status";
const MAX_UPDATE: usize = 100;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/users/{user_id}/preferences", get(get_all).put(update))
        .route("/api/v4/users/{user_id}/preferences/delete", post(delete))
        .route("/api/v4/users/{user_id}/preferences/{category}", get(get_category))
        .route("/api/v4/users/{user_id}/preferences/{category}/name/{preference_name}", get(get_one))
}

impl App {
    pub async fn preferences(&self, user_id: &str) -> ApiResult<Vec<Preference>> {
        let sql = format!("{} WHERE userid = $1", Preference::SELECT);
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(user_id).fetch_all(&self.db).await?)
    }

    pub async fn preference(&self, user_id: &str, category: &str, name: &str) -> ApiResult<Option<Preference>> {
        let sql = format!("{} WHERE userid = $1 AND category = $2 AND name = $3", Preference::SELECT);
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(user_id)
            .bind(category)
            .bind(name)
            .fetch_optional(&self.db)
            .await?)
    }

    /// `App.UpdatePreferences`: upserted in one transaction, favorites synced to the sidebar,
    /// `sidebar_category_updated` and `preferences_changed` to the user.
    pub async fn update_preferences(&self, user_id: &str, prefs: Vec<Preference>) -> ApiResult<()> {
        if prefs.iter().any(|p| p.user_id != user_id) {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "api.preference.update_preferences.set.app_error",
                "Unable to set user preferences.",
            ));
        }
        let mut prefs = prefs;
        let mut tx = self.db.begin().await?;
        for p in &mut prefs {
            pre_update(p);
            is_valid(p)?;
            sqlx::query(
                "INSERT INTO Preferences (UserId, Category, Name, Value) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (userid, category, name) DO UPDATE SET Value = $4",
            )
            .bind(&p.user_id)
            .bind(&p.category)
            .bind(&p.name)
            .bind(&p.value)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                AppError::bad_request("app.preference.save.updating.app_error", "Unable to update the preference.")
                    .detail(e.to_string())
            })?;
        }
        tx.commit().await?;
        let mut tx = self.db.begin().await?;
        for p in prefs.iter().filter(|p| p.category == FAVORITE_CHANNEL) {
            if p.value == "false" {
                remove_favorite(&mut tx, p).await?;
            } else {
                add_favorite(&mut tx, p).await?;
            }
        }
        tx.commit().await?;
        self.publish_preferences(user_id, "preferences_changed", &prefs);
        Ok(())
    }

    /// `App.DeletePreferences`.
    pub async fn delete_preferences(&self, user_id: &str, prefs: Vec<Preference>) -> ApiResult<()> {
        if prefs.iter().any(|p| p.user_id != user_id) {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "api.preference.delete_preferences.delete.app_error",
                "Unable to delete user preferences.",
            ));
        }
        for p in &prefs {
            sqlx::query("DELETE FROM Preferences WHERE UserId = $1 AND Category = $2 AND Name = $3")
                .bind(user_id)
                .bind(&p.category)
                .bind(&p.name)
                .execute(&self.db)
                .await?;
        }
        let mut tx = self.db.begin().await?;
        for p in prefs.iter().filter(|p| p.category == FAVORITE_CHANNEL) {
            remove_favorite(&mut tx, p).await?;
        }
        tx.commit().await?;
        self.publish_preferences(user_id, "preferences_deleted", &prefs);
        Ok(())
    }

    fn publish_preferences(&self, user_id: &str, event: &str, prefs: &[Preference]) {
        let to_user = || Broadcast { user_id: user_id.into(), ..Default::default() };
        self.hub.publish("sidebar_category_updated", json!({}), to_user());
        let s = serde_json::to_string(prefs).unwrap();
        self.hub.publish(event, json!({ "preferences": s }), to_user());
    }
}

/// `Preference.PreUpdate`: invalid theme colours become white.
pub(crate) fn pre_update(p: &mut Preference) {
    if p.category != THEME {
        return;
    }
    let Ok(mut props) = serde_json::from_str::<std::collections::BTreeMap<String, Value>>(&p.value) else {
        return;
    };
    let color = regex::Regex::new(r"^#[0-9a-fA-F]{3}([0-9a-fA-F]{3})?$").unwrap();
    for (k, v) in props.iter_mut() {
        if k == "image" || k == "type" || k == "codeTheme" {
            continue;
        }
        if !v.as_str().is_some_and(|s| color.is_match(s)) {
            *v = Value::String("#ffffff".into());
        }
    }
    p.value = serde_json::to_string(&props).unwrap();
}

/// `Preference.IsValid`.
pub(crate) fn is_valid(p: &Preference) -> ApiResult<()> {
    let bad = |id: &str, field: &str| Err(AppError::bad_request(id, &format!("Invalid {field}.")));
    if !is_valid_id(&p.user_id) {
        return bad("model.preference.is_valid.id.app_error", "user id");
    }
    if p.category.is_empty() || p.category.len() > 32 {
        return bad("model.preference.is_valid.category.app_error", "category");
    }
    if p.name.len() > 32 {
        return bad("model.preference.is_valid.name.app_error", "name");
    }
    if p.value.chars().count() > 20000 {
        return bad("model.preference.is_valid.value.app_error", "value");
    }
    if p.category == THEME && serde_json::from_str::<serde_json::Map<String, Value>>(&p.value).is_err() {
        return bad("model.preference.is_valid.theme.app_error", "theme");
    }
    if p.category == "sidebar_settings" && p.name == "limit_visible_dms_gms" {
        match p.value.parse::<i64>() {
            Ok(v) if (1..=40).contains(&v) => {}
            _ => return bad("model.preference.is_valid.limit_visible_dms_gms.app_error", "limit"),
        }
    }
    Ok(())
}

async fn remove_favorite(tx: &mut sqlx::PgConnection, p: &Preference) -> ApiResult<()> {
    sqlx::query(
        "DELETE FROM SidebarChannels USING SidebarCategories
         WHERE SidebarChannels.CategoryId = SidebarCategories.Id AND SidebarChannels.UserId = $1
           AND SidebarChannels.ChannelId = $2 AND SidebarCategories.Type = 'favorites'",
    )
    .bind(&p.user_id)
    .bind(&p.name)
    .execute(tx)
    .await?;
    Ok(())
}

/// `addChannelToFavoritesCategoryT`: first in the favorites of the channel's team (every team for DMs).
async fn add_favorite(tx: &mut sqlx::PgConnection, p: &Preference) -> ApiResult<()> {
    let team: Option<(String,)> = sqlx::query_as("SELECT COALESCE(TeamId, '') FROM Channels WHERE Id = $1")
        .bind(&p.name)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((team,)) = team else {
        return Err(AppError::internal(
            "api.preference.update_preferences.update_sidebar.app_error",
            "channel not found",
        ));
    };
    let ids: Vec<(String,)> = sqlx::query_as(
        "SELECT sc.Id FROM SidebarCategories sc
         LEFT JOIN SidebarChannels sch ON sc.Id = sch.CategoryId AND sch.ChannelId = $1
         WHERE sc.UserId = $2 AND sc.Type = 'favorites' AND sch.ChannelId IS NULL AND ($3 = '' OR sc.TeamId = $3)",
    )
    .bind(&p.name)
    .bind(&p.user_id)
    .bind(&team)
    .fetch_all(&mut *tx)
    .await?;
    if ids.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = ids.into_iter().map(|r| r.0).collect();
    sqlx::query(
        "INSERT INTO SidebarChannels (ChannelId, CategoryId, UserId, SortOrder)
         SELECT $1, sc.Id, $2, COALESCE(MIN(sch.SortOrder) - 10, 0)
         FROM SidebarCategories sc LEFT JOIN SidebarChannels sch ON sc.Id = sch.CategoryId
         WHERE sc.Id = ANY($3) GROUP BY sc.Id",
    )
    .bind(&p.name)
    .bind(&p.user_id)
    .bind(&ids)
    .execute(tx)
    .await?;
    Ok(())
}

/// The `{user_id}` of a path (`me` resolved), checked as `RequireUserId`.
pub fn user_param(s: &Session, id: &str) -> ApiResult<String> {
    let id = s.resolve(id);
    if !is_valid_id(&id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    Ok(id)
}

/// `getPreferences`: `null` for a user without preferences (the store's nil slice).
async fn get_all(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Option<Vec<Preference>>>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    let p = app.preferences(&user_id).await?;
    Ok(Json((!p.is_empty()).then_some(p)))
}

async fn get_category(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, category)): Path<(String, String)>,
) -> ApiResult<Json<Vec<Preference>>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    let all: Vec<Preference> =
        app.preferences(&user_id).await?.into_iter().filter(|p| p.category == category).collect();
    if all.is_empty() {
        return Err(AppError::not_found(
            "api.preference.preferences_category.get.app_error",
            "Unable to find preferences.",
        ));
    }
    Ok(Json(all))
}

async fn get_one(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, category, name)): Path<(String, String, String)>,
) -> ApiResult<Json<Preference>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    app.preference(&user_id, &category, &name)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::bad_request("app.preference.get.app_error", "Unable to find preference."))
}

async fn update(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Body(prefs): Body<Vec<Preference>>,
) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    if prefs.is_empty() || prefs.len() > MAX_UPDATE {
        return Err(AppError::invalid_param("preferences"));
    }
    for p in prefs.iter().filter(|p| p.category == FLAGGED_POST) {
        let ch: Option<(String,)> = sqlx::query_as("SELECT ChannelId FROM Posts WHERE Id = $1 AND DeleteAt = 0")
            .bind(&p.name)
            .fetch_optional(&app.db)
            .await?;
        let Some((ch,)) = ch else { return Err(AppError::invalid_param("preference.name")) };
        let c = app.channel_or_404(&ch).await?;
        if !app.can_read_channel(&s, &c).await?.0 {
            return Err(AppError::forbidden_perm(crate::perm::READ_CHANNEL_CONTENT));
        }
    }
    app.update_preferences(&user_id, prefs).await?;
    Ok(crate::users::ok())
}

async fn delete(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Body(prefs): Body<Vec<Preference>>,
) -> ApiResult<Json<Value>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    if prefs.is_empty() || prefs.len() > MAX_UPDATE {
        return Err(AppError::invalid_param("preferences"));
    }
    app.delete_preferences(&user_id, prefs).await?;
    Ok(crate::users::ok())
}
