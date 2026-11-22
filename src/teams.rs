//! Teams and team members: reading (the team ids of a user through their cache), joining (by invite, by an
//! admin, the default channels), unread counts with collapsed threads, icons and invitations.

// Based on Mattermost server/channels/api4/team.go, app/team.go, app/teams, app/channel.go (JoinDefaultChannels)
// and store/sqlstore/team_store.go, thread_store.go (GetTeamsUnreadForUser), Copyright (c) 2015-present
// Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::sync::Arc;

use crate::app::Query;
use axum::extract::{Path, State as St};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;

use crate::app::{ApiResult, App, AppError, Auth, Body, MaybeAuth, State, is_valid_id, now_ms};
use crate::db::{self, Table};
use crate::hub::Broadcast;
use crate::model::{Channel, Post, Session, Team, TeamMember, User, channel};
use crate::perm;
use crate::preferences::user_param;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/teams", get(all_teams))
        .route("/api/v4/users/{user_id}/teams", get(teams_for_user))
        .route("/api/v4/users/{user_id}/teams/unread", get(teams_unread))
        .route("/api/v4/users/{user_id}/teams/members", get(members_for_user))
        .route("/api/v4/teams/name/{team_name}", get(team_by_name))
        .route("/api/v4/teams/{team_id}", get(get_team))
        .route("/api/v4/teams/{team_id}/stats", get(team_stats))
        .route("/api/v4/teams/{team_id}/image", get(team_icon))
        .route("/api/v4/teams/{team_id}/members", post(add_member))
        .route("/api/v4/teams/{team_id}/members/batch", post(add_members))
        .route("/api/v4/teams/{team_id}/members/ids", post(members_by_ids))
        .route("/api/v4/teams/{team_id}/members/{user_id}", get(get_member))
        .route("/api/v4/teams/members/invite", post(join_from_invite))
        .route("/api/v4/teams/invite/{invite_id}", get(invite_info))
        .route("/api/v4/teams/{team_id}/invite/email", post(invite_by_email))
}

fn team_not_found() -> AppError {
    AppError::not_found("app.team.get.find.app_error", "Unable to find the existing team.")
}

/// `TeamMember.SanitizeRoleData`.
pub fn sanitize_member(m: &mut TeamMember, viewer: &str) {
    if m.user_id != viewer {
        m.roles.clear();
        m.explicit_roles.clear();
        m.scheme_admin = false;
        m.scheme_guest = false;
        m.scheme_user = false;
        m.delete_at = -1;
    }
}

/// `normalizeDomains`.
fn domains(s: &str) -> Vec<String> {
    s.to_lowercase().replace(['@', ','], " ").split_whitespace().map(str::to_string).collect()
}

/// `IsEmailAddressAllowed`.
pub fn email_allowed(email: &str, allowed: &[&str]) -> bool {
    allowed.iter().all(|r| {
        let d = domains(r);
        d.is_empty() || d.iter().any(|d| email.ends_with(&format!("@{d}")))
    })
}

impl App {
    pub async fn team(&self, id: &str) -> ApiResult<Arc<Team>> {
        let t: Option<Team> = db::get(&self.db, id).await?;
        t.map(Arc::new).ok_or_else(team_not_found)
    }

    pub async fn team_by_name(&self, name: &str) -> ApiResult<Team> {
        db::one(&self.db, "name = $1", name).await?.ok_or_else(|| {
            AppError::not_found("app.team.get_by_name.missing.app_error", "Unable to find the existing team.")
        })
    }

    /// `SanitizeTeam`: the email for team managers, the invite id for inviters.
    pub async fn sanitize_team(&self, s: &Session, t: &mut Team) {
        let manage = self.has_team_perm(s, &t.id, perm::MANAGE_TEAM).await;
        let invite = self.has_team_perm(s, &t.id, perm::INVITE_USER).await;
        if !manage {
            t.email.clear();
        }
        if !invite {
            t.invite_id.clear();
        }
    }

