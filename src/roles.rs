//! Roles (reading them, patching their permissions with the role caches dropped and `role_updated` sent) and
//! the scheme lists of the system console; creating, patching and deleting schemes needs a license.

// Based on Mattermost server/channels/api4/role.go, api4/scheme.go, app/role.go, app/scheme.go,
// public/model/role.go and store/sqlstore/role_store.go, scheme_store.go, store/localcachelayer/role_layer.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::sync::Arc;

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, State, is_valid_id, now_ms};
use crate::channels_api::paging;
use crate::db::{self, Table};
use crate::hub::Broadcast;
use crate::model::Spaced;
use crate::model::{Channel, Role, Scheme, Team};
use crate::team_admin::decode;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/roles", get(all_roles))
        .route("/api/v4/roles/{role_id}", get(get_role))
        .route("/api/v4/roles/name/{role_name}", get(role_by_name))
        .route("/api/v4/roles/{role_id}/patch", put(patch_role))
        .route("/api/v4/schemes", get(get_schemes).post(create_scheme))
        .route("/api/v4/schemes/{scheme_id}", delete(delete_scheme))
        .route("/api/v4/schemes/{scheme_id}/patch", put(patch_scheme))
        .route("/api/v4/schemes/{scheme_id}/teams", get(teams_for_scheme))
        .route("/api/v4/schemes/{scheme_id}/channels", get(channels_for_scheme))
}

/// `AllPermissions` and `DeprecatedPermissions`, dumped from the Go model.
pub(crate) const ALL_PERMISSIONS: &str = include_str!("../data/all_permissions.txt");

/// `NewSystemRoleIDs`.
const NEW_SYSTEM_ROLES: [&str; 4] =
    ["system_user_manager", "system_read_only_admin", "system_manager", "shared_channel_manager"];

/// `BuiltInSchemeManagedRoleIDs` without the channel roles, and `NewSystemRoleIDs`: a change to these touches no
/// channel scheme's role.
const NOT_CHANNEL_HIGHER_SCOPE: [&str; 21] = [
    "system_guest",
    "system_user",
    "system_admin",
    "system_post_all",
    "system_post_all_public",
    "system_user_access_token",
    "team_guest",
    "team_user",
    "team_admin",
    "team_post_all",
    "team_post_all_public",
    "custom_group_user",
    "system_custom_group_admin",
    "playbook_admin",
    "playbook_member",
    "run_admin",
    "run_member",
    "system_user_manager",
    "system_read_only_admin",
    "system_manager",
    "shared_channel_manager",
];

/// Permissions no patch may add or remove.
const NOT_ALLOWED: [&str; 4] = [
    "sysconsole_write_user_management_system_roles",
    "sysconsole_read_user_management_system_roles",
    "manage_roles",
    "manage_system",
];

/// `NewRoleFromModel`'s permissions column: each permission once, each after a space.
pub(crate) fn go_permissions(perms: &[String]) -> String {
    let mut seen = std::collections::HashSet::new();
    perms.iter().filter(|p| seen.insert(p.as_str())).map(|p| format!(" {p}")).collect()
}

/// `IsValidRoleName`.
pub(crate) fn valid_role_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// `Role.IsValidWithoutId`.
pub(crate) fn role_is_valid(r: &Role) -> Result<(), String> {
    if !valid_role_name(&r.name) {
        return Err(format!("invalid role name {:?}", r.name));
    }
    if r.display_name.is_empty() {
        return Err("role display name must not be empty".into());
    }
    if r.display_name.len() > 128 {
        return Err(format!("role display name {:?} exceeds maximum length of 128", r.display_name));
    }
    if r.description.len() > 1024 {
        return Err("role description exceeds maximum length of 1024".into());
    }
    let unknown: Vec<&str> =
        r.permissions.0.iter().map(String::as_str).filter(|p| !ALL_PERMISSIONS.lines().any(|l| l == *p)).collect();
    if !unknown.is_empty() {
        return Err(format!("unknown permissions: {}", unknown.join(", ")));
    }
    Ok(())
}

/// The router's 404 for a path Go's route patterns (`[A-Za-z0-9]+`, `[a-z0-9_]+`) do not match.
fn no_route(path: &str) -> AppError {
    AppError::not_found("api.context.404.app_error", "Sorry, we could not find the page.").detail(format!(
        "There doesn't appear to be an api call for the url='{path}'.  Typo? are you missing a team_id or user_id as part of the url?"
    ))
}

fn role_not_found() -> AppError {
    AppError::not_found("app.role.get.app_error", "Unable to get role.")
}

