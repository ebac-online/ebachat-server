//! Managing teams: creating, updating, patching, privacy, archiving and restoring them, their icons and invite
//! ids, the member list, removing members (with what leaving a team undoes), searches, and the channels of a
//! team that the browse and search dialogs read.

// Based on Mattermost server/channels/api4/team.go, api4/channel.go (the team channel lists), app/team.go,
// app/teams/teams.go, app/teams/utils.go, public/model/team.go and store/sqlstore/team_store.go,
// channel_store.go, channel_store_categories.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under
// AGPL-3.0.

use std::collections::HashMap;

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::multipart::MultipartRejection;
use axum::extract::{Multipart, Path, State as St};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, BusyAuth, State, is_valid_id, new_id, now_ms};
use crate::channels_api::{paging, parse_bool};
use crate::db::{self, Table};
use crate::hub::Broadcast;
use crate::model::{Channel, Post, Session, Team, TeamMember, User};
use crate::perm;
use crate::preferences::user_param;
use crate::teams::{email_allowed, sanitize_member};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/teams", post(create_team))
        .route("/api/v4/teams/search", post(search_teams))
        .route("/api/v4/teams/invites/email", delete(invalidate_email_invites))
        .route("/api/v4/teams/name/{team_name}/exists", get(team_exists))
        .route("/api/v4/teams/{team_id}", put(update_team).delete(delete_team))
        .route("/api/v4/teams/{team_id}/patch", put(patch_team))
        .route("/api/v4/teams/{team_id}/privacy", put(update_privacy))
        .route("/api/v4/teams/{team_id}/restore", post(restore_team))
        .route("/api/v4/teams/{team_id}/regenerate_invite_id", post(regenerate_invite_id))
        .route("/api/v4/teams/{team_id}/scheme", put(update_team_scheme))
        .route("/api/v4/teams/{team_id}/image", post(set_icon).delete(remove_icon))
        .route("/api/v4/teams/{team_id}/members", get(get_members))
        .route("/api/v4/teams/{team_id}/members/{user_id}", delete(remove_member))
        .route("/api/v4/teams/{team_id}/members/{user_id}/schemeRoles", put(update_member_scheme_roles))
        .route("/api/v4/teams/{team_id}/members_minus_group_members", get(members_minus_group_members))
        .route("/api/v4/teams/{team_id}/groups", get(license_error))
        .route("/api/v4/teams/{team_id}/groups_by_channels", get(license_error))
        .route("/api/v4/teams/{team_id}/invite-guests/email", post(invite_guests))
        .route("/api/v4/teams/{team_id}/access_control/policy", get(access_control_policy))
        .route("/api/v4/teams/{team_id}/access_control/attributes", get(access_control_attributes))
        .route("/api/v4/teams/{team_id}/channels/ids", post(public_channels_by_ids))
        .route("/api/v4/teams/{team_id}/channels/private", get(private_channels))
        .route("/api/v4/teams/{team_id}/channels/recommended", get(recommended_channels))
        .route("/api/v4/teams/{team_id}/channels/search", post(search_channels))
        .route("/api/v4/teams/{team_id}/channels/search_autocomplete", get(autocomplete_for_search))
        .route("/api/v4/users/{user_id}/teams/{team_id}/unread", get(team_unread))
        .route("/api/v4/users/{user_id}/teams/{team_id}/read", put(read_all_in_team))
}

// ---------- the model (public/model/team.go) ----------

const RESERVED_NAMES: [&str; 17] = [
    "admin",
    "api",
    "channel",
    "claim",
    "error",
    "files",
    "help",
    "landing",
    "login",
    "mfa",
    "oauth",
    "plug",
    "plugins",
    "post",
    "signup",
    "boards",
    "playbooks",
];

fn bad(id: &str) -> AppError {
    AppError::bad_request(id, "")
}

fn status_ok() -> Json<Value> {
    Json(json!({"status": "OK"}))
}

/// One JSON value read as Go's `json.NewDecoder(r.Body).Decode` reads it: an empty body or a bad value is a 400
/// for `name`, `null` the zero value, anything after the value ignored.
pub fn decode<T: DeserializeOwned + Default>(body: &[u8], name: &str) -> ApiResult<T> {
    let mut it = serde_json::Deserializer::from_slice(body).into_iter::<Option<T>>();
    match it.next() {
        Some(Ok(v)) => Ok(v.unwrap_or_default()),
        Some(Err(e)) => Err(AppError::invalid_param(name).detail(e.to_string())),
        None => Err(AppError::invalid_param(name).detail("EOF")),
    }
}

/// `SanitizeUnicode`: Go's blocklist of invisible and deprecated code points.
pub fn sanitize_unicode(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(*c as u32,
                0x0340 | 0x0341 | 0x17A3 | 0x17D3 | 0x2028 | 0x2029 | 0x202A..=0x202E | 0x206A..=0x206F
                | 0xFFF9..=0xFFFB | 0xFEFF | 0xFFFC | 0x1D173..=0x1D17A | 0xE0000..=0xE007F)
        })
        .collect()
}

/// `isValidAlphaNum`: `^[a-z0-9]+([a-z\-0-9]+|(__)?)[a-z0-9]+$`.
fn valid_alpha_num(s: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[a-z0-9]+([a-z\-0-9]+|(__)?)[a-z0-9]+$").unwrap()).is_match(s)
}

/// `IsValidTeamName`.
pub fn valid_team_name(s: &str) -> bool {
    valid_alpha_num(s) && s.len() >= 2
}

/// `IsValidEmail`: lower case, one plain address.
fn valid_email(s: &str) -> bool {
    if s.chars().any(|c| c.is_uppercase() || c.is_whitespace() || c == '<' || c == '>' || c == ',') {
        return false;
    }
    let Some((local, domain)) = s.rsplit_once('@') else { return false };
    !local.is_empty() && !domain.is_empty() && !domain.starts_with('.') && !domain.ends_with('.')
}

/// `Team.IsValid`.
fn team_is_valid(t: &Team) -> ApiResult<()> {
    let e = |f: &str| bad(&format!("model.team.is_valid.{f}.app_error")).detail(format!("id={}", t.id));
    if !is_valid_id(&t.id) {
        return Err(bad("model.team.is_valid.id.app_error"));
    }
    if t.create_at == 0 {
        return Err(e("create_at"));
    }
    if t.update_at == 0 {
        return Err(e("update_at"));
    }
    if t.email.len() > 128 || !t.email.is_empty() && !valid_email(&t.email) {
        return Err(e("email"));
    }
    let n = t.display_name.chars().count();
    if n == 0 || n > 64 {
        return Err(e("name"));
    }
    if t.name.len() > 64 {
        return Err(e("url"));
    }
    if t.description.len() > 255 {
        return Err(e("description"));
    }
    if t.invite_id.is_empty() {
        return Err(e("invite_id"));
    }
    let lower = t.name.to_lowercase();
    if RESERVED_NAMES.iter().any(|r| lower.starts_with(r)) {
        return Err(e("reserved"));
    }
    if !valid_team_name(&t.name) {
        return Err(e("characters"));
    }
    if t.r#type != "O" && t.r#type != "I" {
        return Err(e("type"));
    }
    if t.company_name.len() > 64 {
        return Err(e("company"));
    }
    if t.allowed_domains.len() > 500 {
        return Err(e("domains"));
    }
    Ok(())
}