    /// `TeamStore.GetUserTeamIds` through the team cache.
    pub async fn user_team_ids(&self, user_id: &str) -> ApiResult<Arc<Vec<String>>> {
        if let Some(ids) = self.caches.team_ids_for_user.get(user_id) {
            return Ok(ids);
        }
        let ids: Vec<(String,)> = sqlx::query_as(
            "SELECT tm.TeamId FROM TeamMembers tm JOIN Teams t ON tm.TeamId = t.Id
             WHERE tm.UserId = $1 AND tm.DeleteAt = 0 AND t.DeleteAt = 0",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        let ids = Arc::new(ids.into_iter().map(|r| r.0).collect::<Vec<_>>());
        self.caches.team_ids_for_user.put(user_id.to_string(), ids.clone());
        Ok(ids)
    }

    /// `GetTeamsForUser`.
    pub async fn teams_of_user(&self, user_id: &str) -> ApiResult<Vec<Team>> {
        let sql = format!(
            "SELECT {} FROM Teams t JOIN TeamMembers tm ON tm.TeamId = t.Id
             WHERE tm.UserId = $1 AND tm.DeleteAt = 0 AND t.DeleteAt = 0",
            db::cols::<Team>("t")
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(user_id).fetch_all(&self.db).await?)
    }

    pub async fn team_member(&self, team_id: &str, user_id: &str) -> ApiResult<Option<TeamMember>> {
        Ok(perm::team_members(self, "tm.teamid = $1 AND tm.userid = $2", &[team_id, user_id]).await?.into_iter().next())
    }

    /// The caches a team membership change makes stale (`ClearSessionCacheForUser`, `InvalidateCacheForUser`,
    /// `invalidateCacheForUserTeams`), and the user's websockets' team list.
    pub async fn invalidate_user_teams(&self, user_id: &str) {
        self.clear_sessions_for_user(user_id);
        self.invalidate_user(user_id);
        self.caches.team_ids_for_user.remove(user_id);
        if let Ok(ids) = self.user_team_ids(user_id).await {
            self.hub.set_user_teams(user_id, &ids);
        }
    }

    /// `JoinUserToTeam`: the membership (new, or the old one restored), sidebar categories, default channels,
    /// caches, `added_to_team`. Returns the member and whether it was there already.
    pub async fn join_user_to_team(&self, team: &Team, user: &User, requestor: &str) -> ApiResult<TeamMember> {
        // `IsTeamEmailAllowed`: a guest against the guests' domains only, not the team's.
        if !self.team_email_allowed(user, team) {
            return Err(AppError::bad_request(
                "api.team.join_user_to_team.allowed_domains.app_error",
                "The user cannot be added as the domain associated with the account is not permitted. Contact your System Administrator for additional details.",
            ));
        }
        let guest = user.is_guest();
        let admin = !guest && team.email == user.email;
        let max = self.cfg.i64("TeamSettings.MaxUsersPerTeam");
        let max_err = || {
            AppError::bad_request(
                "app.team.join_user_to_team.max_accounts.app_error",
                "This team has reached the maximum number of allowed accounts. Contact your System Administrator to set a higher limit.",
            )
        };
        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT COALESCE(DeleteAt, 0) FROM TeamMembers WHERE TeamId = $1 AND UserId = $2")
                .bind(&team.id)
                .bind(&user.id)
                .fetch_optional(&self.db)
                .await?;
        if let Some((0,)) = existing {
            return Ok(self.team_member(&team.id, &user.id).await?.unwrap_or_default());
        }
        if max >= 0 {
            let (count,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM TeamMembers tm JOIN Users u ON tm.UserId = u.Id
                 WHERE tm.TeamId = $1 AND tm.DeleteAt = 0 AND u.DeleteAt = 0",
            )
            .bind(&team.id)
            .fetch_one(&self.db)
            .await?;
            if count + 1 > max {
                return Err(max_err());
            }
        }
        let now = now_ms();
        let q = if existing.is_some() {
            "UPDATE TeamMembers SET Roles = '', DeleteAt = 0, SchemeUser = $3, SchemeAdmin = $4, SchemeGuest = $5
             WHERE TeamId = $1 AND UserId = $2 AND $6 > 0"
        } else {
            "INSERT INTO TeamMembers (TeamId, UserId, Roles, DeleteAt, SchemeUser, SchemeAdmin, SchemeGuest, CreateAt)
             VALUES ($1, $2, '', 0, $3, $4, $5, $6)"
        };
        sqlx::query(q)
            .bind(&team.id)
            .bind(&user.id)
            .bind(!guest)
            .bind(admin)
            .bind(guest)
            .bind(now)
            .execute(&self.db)
            .await
            .map_err(|e| AppError::internal("app.team.join_user_to_team.save_member.app_error", e.to_string()))?;
        sqlx::query("UPDATE Users SET UpdateAt = $1 WHERE Id = $2").bind(now).bind(&user.id).execute(&self.db).await?;
        if let Err(e) = self.create_initial_sidebar(&user.id, &team.id).await {
            eprintln!("creating sidebar categories of {} in {}: {}", user.id, team.id, e.detailed_error);
        }
        if !guest && let Err(e) = self.join_default_channels(&team.id, user, admin, requestor).await {
            eprintln!("joining default channels of {}: {}", team.id, e.detailed_error);
        }
        self.invalidate_user_teams(&user.id).await;
        self.publish_added_to_team(&team.id, &user.id);
        Ok(self.team_member(&team.id, &user.id).await?.unwrap_or_default())
    }

    fn publish_added_to_team(&self, team_id: &str, user_id: &str) {
        self.hub.publish(
            "added_to_team",
            json!({"team_id": team_id, "user_id": user_id}),
            Broadcast { user_id: user_id.into(), ..Default::default() },
        );
    }

    /// `DefaultChannelNames`.
    pub(crate) fn default_channel_names(&self) -> Vec<String> {
        let mut names = vec!["town-square".to_string()];
        let extra: Vec<String> = self
            .cfg
            .get("TeamSettings.ExperimentalDefaultChannels")
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if extra.is_empty() {
            names.push("off-topic".into());
        }
        for n in extra {
            if !names.contains(&n) {
                names.push(n);
            }
        }
        names
    }

    /// `JoinDefaultChannels`: town-square and off-topic (or the configured ones), the join message only with
    /// `ExperimentalEnableDefaultChannelLeaveJoinMessages`.
    pub async fn join_default_channels(
        &self,
        team_id: &str,
        user: &User,
        admin: bool,
        requestor: &str,
    ) -> ApiResult<()> {
        let requestor = if requestor.is_empty() { None } else { self.user(requestor).await? };
        let mut err = None;
        for name in self.default_channel_names() {
            let Some(c) = self.channel_by_name(team_id, &name, false, true).await? else { continue };
            if c.r#type != channel::OPEN {
                continue;
            }
            let now = now_ms();
            let r = sqlx::query(
                "INSERT INTO ChannelMembers (ChannelId, UserId, Roles, LastViewedAt, MsgCount, MentionCount,
                     MentionCountRoot, UrgentMentionCount, MsgCountRoot, NotifyProps, LastUpdateAt, SchemeUser,
                     SchemeAdmin, SchemeGuest, AutoTranslationDisabled)
                 VALUES ($1, $2, '', 0, 0, 0, 0, 0, 0, $3, $4, $5, $6, $7, false)",
            )
            .bind(&c.id)
            .bind(&user.id)
            .bind(crate::model::channel::default_member_notify_props())
            .bind(now)
            .bind(!user.is_guest())
            .bind(admin)
            .bind(user.is_guest())
            .execute(&self.db)
            .await;
            if let Err(e) = r {
                err = Some(e);
            }
            sqlx::query("INSERT INTO ChannelMemberHistory (ChannelId, UserId, JoinTime) VALUES ($1, $2, $3)")
                .bind(&c.id)
                .bind(&user.id)
                .bind(now)
                .execute(&self.db)
                .await?;
            if self.cfg.bool("ServiceSettings.ExperimentalEnableDefaultChannelLeaveJoinMessages") {
                let post = join_message(user, requestor.as_deref(), &c);
                if let Err(e) = self.create_post(post, true).await {
                    eprintln!("join message in {}: {}", c.id, e.detailed_error);
                }
            }
            self.caches.invalidate_channel_members(&c.id);
            self.caches.invalidate_members_for_user(&user.id);
            self.hub.publish(
                "user_added",
                json!({"user_id": user.id, "team_id": c.team_id}),
                Broadcast { channel_id: c.id.clone(), ..Default::default() },
            );
        }
        match err {
            Some(e) if !e.to_string().contains("duplicate key") => {
                Err(AppError::internal("api.channel.join_channel.post_and_forget", e.to_string()))
            }
            _ => Ok(()),
        }
    }

    /// `GetViewUsersRestrictions` reduced to what Team Edition can have: whether the viewer may see everyone.
    pub async fn sees_everyone(&self, s: &Session) -> bool {
        self.has_perm(s, perm::VIEW_MEMBERS).await
    }

    /// `GetViewUsersRestrictions`: None for users with `view_members`; else the teams where they have it (from
    /// the team cache) and every channel they are in (from the memberships cache).
    pub async fn view_restrictions(&self, user_id: &str) -> ApiResult<Option<ViewRestrictions>> {
        if self.user_has_perm(user_id, perm::VIEW_MEMBERS).await {
            return Ok(None);
        }
        let mut teams = vec![];
        for t in self.user_team_ids(user_id).await?.iter() {
            if self.user_team_perm(user_id, t, perm::VIEW_MEMBERS).await {
                teams.push(t.clone());
            }
        }
        let channels = self.member_roles(user_id, true).await?.keys().cloned().collect();
        Ok(Some(ViewRestrictions { teams, channels }))
    }

    /// `applyViewRestrictionsFilter` for a query on `Users u`: None when the viewer sees everyone.
    pub async fn restriction_cond(&self, s: &Session) -> ApiResult<Option<String>> {
        Ok(self.view_restrictions(&s.user_id).await?.map(|r| r.sql()))
    }

    /// `UserCanSeeOtherUser`.
    pub async fn can_see_user(&self, s: &Session, other: &str) -> ApiResult<bool> {
        if s.user_id == other {
            return Ok(true);
        }
        let Some(r) = self.view_restrictions(&s.user_id).await? else { return Ok(true) };
        if !r.teams.is_empty() {
            let found: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM TeamMembers WHERE UserId = $1 AND TeamId = ANY($2) AND DeleteAt = 0)",
            )
            .bind(other)
            .bind(&r.teams)
            .fetch_one(&self.db)
            .await?;
            if found {
                return Ok(true);
            }
        }
        if !r.channels.is_empty() {
            let found: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM ChannelMembers WHERE UserId = $1 AND ChannelId = ANY($2))",
            )
            .bind(other)
            .bind(&r.channels)
            .fetch_one(&self.db)
            .await?;
            if found {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// `model.ViewUsersRestrictions`.
pub struct ViewRestrictions {
    pub teams: Vec<String>,
    pub channels: Vec<String>,
}

impl ViewRestrictions {
    /// The SQL condition on `u.id` (ids are checked, then inlined), `1 = 0` when there is nothing to see.
    pub fn sql(&self) -> String {
        let list = |ids: &[String]| {
            ids.iter().filter(|i| is_valid_id(i)).map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", ")
        };
        let mut parts = vec![];
        if !self.teams.is_empty() {
            parts.push(format!(
                "u.id IN (SELECT r.UserId FROM TeamMembers r WHERE r.DeleteAt = 0 AND r.TeamId IN ({}))",
                list(&self.teams)
            ));
        }
        if !self.channels.is_empty() {
            parts.push(format!(
                "u.id IN (SELECT r.UserId FROM ChannelMembers r WHERE r.ChannelId IN ({}))",
                list(&self.channels)
            ));
        }
        if parts.is_empty() { "1 = 0".into() } else { format!("({})", parts.join(" OR ")) }
    }
}

