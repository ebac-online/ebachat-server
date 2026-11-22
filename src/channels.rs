//! Channels and channel members: reading through the channel caches, creating (open, private, direct, group),
//! joining and leaving, viewing (unread counts), notify props, stats. The handlers are in `channels_api.rs`.
//!
//! Contracts other modules use: [`App::mark_viewed`], [`App::channel_member`], [`App::add_channel_member`].

// Based on Mattermost server/channels/app/channel.go, app/platform/web_hub.go (cache invalidation),
// store/sqlstore/channel_store.go, store/localcachelayer/channel_layer.go and model/channel.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Map, Value, json};

use sqlx::types::Json;

use crate::app::{ApiResult, App, AppError, is_valid_id, new_id, now_ms};
use crate::db::{self, Table};
use crate::hub::Broadcast;
use crate::model::channel::{self, DIRECT, GROUP, OPEN, OPEN_BOARD, PRIVATE, PRIVATE_BOARD, SPACE};
use crate::model::{Channel, ChannelMember, Post, Session, Team, User};
use crate::perm::{self, WithScheme};

pub use crate::channels_api::routes;

/// `model.DefaultChannelName`.
pub const DEFAULT_CHANNEL: &str = "town-square";
const GROUP_MIN_USERS: usize = 3;
const GROUP_MAX_USERS: usize = 8;
const NAME_MAX_LENGTH: usize = 64;
/// `channelSliceColumns(true)` adds the access control policy columns; there are no policies without a
/// license, so they are always false.
const MESSAGE_TYPES: &str = "('O', 'P', 'D', 'G')";

// Permission ids not in `perm`.
pub const CONVERT_PRIVATE_CHANNEL_TO_PUBLIC: &str = "convert_private_channel_to_public";
pub const SYSCONSOLE_WRITE_USERMANAGEMENT_CHANNELS: &str = "sysconsole_write_user_management_channels";
pub const READ_PUBLIC_CHANNEL_GROUPS: &str = "read_public_channel_groups";

// ---------- errors ----------

pub fn channel_not_found(id: &str) -> AppError {
    AppError::not_found("app.channel.get.existing.app_error", &format!("Unable to find the existing channel {id}."))
}
pub fn member_not_found() -> AppError {
    AppError::not_found(
        "app.channel.get_member.missing.app_error",
        "No channel member found for that user ID and channel ID.",
    )
}
pub fn channel_exists() -> AppError {
    AppError::bad_request(
        "store.sql_channel.save_channel.exists.app_error",
        "A channel with that name already exists on the same team.",
    )
}
pub fn by_name_missing() -> AppError {
    AppError::not_found("app.channel.get_by_name.missing.app_error", "Channel does not exist.")
}
fn bad(id: &str, m: &str) -> AppError {
    AppError::bad_request(id, m)
}

/// `IsChannelMemberNotifyPropsValid(props, allowMissingFields = true)` on the props a notify-props update writes.
/// Go's `UpdateChannelMemberNotifyProps` merges them unchecked (`notifyprops || $1::jsonb`), so an invalid value
/// ("mark_unread": "sometimes") is stored and every later `UpdateMember` of that member (roles, scheme roles)
/// fails `ChannelMember.IsValid` with 400; here the update itself is refused with that error.
fn validate_member_notify_props(props: &BTreeMap<String, String>) -> ApiResult<()> {
    const LEVELS: [&str; 4] = ["default", "all", "mention", "none"];
    let checks: [(&str, usize, &[&str], &str, &str); 6] = [
        ("desktop", 20, &LEVELS, "notify_level", "Invalid notify level."),
        ("mark_unread", 20, &["all", "mention"], "unread_level", "Invalid mark unread level."),
        ("push", 20, &LEVELS, "push_level", "Invalid push notification level."),
        ("email", 20, &["default", "true", "false"], "email_value", "Invalid email notification value."),
        (
            "ignore_channel_mentions",
            40,
            &["default", "off", "on"],
            "ignore_channel_mentions_value",
            "Invalid ignore channel mentions status.",
        ),
        (
            "channel_auto_follow_threads",
            3,
            &["on", "off"],
            "channel_auto_follow_threads_value",
            "Invalid channel-auto-follow-threads value.",
        ),
    ];
    for (key, max, valid, id, msg) in checks {
        if let Some(v) = props.get(key)
            && (v.len() > max || !valid.contains(&v.as_str()))
        {
            return Err(bad(&format!("model.channel_member.is_valid.{id}.app_error"), msg));
        }
    }
    Ok(())
}

// ---------- model rules ----------

/// `validSimpleAlphaNum`: `^[a-z0-9]+([a-z\-\_0-9]+|(__)?)[a-z0-9]*$`.
pub fn is_valid_channel_name(s: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-z0-9]+([a-z\-_0-9]+|(__)?)[a-z0-9]*$").unwrap());
    !s.is_empty() && RE.is_match(s)
}

/// `Channel.IsValid` for what this server writes.
pub(crate) fn validate(c: &Channel) -> ApiResult<()> {
    let invalid = |id: &str, m: &str| Err(bad(id, m));
    if !is_valid_id(&c.id) {
        return invalid("model.channel.is_valid.id.app_error", "Invalid Id.");
    }
    if c.display_name.chars().count() > 64 {
        return invalid("model.channel.is_valid.display_name.app_error", "Invalid display name.");
    }
    if !is_valid_channel_name(&c.name) {
        return invalid(
            "model.channel.is_valid.1_or_more.app_error",
            "Name must be 1 or more lowercase alphanumeric character.",
        );
    }
    if ![OPEN, PRIVATE, DIRECT, GROUP, SPACE, OPEN_BOARD, PRIVATE_BOARD].contains(&c.r#type.as_str()) {
        return invalid("model.channel.is_valid.type.app_error", "Invalid type.");
    }
    if c.header.chars().count() > 1024 {
        return invalid("model.channel.is_valid.header.app_error", "Invalid header.");
    }
    if c.purpose.chars().count() > 250 {
        return invalid("model.channel.is_valid.purpose.app_error", "Invalid purpose.");
    }
    if c.creator_id.len() > 26 {
        return invalid("model.channel.is_valid.creator_id.app_error", "Invalid creator id.");
    }
    if c.r#type != DIRECT && c.r#type != GROUP {
        let hex40 = c.name.len() == 40 && c.name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let ids: Vec<&str> = c.name.split("__").collect();
        if hex40 || (ids.len() == 2 && is_valid_id(ids[0]) && is_valid_id(ids[1])) {
            return invalid(
                "model.channel.is_valid.name.app_error",
                "Channel names can't be in a hexadecimal format. Please enter a different channel name.",
            );
        }
    }
    if c.discoverable && c.r#type != PRIVATE {
        return invalid(
            "model.channel.is_valid.discoverable.app_error",
            "Only private channels can be marked as discoverable.",
        );
    }
    Ok(())
}

