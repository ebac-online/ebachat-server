//! The channel routes outside EBAChat's use: the system console's (all channels, members set in bulk, move,
//! moderations, schemes), updating a channel whole, converting a group message, a member's unread counts, and
//! the licensed ones (bookmarks, groups, moderations), which answer as a server without a license does.

// Based on Mattermost server/channels/api4/channel.go, api4/channel_bookmark.go, api4/group.go,
// app/channel.go, app/group.go, store/sqlstore/channel_store.go and store/sqlstore/group_store.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashSet};

use crate::app::Query;
use axum::extract::{Path, State as St};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::app::{ApiResult, App, AppError, Auth, State, is_valid_id};
use crate::channels::{self as ch, WithTeam, sanitize_member, system_post};
use crate::channels_api::{paging, parse_bool, require_id, user_param};
use crate::db;
use crate::model::channel::{DIRECT, GROUP, OPEN, PRIVATE};
use crate::model::{Channel, ChannelMember, Session, Show, User};
use crate::perm;

const SYSCONSOLE_WRITE_USERMANAGEMENT_GROUPS: &str = "sysconsole_write_user_management_groups";
const SYSCONSOLE_READ_USERMANAGEMENT_CHANNELS: &str = "sysconsole_read_user_management_channels";
const SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY: &str = "sysconsole_read_compliance_data_retention_policy";

pub fn routes() -> Router<State> {
    let c = "/api/v4/channels";
    let b = "/api/v4/channels/{channel_id}/bookmarks";
    Router::new()
        .route(c, get(get_all_channels))
        .route(&format!("{c}/members/{{user_id}}/direct/read"), put(read_all_messages))
        .route(&format!("{c}/{{channel_id}}/access_control/attributes"), get(access_control_attributes))
        .route(b, get(bookmark_license).post(bookmark_license))
        .route(&format!("{b}/{{bookmark_id}}"), patch(bookmark_license).delete(bookmark_license))
        .route(&format!("{b}/{{bookmark_id}}/sort_order"), post(bookmark_license))
        .route(&format!("{c}/{{channel_id}}/convert_to_channel"), post(convert_to_channel))
        .route(&format!("{c}/{{channel_id}}/groups"), get(groups_license))
        .route(&format!("{c}/{{channel_id}}/member_counts_by_group"), get(member_counts_by_group))
        .route(&format!("{c}/{{channel_id}}/members/{{user_id}}/autotranslation"), put(member_autotranslation))
        .route(&format!("{c}/{{channel_id}}/members_minus_group_members"), get(members_minus_group_members))
        .route(&format!("{c}/{{channel_id}}/moderations"), get(get_moderations))
        .route(&format!("{c}/{{channel_id}}/moderations/patch"), put(patch_moderations))
        .route(&format!("{c}/{{channel_id}}/move"), post(move_channel))
        .route(&format!("{c}/{{channel_id}}/scheme"), put(update_scheme))
        .route("/api/v4/users/{user_id}/channels/{channel_id}/unread", get(channel_unread))
}

type Q = Query<std::collections::HashMap<String, String>>;

async fn has_any_perm(app: &App, s: &Session, perms: &[&str]) -> bool {
    for p in perms {
        if app.has_perm(s, p).await {
            return true;
        }
    }
    false
}

// ---------- licensed: the errors of a server without a license ----------

/// Every bookmark route checks the license first.
async fn bookmark_license(_: Auth) -> AppError {
    AppError::new(
        StatusCode::NOT_IMPLEMENTED,
        "api.channel.bookmark.channel_bookmark.license.error",
        "Your license does not support bookmarks.",
    )
}

/// `getGroupsByChannel` is wrapped in `requireLicense`.
async fn groups_license(_: Auth) -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "api.license_error", "API requires license.")
}

async fn member_counts_by_group(_: Auth) -> AppError {
    AppError::new(StatusCode::FORBIDDEN, "api.channel.channel_member_counts_by_group.license.error", "")
}

async fn get_moderations(_: Auth) -> AppError {
    AppError::new(StatusCode::FORBIDDEN, "api.channel.get_channel_moderations.license.error", "")
}

async fn patch_moderations(_: Auth) -> AppError {
    AppError::new(StatusCode::FORBIDDEN, "api.channel.patch_channel_moderations.license.error", "")
}

/// Auto-translation needs an enterprise service. `updateChannelMemberAutotranslation` checks it before its board
/// and space guards, so they are never reached.
async fn member_autotranslation(_: Auth) -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        "api.channel.update_channel_member_autotranslation.feature_not_available.app_error",
        "",
    )
}