/// `postJoinTeamMessage` / `postAddToTeamMessage` / `postJoinChannelMessage` / `PostAddToChannelMessage`.
fn join_message(user: &User, requestor: Option<&User>, c: &Channel) -> Post {
    let team = c.name == "town-square";
    let mut props = serde_json::Map::new();
    let (kind, message, author) = match requestor {
        None => {
            props.insert("username".into(), json!(user.username));
            if team {
                ("system_join_team", format!("{} joined the team.", user.username), &user.id)
            } else {
                ("system_join_channel", format!("{} joined the channel.", user.username), &user.id)
            }
        }
        Some(r) => {
            props.insert("userId".into(), json!(r.id));
            props.insert("username".into(), json!(r.username));
            props.insert("addedUserId".into(), json!(user.id));
            props.insert("addedUsername".into(), json!(user.username));
            if team {
                ("system_add_to_team", format!("{} added to the team by {}.", user.username, r.username), &r.id)
            } else {
                ("system_add_to_channel", format!("{} added to the channel by {}.", user.username, r.username), &r.id)
            }
        }
    };
    Post {
        channel_id: c.id.clone(),
        message,
        r#type: kind.into(),
        user_id: author.clone(),
        props: sqlx::types::Json(props),
        ..Default::default()
    }
}

// ---------- reads ----------

/// `getAllTeams`: what the session may list, archived teams included (`GetAllPage` and `AnalyticsTeamCount` with
/// options have no `DeleteAt` filter).
async fn all_teams(St(app): St<State>, Auth(s): Auth, Query(q): Query<HashMap<String, String>>) -> ApiResult<Response> {
    let (page, per_page) = crate::channels_api::paging(&q);
    let private = app.has_perm(&s, "list_private_teams").await;
    let public = app.has_perm(&s, "list_public_teams").await;
    let filter = match (private, public) {
        (true, true) => "",
        (true, false) => " AND NOT COALESCE(AllowOpenInvite, false)",
        (false, true) => " AND COALESCE(AllowOpenInvite, false)",
        _ => {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "api.team.get_all_teams.insufficient_permissions",
                "You don't have the appropriate permissions to list all teams",
            ));
        }
    };
    let sql = format!("{} WHERE true{filter} ORDER BY DisplayName LIMIT $1 OFFSET $2", Team::SELECT);
    let mut teams: Vec<Team> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(per_page)
        .bind(per_page.saturating_mul(page))
        .fetch_all(&app.db)
        .await?;
    for t in &mut teams {
        app.sanitize_team(&s, t).await;
    }
    if crate::channels_api::parse_bool(q.get("include_total_count")) {
        let sql = format!("SELECT COUNT(*) FROM Teams WHERE true{filter}");
        let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql)).fetch_one(&app.db).await?;
        return Ok(Json(json!({"teams": teams, "total_count": n})).into_response());
    }
    Ok(Json(teams).into_response())
}

