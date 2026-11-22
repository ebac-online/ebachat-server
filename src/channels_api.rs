//! The REST handlers of channels and channel members (`api4/channel.go`); the logic is in `channels.rs`.

// Based on Mattermost server/channels/api4/channel.go and api4/user.go (getChannelMembersForUser),
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashMap};

use crate::app::Query;
use axum::extract::{Path, State as St};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::app::{ApiResult, App, AppError, Auth, Body, BusyAuth, State, is_valid_id};
use crate::channels::{self as ch, WithTeam, sanitize_member};
use crate::db::{self, Table};
use crate::model::channel::{DIRECT, GROUP, OPEN, OPEN_BOARD, PRIVATE};
use crate::model::{Channel, ChannelMember, Session, Team};
use crate::perm;

pub fn routes() -> Router<State> {
    let c = "/api/v4/channels";
    Router::new()
        .route(c, post(create_channel))
        .route(&format!("{c}/direct"), post(create_direct))
        .route(&format!("{c}/group"), post(create_group))
        .route(&format!("{c}/group/search"), post(search_group))
        .route(&format!("{c}/search"), post(search_all))
        .route(&format!("{c}/stats/member_count"), post(channels_member_count))
        .route(&format!("{c}/members/{{user_id}}/view"), post(view_channel))
        .route(&format!("{c}/members/{{user_id}}/mark_read"), post(read_multiple))
        .route(
            &format!("{c}/{{channel_id}}"),
            get(get_channel).delete(delete_channel).put(crate::channel_admin::update_channel),
        )
        .route(&format!("{c}/{{channel_id}}/patch"), put(patch_channel))
        .route(&format!("{c}/{{channel_id}}/privacy"), put(update_privacy))
        .route(&format!("{c}/{{channel_id}}/restore"), post(restore_channel))
        .route(&format!("{c}/{{channel_id}}/stats"), get(channel_stats))
        .route(&format!("{c}/{{channel_id}}/timezones"), get(channel_timezones))
        .route(&format!("{c}/{{channel_id}}/common_teams"), get(common_teams))
        .route(
            &format!("{c}/{{channel_id}}/members"),
            post(add_member).get(crate::channel_admin::get_members).put(crate::channel_admin::set_members),
        )
        .route(&format!("{c}/{{channel_id}}/members/ids"), post(members_by_ids))
        .route(&format!("{c}/{{channel_id}}/members/{{user_id}}"), get(get_member).delete(remove_member))
        .route(&format!("{c}/{{channel_id}}/members/{{user_id}}/notify_props"), put(update_notify_props))
        .route(&format!("{c}/{{channel_id}}/members/{{user_id}}/schemeRoles"), put(update_scheme_roles))
        .route("/api/v4/teams/{team_id}/channels", get(public_channels_for_team))
        .route("/api/v4/teams/{team_id}/channels/deleted", get(deleted_channels_for_team))
        .route("/api/v4/teams/{team_id}/channels/autocomplete", get(autocomplete_for_team))
        .route("/api/v4/teams/{team_id}/channels/name/{channel_name}", get(channel_by_name))
        .route("/api/v4/teams/name/{team_name}/channels/name/{channel_name}", get(channel_by_team_name))
        .route("/api/v4/users/{user_id}/teams/{team_id}/channels", get(channels_for_team_for_user))
        .route("/api/v4/users/{user_id}/teams/{team_id}/channels/members", get(members_for_team_for_user))
        .route("/api/v4/users/{user_id}/channels", get(channels_for_user))
        .route("/api/v4/users/{user_id}/channel_members", get(channel_members_for_user))
        .merge(crate::channel_admin::routes())
}

type Q = Query<HashMap<String, String>>;

/// `strconv.ParseBool`.
pub fn parse_bool(v: Option<&String>) -> bool {
    matches!(v.map(String::as_str), Some("1" | "t" | "T" | "TRUE" | "true" | "True"))
}

/// `page` and `per_page` as `web.Params` reads them.
pub fn paging(q: &HashMap<String, String>) -> (i64, i64) {
    let page = q.get("page").and_then(|v| v.parse::<i64>().ok()).filter(|v| *v >= 0).unwrap_or(0);
    let per_page = match q.get("per_page").and_then(|v| v.parse::<i64>().ok()) {
        Some(v) if v >= 0 => v.min(200),
        _ => 60,
    };
    (page, per_page)
}

pub(crate) fn require_id(id: &str, name: &str) -> ApiResult<()> {
    if is_valid_id(id) { Ok(()) } else { Err(AppError::invalid_url_param(name)) }
}

/// `RequireUserId`: `me` is the session's user.
pub(crate) fn user_param(s: &Session, id: &str) -> ApiResult<String> {
    let id = s.resolve(id);
    require_id(&id, "user_id")?;
    Ok(id)
}

pub(crate) fn status_ok() -> Json<Value> {
    Json(json!({"status": "OK"}))
}

pub(crate) fn is_guest(s: &Session) -> bool {
    s.prop("is_guest") == "true"
}

/// `SortedArrayFromJSON` / `NonSortedArrayFromJSON`: a JSON array of strings or a payload error.
fn string_array(v: Value, sorted: bool) -> ApiResult<Vec<String>> {
    let arr: Vec<String> = serde_json::from_value(v)
        .map_err(|_| AppError::bad_request("api.payload.parse.error", "Error parsing the payload."))?;
    let mut arr: Vec<String> = arr;
    if sorted {
        arr.sort();
        arr.dedup();
    }
    Ok(arr)
}

// ---------- create ----------