/// `model.GetGroupNameFromUserIds`: sha1 of the sorted ids, hex.
pub fn group_name(ids: &[String]) -> String {
    use sha1::Digest;
    let mut ids = ids.to_vec();
    ids.sort();
    let mut h = sha1::Sha1::new();
    for id in &ids {
        h.update(id.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `model.ChannelMentions`: `~name` not preceded by a word character.
fn channel_mentions(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out: Vec<String> = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'~' && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-' || b[j] == b'_') {
                j += 1;
            }
            if j > start {
                let name = s[start..j].to_lowercase();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// `ChannelWithTeamData`.
#[derive(Serialize)]
pub struct WithTeam<T: Serialize> {
    #[serde(flatten)]
    pub inner: T,
    pub team_display_name: String,
    pub team_name: String,
    pub team_update_at: i64,
}

impl<'r, T: Serialize + sqlx::FromRow<'r, sqlx::postgres::PgRow>> sqlx::FromRow<'r, sqlx::postgres::PgRow>
    for WithTeam<T>
{
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        Ok(WithTeam {
            inner: T::from_row(row)?,
            team_display_name: db::col(row, "teamdisplayname")?,
            team_name: db::col(row, "teamname")?,
            team_update_at: db::col(row, "teamupdateat")?,
        })
    }
}

/// `ChannelMember.SanitizeForCurrentUser`.
pub fn sanitize_member(m: &mut ChannelMember, current_user: &str) {
    if m.user_id != current_user {
        m.last_viewed_at = -1;
        m.last_update_at = -1;
    }
}

/// A system message as the channel functions post them.
pub(crate) fn system_post(channel_id: &str, user_id: &str, kind: &str, message: String, props: Value) -> Post {
    let Value::Object(props) = props else { unreachable!() };
    Post {
        channel_id: channel_id.into(),
        user_id: user_id.into(),
        r#type: kind.into(),
        message,
        props: Json(props),
        ..Default::default()
    }
}

/// `json:"-"` free JSON of a row, as Go's `json.Marshal` sends it inside event data.
pub(crate) fn json_string<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

impl App {
    // ---------- cache invalidation (platform/web_hub.go) ----------

    /// `InvalidateCacheForChannel`: by id and by name.
    pub fn invalidate_channel_cache(&self, c: &Channel) {
        self.caches.invalidate_channel(c);
        if c.team_id.is_empty() {
            self.caches.channel_by_name.remove(&format!("dm{}", c.name));
        }
    }

    /// `InvalidateChannelCacheForUser`: the user's memberships and the profiles-in-channel lists they are in.
    pub fn invalidate_channel_cache_for_user(&self, user_id: &str) {
        self.caches.invalidate_members_for_user(user_id);
        self.caches.profiles_in_channel.remove_where(|_, m| m.contains_key(user_id));
    }

    /// `invalidateCacheForChannelMembers`: profiles in channel, member and guest counts.
    pub fn invalidate_channel_members_cache(&self, channel_id: &str) {
        self.caches.profiles_in_channel.remove(channel_id);
        self.caches.member_count.remove(channel_id);
        self.caches.guest_count.remove(channel_id);
    }

    // ---------- reads ----------

    /// `GetChannel` or its 404.
    pub async fn get_channel(&self, id: &str) -> ApiResult<Arc<Channel>> {
        self.channel(id).await?.ok_or_else(|| channel_not_found(id))
    }

    /// `ChannelStore.GetByName` / `GetByNameIncludeDeleted`, through the by-name cache when `from_cache`.
    pub async fn channel_by_name(
        &self,
        team_id: &str,
        name: &str,
        include_deleted: bool,
        from_cache: bool,
    ) -> ApiResult<Option<Arc<Channel>>> {
        let key = format!("{team_id}{name}");
        if from_cache
            && let Some(c) = self.caches.channel_by_name.get(&key)
            && (include_deleted || c.delete_at == 0)
        {
            return Ok(Some(c));
        }
        let mut sql =
            format!("{} WHERE (teamid = $1 OR teamid = '') AND name = $2 AND type IN {MESSAGE_TYPES}", Channel::SELECT);
        if !include_deleted {
            sql += " AND deleteat = 0";
        }
        let c: Option<Channel> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(team_id).bind(name).fetch_optional(&self.db).await?;
        Ok(c.map(|c| {
            let c = Arc::new(c);
            if from_cache {
                self.caches.channel_by_name.put(key, c.clone());
            }
            c
        }))
    }

    /// `GetChannelsByNames` (cached, open and archived excluded as the store does).
    async fn channels_by_names(&self, team_id: &str, names: &[String]) -> ApiResult<Vec<Arc<Channel>>> {
        let mut out = vec![];
        let mut misses = vec![];
        for n in names {
            match self.caches.channel_by_name.get(&format!("{team_id}{n}")) {
                Some(c) if c.delete_at == 0 => out.push(c),
                Some(_) => {}
                None => misses.push(n.clone()),
            }
        }
        if !misses.is_empty() {
            let sql = format!(
                "{} WHERE (teamid = $1 OR teamid = '') AND name = ANY($2) AND type IN {MESSAGE_TYPES} AND deleteat = 0",
                Channel::SELECT
            );
            let found: Vec<Channel> =
                sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(team_id).bind(&misses).fetch_all(&self.db).await?;
            for c in found {
                let c = Arc::new(c);
                self.caches.channel_by_name.put(format!("{team_id}{}", c.name), c.clone());
                out.push(c);
            }
        }
        Ok(out)
    }

    /// `FillInChannelsProps`: `props.channel_mentions` for the public channels the header mentions.
    pub async fn fill_channel_props(&self, channels: &mut [Channel]) -> ApiResult<()> {
        let mut by_team: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, c) in channels.iter().enumerate() {
            by_team.entry(c.team_id.clone()).or_default().push(i);
        }
        for (team_id, idx) in by_team {
            let mentions: Vec<Vec<String>> = idx.iter().map(|&i| channel_mentions(&channels[i].header)).collect();
            let all: Vec<String> = mentions.iter().flatten().cloned().collect::<HashSet<_>>().into_iter().collect();
            if all.is_empty() {
                continue;
            }
            let found = self.channels_by_names(&team_id, &all).await?;
            let by_name: HashMap<&str, &Channel> = found.iter().map(|c| (c.name.as_str(), c.as_ref())).collect();
            for (k, &i) in idx.iter().enumerate() {
                let mut prop = Map::new();
                for m in &mentions[k] {
                    if let Some(c) = by_name.get(m.as_str())
                        && c.r#type == OPEN
                    {
                        prop.insert(c.name.clone(), json!({"display_name": c.display_name}));
                    }
                }
                let ch = &mut channels[i];
                if !prop.is_empty() {
                    ch.props
                        .get_or_insert_with(Default::default)
                        .0
                        .insert("channel_mentions".into(), Value::Object(prop));
                } else if let Some(p) = ch.props.as_mut() {
                    p.0.remove("channel_mentions");
                }
            }
        }
        Ok(())
    }

    /// `GetMemberCount` through the member count cache: active users.
    pub async fn member_count(&self, channel_id: &str) -> ApiResult<i64> {
        if let Some(n) = self.caches.member_count.get(channel_id) {
            return Ok(n);
        }
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ChannelMembers, Users WHERE ChannelMembers.UserId = Users.Id
             AND ChannelMembers.ChannelId = $1 AND Users.DeleteAt = 0",
        )
        .bind(channel_id)
        .fetch_one(&self.db)
        .await?;
        self.caches.member_count.put(channel_id.into(), n);
        Ok(n)
    }

    /// `GetGuestCount` through its cache.
    pub async fn guest_count(&self, channel_id: &str) -> ApiResult<i64> {
        if let Some(n) = self.caches.guest_count.get(channel_id) {
            return Ok(n);
        }
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ChannelMembers, Users WHERE ChannelMembers.UserId = Users.Id
             AND ChannelMembers.ChannelId = $1 AND ChannelMembers.SchemeGuest = TRUE AND Users.DeleteAt = 0",
        )
        .bind(channel_id)
        .fetch_one(&self.db)
        .await?;
        self.caches.guest_count.put(channel_id.into(), n);
        Ok(n)
    }

    /// `GetPinnedPostCount` through its cache; posts invalidate it on pin and unpin.
    pub async fn pinned_count(&self, channel_id: &str) -> ApiResult<i64> {
        if let Some(n) = self.caches.pinned_count.get(channel_id) {
            return Ok(n);
        }
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM Posts WHERE IsPinned = true AND ChannelId = $1 AND DeleteAt = 0")
                .bind(channel_id)
                .fetch_one(&self.db)
                .await?;
        self.caches.pinned_count.put(channel_id.into(), n);
        Ok(n)
    }

    /// `GetChannelsMemberCount` through the member count cache (zero counts are not reported from it).
    pub async fn channels_member_count(&self, ids: &[String]) -> ApiResult<BTreeMap<String, i64>> {
        let mut out = BTreeMap::new();
        let mut rest = vec![];
        for id in ids {
            match self.caches.member_count.get(id) {
                Some(n) if n != 0 => {
                    out.insert(id.clone(), n);
                }
                Some(_) => {}
                None => rest.push(id.clone()),
            }
        }
        if !rest.is_empty() {
            let rows: Vec<(String, i64)> = sqlx::query_as(
                "SELECT ChannelMembers.ChannelId, COUNT(*) FROM ChannelMembers
                 INNER JOIN Users ON ChannelMembers.UserId = Users.Id
                 WHERE ChannelMembers.ChannelId = ANY($1) AND Users.DeleteAt = 0 GROUP BY ChannelMembers.ChannelId",
            )
            .bind(&rest)
            .fetch_all(&self.db)
            .await?;
            for (id, n) in rows {
                self.caches.member_count.put(id.clone(), n);
                out.insert(id, n);
            }
        }
        Ok(out)
    }

    /// `GetChannelMember` with the scheme-derived roles.
    pub async fn channel_member(&self, channel_id: &str, user_id: &str) -> ApiResult<Option<ChannelMember>> {
        Ok(perm::channel_members(self, "cm.channelid = $1 AND cm.userid = $2", &[channel_id, user_id])
            .await?
            .into_iter()
            .next())
    }

    pub async fn channel_member_or_404(&self, channel_id: &str, user_id: &str) -> ApiResult<ChannelMember> {
        self.channel_member(channel_id, user_id).await?.ok_or_else(member_not_found)
    }

    /// Channel members with derived roles; `cond` on `cm.`/`c.`/`t.`, binds a text and a text array.
    pub async fn channel_members_where(&self, cond: &str, a: &str, b: &[String]) -> ApiResult<Vec<ChannelMember>> {
        let sql = format!("{} WHERE {cond}", perm::channel_members_select());
        let rows: Vec<WithScheme<ChannelMember>> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(a).bind(b).fetch_all(&self.db).await?;
        Ok(rows.into_iter().map(|w| w.into_member()).collect())
    }

    /// `IsCRTEnabledForUser`.
    pub async fn is_crt_enabled(&self, user_id: &str) -> bool {
        match self.cfg.str("ServiceSettings.CollapsedThreads") {
            "disabled" => false,
            "always_on" => true,
            setting => {
                let pref: Option<String> = sqlx::query_scalar(
                    "SELECT Value FROM Preferences WHERE UserId = $1 AND Category = 'display_settings'
                     AND Name = 'collapsed_reply_threads'",
                )
                .bind(user_id)
                .fetch_optional(&self.db)
                .await
                .ok()
                .flatten();
                match pref {
                    Some(v) => v == "on",
                    None => setting == "default_on",
                }
            }
        }
    }

    /// `HasPermissionToTeam` for a user (not a session): their team roles, then their system roles.
    pub async fn user_has_team_perm(&self, user: &User, team_id: &str, permission: &str) -> ApiResult<bool> {
        if !team_id.is_empty() {
            let tm =
                perm::team_members(self, "tm.teamid = $1 AND tm.userid = $2 AND tm.deleteat = 0", &[team_id, &user.id])
                    .await?;
            if let Some(tm) = tm.first()
                && self.roles_grant(&tm.roles, permission).await
            {
                return Ok(true);
            }
        }
        Ok(self.roles_grant(&user.roles, permission).await)
    }

    /// `HasPermissionToChannel` for a user: channel roles, then team or system.
    pub async fn user_has_channel_perm(&self, user: &User, channel: &Channel, permission: &str) -> ApiResult<bool> {
        let ids = self.member_roles(&user.id, true).await?;
        if let Some(roles) = ids.get(&channel.id)
            && self.roles_grant(roles, permission).await
        {
            return Ok(true);
        }
        if !channel.team_id.is_empty() {
            return self.user_has_team_perm(user, &channel.team_id, permission).await;
        }
        Ok(self.roles_grant(&user.roles, permission).await)
    }

    /// `UserCanSeeOtherUser`: without `view_members`, only people in a shared team or channel.
    pub async fn user_can_see(&self, s: &Session, other: &str) -> ApiResult<bool> {
        self.can_see_user(s, other).await
    }

    // ---------- store writes ----------

    /// `saveChannelT` + `upsertPublicChannelT`: a new channel, or the one already of that name (`Err(existing)`).
    pub(crate) async fn save_channel(&self, c: &mut Channel, check_limit: bool) -> ApiResult<Result<(), Channel>> {
        if c.id.is_empty() {
            c.id = new_id();
        }
        if c.create_at == 0 {
            c.create_at = now_ms();
        }
        c.update_at = c.create_at;
        c.extra_update_at = 0;
        validate(c)?;
        let mut tx = self.db.begin().await?;
        if check_limit && (c.r#type == OPEN || c.r#type == PRIVATE) {
            let max = self.cfg.i64("TeamSettings.MaxChannelsPerTeam");
            if max >= 0 {
                let n: i64 = sqlx::query_scalar(
                    "SELECT COUNT(0) FROM Channels WHERE TeamId = $1 AND DeleteAt = 0 AND (Type = 'O' OR Type = 'P')",
                )
                .bind(&c.team_id)
                .fetch_one(&mut *tx)
                .await?;
                if n >= max {
                    return Err(bad(
                        "store.sql_channel.save_channel.limit.app_error",
                        "You've reached the limit of the number of allowed channels.",
                    ));
                }
            }
        }
        let sql = format!("{} ON CONFLICT (TeamId, Name) DO NOTHING", Channel::INSERT);
        let n = c.bind(sqlx::query(sqlx::AssertSqlSafe(sql))).execute(&mut *tx).await?.rows_affected();
        if n == 0 {
            let dup: Channel =
                sqlx::query_as(sqlx::AssertSqlSafe(format!("{} WHERE TeamId = $1 AND Name = $2", Channel::SELECT)))
                    .bind(&c.team_id)
                    .bind(&c.name)
                    .fetch_one(&mut *tx)
                    .await?;
            return Ok(Err(dup));
        }
        upsert_public_channel(&mut tx, c).await?;
        tx.commit().await?;
        Ok(Ok(()))
    }

    /// `SqlChannelStore.Update` + `UpdateChannel`: saved, caches dropped, `channel_updated` to its members.
    pub async fn update_channel(&self, c: &mut Channel) -> ApiResult<()> {
        self.store_update_channel(c).await?;
        // Space backing channels are internal: no `channel_updated`.
        if c.is_space() {
            return Ok(());
        }
        self.hub.publish(
            "channel_updated",
            json!({"channel": json_string(c)}),
            Broadcast { channel_id: c.id.clone(), ..Default::default() },
        );
        Ok(())
    }

    /// `SqlChannelStore.Update` through the cache layer: saved and the channel's caches dropped, no event.
    pub async fn store_update_channel(&self, c: &mut Channel) -> ApiResult<()> {
        c.update_at = now_ms();
        if c.delete_at != 0 {
            return Err(bad("app.channel.update.bad_id", "Unable to update the channel."));
        }
        validate(c)?;
        let mut tx = self.db.begin().await?;
        let r = c.bind(sqlx::query(Channel::UPDATE)).execute(&mut *tx).await;
        if let Err(e) = r {
            if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
                return Err(channel_exists());
            }
            return Err(e.into());
        }
        upsert_public_channel(&mut tx, c).await?;
        tx.commit().await?;
        self.invalidate_channel_cache(c);
        Ok(())
    }

    /// `saveMultipleMembers` for one member: written with its explicit roles, returned with the derived ones.
    pub(crate) async fn save_member(&self, m: ChannelMember) -> ApiResult<ChannelMember> {
        let mut row = m;
        row.last_update_at = now_ms();
        row.roles = std::mem::take(&mut row.explicit_roles);
        let r = db::insert(&self.db, &row).await;
        if let Err(e) = r {
            if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
                return Err(bad(
                    "app.channel.save_member.exists.app_error",
                    "A channel member with that ID already exists.",
                ));
            }
            return Err(e.into());
        }
        self.invalidate_channel_cache_for_user(&row.user_id);
        self.caches.member_count.remove(&row.channel_id);
        self.channel_member_or_404(&row.channel_id, &row.user_id).await
    }

    /// `SqlChannelStore.UpdateMember`: every column, the explicit roles in `Roles`.
    pub(crate) async fn update_member(&self, m: &ChannelMember) -> ApiResult<ChannelMember> {
        sqlx::query(
            "UPDATE ChannelMembers SET Roles = $3, LastViewedAt = $4, MsgCount = $5, MentionCount = $6,
                 MentionCountRoot = $7, UrgentMentionCount = $8, MsgCountRoot = $9, NotifyProps = $10,
                 LastUpdateAt = $11, SchemeGuest = $12, SchemeUser = $13, SchemeAdmin = $14,
                 AutoTranslationDisabled = $15
             WHERE ChannelId = $1 AND UserId = $2",
        )
        .bind(&m.channel_id)
        .bind(&m.user_id)
        .bind(&m.explicit_roles)
        .bind(m.last_viewed_at)
        .bind(m.msg_count)
        .bind(m.mention_count)
        .bind(m.mention_count_root)
        .bind(m.urgent_mention_count)
        .bind(m.msg_count_root)
        .bind(&m.notify_props)
        .bind(now_ms())
        .bind(m.scheme_guest)
        .bind(m.scheme_user)
        .bind(m.scheme_admin)
        .bind(m.auto_translation_disabled)
        .execute(&self.db)
        .await?;
        self.caches.member_count.remove(&m.channel_id);
        self.channel_member_or_404(&m.channel_id, &m.user_id).await
    }

    /// `updateChannelMember`: saved, the user's channel caches dropped, `channel_member_updated` to them.
    pub async fn update_channel_member(&self, m: &ChannelMember) -> ApiResult<ChannelMember> {
        let m = self.update_member(m).await?;
        self.invalidate_channel_cache_for_user(&m.user_id);
        self.publish_member_updated(&m);
        Ok(m)
    }

    fn publish_member_updated(&self, m: &ChannelMember) {
        self.hub.publish(
            "channel_member_updated",
            json!({"channelMember": json_string(m)}),
            Broadcast { user_id: m.user_id.clone(), ..Default::default() },
        );
    }

    pub(crate) async fn log_join(&self, user_id: &str, channel_id: &str) -> ApiResult<()> {
        sqlx::query("INSERT INTO ChannelMemberHistory (UserId, ChannelId, JoinTime) VALUES ($1, $2, $3)")
            .bind(user_id)
            .bind(channel_id)
            .bind(now_ms())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn log_leave(&self, user_id: &str, channel_id: &str) -> ApiResult<()> {
        sqlx::query(
            "UPDATE ChannelMemberHistory SET LeaveTime = $1 WHERE UserId = $2 AND ChannelId = $3 AND LeaveTime IS NULL",
        )
        .bind(now_ms())
        .bind(user_id)
        .bind(channel_id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Posts a system message; its failure is logged, as Go's `...AndForget` messages are.
    pub(crate) async fn post_system(&self, p: Post) {
        if let Err(e) = self.create_post(p, true).await
            && e.status_code != StatusCode::NOT_IMPLEMENTED.as_u16()
        {
            eprintln!("failed to post a channel system message: {} {}", e.id, e.detailed_error);
        }
    }

    // ---------- create ----------

    /// `CreateChannelWithUser`.
    pub async fn create_channel_with_user(&self, mut c: Channel, user_id: &str) -> ApiResult<Channel> {
        if c.is_group_or_direct() {
            return Err(bad(
                "api.channel.create_channel.direct_channel.app_error",
                "Must use createDirectChannel API service for direct message channel creation.",
            ));
        }
        if c.is_board() {
            return Err(
                bad("app.channel.create_channel.board_type.app_error", "").detail("use CreateBoardChannel instead")
            );
        }
        if c.is_space() {
            return Err(bad("app.channel.create_channel.space_type.app_error", "").detail("use CreateChannel instead"));
        }
        if c.team_id.is_empty() {
            return Err(bad(
                "app.channel.create_channel.no_team_id.app_error",
                "Must specify the team ID to create a channel.",
            ));
        }
        let max = self.cfg.i64("TeamSettings.MaxChannelsPerTeam");
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM Channels WHERE TeamId = $1 AND Type IN ('O', 'P', 'G')")
                .bind(&c.team_id)
                .fetch_one(&self.db)
                .await?;
        if count + 1 > max {
            return Err(bad(
                "api.channel.create_channel.max_channel_limit.app_error",
                &format!("Unable to create more than {max} channels for current team."),
            ));
        }
        c.creator_id = user_id.into();
        // CreateChannel
        c.display_name = c.display_name.trim().into();
        c.default_category_name = c.default_category_name.trim().into();
        c.delete_at = 0;
        if self.save_channel(&mut c, true).await?.is_err() {
            return Err(channel_exists());
        }
        let user = self.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
        self.save_member(ChannelMember {
            channel_id: c.id.clone(),
            user_id: user.id.clone(),
            scheme_guest: user.is_guest(),
            scheme_user: !user.is_guest(),
            scheme_admin: true,
            notify_props: channel::default_member_notify_props(),
            ..Default::default()
        })
        .await?;
        self.log_join(&user.id, &c.id).await?;
        self.invalidate_channel_cache_for_user(&user.id);
        self.post_join_message(&user, &c).await;
        self.hub.publish(
            "channel_created",
            json!({"channel_id": c.id, "team_id": c.team_id}),
            Broadcast { user_id: user_id.into(), ..Default::default() },
        );
        Ok(c)
    }

    /// `GetOrCreateDirectChannel`.
    pub async fn get_or_create_direct(&self, s: &Session, user_id: &str, other_id: &str) -> ApiResult<Channel> {
        let name = channel::direct_name(user_id, other_id);
        if let Some(c) = self.channel_by_name("", &name, false, true).await? {
            return Ok((*c).clone());
        }
        if self.cfg.str("TeamSettings.RestrictDirectMessage") == "team" && !self.has_perm(s, perm::MANAGE_SYSTEM).await
        {
            let common: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM TeamMembers a JOIN TeamMembers b ON a.TeamId = b.TeamId
                 JOIN Teams t ON t.Id = a.TeamId
                 WHERE a.UserId = $1 AND b.UserId = $2 AND a.DeleteAt = 0 AND b.DeleteAt = 0 AND t.DeleteAt = 0)",
            )
            .bind(user_id)
            .bind(other_id)
            .fetch_one(&self.db)
            .await?;
            let bots = self.users(&[user_id.to_string(), other_id.to_string()]).await?.iter().any(|u| u.is_bot);
            if !common && !bots {
                return Err(AppError::new(
                    StatusCode::FORBIDDEN,
                    "api.channel.create_channel.direct_channel.team_restricted_error",
                    "A direct channel cannot be created between these users because they do not share a team in common.",
                ));
            }
        }
        let invalid = || {
            bad(
                "api.channel.create_direct_channel.invalid_user.app_error",
                "Invalid user ID for direct channel creation.",
            )
        };
        let users = self.users(&[user_id.to_string(), other_id.to_string()]).await?;
        let user = users.iter().find(|u| u.id == user_id).ok_or_else(invalid)?.clone();
        let other = users.iter().find(|u| u.id == other_id).ok_or_else(invalid)?.clone();
        // CreateDirectChannel
        let mut c = Channel {
            name: channel::direct_name(&other.id, &user.id),
            r#type: DIRECT.into(),
            shared: Some(false),
            creator_id: user.id.clone(),
            ..Default::default()
        };
        if let Err(existing) = self.save_channel(&mut c, false).await? {
            return Ok(existing);
        }
        let mut pair = vec![user.clone()];
        if other.id != user.id {
            pair.push(other.clone());
        }
        for u in &pair {
            self.save_member(ChannelMember {
                channel_id: c.id.clone(),
                user_id: u.id.clone(),
                scheme_guest: u.is_guest(),
                scheme_user: !u.is_guest(),
                notify_props: channel::default_member_notify_props(),
                ..Default::default()
            })
            .await?;
            self.log_join(&u.id, &c.id).await?;
        }
        // handleCreationEvent
        self.invalidate_channel_cache_for_user(user_id);
        self.invalidate_channel_cache_for_user(other_id);
        self.hub.publish(
            "direct_added",
            json!({"creator_id": user_id, "teammate_id": other_id}),
            Broadcast { channel_id: c.id.clone(), ..Default::default() },
        );
        Ok(c)
    }

    /// `CreateGroupChannel`.
    pub async fn create_group_channel(&self, ids: &[String], creator_id: &str) -> ApiResult<Channel> {
        if ids.len() > GROUP_MAX_USERS || ids.len() < GROUP_MIN_USERS {
            return Err(bad(
                "api.channel.create_group.bad_size.app_error",
                "Group message channels must contain at least 3 and no more than 8 users.",
            ));
        }
        // Go sorts the ids in place when it names the group; the events list them sorted.
        let mut ids = ids.to_vec();
        ids.sort();
        let ids = &ids[..];
        let users = crate::users::load_users(self, "u.id = ANY($1)", ids).await?;
        if users.len() != ids.len() {
            return Err(bad(
                "api.channel.create_group.bad_user.app_error",
                "One of the provided users does not exist.",
            ));
        }
        let mut names: Vec<&str> = users.iter().map(|u| u.username.as_str()).collect();
        names.sort();
        let mut display = names.join(", ");
        if display.len() > NAME_MAX_LENGTH {
            let mut end = NAME_MAX_LENGTH;
            while !display.is_char_boundary(end) {
                end -= 1;
            }
            display.truncate(end);
        }
        let mut c = Channel {
            name: group_name(ids),
            display_name: display,
            r#type: GROUP.into(),
            shared: Some(false),
            ..Default::default()
        };
        if let Err(existing) = self.save_channel(&mut c, true).await? {
            return Ok(existing);
        }
        for u in &users {
            self.save_member(ChannelMember {
                channel_id: c.id.clone(),
                user_id: u.id.clone(),
                scheme_guest: u.is_guest(),
                scheme_user: !u.is_guest(),
                notify_props: channel::default_member_notify_props(),
                ..Default::default()
            })
            .await?;
            self.log_join(&u.id, &c.id).await?;
        }
        let _ = creator_id;
        let teammates = serde_json::to_string(ids).unwrap();
        for id in ids {
            self.invalidate_channel_cache_for_user(id);
            self.hub.publish(
                "group_added",
                json!({"teammate_ids": teammates}),
                Broadcast { channel_id: c.id.clone(), user_id: id.clone(), ..Default::default() },
            );
        }
        Ok(c)
    }

    // ---------- members ----------

    /// `addUserToChannel`: the membership, history and caches; true when added now.
    async fn add_user_to_channel_row(&self, user: &User, channel: &Channel) -> ApiResult<(ChannelMember, bool)> {
        if channel.r#type != OPEN && channel.r#type != PRIVATE && !channel.is_space() {
            return Err(bad("api.channel.add_user_to_channel.type.app_error", "Cannot add user to this channel type."));
        }
        if let Some(m) = self.channel_member(&channel.id, &user.id).await? {
            return Ok((m, false));
        }
        let saved = self
            .save_member(ChannelMember {
                channel_id: channel.id.clone(),
                user_id: user.id.clone(),
                notify_props: channel::default_member_notify_props(),
                scheme_guest: user.is_guest(),
                scheme_user: !user.is_guest(),
                ..Default::default()
            })
            .await;
        let m = match saved {
            Ok(m) => m,
            Err(e) if e.id == "app.channel.save_member.exists.app_error" => {
                return Ok((self.channel_member_or_404(&channel.id, &user.id).await?, false));
            }
            Err(e) => {
                return Err(AppError::internal("api.channel.add_user.to.channel.failed.app_error", e.detailed_error));
            }
        };
        self.log_join(&user.id, &channel.id).await?;
        self.invalidate_channel_cache_for_user(&user.id);
        self.invalidate_channel_members_cache(&channel.id);
        self.caches.members_notify_props.remove(&channel.id);
        Ok((m, true))
    }

    /// `AddUserToChannel`: team membership checked, sidebar, `user_added` to the channel and to the user; true when
    /// added now.
    pub async fn add_user_to_channel(
        &self,
        user: &User,
        channel: &Channel,
        skip_team_check: bool,
    ) -> ApiResult<(ChannelMember, bool)> {
        if !skip_team_check {
            let tm =
                perm::team_members(self, "tm.teamid = $1 AND tm.userid = $2", &[&channel.team_id, &user.id]).await?;
            let Some(tm) = tm.first() else {
                return Err(AppError::not_found(
                    "app.team.get_member.missing.app_error",
                    "No team member found for that user ID and team ID.",
                ));
            };
            if tm.delete_at > 0 {
                return Err(bad(
                    "api.channel.add_user.to.channel.failed.deleted.app_error",
                    "Failed to add user to channel because they have been removed from the team.",
                ));
            }
        }
        let (m, added) = self.add_user_to_channel_row(user, channel).await?;
        if channel.is_space() {
            return Ok((m, added));
        }
        let mut omit = BTreeMap::new();
        omit.insert(user.id.clone(), true);
        let data = json!({"user_id": user.id, "team_id": channel.team_id});
        self.hub.publish(
            "user_added",
            data.clone(),
            Broadcast { channel_id: channel.id.clone(), omit_users: Some(omit), ..Default::default() },
        );
        self.hub.publish(
            "user_added",
            data,
            Broadcast { channel_id: channel.id.clone(), user_id: user.id.clone(), ..Default::default() },
        );
        Ok((m, added))
    }

    /// `AddChannelMember`: an existing membership as it is; else added, with the "joined" (`actor_id` empty or the
    /// user) or "added by" system message.
    pub async fn add_channel_member(
        &self,
        channel: &Channel,
        user_id: &str,
        actor_id: &str,
        post_root_id: &str,
    ) -> ApiResult<ChannelMember> {
        if let Some(m) = self.channel_member(&channel.id, user_id).await? {
            return Ok(m);
        }
        let user = self.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
        if user.delete_at > 0 {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "app.channel.add_member.deleted_user.app_error",
                "Unable to add the user as a member of the channel.",
            ));
        }
        let actor = if actor_id.is_empty() { None } else { self.user(actor_id).await? };
        if !actor_id.is_empty() && actor.is_none() {
            return Err(crate::users::missing_account());
        }
        let (m, added) = self.add_user_to_channel(&user, channel, false).await?;
        if channel.is_space() || !added {
            return Ok(m);
        }
        if actor_id.is_empty() || actor_id == user_id {
            self.post_join_message(&user, channel).await;
        } else if let Some(actor) = actor {
            self.post_add_message(&actor, &user, channel, post_root_id).await;
        }
        Ok(m)
    }

    /// `JoinChannel`: a public channel only.
    pub async fn join_channel(&self, channel: &Channel, user_id: &str) -> ApiResult<()> {
        let user = self.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
        if self.channel_member(&channel.id, user_id).await?.is_some() {
            return Ok(());
        }
        if channel.r#type != OPEN {
            return Err(bad(
                "api.channel.join_channel.permissions.app_error",
                "You do not have the appropriate permissions.",
            ));
        }
        if self.add_user_to_channel(&user, channel, false).await?.1 {
            self.post_join_message(&user, channel).await;
        }
        Ok(())
    }

    async fn post_join_message(&self, user: &User, channel: &Channel) {
        let (message, kind) = if user.is_guest() {
            (format!("{} joined the channel as guest.", user.username), "system_guest_join_channel")
        } else {
            (format!("{} joined the channel.", user.username), "system_join_channel")
        };
        self.post_system(system_post(&channel.id, &user.id, kind, message, json!({"username": user.username}))).await;
    }

    async fn post_add_message(&self, actor: &User, added: &User, channel: &Channel, _root_id: &str) {
        let (message, kind) = if added.is_guest() {
            (
                format!("{} added to the channel as a guest by {}.", added.username, actor.username),
                "system_add_guest_to_chan",
            )
        } else {
            (format!("{} added to the channel by {}.", added.username, actor.username), "system_add_to_channel")
        };
        self.post_system(system_post(
            &channel.id,
            &actor.id,
            kind,
            message,
            json!({"userId": actor.id, "username": actor.username, "addedUserId": added.id, "addedUsername": added.username}),
        ))
        .await;
    }

    /// `removeUserFromChannel`: membership, thread memberships, history, caches and `user_removed`.
    pub(crate) async fn remove_user_from_channel_row(
        &self,
        user: &User,
        remover_id: &str,
        channel: &Channel,
    ) -> ApiResult<()> {
        if channel.name == DEFAULT_CHANNEL && !user.is_guest() {
            return Err(bad(
                "api.channel.remove.default.app_error",
                &format!("Unable to remove user from the default channel {DEFAULT_CHANNEL}."),
            ));
        }
        self.channel_member_or_404(&channel.id, &user.id).await?;
        sqlx::query("DELETE FROM ChannelMembers WHERE ChannelId = $1 AND UserId = $2")
            .bind(&channel.id)
            .bind(&user.id)
            .execute(&self.db)
            .await?;
        self.caches.member_count.remove(&channel.id);
        sqlx::query(
            "DELETE FROM ThreadMemberships WHERE UserId = $1
             AND EXISTS (SELECT 1 FROM Threads WHERE Threads.PostId = ThreadMemberships.PostId AND Threads.ChannelId = $2)",
        )
        .bind(&user.id)
        .bind(&channel.id)
        .execute(&self.db)
        .await?;
        self.log_leave(&user.id, &channel.id).await?;
        self.invalidate_channel_cache_for_user(&user.id);
        self.invalidate_channel_members_cache(&channel.id);
        self.caches.members_notify_props.remove(&channel.id);
        // Space backing channels are internal: no `user_removed`.
        if channel.is_space() {
            return Ok(());
        }
        self.hub.publish(
            "user_removed",
            json!({"user_id": user.id, "remover_id": remover_id}),
            Broadcast { channel_id: channel.id.clone(), ..Default::default() },
        );
        self.hub.publish(
            "user_removed",
            json!({"channel_id": channel.id, "remover_id": remover_id}),
            Broadcast { user_id: user.id.clone(), ..Default::default() },
        );
        Ok(())
    }

    /// `RemoveUserFromChannel`: with the "left" or "removed" system message.
    pub async fn remove_user_from_channel(&self, user_id: &str, remover_id: &str, channel: &Channel) -> ApiResult<()> {
        let user = self.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
        self.remove_user_from_channel_row(&user, remover_id, channel).await?;
        // Space backing channels are internal: no leave or remove system message.
        if channel.is_space() {
            return Ok(());
        }
        if user_id == remover_id {
            self.post_leave_message(&user, channel).await;
        } else {
            self.post_system(system_post(
                &channel.id,
                remover_id,
                "system_remove_from_channel",
                format!("@{} removed from the channel.", user.username),
                json!({"removedUserId": user.id, "removedUsername": user.username}),
            ))
            .await;
        }
        Ok(())
    }

    /// `LeaveChannel`.
    pub async fn leave_channel(&self, channel_id: &str, user_id: &str) -> ApiResult<()> {
        let channel = self.get_channel(channel_id).await?;
        let user = self.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
        if channel.is_group_or_direct() {
            return Err(bad("api.channel.leave.direct.app_error", "Unable to leave a direct message channel."));
        }
        self.remove_user_from_channel_row(&user, user_id, &channel).await?;
        if channel.name == DEFAULT_CHANNEL
            && !self.cfg.bool("ServiceSettings.ExperimentalEnableDefaultChannelLeaveJoinMessages")
        {
            return Ok(());
        }
        self.post_leave_message(&user, &channel).await;
        Ok(())
    }

    async fn post_leave_message(&self, user: &User, channel: &Channel) {
        self.post_system(system_post(
            &channel.id,
            &user.id,
            "system_leave_channel",
            format!("@{} left the channel.", user.username),
            json!({"username": user.username}),
        ))
        .await;
    }

    /// `UpdateChannelMemberNotifyProps`: the known props merged into the member's.
    pub async fn update_member_notify_props(
        &self,
        data: &Map<String, Value>,
        channel_id: &str,
        user_id: &str,
    ) -> ApiResult<ChannelMember> {
        const KEYS: [&str; 10] = [
            "mark_unread",
            "desktop",
            "desktop_sound",
            "desktop_notification_sound",
            "desktop_threads",
            "email",
            "push",
            "push_threads",
            "ignore_channel_mentions",
            "channel_auto_follow_threads",
        ];
        let mut props = BTreeMap::new();
        for k in KEYS {
            if let Some(v) = data.get(k) {
                props.insert(k.to_string(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
            }
        }
        validate_member_notify_props(&props)?;
        let j = serde_json::to_string(&props).unwrap();
        if j.chars().count() > 800 {
            return Err(bad(
                "app.channel.update_member.notify_props_limit_exceeded.app_error",
                "Unable to update the channel member, notify props size limit exceeded.",
            ));
        }
        let n = sqlx::query(
            "UPDATE ChannelMembers SET NotifyProps = NotifyProps || $1::jsonb, LastUpdateAt = $2
             WHERE UserId = $3 AND ChannelId = $4",
        )
        .bind(&j)
        .bind(now_ms())
        .bind(user_id)
        .bind(channel_id)
        .execute(&self.db)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(member_not_found());
        }
        let m = self.channel_member_or_404(channel_id, user_id).await?;
        self.invalidate_channel_cache_for_user(user_id);
        self.caches.members_notify_props.remove(channel_id);
        self.publish_member_updated(&m);
        Ok(m)
    }

    /// `UpdateChannelMemberSchemeRoles`.
    pub async fn update_member_scheme_roles(
        &self,
        channel_id: &str,
        user_id: &str,
        guest: bool,
        user: bool,
        admin: bool,
    ) -> ApiResult<ChannelMember> {
        let mut m = self.channel_member_or_404(channel_id, user_id).await?;
        if m.scheme_guest {
            return Err(bad(
                "api.channel.update_channel_member_roles.guest.app_error",
                "Invalid channel member update: A guest cannot be made team member or team admin, please promote as a user first.",
            ));
        }
        if guest {
            return Err(bad(
                "api.channel.update_channel_member_roles.user_and_guest.app_error",
                "Invalid channel member update: A guest cannot be set for a single channel, a System Admin must promote or demote users to/from guests.",
            ));
        }
        if !user {
            return Err(bad(
                "api.channel.update_channel_member_roles.unset_user_scheme.app_error",
                "Invalid channel member update: A channel member always has a user scheme.",
            ));
        }
        (m.scheme_admin, m.scheme_user, m.scheme_guest) = (admin, user, guest);
        self.update_channel_member(&m).await
    }

    /// `setChannelsMuted`: `mark_unread` of the user's memberships set to `mention` or `all`.
    pub async fn set_channels_muted(&self, channel_ids: &[String], user_id: &str, muted: bool) -> ApiResult<()> {
        let members =
            self.channel_members_where("cm.userid = $1 AND cm.channelid = ANY($2)", user_id, channel_ids).await?;
        for mut m in members {
            let is_muted = m.notify_props.0.get("mark_unread").map(String::as_str) == Some("mention");
            if is_muted == muted {
                continue;
            }
            m.notify_props.0.insert("mark_unread".into(), if muted { "mention" } else { "all" }.into());
            let m = self.update_member(&m).await?;
            self.caches.members_notify_props.remove(&m.channel_id);
            self.publish_member_updated(&m);
        }
        Ok(())
    }

    // ---------- viewing ----------

    /// `MarkChannelsAsViewed` with `collapsed_threads_supported` (contract: posts call it for the poster).
    pub async fn mark_viewed(
        &self,
        ids: &[String],
        user_id: &str,
        session_id: &str,
    ) -> ApiResult<BTreeMap<String, i64>> {
        let crt = self.is_crt_enabled(user_id).await;
        self.mark_channels_viewed(ids, user_id, session_id, true, crt).await
    }

    /// `MarkChannelsAsViewed`: counters reset where there is something unread, `multiple_channels_viewed`, and
    /// the channels' threads read when the client does not handle collapsed threads.
    pub async fn mark_channels_viewed(
        &self,
        ids: &[String],
        user_id: &str,
        session_id: &str,
        crt_supported: bool,
        crt_enabled: bool,
    ) -> ApiResult<BTreeMap<String, i64>> {
        // GetChannelsWithUnreadsAndWithMentions: the channels to view, and those whose push notifications the
        // user's other devices drop.
        let user_push = self.user(user_id).await?.map(|u| u.notify_prop("push").to_string()).unwrap_or_default();
        type Row = (String, i64, i64, i64, i64, i64, String, Option<String>);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT Channels.Id, COALESCE(Channels.TotalMsgCount, 0), COALESCE(Channels.LastPostAt, 0),
                    COALESCE(ChannelMembers.MsgCount, 0), COALESCE(ChannelMembers.MentionCount, 0),
                    COALESCE(ChannelMembers.LastViewedAt, 0), COALESCE(Channels.Type::text, ''), ChannelMembers.NotifyProps->>'push'
             FROM ChannelMembers INNER JOIN Channels ON ChannelMembers.ChannelId = Channels.Id
             WHERE ChannelMembers.ChannelId = ANY($1) AND ChannelMembers.UserId = $2 AND Channels.Type <> 'S'",
        )
        .bind(ids)
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        let mut times = BTreeMap::new();
        let mut to_view = vec![];
        let mut to_clear = vec![];
        for (id, total, last_post, msg, mentions, last_viewed, kind, push) in rows {
            let unread = total - msg > 0 || mentions > 0;
            if unread {
                to_view.push(id.clone());
            }
            let mut notify = push.unwrap_or_default();
            if notify == "default" {
                notify = user_push.clone();
            }
            if (notify == "all" || kind == "D") && unread || notify == "mention" && mentions > 0 {
                to_clear.push(id.clone());
            }
            times.insert(id, last_post.max(last_viewed));
        }
        if to_view.is_empty() {
            return Ok(times);
        }
        let update_threads = self.cfg.bool("ServiceSettings.ThreadAutoFollow") && (!crt_supported || !crt_enabled);
        if update_threads {
            let now = now_ms();
            sqlx::query(
                "UPDATE ThreadMemberships SET LastViewed = $1, UnreadMentions = 0, LastUpdated = $1
                 FROM Threads WHERE ThreadMemberships.UserId = $2 AND Threads.PostId = ThreadMemberships.PostId
                 AND Threads.ChannelId = ANY($3) AND Threads.LastReplyAt > ThreadMemberships.LastViewed",
            )
            .bind(now)
            .bind(user_id)
            .bind(&to_view)
            .execute(&self.db)
            .await?;
        }
        // UpdateLastViewedAt
        sqlx::query(
            "WITH c AS (SELECT Id, LastPostAt, TotalMsgCount, TotalMsgCountRoot FROM Channels WHERE Id = ANY($1)),
             updated AS (UPDATE ChannelMembers cm SET MentionCount = 0, MentionCountRoot = 0, UrgentMentionCount = 0,
                 MsgCount = greatest(cm.MsgCount, c.TotalMsgCount), MsgCountRoot = greatest(cm.MsgCountRoot, c.TotalMsgCountRoot),
                 LastViewedAt = greatest(cm.LastViewedAt, c.LastPostAt), LastUpdateAt = greatest(cm.LastViewedAt, c.LastPostAt)
                 FROM c WHERE cm.UserId = $2 AND c.Id = cm.ChannelId)
             SELECT Id, LastPostAt FROM c",
        )
        .bind(&to_view)
        .bind(user_id)
        .execute(&self.db)
        .await?;
        if self.cfg.bool("ServiceSettings.EnableChannelViewedMessages") {
            self.hub.publish(
                "multiple_channels_viewed",
                json!({"channel_times": times}),
                Broadcast { user_id: user_id.into(), ..Default::default() },
            );
        }
        for id in &to_clear {
            crate::push::clear(self, session_id, user_id, id, "").await;
        }
        if update_threads && crt_enabled {
            let ts = now_ms();
            for id in &to_view {
                self.hub.publish(
                    "thread_read_changed",
                    json!({"timestamp": ts}),
                    Broadcast { channel_id: id.clone(), user_id: user_id.into(), ..Default::default() },
                );
            }
        }
        Ok(times)
    }

    /// `SetActiveChannel`: the status cache knows the channel; an automatic status goes online.
    pub async fn set_active_channel(&self, user_id: &str, channel_id: &str) {
        let now = now_ms();
        let (s, old) = match self.status(user_id).await.ok().flatten() {
            None => (
                crate::model::Status {
                    user_id: user_id.into(),
                    status: crate::model::status::ONLINE.into(),
                    last_activity_at: now,
                    active_channel: channel_id.into(),
                    ..Default::default()
                },
                crate::model::status::OFFLINE.to_string(),
            ),
            Some(mut s) => {
                let old = s.status.clone();
                s.active_channel = channel_id.into();
                if !s.manual && !channel_id.is_empty() {
                    s.status = crate::model::status::ONLINE.into();
                }
                s.last_activity_at = now;
                (s, old)
            }
        };
        self.caches.status.put(user_id.into(), s.clone());
        if s.status != old {
            self.broadcast_status(&s);
        }
    }

    // ---------- channel changes ----------

    /// `PatchChannel` after the handler's checks: patched, saved, and the system messages for what changed.
    pub async fn patch_channel(&self, mut c: Channel, patch: &Map<String, Value>, user_id: &str) -> ApiResult<Channel> {
        let (old_display, old_header, old_purpose) = (c.display_name.clone(), c.header.clone(), c.purpose.clone());
        let s = |k: &str| patch.get(k).and_then(Value::as_str).map(str::to_string);
        if let Some(v) = s("display_name") {
            c.display_name = v.trim().into();
        }
        if let Some(v) = s("name") {
            c.name = v;
        }
        if let Some(v) = s("header") {
            c.header = v;
        }
        if let Some(v) = s("purpose") {
            c.purpose = v;
        }
        if let Some(v) = patch.get("group_constrained").and_then(Value::as_bool) {
            c.group_constrained = Some(v);
        }
        if let Some(b) = patch.get("banner_info").and_then(Value::as_object) {
            let mut cur = c.banner_info.take().map(|j| j.0).unwrap_or_else(|| json!({}));
            if let Some(obj) = cur.as_object_mut() {
                for k in ["enabled", "text", "background_color", "attribute_banner_disabled"] {
                    if let Some(v) = b.get(k).filter(|v| !v.is_null()) {
                        obj.insert(k.into(), v.clone());
                    }
                }
            }
            c.banner_info = Some(Json(cur));
        }
        if let Some(v) = patch.get("autotranslation").and_then(Value::as_bool) {
            c.auto_translation = v;
        }
        if let Some(v) = s("default_category_name") {
            c.default_category_name = v.trim().into();
        }
        if let Some(v) = patch.get("discoverable").and_then(Value::as_bool) {
            c.discoverable = v;
        }
        if let Some(v) = patch.get("disable_join_leave_messages").and_then(Value::as_bool) {
            c.disable_join_leave_messages = v;
        }
        self.update_channel(&mut c).await?;
        let user = self.user(user_id).await?;
        if let Some(user) = user {
            let name = &user.username;
            if old_display != c.display_name {
                self.post_system(system_post(
                    &c.id,
                    user_id,
                    "system_displayname_change",
                    format!("{name} updated the channel display name from: {old_display} to: {}", c.display_name),
                    json!({"username": name, "old_displayname": old_display, "new_displayname": c.display_name}),
                ))
                .await;
            }
            if old_header != c.header {
                let message = if old_header.is_empty() {
                    format!("{name} updated the channel header to: {}", c.header)
                } else if c.header.is_empty() {
                    format!("{name} removed the channel header (was: {old_header})")
                } else {
                    format!("{name} updated the channel header from: {old_header} to: {}", c.header)
                };
                self.post_system(system_post(
                    &c.id,
                    user_id,
                    "system_header_change",
                    message,
                    json!({"username": name, "old_header": old_header, "new_header": c.header}),
                ))
                .await;
            }
            if old_purpose != c.purpose {
                let message = if old_purpose.is_empty() {
                    format!("{name} updated the channel purpose to: {}", c.purpose)
                } else if c.purpose.is_empty() {
                    format!("{name} removed the channel purpose (was: {old_purpose})")
                } else {
                    format!("{name} updated the channel purpose from: {old_purpose} to: {}", c.purpose)
                };
                self.post_system(system_post(
                    &c.id,
                    user_id,
                    "system_purpose_change",
                    message,
                    json!({"username": name, "old_purpose": old_purpose, "new_purpose": c.purpose}),
                ))
                .await;
            }
        }
        Ok(c)
    }

    /// `UpdateChannelPrivacy`: the new type saved, the system message, `channel_converted` to the team.
    pub async fn update_channel_privacy(&self, mut c: Channel, user: &User) -> ApiResult<Channel> {
        if c.is_space() {
            return Err(bad("app.channel.update_channel_privacy.space.app_error", ""));
        }
        if c.r#type == OPEN {
            c.discoverable = false;
        }
        self.update_channel(&mut c).await?;
        let message = if c.r#type == OPEN {
            "This channel has been converted to a Public Channel and can be joined by any team member."
        } else {
            "This channel has been converted to a Private Channel."
        };
        self.post_system(system_post(
            &c.id,
            &user.id,
            "system_change_chan_privacy",
            message.into(),
            json!({"username": user.username}),
        ))
        .await;
        self.invalidate_channel_cache(&c);
        self.hub.publish(
            "channel_converted",
            json!({"channel_id": c.id, "channel_type": c.r#type}),
            Broadcast { team_id: c.team_id.clone(), ..Default::default() },
        );
        Ok(c)
    }

    /// `DeleteChannel` (archive): webhooks archived, the system message, `channel_deleted`.
    pub async fn delete_channel(&self, c: &Channel, user_id: &str) -> ApiResult<()> {
        let user = self.user(user_id).await?;
        if c.delete_at > 0 {
            return Err(bad(
                "api.channel.delete_channel.deleted.app_error",
                "The channel has been archived or deleted.",
            ));
        }
        if c.name == DEFAULT_CHANNEL {
            return Err(bad(
                "api.channel.delete_channel.cannot.app_error",
                &format!("Unable to delete the default channel {DEFAULT_CHANNEL}."),
            ));
        }
        let delete_at = now_ms();
        self.set_delete_at(&c.id, delete_at, delete_at).await?;
        self.invalidate_channel_cache(c);
        // Space backing channels are internal: no archive system message.
        if let Some(user) = user.filter(|_| !c.is_space()) {
            // With the channel as it was before the archive, as Go's CreatePost gets it: SendNotifications sends
            // nothing in an archived channel, so a reloaded channel would leave the members without `posted`.
            let p = system_post(
                &c.id,
                user_id,
                "system_channel_deleted",
                format!("{} archived the channel.", user.username),
                json!({"username": user.username}),
            );
            let flags = crate::posts::CreateFlags { set_online: true, ..Default::default() };
            if let Err(e) = self.create_post_in(p, c, flags, None).await {
                eprintln!("failed to post the archive message: {} {}", e.id, e.detailed_error);
            }
        }
        let now = now_ms();
        sqlx::query("UPDATE IncomingWebhooks SET DeleteAt = $1, UpdateAt = $1 WHERE ChannelId = $2 AND DeleteAt = 0")
            .bind(now)
            .bind(&c.id)
            .execute(&self.db)
            .await?;
        sqlx::query("UPDATE OutgoingWebhooks SET DeleteAt = $1, UpdateAt = $1 WHERE ChannelId = $2 AND DeleteAt = 0")
            .bind(now)
            .bind(&c.id)
            .execute(&self.db)
            .await?;
        sqlx::query(
            "UPDATE PersistentNotifications SET DeleteAt = $1 WHERE DeleteAt = 0
             AND PostId IN (SELECT Id FROM Posts WHERE ChannelId = $2)",
        )
        .bind(now)
        .bind(&c.id)
        .execute(&self.db)
        .await?;
        self.invalidate_channel_cache(c);
        if c.is_space() {
            return Ok(());
        }
        let b = if c.r#type == OPEN {
            Broadcast { team_id: c.team_id.clone(), ..Default::default() }
        } else {
            Broadcast { channel_id: c.id.clone(), ..Default::default() }
        };
        self.hub.publish("channel_deleted", json!({"channel_id": c.id, "delete_at": delete_at}), b);
        Ok(())
    }

    /// `RestoreChannel` (unarchive).
    pub async fn restore_channel(&self, mut c: Channel, user_id: &str) -> ApiResult<Channel> {
        if c.delete_at == 0 {
            return Err(bad(
                "api.channel.restore_channel.restored.app_error",
                "Unable to unarchive channel. The channel is not archived.",
            ));
        }
        self.set_delete_at(&c.id, 0, now_ms()).await?;
        c.delete_at = 0;
        self.invalidate_channel_cache(&c);
        // Space backing channels are internal: no `channel_restored` and no system message.
        if c.is_space() {
            return Ok(c);
        }
        let b = if c.r#type == OPEN {
            Broadcast { team_id: c.team_id.clone(), ..Default::default() }
        } else {
            Broadcast { channel_id: c.id.clone(), ..Default::default() }
        };
        self.hub.publish("channel_restored", json!({"channel_id": c.id}), b);
        let user = self.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
        self.post_system(system_post(
            &c.id,
            user_id,
            "system_channel_restored",
            format!("{} unarchived the channel.", user.username),
            json!({"username": user.username}),
        ))
        .await;
        Ok(c)
    }

    /// `SetDeleteAt`: the channel and its `PublicChannels` row.
    async fn set_delete_at(&self, channel_id: &str, delete_at: i64, update_at: i64) -> ApiResult<()> {
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE Channels SET DeleteAt = $1, UpdateAt = $2 WHERE Id = $3")
            .bind(delete_at)
            .bind(update_at)
            .bind(channel_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE PublicChannels SET DeleteAt = $1 WHERE Id = $2")
            .bind(delete_at)
            .bind(channel_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if let Some(c) = self.caches.channel.get(channel_id) {
            self.invalidate_channel_cache(&c);
        }
        self.caches.channel.remove(channel_id);
        Ok(())
    }

    /// `GetDirectOrGroupMessageMembersCommonTeams` for the asking user.
    pub async fn common_teams(&self, s: &Session, channel_id: &str) -> ApiResult<Vec<Team>> {
        let c = self.get_channel(channel_id).await?;
        if !c.is_group_or_direct() {
            return Err(bad("app.channel.get_common_teams.incorrect_channel_type", "Channel is not a group message."));
        }
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT Users.Id FROM Users JOIN ChannelMembers cm ON cm.UserId = Users.Id
             LEFT JOIN Bots b ON b.UserId = Users.Id
             WHERE cm.ChannelId = $1 AND Users.DeleteAt = 0 ORDER BY Users.Username LIMIT 8",
        )
        .bind(channel_id)
        .fetch_all(&self.db)
        .await?;
        if !ids.contains(&s.user_id) {
            return Ok(vec![]);
        }
        let team_ids: Vec<String> = sqlx::query_scalar(
            "SELECT t.Id FROM Teams AS t JOIN (SELECT TeamId, UserId FROM TeamMembers WHERE UserId = ANY($1) AND DeleteAt = 0) AS tm
             ON t.Id = tm.TeamId WHERE t.DeleteAt = 0 GROUP BY t.Id HAVING COUNT(UserId) = $2",
        )
        .bind(&ids)
        .bind(ids.len() as i64)
        .fetch_all(&self.db)
        .await?;
        if team_ids.is_empty() {
            return Ok(vec![]);
        }
        let mut teams: Vec<Team> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{} WHERE id = ANY($1)", Team::SELECT)))
            .bind(&team_ids)
            .fetch_all(&self.db)
            .await?;
        for t in &mut teams {
            self.sanitize_team(s, t).await;
        }
        Ok(teams)
    }

    /// `GetChannelMembersTimezones`: the members' preferred zones, each once.
    pub async fn channel_timezones(&self, channel_id: &str) -> ApiResult<Option<Vec<String>>> {
        let rows: Vec<Option<Json<HashMap<String, String>>>> = sqlx::query_scalar(
            "SELECT Users.Timezone FROM ChannelMembers LEFT JOIN Users ON ChannelMembers.UserId = Id WHERE ChannelId = $1",
        )
        .bind(channel_id)
        .fetch_all(&self.db)
        .await?;
        let mut out: Vec<String> = vec![];
        for tz in rows.into_iter().flatten() {
            let tz = tz.0;
            let auto = tz.get("automaticTimezone").map(String::as_str).unwrap_or("");
            let manual = tz.get("manualTimezone").map(String::as_str).unwrap_or("");
            if auto.is_empty() && manual.is_empty() {
                continue;
            }
            let pick = if tz.get("useAutomaticTimezone").map(String::as_str) == Some("true") { auto } else { manual };
            if !out.iter().any(|t| t == pick) {
                out.push(pick.to_string());
            }
        }
        Ok(if out.is_empty() { None } else { Some(out) })
    }
}