async fn teams_for_user(St(app): St<State>, Auth(s): Auth, Path(user_id): Path<String>) -> ApiResult<Json<Vec<Team>>> {
    let user_id = user_param(&s, &user_id)?;
    if s.user_id != user_id && !app.has_perm(&s, perm::SYSCONSOLE_READ_USERMANAGEMENT_USERS).await {
        return Err(AppError::forbidden_perm(perm::SYSCONSOLE_READ_USERMANAGEMENT_USERS));
    }
    let mut teams = app.teams_of_user(&user_id).await?;
    for t in &mut teams {
        app.sanitize_team(&s, t).await;
    }
    Ok(Json(teams))
}

/// `model.TeamUnread`.
#[derive(Serialize, Default, Clone)]
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

#[derive(Deserialize, Default)]
#[serde(default)]
struct UnreadQuery {
    exclude_team: String,
    include_collapsed_threads: String,
}

/// `GetTeamsUnreadForUser`: channel unreads summed per team; with collapsed threads, the followed threads'
/// unreads from one query driven by the user's thread memberships.
async fn teams_unread(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Query(q): Query<UnreadQuery>,
) -> ApiResult<Json<Vec<TeamUnread>>> {
    let user_id = user_param(&s, &user_id)?;
    if s.user_id != user_id && !app.has_perm(&s, perm::MANAGE_SYSTEM).await {
        return Err(AppError::forbidden_perm(perm::MANAGE_SYSTEM));
    }
    let rows = sqlx::query(
        "SELECT COALESCE(c.TeamId, '') AS teamid, COALESCE(c.TotalMsgCount, 0) - COALESCE(cm.MsgCount, 0) AS msgcount,
                COALESCE(c.TotalMsgCountRoot, 0) - COALESCE(cm.MsgCountRoot, 0) AS msgcountroot,
                COALESCE(cm.MentionCount, 0) AS mentioncount, COALESCE(cm.MentionCountRoot, 0) AS mentioncountroot,
                cm.NotifyProps ->> 'mark_unread' AS markunread
         FROM Channels c JOIN ChannelMembers cm ON c.Id = cm.ChannelId
         WHERE cm.UserId = $1 AND c.DeleteAt = 0 AND c.TeamId <> $2 AND c.Type <> 'S'",
    )
    .bind(&user_id)
    .bind(&q.exclude_team)
    .fetch_all(&app.db)
    .await?;
    let mut order: Vec<String> = vec![];
    let mut map: HashMap<String, TeamUnread> = HashMap::new();
    for r in rows {
        let team: String = r.try_get("teamid")?;
        let tu = map.entry(team.clone()).or_insert_with(|| {
            order.push(team.clone());
            TeamUnread { team_id: team.clone(), ..Default::default() }
        });
        tu.mention_count += r.try_get::<i64, _>("mentioncount")?;
        tu.mention_count_root += r.try_get::<i64, _>("mentioncountroot")?;
        if r.try_get::<Option<String>, _>("markunread")?.as_deref() != Some("mention") {
            tu.msg_count += r.try_get::<i64, _>("msgcount")?;
            tu.msg_count_root += r.try_get::<i64, _>("msgcountroot")?;
        }
    }
    let crt = q.include_collapsed_threads == "true" && app.cfg.str("ServiceSettings.CollapsedThreads") != "disabled";
    if crt && !order.is_empty() {
        let urgent = app.cfg.bool("ServiceSettings.PostPriority");
        let rows = sqlx::query(
            "SELECT t.ThreadTeamId AS teamid,
                    COUNT(*) FILTER (WHERE t.LastReplyAt > tm.LastViewed) AS threads,
                    COALESCE(SUM(tm.UnreadMentions), 0)::bigint AS mentions,
                    COALESCE(SUM(tm.UnreadMentions) FILTER (WHERE $3 AND EXISTS
                        (SELECT 1 FROM PostsPriority pp WHERE pp.PostId = tm.PostId AND pp.Priority = 'urgent')), 0)::bigint AS urgent
             FROM ThreadMemberships tm JOIN Threads t ON t.PostId = tm.PostId
             WHERE tm.UserId = $1 AND tm.Following AND t.ThreadTeamId = ANY($2) AND COALESCE(t.ThreadDeleteAt, 0) = 0
               AND (t.ThreadTeamId = '' OR EXISTS (SELECT 1 FROM ChannelMembers cm
                    WHERE cm.ChannelId = t.ChannelId AND cm.UserId = tm.UserId))
             GROUP BY t.ThreadTeamId",
        )
        .bind(&user_id)
        .bind(&order)
        .bind(urgent)
        .fetch_all(&app.db)
        .await?;
        for r in rows {
            let team: String = r.try_get("teamid")?;
            if let Some(tu) = map.get_mut(&team) {
                tu.thread_count = r.try_get("threads")?;
                tu.thread_mention_count = r.try_get("mentions")?;
                tu.thread_urgent_mention_count = r.try_get("urgent")?;
            }
        }
    }
    Ok(Json(order.into_iter().filter_map(|t| map.remove(&t)).collect()))
}

async fn members_for_user(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Vec<TeamMember>>> {
    let user_id = user_param(&s, &user_id)?;
    if !app.user_perm(&s, &user_id).await && !app.has_perm(&s, "read_other_users_teams").await {
        return Err(AppError::forbidden_perm("read_other_users_teams"));
    }
    if !app.can_see_user(&s, &user_id).await? {
        return Err(AppError::forbidden_perm(perm::VIEW_MEMBERS));
    }
    let mut members = perm::team_members(&app, "tm.userid = $1", &[&user_id]).await?;
    for m in &mut members {
        if !app.has_team_perm(&s, &m.team_id, "manage_team_roles").await {
            sanitize_member(m, &s.user_id);
        }
    }
    Ok(Json(members))
}

async fn get_team(St(app): St<State>, Auth(s): Auth, Path(team_id): Path<String>) -> ApiResult<Json<Team>> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    let mut team = (*app.team(&team_id).await?).clone();
    if (!team.allow_open_invite || team.r#type != "O") && !app.has_team_perm(&s, &team.id, perm::VIEW_TEAM).await {
        return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
    }
    app.sanitize_team(&s, &mut team).await;
    Ok(Json(team))
}