async fn create_channel(St(app): St<State>, Auth(s): Auth, Body(c): Body<Option<Channel>>) -> ApiResult<Response> {
    let Some(mut c) = c else { return Err(AppError::invalid_param("channel")) };
    if c.is_board() {
        return Err(AppError::invalid_param("type").detail("cannot create board channels via /channels endpoint"));
    }
    if c.is_space() {
        return Err(AppError::invalid_param("type").detail("cannot create space channels via /channels endpoint"));
    }
    if c.team_id.is_empty() {
        return Err(AppError::invalid_param("team_id").detail("Missing team_id"));
    }
    if c.display_name.is_empty() {
        return Err(AppError::invalid_param("display_name").detail("Missing display_name"));
    }
    if c.r#type == OPEN {
        app.require_team(&s, &c.team_id, perm::CREATE_PUBLIC_CHANNEL).await?;
    }
    if c.r#type == PRIVATE {
        app.require_team(&s, &c.team_id, perm::CREATE_PRIVATE_CHANNEL).await?;
    }
    if c.discoverable {
        return Err(AppError::bad_request(
            "api.channel.discoverable_join_request.feature_disabled.app_error",
            "Discoverable channels are not enabled on this server.",
        ));
    }
    if !c.id.is_empty() && c.shared != Some(true) {
        return Err(AppError::bad_request(
            "store.sql_channel.save_channel.existing.app_error",
            "Must call update for existing channel.",
        ));
    }
    c.props = None;
    let c = app.create_channel_with_user(c, &s.user_id).await?;
    crate::audit::log(&app, Some(&s), &format!("name={}", c.name)).await;
    Ok((StatusCode::CREATED, Json(c)).into_response())
}

async fn create_direct(St(app): St<State>, Auth(s): Auth, Body(v): Body<Value>) -> ApiResult<Response> {
    let mut ids = string_array(v, false)?;
    if ids.len() == 1 && ids[0] == s.user_id {
        ids.push(ids[0].clone());
    }
    if ids.len() != 2 {
        return Err(AppError::invalid_param("user_ids"));
    }
    let mut allowed = false;
    for id in &ids {
        if !is_valid_id(id) {
            return Err(AppError::invalid_param("user_id"));
        }
        if *id == s.user_id {
            allowed = true;
        }
    }
    app.require(&s, perm::CREATE_DIRECT_CHANNEL).await?;
    if !allowed {
        app.require(&s, perm::MANAGE_SYSTEM).await?;
    }
    let other = if ids[0] == s.user_id { &ids[1] } else { &ids[0] };
    if !app.user_can_see(&s, other).await? {
        return Err(AppError::forbidden_perm(perm::VIEW_MEMBERS));
    }
    let c = app.get_or_create_direct(&s, &ids[0], &ids[1]).await?;
    Ok((StatusCode::CREATED, Json(c)).into_response())
}