/// `upsertPublicChannelT`: the search table of open channels.
async fn upsert_public_channel(tx: &mut sqlx::PgConnection, c: &Channel) -> sqlx::Result<()> {
    if c.r#type != OPEN {
        sqlx::query("DELETE FROM PublicChannels WHERE Id = $1").bind(&c.id).execute(&mut *tx).await?;
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO PublicChannels (Id, DeleteAt, TeamId, DisplayName, Name, Header, Purpose)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (Id) DO UPDATE SET DeleteAt = $2, TeamId = $3, DisplayName = $4, Name = $5, Header = $6, Purpose = $7",
    )
    .bind(&c.id)
    .bind(c.delete_at)
    .bind(&c.team_id)
    .bind(&c.display_name)
    .bind(&c.name)
    .bind(&c.header)
    .bind(&c.purpose)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

// ---------- search ----------

/// `sanitizeSearchTerm(term, "*")` and `wildcardSearchTerm`: `%term%` with `%` and `_` escaped by `*`.
pub fn like_term(term: &str) -> Option<String> {
    let mut t = term.replace('*', "");
    for c in ["%", "_"] {
        t = t.replace(c, &format!("*{c}"));
    }
    if t.is_empty() { None } else { Some(format!("%{}%", t.to_lowercase())) }
}