/// `updateChannelScheme`: the body is checked, then the license.
async fn update_scheme(_: Auth, Path(channel_id): Path<String>, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    require_id(&channel_id, "channel_id")?;
    let props: Map<String, Value> = serde_json::from_slice(&body).unwrap_or_default();
    let scheme_id = props.get("scheme_id").and_then(Value::as_str).unwrap_or("");
    if !is_valid_id(scheme_id) {
        return Err(AppError::invalid_param("scheme_id"));
    }
    Err(AppError::new(StatusCode::FORBIDDEN, "api.channel.update_channel_scheme.license.error", ""))
}

/// `readAllMessages`: behind the `EnableShiftEscapeToMarkAllRead` feature flag, off in 12.1.
async fn read_all_messages(St(app): St<State>, Auth(_): Auth) -> ApiResult<Json<Value>> {
    if !app.cfg.feature("EnableShiftEscapeToMarkAllRead") {
        return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.mark_all_as_read.disabled.app_error", ""));
    }
    Err(AppError::not_implemented("Marking all messages read"))
}

/// `getChannelAccessControlAttributes`: `{}` with the indicators off; else the access control service, which
/// needs a license.
async fn access_control_attributes(
    St(app): St<State>,
    Auth(s): Auth,
    Path(channel_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_id(&channel_id, "channel_id")?;
    app.require_channel(&s, &channel_id, perm::READ_CHANNEL).await?;
    if app.cfg.get("AccessControlSettings.EnableChannelPolicyIndicators").as_bool() == Some(false) {
        return Ok(Json(json!({})));
    }
    Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "app.pap.get_channel_access_control_attributes.app_error", ""))
}

// ---------- all channels ----------

/// `getAllChannels`: the system console's list of public and private channels with their teams.
async fn get_all_channels(St(app): St<State>, Auth(s): Auth, q: Q) -> ApiResult<Response> {
    let perms = [
        SYSCONSOLE_WRITE_USERMANAGEMENT_GROUPS,
        SYSCONSOLE_READ_USERMANAGEMENT_CHANNELS,
        SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY,
    ];
    if !has_any_perm(&app, &s, &perms).await {
        return Err(AppError::forbidden_perm(&perms.join(",")));
    }
    let flag = |k: &str| parse_bool(q.get(k));
    let retention = app.has_perm(&s, SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY).await;
    if flag("exclude_policy_constrained") && !retention {
        return Err(AppError::forbidden_perm(SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY));
    }
    if flag("exclude_access_control_policy_enforced") && !app.has_perm(&s, perm::MANAGE_SYSTEM).await {
        return Err(AppError::forbidden_perm(perm::MANAGE_SYSTEM));
    }
    let (page, per_page) = paging(&q);
    let group = q.get("not_associated_to_group").cloned().unwrap_or_default();
    let exclude_names = if flag("exclude_default_channels") { default_channel_names(&app) } else { vec![] };

    // getAllChannelsQuery: binds $1 the group, $2 the excluded names.
    let mut cond = "c.Type IN ('P', 'O')".to_string();
    if !flag("include_deleted") {
        cond += " AND c.DeleteAt = 0";
    }
    if !group.is_empty() {
        cond += " AND c.Id NOT IN (SELECT ChannelId FROM GroupChannels WHERE GroupChannels.GroupId = $1 AND GroupChannels.DeleteAt = 0)";
    } else {
        cond += " AND $1::text IS NOT NULL";
    }
    if !exclude_names.is_empty() {
        cond += " AND NOT (c.Name = ANY($2))";
    } else {
        cond += " AND $2::text[] IS NOT NULL";
    }
    let exclude_policy = flag("exclude_policy_constrained");
    let policy_join = if exclude_policy || retention {
        " LEFT JOIN RetentionPoliciesChannels ON c.Id = RetentionPoliciesChannels.ChannelId"
    } else {
        ""
    };
    if exclude_policy {
        cond += " AND RetentionPoliciesChannels.ChannelId IS NULL";
    }
    let mut acp_join = "";
    if flag("exclude_access_control_policy_enforced") {
        cond += " AND c.Id NOT IN (SELECT ID From AccessControlPolicies WHERE Type = 'channel')";
    } else if flag("access_control_policy_enforced") {
        acp_join = " INNER JOIN AccessControlPolicies acp ON c.Id = acp.ID";
    }
    let sql = format!(
        "SELECT {}, Teams.DisplayName AS teamdisplayname, Teams.Name AS teamname, Teams.UpdateAt AS teamupdateat{}
         FROM Channels AS c JOIN Teams ON Teams.Id = c.TeamId{policy_join}{acp_join} WHERE {cond}
         ORDER BY c.DisplayName, Teams.DisplayName LIMIT {per_page} OFFSET {}",
        db::cols::<Channel>("c"),
        if retention { ", RetentionPoliciesChannels.PolicyId AS policyid" } else { "" },
        page * per_page
    );
    let rows: Vec<PgRow> =
        sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&group).bind(&exclude_names).fetch_all(&app.db).await?;
    let mut list = vec![];
    for row in &rows {
        let mut w: WithTeam<Channel> = sqlx::FromRow::from_row(row)?;
        if retention {
            w.inner.policy_id = row.try_get::<Option<String>, _>("policyid")?;
        }
        list.push(w);
    }
    // sanitizeAllChannelsResponse
    if !has_any_perm(
        &app,
        &s,
        &[SYSCONSOLE_READ_COMPLIANCE_DATA_RETENTION_POLICY, SYSCONSOLE_READ_USERMANAGEMENT_CHANNELS],
    )
    .await
    {
        for w in &mut list {
            let c = &w.inner;
            w.inner = Channel {
                id: c.id.clone(),
                team_id: c.team_id.clone(),
                r#type: c.r#type.clone(),
                display_name: c.display_name.clone(),
                ..Default::default()
            };
        }
    }
    if !flag("include_total_count") {
        return Ok(Json(list).into_response());
    }
    let sql = format!("SELECT count(c.Id) FROM Channels AS c{policy_join}{acp_join} WHERE {cond}");
    let total: i64 =
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(&group).bind(&exclude_names).fetch_one(&app.db).await?;
    Ok(Json(ChannelsWithCount { channels: list, total_count: total }).into_response())
}