fn sanitize_fields(t: &mut Team) {
    t.name = sanitize_unicode(&t.name);
    t.display_name = sanitize_unicode(&t.display_name);
    t.description = sanitize_unicode(&t.description);
    t.company_name = sanitize_unicode(&t.company_name);
}

/// `normalizeDomains`.
fn domains(s: &str) -> Vec<String> {
    s.trim().to_lowercase().replace(['@', ','], " ").split_whitespace().map(str::to_string).collect()
}

/// `model.TeamPatch`.
#[derive(Deserialize, Default)]
struct TeamPatch {
    display_name: Option<String>,
    description: Option<String>,
    company_name: Option<String>,
    allowed_domains: Option<String>,
    allow_open_invite: Option<bool>,
    group_constrained: Option<bool>,
    cloud_limits_archived: Option<bool>,
}

impl App {
    /// `SqlTeamStore.Update`: `PreUpdate`, `IsValid`, the creation time kept.
    pub async fn update_team_row(&self, t: &mut Team) -> ApiResult<()> {
        sanitize_fields(t);
        team_is_valid(t)?;
        let old: Option<Team> = db::get(&self.db, &t.id).await?;
        let Some(old) = old else { return Err(bad("app.team.update.find.app_error")) };
        t.create_at = old.create_at;
        t.update_at = now_ms();
        db::update(&self.db, &*t)
            .await
            .map_err(|e| AppError::internal("app.team.update.updating.app_error", e.to_string()))?;
        Ok(())
    }

    /// `sendTeamEvent`: the team without its email and invite id, as a JSON string, to the team.
    pub fn send_team_event(&self, t: &Team, event: &str) {
        let mut t = t.clone();
        t.sanitize();
        self.hub.publish(
            event,
            json!({"team": serde_json::to_string(&t).unwrap()}),
            Broadcast { team_id: t.id.clone(), ..Default::default() },
        );
    }

    /// `checkValidDomains`: the team's allowed domains within `RestrictCreationToDomains`.
    fn check_valid_domains(&self, t: &Team) -> ApiResult<()> {
        let valid = domains(self.cfg.str("TeamSettings.RestrictCreationToDomains"));
        if valid.is_empty() {
            return Ok(());
        }
        for d in domains(&t.allowed_domains) {
            if !valid.contains(&d) {
                return Err(bad("api.team.update_restricted_domains.mismatch.app_error").with_param("Domain", &d));
            }
        }
        Ok(())
    }

    /// `IsTeamEmailAllowed`.
    pub(crate) fn team_email_allowed(&self, user: &User, t: &Team) -> bool {
        if user.is_bot {
            return true;
        }
        let email = user.email.to_lowercase();
        if user.is_guest() {
            return email_allowed(&email, &[self.cfg.str("GuestAccountsSettings.RestrictCreationToDomains")]);
        }
        email_allowed(&email, &[&t.allowed_domains, self.cfg.str("TeamSettings.RestrictCreationToDomains")])
    }

    /// `creatorCanInviteUsersOnTeam`: the session, or the roles the creator gets in the new team.
    async fn creator_can_invite(&self, s: &Session, t: &Team) -> bool {
        if self.has_perm(s, perm::INVITE_USER).await {
            return true;
        }
        let roles = match t.scheme_id.as_deref().filter(|id| !id.is_empty()) {
            Some(id) => match self.scheme(id).await {
                Ok(sc) => format!("{} {}", sc.default_team_user_role, sc.default_team_admin_role),
                Err(_) => return false,
            },
            None => format!("{} {}", perm::TEAM_USER, perm::TEAM_ADMIN),
        };
        self.roles_grant(&roles, perm::INVITE_USER).await
    }