impl App {
    /// `GetRole`: by id (not cached), with the higher scope's channel permissions merged.
    async fn role_by_id(&self, id: &str) -> ApiResult<Role> {
        let r: Role = db::get(&self.db, id).await?.ok_or_else(role_not_found)?;
        let merged = self.merge_channel_higher_scoped(vec![Arc::new(r)]).await?;
        Ok((*merged[0]).clone())
    }

    /// `SqlRoleStore.Save` for an existing role, then the role cache entry and the role permissions cache dropped
    /// (`LocalCacheRoleStore.Save`).
    async fn save_role(&self, r: &mut Role) -> ApiResult<()> {
        role_is_valid(r).map_err(|e| AppError::bad_request("app.role.save.invalid_role.app_error", "").detail(e))?;
        r.update_at = now_ms();
        let res = sqlx::query(
            "UPDATE Roles SET UpdateAt = $2, DeleteAt = $3, CreateAt = $4, Name = $5, DisplayName = $6, Description = $7,
                 Permissions = $8, SchemeManaged = $9, BuiltIn = $10, SchemeId = $11
             WHERE Id = $1",
        )
        .bind(&r.id)
        .bind(r.update_at)
        .bind(r.delete_at)
        .bind(r.create_at)
        .bind(&r.name)
        .bind(&r.display_name)
        .bind(&r.description)
        .bind(go_permissions(&r.permissions.0))
        .bind(r.scheme_managed)
        .bind(r.built_in)
        .bind(&r.scheme_id)
        .execute(&self.db)
        .await;
        self.caches.role.remove(&r.name);
        self.caches.role_permissions.clear();
        match res {
            Ok(x) if x.rows_affected() == 1 => Ok(()),
            Ok(x) => Err(AppError::internal(
                "app.role.save.insert.app_error",
                format!("invalid number of updated rows, expected 1 but got {}", x.rows_affected()),
            )),
            Err(e) => Err(AppError::internal("app.role.save.insert.app_error", e.to_string())),
        }
    }

    /// `UpdateRole`: saved; a change to a role channel scheme roles are merged with sends their new merged
    /// permissions too.
    pub(crate) async fn update_role(&self, r: &mut Role) -> ApiResult<()> {
        self.save_role(r).await?;
        if NOT_CHANNEL_HIGHER_SCOPE.contains(&r.name.as_str()) {
            return Ok(());
        }
        let sql = if ["channel_guest", "channel_user", "channel_admin"].contains(&r.name.as_str()) {
            format!(
                "SELECT {} FROM Roles JOIN Schemes ON Roles.SchemeId = Schemes.Id
                 WHERE Schemes.Scope = 'channel' AND Roles.DeleteAt = 0 AND Schemes.DeleteAt = 0 AND $1::text IS NOT NULL",
                db::cols::<Role>("Roles")
            )
        } else {
            format!(
                "SELECT {} FROM Roles AS HigherScopedRoles
                 JOIN Schemes AS HigherScopedSchemes ON (HigherScopedRoles.Name = HigherScopedSchemes.DefaultChannelGuestRole
                      OR HigherScopedRoles.Name = HigherScopedSchemes.DefaultChannelUserRole
                      OR HigherScopedRoles.Name = HigherScopedSchemes.DefaultChannelAdminRole)
                 JOIN Teams ON Teams.SchemeId = HigherScopedSchemes.Id
                 JOIN Channels ON Channels.TeamId = Teams.Id
                 JOIN Schemes AS ChannelSchemes ON Channels.SchemeId = ChannelSchemes.Id
                 JOIN Roles AS ChannelSchemeRoles ON ChannelSchemeRoles.SchemeId = ChannelSchemes.Id
                 WHERE HigherScopedSchemes.Scope = 'team' AND HigherScopedRoles.Name = $1
                   AND HigherScopedRoles.DeleteAt = 0 AND HigherScopedSchemes.DeleteAt = 0 AND Teams.DeleteAt = 0
                   AND Channels.DeleteAt = 0 AND ChannelSchemes.DeleteAt = 0 AND ChannelSchemeRoles.DeleteAt = 0",
                db::cols::<Role>("ChannelSchemeRoles")
            )
        };
        let impacted: Vec<Role> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&r.name)
            .fetch_all(&self.db)
            .await
            .map_err(|e| AppError::internal("app.role.get.app_error", e.to_string()))?;
        let mut all: Vec<Arc<Role>> = impacted.into_iter().map(Arc::new).collect();
        all.push(Arc::new(r.clone()));
        for ir in self.merge_channel_higher_scoped(all).await? {
            if ir.name != r.name {
                self.send_role_event(&ir).await?;
            }
        }
        Ok(())
    }

    /// `sendUpdatedRoleEvent`: to everyone for built-in and scheme-less roles, else to the teams or channels of
    /// the role's scheme.
    async fn send_role_event(&self, r: &Role) -> ApiResult<()> {
        let data = json!({"role": serde_json::to_string(r).unwrap()});
        let scheme_id = match (&r.scheme_id, r.built_in) {
            (Some(id), false) => id.clone(),
            _ => {
                self.hub.publish("role_updated", data, Broadcast::default());
                return Ok(());
            }
        };
        let Some(scheme) = db::get::<Scheme>(&self.db, &scheme_id).await? else {
            eprintln!("role event: no scheme {scheme_id} for role {}", r.id);
            return Ok(());
        };
        let err = |e: sqlx::Error| AppError::internal("app.role.send_updated_role_event.app_error", e.to_string());
        match scheme.scope.as_str() {
            "team" => {
                let ids: Vec<(String,)> =
                    sqlx::query_as("SELECT Id FROM Teams WHERE SchemeId = $1 ORDER BY DisplayName LIMIT 100000")
                        .bind(&scheme.id)
                        .fetch_all(&self.db)
                        .await
                        .map_err(err)?;
                for (id,) in ids {
                    self.hub.publish("role_updated", data.clone(), Broadcast { team_id: id, ..Default::default() });
                }
            }
            "channel" => {
                let ids: Vec<(String,)> =
                    sqlx::query_as("SELECT Id FROM Channels WHERE SchemeId = $1 ORDER BY DisplayName LIMIT 100000")
                        .bind(&scheme.id)
                        .fetch_all(&self.db)
                        .await
                        .map_err(err)?;
                for (id,) in ids {
                    self.hub.publish("role_updated", data.clone(), Broadcast { channel_id: id, ..Default::default() });
                }
            }
            _ => self.hub.publish("role_updated", data, Broadcast::default()),
        }
        Ok(())
    }
}