/// `DefaultChannelNames`.
fn default_channel_names(app: &App) -> Vec<String> {
    let mut names = vec![ch::DEFAULT_CHANNEL.to_string()];
    let extra: Vec<String> = app
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

// ---------- update ----------

/// `updateChannel` (PUT): header and purpose as given, the name and display name when given.
pub async fn update_channel(
    St(app): St<State>,
    Auth(s): Auth,
    Path(channel_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Channel>> {
    require_id(&channel_id, "channel_id")?;
    let new: Channel = match serde_json::from_slice::<Option<Channel>>(&body) {
        Ok(Some(c)) => c,
        Ok(None) => return Err(AppError::invalid_param("channel")),
        Err(e) => return Err(AppError::invalid_param("channel").detail(e.to_string())),
    };
    if new.id != channel_id {
        return Err(AppError::invalid_param("channel_id"));
    }
    let mut old = (*app.get_channel(&new.id).await?).clone();
    let forbidden = || AppError::new(StatusCode::FORBIDDEN, "api.channel.patch_update_channel.forbidden.app_error", "");
    match old.r#type.as_str() {
        OPEN => app.require_channel(&s, &channel_id, perm::MANAGE_PUBLIC_CHANNEL_PROPERTIES).await?,
        PRIVATE => app.require_channel(&s, &channel_id, perm::MANAGE_PRIVATE_CHANNEL_PROPERTIES).await?,
        GROUP | DIRECT => {
            if app.channel_member(&new.id, &s.user_id).await?.is_none() {
                return Err(forbidden());
            }
            if (!new.name.is_empty() && new.name != old.name)
                || (!new.display_name.is_empty() && new.display_name != old.display_name)
                || new.purpose != old.purpose
            {
                return Err(AppError::bad_request(
                    "api.channel.update_channel.update_direct_or_group_messages_not_allowed.app_error",
                    "",
                ));
            }
        }
        _ => return Err(forbidden()),
    }
    if old.delete_at > 0 {
        return Err(AppError::bad_request("api.channel.update_channel.deleted.app_error", ""));
    }
    if !new.r#type.is_empty() && new.r#type != old.r#type {
        return Err(AppError::bad_request("api.channel.update_channel.typechange.app_error", ""));
    }
    if old.name == ch::DEFAULT_CHANNEL && !new.name.is_empty() && new.name != old.name {
        return Err(AppError::bad_request("api.channel.update_channel.tried.app_error", "")
            .with_param("Channel", ch::DEFAULT_CHANNEL));
    }
    old.header = new.header.clone();
    old.purpose = new.purpose.clone();
    let old_display = old.display_name.clone();
    if !new.display_name.is_empty() {
        old.display_name = new.display_name.clone();
    }
    if !new.name.is_empty() {
        old.name = new.name.clone();
    }
    if new.group_constrained.is_some() {
        old.group_constrained = new.group_constrained;
    }
    app.update_channel(&mut old).await?;
    // Go posts the change whenever the body's display name differs from the old one, also when the body has
    // none ("from: X to: "), though the name was kept; only a real change is posted here.
    if !new.display_name.is_empty() && old_display != new.display_name {
        app.post_display_name_change(&s.user_id, &old, &old_display).await;
    }
    crate::audit::log(&app, Some(&s), &format!("name={}", new.name)).await;
    Ok(Json(old))
}

impl App {
    /// `PostUpdateChannelDisplayNameMessage`.
    pub async fn post_display_name_change(&self, user_id: &str, c: &Channel, old: &str) {
        let Ok(Some(user)) = self.user(user_id).await else { return };
        let name = &user.username;
        self.post_system(system_post(
            &c.id,
            user_id,
            "system_displayname_change",
            format!("{name} updated the channel display name from: {old} to: {}", c.display_name),
            json!({"username": name, "old_displayname": old, "new_displayname": c.display_name}),
        ))
        .await;
    }
}

// ---------- members ----------

/// `getChannelMembers`: a page of members, unordered as Go's query is.
pub async fn get_members(
    St(app): St<State>,
    Auth(s): Auth,
    Path(channel_id): Path<String>,
    q: Q,
) -> ApiResult<Json<Vec<ChannelMember>>> {
    require_id(&channel_id, "channel_id")?;
    app.require_channel(&s, &channel_id, perm::READ_CHANNEL).await?;
    let (page, per_page) = paging(&q);
    let cond = format!("cm.channelid = $1 LIMIT {per_page} OFFSET {}", page * per_page);
    let mut members = perm::channel_members(&app, &cond, &[&channel_id]).await?;
    for m in &mut members {
        sanitize_member(m, &s.user_id);
    }
    Ok(Json(members))
}

#[derive(Deserialize)]
struct SetMembersRequest {
    members: Option<Vec<String>>,
    channel_admins: Option<Vec<String>>,
}

#[derive(Serialize, Default)]
struct SetMembersResponse {
    added: Vec<String>,
    removed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    promoted: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    demoted: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<SetMembersError>,
}

#[derive(Serialize)]
struct SetMembersError {
    user_id: String,
    id: String,
    error: String,
}

impl SetMembersError {
    fn new(user_id: &str, e: AppError) -> SetMembersError {
        let mut error = e.message;
        if !e.detailed_error.is_empty() {
            error = format!("{error}, {}", e.detailed_error);
        }
        SetMembersError { user_id: user_id.into(), id: e.id, error }
    }
}

/// `setChannelMembers` (PUT): the membership made the given list, in batches, each batch's result a line of
/// NDJSON. Go streams the lines as the batches finish; here they are sent together at the end.
pub async fn set_members(
    St(app): St<State>,
    Auth(s): Auth,
    Path(channel_id): Path<String>,
    q: Q,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    require_id(&channel_id, "channel_id")?;
    app.require(&s, perm::MANAGE_SYSTEM).await?;
    let ranged = |k: &str, d: i64, min: i64, max: i64| -> ApiResult<i64> {
        match q.get(k).filter(|v| !v.is_empty()) {
            None => Ok(d),
            Some(v) => {
                v.parse::<i64>().ok().filter(|n| (min..=max).contains(n)).ok_or_else(|| AppError::invalid_url_param(k))
            }
        }
    };
    let batch_size = ranged("batch_size", 100, 1, 1000)? as usize;
    let batch_delay = ranged("batch_delay_ms", 500, 0, 10000)? as u64;
    if body.len() > 12 * 1024 * 1024 {
        return Err(AppError::invalid_param("body"));
    }
    let req: SetMembersRequest =
        serde_json::from_slice(&body).map_err(|e| AppError::invalid_param("body").detail(e.to_string()))?;
    let Some(members) = req.members else { return Err(AppError::invalid_param("members")) };
    let mut seen = HashSet::new();
    let mut desired = vec![];
    for id in members {
        if !is_valid_id(&id) {
            return Err(AppError::invalid_param("members"));
        }
        if seen.insert(id.clone()) {
            desired.push(id);
        }
    }
    let mut admins: Option<HashSet<String>> = None;
    if let Some(list) = req.channel_admins {
        let mut set = HashSet::new();
        for id in list {
            if !is_valid_id(&id) {
                return Err(AppError::invalid_param("channel_admins"));
            }
            set.insert(id.clone());
            if seen.insert(id.clone()) {
                desired.push(id);
            }
        }
        admins = Some(set);
    }
    let channel = app.get_channel(&channel_id).await?;
    if channel.is_group_or_direct() {
        return Err(AppError::bad_request("api.channel.set_members.type.app_error", ""));
    }
    if channel.group_constrained == Some(true) {
        return Err(AppError::bad_request("api.channel.set_members.group_constrained.app_error", ""));
    }

    // SetChannelMembers
    let current: Vec<String> = sqlx::query_scalar("SELECT UserId FROM ChannelMembers WHERE ChannelId = $1")
        .bind(&channel_id)
        .fetch_all(&app.db)
        .await?;
    let current_set: HashSet<&String> = current.iter().collect();
    let desired_set: HashSet<&String> = desired.iter().collect();
    let to_add: Vec<String> =
        desired_set.iter().filter(|id| !current_set.contains(*id)).map(|s| s.to_string()).collect();
    let to_remove: Vec<String> = current.iter().filter(|id| !desired_set.contains(id)).cloned().collect();
    if channel.r#type == PRIVATE && desired.is_empty() {
        return Err(AppError::bad_request("app.channel.set_members.empty_private.app_error", ""));
    }
    let mut lines: Vec<SetMembersResponse> = vec![];
    let delay = std::time::Duration::from_millis(batch_delay);
    if to_add.is_empty() && to_remove.is_empty() && admins.is_none() {
        lines.push(SetMembersResponse::default());
    } else {
        let mut work = vec![(Change::Remove, to_remove), (Change::Add, to_add)];
        let mut admins = admins;
        let mut i = 0;
        while i < work.len() || admins.is_some() {
            if i == work.len() {
                // After the additions: promotions and demotions against the members now in the channel.
                let admins = admins.take().unwrap_or_default();
                let members = perm::channel_members(&app, "cm.channelid = $1 LIMIT 100000", &[&channel_id]).await?;
                let pick = |want: bool| -> Vec<String> {
                    members
                        .iter()
                        .filter(|m| admins.contains(&m.user_id) == want && m.scheme_admin != want)
                        .map(|m| m.user_id.clone())
                        .collect()
                };
                work.push((Change::Promote, pick(true)));
                work.push((Change::Demote, pick(false)));
                continue;
            }
            let (change, ids) = (work[i].0, std::mem::take(&mut work[i].1));
            i += 1;
            for (n, chunk) in ids.chunks(batch_size).enumerate() {
                let mut r = SetMembersResponse::default();
                for id in chunk {
                    match apply_change(&app, change, &channel, &s.user_id, id).await {
                        Ok(()) => match change {
                            Change::Remove => r.removed.push(id.clone()),
                            Change::Add => r.added.push(id.clone()),
                            Change::Promote => r.promoted.push(id.clone()),
                            Change::Demote => r.demoted.push(id.clone()),
                        },
                        Err(e) => r.errors.push(SetMembersError::new(id, e)),
                    }
                }
                lines.push(r);
                if (n + 1) * batch_size < ids.len() {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    let mut out = String::new();
    for l in &lines {
        out += &serde_json::to_string(l).unwrap();
        out.push('\n');
    }
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], out).into_response())
}

#[derive(Clone, Copy)]
enum Change {
    Remove,
    Add,
    Promote,
    Demote,
}

/// One user's change: `removeUserFromChannel` (no system message), `AddChannelMember` (with one), or
/// `UpdateChannelMemberSchemeRoles`.
async fn apply_change(app: &App, change: Change, channel: &Channel, actor: &str, user_id: &str) -> ApiResult<()> {
    match change {
        Change::Remove => {
            let user = app.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
            app.remove_user_from_channel_row(&user, actor, channel).await
        }
        Change::Add => app.add_channel_member(channel, user_id, actor, "").await.map(|_| ()),
        Change::Promote => app.update_member_scheme_roles(&channel.id, user_id, false, true, true).await.map(|_| ()),
        Change::Demote => app.update_member_scheme_roles(&channel.id, user_id, false, true, false).await.map(|_| ()),
    }
}

// ---------- group message to channel ----------

#[derive(Deserialize, Default)]
#[serde(default)]
struct ConvertRequest {
    channel_id: String,
    team_id: String,
    name: String,
    display_name: String,
}

/// `convertGroupMessageToChannel`: a group message becomes a private channel of a team its members share.
async fn convert_to_channel(
    St(app): St<State>,
    Auth(s): Auth,
    Path(channel_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Channel>> {
    require_id(&channel_id, "channel_id")?;
    app.reject_board_or_space_channel(&channel_id).await?;
    let r: ConvertRequest = match serde_json::from_slice::<Option<ConvertRequest>>(&body) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(AppError::invalid_param("body")),
        Err(e) => return Err(AppError::invalid_param("body").detail(e.to_string())),
    };
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    if user.is_guest() {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.channel.gm_to_channel_conversion.not_allowed_for_user.request_error",
            "",
        ));
    }
    app.require_team(&s, &r.team_id, perm::CREATE_PRIVATE_CHANNEL).await?;
    if r.channel_id != channel_id {
        return Err(AppError::invalid_param("channel_id"));
    }

    // ConvertGroupMessageToChannel
    let original = (*app.get_channel(&r.channel_id).await?).clone();
    if !app.common_teams(&s, &original.id).await?.iter().any(|t| t.id == r.team_id) {
        return Err(AppError::bad_request("app.channel.group_message_conversion.incorrect_team", ""));
    }
    if original.r#type != GROUP {
        return Err(AppError::not_found("app.channel.group_message_conversion.original_channel_not_gm", ""));
    }
    app.channel_member_or_404(&original.id, &s.user_id).await?;
    let mut c = original.clone();
    c.r#type = PRIVATE.into();
    c.name = r.name.clone();
    c.display_name = r.display_name.clone();
    ch::validate(&c)?;
    c.team_id = r.team_id.clone();
    app.update_channel(&mut c).await?;
    app.invalidate_channel_cache(&original);

    // GetUsersInChannelPage: up to the group size, by username.
    let users: Vec<(String, String)> = sqlx::query_as(
        "SELECT Users.Id, Users.Username FROM Users JOIN ChannelMembers cm ON cm.UserId = Users.Id
         WHERE cm.ChannelId = $1 ORDER BY Users.Username ASC LIMIT 8",
    )
    .bind(&original.id)
    .fetch_all(&app.db)
    .await?;

    // setSidebarCategoriesForConvertedGroupMessage: out of every sidebar, then each member's first category of
    // the team saved again (its event sent).
    sqlx::query("DELETE FROM SidebarChannels WHERE ChannelId = $1").bind(&original.id).execute(&app.db).await?;
    for (uid, _) in &users {
        if let Ok(ordered) = app.sidebar_for_team_for_user(uid, &r.team_id).await
            && let Some(first) = ordered.categories.first()
        {
            let _ = app.update_sidebar_categories(uid, &r.team_id, std::slice::from_ref(first)).await;
        }
    }

    // postMessageForConvertGroupMessageToChannel
    let names: Vec<&str> = users.iter().map(|(_, n)| n.as_str()).collect();
    let members = match names.len() {
        0 => String::new(),
        1 => names[0].to_string(),
        n => crate::i18n::tr(
            "humanize.list_join",
            &[("OtherItems", &names[..n - 1].join(", ")), ("LastItem", names[n - 1])],
        )
        .unwrap_or_default(),
    };
    let message = crate::i18n::tr(
        "api.channel.group_message.converted.to_private_channel",
        &[("ConvertedByUsername", &user.username), ("GMMembers", &members)],
    )
    .unwrap_or_default();
    let ids: Vec<&str> = users.iter().map(|(id, _)| id.as_str()).collect();
    app.post_system(system_post(
        &original.id,
        &s.user_id,
        "system_gm_to_channel",
        message,
        json!({"convertedByUserId": user.id, "gmMembersDuringConversionIDs": ids}),
    ))
    .await;

    app.update_member_scheme_roles(&original.id, &s.user_id, false, true, true).await?;
    Ok(Json(c))
}