    /// `CreateTeam`: the team saved (a new invite id), its default channels.
    pub(crate) async fn create_team_row(&self, t: &mut Team) -> ApiResult<()> {
        t.invite_id.clear();
        if !t.id.is_empty() {
            return Err(bad("store.sql_team.save_team.existing.app_error").detail(format!("id={}", t.id)));
        }
        t.id = new_id();
        t.create_at = now_ms();
        t.update_at = t.create_at;
        sanitize_fields(t);
        t.invite_id = new_id();
        team_is_valid(t)?;
        if let Err(e) = db::insert(&self.db, &*t).await {
            if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
                return Err(bad("store.sql_team.save_team.existing.app_error").detail(format!("id={}", t.id)));
            }
            return Err(AppError::internal("app.team.save.app_error", e.to_string()));
        }
        for name in self.default_channel_names() {
            let display = match name.as_str() {
                "town-square" => {
                    crate::i18n::t("api.channel.create_default_channels.town_square").unwrap_or("Town Square")
                }
                "off-topic" => crate::i18n::t("api.channel.create_default_channels.off_topic").unwrap_or("Off-Topic"),
                n => n,
            }
            .to_string();
            let mut c = Channel {
                display_name: display,
                name,
                r#type: "O".into(),
                team_id: t.id.clone(),
                ..Default::default()
            };
            if let Err(existing) = self.save_channel(&mut c, true).await? {
                return Err(
                    bad("store.sql_channel.save_channel.exists.app_error").detail(format!("id={}", existing.id))
                );
            }
        }
        Ok(())
    }

    /// `leaveTeam`: out of the team's channels, the leave message, the membership marked deleted, then
    /// `postProcessTeamMemberLeave` (sidebar, team preferences, caches).
    pub async fn leave_team(&self, team: &Team, user: &User, requestor: &str) -> ApiResult<()> {
        let m = self
            .team_member(&team.id, &user.id)
            .await?
            .ok_or_else(|| bad("api.team.remove_user_from_team.missing.app_error"))?;
        let channels: Vec<(String,)> = sqlx::query_as(
            "SELECT c.Id FROM Channels c JOIN ChannelMembers cm ON cm.ChannelId = c.Id
             WHERE cm.UserId = $1 AND c.TeamId = $2 AND c.Type NOT IN ('D', 'G')",
        )
        .bind(&user.id)
        .bind(&team.id)
        .fetch_all(&self.db)
        .await?;
        for (cid,) in channels {
            self.invalidate_channel_members_cache(&cid);
            let r: Result<(), sqlx::Error> = async {
                sqlx::query("DELETE FROM ChannelMembers WHERE ChannelId = $1 AND UserId = $2")
                    .bind(&cid)
                    .bind(&user.id)
                    .execute(&self.db)
                    .await?;
                sqlx::query("DELETE FROM SidebarChannels WHERE ChannelId = $1 AND UserId = $2")
                    .bind(&cid)
                    .bind(&user.id)
                    .execute(&self.db)
                    .await?;
                Ok(())
            }
            .await;
            r.map_err(|e| AppError::internal("app.channel.remove_member.app_error", e.to_string()))?;
            sqlx::query(
                "DELETE FROM ThreadMemberships WHERE UserId = $1
                 AND EXISTS (SELECT 1 FROM Threads WHERE Threads.PostId = ThreadMemberships.PostId AND Threads.ChannelId = $2)",
            )
            .bind(&user.id)
            .bind(&cid)
            .execute(&self.db)
            .await?;
        }
        if self.cfg.bool("ServiceSettings.ExperimentalEnableDefaultChannelLeaveJoinMessages") {
            let c = self
                .channel_by_name(&team.id, "town-square", false, false)
                .await?
                .ok_or_else(|| AppError::not_found("app.channel.get_by_name.missing.app_error", ""))?;
            let (kind, key) = if requestor == user.id {
                (crate::model::post::kind::LEAVE_TEAM, "api.team.leave.left")
            } else {
                (crate::model::post::kind::REMOVE_FROM_TEAM, "api.team.remove_user_from_team.removed")
            };
            let mut props = serde_json::Map::new();
            props.insert("username".into(), json!(user.username));
            let post = Post {
                channel_id: c.id.clone(),
                message: crate::i18n::t(key).unwrap_or("%v").replace("%v", &user.username),
                r#type: kind.into(),
                user_id: user.id.clone(),
                props: sqlx::types::Json(props),
                ..Default::default()
            };
            if let Err(e) = self.create_post(post, true).await {
                eprintln!("leave team message in {}: {}", c.id, e.detailed_error);
            }
        }
        // RemoveTeamMember: leave_team to the rest of the team and to the user, then the membership deleted.
        let mut omit = std::collections::BTreeMap::new();
        omit.insert(user.id.clone(), true);
        let data = json!({"user_id": user.id, "team_id": team.id});
        self.hub.publish(
            "leave_team",
            data.clone(),
            Broadcast { team_id: team.id.clone(), omit_users: Some(omit), ..Default::default() },
        );
        self.hub.publish("leave_team", data, Broadcast { user_id: user.id.clone(), ..Default::default() });
        sqlx::query(
            "UPDATE TeamMembers SET Roles = '', DeleteAt = $3, SchemeGuest = $4, SchemeUser = $5, SchemeAdmin = $6
             WHERE TeamId = $1 AND UserId = $2",
        )
        .bind(&team.id)
        .bind(&user.id)
        .bind(now_ms())
        .bind(m.scheme_guest)
        .bind(m.scheme_user)
        .bind(m.scheme_admin)
        .execute(&self.db)
        .await
        .map_err(|e| AppError::internal("app.team.save_member.save.app_error", e.to_string()))?;
        // postProcessTeamMemberLeave
        sqlx::query("UPDATE Users SET UpdateAt = $1 WHERE Id = $2")
            .bind(now_ms())
            .bind(&user.id)
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("app.user.update_update.app_error", e.to_string()))?;
        sqlx::query(
            "DELETE FROM SidebarChannels WHERE CategoryId IN (SELECT CategoryId FROM SidebarChannels, SidebarCategories
             WHERE SidebarChannels.CategoryId = SidebarCategories.Id AND SidebarCategories.TeamId = $1
               AND SidebarChannels.UserId = $2)",
        )
        .bind(&team.id)
        .bind(&user.id)
        .execute(&self.db)
        .await
        .map_err(|e| AppError::internal("app.channel.sidebar_categories.app_error", e.to_string()))?;
        sqlx::query("DELETE FROM SidebarCategories WHERE TeamId = $1 AND UserId = $2")
            .bind(&team.id)
            .bind(&user.id)
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("app.channel.sidebar_categories.app_error", e.to_string()))?;
        sqlx::query("DELETE FROM Preferences WHERE UserId = $1 AND Category = $2")
            .bind(&user.id)
            .bind(&team.id)
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("app.preference.delete.app_error", e.to_string()))?;
        self.invalidate_user_teams(&user.id).await;
        Ok(())
    }
}

// ---------- team CRUD ----------

fn require_team_id(id: &str) -> ApiResult<()> {
    if is_valid_id(id) { Ok(()) } else { Err(AppError::invalid_url_param("team_id")) }
}

