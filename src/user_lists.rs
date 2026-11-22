//! Lists of users: `GET /users` with its filters, search, autocomplete, counts, known users.

// Based on Mattermost server/channels/api4/user.go (getUsers, searchUsers, autocompleteUsers, ...), app/user.go
// and store/sqlstore/user_store.go (GetProfiles*, Search*, Count) and localcachelayer/user_layer.go
// (GetAllProfiles), Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::sync::Arc;

use crate::app::Query;
use axum::extract::State as St;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::{ApiResult, App, AppError, Auth, Body, BusyAuth, State, is_valid_id};
use crate::model::{Session, Show, User};
use crate::perm;
use crate::users::{Arg, query_users, sanitized};

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/users", get(get_users))
        .route("/api/v4/users/autocomplete", get(autocomplete))
        .route("/api/v4/users/search", post(search))
        .route("/api/v4/users/stats", get(stats))
        .route("/api/v4/users/stats/filtered", get(stats_filtered))
        .route("/api/v4/users/known", get(known))
}

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 1000;

#[derive(Deserialize, Default)]
#[serde(default)]
struct UsersQuery {
    in_team: String,
    not_in_team: String,
    in_channel: String,
    not_in_channel: String,
    without_team: String,
    inactive: String,
    active: String,
    role: String,
    roles: String,
    team_roles: String,
    channel_roles: String,
    sort: String,
    page: Option<i64>,
    per_page: Option<i64>,
}

/// `c.Params.Page` / `PerPage`: 60 by default, at most 200.
fn paging(page: Option<i64>, per_page: Option<i64>) -> (i64, i64) {
    let per = match per_page {
        Some(p) if p >= 0 => p.min(200),
        _ => 60,
    };
    (page.unwrap_or(0).max(0), per)
}

fn truthy(s: &str) -> bool {
    matches!(s, "1" | "t" | "T" | "true" | "TRUE" | "True")
}

/// Viewers without `view_members` see their teams' and channels' people only.
async fn restrict(app: &App, s: &Session, cond: &mut String) -> ApiResult<()> {
    if let Some(r) = app.restriction_cond(s).await? {
        cond.push_str(" AND ");
        cond.push_str(&r);
    }
    Ok(())
}

fn activity(cond: &mut String, active: bool, inactive: bool) {
    if inactive && !active {
        cond.push_str(" AND u.DeleteAt != 0");
    } else if active && !inactive {
        cond.push_str(" AND u.DeleteAt = 0");
    }
}

/// `applyRoleFilter`.
fn role_filter(cond: &mut String, args: &mut Vec<Arg>, role: &str) {
    if !role.is_empty() {
        args.push(Arg::S(format!("%{}%", like_escape(role, '\\'))));
        cond.push_str(&format!(" AND u.Roles LIKE LOWER(${})", args.len()));
    }
}

/// `model.CleanRoleNames` over a comma-separated list: blanks dropped; `None` when one is not a role name.
fn clean_role_names(names: &str) -> Option<Vec<String>> {
    let mut out = vec![];
    for n in names.split(',') {
        if n.trim().is_empty() {
            continue;
        }
        if n.len() > 64 || !n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return None;
        }
        out.push(n.to_string());
    }
    Some(out)
}

/// The system, team and channel roles a list is narrowed to (`Roles`, `TeamRoles`, `ChannelRoles`).
#[derive(Default, Deserialize)]
#[serde(default)]
struct MultiRoles {
    #[serde(rename = "roles", deserialize_with = "null_vec")]
    system: Vec<String>,
    #[serde(rename = "team_roles", deserialize_with = "null_vec")]
    team: Vec<String>,
    #[serde(rename = "channel_roles", deserialize_with = "null_vec")]
    channel: Vec<String>,
}

fn null_vec<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