// ---------- roles ----------

/// `getAllRoles`: system admins.
async fn all_roles(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Vec<Role>>> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    let roles: Vec<Role> = sqlx::query_as(Role::SELECT)
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("app.role.get_all.app_error", e.to_string()))?;
    let merged = app.merge_channel_higher_scoped(roles.into_iter().map(Arc::new).collect()).await?;
    Ok(Json(merged.iter().map(|r| (**r).clone()).collect()))
}

async fn get_role(St(app): St<State>, Auth(_): Auth, Path(id): Path<String>) -> ApiResult<Json<Role>> {
    if !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(no_route(&format!("/api/v4/roles/{id}")));
    }
    if !is_valid_id(&id) {
        return Err(AppError::invalid_url_param("role_id"));
    }
    Ok(Json(app.role_by_id(&id).await?))
}

/// `getRoleByName` through the role cache.
async fn role_by_name(St(app): St<State>, Auth(_): Auth, Path(name): Path<String>) -> ApiResult<Json<Role>> {
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return Err(no_route(&format!("/api/v4/roles/name/{name}")));
    }
    if !valid_role_name(&name) {
        return Err(AppError::invalid_url_param("role_name"));
    }
    let r = app.roles(&[name.as_str()]).await?.into_iter().next().ok_or_else(|| {
        AppError::not_found("app.role.get_by_name.app_error", "Unable to get role.").detail(format!("name={name}"))
    })?;
    Ok(Json((*r).clone()))
}

#[derive(Deserialize, Default)]
struct RolePatch {
    permissions: Option<Vec<String>>,
}

/// `patchRole`: protected system roles need `manage_system`; guest roles need a license; a few permissions can
/// never be added or removed.
async fn patch_role(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>, body: Bytes) -> ApiResult<Json<Role>> {
    if !is_valid_id(&id) {
        return Err(AppError::invalid_url_param("role_id"));
    }
    let mut patch: RolePatch = decode(&body, "role")?;
    let mut role = app.role_by_id(&id).await?;
    let protected = NEW_SYSTEM_ROLES.contains(&role.name.as_str())
        || ["system_admin", "system_user", "system_guest"].contains(&role.name.as_str());
    let required = if protected { crate::perm::MANAGE_SYSTEM } else { "sysconsole_write_user_management_permissions" };
    app.require(&s, required).await?;
    let guest = ["system_guest", "team_guest", "channel_guest"].contains(&role.name.as_str());
    if let Some(perms) = &mut patch.permissions {
        if guest {
            return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.roles.patch_roles.license.error", ""));
        }
        // PermissionsChangedByPatch
        let changed = role
            .permissions
            .0
            .iter()
            .filter(|p| !perms.contains(p))
            .chain(perms.iter().filter(|p| !role.permissions.0.contains(p)));
        for p in changed {
            if NOT_ALLOWED.contains(&p.as_str()) {
                return Err(AppError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "api.roles.patch_roles.not_allowed_permission.error",
                    "",
                )
                .detail(format!("Cannot add or remove permission: {p}")));
            }
        }
        // RemoveDuplicateStrings: sorted, unique.
        perms.sort();
        perms.dedup();
    }
    let scheme_role = [
        "team_admin",
        "channel_admin",
        "system_user",
        "team_user",
        "channel_user",
        "system_guest",
        "team_guest",
        "channel_guest",
        "playbook_admin",
        "playbook_member",
        "run_admin",
        "run_member",
    ]
    .contains(&role.name.as_str());
    let p = if scheme_role {
        "sysconsole_write_user_management_permissions"
    } else {
        "sysconsole_write_user_management_system_roles"
    };
    app.require(&s, p).await?;
    // PatchRole
    if let Some(perms) = patch.permissions {
        if perms == role.permissions.0 {
            crate::audit::log(&app, Some(&s), "").await;
            return Ok(Json(role));
        }
        role.permissions = Spaced(perms);
    }
    app.update_role(&mut role).await?;
    app.send_role_event(&role).await?;
    crate::audit::log(&app, Some(&s), "").await;
    Ok(Json(role))
}