// ---------- move ----------

/// `moveChannel`: a public or private channel to another team, its members all in that team (or, with
/// `force`, those who are not removed).
async fn move_channel(
    St(app): St<State>,
    Auth(s): Auth,
    Path(channel_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Channel>> {
    require_id(&channel_id, "channel_id")?;
    let channel = (*app.get_channel(&channel_id).await?).clone();
    let props: Map<String, Value> = serde_json::from_slice(&body).unwrap_or_default();
    let Some(team_id) = props.get("team_id").and_then(Value::as_str) else {
        return Err(AppError::invalid_param("team_id"));
    };
    let Some(force) = props.get("force").and_then(Value::as_bool) else {
        return Err(AppError::invalid_param("force"));
    };
    let team = app.team(team_id).await?;
    if channel.is_group_or_direct() {
        return Err(AppError::new(StatusCode::FORBIDDEN, "api.channel.move_channel.type.invalid", ""));
    }
    app.require(&s, perm::MANAGE_SYSTEM).await?;
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;

    // RemoveAllDeactivatedMembersFromChannel
    sqlx::query(
        "DELETE FROM ChannelMembers WHERE UserId IN (SELECT Id FROM Users WHERE Users.DeleteAt != 0)
         AND ChannelMembers.ChannelId = $1",
    )
    .bind(&channel.id)
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.channel.remove_all_deactivated_members.app_error", e.to_string()))?;
    app.invalidate_channel_members_cache(&channel.id);
    if force {
        app.remove_non_team_members(&user, &channel, &team.id).await?;
    }

    // MoveChannel
    if channel.is_space() {
        return Err(AppError::new(StatusCode::FORBIDDEN, "app.channel.move_channel.space.app_error", ""));
    }
    let non_members = app.non_team_members(&channel.id, &team.id).await?;
    if !non_members.is_empty() {
        return Err(AppError::internal("app.channel.move_channel.members_do_not_match.error", ""));
    }
    let previous = app.team(&channel.team_id).await?;
    sqlx::query("DELETE FROM SidebarChannels WHERE ChannelId = $1").bind(&channel.id).execute(&app.db).await?;
    let mut moved = channel.clone();
    moved.team_id = team.id.clone();
    app.store_update_channel(&mut moved).await?;
    // Go drops only the by-id entry; the by-name entry of the old team is dropped too.
    app.invalidate_channel_cache(&channel);
    let now = crate::app::now_ms();
    let hooks: Vec<String> = sqlx::query_scalar(
        "UPDATE IncomingWebhooks SET TeamId = $1, UpdateAt = $2 WHERE TeamId = $3 AND ChannelId = $4 AND DeleteAt = 0
         RETURNING Id",
    )
    .bind(&team.id)
    .bind(now)
    .bind(&previous.id)
    .bind(&channel.id)
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();
    for id in hooks {
        app.caches.webhook.remove(&id);
    }
    let _ = sqlx::query(
        "UPDATE OutgoingWebhooks SET TeamId = $1, UpdateAt = $2 WHERE TeamId = $3 AND ChannelId = $4 AND DeleteAt = 0",
    )
    .bind(&team.id)
    .bind(now)
    .bind(&previous.id)
    .bind(&channel.id)
    .execute(&app.db)
    .await;
    let _ = sqlx::query(
        "UPDATE Threads SET ThreadTeamId = $1 WHERE ChannelId = $2 AND EXISTS(SELECT 1 FROM Teams WHERE Id = $1)",
    )
    .bind(&team.id)
    .bind(&channel.id)
    .execute(&app.db)
    .await;
    let _ = app.remove_non_team_members(&user, &moved, &team.id).await;
    let message = crate::i18n::t("api.team.move_channel.success")
        .unwrap_or("This channel has been moved to this team from %v.")
        .replace("%v", &previous.name);
    app.post_system(system_post(
        &moved.id,
        &user.id,
        "system_move_channel",
        message,
        json!({"username": user.username}),
    ))
    .await;
    crate::audit::log(&app, Some(&s), &format!("channel={}", channel.name)).await;
    crate::audit::log(&app, Some(&s), &format!("team={}", team.name)).await;
    // The handler answers with the channel as it was read, before the move.
    Ok(Json(channel))
}

impl App {
    /// The channel's members who are not active members of the team.
    async fn non_team_members(&self, channel_id: &str, team_id: &str) -> ApiResult<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT cm.UserId FROM ChannelMembers cm WHERE cm.ChannelId = $1 AND NOT EXISTS
             (SELECT 1 FROM TeamMembers tm WHERE tm.TeamId = $2 AND tm.UserId = cm.UserId AND tm.DeleteAt = 0)",
        )
        .bind(channel_id)
        .bind(team_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// `RemoveUsersFromChannelNotMemberOfTeam`.
    async fn remove_non_team_members(&self, remover: &User, channel: &Channel, team_id: &str) -> ApiResult<()> {
        for id in self.non_team_members(&channel.id, team_id).await? {
            let user = self.user(&id).await?.ok_or_else(crate::users::missing_account)?;
            self.remove_user_from_channel_row(&user, &remover.id, channel).await?;
        }
        Ok(())
    }
}

// ---------- members minus group members ----------

/// `channelMembersMinusGroupMembers`: the channel's members who are in none of the groups.
async fn members_minus_group_members(
    St(app): St<State>,
    Auth(s): Auth,
    Path(channel_id): Path<String>,
    q: Q,
) -> ApiResult<Json<UsersWithGroupsAndCount>> {
    require_id(&channel_id, "channel_id")?;
    app.reject_space_channel(&channel_id).await?;
    let raw = q.get("group_ids").cloned().unwrap_or_default();
    // groupIDsQueryParamRegex removes what is not an id character or a comma.
    let kept: String = raw.chars().filter(|c| c.is_ascii_alphanumeric() || *c == ',').collect();
    if kept.len() < 26 {
        return Err(AppError::invalid_param("group_ids"));
    }
    let group_ids: Vec<String> = raw.split(',').map(str::to_string).collect();
    if !group_ids.iter().all(|g| is_valid_id(g)) {
        return Err(AppError::invalid_param("group_ids"));
    }
    app.require(&s, SYSCONSOLE_READ_USERMANAGEMENT_CHANNELS).await?;
    let (page, per_page) = paging(&q);
    let from = "FROM ChannelMembers JOIN Channels ON Channels.Id = ChannelMembers.ChannelId
         JOIN Users ON Users.Id = ChannelMembers.UserId
         LEFT JOIN Bots ON Bots.UserId = ChannelMembers.UserId
         LEFT JOIN GroupMembers ON GroupMembers.UserId = Users.Id
         LEFT JOIN UserGroups ON UserGroups.Id = GroupMembers.GroupId
         WHERE Channels.DeleteAt = 0 AND Users.DeleteAt = 0 AND Bots.UserId IS NULL AND Channels.Id = $1
         AND Users.Id NOT IN (SELECT GroupMembers.UserId FROM GroupMembers
             JOIN UserGroups ON UserGroups.Id = GroupMembers.GroupId
             WHERE GroupMembers.DeleteAt = 0 AND GroupMembers.GroupId = ANY($2))";
    let sql = format!(
        "SELECT {}, COALESCE(ChannelMembers.SchemeGuest, FALSE) AS mg_guest, ChannelMembers.SchemeAdmin AS mg_admin,
                ChannelMembers.SchemeUser AS mg_user, string_agg(UserGroups.Id, ',') AS mg_groups
         {from}
         GROUP BY Users.Id, ChannelMembers.SchemeGuest, ChannelMembers.SchemeAdmin, ChannelMembers.SchemeUser
         ORDER BY Users.Username ASC LIMIT {per_page} OFFSET {}",
        db::cols::<User>("Users"),
        page * per_page
    );
    let rows: Vec<PgRow> =
        sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&channel_id).bind(&group_ids).fetch_all(&app.db).await?;
    let show = Show {
        email: app.cfg.bool("PrivacySettings.ShowEmailAddress"),
        fullname: app.cfg.bool("PrivacySettings.ShowFullName"),
        admin: false,
    };
    let mut users = vec![];
    for row in &rows {
        let mut u: User = sqlx::FromRow::from_row(row)?;
        u.sanitize_profile(show);
        users.push(UserWithGroups {
            user: u,
            // Groups need a license; without one there are none to list.
            groups: vec![],
            scheme_guest: db::col(row, "mg_guest")?,
            scheme_user: db::col(row, "mg_user")?,
            scheme_admin: db::col(row, "mg_admin")?,
        });
    }
    let sql = format!("SELECT count(DISTINCT Users.Id) {from}");
    let total_count: i64 =
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(&channel_id).bind(&group_ids).fetch_one(&app.db).await?;
    Ok(Json(UsersWithGroupsAndCount { users, total_count }))
}

/// `model.UserWithGroups`, in Go's order.
#[derive(Serialize)]
struct UserWithGroups {
    #[serde(flatten)]
    user: User,
    groups: Vec<Value>,
    scheme_guest: bool,
    scheme_user: bool,
    scheme_admin: bool,
}

#[derive(Serialize)]
struct UsersWithGroupsAndCount {
    users: Vec<UserWithGroups>,
    total_count: i64,
}

/// `model.ChannelsWithCount`.
#[derive(Serialize)]
struct ChannelsWithCount {
    channels: Vec<WithTeam<Channel>>,
    total_count: i64,
}

// ---------- a member's unread counts ----------

#[derive(Serialize)]
struct ChannelUnread {
    team_id: String,
    channel_id: String,
    msg_count: i64,
    mention_count: i64,
    mention_count_root: i64,
    urgent_mention_count: i64,
    msg_count_root: i64,
}

/// `getChannelUnread`.
async fn channel_unread(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, channel_id)): Path<(String, String)>,
) -> ApiResult<Json<ChannelUnread>> {
    require_id(&channel_id, "channel_id")?;
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    app.require_channel(&s, &channel_id, perm::READ_CHANNEL).await?;
    let row: Option<PgRow> = sqlx::query(
        "SELECT Channels.TeamId, Channels.Id AS ChannelId, (Channels.TotalMsgCount - ChannelMembers.MsgCount) AS MsgCount,
                (Channels.TotalMsgCountRoot - ChannelMembers.MsgCountRoot) AS MsgCountRoot, ChannelMembers.MentionCount,
                ChannelMembers.MentionCountRoot, COALESCE(ChannelMembers.UrgentMentionCount, 0) AS UrgentMentionCount,
                ChannelMembers.NotifyProps
         FROM Channels, ChannelMembers
         WHERE Id = ChannelId AND Id = $1 AND UserId = $2 AND DeleteAt = 0 AND Channels.Type IN ('O', 'P', 'D', 'G')",
    )
    .bind(&channel_id)
    .bind(&user_id)
    .fetch_optional(&app.db)
    .await?;
    let Some(row) = row else {
        return Err(AppError::not_found("app.channel.get_unread.app_error", ""));
    };
    let mut u = ChannelUnread {
        team_id: db::col(&row, "teamid")?,
        channel_id: db::col(&row, "channelid")?,
        msg_count: db::col(&row, "msgcount")?,
        mention_count: db::col(&row, "mentioncount")?,
        mention_count_root: db::col(&row, "mentioncountroot")?,
        urgent_mention_count: db::col(&row, "urgentmentioncount")?,
        msg_count_root: db::col(&row, "msgcountroot")?,
    };
    let props: Option<sqlx::types::Json<BTreeMap<String, String>>> = row.try_get("notifyprops").ok().flatten();
    if props.is_some_and(|p| p.0.get("mark_unread").is_some_and(|v| v == "mention")) {
        u.msg_count = 0;
        u.msg_count_root = 0;
    }
    Ok(Json(u))
}