async fn create_group(St(app): St<State>, Auth(s): Auth, Body(v): Body<Value>) -> ApiResult<Response> {
    let mut ids = string_array(v, true)?;
    if ids.is_empty() {
        return Err(AppError::invalid_param("user_ids"));
    }
    if ids.iter().any(|id| !is_valid_id(id)) {
        return Err(AppError::invalid_param("user_id"));
    }
    if !ids.contains(&s.user_id) {
        ids.push(s.user_id.clone());
    }
    app.require(&s, perm::CREATE_GROUP_CHANNEL).await?;
    for id in &ids {
        if *id != s.user_id && !app.user_can_see(&s, id).await? {
            return Err(AppError::forbidden_perm(perm::VIEW_MEMBERS));
        }
    }
    let c = app.create_group_channel(&ids, &s.user_id).await?;
    Ok((StatusCode::CREATED, Json(c)).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChannelSearch {
    term: String,
    team_ids: Vec<String>,
    private: bool,
    public: bool,
    exclude_group_constrained: bool,
    include_deleted: bool,
    deleted: bool,
    exclude_default_channels: bool,
    page: Option<i64>,
    per_page: Option<i64>,
}

async fn search_group(
    St(app): St<State>,
    BusyAuth(s): BusyAuth,
    Body(b): Body<Option<ChannelSearch>>,
) -> ApiResult<Json<Vec<Channel>>> {
    let b = b.ok_or_else(|| AppError::invalid_param("channel_search"))?;
    Ok(Json(app.search_group_channels(&s.user_id, &b.term).await?))
}

/// `searchAllChannels`: as the clients ask it (`system_console=false`), autocomplete; else the System Console's
/// search.
async fn search_all(
    St(app): St<State>,
    BusyAuth(s): BusyAuth,
    q: Q,
    Body(b): Body<Option<ChannelSearch>>,
) -> ApiResult<Response> {
    let b = b.ok_or_else(|| AppError::invalid_param("channel_search"))?;
    let from_console = match q.get("system_console") {
        None => true,
        Some(v) => match v.as_str() {
            "1" | "t" | "T" | "TRUE" | "true" | "True" => true,
            "0" | "f" | "F" | "FALSE" | "false" | "False" => false,
            _ => return Err(AppError::invalid_param("system_console")),
        },
    };
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    if !from_console {
        if b.team_ids.len() == 1 && is_valid_id(&b.team_ids[0]) {
            app.require_team(&s, &b.team_ids[0], perm::VIEW_TEAM).await?;
            let list = app
                .autocomplete_in_team(&b.team_ids[0], &user, &b.term, b.private, b.exclude_group_constrained)
                .await?;
            return Ok(Json(list).into_response());
        }
        return Ok(Json(app.autocomplete_all(&user, &b.term).await?).into_response());
    }
    let mut ok = false;
    for p in [
        "sysconsole_write_user_management_groups",
        "sysconsole_read_user_management_channels",
        "sysconsole_read_compliance_data_retention_policy",
    ] {
        ok |= app.has_perm(&s, p).await;
    }
    if !ok {
        return Err(AppError::forbidden_perm("sysconsole_read_user_management_channels"));
    }
    let include_deleted = parse_bool(q.get("include_deleted")) || b.include_deleted;
    let (list, total) = search_all_channels(&app, &b, include_deleted).await?;
    if b.page.is_some() && b.per_page.is_some() {
        return Ok(Json(json!({"channels": list, "total_count": total})).into_response());
    }
    Ok(Json(list).into_response())
}

/// `SqlChannelStore.SearchAllChannels` for the filters the clients send.
async fn search_all_channels(
    app: &App,
    b: &ChannelSearch,
    include_deleted: bool,
) -> ApiResult<(Vec<WithTeam<Channel>>, i64)> {
    let mut cond = vec!["c.Type IN ('O', 'P')".to_string()];
    if !include_deleted {
        cond.push("c.DeleteAt = 0".into());
    }
    if b.deleted {
        cond.push("c.DeleteAt <> 0".into());
    }
    if b.public && !b.private {
        cond.push("c.Type = 'O'".into());
    } else if b.private && !b.public {
        cond.push("c.Type = 'P'".into());
    }
    if b.exclude_group_constrained {
        cond.push("(c.GroupConstrained IS NOT TRUE)".into());
    }
    if b.exclude_default_channels {
        cond.push(format!("c.Name <> '{}'", ch::DEFAULT_CHANNEL));
    }
    if !b.team_ids.is_empty() {
        cond.push("c.TeamId = ANY($1)".into());
    } else {
        cond.push("$1::text[] IS NOT NULL".into());
    }
    let like = ch::like_term(&b.term);
    if like.is_some() {
        cond.push("(LOWER(c.Name) LIKE LOWER($2) ESCAPE '*' OR LOWER(c.DisplayName) LIKE LOWER($2) ESCAPE '*' OR LOWER(c.Purpose) LIKE LOWER($2) ESCAPE '*')".into());
    } else {
        cond.push("$2::text IS NULL".into());
    }
    let where_ = cond.join(" AND ");
    let (page, per_page) = (b.page.unwrap_or(0), b.per_page.unwrap_or(10000));
    let sql = format!(
        "SELECT {}, COALESCE(t.DisplayName, '') AS teamdisplayname, COALESCE(t.Name, '') AS teamname,
                COALESCE(t.UpdateAt, 0) AS teamupdateat
         FROM Channels c JOIN Teams t ON t.Id = c.TeamId WHERE {where_}
         ORDER BY c.DisplayName, t.DisplayName LIMIT {per_page} OFFSET {}",
        db::cols::<Channel>("c"),
        page * per_page
    );
    let list: Vec<WithTeam<Channel>> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&b.team_ids).bind(&like).fetch_all(&app.db).await?;
    let count_sql = format!("SELECT count(*) FROM Channels c JOIN Teams t ON t.Id = c.TeamId WHERE {where_}");
    let total: i64 =
        sqlx::query_scalar(sqlx::AssertSqlSafe(count_sql)).bind(&b.team_ids).bind(&like).fetch_one(&app.db).await?;
    Ok((list, total))
}

async fn channels_member_count(
    St(app): St<State>,
    Auth(s): Auth,
    Body(v): Body<Value>,
) -> ApiResult<Json<BTreeMap<String, i64>>> {
    let ids = string_array(v, true)?;
    let mut found = vec![];
    for id in &ids {
        if let Some(c) = app.channel(id).await? {
            found.push(c);
        }
    }
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    for c in &found {
        // HasPermissionToChannelMemberCount
        let ok = app.user_has_channel_perm(&user, c, perm::READ_CHANNEL_CONTENT).await?
            || ((c.r#type == OPEN || c.r#type == OPEN_BOARD)
                && app.user_has_team_perm(&user, &c.team_id, perm::LIST_TEAM_CHANNELS).await?);
        if !ok {
            return Err(AppError::forbidden_perm(perm::LIST_TEAM_CHANNELS));
        }
    }
    let ids: Vec<String> = found.iter().map(|c| c.id.clone()).collect();
    Ok(Json(app.channels_member_count(&ids).await?))
}

// ---------- viewing ----------

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChannelView {
    channel_id: String,
    prev_channel_id: String,
    collapsed_threads_supported: bool,
}

/// `model.ChannelViewResponse`.
#[derive(serde::Serialize)]
struct ViewResponse {
    status: &'static str,
    last_viewed_at_times: BTreeMap<String, i64>,
}

/// `viewChannel`.
async fn view_channel(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Body(view): Body<ChannelView>,
) -> ApiResult<Json<ViewResponse>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    for (name, id) in
        [("channel_view.channel_id", &view.channel_id), ("channel_view.prev_channel_id", &view.prev_channel_id)]
    {
        if !id.is_empty() && !is_valid_id(id) {
            return Err(AppError::invalid_param(name));
        }
    }
    for id in [&view.channel_id, &view.prev_channel_id] {
        if !id.is_empty() {
            app.reject_board_channel(id).await?;
        }
    }
    // ViewChannel
    app.set_active_channel(&user_id, &view.channel_id).await;
    let ids: Vec<String> = [view.channel_id, view.prev_channel_id].into_iter().filter(|c| !c.is_empty()).collect();
    let times = if ids.is_empty() {
        BTreeMap::new()
    } else {
        let crt = app.is_crt_enabled(&user_id).await;
        app.mark_channels_viewed(&ids, &user_id, &s.id, view.collapsed_threads_supported, crt).await?
    };
    app.touch_session(&s).await;
    app.extend_session_if_needed(&s).await;
    Ok(Json(ViewResponse { status: "OK", last_viewed_at_times: times }))
}

/// `readMultipleChannels`.
async fn read_multiple(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Body(v): Body<Value>,
) -> ApiResult<Json<ViewResponse>> {
    let user_id = user_param(&s, &user_id)?;
    let ids = string_array(v, true)?;
    if ids.is_empty() {
        return Err(AppError::invalid_param("channel_ids"));
    }
    app.require_user(&s, &user_id).await?;
    let crt = app.is_crt_enabled(&user_id).await;
    let times = app.mark_channels_viewed(&ids, &user_id, &s.id, true, crt).await?;
    Ok(Json(ViewResponse { status: "OK", last_viewed_at_times: times }))
}

// ---------- one channel ----------

async fn get_channel(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Channel>> {
    require_id(&id, "channel_id")?;
    let c = app.get_channel(&id).await?;
    if c.r#type == OPEN {
        if !app.has_team_perm(&s, &c.team_id, perm::READ_PUBLIC_CHANNEL).await {
            app.require_channel(&s, &id, perm::READ_CHANNEL).await?;
        }
    } else {
        app.require_channel(&s, &id, perm::READ_CHANNEL).await?;
    }
    let mut list = [(*c).clone()];
    app.fill_channel_props(&mut list).await?;
    let [c] = list;
    Ok(Json(c))
}

async fn delete_channel(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>, q: Q) -> ApiResult<Json<Value>> {
    require_id(&id, "channel_id")?;
    let c = app.get_channel(&id).await?;
    if c.r#type == DIRECT || c.r#type == GROUP {
        return Err(AppError::bad_request(
            "api.channel.delete_channel.type.invalid",
            "Unable to delete direct or group message channels",
        ));
    }
    if c.r#type == OPEN {
        app.require_channel(&s, &id, perm::DELETE_PUBLIC_CHANNEL).await?;
    }
    if c.r#type == PRIVATE {
        app.require_channel(&s, &id, perm::DELETE_PRIVATE_CHANNEL).await?;
    }
    if parse_bool(q.get("permanent")) {
        if app.cfg.bool("ServiceSettings.EnableAPIChannelDeletion") {
            app.permanent_delete_channel(&c).await?;
            crate::audit::log(&app, Some(&s), &format!("name={}", c.name)).await;
            return Ok(status_ok());
        }
        let admin = app.user(&s.user_id).await?.is_some_and(|u| u.is_system_admin());
        let id = if admin {
            "api.user.delete_channel.not_enabled.for_admin.app_error"
        } else {
            "api.user.delete_channel.not_enabled.app_error"
        };
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            id,
            "Permanent channel deletion feature is not enabled. Please contact your System Administrator.",
        ));
    }
    app.delete_channel(&c, &s.user_id).await?;
    crate::audit::log(&app, Some(&s), &format!("name={}", c.name)).await;
    Ok(status_ok())
}