impl MultiRoles {
    /// `applyMultiRoleFilters`: any of the roles. `system_user` matches users who are nothing more; the team and
    /// channel roles look at the scheme flags of the list's `tm` / `cm` join, system admins left out.
    fn apply(&self, cond: &mut String, args: &mut Vec<Arg>) {
        let mut or: Vec<String> = vec![];
        let not_admin = |args: &mut Vec<Arg>| {
            args.push(Arg::S("%system_admin%".into()));
            format!("u.Roles NOT ILIKE ${}", args.len())
        };
        if self.system.first().is_some_and(|r| !r.is_empty()) {
            for r in &self.system {
                match r.as_str() {
                    "system_user" => {
                        args.push(Arg::S(r.clone()));
                        or.push(format!("u.Roles = ${}", args.len()));
                    }
                    "system_guest"
                    | "system_admin"
                    | "system_user_manager"
                    | "system_read_only_admin"
                    | "system_manager"
                    | "system_custom_group_admin"
                    | "shared_channel_manager" => {
                        args.push(Arg::S(format!("%{r}%").to_lowercase()));
                        or.push(format!("u.Roles ILIKE ${}", args.len()));
                    }
                    _ => {}
                }
            }
        }
        for (roles, alias, admin, user, guest) in [
            (&self.channel, "cm", "channel_admin", "channel_user", "channel_guest"),
            (&self.team, "tm", "team_admin", "team_user", "team_guest"),
        ] {
            if !roles.first().is_some_and(|r| !r.is_empty()) {
                continue;
            }
            for r in roles {
                if r == admin {
                    let na = not_admin(args);
                    or.push(format!("({alias}.SchemeAdmin = true AND {na})"));
                } else if r == user {
                    let na = not_admin(args);
                    or.push(format!("({alias}.SchemeUser = true AND {alias}.SchemeAdmin = false AND {na})"));
                } else if r == guest {
                    or.push(format!("{alias}.SchemeGuest = true"));
                }
            }
        }
        if !or.is_empty() {
            cond.push_str(&format!(" AND ({})", or.join(" OR ")));
        }
    }
}

/// `sanitizeSearchTerm`: the escape character dropped, `%` and `_` escaped with it.
fn like_escape(term: &str, esc: char) -> String {
    let mut out = String::new();
    for c in term.chars().filter(|c| *c != esc) {
        if c == '%' || c == '_' {
            out.push(esc);
        }
        out.push(c);
    }
    out
}

/// `getUsers`.
/// `getUsers`: the session's activity recorded on success, as Go does (`UpdateLastActivityAtIfNeeded`).
async fn get_users(St(app): St<State>, Auth(s): Auth, q: Query<UsersQuery>) -> ApiResult<Json<Vec<User>>> {
    let r = get_users_inner(St(app.clone()), Auth(s.clone()), q).await;
    if r.is_ok() {
        app.touch_session(&s).await;
    }
    r
}