async fn team_by_name(St(app): St<State>, Auth(s): Auth, Path(name): Path<String>) -> ApiResult<Json<Team>> {
    let mut team = app.team_by_name(&name).await?;
    if (!team.allow_open_invite || team.r#type != "O") && !app.has_team_perm(&s, &team.id, perm::VIEW_TEAM).await {
        return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
    }
    app.sanitize_team(&s, &mut team).await;
    Ok(Json(team))
}

async fn team_stats(St(app): St<State>, Auth(s): Auth, Path(team_id): Path<String>) -> ApiResult<impl IntoResponse> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    // `GetTeamStats` with the session's `GetViewUsersRestrictions` (`applyTeamMemberViewRestrictionsFilterForStats`):
    // a guest counts only the members it can see.
    let restrict = app.restriction_cond(&s).await?.map(|c| format!(" AND {c}")).unwrap_or_default();
    let sql = format!(
        "SELECT COUNT(DISTINCT tm.UserId), COUNT(DISTINCT tm.UserId) FILTER (WHERE u.DeleteAt = 0)
         FROM TeamMembers tm JOIN Users u ON tm.UserId = u.Id WHERE tm.DeleteAt = 0 AND tm.TeamId = $1{restrict}"
    );
    let (total, active): (i64, i64) =
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&team_id).fetch_one(&app.db).await?;
    #[derive(Serialize)]
    struct TeamStats {
        team_id: String,
        total_member_count: i64,
        active_member_count: i64,
    }
    Ok(Json(TeamStats { team_id, total_member_count: total, active_member_count: active }))
}

/// `getTeamIcon`: `teams/{id}/teamIcon.png`, with the icon's update time as its ETag.
async fn team_icon(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    let team = app.team(&team_id).await?;
    if !app.has_team_perm(&s, &team_id, perm::VIEW_TEAM).await && (team.r#type != "O" || !team.allow_open_invite) {
        return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
    }
    let etag = team.last_team_icon_update.to_string();
    if let Some(r) = not_modified(&headers, &etag) {
        return Ok(r);
    }
    let data = app.files.read(&format!("teams/{team_id}/teamIcon.png")).await.map_err(|e| {
        AppError::not_found("api.team.get_team_icon.read_file.app_error", "Unable to read the team icon file.")
            .detail(e.to_string())
    })?;
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=86400, private"));
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    Ok(r)
}

/// `HandleEtag`: 304 when the client has this version.
pub fn not_modified(headers: &HeaderMap, etag: &str) -> Option<Response> {
    let inm = headers.get(header::IF_NONE_MATCH)?.to_str().ok()?;
    (inm == etag).then(|| {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::ETAG, HeaderValue::from_str(etag).unwrap());
        r
    })
}

/// The answer with its `ETag`, for the client to send back as `If-None-Match`.
pub fn with_etag(r: impl IntoResponse, etag: &str) -> Response {
    let mut r = r.into_response();
    if let Ok(v) = HeaderValue::from_str(etag) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r
}

// ---------- members ----------

async fn get_member(
    St(app): St<State>,
    Auth(s): Auth,
    Path((team_id, user_id)): Path<(String, String)>,
) -> ApiResult<Json<TeamMember>> {
    let user_id = user_param(&s, &user_id)?;
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    if !app.can_see_user(&s, &user_id).await? {
        return Err(AppError::forbidden_perm(perm::VIEW_MEMBERS));
    }
    let mut m = app.team_member(&team_id, &user_id).await?.ok_or_else(|| {
        AppError::not_found(
            "app.team.get_member.missing.app_error",
            "No team member found for that user ID and team ID.",
        )
    })?;
    if !app.has_team_perm(&s, &team_id, "manage_team_roles").await {
        sanitize_member(&mut m, &s.user_id);
    }
    Ok(Json(m))
}

async fn members_by_ids(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Vec<TeamMember>>> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    // `SortedArrayFromJSON`.
    let mut ids: Vec<String> = serde_json::from_slice(&body)
        .map_err(|e| AppError::bad_request("api.payload.parse.error", "").detail(e.to_string()))?;
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(AppError::invalid_param("user_ids"));
    }
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    // `GetTeamMembersByIds` with the session's `GetViewUsersRestrictions` (`applyTeamMemberViewRestrictionsFilter`).
    let sql = match app.restriction_cond(&s).await? {
        Some(r) => format!(
            "{} JOIN Users u ON tm.UserId = u.Id WHERE tm.teamid = $1 AND tm.userid = ANY($2) AND tm.deleteat = 0 AND {r}",
            perm::team_members_select()
        ),
        None => {
            format!("{} WHERE tm.teamid = $1 AND tm.userid = ANY($2) AND tm.deleteat = 0", perm::team_members_select())
        }
    };
    let rows: Vec<perm::WithScheme<TeamMember>> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&team_id).bind(&ids).fetch_all(&app.db).await?;
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

#[derive(Deserialize, Default)]
#[serde(default)]
struct MemberBody {
    team_id: String,
    user_id: String,
}