/// `patchChannel`.
async fn patch_channel(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    Body(patch): Body<Option<Map<String, Value>>>,
) -> ApiResult<Json<Channel>> {
    require_id(&id, "channel_id")?;
    let patch = patch.ok_or_else(|| AppError::invalid_param("channel"))?;
    let has = |k: &str| patch.get(k).is_some_and(|v| !v.is_null());
    let old = (*app.get_channel(&id).await?).clone();
    let props = ["display_name", "name", "header", "purpose", "group_constrained", "default_category_name"]
        .iter()
        .any(|k| has(k));
    let auto = has("autotranslation");
    let join_leave = has("disable_join_leave_messages");
    if !props && !auto && !has("banner_info") && !has("managed_category_name") && !has("discoverable") && !join_leave {
        return Err(AppError::bad_request(
            "api.channel.patch_update_channel.no_changes.app_error",
            "No changes in the patch.",
        ));
    }
    if has("discoverable") {
        return Err(AppError::bad_request(
            "api.channel.discoverable_join_request.feature_disabled.app_error",
            "Discoverable channels are not enabled on this server.",
        ));
    }
    if auto {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.channel.patch_update_channel.feature_not_available.app_error",
            "Feature not available.",
        ));
    }
    if has("group_constrained") {
        return Err(AppError::bad_request(
            "api.channel.patch_update_channel.group_constrained_not_allowed.app_error",
            "Group constrained channels need a license.",
        ));
    }
    if join_leave && old.is_group_or_direct() {
        return Err(AppError::bad_request(
            "api.channel.patch_update_channel.update_direct_or_group_messages_not_allowed.app_error",
            "You are not allowed to update the name, display_name, and purpose of direct or group messages.",
        ));
    }
    let s_str = |k: &str| patch.get(k).and_then(Value::as_str);
    match old.r#type.as_str() {
        OPEN if props || join_leave => app.require_channel(&s, &id, perm::MANAGE_PUBLIC_CHANNEL_PROPERTIES).await?,
        PRIVATE if props || join_leave => app.require_channel(&s, &id, perm::MANAGE_PRIVATE_CHANNEL_PROPERTIES).await?,
        OPEN | PRIVATE => {}
        GROUP | DIRECT => {
            if app.channel_member(&id, &s.user_id).await?.is_none() {
                return Err(AppError::new(
                    StatusCode::FORBIDDEN,
                    "api.channel.patch_update_channel.forbidden.app_error",
                    "Failed to update the channel.",
                ));
            }
            if s_str("name").is_some_and(|v| v != old.name)
                || s_str("display_name").is_some_and(|v| v != old.display_name)
                || s_str("purpose").is_some_and(|v| v != old.purpose)
                || has("default_category_name")
            {
                return Err(AppError::bad_request(
                    "api.channel.patch_update_channel.update_direct_or_group_messages_not_allowed.app_error",
                    "You are not allowed to update the name, display_name, and purpose of direct or group messages.",
                ));
            }
        }
        _ => {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "api.channel.patch_update_channel.forbidden.app_error",
                "Failed to update the channel.",
            ));
        }
    }
    if old.name == ch::DEFAULT_CHANNEL && s_str("name").is_some_and(|v| v != old.name) {
        return Err(AppError::bad_request(
            "api.channel.update_channel.tried.app_error",
            &format!("Tried to perform an invalid update of the default channel {}.", ch::DEFAULT_CHANNEL),
        ));
    }
    if has("banner_info") {
        // canEditChannelBanner: a licensed feature.
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.channel.update_channel.banner_info.license.error",
            "Channel banners need a license.",
        ));
    }
    let c = app.patch_channel(old, &patch, &s.user_id).await?;
    crate::audit::log(&app, Some(&s), "").await;
    let mut list = [c];
    app.fill_channel_props(&mut list).await?;
    let [c] = list;
    Ok(Json(c))
}