async fn get_users_inner(St(app): St<State>, Auth(s): Auth, Query(q): Query<UsersQuery>) -> ApiResult<Json<Vec<User>>> {
    if !q.not_in_channel.is_empty() && q.in_team.is_empty() {
        return Err(AppError::invalid_url_param("team_id"));
    }
    let sort = q.sort.as_str();
    let bad_sort = || Err(AppError::invalid_url_param("sort"));
    match sort {
        "" | "admin" | "status" | "last_activity_at" | "create_at" | "display_name" => {}
        _ => return bad_sort(),
    }
    if (sort == "last_activity_at" || sort == "create_at")
        && (q.in_team.is_empty()
            || !q.not_in_team.is_empty()
            || !q.in_channel.is_empty()
            || !q.not_in_channel.is_empty()
            || !q.without_team.is_empty())
    {
        return bad_sort();
    }
    if (sort == "status" || sort == "admin") && q.in_channel.is_empty() || sort == "display_name" {
        return bad_sort();
    }
    let (active, inactive) = (truthy(&q.active), truthy(&q.inactive));
    if active && inactive {
        return Err(AppError::invalid_url_param("inactive"));
    }
    // MM-47378: `role`, `roles`, `team_roles` (with in_team) and `channel_roles` (with in_channel) must be role
    // names that exist.
    let mut multi = MultiRoles::default();
    if !q.role.is_empty() || !q.roles.is_empty() || !q.channel_roles.is_empty() || !q.team_roles.is_empty() {
        let all: Vec<(String,)> = sqlx::query_as("SELECT Name FROM Roles").fetch_all(&app.db).await?;
        let known = |names: &[String]| names.iter().all(|n| all.iter().any(|(a,)| a == n));
        if !q.role.is_empty()
            && (clean_role_names(&q.role).is_none() || q.role.contains(',') || !known(std::slice::from_ref(&q.role)))
        {
            return Err(AppError::invalid_param("role"));
        }
        for (raw, name, scope, out) in [
            (&q.roles, "roles", "x", &mut multi.system),
            (&q.channel_roles, "channelRoles", q.in_channel.as_str(), &mut multi.channel),
            (&q.team_roles, "teamRoles", q.in_team.as_str(), &mut multi.team),
        ] {
            if raw.is_empty() || scope.is_empty() {
                continue;
            }
            match clean_role_names(raw) {
                Some(v) if known(&v) => *out = v,
                _ => return Err(AppError::invalid_param(name)),
            }
        }
    }
    if !q.in_team.is_empty() && !app.has_team_perm(&s, &q.in_team, perm::VIEW_TEAM).await {
        return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
    }
    let (page, per) = paging(q.page, q.per_page);
    let show = app.show(&s).await;
    let mut args: Vec<Arg> = vec![];
    let mut cond = "true".to_string();
    let joins;
    let mut order = "ORDER BY u.Username ASC".to_string();
    // `GetAllProfiles` for the webapp's call (page 0 of 100, no filters) goes through its cache.
    let plain = q.in_team.is_empty()
        && q.not_in_team.is_empty()
        && q.in_channel.is_empty()
        && q.not_in_channel.is_empty()
        && !truthy(&q.without_team)
        && q.role.is_empty()
        && multi.system.is_empty()
        && !active
        && !inactive;
    if plain && page == 0 && per == 100 && app.sees_everyone(&s).await {
        let users = match app.caches.all_profiles.get(&()) {
            Some(u) => u,
            None => {
                let u: Vec<Arc<User>> = query_users(&app, "", "true", "ORDER BY u.Username ASC LIMIT 100", vec![])
                    .await?
                    .into_iter()
                    .map(Arc::new)
                    .collect();
                let u = Arc::new(u);
                app.caches.all_profiles.put((), u.clone());
                u
            }
        };
        return Ok(Json(users.iter().map(|u| sanitized(u, show)).collect()));
    }
    if truthy(&q.without_team) {
        app.require(&s, perm::LIST_USERS_WITHOUT_TEAM).await?;
        joins = String::new();
        cond.push_str(" AND NOT EXISTS (SELECT 1 FROM TeamMembers t WHERE t.UserId = u.Id AND t.DeleteAt = 0)");
        role_filter(&mut cond, &mut args, &q.role);
        activity(&mut cond, active, inactive);
    } else if !q.not_in_channel.is_empty() {
        if !app.channel_perm(&s, &q.not_in_channel, perm::READ_CHANNEL).await?.0 {
            return Err(AppError::forbidden_perm(perm::READ_CHANNEL));
        }
        args.push(Arg::S(q.in_team.clone()));
        args.push(Arg::S(q.not_in_channel.clone()));
        joins = "JOIN TeamMembers tm ON (tm.UserId = u.Id AND tm.DeleteAt = 0 AND tm.TeamId = $1)
                 LEFT JOIN ChannelMembers cm ON (cm.UserId = u.Id AND cm.ChannelId = $2)"
            .into();
        cond.push_str(" AND cm.UserId IS NULL");
    } else if !q.not_in_team.is_empty() {
        if !app.has_team_perm(&s, &q.not_in_team, perm::VIEW_TEAM).await {
            return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
        }
        args.push(Arg::S(q.not_in_team.clone()));
        joins = "LEFT JOIN TeamMembers tm ON (tm.UserId = u.Id AND tm.DeleteAt = 0 AND tm.TeamId = $1)".into();
        cond.push_str(" AND tm.UserId IS NULL");
    } else if !q.in_team.is_empty() {
        args.push(Arg::S(q.in_team.clone()));
        joins = "JOIN TeamMembers tm ON (tm.UserId = u.Id AND tm.DeleteAt = 0)".into();
        cond.push_str(" AND tm.TeamId = $1");
        match sort {
            "last_activity_at" => {
                return recent_or_new(&app, &s, &q.in_team, true, page, per, show).await;
            }
            "create_at" => {
                return recent_or_new(&app, &s, &q.in_team, false, page, per, show).await;
            }
            _ => {}
        }
        role_filter(&mut cond, &mut args, &q.role);
        multi.apply(&mut cond, &mut args);
        activity(&mut cond, active, inactive);
    } else if !q.in_channel.is_empty() {
        if !app.channel_perm(&s, &q.in_channel, perm::READ_CHANNEL).await?.0 {
            return Err(AppError::forbidden_perm(perm::READ_CHANNEL));
        }
        args.push(Arg::S(q.in_channel.clone()));
        match sort {
            "status" => {
                joins = "JOIN ChannelMembers cm ON (cm.UserId = u.Id) LEFT JOIN Status s ON (s.UserId = u.Id)".into();
                order = "ORDER BY CASE s.Status WHEN 'online' THEN 1 WHEN 'away' THEN 2 WHEN 'dnd' THEN 3 ELSE 4 END, u.Username ASC".into();
            }
            "admin" => {
                joins = "JOIN ChannelMembers cm ON (cm.UserId = u.Id)".into();
                order = "ORDER BY cm.SchemeAdmin DESC, u.Username ASC".into();
            }
            _ => joins = "JOIN ChannelMembers cm ON (cm.UserId = u.Id)".into(),
        }
        cond.push_str(" AND cm.ChannelId = $1");
        activity(&mut cond, active, inactive);
        // Only GetProfilesInChannel (no sort) applies the role filters.
        if sort.is_empty() {
            multi.apply(&mut cond, &mut args);
        }
        // GetProfilesInChannel has no view restriction filter.
        let tail = format!("{order} LIMIT {per} OFFSET {}", page * per);
        let users = query_users(&app, &joins, &cond, &tail, args).await?;
        return Ok(Json(users.iter().map(|u| sanitized(u, show)).collect()));
    } else {
        joins = String::new();
        role_filter(&mut cond, &mut args, &q.role);
        multi.apply(&mut cond, &mut args);
        activity(&mut cond, active, inactive);
    }
    restrict(&app, &s, &mut cond).await?;
    let tail = format!("{order} LIMIT {per} OFFSET {}", page * per);
    let users = query_users(&app, &joins, &cond, &tail, args).await?;
    Ok(Json(dedup(users).iter().map(|u| sanitized(u, show)).collect()))
}