/// `addTeamMember`: oneself into an open team (`join_public_teams`), or someone else with `add_user_to_team`.
async fn add_member(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    Body(m): Body<MemberBody>,
) -> ApiResult<Response> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    if m.team_id != team_id {
        return Err(AppError::invalid_param("team_id"));
    }
    if !is_valid_id(&m.user_id) {
        return Err(AppError::invalid_param("user_id"));
    }
    let team = app.team(&team_id).await?;
    if m.user_id == s.user_id {
        let p = if team.allow_open_invite { perm::JOIN_PUBLIC_TEAMS } else { perm::JOIN_PRIVATE_TEAMS };
        app.require(&s, p).await?;
    } else {
        app.require_team(&s, &team_id, perm::ADD_USER_TO_TEAM).await?;
        if !app.has_team_perm(&s, &team_id, "invite_guest").await {
            let u = app
                .user(&m.user_id)
                .await?
                .ok_or_else(|| AppError::not_found("api.team.user.missing_account", "Unable to find the user."))?;
            if u.is_guest() {
                return Err(AppError::forbidden_perm("invite_guest"));
            }
        }
    }
    if team.group_constrained == Some(true) {
        return Err(AppError::bad_request("api.team.add_members.user_denied", "This team is managed by groups."));
    }
    let user = app.user(&m.user_id).await?.ok_or_else(crate::users::missing_account)?;
    let mut tm = app.join_user_to_team(&team, &user, "").await?;
    app.publish_added_to_team(&team_id, &m.user_id);
    if !app.has_team_perm(&s, &team_id, "manage_team_roles").await {
        sanitize_member(&mut tm, &s.user_id);
    }
    Ok((StatusCode::CREATED, Json(tm)).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GracefulQuery {
    graceful: String,
}

#[derive(Serialize)]
struct MemberWithError {
    user_id: String,
    member: Option<TeamMember>,
    error: Option<AppError>,
}

/// `addTeamMembers`: each with its own error when `graceful`.
async fn add_members(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    Query(q): Query<GracefulQuery>,
    Body(members): Body<Vec<MemberBody>>,
) -> ApiResult<Response> {
    let graceful = !q.graceful.is_empty();
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    if members.len() > 256 {
        return Err(AppError::invalid_param("too many members in batch"));
    }
    if members.is_empty() {
        return Err(AppError::invalid_param("no members in batch"));
    }
    let team = app.team(&team_id).await?;
    if team.group_constrained == Some(true) {
        return Err(AppError::bad_request("api.team.add_members.user_denied", "This team is managed by groups."));
    }
    app.require_team(&s, &team_id, perm::ADD_USER_TO_TEAM).await?;
    let guests_ok = app.has_team_perm(&s, &team_id, "invite_guest").await;
    for m in &members {
        if m.team_id != team_id {
            return Err(AppError::invalid_param(&format!("team_id for member with user_id={}", m.user_id)));
        }
        if !is_valid_id(&m.user_id) {
            return Err(AppError::invalid_param("user_id"));
        }
        if !guests_ok {
            let u = app
                .user(&m.user_id)
                .await?
                .ok_or_else(|| AppError::not_found("api.team.user.missing_account", "Unable to find the user."))?;
            if u.is_guest() {
                return Err(AppError::forbidden_perm("invite_guest"));
            }
        }
    }
    let manage = app.has_team_perm(&s, &team_id, "manage_team_roles").await;
    let mut out = vec![];
    for m in members {
        let r = match app.user(&m.user_id).await? {
            Some(u) => app.join_user_to_team(&team, &u, &s.user_id).await,
            None => Err(crate::users::missing_account()),
        };
        match r {
            Ok(mut tm) => {
                app.publish_added_to_team(&team_id, &m.user_id);
                if !manage {
                    sanitize_member(&mut tm, &s.user_id);
                }
                out.push(MemberWithError { user_id: m.user_id, member: Some(tm), error: None });
            }
            Err(e) if graceful => out.push(MemberWithError { user_id: m.user_id, member: None, error: Some(e) }),
            Err(e) => return Err(e),
        }
    }
    if graceful {
        return Ok((StatusCode::CREATED, Json(out)).into_response());
    }
    let members: Vec<TeamMember> = out.into_iter().filter_map(|m| m.member).collect();
    Ok((StatusCode::CREATED, Json(members)).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct InviteQuery {
    token: String,
    invite_id: String,
}

/// `addUserToTeamFromInvite` with an invite id (email tokens need email, which this server does not send).
async fn join_from_invite(St(app): St<State>, Auth(s): Auth, Query(q): Query<InviteQuery>) -> ApiResult<Response> {
    if !q.token.is_empty() {
        return Err(AppError::bad_request(
            "api.user.create_user.signup_link_invalid.app_error",
            "The signup link does not appear to be valid.",
        ));
    }
    if q.invite_id.is_empty() {
        return Err(AppError::bad_request(
            "api.team.add_user_to_team.missing_parameter.app_error",
            "Parameter required to add user to team.",
        ));
    }
    if s.prop("is_guest") == "true" {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.team.add_user_to_team_from_invite.guest.app_error",
            "Guests are restricted from joining this team via an invite link.",
        ));
    }
    let team = team_by_invite(&app, &q.invite_id).await?;
    if team.group_constrained == Some(true) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "app.team.invite_id.group_constrained.error",
            "Unable to join a group-constrained team by invite.",
        ));
    }
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    let tm = app.join_user_to_team(&team, &user, "").await?;
    Ok((StatusCode::CREATED, Json(tm)).into_response())
}

pub async fn team_by_invite(app: &App, invite_id: &str) -> ApiResult<Team> {
    let t: Option<Team> =
        if invite_id.is_empty() { None } else { db::one(&app.db, "inviteid = $1", invite_id).await? };
    t.ok_or_else(|| {
        AppError::not_found("app.team.get_by_invite_id.finding.app_error", "Unable to find the existing team.")
    })
}

/// `getInviteInfo`: no session needed.
async fn invite_info(St(app): St<State>, _: MaybeAuth, Path(invite_id): Path<String>) -> ApiResult<Response> {
    let team = team_by_invite(&app, &invite_id).await?;
    if team.r#type != "O" {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.team.get_invite_info.not_open_team",
            "Invite is invalid because this is not an open team.",
        ));
    }
    // Go's field order: display_name, description, name, id.
    let v = format!(
        r#"{{"display_name":{},"description":{},"name":{},"id":{}}}"#,
        json!(team.display_name),
        json!(team.description),
        json!(team.name),
        json!(team.id)
    );
    Ok(([(header::CONTENT_TYPE, "application/json")], v).into_response())
}

/// `MemberInvite`: a bare list of emails, or `{"emails", "channelIds", "message", "profiles"}`.
#[derive(Deserialize)]
#[serde(untagged)]
enum MemberInvite {
    List(Vec<String>),
    Full {
        #[serde(default)]
        emails: Vec<String>,
        #[serde(default, rename = "channelIds")]
        channel_ids: Option<Vec<String>>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        profiles: Option<Vec<serde_json::Value>>,
    },
}

#[derive(Serialize)]
pub struct EmailInviteWithError {
    pub email: String,
    pub error: Option<AppError>,
}