/// `updateChannelPrivacy`.
async fn update_privacy(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    Body(props): Body<Option<Map<String, Value>>>,
) -> ApiResult<Json<Channel>> {
    require_id(&id, "channel_id")?;
    let privacy = props.as_ref().and_then(|p| p.get("privacy")).and_then(Value::as_str).unwrap_or("").to_string();
    if privacy != OPEN && privacy != PRIVATE {
        return Err(AppError::invalid_param("privacy"));
    }
    let mut c = (*app.get_channel(&id).await?).clone();
    if privacy == OPEN {
        app.require_channel(&s, &id, ch::CONVERT_PRIVATE_CHANNEL_TO_PUBLIC).await?;
    } else {
        app.require_channel(&s, &id, perm::CONVERT_PUBLIC_CHANNEL_TO_PRIVATE).await?;
    }
    if c.name == ch::DEFAULT_CHANNEL && privacy == PRIVATE {
        return Err(AppError::bad_request(
            "api.channel.update_channel_privacy.default_channel_error",
            "The default channel cannot be made private.",
        ));
    }
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    c.r#type = privacy;
    let c = app.update_channel_privacy(c, &user).await?;
    crate::audit::log(&app, Some(&s), &format!("name={}", c.name)).await;
    Ok(Json(c))
}

/// `restoreChannel`.
async fn restore_channel(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Channel>> {
    require_id(&id, "channel_id")?;
    let c = (*app.get_channel(&id).await?).clone();
    if !app.has_team_perm(&s, &c.team_id, perm::MANAGE_TEAM).await
        && !app.has_perm(&s, ch::SYSCONSOLE_WRITE_USERMANAGEMENT_CHANNELS).await
    {
        return Err(AppError::forbidden_perm(perm::MANAGE_TEAM));
    }
    let c = app.restore_channel(c, &s.user_id).await?;
    crate::audit::log(&app, Some(&s), &format!("name={}", c.name)).await;
    Ok(Json(c))
}

/// `model.ChannelStats`.
#[derive(serde::Serialize)]
struct ChannelStats {
    channel_id: String,
    member_count: i64,
    guest_count: i64,
    pinnedpost_count: i64,
    files_count: i64,
}

/// `getChannelStats`.
async fn channel_stats(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    q: Q,
) -> ApiResult<Json<ChannelStats>> {
    require_id(&id, "channel_id")?;
    app.require_channel(&s, &id, perm::READ_CHANNEL).await?;
    let member_count = app.member_count(&id).await?;
    let guest_count = app.guest_count(&id).await?;
    let pinned = app.pinned_count(&id).await?;
    let files: i64 = if parse_bool(q.get("exclude_files_count")) {
        -1
    } else {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM FileInfo WHERE FileInfo.DeleteAt = 0 AND FileInfo.PostId != '' AND FileInfo.ChannelId = $1",
        )
        .bind(&id)
        .fetch_one(&app.db)
        .await?
    };
    Ok(Json(ChannelStats { channel_id: id, member_count, guest_count, pinnedpost_count: pinned, files_count: files }))
}

/// `getChannelMembersTimezones`: `null` when nobody has one, as Go's `ArrayToJSON(nil)`.
async fn channel_timezones(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<Option<Vec<String>>>> {
    require_id(&id, "channel_id")?;
    app.require_channel(&s, &id, perm::READ_CHANNEL).await?;
    Ok(Json(app.channel_timezones(&id).await?))
}

async fn common_teams(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Vec<Team>>> {
    require_id(&id, "channel_id")?;
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    if user.is_guest() {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.channel.gm_to_channel_conversion.not_allowed_for_user.request_error",
            "User is not allowed to convert group message to private channel",
        ));
    }
    app.require_channel(&s, &id, perm::READ_CHANNEL).await?;
    Ok(Json(app.common_teams(&s, &id).await?))
}

// ---------- members ----------

/// `addChannelMember`: one (`user_id`) or many (`user_ids`).
async fn add_member(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    Body(props): Body<Option<Map<String, Value>>>,
) -> ApiResult<Response> {
    require_id(&id, "channel_id")?;
    let props = props.unwrap_or_default();
    let mut ids: Vec<String> = vec![];
    if let Some(list) = props.get("user_ids").and_then(Value::as_array) {
        if list.len() > 1000 {
            return Err(AppError::invalid_param("user_ids"));
        }
        for v in list {
            match v.as_str() {
                Some(u) if is_valid_id(u) => ids.push(u.to_string()),
                _ => return Err(AppError::invalid_param("user_id in user_ids")),
            }
        }
    } else {
        match props.get("user_id").and_then(Value::as_str) {
            Some(u) if is_valid_id(u) => ids.push(u.to_string()),
            _ => return Err(AppError::invalid_param("user_id or user_ids")),
        }
    }
    let root_id = props.get("post_root_id").and_then(Value::as_str).unwrap_or("").to_string();
    if !root_id.is_empty() {
        if !is_valid_id(&root_id) {
            return Err(AppError::invalid_param("post_root_id"));
        }
        let root: Option<String> = sqlx::query_scalar("SELECT ChannelId FROM Posts WHERE Id = $1 AND DeleteAt = 0")
            .bind(&root_id)
            .fetch_optional(&app.db)
            .await?;
        match root {
            None => return Err(AppError::not_found("app.post.get.app_error", "Unable to get the post.")),
            Some(c) if c != id => return Err(AppError::invalid_param("post_root_id")),
            _ => {}
        }
    }
    let c = app.get_channel(&id).await?;
    if is_guest(&s) {
        app.require_channel(&s, &id, perm::READ_CHANNEL).await?;
        for u in &ids {
            if !app.user_can_see(&s, u).await? {
                return Err(AppError::forbidden_perm(perm::INVITE_USER));
            }
        }
    }
    if c.is_group_or_direct() {
        return Err(AppError::bad_request(
            "api.channel.add_user_to_channel.type.app_error",
            "Cannot add user to this channel type.",
        ));
    }
    let (mut can_add_self, mut can_add_others) = (false, false);
    if c.r#type == OPEN {
        can_add_self = app.has_team_perm(&s, &c.team_id, perm::JOIN_PUBLIC_CHANNELS).await;
        can_add_others = app.channel_perm(&s, &c.id, perm::MANAGE_PUBLIC_CHANNEL_MEMBERS).await?.0;
    }
    if c.r#type == PRIVATE {
        app.require_channel(&s, &c.id, perm::MANAGE_PRIVATE_CHANNEL_MEMBERS).await?;
    }
    let mut last_error = None;
    let mut added: Vec<ChannelMember> = vec![];
    for u in &ids {
        let existing = app.channel_member(&c.id, u).await?;
        if c.r#type == OPEN {
            let self_add = *u == s.user_id;
            if self_add && let Some(m) = &existing {
                added.push(m.clone());
                continue;
            } else if self_add && !can_add_self {
                last_error = Some(AppError::forbidden_perm(perm::JOIN_PUBLIC_CHANNELS));
                continue;
            } else if !self_add && !can_add_others {
                last_error = Some(AppError::forbidden_perm(perm::MANAGE_PUBLIC_CHANNEL_MEMBERS));
                continue;
            }
        }
        if let Some(m) = existing {
            added.push(m);
            continue;
        }
        match app.add_channel_member(&c, u, &s.user_id, &root_id).await {
            Ok(m) => {
                crate::audit::log(&app, Some(&s), &format!("name={} user_id={}", c.name, m.user_id)).await;
                added.push(m)
            }
            Err(e) => last_error = Some(e),
        }
    }
    if let Some(e) = last_error
        && added.is_empty()
    {
        return Err(e);
    }
    for m in &mut added {
        sanitize_member(m, &s.user_id);
    }
    let single = props.get("user_id").and_then(Value::as_str);
    if single.is_some() && added.len() == 1 && Some(added[0].user_id.as_str()) == single {
        return Ok((StatusCode::CREATED, Json(added.remove(0))).into_response());
    }
    Ok((StatusCode::CREATED, Json(added)).into_response())
}

