//! Schemes through Go's scheme cache, and the member role updates that read them.

// Based on Mattermost server/channels/app/scheme.go, app/channel.go (GetSchemeRolesForChannel,
// GetTeamSchemeChannelRoles, updateChannelMemberRolesInternal), app/team.go (GetSchemeRolesForTeam,
// updateTeamMemberRolesInternal), api4/channel.go and api4/team.go, Copyright (c) 2015-present Mattermost, Inc.
// Licensed under AGPL-3.0.

use std::sync::Arc;

use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, Body, State, is_valid_id};
use crate::db;
use crate::hub::Broadcast;
use crate::model::{Scheme, TeamMember};
use crate::perm;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/schemes/{scheme_id}", get(get_scheme))
        .route("/api/v4/channels/{channel_id}/members/{user_id}/roles", put(update_channel_member_roles))
        .route("/api/v4/teams/{team_id}/members/{user_id}/roles", put(update_team_member_roles))
}

fn bad(id: &str, m: &str) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, id, m)
}

/// `IsBuiltInRole`: the roles the server defines itself.
pub(crate) fn is_builtin(r: &str) -> bool {
    ["system_", "team_", "channel_", "playbook_", "run_", "custom_group_"].iter().any(|p| r.starts_with(p))
}

/// `IsValidMemberRoles`: role names, and of the built-in ones only those of the scope.
fn valid_member_roles(roles: &str, scope: [&str; 3]) -> bool {
    roles.split_whitespace().all(|r| {
        !r.is_empty()
            && r.len() <= 64
            && r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            && (!is_builtin(r) || scope.contains(&r))
    })
}

impl App {
    /// `GetScheme` through the scheme cache (`SchemeStore.Get`).
    pub async fn scheme(&self, id: &str) -> ApiResult<Arc<Scheme>> {
        if let Some(s) = self.caches.scheme.get(id) {
            return Ok(s);
        }
        let s: Scheme = db::get(&self.db, id)
            .await?
            .ok_or_else(|| AppError::not_found("app.scheme.get.app_error", "Unable to get the scheme."))?;
        let s = Arc::new(s);
        self.caches.scheme.put(id.to_string(), s.clone());
        Ok(s)
    }

    /// `GetTeamSchemeChannelRoles`: (guest, user, admin) channel roles of the team's scheme, or the built-in ones.
    pub async fn team_scheme_channel_roles(&self, team_id: &str) -> ApiResult<[String; 3]> {
        let team = self.team(team_id).await?;
        if let Some(id) = team.scheme_id.as_deref().filter(|s| !s.is_empty()) {
            let s = self.scheme(id).await?;
            return Ok([
                s.default_channel_guest_role.clone(),
                s.default_channel_user_role.clone(),
                s.default_channel_admin_role.clone(),
            ]);
        }
        Ok([perm::CHANNEL_GUEST.into(), perm::CHANNEL_USER.into(), perm::CHANNEL_ADMIN.into()])
    }

    /// `GetSchemeRolesForChannel`.
    pub async fn scheme_roles_for_channel(&self, channel_id: &str) -> ApiResult<[String; 3]> {
        let c = self.channel_or_404(channel_id).await?;
        if let Some(id) = c.scheme_id.as_deref().filter(|s| !s.is_empty()) {
            let s = self.scheme(id).await?;
            return Ok([
                s.default_channel_guest_role.clone(),
                s.default_channel_user_role.clone(),
                s.default_channel_admin_role.clone(),
            ]);
        }
        self.team_scheme_channel_roles(&c.team_id).await
    }

    /// `GetSchemeRolesForTeam`.
    pub async fn scheme_roles_for_team(&self, team_id: &str) -> ApiResult<[String; 3]> {
        let team = self.team(team_id).await?;
        if let Some(id) = team.scheme_id.as_deref().filter(|s| !s.is_empty()) {
            let s = self.scheme(id).await?;
            return Ok([
                s.default_team_guest_role.clone(),
                s.default_team_user_role.clone(),
                s.default_team_admin_role.clone(),
            ]);
        }
        Ok([perm::TEAM_GUEST.into(), perm::TEAM_USER.into(), perm::TEAM_ADMIN.into()])
    }