/// `createTeam`: the creator becomes its admin (the team's email is theirs).
async fn create_team(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Response> {
    let mut team: Team = decode(&body, "team")?;
    team.email = team.email.to_lowercase();
    if !app.has_perm(&s, "create_team").await {
        return Err(AppError::new(StatusCode::FORBIDDEN, "api.team.is_team_creation_allowed.disabled.app_error", ""));
    }
    if team.scheme_id.is_some() && !app.has_perm(&s, "sysconsole_write_user_management_permissions").await {
        return Err(AppError::forbidden_perm("sysconsole_write_user_management_permissions"));
    }
    if (team.allow_open_invite || !team.allowed_domains.is_empty()) && !app.creator_can_invite(&s, &team).await {
        return Err(AppError::forbidden_perm(perm::INVITE_USER));
    }
    // CreateTeamWithUser
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    team.email = user.email.clone();
    if !app.team_email_allowed(&user, &team) {
        return Err(bad("api.team.is_team_creation_allowed.domain.app_error"));
    }
    app.create_team_row(&mut team).await?;
    app.join_user_to_team(&team, &user, "").await?;
    if !app.creator_can_invite(&s, &team).await {
        team.invite_id.clear();
    }
    Ok((StatusCode::CREATED, Json(team)).into_response())
}

/// `updateTeam`: the editable fields of the body over the stored team (`UpdateTeam` with `Sanitized`).
async fn update_team(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Team>> {
    require_team_id(&team_id)?;
    let mut team: Team = decode(&body, "team")?;
    team.email = team.email.to_lowercase();
    if team.id != team_id {
        return Err(AppError::invalid_param("id"));
    }
    app.require_team(&s, &team_id, perm::MANAGE_TEAM).await?;
    let old = app.team(&team_id).await?;
    if (team.allow_open_invite != old.allow_open_invite || team.allowed_domains != old.allowed_domains)
        && !app.has_team_perm(&s, &team_id, perm::INVITE_USER).await
    {
        return Err(AppError::forbidden_perm(perm::INVITE_USER));
    }
    let mut stored: Team = (*app.team(&team_id).await?).clone();
    app.check_valid_domains(&team)?;
    if !team.name.is_empty() && team.name != stored.name && team.name != "-" {
        if db::one::<Team>(&app.db, "name = $1", &team.name).await?.is_some() {
            return Err(bad("app.team.rename_team.name_occupied")
                .detail(format!("team with name {} already exists", team.name)));
        }
        stored.name = team.name;
    }
    stored.display_name = team.display_name;
    stored.description = team.description;
    stored.allow_open_invite = team.allow_open_invite;
    stored.company_name = team.company_name;
    stored.allowed_domains = team.allowed_domains;
    stored.last_team_icon_update = team.last_team_icon_update;
    stored.group_constrained = team.group_constrained;
    app.update_team_row(&mut stored).await?;
    app.send_team_event(&stored, "update_team");
    app.sanitize_team(&s, &mut stored).await;
    Ok(Json(stored))
}

/// `patchTeam`. Making a team group-constrained does not drop the members outside its groups here: groups need
/// a license this server does not have.
async fn patch_team(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Team>> {
    require_team_id(&team_id)?;
    let patch: TeamPatch = decode(&body, "team")?;
    app.require_team(&s, &team_id, perm::MANAGE_TEAM).await?;
    if (patch.allow_open_invite.is_some() || patch.allowed_domains.is_some())
        && !app.has_team_perm(&s, &team_id, perm::INVITE_USER).await
    {
        return Err(AppError::forbidden_perm(perm::INVITE_USER));
    }
    let mut team = (*app.team(&team_id).await?).clone();
    if let Some(v) = patch.display_name {
        team.display_name = v;
    }
    if let Some(v) = patch.description {
        team.description = v;
    }
    if let Some(v) = patch.company_name {
        team.company_name = v;
    }
    if let Some(v) = patch.allowed_domains {
        team.allowed_domains = v;
    }
    if let Some(v) = patch.allow_open_invite {
        team.allow_open_invite = v;
        if !v {
            team.invite_id = new_id();
        }
    }
    if patch.group_constrained.is_some() {
        team.group_constrained = patch.group_constrained;
    }
    if let Some(v) = patch.cloud_limits_archived {
        team.cloud_limits_archived = v;
    }
    app.check_valid_domains(&team)?;
    app.update_team_row(&mut team).await?;
    app.send_team_event(&team, "update_team");
    app.sanitize_team(&s, &mut team).await;
    crate::audit::log(&app, Some(&s), "").await;
    Ok(Json(team))
}

/// `updateTeamPrivacy`: `O` opens the team, `I` makes it invite only (with a new invite id).
async fn update_privacy(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Team>> {
    require_team_id(&team_id)?;
    let props: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let privacy = props.get("privacy").and_then(Value::as_str).ok_or_else(|| AppError::invalid_param("privacy"))?;
    let open = match privacy {
        "O" => true,
        "I" => false,
        _ => return Err(AppError::invalid_param("privacy")),
    };
    app.require_team(&s, &team_id, perm::MANAGE_TEAM).await?;
    app.require_team(&s, &team_id, perm::INVITE_USER).await?;
    let mut team = (*app.team(&team_id).await?).clone();
    if (open != team.allow_open_invite || privacy != team.r#type) && (!open || privacy == "I") {
        team.invite_id = new_id();
    }
    team.r#type = privacy.into();
    team.allow_open_invite = open;
    app.update_team_row(&mut team).await?;
    app.send_team_event(&team, "update_team");
    let mut team = (*app.team(&team_id).await?).clone();
    app.sanitize_team(&s, &mut team).await;
    Ok(Json(team))
}

async fn restore_team(St(app): St<State>, Auth(s): Auth, Path(team_id): Path<String>) -> ApiResult<Json<Team>> {
    require_team_id(&team_id)?;
    app.require_team(&s, &team_id, perm::MANAGE_TEAM).await?;
    let mut team = (*app.team(&team_id).await?).clone();
    team.delete_at = 0;
    app.update_team_row(&mut team).await?;
    app.send_team_event(&team, "restore_team");
    let mut team = (*app.team(&team_id).await?).clone();
    app.sanitize_team(&s, &mut team).await;
    Ok(Json(team))
}

async fn regenerate_invite_id(St(app): St<State>, Auth(s): Auth, Path(team_id): Path<String>) -> ApiResult<Json<Team>> {
    require_team_id(&team_id)?;
    app.require_team(&s, &team_id, perm::MANAGE_TEAM).await?;
    app.require_team(&s, &team_id, perm::INVITE_USER).await?;
    let mut team = (*app.team(&team_id).await?).clone();
    team.invite_id = new_id();
    app.update_team_row(&mut team).await?;
    app.send_team_event(&team, "update_team");
    app.sanitize_team(&s, &mut team).await;
    crate::audit::log(&app, Some(&s), "").await;
    Ok(Json(team))
}

/// `deleteTeam`: archived (`SoftDeleteTeam`); `permanent=true` needs `EnableAPITeamDeletion`, off on the
/// servers this replaces.
async fn delete_team(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    require_team_id(&team_id)?;
    app.require_team(&s, &team_id, perm::MANAGE_TEAM).await?;
    if parse_bool(q.get("permanent")) {
        if app.cfg.bool("ServiceSettings.EnableAPITeamDeletion") {
            let team = (*app.team(&team_id).await?).clone();
            app.permanent_delete_team(team).await?;
            return Ok(status_ok());
        }
        let admin = app.user(&s.user_id).await?.is_some_and(|u| u.is_system_admin());
        let id = if admin {
            "api.user.delete_team.not_enabled.for_admin.app_error"
        } else {
            "api.user.delete_team.not_enabled.app_error"
        };
        return Err(AppError::new(StatusCode::UNAUTHORIZED, id, "").detail(format!("teamId={team_id}")));
    }
    let mut team = (*app.team(&team_id).await?).clone();
    // PostPersistentNotificationStore.DeleteByTeam: marked deleted.
    sqlx::query(
        "UPDATE PersistentNotifications SET DeleteAt = $2 FROM Posts, Channels
         WHERE Posts.Id = PersistentNotifications.PostId AND Posts.ChannelId = Channels.Id AND Channels.TeamId = ANY($1)",
    )
    .bind(vec![team.id.clone()])
    .bind(now_ms())
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.post_persistent_notification.delete_by_team.app_error", e.to_string()))?;
    team.delete_at = now_ms();
    app.update_team_row(&mut team).await?;
    app.send_team_event(&team, "delete_team");
    Ok(status_ok())
}

#[derive(Deserialize, Default)]
struct SchemeIdPatch {
    scheme_id: Option<String>,
}

/// `updateTeamScheme`: team schemes need a license.
async fn update_team_scheme(Auth(_): Auth, Path(team_id): Path<String>, body: Bytes) -> ApiResult<Json<Value>> {
    require_team_id(&team_id)?;
    let p: SchemeIdPatch = decode(&body, "scheme_id")?;
    match p.scheme_id.as_deref() {
        Some(id) if id.is_empty() || is_valid_id(id) => {}
        _ => return Err(AppError::invalid_param("scheme_id")),
    }
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.team.update_team_scheme.license.error", ""))
}

// ---------- the icon ----------

/// `setTeamIcon`: multipart `image`, upright, filled into 128×128, PNG at `teams/{id}/teamIcon.png`.
async fn set_icon(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    headers: HeaderMap,
    form: Result<Multipart, MultipartRejection>,
) -> ApiResult<Json<Value>> {
    require_team_id(&team_id)?;
    app.require_team(&s, &team_id, perm::MANAGE_TEAM).await?;
    let max = app.cfg.i64("FileSettings.MaxFileSize");
    let length = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<i64>().ok()).unwrap_or(-1);
    if length > max {
        return Err(bad("api.team.set_team_icon.too_large.app_error"));
    }
    let parse = |e: String| bad("api.team.set_team_icon.parse.app_error").detail(e);
    let mut form = form.map_err(|e| parse(e.to_string()))?;
    let mut data = None;
    while let Some(field) = form.next_field().await.map_err(|e| parse(e.to_string()))? {
        if field.name() == Some("image") && field.file_name().is_some() && data.is_none() {
            data = Some(field.bytes().await.map_err(|e| parse(e.to_string()))?);
        }
    }
    let data = data.ok_or_else(|| bad("api.team.set_team_icon.no_file.app_error"))?;
    let team = app
        .team(&team_id)
        .await
        .map_err(|e| bad("api.team.set_team_icon.get_team.app_error").detail(e.detailed_error))?;
    let max_res = app.cfg.i64("FileSettings.MaxImageResolution");
    // checkImageLimits: an unreadable header fails here, before decoding.
    match crate::images::header(&data) {
        Some(h) if max_res <= 0 || (h.width as i64) * (h.height as i64) <= max_res => {}
        _ => return Err(bad("api.team.set_team_icon.check_image_limits.app_error")),
    }
    let (img, _) =
        crate::images::decode_upright(&data).ok_or_else(|| bad("api.team.set_team_icon.decode.app_error"))?;
    let img = img.resize_to_fill(128, 128, image::imageops::FilterType::Lanczos3);
    let png =
        crate::images::png(&img).ok_or_else(|| AppError::internal("api.team.set_team_icon.encode.app_error", "png"))?;
    app.files
        .write(&format!("teams/{}/teamIcon.png", team.id), png.into())
        .await
        .map_err(|e| AppError::internal("api.team.set_team_icon.write_file.app_error", e.to_string()))?;
    let now = now_ms();
    sqlx::query("UPDATE Teams SET LastTeamIconUpdate = $1, UpdateAt = $1 WHERE Id = $2")
        .bind(now)
        .bind(&team.id)
        .execute(&app.db)
        .await
        .map_err(|e| bad("api.team.team_icon.update.app_error").detail(e.to_string()))?;
    let mut team = (*team).clone();
    team.last_team_icon_update = now;
    app.send_team_event(&team, "update_team");
    crate::audit::log(&app, Some(&s), "").await;
    Ok(status_ok())
}

async fn remove_icon(St(app): St<State>, Auth(s): Auth, Path(team_id): Path<String>) -> ApiResult<Json<Value>> {
    require_team_id(&team_id)?;
    app.require_team(&s, &team_id, perm::MANAGE_TEAM).await?;
    let team = app
        .team(&team_id)
        .await
        .map_err(|e| bad("api.team.remove_team_icon.get_team.app_error").detail(e.detailed_error))?;
    // Go's `UpdateLastTeamIconUpdate(id, 0)` sets UpdateAt to 0 too; the update time is kept meaningful here.
    sqlx::query("UPDATE Teams SET LastTeamIconUpdate = 0, UpdateAt = $2 WHERE Id = $1")
        .bind(&team.id)
        .bind(now_ms())
        .execute(&app.db)
        .await
        .map_err(|e| bad("api.team.team_icon.update.app_error").detail(e.to_string()))?;
    let mut team = (*team).clone();
    team.last_team_icon_update = 0;
    app.send_team_event(&team, "update_team");
    crate::audit::log(&app, Some(&s), "").await;
    Ok(status_ok())
}

// ---------- members ----------

/// `getTeamMembers`: by user id, or by username with `sort=username`; only users the session may see.
async fn get_members(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<TeamMember>>> {
    require_team_id(&team_id)?;
    let sort = q.get("sort").map(String::as_str).unwrap_or("");
    let exclude_deleted = q.get("exclude_deleted_users").is_some_and(|v| parse_bool(Some(v)));
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    let restriction = app.restriction_cond(&s).await?;
    let (page, per_page) = paging(&q);
    let mut sql = format!(
        "{} LEFT JOIN Users u ON tm.UserId = u.Id WHERE tm.TeamId = $1 AND tm.DeleteAt = 0",
        perm::team_members_select()
    );
    if exclude_deleted {
        sql += " AND u.DeleteAt = 0";
    }
    if let Some(r) = restriction {
        sql += &format!(" AND {r}");
    }
    sql += match sort {
        "" => " ORDER BY tm.UserId",
        "username" => " ORDER BY u.Username",
        _ => "",
    };
    sql += " LIMIT $2 OFFSET $3";
    let rows: Vec<perm::WithScheme<TeamMember>> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&team_id)
        .bind(per_page)
        .bind(page * per_page)
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("app.team.get_members.app_error", e.to_string()))?;
    let manage = app.has_team_perm(&s, &team_id, "manage_team_roles").await;
    Ok(Json(
        rows.into_iter()
            .map(|w| {
                let mut m = w.into_member();
                if !manage {
                    sanitize_member(&mut m, &s.user_id);
                }
                m
            })
            .collect(),
    ))
}

/// `removeTeamMember`: oneself, or another with `remove_user_from_team`.
async fn remove_member(
    St(app): St<State>,
    Auth(s): Auth,
    Path((team_id, user_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    require_team_id(&team_id)?;
    let user_id = user_param(&s, &user_id)?;
    if s.user_id != user_id {
        app.require_team(&s, &team_id, "remove_user_from_team").await?;
    }
    let team = app.team(&team_id).await?;
    let user = app.user(&user_id).await?.ok_or_else(crate::users::missing_account)?;
    if team.group_constrained == Some(true) && user_id != s.user_id && !user.is_bot {
        return Err(bad("api.team.remove_member.group_constrained.app_error"));
    }
    app.leave_team(&team, &user, &s.user_id).await?;
    Ok(status_ok())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SchemeRoles {
    scheme_admin: bool,
    scheme_user: bool,
    scheme_guest: bool,
}

/// `updateTeamMemberSchemeRoles`.
async fn update_member_scheme_roles(
    St(app): St<State>,
    Auth(s): Auth,
    Path((team_id, user_id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_team_id(&team_id)?;
    let user_id = user_param(&s, &user_id)?;
    let r: SchemeRoles = decode(&body, "scheme_roles")?;
    app.require_team(&s, &team_id, "manage_team_roles").await?;
    let m = app.team_member(&team_id, &user_id).await?.ok_or_else(|| {
        AppError::not_found(
            "app.team.get_member.missing.app_error",
            "No team member found for that user ID and team ID.",
        )
    })?;
    if m.scheme_guest {
        return Err(bad("api.team.update_team_member_roles.guest.app_error"));
    }
    if r.scheme_guest {
        return Err(bad("api.team.update_team_member_roles.user_and_guest.app_error"));
    }
    if !r.scheme_user {
        return Err(bad("api.team.update_team_member_roles.unset_user_scheme.app_error"));
    }
    sqlx::query(
        "UPDATE TeamMembers SET SchemeGuest = $3, SchemeUser = $4, SchemeAdmin = $5 WHERE TeamId = $1 AND UserId = $2",
    )
    .bind(&team_id)
    .bind(&user_id)
    .bind(r.scheme_guest)
    .bind(r.scheme_user)
    .bind(r.scheme_admin)
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.team.save_member.save.app_error", e.to_string()))?;
    app.clear_sessions_for_user(&user_id);
    if let Some(m) = app.team_member(&team_id, &user_id).await? {
        app.hub.publish(
            "memberrole_updated",
            json!({"member": serde_json::to_string(&m).unwrap()}),
            Broadcast { user_id: user_id.clone(), ..Default::default() },
        );
    }
    Ok(status_ok())
}

/// `teamMembersMinusGroupMembers`: the team's people outside the given groups (system console).
async fn members_minus_group_members(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    require_team_id(&team_id)?;
    let raw = q.get("group_ids").map(String::as_str).unwrap_or("");
    let stripped: String = raw.chars().filter(|c| c.is_ascii_alphanumeric() || *c == ',').collect();
    if stripped.len() < 26 {
        return Err(AppError::invalid_param("group_ids"));
    }
    let groups: Vec<String> = raw.split(',').map(str::to_string).collect();
    if !groups.iter().all(|g| is_valid_id(g)) {
        return Err(AppError::invalid_param("group_ids"));
    }
    app.require(&s, "sysconsole_read_user_management_groups").await?;
    let (page, per_page) = paging(&q);
    let base = "FROM TeamMembers tm JOIN Teams t ON t.Id = tm.TeamId JOIN Users u ON u.Id = tm.UserId
                LEFT JOIN Bots b ON b.UserId = tm.UserId
                LEFT JOIN GroupMembers gm ON gm.UserId = u.Id LEFT JOIN UserGroups ug ON ug.Id = gm.GroupId
                WHERE tm.DeleteAt = 0 AND t.DeleteAt = 0 AND u.DeleteAt = 0 AND b.UserId IS NULL AND t.Id = $1
                  AND u.Id NOT IN (SELECT gm2.UserId FROM GroupMembers gm2 JOIN UserGroups ug2 ON ug2.Id = gm2.GroupId
                                   WHERE gm2.DeleteAt = 0 AND gm2.GroupId = ANY($2))";
    let sql = format!(
        "SELECT u.Id, COALESCE(tm.SchemeGuest, false), tm.SchemeAdmin, tm.SchemeUser, string_agg(ug.Id, ',') {base}
         GROUP BY u.Id, tm.SchemeGuest, tm.SchemeAdmin, tm.SchemeUser ORDER BY u.Username ASC LIMIT $3 OFFSET $4"
    );
    let rows: Vec<(String, bool, bool, bool, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&team_id)
        .bind(&groups)
        .bind(per_page)
        .bind(page * per_page)
        .fetch_all(&app.db)
        .await?;
    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(DISTINCT u.Id) {base}")))
        .bind(&team_id)
        .bind(&groups)
        .fetch_one(&app.db)
        .await?;
    let show = app.show(&s).await;
    let mut users = vec![];
    for (id, guest, admin, user, group_ids) in rows {
        let Some(u) = app.user(&id).await? else { continue };
        let mut v = serde_json::to_value(app.profile_for(&u, &s, show)).unwrap_or(Value::Null);
        if let Value::Object(o) = &mut v {
            o.insert("group_ids".into(), json!(group_ids));
            o.insert("scheme_guest".into(), json!(guest));
            o.insert("scheme_user".into(), json!(user));
            o.insert("scheme_admin".into(), json!(admin));
        }
        users.push(v);
    }
    Ok(Json(json!({"users": users, "total_count": count})))
}

// ---------- reads and searches ----------

/// `model.TeamSearch`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct TeamSearch {
    term: String,
    page: Option<i64>,
    per_page: Option<i64>,
    allow_open_invite: Option<bool>,
    group_constrained: Option<bool>,
    exclude_policy_constrained: Option<bool>,
}

/// `searchTeams`: all the teams with both list permissions (archived ones too, as the system console wants),
/// otherwise the public or the private ones; `{teams, total_count}` when paged.
async fn search_teams(St(app): St<State>, BusyAuth(s): BusyAuth, body: Bytes) -> ApiResult<Response> {
    let mut o: TeamSearch = decode(&body, "team_search")?;
    if o.exclude_policy_constrained.is_some()
        && !app.has_perm(&s, "sysconsole_read_compliance_data_retention_policy").await
    {
        return Err(AppError::forbidden_perm("sysconsole_read_compliance_data_retention_policy"));
    }
    let private = app.has_perm(&s, "list_private_teams").await;
    let public = app.has_perm(&s, "list_public_teams").await;
    let paged = o.page.is_some() && o.per_page.is_some();
    let (mut teams, total) = if private && public {
        search_teams_sql(&app, &o, None, paged).await?
    } else if private {
        if o.page.is_some() || o.per_page.is_some() {
            return Err(AppError::new(
                StatusCode::NOT_IMPLEMENTED,
                "api.team.search_teams.pagination_not_implemented.private_team_search",
                "",
            ));
        }
        o.allow_open_invite = Some(false);
        o.group_constrained = None;
        search_teams_sql(&app, &o, None, false).await?
    } else if public {
        if o.page.is_some() || o.per_page.is_some() {
            return Err(AppError::new(
                StatusCode::NOT_IMPLEMENTED,
                "api.team.search_teams.pagination_not_implemented.public_team_search",
                "",
            ));
        }
        o.allow_open_invite = Some(true);
        o.group_constrained = None;
        search_teams_sql(&app, &o, Some("O"), false).await?
    } else {
        (vec![], 0)
    };
    for t in &mut teams {
        app.sanitize_team(&s, t).await;
    }
    if paged {
        return Ok(Json(json!({"teams": teams, "total_count": total})).into_response());
    }
    Ok(Json(teams).into_response())
}

/// `teamSearchQuery` (archived teams included, as Go's).
async fn search_teams_sql(app: &App, o: &TeamSearch, kind: Option<&str>, paged: bool) -> ApiResult<(Vec<Team>, i64)> {
    let mut cond = vec!["TRUE".to_string()];
    let term = o.term.replace('\\', "").replace('%', "\\%").replace('_', "\\_");
    let like = format!("%{}%", term.to_lowercase());
    if !o.term.is_empty() {
        cond.push("(t.Name ILIKE $1 OR t.DisplayName ILIKE $1)".into());
    } else {
        cond.push("$1::text IS NOT NULL".into());
    }
    if o.exclude_policy_constrained == Some(true) {
        cond.push("t.Id NOT IN (SELECT TeamId FROM RetentionPoliciesTeams)".into());
    }
    match o.allow_open_invite {
        Some(true) => cond.push("t.AllowOpenInvite = true".into()),
        Some(false) => cond.push(
            "(t.AllowOpenInvite <> true OR t.AllowOpenInvite IS NULL) AND (t.GroupConstrained <> true OR t.GroupConstrained IS NULL)"
                .into(),
        ),
        None => {}
    }
    match o.group_constrained {
        Some(true) => cond.push("t.GroupConstrained = true".into()),
        Some(false) => cond.push("(t.GroupConstrained <> true OR t.GroupConstrained IS NULL)".into()),
        None => {}
    }
    if let Some(k) = kind {
        cond.push(format!("t.Type = '{k}'"));
    }
    let where_ = cond.join(" AND ");
    let mut sql = format!("SELECT {} FROM Teams t WHERE {where_} ORDER BY t.DisplayName", db::cols::<Team>("t"));
    if paged {
        let (page, per_page) = (o.page.unwrap_or(0).max(0), o.per_page.unwrap_or(0).max(0));
        sql += &format!(" LIMIT {per_page} OFFSET {}", page * per_page);
    }
    let teams: Vec<Team> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&like)
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("app.team.search_all_team.app_error", e.to_string()))?;
    let total = if paged {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM Teams t WHERE {where_}")))
            .bind(&like)
            .fetch_one(&app.db)
            .await?
    } else {
        teams.len() as i64
    };
    Ok((teams, total))
}

/// `teamExists`: whether the session can see a team of that name.
async fn team_exists(St(app): St<State>, Auth(s): Auth, Path(name): Path<String>) -> ApiResult<Response> {
    if !valid_team_name(&name) {
        return Err(AppError::invalid_url_param("team_name"));
    }
    let team: Option<Team> = db::one(&app.db, "name = $1", &name).await?;
    let mut exists = false;
    if let Some(t) = team {
        let member = app.team_member(&t.id, &s.user_id).await?;
        exists = member.is_some_and(|m| m.delete_at == 0)
            || t.allow_open_invite && app.has_perm(&s, "list_public_teams").await
            || !t.allow_open_invite && app.has_perm(&s, "list_private_teams").await;
    }
    Ok(([(header::CONTENT_TYPE, "application/json")], format!(r#"{{"exists":{exists}}}"#)).into_response())
}

/// `invalidateAllEmailInvites`: every team and guest invitation token, and the pending resend jobs.
async fn invalidate_email_invites(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    app.require(&s, "invalidate_email_invite").await?;
    let err = |e: sqlx::Error| AppError::internal("api.team.invalidate_all_email_invites.app_error", e.to_string());
    sqlx::query("DELETE FROM Tokens WHERE Type = 'team_invitation'").execute(&app.db).await.map_err(err)?;
    sqlx::query("DELETE FROM Tokens WHERE Type = 'guest_invitation'").execute(&app.db).await.map_err(err)?;
    let jobs: Vec<(String,)> = sqlx::query_as(
        "UPDATE Jobs SET Status = 'canceled', LastActivityAt = $1 WHERE Type = 'resend_invitation_email' AND Status = 'pending'
         RETURNING Id",
    )
    .bind(now_ms())
    .fetch_all(&app.db)
    .await
    .map_err(err)?;
    for (id,) in jobs {
        sqlx::query("DELETE FROM Systems WHERE Name = $1").bind(&id).execute(&app.db).await.map_err(err)?;
    }
    Ok(status_ok())
}

/// `model.TeamUnread`.
#[derive(Serialize, Default)]
struct TeamUnread {
    team_id: String,
    msg_count: i64,
    mention_count: i64,
    mention_count_root: i64,
    msg_count_root: i64,
    thread_count: i64,
    thread_mention_count: i64,
    thread_urgent_mention_count: i64,
}

/// `getTeamUnread`: the channel unreads of one team (messages not counted where only mentions mark unread).
async fn team_unread(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id)): Path<(String, String)>,
) -> ApiResult<Json<TeamUnread>> {
    let user_id = user_param(&s, &user_id)?;
    require_team_id(&team_id)?;
    if !app.user_perm(&s, &user_id).await {
        return Err(AppError::forbidden_perm(perm::EDIT_OTHER_USERS));
    }
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    let rows: Vec<(i64, i64, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT c.TotalMsgCount - cm.MsgCount, c.TotalMsgCountRoot - cm.MsgCountRoot, cm.MentionCount, cm.MentionCountRoot,
                cm.NotifyProps ->> 'mark_unread'
         FROM Channels c JOIN ChannelMembers cm ON c.Id = cm.ChannelId
         WHERE cm.UserId = $1 AND c.TeamId = $2 AND c.DeleteAt = 0 AND c.Type <> 'S'",
    )
    .bind(&user_id)
    .bind(&team_id)
    .fetch_all(&app.db)
    .await
    .map_err(|e| AppError::internal("app.team.get_unread.app_error", e.to_string()))?;
    let mut u = TeamUnread { team_id, ..Default::default() };
    for (msg, msg_root, mention, mention_root, mark) in rows {
        u.mention_count += mention;
        u.mention_count_root += mention_root;
        if mark.as_deref() != Some("mention") {
            u.msg_count += msg;
            u.msg_count_root += msg_root;
        }
    }
    Ok(Json(u))
}

/// `readAllInTeam`: behind the `EnableShiftEscapeToMarkAllRead` feature flag, off in 12.1.
async fn read_all_in_team(Auth(_): Auth) -> ApiResult<Json<Value>> {
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.mark_all_as_read.disabled.app_error", ""))
}

/// `requireLicense`: LDAP groups.
async fn license_error(Auth(_): Auth) -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "api.license_error", "")
}

/// `inviteGuestsToChannels`: guest accounts need a license.
async fn invite_guests(Auth(_): Auth) -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "api.team.invite_guests_to_channels.license.error", "")
}

/// `getTeamAccessControlPolicy` without attribute-based access control: no policy.
async fn access_control_policy(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_team_id(&team_id)?;
    if !app.has_perm(&s, perm::MANAGE_SYSTEM).await
        && !app.has_team_perm(&s, &team_id, "manage_team_access_rules").await
    {
        return Err(AppError::forbidden_perm("manage_team_access_rules"));
    }
    Ok(Json(json!({"policy": null, "enforced": false})))
}

/// `getTeamAccessControlAttributes`: no access control service on this server.
async fn access_control_attributes(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_team_id(&team_id)?;
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "app.pap.get_channel_access_control_attributes.app_error", ""))
}