async fn members_by_ids(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    Body(v): Body<Value>,
) -> ApiResult<Json<Vec<ChannelMember>>> {
    require_id(&id, "channel_id")?;
    let ids = string_array(v, true)?;
    if ids.is_empty() {
        return Err(AppError::invalid_param("user_ids"));
    }
    app.require_channel(&s, &id, perm::READ_CHANNEL).await?;
    let mut members = app.channel_members_where("cm.channelid = $1 AND cm.userid = ANY($2)", &id, &ids).await?;
    for m in &mut members {
        sanitize_member(m, &s.user_id);
    }
    Ok(Json(members))
}

async fn get_member(
    St(app): St<State>,
    Auth(s): Auth,
    Path((id, user_id)): Path<(String, String)>,
) -> ApiResult<Json<ChannelMember>> {
    require_id(&id, "channel_id")?;
    let user_id = user_param(&s, &user_id)?;
    app.require_channel(&s, &id, perm::READ_CHANNEL).await?;
    let mut m = app.channel_member_or_404(&id, &user_id).await?;
    sanitize_member(&mut m, &s.user_id);
    Ok(Json(m))
}

/// `removeChannelMember`: leaving, or removing someone with the right.
async fn remove_member(
    St(app): St<State>,
    Auth(s): Auth,
    Path((id, user_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    require_id(&id, "channel_id")?;
    let user_id = user_param(&s, &user_id)?;
    let c = app.get_channel(&id).await?;
    app.user(&user_id).await?.ok_or_else(crate::users::missing_account)?;
    if !(c.r#type == OPEN || c.r#type == PRIVATE) {
        return Err(AppError::bad_request(
            "api.channel.remove_channel_member.type.app_error",
            "Unable to remove user from a channel.",
        ));
    }
    if user_id != s.user_id {
        if c.r#type == OPEN {
            app.require_channel(&s, &c.id, perm::MANAGE_PUBLIC_CHANNEL_MEMBERS).await?;
        }
        if c.r#type == PRIVATE {
            app.require_channel(&s, &c.id, perm::MANAGE_PRIVATE_CHANNEL_MEMBERS).await?;
        }
    }
    app.remove_user_from_channel(&user_id, &s.user_id, &c).await?;
    crate::audit::log(&app, Some(&s), &format!("name={} user_id={user_id}", c.name)).await;
    Ok(status_ok())
}

async fn update_notify_props(
    St(app): St<State>,
    Auth(s): Auth,
    Path((id, user_id)): Path<(String, String)>,
    Body(props): Body<Option<Map<String, Value>>>,
) -> ApiResult<Json<Value>> {
    require_id(&id, "channel_id")?;
    let user_id = user_param(&s, &user_id)?;
    app.reject_board_or_space_channel(&id).await?;
    let props = props.ok_or_else(|| AppError::invalid_param("notify_props"))?;
    app.require_user(&s, &user_id).await?;
    app.update_member_notify_props(&props, &id, &user_id).await?;
    Ok(status_ok())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SchemeRoles {
    scheme_guest: bool,
    scheme_user: bool,
    scheme_admin: bool,
}

async fn update_scheme_roles(
    St(app): St<State>,
    Auth(s): Auth,
    Path((id, user_id)): Path<(String, String)>,
    Body(r): Body<SchemeRoles>,
) -> ApiResult<Json<Value>> {
    require_id(&id, "channel_id")?;
    let user_id = user_param(&s, &user_id)?;
    app.reject_board_or_space_channel(&id).await?;
    app.require_channel(&s, &id, perm::MANAGE_CHANNEL_ROLES).await?;
    app.update_member_scheme_roles(&id, &user_id, r.scheme_guest, r.scheme_user, r.scheme_admin).await?;
    Ok(status_ok())
}

// ---------- lists ----------

async fn public_channels_for_team(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    q: Q,
) -> ApiResult<Json<Vec<Channel>>> {
    require_id(&team_id, "team_id")?;
    app.require_team(&s, &team_id, perm::LIST_TEAM_CHANNELS).await?;
    let (page, per_page) = paging(&q);
    let sql = format!(
        "SELECT {} FROM Channels JOIN PublicChannels pc ON (pc.Id = Channels.Id)
         WHERE pc.TeamId = $1 AND pc.DeleteAt = 0 ORDER BY pc.DisplayName LIMIT $2 OFFSET $3",
        db::cols::<Channel>("Channels")
    );
    let mut list: Vec<Channel> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&team_id)
        .bind(per_page)
        .bind(page * per_page)
        .fetch_all(&app.db)
        .await?;
    app.fill_channel_props(&mut list).await?;
    Ok(Json(list))
}