    /// The new roles split as `update*MemberRolesInternal` does: scheme flags (guest, user, admin) and explicit
    /// roles. `builtin_scope`: for channels, built-in roles outside the channel scope are refused.
    async fn split_roles(
        &self,
        new_roles: &str,
        scheme: &[String; 3],
        channel: bool,
        prev_guest: bool,
    ) -> ApiResult<([bool; 3], String)> {
        let (scope_err, guest_user, guest_admin, guest_change, unset) = if channel {
            (
                "api.channel.update_channel_member_roles.scheme_role.app_error",
                "api.channel.update_channel_member_roles.guest_and_user.app_error",
                "api.channel.update_channel_member_roles.guest_and_admin.app_error",
                "api.channel.update_channel_member_roles.changing_guest_role.app_error",
                "api.channel.update_channel_member_roles.unset_user_scheme.app_error",
            )
        } else {
            (
                "api.channel.update_team_member_roles.scheme_role.app_error",
                "api.team.update_team_member_roles.guest_and_user.app_error",
                "api.team.update_team_member_roles.guest_and_admin.app_error",
                "api.channel.update_team_member_roles.changing_guest_role.app_error",
                "api.team.update_team_member_roles.unset_user_scheme.app_error",
            )
        };
        let mut flags = [false; 3];
        let mut explicit: Vec<&str> = vec![];
        for name in new_roles.split_whitespace() {
            let role = self.roles(&[name]).await?.into_iter().next().ok_or_else(|| {
                bad("app.role.get_by_name.app_error", "Unable to get role.").detail(format!("name={name}"))
            })?;
            if !role.scheme_managed {
                if channel
                    && is_builtin(name)
                    && ![perm::CHANNEL_GUEST, perm::CHANNEL_USER, perm::CHANNEL_ADMIN].contains(&name)
                {
                    return Err(bad(
                        scope_err,
                        "The provided role is managed by a Scheme and therefore cannot be applied directly to a Channel Member.",
                    ));
                }
                explicit.push(name);
            } else if name == scheme[2] {
                flags[2] = true;
            } else if name == scheme[1] {
                flags[1] = true;
            } else if name == scheme[0] {
                flags[0] = true;
            } else {
                return Err(bad(
                    scope_err,
                    "The provided role is managed by a Scheme and therefore cannot be applied directly to a Channel Member.",
                ));
            }
        }
        if flags[1] && flags[0] {
            return Err(bad(guest_user, "Invalid member update: A user must be a guest or a user but not both."));
        }
        if flags[0] && flags[2] {
            return Err(bad(guest_admin, "Invalid member update: A guest cannot be made team admin."));
        }
        if prev_guest != flags[0] {
            return Err(bad(guest_change, "Invalid member update: You can't change a guest to a user or vice versa."));
        }
        if !flags[0] && !flags[1] {
            return Err(bad(unset, "Invalid member update: A member always needs a user or guest scheme role."));
        }
        Ok((flags, explicit.join(" ")))
    }
}

/// `getScheme`: system console readers only.
async fn get_scheme(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Scheme>> {
    if !is_valid_id(&id) {
        return Err(AppError::invalid_url_param("scheme_id"));
    }
    if !app.has_perm(&s, "sysconsole_read_user_management_permissions").await {
        return Err(AppError::forbidden_perm("sysconsole_read_user_management_permissions"));
    }
    Ok(Json((*app.scheme(&id).await?).clone()))
}

fn roles_param(b: &Value) -> String {
    b.get("roles").and_then(Value::as_str).unwrap_or("").to_string()
}

/// `updateChannelMemberRoles`.
async fn update_channel_member_roles(
    St(app): St<State>,
    Auth(s): Auth,
    Path((channel_id, user_id)): Path<(String, String)>,
    Body(b): Body<Value>,
) -> ApiResult<Json<Value>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&channel_id) {
        return Err(AppError::invalid_url_param("channel_id"));
    }
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    app.reject_board_or_space_channel(&channel_id).await?;
    let roles = roles_param(&b);
    if !valid_member_roles(&roles, [perm::CHANNEL_GUEST, perm::CHANNEL_USER, perm::CHANNEL_ADMIN]) {
        return Err(AppError::invalid_param("roles"));
    }
    app.require_channel(&s, &channel_id, perm::MANAGE_CHANNEL_ROLES).await?;
    let mut m = app.channel_member(&channel_id, &user_id).await?.ok_or_else(|| {
        AppError::not_found(
            "app.channel.get_member.missing.app_error",
            "No channel member found for that user ID and channel ID.",
        )
    })?;
    let scheme = app.scheme_roles_for_channel(&channel_id).await?;
    let (f, explicit) = app.split_roles(&roles, &scheme, true, m.scheme_guest).await?;
    [m.scheme_guest, m.scheme_user, m.scheme_admin] = f;
    m.explicit_roles = explicit;
    app.update_channel_member(&m).await?;
    Ok(Json(json!({"status": "OK"})))
}

/// `updateTeamMemberRoles`.
async fn update_team_member_roles(
    St(app): St<State>,
    Auth(s): Auth,
    Path((team_id, user_id)): Path<(String, String)>,
    Body(b): Body<Value>,
) -> ApiResult<Json<Value>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    let roles = roles_param(&b);
    if !valid_member_roles(&roles, [perm::TEAM_GUEST, perm::TEAM_USER, perm::TEAM_ADMIN]) {
        return Err(AppError::invalid_param("roles"));
    }
    app.require_team(&s, &team_id, "manage_team_roles").await?;
    let m: TeamMember = app.team_member(&team_id, &user_id).await?.ok_or_else(|| {
        AppError::not_found(
            "app.team.get_member.missing.app_error",
            "No team member found for that user ID and team ID.",
        )
    })?;
    let scheme = app.scheme_roles_for_team(&team_id).await?;
    let (f, explicit) = app.split_roles(&roles, &scheme, false, m.scheme_guest).await?;
    sqlx::query(
        "UPDATE TeamMembers SET Roles = $3, SchemeGuest = $4, SchemeUser = $5, SchemeAdmin = $6 WHERE TeamId = $1 AND UserId = $2",
    )
    .bind(&team_id)
    .bind(&user_id)
    .bind(&explicit)
    .bind(f[0])
    .bind(f[1])
    .bind(f[2])
    .execute(&app.db)
    .await?;
    app.clear_sessions_for_user(&user_id);
    // sendUpdatedTeamMemberEvent: the member as the store now reads it.
    if let Some(m) = app.team_member(&team_id, &user_id).await? {
        app.hub.publish(
            "memberrole_updated",
            json!({ "member": serde_json::to_string(&m).unwrap() }),
            Broadcast { user_id: user_id.clone(), ..Default::default() },
        );
    }
    Ok(Json(json!({"status": "OK"})))
}