// ---------- the channels of a team ----------

/// `getPublicChannelsByIdsForTeam`.
async fn public_channels_by_ids(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Vec<Channel>>> {
    require_team_id(&team_id)?;
    let mut ids: Vec<String> = serde_json::from_slice(&body)
        .map_err(|e| AppError::bad_request("api.payload.parse.error", "").detail(e.to_string()))?;
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(AppError::invalid_param("channel_ids"));
    }
    if !ids.iter().all(|i| is_valid_id(i)) {
        return Err(AppError::invalid_param("channel_id"));
    }
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    let sql = format!(
        "SELECT {} FROM Channels JOIN PublicChannels pc ON (pc.Id = Channels.Id)
         WHERE pc.TeamId = $1 AND pc.DeleteAt = 0 AND pc.Id = ANY($2) ORDER BY pc.DisplayName",
        db::cols::<Channel>("Channels")
    );
    let mut list: Vec<Channel> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&team_id)
        .bind(&ids)
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("app.channel.get_channels_by_ids.get.app_error", e.to_string()))?;
    if list.is_empty() {
        return Err(AppError::not_found("app.channel.get_channels_by_ids.not_found.app_error", ""));
    }
    if s.prop("is_guest") == "true" {
        for c in &list {
            if !app.channel_perm(&s, &c.id, perm::READ_CHANNEL).await?.0 {
                return Err(AppError::forbidden_perm(perm::READ_CHANNEL));
            }
        }
    }
    app.fill_channel_props(&mut list).await?;
    Ok(Json(list))
}