async fn deleted_channels_for_team(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    q: Q,
) -> ApiResult<Json<Vec<Channel>>> {
    require_id(&team_id, "team_id")?;
    app.require_team(&s, &team_id, perm::LIST_TEAM_CHANNELS).await?;
    let skip_membership = app.has_perm(&s, perm::MANAGE_SYSTEM).await;
    let (page, per_page) = paging(&q);
    let mut sql = format!(
        "{} WHERE (TeamId = $1 OR TeamId = '') AND DeleteAt <> 0 AND Type IN ('O', 'P', 'D', 'G')",
        Channel::SELECT
    );
    if !skip_membership {
        sql += " AND (Type = 'O' OR (Type = 'P' AND Id IN (SELECT ChannelId FROM ChannelMembers WHERE UserId = $4)))";
    } else {
        sql += " AND $4::text IS NOT NULL";
    }
    sql += " ORDER BY DisplayName LIMIT $2 OFFSET $3";
    let mut list: Vec<Channel> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&team_id)
        .bind(per_page)
        .bind(page * per_page)
        .bind(&s.user_id)
        .fetch_all(&app.db)
        .await?;
    app.fill_channel_props(&mut list).await?;
    Ok(Json(list))
}

async fn autocomplete_for_team(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    q: Q,
) -> ApiResult<Json<Vec<Channel>>> {
    require_id(&team_id, "team_id")?;
    app.require_team(&s, &team_id, perm::LIST_TEAM_CHANNELS).await?;
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    let name = q.get("name").map(String::as_str).unwrap_or("");
    Ok(Json(app.autocomplete_in_team(&team_id, &user, name, false, false).await?))
}

/// The read checks of `getChannelByName` / `getChannelByNameForTeamName`.
async fn check_read_by_name(app: &App, s: &Session, c: &Channel) -> ApiResult<()> {
    if c.r#type == OPEN {
        if !app.has_team_perm(s, &c.team_id, perm::READ_PUBLIC_CHANNEL).await
            && !app.channel_perm(s, &c.id, perm::READ_CHANNEL).await?.0
        {
            return Err(AppError::forbidden_perm(perm::READ_PUBLIC_CHANNEL));
        }
        return Ok(());
    }
    let channel_ok = app.channel_perm(s, &c.id, perm::READ_CHANNEL).await?.0;
    let team_ok = app.has_team_perm(s, &c.team_id, perm::MANAGE_TEAM).await;
    if !(team_ok || channel_ok) {
        return Err(ch::by_name_missing());
    }
    Ok(())
}

async fn channel_by_name(
    St(app): St<State>,
    Auth(s): Auth,
    Path((team_id, name)): Path<(String, String)>,
    q: Q,
) -> ApiResult<Json<Channel>> {
    require_id(&team_id, "team_id")?;
    let c = app
        .channel_by_name(&team_id, &name, parse_bool(q.get("include_deleted")), false)
        .await?
        .ok_or_else(ch::by_name_missing)?;
    check_read_by_name(&app, &s, &c).await?;
    let mut list = [(*c).clone()];
    app.fill_channel_props(&mut list).await?;
    let [c] = list;
    Ok(Json(c))
}

async fn channel_by_team_name(
    St(app): St<State>,
    Auth(s): Auth,
    Path((team_name, name)): Path<(String, String)>,
    q: Q,
) -> ApiResult<Json<Channel>> {
    let team: Option<Team> = db::one(&app.db, "name = $1", &team_name).await?;
    let team = team.ok_or_else(|| {
        AppError::not_found("app.team.get_by_name.missing.app_error", "Unable to find the existing team.")
    })?;
    let c = app
        .channel_by_name(&team.id, &name, parse_bool(q.get("include_deleted")), false)
        .await?
        .ok_or_else(ch::by_name_missing)?;
    check_read_by_name(&app, &s, &c).await?;
    let mut list = [(*c).clone()];
    app.fill_channel_props(&mut list).await?;
    let [c] = list;
    Ok(Json(c))
}

/// `GetChannels`: the user's channels in the team and their DMs and GMs; 404 when there are none.
pub async fn channels_for_team_for_user_list(
    app: &App,
    team_id: &str,
    user_id: &str,
    include_deleted: bool,
    last_delete_at: i64,
) -> ApiResult<Vec<Channel>> {
    let mut sql = format!(
        "SELECT {} FROM Channels ch, ChannelMembers cm WHERE ch.Id = cm.ChannelId AND cm.UserId = $1
         AND ch.Type IN ('O', 'P', 'D', 'G') AND (ch.TeamId = $2 OR ch.TeamId = '')",
        db::cols::<Channel>("ch")
    );
    if include_deleted {
        if last_delete_at != 0 {
            sql += " AND (ch.DeleteAt = 0 OR ch.DeleteAt >= $3)";
        } else {
            sql += " AND $3::bigint IS NOT NULL";
        }
    } else {
        sql += " AND ch.DeleteAt = 0 AND $3::bigint IS NOT NULL";
    }
    sql += " ORDER BY ch.DisplayName";
    let list: Vec<Channel> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user_id)
        .bind(team_id)
        .bind(last_delete_at)
        .fetch_all(&app.db)
        .await?;
    if list.is_empty() {
        return Err(AppError::not_found("app.channel.get_channels.not_found.app_error", "No channels were found."));
    }
    Ok(list)
}

async fn channels_for_team_for_user(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    q: Q,
) -> ApiResult<Response> {
    let user_id = user_param(&s, &user_id)?;
    require_id(&team_id, "team_id")?;
    app.require_user(&s, &user_id).await?;
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    let last_delete_at = q.get("last_delete_at").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    if last_delete_at < 0 {
        return Err(AppError::invalid_url_param("last_delete_at"));
    }
    let mut list =
        channels_for_team_for_user_list(&app, &team_id, &user_id, parse_bool(q.get("include_deleted")), last_delete_at)
            .await?;
    let etag = channel_list_etag(&list);
    if let Some(r) = crate::teams::not_modified(&headers, &etag) {
        return Ok(r);
    }
    app.fill_channel_props(&mut list).await?;
    Ok(crate::teams::with_etag(Json(list), &etag))
}