/// `UserIsFirstAdmin`: the oldest active system admin.
pub async fn is_first_admin(app: &App, u: &User) -> bool {
    if !u.is_system_admin() {
        return false;
    }
    let first: Option<String> = sqlx::query_scalar(
        "SELECT Id FROM Users WHERE DeleteAt = 0 AND Roles LIKE '%system_admin%' ORDER BY CreateAt ASC LIMIT 1",
    )
    .fetch_optional(&app.db)
    .await
    .ok()
    .flatten();
    first.as_deref() == Some(&u.id)
}

/// `ValidateUserPermissionsOnChannels`: the channels whose members the user may manage.
pub async fn channels_user_may_invite_to(app: &App, user_id: &str, ids: &[String]) -> Vec<String> {
    let mut out = vec![];
    for id in ids {
        let Ok(c) = app.channel_or_404(id).await else { continue };
        let need = match c.r#type.as_str() {
            crate::model::channel::PRIVATE => perm::MANAGE_PRIVATE_CHANNEL_MEMBERS,
            crate::model::channel::OPEN => perm::MANAGE_PUBLIC_CHANNEL_MEMBERS,
            _ => continue,
        };
        if app.user_channel_perm(user_id, id, need).await.0 {
            out.push(id.clone());
        }
    }
    out
}

fn rate_limited(user_id: &str, team_id: &str) -> AppError {
    AppError::new(StatusCode::PAYLOAD_TOO_LARGE, "app.email.rate_limit_exceeded.app_error", "")
        .detail(format!("user_id={user_id}, team_id={team_id}, error=rate limit exceeded"))
}

/// `InviteNewUsersToTeam`: every address allowed and not deactivated, then one email each (failures logged).
pub async fn invite_new_users_to_team(app: &App, emails: Vec<String>, team_id: &str, sender_id: &str) -> ApiResult<()> {
    if !app.cfg.bool("ServiceSettings.EnableEmailInvitations") {
        return Err(AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "api.team.invite_members.disabled.app_error",
            "Email invitations are disabled.",
        ));
    }
    if emails.is_empty() {
        return Err(AppError::bad_request("api.team.invite_members.no_one.app_error", ""));
    }
    let team = app.team(team_id).await?;
    let sender = app.user(sender_id).await?.ok_or_else(crate::users::missing_account)?;
    let restrict = app.cfg.str("TeamSettings.RestrictCreationToDomains").to_string();
    let allowed: Vec<&str> = if sender.is_guest() {
        vec![app.cfg.str("GuestAccountsSettings.RestrictCreationToDomains")]
    } else {
        vec![&team.allowed_domains, &restrict]
    };
    let bad: Vec<String> = emails.iter().filter(|e| !email_allowed(e, &allowed)).cloned().collect();
    if !bad.is_empty() {
        return Err(AppError::bad_request("api.team.invite_members.invalid_email.app_error", "")
            .with_param("Addresses", &bad.join(", ")));
    }
    let gone: Vec<String> = sqlx::query_scalar("SELECT Email FROM Users WHERE Email = ANY($1) AND DeleteAt <> 0")
        .bind(&emails)
        .fetch_all(&app.db)
        .await?;
    let gone: Vec<String> = emails.iter().filter(|e| gone.contains(e)).cloned().collect();
    if !gone.is_empty() {
        return Err(AppError::bad_request("api.team.invite_members.account_deactivated.app_error", "")
            .with_param("Addresses", &gone.join(", ")));
    }
    let d = crate::email::InviteData {
        team: &team,
        channels: vec![],
        sender_name: sender.display_name(app.cfg.str("TeamSettings.TeammateNameDisplay")),
        sender_id: sender.id.clone(),
        sender_image: None,
        invites: emails,
        profiles: Default::default(),
        message: String::new(),
        reminder_interval: None,
        error_when_not_sent: false,
        is_system_admin: sender.is_system_admin(),
        is_first_admin: is_first_admin(app, &sender).await,
    };
    if crate::email::send_invites(app, d).await == Err(crate::email::InviteError::RateLimited) {
        return Err(rate_limited(&sender.id, &team.id));
    }
    Ok(())
}