/// `getPrivateChannelsForTeam`: system admins.
async fn private_channels(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<Channel>>> {
    require_team_id(&team_id)?;
    app.require(&s, perm::MANAGE_SYSTEM).await?;
    let (page, per_page) = paging(&q);
    let sql = format!(
        "{} WHERE Type = 'P' AND TeamId = $1 AND DeleteAt = 0 ORDER BY DisplayName LIMIT $2 OFFSET $3",
        Channel::SELECT
    );
    let mut list: Vec<Channel> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&team_id)
        .bind(per_page)
        .bind(page * per_page)
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("app.channel.get_private_channels.get.app_error", e.to_string()))?;
    app.fill_channel_props(&mut list).await?;
    Ok(Json(list))
}

/// `getRecommendedChannelsForTeam`: recommendations come from attribute-based access control, which needs a
/// license; none.
async fn recommended_channels(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
) -> ApiResult<Json<Vec<Channel>>> {
    require_team_id(&team_id)?;
    app.require_team(&s, &team_id, perm::LIST_TEAM_CHANNELS).await?;
    Ok(Json(vec![]))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChannelSearch {
    term: String,
}

/// `searchChannelsForTeam`: the team's public channels (archived too); without `list_team_channels`, only those
/// the member is in.
async fn search_channels(
    St(app): St<State>,
    BusyAuth(s): BusyAuth,
    Path(team_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Vec<Channel>>> {
    require_team_id(&team_id)?;
    let b: Option<ChannelSearch> = serde_json::Deserializer::from_slice(&body)
        .into_iter::<Option<ChannelSearch>>()
        .next()
        .and_then(Result::ok)
        .flatten();
    let b = b.ok_or_else(|| AppError::invalid_param("channel_search"))?;
    let term = b.term.trim();
    let mut sql = format!(
        "SELECT {} FROM Channels JOIN PublicChannels c ON (c.Id = Channels.Id)",
        db::cols::<Channel>("Channels")
    );
    let member_only = !app.has_team_perm(&s, &team_id, perm::LIST_TEAM_CHANNELS).await;
    if member_only {
        app.team_member(&team_id, &s.user_id).await?.ok_or_else(|| {
            AppError::not_found(
                "app.team.get_member.missing.app_error",
                "No team member found for that user ID and team ID.",
            )
        })?;
        sql += " JOIN ChannelMembers cm ON (c.Id = cm.ChannelId) WHERE c.TeamId = $1 AND cm.UserId = $2";
    } else {
        sql += " WHERE c.TeamId = $1 AND $2::text IS NOT NULL";
    }
    let like = crate::channels::like_term(term);
    if like.is_some() {
        sql += &format!(" AND {}", app.search_sql(3, &["c.Name", "c.DisplayName", "c.Purpose"]).await?);
    }
    sql += " ORDER BY c.DisplayName LIMIT 100";
    let mut q = sqlx::query_as::<_, Channel>(sqlx::AssertSqlSafe(sql)).bind(&team_id).bind(&s.user_id);
    if let Some(l) = &like {
        q = q.bind(l).bind(crate::channels::fulltext_term(term));
    }
    let list =
        q.fetch_all(&app.db).await.map_err(|e| AppError::internal("app.channel.search.app_error", e.to_string()))?;
    Ok(Json(list))
}

/// `autocompleteChannelsForTeamForSearch` (`AutocompleteInTeamForSearch`): the user's channels of the team and
/// group messages matching the term, and their direct messages with matching people (named after them).
async fn autocomplete_for_search(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<Channel>>> {
    require_team_id(&team_id)?;
    let term = q.get("name").map(String::as_str).unwrap_or("").trim();
    let err = |e: sqlx::Error| AppError::internal("app.channel.search.app_error", e.to_string());
    let base = format!(
        "SELECT {} FROM Channels AS C JOIN ChannelMembers AS CM ON CM.ChannelId = C.Id
         WHERE (C.TeamId = $1 OR (C.TeamId = '' AND C.Type = 'G')) AND CM.UserId = $2 AND C.Type IN ('O', 'P', 'D', 'G')",
        db::cols::<Channel>("C")
    );
    let like = crate::channels::like_term(term);
    let mut list: Vec<Channel> = match &like {
        None => sqlx::query_as(sqlx::AssertSqlSafe(format!("{base} LIMIT 50")))
            .bind(&team_id)
            .bind(&s.user_id)
            .fetch_all(&app.db)
            .await
            .map_err(err)?,
        Some(l) => {
            // Go's UNION of the LIKE and the full text queries; `search_sql` is their OR.
            let full = app.search_sql(3, &["C.Name", "C.DisplayName", "C.Purpose"]).await?;
            sqlx::query_as(sqlx::AssertSqlSafe(format!("{base} AND {full} LIMIT 50")))
                .bind(&team_id)
                .bind(&s.user_id)
                .bind(l)
                .bind(crate::channels::fulltext_term(term))
                .fetch_all(&app.db)
                .await
                .map_err(err)?
        }
    };
    // autocompleteInTeamForSearchDirectMessages: the other member's username as the display name.
    let cols = db::cols::<Channel>("C").replace("C.displayname", "OtherUsers.Username AS displayname");
    let mut sql = format!(
        "SELECT {cols} FROM Channels AS C JOIN ChannelMembers AS CM ON CM.ChannelId = C.Id
         INNER JOIN (SELECT ICM.ChannelId AS ChannelId, IU.Username AS Username FROM Users AS IU
                     JOIN ChannelMembers AS ICM ON ICM.UserId = IU.Id WHERE IU.Id <> $1"
    );
    if like.is_some() {
        sql += " AND (LOWER(IU.Username) LIKE LOWER($2) ESCAPE '*' OR LOWER(IU.Nickname) LIKE LOWER($2) ESCAPE '*')";
    } else {
        sql += " AND $2::text IS NULL";
    }
    sql += ") AS OtherUsers ON OtherUsers.ChannelId = C.Id WHERE C.Type = 'D' AND CM.UserId = $1 LIMIT 50";
    let dms: Vec<Channel> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&s.user_id).bind(&like).fetch_all(&app.db).await.map_err(err)?;
    list.extend(dms);
    list.sort_by_key(|c| c.display_name.to_lowercase());
    Ok(Json(list))
}