/// `buildFulltextClause`'s term: prefix matches of every word, ANDed.
pub fn fulltext_term(term: &str) -> String {
    const SPACE: &str = "<>+-()~:*\"!@&";
    let t: String = term.chars().map(|c| if SPACE.contains(c) { ' ' } else { c }).collect();
    let t = t.replace('|', "");
    t.split_whitespace().map(|w| format!("{w}:*")).collect::<Vec<_>>().join(" & ")
}

impl App {
    /// `searchClause` + `orderByDisplayNameMatch` as SQL text, with `$n` for the like term (`n`) and the fulltext
    /// term (`n + 1`). None when the term is empty.
    pub(crate) async fn search_sql(&self, n: usize, columns: &[&str]) -> ApiResult<String> {
        let likes: Vec<String> = columns.iter().map(|c| format!("LOWER({c}) LIKE LOWER(${n}) ESCAPE '*'")).collect();
        let cfg = self.text_search_config().await?;
        Ok(format!(
            "({} OR ((to_tsvector('{cfg}', {})) @@ to_tsquery('{cfg}', ${})))",
            likes.join(" OR "),
            columns.join(" || ' ' || "),
            n + 1
        ))
    }

    /// `SHOW default_text_search_config`, read once as Go's store does at start.
    async fn text_search_config(&self) -> ApiResult<String> {
        static CFG: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
        let c = CFG
            .get_or_try_init(|| async {
                sqlx::query_scalar::<_, String>("SHOW default_text_search_config").fetch_one(&self.db).await
            })
            .await?;
        Ok(c.replace('\'', ""))
    }