/// `GetRecentlyActiveUsersForTeam` / `GetNewUsersForTeam`.
#[allow(clippy::too_many_arguments)]
async fn recent_or_new(
    app: &App,
    s: &Session,
    team: &str,
    recent: bool,
    page: i64,
    per: i64,
    show: Show,
) -> ApiResult<Json<Vec<User>>> {
    let mut cond = "tm.TeamId = $1".to_string();
    restrict(app, s, &mut cond).await?;
    let (joins, order) = if recent {
        (
            "JOIN TeamMembers tm ON (tm.UserId = u.Id AND tm.DeleteAt = 0) JOIN Status s ON (s.UserId = u.Id)",
            "ORDER BY s.LastActivityAt DESC, u.Username ASC",
        )
    } else {
        ("JOIN TeamMembers tm ON (tm.UserId = u.Id AND tm.DeleteAt = 0)", "ORDER BY u.CreateAt DESC, u.Username ASC")
    };
    let tail = format!("{order} LIMIT {per} OFFSET {}", page * per);
    let mut users = query_users(app, joins, &cond, &tail, vec![Arg::S(team.into())]).await?;
    if recent {
        let ids: Vec<String> = users.iter().map(|u| u.id.clone()).collect();
        let st = app.statuses(&ids).await?;
        for u in &mut users {
            if let Some(x) = st.iter().find(|x| x.user_id == u.id) {
                u.last_activity_at = x.last_activity_at;
            }
        }
    }
    Ok(Json(users.iter().map(|u| sanitized(u, show)).collect()))
}

/// The restriction joins can repeat a user (Go selects DISTINCT).
fn dedup(mut users: Vec<User>) -> Vec<User> {
    let mut seen = std::collections::HashSet::new();
    users.retain(|u| seen.insert(u.id.clone()));
    users
}

/// Fields a search looks in (`UserSearchType*`).
fn search_fields(emails: bool, full_names: bool) -> &'static [&'static str] {
    match (emails, full_names) {
        (true, true) => &["u.Username", "u.FirstName", "u.LastName", "u.Nickname", "u.Email"],
        (true, false) => &["u.Username", "u.Nickname", "u.Email"],
        (false, true) => &["u.Username", "u.FirstName", "u.LastName", "u.Nickname"],
        (false, false) => &["u.Username", "u.Nickname"],
    }
}