/// `inviteUsersToTeam`: `InviteNewUsersToTeam`, or with `graceful` `InviteNewUsersToTeamGracefully` and the
/// `resend_invitation_email` job.
async fn invite_by_email(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    Query(q): Query<GracefulQuery>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let graceful = !q.graceful.is_empty();
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    app.require_team(&s, &team_id, perm::INVITE_USER).await?;
    if !app.has_team_perm(&s, &team_id, perm::ADD_USER_TO_TEAM).await {
        return Err(AppError::forbidden_perm(perm::INVITE_USER));
    }
    let invalid_body = |e: String| {
        AppError::bad_request("api.team.invite_members_to_team_and_channels.invalid_body.app_error", "").detail(e)
    };
    let invite: MemberInvite = serde_json::from_slice(&body).map_err(|e| invalid_body(e.to_string()))?;
    let (emails, mut channel_ids, message, profiles) = match invite {
        MemberInvite::List(l) => (l, vec![], String::new(), vec![]),
        MemberInvite::Full { emails, channel_ids, message, profiles } => {
            (emails, channel_ids.unwrap_or_default(), message.unwrap_or_default(), profiles.unwrap_or_default())
        }
    };
    let emails: Vec<String> = emails.into_iter().map(|e| e.trim().to_lowercase()).collect();
    if emails.is_empty() {
        return Err(AppError::invalid_param("user_email"));
    }
    if !graceful && !profiles.is_empty() {
        return Err(AppError::bad_request("api.team.invite_members.profiles_graceful.app_error", ""));
    }
    if !channel_ids.is_empty() {
        channel_ids = channels_user_may_invite_to(&app, &s.user_id, &channel_ids).await;
    }
    if !graceful {
        invite_new_users_to_team(&app, emails, &team_id, &s.user_id).await?;
        return Ok(crate::users::ok().into_response());
    }
    // InviteNewUsersToTeamGracefully: profiles need an Enterprise license.
    if !profiles.is_empty() {
        if !app.cfg.bool("ServiceSettings.EnableEmailInvitations") {
            return Err(AppError::new(
                StatusCode::NOT_IMPLEMENTED,
                "api.team.invite_members.disabled.app_error",
                "Email invitations are disabled.",
            ));
        }
        return Err(AppError::bad_request("api.team.invite_members.profiles_license.app_error", ""));
    }
    let out =
        invite_new_users_gracefully(&app, emails.clone(), &channel_ids, message, &team_id, &s.user_id, "").await?;
    // The reminder job Go schedules (`resend_invitation_email`), for the job server to pick up.
    let mut data = serde_json::Map::new();
    data.insert("emailList".into(), serde_json::Value::String(serde_json::to_string(&emails).unwrap_or_default()));
    data.insert("teamID".into(), serde_json::Value::String(team_id.clone()));
    data.insert("senderID".into(), serde_json::Value::String(s.user_id.clone()));
    data.insert("scheduledAt".into(), serde_json::Value::String(crate::app::now_ms().to_string()));
    if !channel_ids.is_empty() {
        data.insert(
            "channelList".into(),
            serde_json::Value::String(serde_json::to_string(&channel_ids).unwrap_or_default()),
        );
    }
    sqlx::query(
        "INSERT INTO Jobs (Id, Type, Priority, CreateAt, StartAt, LastActivityAt, Status, Progress, Data)
         VALUES ($1, 'resend_invitation_email', 0, $2, 0, 0, 'pending', 0, $3::jsonb)",
    )
    .bind(crate::app::new_id())
    .bind(crate::app::now_ms())
    .bind(serde_json::Value::Object(data).to_string())
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.job.save.app_error", e.to_string()))?;
    let body = if out.is_empty() { "null".to_string() } else { serde_json::to_string(&out).unwrap_or_default() };
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

/// `InviteNewUsersToTeamGracefully` (the invite route with `graceful`, and the `resend_invitation_email` job with
/// a reminder interval): each address with its error, the valid ones invited by email.
pub async fn invite_new_users_gracefully(
    app: &App,
    emails: Vec<String>,
    channel_ids: &[String],
    message: String,
    team_id: &str,
    sender_id: &str,
    reminder_interval: &str,
) -> ApiResult<Vec<EmailInviteWithError>> {
    if !app.cfg.bool("ServiceSettings.EnableEmailInvitations") {
        return Err(AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "api.team.invite_members.disabled.app_error",
            "Email invitations are disabled.",
        ));
    }
    if emails.is_empty() {
        return Err(AppError::bad_request("api.team.invite_members.no_one.app_error", ""));
    }
    // prepareInviteNewUsersToTeam
    let team = app.team(team_id).await?;
    let sender = app.user(sender_id).await?.ok_or_else(crate::users::missing_account)?;
    let mut channels = vec![];
    for id in channel_ids {
        if let Ok(c) = app.channel_or_404(id).await {
            if c.team_id != team_id {
                return Err(AppError::bad_request("api.team.invite_guests.channel_in_invalid_team.app_error", ""));
            }
            channels.push((c.id.clone(), c.display_name.clone()));
        }
    }
    let restrict = app.cfg.str("TeamSettings.RestrictCreationToDomains").to_string();
    let allowed: Vec<&str> = if sender.is_guest() {
        vec![app.cfg.str("GuestAccountsSettings.RestrictCreationToDomains")]
    } else {
        vec![&team.allowed_domains, &restrict]
    };
    let deactivated = |email: String| async move {
        let d: Option<i64> = sqlx::query_scalar("SELECT DeleteAt FROM Users WHERE Email = $1")
            .bind(&email)
            .fetch_optional(&app.db)
            .await?;
        ApiResult::Ok(d.is_some_and(|d| d != 0))
    };
    let sender_name = sender.display_name(app.cfg.str("TeamSettings.TeammateNameDisplay"));
    let first_admin = is_first_admin(app, &sender).await;
    let mut out: Vec<EmailInviteWithError> = vec![];
    let mut good = vec![];
    for email in emails {
        let email = email.trim().to_lowercase();
        let error = if !email_allowed(&email, &allowed) {
            Some(
                AppError::bad_request("api.team.invite_members.invalid_email.app_error", "")
                    .with_param("Addresses", &email),
            )
        } else if deactivated(email.clone()).await? {
            Some(
                AppError::bad_request("api.team.invite_members.account_deactivated.app_error", "")
                    .with_param("Addresses", &email),
            )
        } else {
            good.push(email.clone());
            None
        };
        out.push(EmailInviteWithError { email: email.clone(), error });
    }
    if !good.is_empty() {
        let with_channels = !channels.is_empty();
        let image = if with_channels { Some(app.profile_image(&sender).await.0) } else { None };
        let d = crate::email::InviteData {
            team: &team,
            channels,
            sender_name,
            sender_id: sender.id.clone(),
            sender_image: image,
            invites: good,
            profiles: Default::default(),
            message,
            reminder_interval: (!reminder_interval.is_empty()).then(|| reminder_interval.to_string()),
            error_when_not_sent: true,
            is_system_admin: sender.is_system_admin(),
            is_first_admin: first_admin,
        };
        match crate::email::send_invites(app, d).await {
            Ok(failed) => {
                for (email, e) in failed {
                    let mut err = AppError::new(StatusCode::OK, "", "");
                    err.message = e;
                    err.id.clear();
                    err.status_code = 0;
                    out.push(EmailInviteWithError { email, error: Some(err) });
                }
            }
            Err(crate::email::InviteError::RateLimited) => return Err(rate_limited(&sender.id, &team.id)),
            Err(crate::email::InviteError::SendFailed) => {
                let defaults = app.cfg.str("EmailSettings.SMTPServer") == "localhost"
                    && app.cfg.str("EmailSettings.SMTPPort") == "10025";
                for i in out.iter_mut().filter(|i| i.error.is_none()) {
                    i.error = Some(if defaults {
                        AppError::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "api.team.invite_members.unable_to_send_email_with_defaults.app_error",
                            "",
                        )
                    } else {
                        AppError::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "api.team.invite_members.unable_to_send_email.app_error",
                            "Error while sending the email",
                        )
                    });
                }
            }
        }
    }
    // The errors inside the list are Go's bare `NewAppError`s: no request id.
    for e in out.iter_mut().filter_map(|i| i.error.as_mut()) {
        e.request_id.clear();
    }
    Ok(out)
}