    /// `AutocompleteInTeam` (and `...Filtered`): the team's channels the user may see matching `term`, archived
    /// ones too, at most 50.
    pub async fn autocomplete_in_team(
        &self,
        team_id: &str,
        user: &User,
        term: &str,
        private_only: bool,
        exclude_group_constrained: bool,
    ) -> ApiResult<Vec<Channel>> {
        let term = term.trim();
        let mut sql = format!(
            "SELECT {} FROM Channels c WHERE c.TeamId = $1 AND c.Type IN {MESSAGE_TYPES}",
            db::cols::<Channel>("c")
        );
        if user.is_guest() {
            sql += " AND c.Id IN (SELECT ChannelId FROM ChannelMembers WHERE UserId = $2)";
        } else {
            sql += " AND (c.Type <> 'P' OR c.Id IN (SELECT ChannelId FROM ChannelMembers WHERE UserId = $2)
                      OR (c.Type = 'P' AND c.Discoverable = true))";
        }
        let like = like_term(term);
        if like.is_some() {
            sql += &format!(" AND {}", self.search_sql(3, &["c.Name", "c.DisplayName", "c.Purpose"]).await?);
        }
        if private_only {
            sql += " AND c.Type = 'P' AND (c.Shared IS NULL OR c.Shared = false)";
        }
        if exclude_group_constrained {
            sql += " AND (c.GroupConstrained = false OR c.GroupConstrained IS NULL)";
        }
        sql += match like {
            Some(_) => {
                " ORDER BY CASE WHEN LOWER(c.DisplayName) LIKE LOWER($3) ESCAPE '*' THEN 0 ELSE 1 END, c.DisplayName LIMIT 50"
            }
            None => " ORDER BY c.DisplayName LIMIT 50",
        };
        let mut q = sqlx::query_as::<_, Channel>(sqlx::AssertSqlSafe(sql)).bind(team_id).bind(&user.id);
        if let Some(l) = &like {
            q = q.bind(l).bind(fulltext_term(term));
        }
        Ok(q.fetch_all(&self.db).await?)
    }

    /// `Autocomplete`: in every team of the user, with the team's data.
    pub async fn autocomplete_all(&self, user: &User, term: &str) -> ApiResult<Vec<WithTeam<Channel>>> {
        let term = term.trim();
        let mut sql = format!(
            "SELECT {}, t.DisplayName AS teamdisplayname, t.Name AS teamname, t.UpdateAt AS teamupdateat
             FROM Channels c, Teams t, TeamMembers tm
             WHERE c.TeamId = t.id AND t.id = tm.TeamId AND tm.UserId = $1 AND c.Type IN {MESSAGE_TYPES} AND tm.DeleteAt = 0",
            db::cols::<Channel>("c")
        );
        if user.is_guest() {
            sql += " AND c.Id IN (SELECT ChannelId FROM ChannelMembers WHERE UserId = $1)";
        } else {
            sql += " AND (c.Type <> 'P' OR c.Id IN (SELECT ChannelId FROM ChannelMembers WHERE UserId = $1)
                      OR (c.Type = 'P' AND c.Discoverable = true))";
        }
        let like = like_term(term);
        if like.is_some() {
            sql += &format!(" AND {}", self.search_sql(2, &["c.Name", "c.DisplayName", "c.Purpose"]).await?);
        }
        sql += match like {
            Some(_) => {
                " ORDER BY CASE WHEN LOWER(c.DisplayName) LIKE LOWER($2) ESCAPE '*' THEN 0 ELSE 1 END, c.DisplayName LIMIT 50"
            }
            None => " ORDER BY c.DisplayName LIMIT 50",
        };
        let mut q = sqlx::query_as::<_, WithTeam<Channel>>(sqlx::AssertSqlSafe(sql)).bind(&user.id);
        if let Some(l) = &like {
            q = q.bind(l).bind(fulltext_term(term));
        }
        Ok(q.fetch_all(&self.db).await?)
    }

    /// `SearchGroupChannels`: the user's group messages whose members' usernames contain every word.
    pub async fn search_group_channels(&self, user_id: &str, term: &str) -> ApiResult<Vec<Channel>> {
        let words: Vec<String> = term
            .to_lowercase()
            .split_whitespace()
            .map(|w| {
                let mut w = w.replace('\\', "");
                for c in ["%", "_"] {
                    w = w.replace(c, &format!("\\{c}"));
                }
                format!("%{w}%")
            })
            .collect();
        // Go's empty `HAVING` matches nothing.
        if words.is_empty() {
            return Ok(vec![]);
        }
        let parts: Vec<String> =
            (0..words.len()).map(|i| format!("ARRAY_TO_STRING(ARRAY_AGG(u.Username), ', ') LIKE ${}", i + 2)).collect();
        let having = format!("HAVING {}", parts.join(" AND "));
        let sql = format!(
            "SELECT {} FROM Channels WHERE Id IN (
                SELECT cc.id FROM (SELECT c.Id FROM Channels c JOIN ChannelMembers cm ON c.Id = cm.ChannelId
                    JOIN Users u ON u.Id = cm.UserId WHERE c.Type = 'G' AND u.Id = $1 GROUP BY c.Id) cc
                JOIN ChannelMembers cm ON cc.Id = cm.ChannelId JOIN Users u ON u.Id = cm.UserId
                GROUP BY cc.Id {having} LIMIT 50)",
            db::cols::<Channel>("Channels")
        );
        let mut q = sqlx::query_as::<_, Channel>(sqlx::AssertSqlSafe(sql)).bind(user_id);
        for w in &words {
            q = q.bind(w);
        }
        Ok(q.fetch_all(&self.db).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_names() {
        for ok in ["town-square", "a", "bench-30", "a_b", "a__b", "ab9"] {
            assert!(is_valid_channel_name(ok), "{ok}");
        }
        for bad in ["", "-a", "A", "a b", "é"] {
            assert!(!is_valid_channel_name(bad), "{bad}");
        }
    }

    #[test]
    fn mentions_need_a_boundary() {
        assert_eq!(channel_mentions("see ~bench-30 and ~Off_Topic, not a~b"), vec!["bench-30", "off_topic"]);
        assert!(channel_mentions("~").is_empty());
    }

    #[test]
    fn group_name_is_sha1_of_sorted_ids() {
        let a = group_name(&["b".into(), "a".into()]);
        assert_eq!(a, group_name(&["a".into(), "b".into()]));
        assert_eq!(a, "da23614e02469a0d7c7bd1bdab5c9c474b1904dc"); // sha1("ab")
    }

    #[test]
    fn search_terms() {
        assert_eq!(like_term("a%b_c*").as_deref(), Some("%a*%b*_c%"));
        assert_eq!(like_term(""), None);
        assert_eq!(fulltext_term("foo-bar baz|"), "foo:* & bar:* & baz:*");
    }
}