#[derive(Clone, Copy)]
struct SearchOpts {
    emails: bool,
    full_names: bool,
    allow_inactive: bool,
    limit: i64,
}

/// `performSearch`: every word of the term in one of the fields (or the id).
#[allow(clippy::too_many_arguments)]
async fn perform_search(
    app: &App,
    s: &Session,
    joins: &str,
    mut cond: String,
    mut args: Vec<Arg>,
    term: &str,
    role: (&str, &MultiRoles),
    o: SearchOpts,
) -> ApiResult<Vec<User>> {
    let term = like_escape(term.trim(), '*');
    role_filter(&mut cond, &mut args, role.0);
    role.1.apply(&mut cond, &mut args);
    if !o.allow_inactive {
        cond.push_str(" AND u.DeleteAt = 0");
    }
    for word in term.split_whitespace() {
        let w = word.trim_start_matches('@');
        args.push(Arg::S(format!("%{w}%")));
        let like = args.len();
        args.push(Arg::S(w.to_string()));
        let id = args.len();
        let parts: Vec<String> = search_fields(o.emails, o.full_names)
            .iter()
            .map(|f| format!("lower({f}) LIKE lower(${like}) escape '*'"))
            .collect();
        cond.push_str(&format!(" AND ({} OR u.Id = ${id})", parts.join(" OR ")));
    }
    restrict(app, s, &mut cond).await?;
    let tail = format!("ORDER BY u.Username ASC LIMIT {}", o.limit);
    Ok(dedup(query_users(app, joins, &cond, &tail, args).await?))
}

async fn search_in_team(
    app: &App,
    s: &Session,
    team: &str,
    term: &str,
    role: (&str, &MultiRoles),
    o: SearchOpts,
) -> ApiResult<Vec<User>> {
    if team.is_empty() {
        return perform_search(app, s, "", "true".into(), vec![], term, role, o).await;
    }
    let joins = "JOIN TeamMembers tm ON (tm.UserId = u.Id AND tm.DeleteAt = 0 AND tm.TeamId = $1)";
    perform_search(app, s, joins, "true".into(), vec![Arg::S(team.into())], term, role, o).await
}

async fn search_in_channel(
    app: &App,
    s: &Session,
    channel: &str,
    term: &str,
    role: (&str, &MultiRoles),
    o: SearchOpts,
) -> ApiResult<Vec<User>> {
    let joins = "JOIN ChannelMembers cm ON (cm.UserId = u.Id AND cm.ChannelId = $1)";
    perform_search(app, s, joins, "true".into(), vec![Arg::S(channel.into())], term, role, o).await
}

async fn search_not_in_channel(
    app: &App,
    s: &Session,
    team: &str,
    channel: &str,
    term: &str,
    role: (&str, &MultiRoles),
    o: SearchOpts,
) -> ApiResult<Vec<User>> {
    let mut joins = "LEFT JOIN ChannelMembers cm ON (cm.UserId = u.Id AND cm.ChannelId = $1)".to_string();
    let mut args = vec![Arg::S(channel.into())];
    if !team.is_empty() {
        joins.push_str(" JOIN TeamMembers tm ON (tm.UserId = u.Id AND tm.DeleteAt = 0 AND tm.TeamId = $2)");
        args.push(Arg::S(team.into()));
    }
    perform_search(app, s, &joins, "cm.UserId IS NULL".into(), args, term, role, o).await
}

#[derive(Serialize)]
struct Autocomplete {
    users: Vec<User>,
    /// `omitempty`: missing when nobody is out of the channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    out_of_channel: Option<Vec<User>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AutocompleteQuery {
    in_channel: String,
    in_team: String,
    name: String,
    limit: String,
}