// ---------- schemes ----------

fn require_scheme_id(id: &str) -> ApiResult<()> {
    if is_valid_id(id) { Ok(()) } else { Err(AppError::invalid_url_param("scheme_id")) }
}

/// `getSchemes`: newest first.
async fn get_schemes(
    St(app): St<State>,
    Auth(s): Auth,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<Scheme>>> {
    app.require(&s, "sysconsole_read_user_management_permissions").await?;
    let scope = q.get("scope").map(String::as_str).unwrap_or("");
    if !scope.is_empty() && scope != "team" && scope != "channel" {
        return Err(AppError::invalid_param("scope"));
    }
    let (page, per_page) = paging(&q);
    let mut sql = format!("{} WHERE DeleteAt = 0", Scheme::SELECT);
    if !scope.is_empty() {
        sql += " AND Scope = $3";
    } else {
        sql += " AND $3::text IS NOT NULL";
    }
    sql += " ORDER BY CreateAt DESC LIMIT $1 OFFSET $2";
    let list: Vec<Scheme> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(per_page)
        .bind(page * per_page)
        .bind(scope)
        .fetch_all(&app.db)
        .await?;
    Ok(Json(list))
}

fn scheme_license(what: &str) -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, &format!("api.scheme.{what}_scheme.license.error"), "")
}

async fn create_scheme(Auth(_): Auth, body: Bytes) -> ApiResult<Response> {
    let _: Value = decode(&body, "scheme")?;
    Err(scheme_license("create"))
}

async fn delete_scheme(Auth(_): Auth, Path(id): Path<String>) -> ApiResult<Response> {
    require_scheme_id(&id)?;
    Err(scheme_license("delete"))
}

async fn patch_scheme(Auth(_): Auth, Path(id): Path<String>, body: Bytes) -> ApiResult<Response> {
    require_scheme_id(&id)?;
    let _: Value = decode(&body, "scheme")?;
    Err(scheme_license("patch"))
}

async fn teams_for_scheme(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    require_scheme_id(&id)?;
    app.require(&s, "sysconsole_read_user_management_teams").await?;
    let scheme = app.scheme(&id).await?;
    if scheme.scope != "team" {
        return Err(AppError::bad_request("api.scheme.get_teams_for_scheme.scope.error", ""));
    }
    let (page, per_page) = paging(&q);
    let sql = format!("{} WHERE SchemeId = $1 ORDER BY DisplayName LIMIT $2 OFFSET $3", Team::SELECT);
    let mut teams: Vec<Team> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&id)
        .bind(per_page)
        .bind(page * per_page)
        .fetch_all(&app.db)
        .await?;
    for t in &mut teams {
        app.sanitize_team(&s, t).await;
    }
    Ok(Json(teams).into_response())
}

async fn channels_for_scheme(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    require_scheme_id(&id)?;
    app.require(&s, "sysconsole_read_user_management_channels").await?;
    let scheme = app.scheme(&id).await?;
    if scheme.scope != "channel" {
        return Err(AppError::bad_request("api.scheme.get_channels_for_scheme.scope.error", ""));
    }
    let (page, per_page) = paging(&q);
    let sql =
        format!("{} WHERE SchemeId = $1 AND Type <> 'S' ORDER BY DisplayName LIMIT $2 OFFSET $3", Channel::SELECT);
    let list: Vec<Channel> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&id)
        .bind(per_page)
        .bind(page * per_page)
        .fetch_all(&app.db)
        .await?;
    Ok(Json(list).into_response())
}