/// `ChannelList.Etag`: the channel posted in or updated last, and how many there are.
fn channel_list_etag(list: &[Channel]) -> String {
    let (mut id, mut t) = ("0", 0i64);
    for c in list {
        if c.last_post_at > t {
            (id, t) = (&c.id, c.last_post_at);
        }
        if c.update_at > t {
            (id, t) = (&c.id, c.update_at);
        }
    }
    format!("{}.{id}.{t}.0.{}", crate::SERVER_VERSION, list.len())
}

/// `getChannelsForUser`: every channel of the user, in pages of 100 by id.
async fn channels_for_user(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    q: Q,
) -> ApiResult<Json<Vec<Channel>>> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    let last_delete_at = q.get("last_delete_at").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    if last_delete_at < 0 {
        return Err(AppError::invalid_url_param("last_delete_at"));
    }
    let include_deleted = parse_bool(q.get("include_deleted"));
    let mut sql = format!(
        "SELECT {} FROM Channels INNER JOIN ChannelMembers ON (Channels.Id = ChannelMembers.ChannelId)
         LEFT JOIN Teams ON (Channels.TeamId = Teams.Id)
         WHERE ChannelMembers.UserId = $1 AND Channels.Type IN ('O', 'P', 'D', 'G')",
        db::cols::<Channel>("Channels")
    );
    if include_deleted {
        if last_delete_at != 0 {
            sql += " AND (Channels.DeleteAt = 0 OR Channels.DeleteAt >= $2)
                     AND (Teams.Id IS NULL OR Teams.DeleteAt = 0 OR Teams.DeleteAt >= $2)";
        } else {
            sql += " AND $2::bigint IS NOT NULL";
        }
    } else {
        sql += " AND Channels.DeleteAt = 0 AND (Teams.DeleteAt = 0 OR Teams.Id IS NULL) AND $2::bigint IS NOT NULL";
    }
    sql += " ORDER BY Channels.Id ASC";
    let mut list: Vec<Channel> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&user_id).bind(last_delete_at).fetch_all(&app.db).await?;
    // Go streams `[` before its first page, so for a user in no channel it sends `[` and then the 404 error
    // JSON, with status 200: broken JSON. Fixed here: an empty array.
    app.fill_channel_props(&mut list).await?;
    Ok(Json(list))
}

/// `GetMembersForUser`: the user's memberships in the team's channels and in DMs and GMs.
async fn members_for_team_for_user(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id)): Path<(String, String)>,
) -> ApiResult<Json<Vec<ChannelMember>>> {
    let user_id = user_param(&s, &user_id)?;
    require_id(&team_id, "team_id")?;
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    if s.user_id != user_id && !app.has_team_perm(&s, &team_id, perm::MANAGE_SYSTEM).await {
        return Err(AppError::forbidden_perm(perm::MANAGE_SYSTEM));
    }
    let mut members = app
        .channel_members_where(
            "cm.userid = $1 AND (t.id = ANY($2) OR t.id = '' OR t.id IS NULL) AND c.type <> 'S'",
            &user_id,
            std::slice::from_ref(&team_id),
        )
        .await?;
    for m in &mut members {
        sanitize_member(m, &s.user_id);
    }
    Ok(Json(members))
}

/// `getChannelMembersForUser`: a page (`page` ≥ 0), or every membership as NDJSON (`page=-1`), with team data.
async fn channel_members_for_user(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    q: Q,
) -> ApiResult<Response> {
    let user_id = user_param(&s, &user_id)?;
    app.require_user(&s, &user_id).await?;
    let page = q.get("page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let (_, per_page) = paging(&q);
    let select = perm::channel_members_select().replacen(
        " FROM ChannelMembers",
        ", COALESCE(t.DisplayName, '') AS teamdisplayname, COALESCE(t.Name, '') AS teamname,
           COALESCE(t.UpdateAt, 0) AS teamupdateat FROM ChannelMembers",
        1,
    );
    if page != -1 {
        let page = page.max(0);
        let sql =
            format!("{select} WHERE cm.UserId = $1 AND c.Type <> 'S' ORDER BY cm.ChannelId ASC LIMIT $2 OFFSET $3");
        let rows: Vec<WithTeam<perm::WithScheme<ChannelMember>>> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&user_id)
            .bind(per_page)
            .bind(page * per_page)
            .fetch_all(&app.db)
            .await?;
        let out: Vec<WithTeam<ChannelMember>> = rows
            .into_iter()
            .map(|w| {
                let mut m = w.inner.into_member();
                sanitize_member(&mut m, &s.user_id);
                WithTeam {
                    inner: m,
                    team_display_name: w.team_display_name,
                    team_name: w.team_name,
                    team_update_at: w.team_update_at,
                }
            })
            .collect();
        return Ok(Json(out).into_response());
    }
    let sql = format!("{select} WHERE cm.UserId = $1 AND c.Type <> 'S' ORDER BY cm.ChannelId ASC");
    let rows: Vec<WithTeam<perm::WithScheme<ChannelMember>>> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&user_id).fetch_all(&app.db).await?;
    let mut body = String::new();
    for w in rows {
        let mut m = w.inner.into_member();
        sanitize_member(&mut m, &s.user_id);
        let out = WithTeam {
            inner: m,
            team_display_name: w.team_display_name,
            team_name: w.team_name,
            team_update_at: w.team_update_at,
        };
        body += &serde_json::to_string(&out).unwrap();
        body.push('\n');
    }
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response())
}

impl<T: serde::Serialize> serde::Serialize for perm::WithScheme<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

#[cfg(test)]
mod etag_tests {
    use crate::model::Channel;

    #[test]
    fn channel_list_etag() {
        let v = crate::SERVER_VERSION;
        let c = |id: &str, post: i64, upd: i64| Channel {
            id: id.into(),
            last_post_at: post,
            update_at: upd,
            ..Default::default()
        };
        assert_eq!(super::channel_list_etag(&[]), format!("{v}.0.0.0.0"));
        assert_eq!(super::channel_list_etag(&[c("a", 5, 1), c("b", 2, 9), c("c", 9, 0)]), format!("{v}.b.9.0.3"));
    }
}