/// `autocompleteUsers`: names only (no emails), full names as the privacy settings allow.
async fn autocomplete(
    St(app): St<State>,
    Auth(s): Auth,
    Query(q): Query<AutocompleteQuery>,
) -> ApiResult<Json<Autocomplete>> {
    let limit = if q.limit.is_empty() { DEFAULT_LIMIT } else { q.limit.parse::<i64>().unwrap_or(0).min(MAX_LIMIT) };
    let show = app.show(&s).await;
    let o = SearchOpts {
        emails: false,
        full_names: show.admin || app.cfg.bool("PrivacySettings.ShowFullName"),
        allow_inactive: false,
        limit,
    };
    if !q.in_channel.is_empty() && !app.channel_perm(&s, &q.in_channel, perm::READ_CHANNEL).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL));
    }
    if !q.in_team.is_empty() && !app.has_team_perm(&s, &q.in_team, perm::VIEW_TEAM).await {
        return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
    }
    let none = MultiRoles::default();
    let sanitize = |v: Vec<User>| v.iter().map(|u| sanitized(u, show)).collect::<Vec<_>>();
    if !q.in_channel.is_empty() {
        if q.in_team.is_empty() {
            return Err(AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "api.user.autocomplete_users.missing_team_id.app_error",
                "Team id parameter is required to autocomplete by channel.",
            ));
        }
        let (a, b) = tokio::join!(
            search_in_channel(&app, &s, &q.in_channel, &q.name, ("", &none), o),
            search_not_in_channel(&app, &s, &q.in_team, &q.in_channel, &q.name, ("", &none), o)
        );
        let out = sanitize(b?);
        return Ok(Json(Autocomplete { users: sanitize(a?), out_of_channel: Some(out).filter(|v| !v.is_empty()) }));
    }
    let users = search_in_team(&app, &s, &q.in_team, &q.name, ("", &none), o).await?;
    Ok(Json(Autocomplete { users: sanitize(users), out_of_channel: None }))
}

/// `model.UserSearch`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct UserSearch {
    term: String,
    team_id: String,
    not_in_team_id: String,
    in_channel_id: String,
    not_in_channel_id: String,
    in_group_id: String,
    not_in_group_id: String,
    allow_inactive: bool,
    without_team: bool,
    limit: i64,
    role: String,
    #[serde(flatten)]
    multi: MultiRoles,
}

/// `searchUsers`.
async fn search(St(app): St<State>, BusyAuth(s): BusyAuth, Body(p): Body<UserSearch>) -> ApiResult<Json<Vec<User>>> {
    let limit = if p.limit == 0 { DEFAULT_LIMIT } else { p.limit };
    if p.term.is_empty() {
        return Err(AppError::invalid_param("term"));
    }
    if p.team_id.is_empty() && !p.not_in_channel_id.is_empty() {
        return Err(AppError::invalid_param("team_id"));
    }
    for c in [&p.in_channel_id, &p.not_in_channel_id] {
        if !c.is_empty() && !app.channel_perm(&s, c, perm::READ_CHANNEL).await?.0 {
            return Err(AppError::forbidden_perm(perm::READ_CHANNEL));
        }
    }
    for t in [&p.team_id, &p.not_in_team_id] {
        if !t.is_empty() && !app.has_team_perm(&s, t, perm::VIEW_TEAM).await {
            return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
        }
    }
    if limit <= 0 || limit > MAX_LIMIT {
        return Err(AppError::invalid_param("limit"));
    }
    if !p.in_group_id.is_empty() || !p.not_in_group_id.is_empty() {
        return Err(AppError::not_implemented("Groups"));
    }
    let show = app.show(&s).await;
    let o = SearchOpts { emails: show.email, full_names: show.fullname, allow_inactive: p.allow_inactive, limit };
    let users = if p.without_team {
        let cond = "NOT EXISTS (SELECT 1 FROM TeamMembers t WHERE t.UserId = u.Id AND t.DeleteAt = 0)".to_string();
        perform_search(&app, &s, "", cond, vec![], &p.term, (&p.role, &p.multi), o).await?
    } else if !p.in_channel_id.is_empty() {
        search_in_channel(&app, &s, &p.in_channel_id, &p.term, (&p.role, &p.multi), o).await?
    } else if !p.not_in_channel_id.is_empty() {
        search_not_in_channel(&app, &s, &p.team_id, &p.not_in_channel_id, &p.term, (&p.role, &p.multi), o).await?
    } else if !p.not_in_team_id.is_empty() {
        let joins = "LEFT JOIN TeamMembers tm ON (tm.UserId = u.Id AND tm.DeleteAt = 0 AND tm.TeamId = $1)";
        let args = vec![Arg::S(p.not_in_team_id.clone())];
        perform_search(&app, &s, joins, "tm.UserId IS NULL".into(), args, &p.term, (&p.role, &p.multi), o).await?
    } else {
        search_in_team(&app, &s, &p.team_id, &p.term, (&p.role, &p.multi), o).await?
    };
    Ok(Json(users.iter().map(|u| sanitized(u, show)).collect()))
}

#[derive(Serialize)]
struct UsersStats {
    total_users_count: i64,
}

/// `getTotalUsersStats`: active accounts, bots included, remote ones not.
async fn stats(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<UsersStats>> {
    let mut cond = "u.DeleteAt = 0 AND COALESCE(u.RemoteId, '') = ''".to_string();
    restrict(&app, &s, &mut cond).await?;
    let sql = format!("SELECT COUNT(*) FROM Users u WHERE {cond}");
    let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql)).fetch_one(&app.db).await?;
    Ok(Json(UsersStats { total_users_count: n }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FilteredQuery {
    in_team: String,
    in_channel: String,
    include_deleted: String,
    include_bots: String,
    include_remote_users: String,
    roles: String,
    team_roles: String,
    channel_roles: String,
}

/// `getFilteredUsersStats` (system console): the role lists checked for their form first (not against the
/// existing roles, unlike `getUsers`), `team_roles` with `in_team` only, `channel_roles` with `in_channel` only.
async fn stats_filtered(
    St(app): St<State>,
    Auth(s): Auth,
    Query(q): Query<FilteredQuery>,
) -> ApiResult<Json<UsersStats>> {
    let mut multi = MultiRoles::default();
    for (raw, name, scope, out) in [
        (&q.roles, "roles", "x", &mut multi.system),
        (&q.channel_roles, "channelRoles", q.in_channel.as_str(), &mut multi.channel),
        (&q.team_roles, "teamRoles", q.in_team.as_str(), &mut multi.team),
    ] {
        if raw.is_empty() || scope.is_empty() {
            continue;
        }
        *out = clean_role_names(raw).ok_or_else(|| AppError::invalid_param(name))?;
    }
    app.require(&s, perm::SYSCONSOLE_READ_USERMANAGEMENT_USERS).await?;
    let mut sql = "SELECT COUNT(*) FROM Users u".to_string();
    let mut cond = "true".to_string();
    let mut args: Vec<Arg> = vec![];
    if !truthy(&q.include_deleted) {
        cond.push_str(" AND u.DeleteAt = 0");
    }
    if !truthy(&q.include_remote_users) {
        cond.push_str(" AND COALESCE(u.RemoteId, '') = ''");
    }
    if !truthy(&q.include_bots) {
        sql.push_str(" LEFT JOIN Bots b ON u.Id = b.UserId");
        cond.push_str(" AND b.UserId IS NULL");
    }
    if !q.in_team.is_empty() {
        args.push(Arg::S(q.in_team.clone()));
        sql.push_str(" LEFT JOIN TeamMembers tm ON u.Id = tm.UserId");
        cond.push_str(&format!(" AND tm.TeamId = ${} AND tm.DeleteAt = 0", args.len()));
    } else if !q.in_channel.is_empty() {
        args.push(Arg::S(q.in_channel.clone()));
        sql.push_str(" LEFT JOIN ChannelMembers cm ON u.Id = cm.UserId");
        cond.push_str(&format!(" AND cm.ChannelId = ${}", args.len()));
    }
    multi.apply(&mut cond, &mut args);
    let sql = format!("{sql} WHERE {cond}");
    let mut query = sqlx::query_as::<_, (i64,)>(sqlx::AssertSqlSafe(sql));
    for a in args {
        query = match a {
            Arg::S(v) => query.bind(v),
            Arg::V(v) => query.bind(v),
            Arg::I(v) => query.bind(v),
        };
    }
    let (n,) = query.fetch_one(&app.db).await?;
    Ok(Json(UsersStats { total_users_count: n }))
}

/// `getKnownUsers`: everyone sharing a channel with the user.
async fn known(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Vec<String>>> {
    if !is_valid_id(&s.user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    let ids: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT ocm.UserId FROM ChannelMembers cm JOIN ChannelMembers ocm ON ocm.ChannelId = cm.ChannelId
         WHERE ocm.UserId <> $1 AND cm.UserId = $1",
    )
    .bind(&s.user_id)
    .fetch_all(&app.db)
    .await?;
    Ok(Json(ids.into_iter().map(|r| r.0).collect()))
}
