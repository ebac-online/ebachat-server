//! Posts: the post store with Go's post caches (last post time, first pages), creating, reading, editing,
//! deleting and pinning, and what `PreparePostForClient` adds.

// Based on Mattermost server/channels/api4/post.go, app/post.go, app/post_metadata.go,
// store/sqlstore/post_store.go and store/localcachelayer/post_layer.go, Copyright (c) 2015-present
// Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};

use crate::app::Query;
use axum::extract::{Path, State as St};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::app::{ApiResult, App, AppError, Auth, Body, BusyAuth, State, is_valid_id, now_ms};
use crate::db::{self, Table};
use crate::hub::Broadcast;
use crate::model::post::{EmbedData, PostEmbed, PreviewPost, kind};
use crate::model::{
    Channel, Post, PostAcknowledgement, PostList, PostMetadata, PostPriority, Session, Show, User, channel,
};
use crate::perm;
use crate::users::ok;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/posts", post(create_post_h))
        .route("/api/v4/posts/ids", post(posts_by_ids))
        .route("/api/v4/posts/{post_id}", get(get_post).put(update_post_h).delete(delete_post_h))
        .route("/api/v4/posts/{post_id}/patch", put(patch_post_h))
        .route("/api/v4/posts/{post_id}/thread", get(get_post_thread))
        .route("/api/v4/posts/{post_id}/edit_history", get(edit_history))
        .route("/api/v4/posts/{post_id}/restore/{restore_version_id}", post(restore_version))
        .route("/api/v4/posts/{post_id}/pin", post(pin_post))
        .route("/api/v4/posts/{post_id}/unpin", post(unpin_post))
        .route("/api/v4/posts/{post_id}/info", get(post_info))
        .route("/api/v4/channels/{channel_id}/posts", get(posts_for_channel))
        .route("/api/v4/channels/{channel_id}/pinned", get(pinned_posts))
        .route("/api/v4/users/{user_id}/posts/flagged", get(flagged_posts))
        .route("/api/v4/users/{user_id}/posts/{post_id}/set_unread", post(set_unread))
        .route("/api/v4/users/{user_id}/posts/{post_id}/reminder", post(set_reminder))
        .route("/api/v4/users/{user_id}/channels/{channel_id}/posts/unread", get(posts_around_unread))
        .route("/api/v4/users/{user_id}/typing", post(user_typing))
        .route("/api/v4/notifications/test", post(test_notification))
        .merge(crate::threads::routes())
        .merge(crate::reactions::routes())
        .merge(crate::drafts::routes())
        .merge(crate::search::routes())
        .merge(crate::posts_more::routes())
}

// ---------- rows ----------

/// A post and its `ReplyCount` column.
pub struct WithCount(pub Post);

impl<'r> sqlx::FromRow<'r, PgRow> for WithCount {
    fn from_row(row: &'r PgRow) -> sqlx::Result<Self> {
        let mut p = Post::from_row(row)?;
        p.reply_count = row.try_get::<Option<i64>, _>("replycount")?.unwrap_or(0);
        Ok(WithCount(p))
    }
}

/// `postWithExtra`: a root post with its thread (collapsed threads).
pub struct WithThread {
    pub post: Post,
    pub participants: Vec<String>,
}

impl<'r> sqlx::FromRow<'r, PgRow> for WithThread {
    fn from_row(row: &'r PgRow) -> sqlx::Result<Self> {
        let mut post = Post::from_row(row)?;
        post.reply_count = row.try_get::<Option<i64>, _>("threadreplycount")?.unwrap_or(0);
        post.last_reply_at = row.try_get::<Option<i64>, _>("threadlastreplyat")?.unwrap_or(0);
        post.is_following = row.try_get("isfollowing")?;
        let participants: Option<sqlx::types::Json<Vec<String>>> = row.try_get("threadparticipants")?;
        Ok(WithThread { post, participants: participants.map(|p| p.0).unwrap_or_default() })
    }
}

/// The thread columns of a collapsed-threads query; `t` Threads, `tm` the user's ThreadMemberships.
const CRT_COLS: &str = "COALESCE(t.ReplyCount, 0) AS threadreplycount, COALESCE(t.LastReplyAt, 0) AS threadlastreplyat, \
     COALESCE(t.Participants, '[]') AS threadparticipants, tm.Following AS isfollowing";

fn crt_joins(alias: &str) -> String {
    format!(
        "LEFT JOIN Threads t ON t.PostId = {alias}.Id \
         LEFT JOIN ThreadMemberships tm ON tm.PostId = {alias}.Id AND tm.UserId = $1"
    )
}

/// `ReplyCount` of the post's thread, as a column.
fn reply_count_col(alias: &str, include_deleted: bool) -> String {
    let del = if include_deleted { "" } else { " AND rc.DeleteAt = 0" };
    format!(
        "(SELECT COUNT(*) FROM Posts rc WHERE rc.RootId = (CASE WHEN {alias}.RootId = '' THEN {alias}.Id ELSE {alias}.RootId END){del}) AS replycount"
    )
}

/// `membershipSystemPostsExcludeSQLClause`.
fn exclude_membership(alias: &str, exclude: bool) -> String {
    if !exclude {
        return String::new();
    }
    let types: Vec<String> = kind::JOIN_LEAVE_TYPES
        .iter()
        .chain([kind::GUEST_JOIN_CHANNEL, kind::ADD_GUEST_TO_CHANNEL].iter())
        .map(|t| format!("'{t}'"))
        .collect();
    format!(" AND {alias}.Type NOT IN ({})", types.join(", "))
}

/// `ShouldChannelExcludeMembershipSystemPosts`.
pub fn excludes_membership(c: &Channel) -> bool {
    !c.is_group_or_direct() && c.disable_join_leave_messages
}

fn pc(alias: &str) -> String {
    db::cols::<Post>(alias)
}

// ---------- options ----------

#[derive(Default, Clone)]
pub struct GetPostsOpts {
    pub channel_id: String,
    pub post_id: String,
    pub page: i64,
    pub per_page: i64,
    pub skip_fetch_threads: bool,
    pub collapsed_threads: bool,
    pub collapsed_threads_extended: bool,
    pub user_id: String,
    pub include_deleted: bool,
    pub exclude_membership: bool,
    // thread
    pub direction: String,
    pub from_post: String,
    pub from_create_at: i64,
    pub from_update_at: i64,
    pub updates_only: bool,
}

/// What `PreparePostForClient` is asked to add.
#[derive(Default, Clone, Copy)]
pub struct Prep {
    pub is_new: bool,
    pub is_edit: bool,
    pub include_priority: bool,
    pub include_deleted: bool,
    pub retain_content: bool,
}

pub fn post_not_found() -> AppError {
    AppError::not_found("app.post.get.app_error", "Unable to get the post.")
}

static EMOJI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":[a-zA-Z0-9_+-]+:").unwrap());
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<((?:https?|ftp)://[^\s>]+)>|((?:https?://|ftp://|www\.)[^\s<]+)"#).unwrap());
static MD_IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"!\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)"#).unwrap());
static MD_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!?\[[^\]]*\]\([^)]*\)").unwrap());
static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?ms)^\s*(```|~~~).*?(^\s*(```|~~~)|\z)").unwrap());
static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`\n]*`").unwrap());
static PERMALINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-z_-]{1,64}/pl/[a-z0-9]{26}$").unwrap());
static HASHTAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#\pL[\pL\d\-_.]*[\pL\d]$").unwrap());

/// Markdown text with code taken out: where mentions, links and emoji count.
pub fn without_code(s: &str) -> String {
    INLINE_CODE.replace_all(&FENCE.replace_all(s, " "), " ").into_owned()
}

/// `getFirstLink`: the first autolink (not a `[text](url)` link nor an image).
fn first_link(s: &str) -> Option<String> {
    let text = MD_LINK.replace_all(&without_code(s), " ").into_owned();
    let m = LINK.captures(&text)?;
    let raw = m.get(1).or(m.get(2))?.as_str();
    let mut link = raw.trim_end_matches(['.', ',', ':', ';', '!', '?', '\'', '"', '*', '_', '~']).to_string();
    // A closing parenthesis that has no opening one in the link is the text's.
    while link.ends_with(')') && link.matches('(').count() < link.matches(')').count() {
        link.pop();
    }
    if link.to_ascii_lowercase().starts_with("www.") {
        link = format!("http://{link}");
    }
    Some(link)
}

/// `getImages`: the URLs of `![alt](url)` images.
fn markdown_images(s: &str) -> Vec<String> {
    MD_IMAGE.captures_iter(&without_code(s)).map(|c| c[1].to_string()).collect()
}

/// `model.ParseHashtags`.
pub fn parse_hashtags(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        let w = word.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '#');
        let w = w.trim_end_matches(|c: char| !c.is_alphanumeric());
        let pounds = w.len() - w.trim_start_matches('#').len();
        let w = if pounds > 1 { &w[pounds - 1..] } else { w };
        if HASHTAG.is_match(w) {
            out.push(' ');
            out.push_str(w);
        }
    }
    if out.len() > 1000 {
        let cut = out.as_bytes()[..999].iter().rposition(|&b| b == b' ').unwrap_or(0);
        out.truncate(cut);
    }
    out.trim_start().to_string()
}

/// `Post.AllStrings` without interactive blocks: the message and the attachments' texts.
pub fn all_strings(p: &Post) -> Vec<String> {
    let mut out = vec![p.message.clone()];
    if let Some(Value::Array(atts)) = p.prop("attachments") {
        for a in atts {
            for k in ["pretext", "title", "text", "footer"] {
                if let Some(s) = a[k].as_str() {
                    out.push(s.to_string());
                }
            }
            if let Some(fields) = a["fields"].as_array() {
                for f in fields {
                    if let Some(s) = f["value"].as_str() {
                        out.push(s.to_string());
                    }
                }
            }
        }
    }
    out
}

impl App {
    pub fn show_cfg(&self) -> Show {
        Show {
            email: self.cfg.bool("PrivacySettings.ShowEmailAddress"),
            fullname: self.cfg.bool("PrivacySettings.ShowFullName"),
            admin: false,
        }
    }

    pub fn post_priority_on(&self) -> bool {
        self.cfg.bool("ServiceSettings.PostPriority")
    }

    pub fn crt_allowed(&self) -> bool {
        self.cfg.str("ServiceSettings.CollapsedThreads") != "disabled"
    }

    /// `IsCRTEnabledForUser` for several users at once: the server default, unless a user's
    /// `display_settings/collapsed_reply_threads` preference says otherwise.
    pub async fn crt_for_users(&self, ids: &[String]) -> HashMap<String, bool> {
        let mode = self.cfg.str("ServiceSettings.CollapsedThreads");
        let default = matches!(mode, "default_on" | "always_on");
        let mut out: HashMap<String, bool> = ids.iter().map(|i| (i.clone(), default)).collect();
        if mode == "disabled" || mode == "always_on" {
            if mode == "disabled" {
                out.values_mut().for_each(|v| *v = false);
            }
            return out;
        }
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT UserId, Value FROM Preferences WHERE Category = 'display_settings'
             AND Name = 'collapsed_reply_threads' AND UserId = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();
        for (u, v) in rows {
            out.insert(u, v.as_deref() == Some("on"));
        }
        out
    }

    pub async fn crt_for_user(&self, id: &str) -> bool {
        self.crt_for_users(&[id.to_string()]).await.get(id).copied().unwrap_or(false)
    }

    /// `SqlPostStore.GetSingle`, with its reply count.
    pub async fn get_single_post(&self, id: &str, include_deleted: bool) -> ApiResult<Option<Post>> {
        let del = if include_deleted { "" } else { " AND p.DeleteAt = 0" };
        let sql = format!(
            "SELECT {}, (SELECT COUNT(*) FROM Posts rc WHERE rc.RootId = (CASE WHEN p.RootId = '' THEN p.Id ELSE p.RootId END) AND rc.DeleteAt = 0) AS replycount
             FROM Posts p WHERE p.Id = $1{del}",
            pc("p")
        );
        let r: Option<WithCount> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(id).fetch_optional(&self.db).await?;
        Ok(r.map(|w| w.0))
    }

    /// `App.GetSinglePost`: 404 when missing, or a membership message hidden by the channel.
    pub async fn single_post(&self, id: &str, include_deleted: bool) -> ApiResult<Post> {
        let p = self.get_single_post(id, include_deleted).await?.ok_or_else(post_not_found)?;
        if kind::is_membership(&p.r#type)
            && !kind::is_add_membership(&p.r#type)
            && let Some(c) = self.channel(&p.channel_id).await?
            && excludes_membership(&c)
        {
            return Err(post_not_found());
        }
        Ok(p)
    }

    /// `GetPostIfAuthorized`: (post, is a member).
    pub async fn post_if_authorized(&self, s: &Session, id: &str, include_deleted: bool) -> ApiResult<(Post, bool)> {
        let p = self.single_post(id, include_deleted).await?;
        let c = self.channel_or_404(&p.channel_id).await?;
        let (ok, member) = self.user_read_channel(&s.user_id, &c).await;
        if !ok {
            if c.r#type == channel::OPEN && !self.cfg.bool("ComplianceSettings.Enable") {
                if !self.has_team_perm(s, &c.team_id, perm::READ_PUBLIC_CHANNEL).await {
                    return Err(AppError::forbidden_perm(perm::READ_PUBLIC_CHANNEL));
                }
            } else {
                return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
            }
        }
        Ok((p, member))
    }

    /// `SessionHasPermissionToReadPost`: (allowed, is a member).
    pub async fn read_post_perm(&self, s: &Session, post_id: &str) -> ApiResult<(bool, bool)> {
        if post_id.is_empty() {
            return Ok((false, false));
        }
        let c: Option<(String,)> =
            sqlx::query_as("SELECT ChannelId FROM Posts WHERE Id = $1").bind(post_id).fetch_optional(&self.db).await?;
        let Some(Some(c)) = (match c {
            Some((cid,)) => Some(self.channel(&cid).await?),
            None => None,
        }) else {
            return Ok((self.has_perm(s, perm::READ_CHANNEL_CONTENT).await, false));
        };
        Ok(self.user_read_channel(&s.user_id, &c).await)
    }

    /// `SessionHasPermissionToChannelByPost`.
    pub async fn post_channel_perm(&self, s: &Session, post_id: &str, permission: &str) -> ApiResult<bool> {
        if post_id.is_empty() {
            return Ok(false);
        }
        let c: Option<(String,)> =
            sqlx::query_as("SELECT ChannelId FROM Posts WHERE Id = $1").bind(post_id).fetch_optional(&self.db).await?;
        if let Some((cid,)) = c {
            let roles = self.member_roles(&s.user_id, true).await?;
            if let Some(r) = roles.get(&cid)
                && self.roles_grant(r, permission).await
            {
                return Ok(true);
            }
            if let Some(ch) = self.channel(&cid).await?
                && !ch.team_id.is_empty()
            {
                return Ok(self.has_team_perm(s, &ch.team_id, permission).await);
            }
        }
        Ok(self.has_perm(s, permission).await)
    }

    /// `InvalidateCacheForChannelPosts` / `InvalidateLastPostTimeCache`.
    pub fn invalidate_channel_posts(&self, channel_id: &str) {
        self.caches.invalidate_last_post_time(channel_id);
    }

    // ---------- reading lists ----------

    /// `prepareThreadedResponse`: reply counts, followed flags and participants of collapsed threads.
    async fn threaded(&self, rows: Vec<WithThread>, extended: bool, reversed: bool) -> ApiResult<PostList> {
        let mut ids: Vec<String> = vec![];
        for r in &rows {
            for p in &r.participants {
                if !ids.contains(p) {
                    ids.push(p.clone());
                }
            }
        }
        let mut users: HashMap<String, User> = HashMap::new();
        if extended {
            let show = self.show_cfg();
            for u in self.users(&ids).await? {
                let mut u = (*u).clone();
                u.sanitize_profile(show);
                users.insert(u.id.clone(), u);
            }
        }
        let mut list = PostList::default();
        let mut rows = rows;
        if reversed {
            rows.reverse();
        }
        for r in rows {
            let mut p = r.post;
            let mut parts = Vec::with_capacity(r.participants.len());
            for id in &r.participants {
                match users.get(id) {
                    Some(u) => parts.push(u.clone()),
                    None if !extended => parts.push(User { id: id.clone(), ..Default::default() }),
                    None => {}
                }
            }
            // Go appends to a nil slice: no participants is `null`.
            p.participants = (!parts.is_empty()).then_some(parts);
            list.add_ordered(p);
        }
        Ok(list)
    }

    /// `SqlPostStore.GetPosts`: a page of the channel.
    pub async fn get_posts(&self, o: &GetPostsOpts) -> ApiResult<PostList> {
        if o.per_page > 1000 {
            return Err(AppError::bad_request("app.post.get_posts.app_error", "Limit exceeded for paging."));
        }
        let offset = o.per_page * o.page;
        let ex = exclude_membership("p", o.exclude_membership);
        if o.collapsed_threads {
            let sql = format!(
                "SELECT {}, {CRT_COLS} FROM Posts p {} WHERE p.DeleteAt = 0 AND p.ChannelId = $2 AND p.RootId = ''{ex}
                 ORDER BY p.CreateAt DESC LIMIT $3 OFFSET $4",
                pc("p"),
                crt_joins("p")
            );
            let rows: Vec<WithThread> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(&o.user_id)
                .bind(&o.channel_id)
                .bind(o.per_page)
                .bind(offset)
                .fetch_all(&self.db)
                .await?;
            return self.threaded(rows, o.collapsed_threads_extended, false).await;
        }
        let del = if o.include_deleted { "" } else { " AND p.DeleteAt = 0" };
        let rc = if o.skip_fetch_threads {
            format!(", {}", reply_count_col("p", o.include_deleted))
        } else {
            ", 0::bigint AS replycount".into()
        };
        let roots = format!(
            "SELECT {}{rc} FROM Posts p WHERE p.ChannelId = $1{del}{ex} ORDER BY p.CreateAt DESC LIMIT $2 OFFSET $3",
            pc("p")
        );
        let q2del = if o.include_deleted { "" } else { "AND q2.DeleteAt = 0" };
        let subdel = if o.include_deleted { "" } else { "AND Posts.DeleteAt = 0" };
        let on = if o.skip_fetch_threads { "q1.RootId = q2.Id" } else { "q1.RootId = q2.Id OR q1.RootId = q2.RootId" };
        let prc = if o.skip_fetch_threads {
            format!(", {}", reply_count_col("q2", o.include_deleted))
        } else {
            ", 0::bigint AS replycount".into()
        };
        let parents = format!(
            "SELECT {}{prc} FROM Posts q2 INNER JOIN (SELECT DISTINCT q3.RootId FROM (SELECT Posts.RootId FROM Posts
               WHERE Posts.ChannelId = $1 {subdel} ORDER BY Posts.CreateAt DESC LIMIT $2 OFFSET $3) q3 WHERE q3.RootId != '') q1
             ON {on} WHERE q2.ChannelId = $1 {q2del}{} ORDER BY q2.CreateAt",
            pc("q2"),
            exclude_membership("q2", o.exclude_membership)
        );
        let (r, c) = tokio::join!(
            sqlx::query_as::<_, WithCount>(sqlx::AssertSqlSafe(roots))
                .bind(&o.channel_id)
                .bind(o.per_page)
                .bind(offset)
                .fetch_all(&self.db),
            sqlx::query_as::<_, WithCount>(sqlx::AssertSqlSafe(parents))
                .bind(&o.channel_id)
                .bind(o.per_page)
                .bind(offset)
                .fetch_all(&self.db)
        );
        let mut list = PostList::default();
        for p in r? {
            list.add_ordered(p.0);
        }
        for p in c? {
            list.add(p.0);
        }
        Ok(list)
    }

    /// `App.GetPosts` (allowFromCache): the first page of 30 or 60 from the last-posts cache.
    pub async fn get_posts_cached(&self, o: &GetPostsOpts) -> ApiResult<PostList> {
        let cacheable = o.page == 0 && (o.per_page == 30 || o.per_page == 60);
        let key = format!("{}{}", o.channel_id, o.per_page);
        if cacheable
            && let Some(v) = self.caches.last_posts.get(&key)
            && let Some(l) = list_from_value(&v)
        {
            return Ok(l);
        }
        let list = self.get_posts(&GetPostsOpts { collapsed_threads: false, ..o.clone() }).await?;
        if cacheable {
            self.caches.last_posts.put(key, Arc::new(serde_json::to_value(&list).unwrap()));
        }
        Ok(list)
    }

    /// `GetPostsSince` through the last-post-time cache: nothing newer than `since` in the channel answers empty
    /// without a query.
    pub async fn get_posts_since(&self, o: &GetPostsOpts, since: i64) -> ApiResult<PostList> {
        if let Some(last) = self.caches.last_post_time.get(&o.channel_id)
            && last <= since
        {
            return Ok(PostList::default());
        }
        let ex = exclude_membership("p", o.exclude_membership);
        let list = if o.collapsed_threads {
            let sql = format!(
                "SELECT {}, {CRT_COLS} FROM Posts p {} WHERE p.ChannelId = $2 AND p.UpdateAt > $3 AND p.RootId = ''{ex}
                 ORDER BY p.CreateAt DESC LIMIT 1000",
                pc("p"),
                crt_joins("p")
            );
            let rows: Vec<WithThread> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(&o.user_id)
                .bind(&o.channel_id)
                .bind(since)
                .fetch_all(&self.db)
                .await?;
            self.threaded(rows, o.collapsed_threads_extended, false).await?
        } else {
            let cols = Post::COLUMNS.join(", ");
            let (rc1, rc2) = if o.skip_fetch_threads {
                (format!(", {}", reply_count_col("p1", false)), format!(", {}", reply_count_col("cte", false)))
            } else {
                (", 0::bigint AS replycount".to_string(), ", 0::bigint AS replycount".to_string())
            };
            let ex_plain = exclude_membership("Posts", o.exclude_membership);
            let sql = format!(
                "WITH cte AS (SELECT {cols} FROM Posts WHERE UpdateAt > $1 AND ChannelId = $2{ex_plain} LIMIT 1000)
                 (SELECT {}{rc2} FROM cte) UNION (SELECT {}{rc1} FROM Posts p1 WHERE Id IN (SELECT RootId FROM cte){})
                 ORDER BY createat DESC",
                db::cols::<Post>("cte"),
                pc("p1"),
                exclude_membership("p1", o.exclude_membership)
            );
            let rows: Vec<WithCount> =
                sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(since).bind(&o.channel_id).fetch_all(&self.db).await?;
            let mut list = PostList::default();
            for WithCount(p) in rows {
                if p.update_at > since {
                    list.order.push(p.id.clone());
                }
                list.add(p);
            }
            list
        };
        let latest = list.posts.values().map(|p| p.update_at).fold(since, i64::max);
        self.caches.last_post_time.put(o.channel_id.clone(), latest);
        Ok(list)
    }

    /// `getPostsAround`: before or after a post.
    pub async fn get_posts_around(&self, o: &GetPostsOpts, before: bool) -> ApiResult<PostList> {
        if o.page < 0 || o.per_page < 0 {
            return Err(AppError::bad_request("app.post.get_posts_around.get.app_error", "Unable to get the posts."));
        }
        let (dir, sort) = if before { ("<", "DESC") } else { (">", "ASC") };
        let offset = o.page * o.per_page;
        let del = if o.include_deleted { "" } else { " AND p.DeleteAt = 0" };
        let ex = exclude_membership("p", o.exclude_membership);
        if o.collapsed_threads {
            let sql = format!(
                "SELECT {}, {CRT_COLS} FROM Posts p {} WHERE p.CreateAt {dir} (SELECT CreateAt FROM Posts WHERE Id = $2)
                 AND p.ChannelId = $3{del}{ex} AND p.RootId = '' ORDER BY p.CreateAt {sort} LIMIT $4 OFFSET $5",
                pc("p"),
                crt_joins("p")
            );
            let rows: Vec<WithThread> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(&o.user_id)
                .bind(&o.post_id)
                .bind(&o.channel_id)
                .bind(o.per_page)
                .bind(offset)
                .fetch_all(&self.db)
                .await?;
            return self.threaded(rows, o.collapsed_threads_extended, !before).await;
        }
        let sql = format!(
            "SELECT {}, {} FROM Posts p WHERE p.CreateAt {dir} (SELECT CreateAt FROM Posts WHERE Id = $1)
             AND p.ChannelId = $2{del}{ex} ORDER BY p.CreateAt {sort} LIMIT $3 OFFSET $4",
            pc("p"),
            reply_count_col("p", o.include_deleted)
        );
        let posts: Vec<WithCount> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&o.post_id)
            .bind(&o.channel_id)
            .bind(o.per_page)
            .bind(offset)
            .fetch_all(&self.db)
            .await?;
        let mut list = PostList::default();
        let mut root_ids: Vec<String> = vec![];
        for p in &posts {
            root_ids.push(p.0.id.clone());
            if !p.0.root_id.is_empty() {
                root_ids.push(p.0.root_id.clone());
            }
        }
        let mut ordered: Vec<Post> = posts.into_iter().map(|w| w.0).collect();
        if !before {
            ordered.reverse();
        }
        for p in ordered {
            list.add_ordered(p);
        }
        if !root_ids.is_empty() {
            let ids = if o.skip_fetch_threads { "p.Id = ANY($1)" } else { "(p.Id = ANY($1) OR p.RootId = ANY($1))" };
            let sql = format!(
                "SELECT {}, {} FROM Posts p WHERE {ids} AND p.ChannelId = $2{del}{ex} ORDER BY p.CreateAt DESC",
                pc("p"),
                reply_count_col("p", o.include_deleted)
            );
            let parents: Vec<WithCount> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(&root_ids)
                .bind(&o.channel_id)
                .fetch_all(&self.db)
                .await?;
            for p in parents {
                list.posts.entry(p.0.id.clone()).or_insert(p.0);
            }
        }
        Ok(list)
    }

    /// `getPostIdAroundTime`.
    async fn post_id_around(&self, channel_id: &str, time: i64, before: bool, crt: bool, exclude: bool) -> String {
        let (dir, sort) = if before { ("<", "DESC") } else { (">", "ASC") };
        let root = if crt { " AND p.RootId = ''" } else { "" };
        let sql = format!(
            "SELECT p.Id FROM Posts p WHERE p.CreateAt {dir} $1 AND p.ChannelId = $2 AND p.DeleteAt = 0{root}{}
             ORDER BY p.CreateAt {sort} LIMIT 1",
            exclude_membership("p", exclude)
        );
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(time)
            .bind(channel_id)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// `AddCursorIdsForPostList`.
    #[allow(clippy::too_many_arguments)]
    async fn add_cursor_ids(
        &self,
        list: &mut PostList,
        after: &str,
        before: &str,
        since: i64,
        page: i64,
        per_page: i64,
        crt: bool,
        exclude: bool,
    ) {
        let (mut next, mut prev) = (None, None);
        if since > 0 {
            next = Some(String::new());
            prev = Some(String::new());
        } else if !after.is_empty() {
            if page == 0 {
                prev = Some(after.to_string());
            }
            if (list.order.len() as i64) < per_page {
                next = Some(String::new());
            }
        } else if !before.is_empty() {
            if page == 0 {
                next = Some(before.to_string());
            }
            if (list.order.len() as i64) < per_page {
                prev = Some(String::new());
            }
        }
        let first = list.order.first().and_then(|id| list.posts.get(id)).map(|p| (p.channel_id.clone(), p.create_at));
        let last = list.order.last().and_then(|id| list.posts.get(id)).map(|p| (p.channel_id.clone(), p.create_at));
        list.next_post_id = match next {
            Some(n) => n,
            None => match first {
                Some((c, t)) => self.post_id_around(&c, t, false, crt, exclude).await,
                None => String::new(),
            },
        };
        list.prev_post_id = match prev {
            Some(p) => p,
            None => match last {
                Some((c, t)) => self.post_id_around(&c, t, true, crt, exclude).await,
                None => String::new(),
            },
        };
    }

    /// `SqlPostStore.Get`: a post and its thread.
    pub async fn get_post_thread(&self, id: &str, o: &GetPostsOpts) -> ApiResult<PostList> {
        let sort = match o.direction.as_str() {
            "up" => "DESC",
            "down" => "ASC",
            _ => "",
        };
        let order_col = if o.updates_only { "UpdateAt" } else { "CreateAt" };
        let mut conds = vec![];
        let mut args: Vec<i64> = vec![];
        // $1 is the root id, $2 the from post; the times follow.
        if o.from_create_at != 0 {
            let (d, pd) = if o.direction == "down" { (">", ">") } else { ("<", "<") };
            args.push(o.from_create_at);
            let n = args.len() + 2;
            conds.push(if o.from_post.is_empty() {
                format!("p.CreateAt {d} ${n}")
            } else {
                format!("(p.CreateAt {d} ${n} OR (p.CreateAt = ${n} AND p.Id {pd} $2))")
            });
        }
        if o.from_update_at != 0 && (o.direction == "down" || !o.collapsed_threads) {
            let (d, pd) = if o.direction == "down" { (">", ">") } else { ("<", "<") };
            args.push(o.from_update_at);
            let n = args.len() + 2;
            conds.push(if o.from_post.is_empty() {
                format!("p.UpdateAt {d} ${n}")
            } else {
                format!("(p.UpdateAt {d} ${n} OR (p.UpdateAt = ${n} AND p.Id {pd} $2))")
            });
        }
        let extra: String = conds.iter().map(|c| format!(" AND {c}")).collect();
        let order =
            if sort.is_empty() { String::new() } else { format!(" ORDER BY p.{order_col} {sort}, p.Id {sort}") };
        let limit = if o.per_page != 0 { format!(" LIMIT {}", o.per_page + 1) } else { String::new() };

        let mut list;
        let root_id;
        if o.collapsed_threads {
            let sql = format!(
                "SELECT {}, {CRT_COLS} FROM Posts p {} WHERE p.DeleteAt = 0 AND p.Id = $2",
                pc("p"),
                crt_joins("p")
            );
            let row: Option<WithThread> =
                sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&o.user_id).bind(id).fetch_optional(&self.db).await?;
            let row = row.ok_or_else(post_not_found)?;
            root_id = id.to_string();
            list = self.threaded(vec![row], o.collapsed_threads_extended, false).await?;
            let sql = format!(
                "SELECT {}, 0::bigint AS replycount FROM Posts p WHERE p.RootId = $1 AND p.DeleteAt = 0{extra}{order}{limit}",
                pc("p")
            );
            let mut q = sqlx::query_as::<_, WithCount>(sqlx::AssertSqlSafe(sql)).bind(&root_id).bind(&o.from_post);
            for a in &args {
                q = q.bind(*a);
            }
            let mut posts: Vec<Post> = q.fetch_all(&self.db).await?.into_iter().map(|w| w.0).collect();
            let has_next = o.per_page != 0 && posts.len() as i64 == o.per_page + 1;
            if has_next {
                posts.pop();
            }
            for p in posts {
                list.add_ordered(p);
            }
            list.has_next = Some(has_next);
            return Ok(list);
        }
        let post = self.get_single_post(id, false).await?.ok_or_else(post_not_found)?;
        root_id = if post.root_id.is_empty() { post.id.clone() } else { post.root_id.clone() };
        list = PostList::default();
        list.add_ordered(post);
        if o.skip_fetch_threads {
            return Ok(list);
        }
        let sql = format!(
            "WITH replycount AS (SELECT COUNT(*) AS num FROM Posts WHERE RootId = $1 AND DeleteAt = 0)
             SELECT {}, replycount.num AS replycount FROM Posts p, replycount
             WHERE (p.Id = $1 OR p.RootId = $1) AND p.DeleteAt = 0{extra}{order}{limit}",
            pc("p")
        );
        let mut q = sqlx::query_as::<_, WithCount>(sqlx::AssertSqlSafe(sql)).bind(&root_id).bind(&o.from_post);
        for a in &args {
            q = q.bind(*a);
        }
        let mut posts: Vec<Post> = q.fetch_all(&self.db).await?.into_iter().map(|w| w.0).collect();
        let has_next = o.per_page != 0 && posts.len() as i64 == o.per_page + 1;
        if has_next {
            posts.pop();
        }
        for p in posts {
            if p.id == id {
                continue;
            }
            list.add_ordered(p);
        }
        list.has_next = Some(has_next);
        Ok(list)
    }

    /// `filterSuppressedMembershipPosts`: membership messages a channel hides, out of the list.
    async fn filter_membership(&self, list: &mut PostList) -> ApiResult<()> {
        let mut hide: HashMap<String, bool> = HashMap::new();
        let mut gone = vec![];
        for (id, p) in &list.posts {
            if !kind::is_membership(&p.r#type) || kind::is_add_membership(&p.r#type) {
                continue;
            }
            let ex = match hide.get(&p.channel_id) {
                Some(e) => *e,
                None => {
                    let e = self.channel(&p.channel_id).await?.is_some_and(|c| excludes_membership(&c));
                    hide.insert(p.channel_id.clone(), e);
                    e
                }
            };
            if ex {
                gone.push(id.clone());
            }
        }
        for id in gone {
            list.posts.remove(&id);
            list.order.retain(|o| *o != id);
        }
        Ok(())
    }

    // ---------- metadata ----------

    /// `GetReactionsForPost` through the reaction cache.
    pub async fn reactions_for_post(&self, post_id: &str) -> ApiResult<Arc<Vec<crate::model::Reaction>>> {
        if let Some(r) = self.caches.reactions.get(post_id) {
            return Ok(r);
        }
        let r: Vec<crate::model::Reaction> = sqlx::query_as(
            "SELECT UserId, PostId, EmojiName, CreateAt, COALESCE(UpdateAt, CreateAt) AS UpdateAt,
                    COALESCE(DeleteAt, 0) AS DeleteAt, RemoteId, ChannelId
             FROM Reactions WHERE PostId = $1 AND COALESCE(DeleteAt, 0) = 0 ORDER BY CreateAt",
        )
        .bind(post_id)
        .fetch_all(&self.db)
        .await?;
        let r = Arc::new(r);
        self.caches.reactions.put(post_id.to_string(), r.clone());
        Ok(r)
    }

    pub async fn priority_for_posts(&self, ids: &[String]) -> ApiResult<HashMap<String, PostPriority>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<PostPriority> = sqlx::query_as(
            "SELECT PostId, ChannelId, Priority, RequestedAck, PersistentNotifications FROM PostsPriority WHERE PostId = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|p| (p.post_id.clone(), p)).collect())
    }

    pub async fn acks_for_posts(&self, ids: &[String]) -> ApiResult<HashMap<String, Vec<PostAcknowledgement>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<PostAcknowledgement> = sqlx::query_as(
            "SELECT PostId, UserId, COALESCE(ChannelId, '') AS ChannelId, AcknowledgedAt, RemoteId FROM PostAcknowledgements
             WHERE AcknowledgedAt != 0 AND PostId = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&self.db)
        .await?;
        let mut out: HashMap<String, Vec<PostAcknowledgement>> = HashMap::new();
        for a in rows {
            out.entry(a.post_id.clone()).or_default().push(a);
        }
        Ok(out)
    }

    /// `preparePostForClient`: reactions and custom emoji, files, and with `include_priority` the root's
    /// priority and acknowledgements.
    pub async fn prepare_post(&self, original: &Post, o: Prep) -> Post {
        let mut post = original.clone();
        // OverrideIconURLIfEmoji: an emoji icon shows as its image.
        if let Some(Value::String(name)) = post.props.0.get("override_icon_emoji")
            && !name.is_empty()
            && self.cfg.bool("ServiceSettings.EnablePostIconOverride")
            && let Some(url) = self.emoji_static_url(&name.replace(':', "")).await
        {
            post.props.0.insert("override_icon_url".into(), url.into());
        }
        let mut md = PostMetadata::default();
        if post.delete_at > 0 && !o.retain_content {
            post.message.clear();
            post.metadata = Some(md);
            return post;
        }
        let reactions = if post.has_reactions {
            self.reactions_for_post(&post.id).await.map(|r| (*r).clone()).unwrap_or_default()
        } else {
            vec![]
        };
        if self.cfg.bool("ServiceSettings.EnableCustomEmoji") {
            let mut names: Vec<String> = vec![];
            for s in all_strings(&post) {
                for m in EMOJI.find_iter(&s) {
                    names.push(m.as_str().trim_matches(':').to_string());
                }
            }
            names.extend(reactions.iter().map(|r| r.emoji_name.clone()));
            names.sort();
            names.dedup();
            if !names.is_empty()
                && let Ok(e) = self.emojis_by_names(&names).await
            {
                md.emojis = e.iter().map(|e| (**e).clone()).collect();
            }
        }
        md.reactions = reactions;
        if !post.file_ids.0.is_empty()
            && let Ok(f) = self.file_infos_for_ids(&post.file_ids.0, o.include_deleted).await
        {
            md.files = f;
        }
        if o.include_priority && self.post_priority_on() && post.root_id.is_empty() {
            let ids = [post.id.clone()];
            if let Ok(mut p) = self.priority_for_posts(&ids).await {
                md.priority = p.remove(&post.id);
            }
            if let Ok(mut a) = self.acks_for_posts(&ids).await {
                md.acknowledgements = a.remove(&post.id).unwrap_or_default();
            }
        }
        post.metadata = Some(md);
        post
    }

    /// `preparePostForClientWithEmbedsAndImages`.
    pub async fn prepare_post_full(&self, original: &Post, o: Prep) -> Post {
        let mut post = self.prepare_post(original, o).await;
        if post.delete_at > 0 && !o.retain_content {
            return post;
        }
        self.embeds_and_images(&mut post, o.is_new).await;
        post
    }

    fn link_allowed(&self, link: &str) -> bool {
        let restrict = self.cfg.str("ServiceSettings.RestrictLinkPreviews");
        if restrict.trim().is_empty() {
            return true;
        }
        let host =
            link.split("://").nth(1).unwrap_or(link).split(['/', '?', '#', ':']).next().unwrap_or("").to_lowercase();
        !restrict
            .split(',')
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty())
            .any(|d| host == d || host.ends_with(&format!(".{d}")))
    }

    fn site_url(&self) -> String {
        self.cfg.str("ServiceSettings.SiteURL").trim_end_matches('/').to_string()
    }

    pub fn looks_like_permalink(&self, url: &str) -> bool {
        let site = self.site_url();
        if site.is_empty() {
            return false;
        }
        match url.trim().strip_prefix(&site) {
            Some(path) => PERMALINK.is_match(path.trim_start_matches('/')),
            None => false,
        }
    }

    /// `getEmbedsAndImages`: the first link's embed (attachments, permalink, OpenGraph, image, link) and the
    /// sizes of the images.
    pub async fn embeds_and_images(&self, post: &mut Post, is_new: bool) {
        let mut md = post.metadata.take().unwrap_or_default();
        let link = first_link(&post.message).filter(|l| self.link_allowed(l));
        let unsafe_links = post.prop("unsafe_links").is_some_and(|v| v == "true" || v == true);
        if unsafe_links && !link.as_deref().is_some_and(|l| self.looks_like_permalink(l)) {
            post.metadata = Some(md);
            return;
        }
        let mut og_images: Vec<String> = vec![];
        if post.prop("attachments").is_some() {
            md.embeds.push(PostEmbed::new("message_attachment", "", None));
        } else if let Some(b) = post.prop("boards") {
            md.embeds.push(PostEmbed::new("boards", "", Some(EmbedData::Other(b.clone()))));
        } else if let Some(link) = link {
            let permalink = self.looks_like_permalink(&link);
            if self.cfg.bool("ServiceSettings.EnableLinkPreviews") || permalink {
                if permalink {
                    if self.cfg.bool("ServiceSettings.EnablePermalinkPreviews")
                        && let Some(pp) = self.permalink_preview(&link, post.create_at).await
                    {
                        md.embeds.push(PostEmbed::new("permalink", "", Some(EmbedData::Permalink(Box::new(pp)))));
                    }
                } else if let Some(meta) = self.link_metadata_at(&link, post.create_at, is_new).await {
                    if let Some(og) = &meta.opengraph {
                        for i in og.images.iter().flatten() {
                            let u = if i.secure_url.is_empty() { &i.url } else { &i.secure_url };
                            if !u.is_empty() {
                                og_images.push(u.clone());
                            }
                        }
                        md.embeds.push(PostEmbed::new(
                            "opengraph",
                            &link,
                            Some(EmbedData::OpenGraph(Box::new(og.clone()))),
                        ));
                    } else if let Some(img) = &meta.image {
                        if img.format == "svg" || link.to_lowercase().ends_with(".svg") {
                            md.embeds.push(PostEmbed::new("link", &link, None));
                        } else {
                            og_images.push(link.clone());
                            md.embeds.push(PostEmbed::new("image", &link, None));
                        }
                    } else {
                        md.embeds.push(PostEmbed::new("link", &link, None));
                    }
                } else {
                    md.embeds.push(PostEmbed::new("link", &link, None));
                }
            }
        }
        // getImagesForPost
        let mut urls: Vec<String> = vec![];
        for s in all_strings(post) {
            urls.extend(markdown_images(&s).into_iter().filter(|u| self.link_allowed(u)));
        }
        urls.extend(og_images);
        urls.sort();
        urls.dedup();
        for u in urls {
            if self.looks_like_permalink(&u) || u.to_lowercase().starts_with("data:image/") {
                continue;
            }
            if let Some(meta) = self.link_metadata_at(&u, post.create_at, is_new).await
                && let Some(img) = &meta.image
                && img.format != "svg"
            {
                md.images.insert(u, img.clone());
            }
        }
        post.metadata = Some(md);
    }

    /// `getLinkMetadataForPermalink`, cached as Go's link cache (an hour).
    async fn permalink_preview(&self, url: &str, at: i64) -> Option<PreviewPost> {
        let key = format!("pl|{url}|{}", at - at.rem_euclid(3_600_000));
        if let Some(v) = self.caches.open_graph.get(&key) {
            return v.permalink.clone();
        }
        let id = &url[url.len() - 26..];
        let p = self.single_post(id, false).await.ok()?;
        if p.r#type == kind::BURN_ON_READ {
            return None;
        }
        let c = self.channel(&p.channel_id).await.ok()??;
        let team_name: String = if c.is_group_or_direct() {
            String::new()
        } else {
            sqlx::query_scalar("SELECT Name FROM Teams WHERE Id = $1")
                .bind(&c.team_id)
                .fetch_optional(&self.db)
                .await
                .ok()??
        };
        let contains = first_link(&p.message).is_some_and(|l| self.looks_like_permalink(&l));
        let p = if contains {
            p
        } else {
            Box::pin(self.prepare_post_full(&p, Prep { include_priority: true, ..Default::default() })).await
        };
        let v = PreviewPost {
            post_id: p.id.clone(),
            post: Some(p),
            team_name,
            channel_display_name: c.display_name.clone(),
            channel_type: c.r#type.clone(),
            channel_id: c.id.clone(),
        };
        let meta = crate::file_api::LinkMeta { permalink: Some(v.clone()), url: url.to_string(), ..Default::default() };
        self.caches.open_graph.put(key, Arc::new(meta));
        Some(v)
    }

    /// `PreparePostListForClient`: each post with embeds and images, and the priority and acknowledgements of
    /// the ordered ones.
    pub async fn prepare_list(&self, list: PostList) -> PostList {
        let mut out = PostList { posts: BTreeMap::new(), ..list.clone() };
        for (id, p) in &list.posts {
            out.posts.insert(id.clone(), self.prepare_post_full(p, Prep::default()).await);
        }
        if self.post_priority_on() {
            let pr = self.priority_for_posts(&out.order).await.unwrap_or_default();
            let acks = self.acks_for_posts(&out.order).await.unwrap_or_default();
            for id in &out.order {
                if let Some(p) = out.posts.get_mut(id)
                    && let Some(md) = p.metadata.as_mut()
                {
                    if let Some(x) = pr.get(id) {
                        md.priority = Some(x.clone());
                    }
                    if let Some(x) = acks.get(id) {
                        md.acknowledgements = x.clone();
                    }
                }
            }
        }
        out
    }

    /// `SanitizePostMetadataForUser`: a permalink preview of a channel the user may not read is taken out, and
    /// so are mentions of channels they may not read.
    pub async fn sanitize_for_user(&self, mut post: Post, user_id: &str) -> Post {
        if let Some(md) = post.metadata.as_mut()
            && let Some(i) = md.embeds.iter().position(|e| e.r#type == "permalink")
        {
            let cid = match &md.embeds[i].data {
                Some(EmbedData::Permalink(pp)) => pp.channel_id.clone(),
                _ => String::new(),
            };
            let ok = match self.channel(&cid).await {
                Ok(Some(c)) => self.user_read_channel(user_id, &c).await.0,
                _ => false,
            };
            if !ok {
                md.embeds.remove(i);
                post.props.0.remove("previewed_post");
            }
        }
        self.sanitize_channel_mentions(&mut post, user_id).await;
        // sanitizeEmbeddedChannelMentionsForUser: a permalink preview shows its post's mentions too.
        if let Some(md) = post.metadata.as_mut() {
            for e in md.embeds.iter_mut().filter(|e| e.r#type == "permalink") {
                if let Some(EmbedData::Permalink(pp)) = e.data.as_mut()
                    && let Some(p) = pp.post.as_mut()
                {
                    self.sanitize_channel_mentions(p, user_id).await;
                }
            }
        }
        post
    }

    /// `sanitizeChannelMentionsForUser`: each mention looked up again by its team and channel name, kept (with the
    /// channel's current display name, without its id) when the viewer may resolve it.
    async fn sanitize_channel_mentions(&self, post: &mut Post, user_id: &str) {
        let Some(m) = post.props.0.get("channel_mentions") else { return };
        let Value::Object(m) = m.clone() else { return };
        let mut keep = serde_json::Map::new();
        for (name, v) in m {
            let Some(team_name) =
                v.as_object().and_then(|o| o.get("team_name")).and_then(Value::as_str).filter(|t| !t.is_empty())
            else {
                continue;
            };
            // GetChannelByNameForTeamName(name, team, includeDeleted)
            let found: Option<(String, String, String, String)> = sqlx::query_as(
                "SELECT c.Id, c.Type::text, c.DisplayName, c.TeamId FROM Channels c
                 WHERE c.Name = $1 AND c.TeamId = (SELECT Id FROM Teams WHERE Name = $2 LIMIT 1)",
            )
            .bind(&name)
            .bind(team_name)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten();
            let Some((id, kind, display, team_id)) = found else { continue };
            if self.user_resolve_mention(user_id, &id, &kind, &team_id).await {
                keep.insert(name, json!({"display_name": display, "team_name": team_name}));
            }
        }
        if keep.is_empty() {
            post.props.0.remove("channel_mentions");
        } else {
            post.props.0.insert("channel_mentions".into(), Value::Object(keep));
        }
    }

    pub async fn sanitize_list_for_user(&self, mut list: PostList, user_id: &str) -> PostList {
        let posts = std::mem::take(&mut list.posts);
        for (id, p) in posts {
            list.posts.insert(id, self.sanitize_for_user(p, user_id).await);
        }
        list
    }

    /// `publishWebsocketEventForPost`: the post without permalink embeds and without its channel mentions for
    /// everyone; the mentions go back in per recipient (`channelMentionsBroadcastHook`), as stored (with the
    /// channel id), those of the channels the recipient may resolve (`HasPermissionToResolveChannelMention`).
    /// Mentions without a channel id, or whose channel is gone, are dropped. The permalink preview goes back in
    /// for the recipients who may read its channel (`permalinkBroadcastHook`), with its post's mentions filtered
    /// the same way.
    pub async fn post_for_broadcast(&self, post: &Post) -> BroadcastPost {
        let mut p = post.clone();
        let preview = p.metadata.as_mut().and_then(|md| {
            let pp = md.embeds.iter().find_map(|e| match &e.data {
                Some(EmbedData::Permalink(pp)) if e.r#type == "permalink" => Some((**pp).clone()),
                _ => None,
            });
            md.embeds.retain(|e| e.r#type != "permalink");
            pp
        });
        let prop = p.props.0.remove("previewed_post");
        let mentions = match p.props.0.remove("channel_mentions") {
            Some(Value::Object(m)) => self.mention_channels(m).await,
            _ => vec![],
        };
        let mut permalink = None;
        if let (Some(preview), Some(Value::String(prop))) = (preview, prop)
            && is_valid_id(&prop)
            && let Ok(Some(channel)) = self.channel(&preview.channel_id).await
        {
            let mentions = match preview.post.as_ref().and_then(|x| x.props.0.get("channel_mentions")) {
                Some(Value::Object(m)) => self.mention_channels(m.clone()).await,
                _ => vec![],
            };
            permalink = Some(Arc::new(Permalink { prop, preview, channel, mentions }));
        }
        BroadcastPost { post: p, mentions: Arc::new(mentions), permalink }
    }

    /// Channel mentions with the channel each names; those without a channel id, or whose channel is gone, left out.
    async fn mention_channels(&self, m: serde_json::Map<String, Value>) -> Mentions {
        let mut out = vec![];
        for (name, v) in m {
            let Some(id) = v.get("id").and_then(Value::as_str).filter(|i| !i.is_empty()) else { continue };
            let Ok(Some(c)) = self.channel(id).await else { continue };
            out.push((name, v, c));
        }
        out
    }

    /// The mentions `user_id` may resolve.
    async fn resolvable(&self, user_id: &str, mentions: &Mentions) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        for (name, v, c) in mentions {
            if self.user_resolve_mention(user_id, &c.id, &c.r#type, &c.team_id).await {
                m.insert(name.clone(), v.clone());
            }
        }
        m
    }
}

type Mentions = Vec<(String, Value, Arc<Channel>)>;

/// A permalink preview taken out of a broadcast post: the `previewed_post` prop, the embed's preview, the channel
/// it shows and its post's channel mentions.
struct Permalink {
    prop: String,
    preview: PreviewPost,
    channel: Arc<Channel>,
    mentions: Mentions,
}

/// A post for its websocket events: `post` for every recipient, and with the channel mentions and the permalink
/// preview each recipient may see (`hook`).
pub struct BroadcastPost {
    pub post: Post,
    mentions: Arc<Mentions>,
    permalink: Option<Arc<Permalink>>,
}

impl BroadcastPost {
    /// `permalinkBroadcastHook` and `channelMentionsBroadcastHook`: the post's JSON for one recipient, when some
    /// mention or the preview is theirs to see.
    pub fn hook(&self) -> Option<crate::hub::PostHook> {
        if self.mentions.is_empty() && self.permalink.is_none() {
            return None;
        }
        let (base, mentions, permalink) = (self.post.clone(), self.mentions.clone(), self.permalink.clone());
        Some(Arc::new(move |app: State, uid: String| {
            let (mut p, mentions, permalink) = (base.clone(), mentions.clone(), permalink.clone());
            Box::pin(async move {
                let mut changed = false;
                if let Some(pl) = permalink.as_deref()
                    && app.user_read_channel(&uid, &pl.channel).await.0
                {
                    let mut preview = pl.preview.clone();
                    if let Some(post) = preview.post.as_mut()
                        && post.props.0.contains_key("channel_mentions")
                    {
                        let m = app.resolvable(&uid, &pl.mentions).await;
                        if m.is_empty() {
                            post.props.0.remove("channel_mentions");
                        } else {
                            post.props.0.insert("channel_mentions".into(), Value::Object(m));
                        }
                    }
                    p.props.0.insert("previewed_post".into(), pl.prop.clone().into());
                    let embed = PostEmbed::new("permalink", "", Some(EmbedData::Permalink(Box::new(preview))));
                    p.metadata.get_or_insert_with(Default::default).embeds.push(embed);
                    changed = true;
                }
                let m = app.resolvable(&uid, &mentions).await;
                if !m.is_empty() {
                    p.props.0.insert("channel_mentions".into(), Value::Object(m));
                    changed = true;
                }
                if !changed {
                    return None;
                }
                serde_json::to_string(&p).ok()
            })
        }))
    }
}

/// `Post.Etag`.
pub fn post_etag(p: &Post) -> String {
    format!("{}.{}.{}", crate::SERVER_VERSION, p.id, p.update_at)
}

/// `PostList.Etag`: the first post in order and the post updated last (on a tie, the greatest id).
pub fn list_etag(l: &PostList) -> String {
    let (mut id, mut t) = ("0", 0i64);
    for p in l.posts.values() {
        if p.update_at > t || (p.update_at == t && p.id.as_str() > id) {
            (id, t) = (&p.id, p.update_at);
        }
    }
    let first = l.order.first().map_or("", String::as_str);
    format!("{}.{first}.{id}.{t}", crate::SERVER_VERSION)
}

impl App {
    /// `GetPostsEtag`: the channel's latest post update (now, when there is none) and its join/leave setting.
    pub async fn posts_etag(&self, c: &Channel) -> String {
        let t: Option<i64> =
            sqlx::query_scalar("SELECT UpdateAt FROM Posts WHERE ChannelId = $1 ORDER BY UpdateAt DESC LIMIT 1")
                .bind(&c.id)
                .fetch_optional(&self.db)
                .await
                .ok()
                .flatten();
        format!("{}.{}.{}", crate::SERVER_VERSION, t.unwrap_or_else(now_ms), c.disable_join_leave_messages)
    }
}

fn list_from_value(v: &Value) -> Option<PostList> {
    let order: Vec<String> = serde_json::from_value(v["order"].clone()).ok()?;
    let posts: BTreeMap<String, Post> = serde_json::from_value(v["posts"].clone()).ok()?;
    Some(PostList { order, posts, ..Default::default() })
}

// ---------- create ----------

/// `CreatePostFlags`.
#[derive(Clone, Default)]
pub struct CreateFlags {
    pub set_online: bool,
    pub trigger_webhooks: bool,
    pub force_notification: bool,
    pub silent: bool,
    /// `FromIncomingWebhook`: `from_webhook`, and the display-identity overrides below re-injected.
    pub from_incoming_webhook: bool,
    pub allow_mm_blocks_actions: bool,
    /// A plugin's post: `from_plugin`.
    pub from_plugin: bool,
    pub override_username: String,
    pub override_icon_url: String,
    pub override_icon_emoji: String,
    pub webhook_display_name: String,
}

impl App {
    /// `App.CreatePost`: saves the post (ids, times, channel counters, thread), notifies (mentions, unread counts,
    /// thread memberships) and publishes `posted`. Other modules post system messages through it
    /// (`join_channel`, `leave_channel`, header changes, ...). `post.id` empty: a new id.
    pub async fn create_post(&self, post: Post, set_online: bool) -> ApiResult<Post> {
        let c = self.channel_or_404(&post.channel_id).await?;
        let flags = CreateFlags { set_online, ..Default::default() };
        self.create_post_in(post, &c, flags, None).await
    }

    /// `CreatePost` with the channel and the request's session (dedup of `pending_post_id`, OAuth).
    pub async fn create_post_in(
        &self,
        mut post: Post,
        c: &Channel,
        flags: CreateFlags,
        session: Option<&Session>,
    ) -> ApiResult<Post> {
        // deduplicateCreatePost
        let pending = post.pending_post_id.clone();
        if !pending.is_empty() {
            match self.caches.seen_pending_post_ids.get(&pending) {
                None => self.caches.seen_pending_post_ids.put(pending.clone(), String::new()),
                Some(id) if id.is_empty() => {
                    return Err(AppError::internal(
                        "api.post.deduplicate_create_post.pending",
                        "Rejected post since another client is making the same request.",
                    ));
                }
                Some(id) => {
                    let found = match session {
                        Some(s) => self.post_if_authorized(s, &id, false).await,
                        None => self.single_post(&id, false).await.map(|p| (p, true)),
                    };
                    match found {
                        Ok((p, _)) => return Ok(Post { metadata: None, ..p }),
                        Err(e) if e.status_code == 403 => {}
                        Err(e) => {
                            return Err(AppError::internal(
                                "api.post.deduplicate_create_post.failed_to_get",
                                e.detailed_error,
                            ));
                        }
                    }
                }
            }
        }
        let r = self.create_post_inner(&mut post, c, flags, session).await;
        if !pending.is_empty() {
            match &r {
                Ok(p) => self.caches.seen_pending_post_ids.put(pending, p.id.clone()),
                Err(_) => self.caches.seen_pending_post_ids.remove(&pending),
            }
        }
        r
    }

    async fn create_post_inner(
        &self,
        post: &mut Post,
        c: &Channel,
        flags: CreateFlags,
        session: Option<&Session>,
    ) -> ApiResult<Post> {
        post.sanitize_props();
        let user = self.user(&post.user_id).await?.ok_or_else(crate::users::missing_account)?;
        let p = &mut post.props.0;
        if user.is_bot {
            p.insert("from_bot".into(), "true".into());
        }
        if flags.from_incoming_webhook {
            p.insert("from_webhook".into(), "true".into());
        }
        if flags.from_plugin {
            p.insert("from_plugin".into(), "true".into());
        }
        if flags.force_notification {
            p.insert("force_notification".into(), crate::app::new_id().into());
        }
        if flags.silent {
            p.insert("silent_notification".into(), true.into());
        }
        if session.is_some_and(|s| s.is_oauth) {
            p.insert("from_oauth_app".into(), "true".into());
        }
        // Re-inject display-identity overrides under the incoming webhook's authority.
        if flags.from_incoming_webhook {
            for (k, v) in [
                ("override_username", &flags.override_username),
                ("override_icon_url", &flags.override_icon_url),
                ("override_icon_emoji", &flags.override_icon_emoji),
                ("webhook_display_name", &flags.webhook_display_name),
            ] {
                if !v.is_empty() {
                    p.insert(k.into(), v.clone().into());
                }
            }
        }
        if !user.is_bot && !session.is_some_and(|s| s.is_user_access_token()) && !flags.allow_mm_blocks_actions {
            p.remove("mm_blocks_actions");
        }
        // Channel mentions without the permission: highlighted as text, and the poster is told.
        let mut ephemeral = None;
        if post.r#type.is_empty() && !self.user_channel_perm(&user.id, &c.id, perm::USE_CHANNEL_MENTIONS).await.0 {
            let m = crate::notify::disable_mention_highlights(post);
            if !m.is_empty() {
                ephemeral = Some(Post {
                    user_id: user.id.clone(),
                    root_id: post.root_id.clone(),
                    channel_id: c.id.clone(),
                    message: format!(
                        "Channel notifications are disabled in {}. The {m} did not trigger any notifications.",
                        c.name
                    ),
                    props: sqlx::types::Json(
                        [("mentionHighlightDisabled".to_string(), Value::Bool(true))].into_iter().collect(),
                    ),
                    ..Default::default()
                });
            }
        }
        let mut parent: Option<PostList> = None;
        if !post.root_id.is_empty() {
            let root_err =
                || AppError::bad_request("api.post.create_post.root_id.app_error", "Invalid RootId parameter.");
            let list = self.get_post_thread(&post.root_id, &GetPostsOpts::default()).await.map_err(|_| root_err())?;
            let Some(root) = list.posts.get(&post.root_id) else {
                return Err(root_err());
            };
            if list.posts.values().any(|p| p.channel_id != post.channel_id) {
                return Err(AppError::internal(
                    "api.post.create_post.channel_root_id.app_error",
                    "invalid ChannelId for RootId",
                ));
            }
            if !root.root_id.is_empty() {
                return Err(root_err());
            }
            if root.r#type == kind::BURN_ON_READ {
                return Err(AppError::bad_request(
                    "api.post.create_post.burn_on_read.app_error",
                    "Cannot reply to this post.",
                ));
            }
            parent = Some(list);
        }
        post.hashtags = parse_hashtags(&post.message);
        self.fill_in_post_props(post, Some(c)).await?;
        if post.create_at == 0 {
            post.create_at = now_ms();
        }
        // getEmbedsAndImages before saving: a permalink names the post it previews.
        let mut probe = post.clone();
        self.embeds_and_images(&mut probe, true).await;
        if let Some(pid) =
            probe.metadata.as_ref().and_then(|m| m.embeds.iter().find(|e| e.r#type == "permalink")).and_then(|e| {
                match &e.data {
                    Some(EmbedData::Permalink(pp)) => Some(pp.post_id.as_str()),
                    _ => None,
                }
            })
        {
            post.props.0.insert("previewed_post".into(), pid.into());
        }

        let file_ids = post.file_ids.0.clone();
        self.save_post(post).await?;
        if !file_ids.is_empty() {
            let attached = self.attach_files_to_post(&post.id, &post.channel_id, &post.user_id, &file_ids).await?;
            if attached.len() != file_ids.len() && !post.is_remote() {
                post.file_ids = crate::model::FileIds(attached.clone(), attached.is_empty());
                sqlx::query("UPDATE Posts SET FileIds = $1 WHERE Id = $2")
                    .bind(&post.file_ids)
                    .bind(&post.id)
                    .execute(&self.db)
                    .await?;
            }
            self.caches.file_infos.remove(&post.id);
        }
        let mut rpost = self.prepare_post(post, Prep { is_edit: true, ..Default::default() }).await;
        rpost.metadata.get_or_insert_with(Default::default).embeds =
            probe.metadata.map(|m| m.embeds).unwrap_or_default();
        if let Some(md) = rpost.metadata.as_mut()
            && md.priority.is_none()
        {
            // The priority as the client sent it: Go saves a copy with PostId and ChannelId and answers (and sends
            // `posted`) with the one passed in, without them.
            md.priority = post.metadata.as_ref().and_then(|m| m.priority.clone());
        }
        if self.cfg.bool("ServiceSettings.ThreadAutoFollow") && !rpost.root_id.is_empty() {
            let _ = self
                .maintain_membership(
                    &user.id,
                    &rpost.root_id,
                    crate::threads::MembershipOpts { following: true, update_following: true, ..Default::default() },
                )
                .await;
        }
        // handlePostEvents
        self.caches.invalidate_channel(c);
        if rpost.is_pinned {
            self.caches.pinned_count.remove(&c.id);
        }
        self.invalidate_channel_posts(&c.id);
        if let Err(e) =
            crate::notify::send_notifications(self, &rpost, c, &user, parent.as_ref(), flags.set_online).await
        {
            eprintln!("send notifications for {}: {}", rpost.id, e.detailed_error);
        }
        if flags.trigger_webhooks {
            crate::outgoing::post_created(&rpost, c, &user);
        }
        if let Some(ep) = ephemeral {
            self.send_ephemeral(&user.id, ep).await;
        }
        rpost.pending_post_id = post.pending_post_id.clone();
        Ok(rpost)
    }

    /// `FillInPostProps`: `channel_mentions` of the `~channel` names the poster may see.
    pub async fn fill_in_post_props(&self, post: &mut Post, c: Option<&Channel>) -> ApiResult<()> {
        let names = crate::notify::channel_mentions(post);
        let mut prop = serde_json::Map::new();
        if !names.is_empty() {
            // Without the channel, the post's own (`GetForPost`); a DM or GM takes the client's current team, and
            // without one every team is searched (`GetByNames` with no team).
            let own;
            let c = match c {
                Some(c) => c,
                None => {
                    own = self.channel(&post.channel_id).await?.ok_or_else(|| {
                        AppError::bad_request("api.context.invalid_param.app_error", "")
                            .with_param("Name", "post.channel_id")
                    })?;
                    &*own
                }
            };
            let team_id =
                if c.team_id.is_empty() { post.prop_str("current_team_id").to_string() } else { c.team_id.clone() };
            post.props.0.remove("current_team_id");
            let found: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
                "SELECT c.Id, c.Name, c.DisplayName, c.Type::text, c.TeamId, t.Name FROM Channels c JOIN Teams t ON t.Id = c.TeamId
                 WHERE ($1 = '' OR c.TeamId = $1) AND c.Name = ANY($2) AND c.DeleteAt = 0",
            )
            .bind(&team_id)
            .bind(&names)
            .fetch_all(&self.db)
            .await?;
            for (id, name, display, ty, channel_team, team) in found {
                if self.user_resolve_mention(&post.user_id, &id, &ty, &channel_team).await {
                    prop.insert(name, json!({"display_name": display, "team_name": team, "id": id}));
                }
            }
        }
        if prop.is_empty() {
            post.props.0.remove("channel_mentions");
        } else {
            post.props.0.insert("channel_mentions".into(), Value::Object(prop));
        }
        Ok(())
    }

    /// `SqlPostStore.Save`: the post, the thread, the priority, then the channel's counters and the root's
    /// `UpdateAt`.
    pub async fn save_post(&self, post: &mut Post) -> ApiResult<()> {
        if !post.id.is_empty() && !post.is_remote() {
            return Err(AppError::bad_request(
                "app.post.save.existing.app_error",
                "You cannot update an existing post.",
            ));
        }
        post.pre_save(now_ms());
        if post.remote_id.is_none() {
            post.remote_id = None;
        }
        self.validate_post(post, self.max_post_size().await as usize)?;
        let mut tx = self.db.begin().await?;
        post.bind(sqlx::query(Post::INSERT)).execute(&mut *tx).await?;
        if !post.root_id.is_empty() {
            update_thread_from_post(&mut tx, post).await?;
        }
        if let Some(pr) = post.priority()
            && (pr.priority.is_some() || pr.requested_ack.is_some() || pr.persistent_notifications.is_some())
        {
            sqlx::query(
                "INSERT INTO PostsPriority (PostId, ChannelId, Priority, RequestedAck, PersistentNotifications) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&post.id)
            .bind(&post.channel_id)
            .bind(pr.priority.clone().unwrap_or_default())
            .bind(pr.requested_ack)
            .bind(pr.persistent_notifications)
            .execute(&mut *tx)
            .await?;
            if pr.persistent_notifications == Some(true) {
                sqlx::query("INSERT INTO PersistentNotifications (PostId, CreateAt, LastSentAt, DeleteAt, SentCount) VALUES ($1, $2, 0, 0, 0)")
                    .bind(&post.id)
                    .bind(post.create_at)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        let count = if post.excludes_from_message_count() { 0 } else { 1 };
        let root = post.root_id.is_empty();
        let _ = sqlx::query(
            "UPDATE Channels SET LastPostAt = GREATEST($1, LastPostAt),
                 LastRootPostAt = GREATEST($2, LastRootPostAt),
                 TotalMsgCount = TotalMsgCount + $3, TotalMsgCountRoot = TotalMsgCountRoot + $4
             WHERE Id = $5",
        )
        .bind(post.create_at)
        .bind(if root { post.create_at } else { 0 })
        .bind(count)
        .bind(if root { count } else { 0 })
        .bind(&post.channel_id)
        .execute(&self.db)
        .await;
        if !root {
            let _ = sqlx::query("UPDATE Posts SET UpdateAt = $1 WHERE Id = $2")
                .bind(post.create_at)
                .bind(&post.root_id)
                .execute(&self.db)
                .await;
            post.reply_count = sqlx::query_scalar("SELECT COUNT(Id) FROM Posts WHERE RootId = $1 AND DeleteAt = 0")
                .bind(&post.root_id)
                .fetch_one(&self.db)
                .await
                .unwrap_or(0);
        }
        Ok(())
    }

    /// `Post.IsValid`.
    /// `Post.IsValid(maxPostSize)`.
    pub(crate) fn validate_post(&self, p: &Post, max_post_size: usize) -> ApiResult<()> {
        let bad = |what: &str| {
            AppError::bad_request(&format!("model.post.is_valid.{what}.app_error"), &format!("Invalid {what}."))
        };
        if !is_valid_id(&p.id) {
            return Err(bad("id"));
        }
        if !is_valid_id(&p.user_id) {
            return Err(bad("user_id"));
        }
        if !is_valid_id(&p.channel_id) {
            return Err(bad("channel_id"));
        }
        if !(p.root_id.is_empty() || is_valid_id(&p.root_id)) {
            return Err(bad("root_id"));
        }
        if p.message.chars().count() > max_post_size {
            return Err(bad("message_length"));
        }
        if !kind::VALID.contains(&p.r#type.as_str()) && !p.r#type.starts_with(kind::CUSTOM_PREFIX) {
            return Err(bad("type"));
        }
        if serde_json::to_string(&p.file_ids.0).unwrap_or_default().chars().count() > 300 {
            return Err(bad("file_ids"));
        }
        Ok(())
    }

    /// `SendEphemeralPost`: shown to one user only, never saved.
    pub async fn send_ephemeral(&self, user_id: &str, mut p: Post) -> Post {
        p.r#type = kind::EPHEMERAL.into();
        if p.id.is_empty() {
            p.id = crate::app::new_id();
        }
        // GenerateActionIds, and AddPostActionCookies after preparing (below, on the prepared copy).
        crate::integrations::generate_action_ids(&mut p.props.0);
        if p.create_at == 0 {
            p.create_at = now_ms();
        }
        let mut p =
            self.prepare_post_full(&p, Prep { is_new: true, include_priority: true, ..Default::default() }).await;
        if (p.props.0.contains_key("attachments") || p.props.0.contains_key("mm_blocks_actions"))
            && let Ok(k) = self.integration_keys().await
        {
            crate::integrations::add_action_cookies(&mut p, &k.cookie);
        }
        self.hub.publish(
            "ephemeral_message",
            json!({"post": serde_json::to_string(&p).unwrap()}),
            Broadcast { user_id: user_id.into(), channel_id: p.channel_id.clone(), ..Default::default() },
        );
        p
    }
}

/// `SqlStore.GetMaxPostSize` on a current schema (Posts.Message is varchar(65535)).
pub const MAX_POST_SIZE: usize = 16383;

const LOCK_THREAD: &str = "SELECT COALESCE(Participants, '[]'), COALESCE(ReplyCount, 0), COALESCE(LastReplyAt, 0) \
                           FROM Threads WHERE PostId = $1 FOR UPDATE";

/// `updateThreadsFromPosts` for one reply, in the saving transaction.
async fn update_thread_from_post(tx: &mut sqlx::PgConnection, post: &Post) -> ApiResult<()> {
    let mut found: Option<(sqlx::types::Json<Vec<String>>, i64, i64)> =
        sqlx::query_as(LOCK_THREAD).bind(&post.root_id).fetch_optional(&mut *tx).await?;
    if found.is_none() {
        let parts: Vec<(String,)> = sqlx::query_as(
            "SELECT UserId FROM Posts WHERE RootId = $1 AND DeleteAt = 0 GROUP BY UserId ORDER BY MAX(CreateAt) ASC",
        )
        .bind(&post.root_id)
        .fetch_all(&mut *tx)
        .await?;
        let (count, last): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(Id), COALESCE(MAX(CreateAt), 0) FROM Posts WHERE RootId = $1 AND DeleteAt = 0",
        )
        .bind(&post.root_id)
        .fetch_one(&mut *tx)
        .await?;
        let team: String = sqlx::query_scalar("SELECT COALESCE(TeamId, '') FROM Channels WHERE Id = $1")
            .bind(&post.channel_id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_default();
        let parts: Vec<String> = parts.into_iter().map(|p| p.0).collect();
        let created = sqlx::query(
            "INSERT INTO Threads (PostId, ChannelId, ReplyCount, LastReplyAt, Participants, ThreadTeamId)
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (PostId) DO NOTHING",
        )
        .bind(&post.root_id)
        .bind(&post.channel_id)
        .bind(count)
        .bind(last)
        .bind(sqlx::types::Json(parts))
        .bind(team)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if created {
            return Ok(());
        }
        found = sqlx::query_as(LOCK_THREAD).bind(&post.root_id).fetch_optional(&mut *tx).await?;
    }
    let Some((mut parts, count, last)) = found else { return Ok(()) };
    parts.0.retain(|u| *u != post.user_id);
    parts.0.push(post.user_id.clone());
    sqlx::query(
        "UPDATE Threads SET ChannelId = $1, ReplyCount = $2, LastReplyAt = $3, Participants = $4 WHERE PostId = $5",
    )
    .bind(&post.channel_id)
    .bind(count + 1)
    .bind(last.max(post.create_at))
    .bind(parts)
    .bind(&post.root_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

// ---------- update, delete ----------

/// `PostPatch`.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct PostPatch {
    pub is_pinned: Option<bool>,
    pub message: Option<String>,
    pub props: Option<serde_json::Map<String, Value>>,
    pub file_ids: Option<Vec<String>>,
    pub has_reactions: Option<bool>,
}

impl App {
    /// `App.UpdatePost` (not a safe update): message, pin, props and files; the old version kept as a deleted
    /// row with `OriginalId`, `post_edited` published.
    pub async fn update_post(&self, s: &Session, received: Post, keep_props: bool) -> ApiResult<Post> {
        self.update_post_opts(s, received, keep_props, false).await
    }

    /// `UpdatePost` with `AllowMmBlocksActionsUpdate` (an integration's answer may replace its own actions).
    pub async fn update_post_opts(
        &self,
        s: &Session,
        received: Post,
        keep_props: bool,
        allow_mm_blocks: bool,
    ) -> ApiResult<Post> {
        let mut received = received;
        received.remote_id = None;
        received.sanitize_props();
        let old = self.get_single_post(&received.id, false).await?.ok_or_else(post_not_found)?;
        if old.delete_at != 0 {
            return Err(AppError::bad_request(
                "api.post.update_post.permissions_details.app_error",
                "Already deleted id={{.PostId}}.",
            ));
        }
        if old.r#type == kind::BURN_ON_READ {
            return Err(AppError::bad_request(
                "api.post.update_post.burn_on_read.app_error",
                "Cannot update this post.",
            ));
        }
        if old.is_system_message() {
            return Err(AppError::bad_request(
                "api.post.update_post.system_message.app_error",
                "Unable to update system message.",
            ));
        }
        let c = self.channel_or_404(&old.channel_id).await?;
        if c.delete_at != 0 {
            return Err(AppError::bad_request(
                "api.post.update_post.can_not_update_post_in_deleted.error",
                "Can not update a post in a deleted channel.",
            ));
        }
        let mut new = old.clone();
        let now = now_ms();
        if new.message != received.message {
            new.message = received.message.clone();
            new.edit_at = now;
            new.hashtags = parse_hashtags(&received.message);
        }
        new.is_pinned = received.is_pinned;
        new.has_reactions = received.has_reactions;
        if !keep_props {
            new.props = received.props.clone();
        }
        // PreserveIdentityPropsFrom
        for k in [
            "silent_notification",
            "from_bot",
            "from_webhook",
            "from_oauth_app",
            "from_plugin",
            "override_username",
            "override_icon_url",
            "override_icon_emoji",
            "webhook_display_name",
        ] {
            if let Some(v) = old.prop(k) {
                new.props.0.insert(k.into(), v.clone());
            }
        }
        match old.prop("mm_blocks_actions").filter(|_| !allow_mm_blocks || received.prop("mm_blocks_actions").is_none())
        {
            Some(v) => {
                new.props.0.insert("mm_blocks_actions".into(), v.clone());
            }
            None => {
                new.props.0.remove("mm_blocks_actions");
            }
        }
        // processPostFileChanges
        let mut new_ids = received.file_ids.0.clone();
        new_ids.dedup();
        let old_ids: Vec<String> = old.file_ids.0.clone();
        let added: Vec<String> = new_ids.iter().filter(|i| !old_ids.contains(i)).cloned().collect();
        let removed: Vec<String> = old_ids.iter().filter(|i| !new_ids.contains(i)).cloned().collect();
        let unchanged: Vec<String> = new_ids.iter().filter(|i| old_ids.contains(i)).cloned().collect();
        let mut file_ids = new_ids.clone();
        if !added.is_empty() {
            let attached = self.attach_files_to_post(&new.id, &new.channel_id, &s.user_id, &added).await?;
            if attached.len() != added.len() {
                file_ids = attached.into_iter().chain(unchanged).collect();
            }
        }
        if !removed.is_empty() {
            sqlx::query("UPDATE FileInfo SET DeleteAt = $1 WHERE PostId = $2 AND Id = ANY($3)")
                .bind(now)
                .bind(&new.id)
                .bind(&removed)
                .execute(&self.db)
                .await?;
        }
        if !added.is_empty() || !removed.is_empty() {
            self.caches.file_infos.remove(&new.id);
        }
        new.file_ids = crate::model::FileIds(file_ids, false);
        if new.edit_at == old.edit_at
            && (old.file_ids != new.file_ids || old.prop("attachments") != new.prop("attachments"))
        {
            new.edit_at = now;
        }
        self.fill_in_post_props(&mut new, None).await?;
        new.metadata = received.metadata.clone().map(|mut m| {
            m.embeds.clear();
            m
        });
        // SqlPostStore.Update (PreCommit)
        crate::integrations::generate_action_ids(&mut new.props.0);
        new.update_at = now_ms();
        new.strip_nul();
        self.validate_post(&new, self.max_post_size().await as usize)?;
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "UPDATE Posts SET CreateAt=$1, UpdateAt=$2, EditAt=$3, DeleteAt=$4, IsPinned=$5, UserId=$6, ChannelId=$7,
                 RootId=$8, OriginalId=$9, Message=$10, Type=$11, Props=$12, Hashtags=$13, Filenames=$14, FileIds=$15,
                 HasReactions=$16, RemoteId=$17 WHERE Id=$18",
        )
        .bind(new.create_at)
        .bind(new.update_at)
        .bind(new.edit_at)
        .bind(new.delete_at)
        .bind(new.is_pinned)
        .bind(&new.user_id)
        .bind(&new.channel_id)
        .bind(&new.root_id)
        .bind(&new.original_id)
        .bind(&new.message)
        .bind(&new.r#type)
        .bind(&new.props)
        .bind(&new.hashtags)
        .bind(&new.filenames)
        .bind(&new.file_ids)
        .bind(new.has_reactions)
        .bind(&new.remote_id)
        .bind(&new.id)
        .execute(&mut *tx)
        .await?;
        let t = now_ms();
        sqlx::query("UPDATE Channels SET LastPostAt = $1 WHERE Id = $2 AND LastPostAt < $1")
            .bind(t)
            .bind(&new.channel_id)
            .execute(&mut *tx)
            .await?;
        if !new.root_id.is_empty() {
            sqlx::query("UPDATE Posts SET UpdateAt = $1 WHERE Id = $2 AND UpdateAt < $1")
                .bind(t)
                .bind(&new.root_id)
                .execute(&mut *tx)
                .await?;
        }
        let mut hist = old.clone();
        hist.delete_at = new.update_at;
        hist.update_at = new.update_at;
        hist.original_id = old.id.clone();
        hist.id = crate::app::new_id();
        hist.bind(sqlx::query(Post::INSERT)).execute(&mut *tx).await?;
        tx.commit().await?;
        // LastPostAt moved: the cached channel is stale.
        self.caches.invalidate_channel(&c);
        if new.is_pinned != old.is_pinned {
            self.caches.pinned_count.remove(&new.channel_id);
        }

        let mut rpost =
            self.prepare_post_full(&new, Prep { is_edit: true, include_priority: true, ..Default::default() }).await;
        rpost.is_following = None;
        rpost.reply_count = old.reply_count;
        let bpost = self.post_for_broadcast(&rpost).await;
        self.hub.publish(
            "post_edited",
            json!({"post": serde_json::to_string(&bpost.post).unwrap()}),
            Broadcast { channel_id: rpost.channel_id.clone(), post_hook: bpost.hook(), ..Default::default() },
        );
        self.invalidate_channel_posts(&rpost.channel_id);
        Ok(self.sanitize_for_user(rpost, &s.user_id).await)
    }

    /// `App.PatchPost`.
    pub async fn patch_post(&self, s: &Session, post_id: &str, mut patch: PostPatch) -> ApiResult<Post> {
        let mut post = self.single_post(post_id, false).await?;
        if post.r#type == kind::BURN_ON_READ {
            return Err(AppError::bad_request(
                "api.post.patch_post.can_not_update_burn_on_read_post.error",
                "Cannot update this post.",
            ));
        }
        let c = self.channel_or_404(&post.channel_id).await?;
        if c.delete_at != 0 {
            return Err(AppError::bad_request(
                "api.post.patch_post.can_not_update_post_in_deleted.error",
                "Can not update a post in a deleted channel.",
            ));
        }
        if !self.user_channel_perm(&post.user_id, &post.channel_id, perm::USE_CHANNEL_MENTIONS).await.0
            && let Some(props) = patch.props.as_mut()
        {
            props.insert("mentionHighlightDisabled".into(), Value::Bool(true));
        }
        if let Some(v) = patch.is_pinned {
            post.is_pinned = v;
        }
        if let Some(v) = patch.message.take() {
            post.message = v;
        }
        if let Some(v) = patch.props.take() {
            post.props = sqlx::types::Json(v);
        }
        if let Some(v) = patch.file_ids.take() {
            post.file_ids = crate::model::FileIds(v, false);
        }
        if let Some(v) = patch.has_reactions {
            post.has_reactions = v;
        }
        self.update_post(s, post, false).await
    }

    /// `App.DeletePost`: the post and its replies marked deleted, the thread updated, `post_deleted` sent.
    pub async fn delete_post(&self, post_id: &str, delete_by: &str) -> ApiResult<Post> {
        let post = self
            .get_single_post(post_id, false)
            .await?
            .ok_or_else(|| AppError::bad_request("app.post.get.app_error", "Unable to get the post."))?;
        let c = self.channel_or_404(&post.channel_id).await?;
        if c.delete_at != 0 {
            return Err(AppError::bad_request(
                "api.post.delete_post.can_not_delete_post_in_deleted.error",
                "Can not delete a post in a deleted channel.",
            ));
        }
        let now = now_ms();
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "UPDATE Posts SET DeleteAt = $1, UpdateAt = $1, Props = jsonb_set(Props, '{deleteBy}', to_jsonb($2::text))
             WHERE Id = $3 OR RootId = $3",
        )
        .bind(now)
        .bind(delete_by)
        .bind(post_id)
        .execute(&mut *tx)
        .await?;
        if post.root_id.is_empty() {
            sqlx::query("UPDATE Threads SET ThreadDeleteAt = $1 WHERE PostId = $2")
                .bind(now)
                .bind(post_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE FileInfo SET DeleteAt = $1 FROM Posts WHERE FileInfo.PostId = Posts.Id AND Posts.RootId = $2",
            )
            .bind(now)
            .bind(post_id)
            .execute(&mut *tx)
            .await?;
        } else {
            let mine: i64 =
                sqlx::query_scalar("SELECT COUNT(Id) FROM Posts WHERE RootId = $1 AND UserId = $2 AND DeleteAt = 0")
                    .bind(&post.root_id)
                    .bind(&post.user_id)
                    .fetch_one(&mut *tx)
                    .await?;
            // updateThreadAfterReplyDeletion: the poster leaves the participants with their last reply.
            let parts = if mine == 0 { "Participants = Participants - $2::text, " } else { "" };
            let sql = format!(
                "UPDATE Threads SET {parts}LastReplyAt = (SELECT COALESCE(MAX(CreateAt), 0) FROM Posts WHERE RootId = $1 AND DeleteAt = 0),
                     ReplyCount = (SELECT COUNT(*) FROM Posts WHERE RootId = $1 AND DeleteAt = 0)
                 WHERE PostId = $1 AND ReplyCount > 0"
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&post.root_id);
            if mine == 0 {
                q = q.bind(&post.user_id);
            }
            q.execute(&mut *tx).await?;
            sqlx::query("UPDATE Posts SET UpdateAt = $1 WHERE Id = $2")
                .bind(now)
                .bind(&post.root_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        if !post.file_ids.0.is_empty() {
            let _ = sqlx::query("UPDATE FileInfo SET DeleteAt = $1 WHERE PostId = $2")
                .bind(now)
                .bind(post_id)
                .execute(&self.db)
                .await;
            self.caches.file_infos.remove(post_id);
        }
        if post.root_id.is_empty() {
            let _ = sqlx::query("UPDATE PersistentNotifications SET DeleteAt = $1 WHERE PostId = $2")
                .bind(now)
                .bind(post_id)
                .execute(&self.db)
                .await;
        }
        // CleanUpAfterPostDeletion
        let mut admin = serde_json::to_value(&post).unwrap();
        let user = serde_json::to_string(&post).unwrap();
        admin = json!({"post": admin.to_string(), "delete_by": delete_by});
        self.hub.publish(
            "post_deleted",
            admin,
            Broadcast {
                channel_id: post.channel_id.clone(),
                sanitized: Some(json!({"post": user})),
                ..Default::default()
            },
        );
        let _ = sqlx::query("DELETE FROM Preferences WHERE Category = 'flagged_post' AND Name = $1")
            .bind(post_id)
            .execute(&self.db)
            .await;
        let _ = sqlx::query("DELETE FROM Drafts WHERE ChannelId = $1 AND RootId = $2")
            .bind(&post.channel_id)
            .bind(post_id)
            .execute(&self.db)
            .await;
        if !post.root_id.is_empty() {
            let _ = crate::notify::remove_notifications(self, &post, &c).await;
        }
        if post.is_pinned {
            self.caches.pinned_count.remove(&post.channel_id);
        }
        self.invalidate_channel_posts(&post.channel_id);
        Ok(post)
    }
}

// ---------- handlers ----------

#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateQuery {
    set_online: Option<String>,
    silent: Option<String>,
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// `userCreatePostPermissionCheckWithContext`.
pub async fn check_create_post(app: &App, s: &Session, channel_id: &str) -> ApiResult<()> {
    if app.channel_perm(s, channel_id, perm::CREATE_POST).await?.0 {
        return Ok(());
    }
    if let Some(c) = app.channel(channel_id).await?
        && c.r#type == channel::OPEN
        && app.has_team_perm(s, &c.team_id, perm::CREATE_POST_PUBLIC).await
    {
        return Ok(());
    }
    Err(AppError::forbidden_perm(perm::CREATE_POST))
}

async fn create_post_h(
    St(app): St<State>,
    Auth(s): Auth,
    Query(q): Query<CreateQuery>,
    Body(mut post): Body<Post>,
) -> ApiResult<impl IntoResponse> {
    // SanitizeInput
    post.delete_at = 0;
    post.remote_id = Some(String::new());
    if let Some(md) = post.metadata.as_mut() {
        md.embeds.clear();
    }
    post.user_id = s.user_id.clone();
    if post.create_at != 0 && !app.has_perm(&s, perm::MANAGE_SYSTEM).await {
        post.create_at = 0;
    }
    check_create_post(&app, &s, &post.channel_id).await?;
    if post.is_system_message() {
        return Err(AppError::invalid_param("post.type"));
    }
    if !post.file_ids.0.is_empty() && !app.channel_perm(&s, &post.channel_id, perm::UPLOAD_FILE).await?.0 {
        return Err(AppError::forbidden_perm(perm::UPLOAD_FILE));
    }
    if let Some(pr) = post.priority() {
        if !app.post_priority_on() {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "api.post.post_priority.priority_post_not_allowed_for_user.request_error",
                "Post priority is not allowed.",
            ));
        }
        if !post.root_id.is_empty() {
            return Err(AppError::bad_request(
                "api.post.post_priority.priority_post_only_allowed_for_root_post.request_error",
                "Post priority is only allowed for root posts.",
            ));
        }
        if pr.requested_ack == Some(true) || pr.persistent_notifications == Some(true) {
            return Err(AppError::new(
                StatusCode::NOT_IMPLEMENTED,
                "license_error.feature_unavailable",
                "Feature is not available for the current license.",
            ));
        }
    }
    if post.r#type == kind::BURN_ON_READ || post.r#type == "card" {
        return Err(AppError::new(
            StatusCode::NOT_IMPLEMENTED,
            "api.post.fill_in_post_props.burn_on_read.license.app_error",
            "Feature is not available for the current license.",
        ));
    }
    let n = post.message.chars().count();
    let max = app.max_post_size().await as usize;
    if n > max {
        return Err(AppError::bad_request(
            "model.post.is_valid.message_length.app_error",
            &format!("Post Message property is longer than the maximum permitted length: {n} > {max}."),
        ));
    }
    let set_online =
        q.set_online.as_deref().filter(|s| !s.is_empty()).map(|s| parse_bool(s).unwrap_or(true)).unwrap_or(true);
    let silent = match q.silent.as_deref().filter(|s| !s.is_empty()) {
        Some(v) => parse_bool(v).ok_or_else(|| AppError::invalid_param("silent"))?,
        None => false,
    };
    // CreatePostAsUserWithFlags
    let c = app.channel(&post.channel_id).await?.ok_or_else(|| AppError::invalid_param("post.channel_id"))?;
    if c.delete_at != 0 {
        return Err(AppError::bad_request(
            "api.post.create_post.can_not_post_to_deleted.error",
            "Can not post to deleted channel.",
        ));
    }
    let user = app.user(&s.user_id).await?.ok_or_else(crate::users::missing_account)?;
    if silent && !user.is_bot && !s.is_oauth {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.post.create_post.silent_notification.app_error",
            "Only integrations can send silent notifications.",
        ));
    }
    let flags = CreateFlags { set_online, trigger_webhooks: true, silent, ..Default::default() };
    let rp = app.create_post_in(post, &c, flags, Some(&s)).await.map_err(|mut e| {
        // A bad root is the request's fault here.
        if e.id == "api.post.create_post.root_id.app_error" || e.id == "api.post.create_post.channel_root_id.app_error"
        {
            e.status_code = 400;
        }
        e
    })?;
    let from_bot_or_hook = rp.prop("from_webhook").is_some() || rp.prop("from_bot").is_some();
    let crt = app.crt_for_user(&s.user_id).await;
    if !from_bot_or_hook && !(crt && !rp.root_id.is_empty()) {
        let _ = app.mark_viewed(std::slice::from_ref(&rp.channel_id), &s.user_id, &s.id).await;
    }
    if set_online {
        app.set_status_online(&s.user_id, false).await;
    }
    app.touch_session(&s).await;
    app.extend_session_if_needed(&s).await;
    let rp = app.sanitize_for_user(rp, &s.user_id).await;
    Ok((StatusCode::CREATED, Json(rp)))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PostsQuery {
    since: Option<String>,
    before: String,
    after: String,
    page: Option<String>,
    per_page: Option<String>,
    #[serde(rename = "skipFetchThreads")]
    skip_fetch_threads: String,
    #[serde(rename = "collapsedThreads")]
    collapsed_threads: String,
    #[serde(rename = "collapsedThreadsExtended")]
    collapsed_threads_extended: String,
    include_deleted: String,
}

/// `c.Params.Page` / `PerPage`: bad values fall back to 0 and 60, per page at most 200.
pub fn page_params(page: Option<&str>, per_page: Option<&str>) -> (i64, i64) {
    let page = page.and_then(|p| p.parse::<i64>().ok()).filter(|p| *p >= 0).unwrap_or(0);
    let per_page = match per_page.and_then(|p| p.parse::<i64>().ok()) {
        Some(n) if n >= 0 => n.min(200),
        _ => 60,
    };
    (page, per_page)
}

async fn posts_for_channel(
    St(app): St<State>,
    Auth(s): Auth,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<PostsQuery>,
) -> ApiResult<Response> {
    if !is_valid_id(&channel_id) {
        return Err(AppError::invalid_url_param("channel_id"));
    }
    if !q.after.is_empty() && !is_valid_id(&q.after) {
        return Err(AppError::invalid_param("after"));
    }
    if !q.before.is_empty() && !is_valid_id(&q.before) {
        return Err(AppError::invalid_param("before"));
    }
    let since = match q.since.as_deref().filter(|s| !s.is_empty()) {
        Some(v) => v.parse::<i64>().map_err(|_| AppError::invalid_param("since"))?,
        None => 0,
    };
    let b = |v: &str| parse_bool(v).unwrap_or(false);
    let include_deleted = b(&q.include_deleted);
    if include_deleted && !app.has_perm(&s, perm::MANAGE_SYSTEM).await {
        return Err(AppError::forbidden_perm("read_deleted_posts"));
    }
    let c = app.channel_or_404(&channel_id).await?;
    if !app.user_read_channel(&s.user_id, &c).await.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    let (page, per_page) = page_params(q.page.as_deref(), q.per_page.as_deref());
    let o = GetPostsOpts {
        channel_id: channel_id.clone(),
        page,
        per_page,
        skip_fetch_threads: b(&q.skip_fetch_threads),
        collapsed_threads: b(&q.collapsed_threads),
        collapsed_threads_extended: b(&q.collapsed_threads_extended),
        user_id: s.user_id.clone(),
        include_deleted,
        exclude_membership: excludes_membership(&c),
        ..Default::default()
    };
    // Pages (not `since`) carry the channel's ETag; a client that has this version gets 304.
    let etag = if since > 0 { None } else { Some(app.posts_etag(&c).await) };
    if let Some(e) = &etag
        && let Some(r) = crate::teams::not_modified(&headers, e)
    {
        return Ok(r);
    }
    let list = if since > 0 {
        app.get_posts_since(&o, since).await?
    } else if !q.after.is_empty() {
        app.get_posts_around(
            &GetPostsOpts { post_id: q.after.clone(), collapsed_threads_extended: false, ..o.clone() },
            false,
        )
        .await?
    } else if !q.before.is_empty() {
        app.get_posts_around(&GetPostsOpts { post_id: q.before.clone(), ..o.clone() }, true).await?
    } else {
        app.get_posts(&o).await?
    };
    let mut list = app.prepare_list(list).await;
    app.add_cursor_ids(
        &mut list,
        &q.after,
        &q.before,
        since,
        page,
        per_page,
        o.collapsed_threads,
        o.exclude_membership,
    )
    .await;
    let body = Json(app.sanitize_list_for_user(list, &s.user_id).await);
    Ok(match etag {
        Some(e) => crate::teams::with_etag(body, &e),
        None => body.into_response(),
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IncludeDeleted {
    include_deleted: String,
}

async fn get_post(
    St(app): St<State>,
    Auth(s): Auth,
    Path(post_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<IncludeDeleted>,
) -> ApiResult<Response> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    let include_deleted = parse_bool(&q.include_deleted).unwrap_or(false);
    if include_deleted && !app.has_perm(&s, perm::MANAGE_SYSTEM).await {
        return Err(AppError::forbidden_perm(perm::MANAGE_SYSTEM));
    }
    let (p, _) = app.post_if_authorized(&s, &post_id, include_deleted).await?;
    let etag = post_etag(&p);
    if let Some(r) = crate::teams::not_modified(&headers, &etag) {
        return Ok(r);
    }
    let p = app.prepare_post_full(&p, Prep { include_priority: true, ..Default::default() }).await;
    Ok(crate::teams::with_etag(Json(app.sanitize_for_user(p, &s.user_id).await), &etag))
}

/// `model.SortedArrayFromJSON`: a JSON array of strings (`null` is empty), sorted without duplicates, or Go's
/// payload error.
pub fn sorted_array_from_json(body: &[u8]) -> ApiResult<Vec<String>> {
    let mut v: Vec<String> = serde_json::from_slice::<Option<Vec<String>>>(body)
        .map_err(|e| AppError::bad_request("api.payload.parse.error", "").detail(e.to_string()))?
        .unwrap_or_default();
    v.sort();
    v.dedup();
    Ok(v)
}

async fn posts_by_ids(St(app): St<State>, Auth(s): Auth, body: axum::body::Bytes) -> ApiResult<Json<Vec<Post>>> {
    let ids = sorted_array_from_json(&body)?;
    if ids.is_empty() {
        return Err(AppError::invalid_param("post_ids"));
    }
    if ids.len() > 1000 {
        return Err(AppError::bad_request(
            "api.post.posts_by_ids.invalid_body.request_error",
            "The number of post ids exceeds the maximum.",
        ));
    }
    let sql = format!(
        "SELECT {}, {} FROM Posts p WHERE p.Id = ANY($1) ORDER BY p.CreateAt DESC",
        pc("p"),
        reply_count_col("p", false)
    );
    let posts: Vec<WithCount> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&ids).fetch_all(&app.db).await?;
    let mut out = vec![];
    let mut allowed: HashMap<String, bool> = HashMap::new();
    for WithCount(p) in posts {
        let ok = match allowed.get(&p.channel_id) {
            Some(v) => *v,
            None => {
                let v = match app.channel(&p.channel_id).await? {
                    Some(c) => app.user_read_channel(&s.user_id, &c).await.0,
                    None => false,
                };
                allowed.insert(p.channel_id.clone(), v);
                v
            }
        };
        if ok {
            let p = app.prepare_post(&p, Prep::default()).await;
            out.push(app.sanitize_for_user(p, &s.user_id).await);
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ThreadQuery {
    #[serde(rename = "perPage")]
    per_page: String,
    #[serde(rename = "fromCreateAt")]
    from_create_at: String,
    #[serde(rename = "fromPost")]
    from_post: String,
    #[serde(rename = "fromUpdateAt")]
    from_update_at: String,
    #[serde(rename = "updatesOnly")]
    updates_only: String,
    direction: String,
    #[serde(rename = "skipFetchThreads")]
    skip_fetch_threads: String,
    #[serde(rename = "collapsedThreads")]
    collapsed_threads: String,
    #[serde(rename = "collapsedThreadsExtended")]
    collapsed_threads_extended: String,
}

async fn get_post_thread(
    St(app): St<State>,
    Auth(s): Auth,
    Path(post_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<ThreadQuery>,
) -> ApiResult<Response> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    let num = |v: &str, name: &str| -> ApiResult<i64> {
        if v.is_empty() { Ok(0) } else { v.parse().map_err(|_| AppError::invalid_param(name)) }
    };
    let per_page = num(&q.per_page, "perPage")?;
    if per_page > 200 {
        return Err(AppError::invalid_param("perPage"));
    }
    let from_create_at = num(&q.from_create_at, "fromCreateAt")?;
    if !q.from_post.is_empty() && from_create_at == 0 {
        return Err(AppError::invalid_param("if fromPost is set, then fromCreateAt must also be set"));
    }
    let from_update_at = num(&q.from_update_at, "fromUpdateAt")?;
    if from_update_at != 0 && from_create_at != 0 {
        return Err(AppError::invalid_param("fromUpdateAt"));
    }
    let updates_only = q.updates_only == "true";
    if updates_only && from_update_at == 0 {
        return Err(AppError::invalid_param("fromUpdateAt"));
    }
    if !q.direction.is_empty() && q.direction != "up" && q.direction != "down" {
        return Err(AppError::invalid_param("direction"));
    }
    if updates_only && q.direction == "up" {
        return Err(AppError::invalid_param("updatesOnly"));
    }
    let o = GetPostsOpts {
        skip_fetch_threads: q.skip_fetch_threads == "true",
        collapsed_threads: q.collapsed_threads == "true",
        collapsed_threads_extended: q.collapsed_threads_extended == "true",
        updates_only,
        per_page,
        direction: q.direction.clone(),
        from_post: q.from_post.clone(),
        from_create_at,
        from_update_at,
        user_id: s.user_id.clone(),
        ..Default::default()
    };
    let mut list = app.get_post_thread(&post_id, &o).await.map_err(|e| {
        if e.status_code == 404 { e } else { AppError::internal("app.post.get.app_error", e.detailed_error) }
    })?;
    app.filter_membership(&mut list).await?;
    if !list.posts.contains_key(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    app.post_if_authorized(&s, &post_id, false).await?;
    let etag = list_etag(&list);
    if let Some(r) = crate::teams::not_modified(&headers, &etag) {
        return Ok(r);
    }
    let list = app.prepare_list(list).await;
    Ok(crate::teams::with_etag(Json(app.sanitize_list_for_user(list, &s.user_id).await), &etag))
}

/// `postEditTimeLimitExpired`.
fn edit_time_expired(app: &App, p: &Post) -> bool {
    let limit = app.cfg.i64("ServiceSettings.PostEditTimeLimit");
    limit != -1 && now_ms() > p.create_at + limit * 1000
}

async fn update_post_h(
    St(app): St<State>,
    Auth(s): Auth,
    Path(post_id): Path<String>,
    Body(mut post): Body<Value>,
) -> ApiResult<Json<Post>> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    let has_files = post.get("file_ids").is_some_and(|v| !v.is_null());
    let has_props = post.get("props").is_some_and(|v| !v.is_null());
    if let Some(o) = post.as_object_mut() {
        o.remove("metadata");
    }
    let mut post: Post =
        serde_json::from_value(post).map_err(|e| AppError::invalid_param("post").detail(e.to_string()))?;
    post.delete_at = 0;
    if post.id != post_id {
        return Err(AppError::invalid_param("id"));
    }
    let original = app.single_post(&post_id, false).await.map_err(|_| AppError::forbidden_perm(perm::EDIT_POST))?;
    if !app.channel_perm(&s, &original.channel_id, perm::EDIT_POST).await?.0 {
        return Err(AppError::forbidden_perm(perm::EDIT_POST));
    }
    check_create_post(&app, &s, &original.channel_id).await?;
    if !has_files {
        post.file_ids = original.file_ids.clone();
    }
    if !has_props {
        post.props = original.props.clone();
    }
    if edit_time_expired(&app, &original)
        && (post.message != original.message || post.file_ids != original.file_ids || post.props != original.props)
    {
        return Err(AppError::bad_request(
            "api.post.update_post.permissions_time_limit.app_error",
            "Post edit is only allowed for a limited time.",
        ));
    }
    if post.message.chars().count() > app.max_post_size().await as usize {
        return Err(AppError::bad_request(
            "model.post.is_valid.message_length.app_error",
            "Post Message property is longer than the maximum permitted length.",
        ));
    }
    if s.user_id != original.user_id && !app.channel_perm(&s, &original.channel_id, perm::EDIT_OTHERS_POSTS).await?.0 {
        return Err(AppError::forbidden_perm(perm::EDIT_OTHERS_POSTS));
    }
    post.id = post_id;
    Ok(Json(app.update_post(&s, post, false).await?))
}

async fn patch_post_h(
    St(app): St<State>,
    Auth(s): Auth,
    Path(post_id): Path<String>,
    Body(patch): Body<PostPatch>,
) -> ApiResult<Json<Post>> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    let original = app.single_post(&post_id, false).await.map_err(|_| AppError::forbidden_perm(perm::EDIT_POST))?;
    let permission = if s.user_id == original.user_id { perm::EDIT_POST } else { perm::EDIT_OTHERS_POSTS };
    if !app.channel_perm(&s, &original.channel_id, permission).await?.0 {
        return Err(AppError::forbidden_perm(permission));
    }
    check_create_post(&app, &s, &original.channel_id).await?;
    let more_than_pin =
        patch.message.is_some() || patch.props.is_some() || patch.file_ids.is_some() || patch.has_reactions.is_some();
    if edit_time_expired(&app, &original) && more_than_pin {
        return Err(AppError::bad_request(
            "api.post.update_post.permissions_time_limit.app_error",
            "Post edit is only allowed for a limited time.",
        ));
    }
    let max = app.max_post_size().await as usize;
    if patch.message.as_ref().is_some_and(|m| m.chars().count() > max) {
        return Err(AppError::bad_request(
            "model.post.is_valid.message_length.app_error",
            "Post Message property is longer than the maximum permitted length.",
        ));
    }
    if let Some(ids) = &patch.file_ids {
        let added = ids.iter().any(|i| !original.file_ids.0.contains(i));
        if added && !app.channel_perm(&s, &original.channel_id, perm::UPLOAD_FILE).await?.0 {
            return Err(AppError::forbidden_perm(perm::UPLOAD_FILE));
        }
    }
    Ok(Json(app.patch_post(&s, &post_id, patch).await?))
}

async fn delete_post_h(St(app): St<State>, Auth(s): Auth, Path(post_id): Path<String>) -> ApiResult<Json<Value>> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    let post = app.single_post(&post_id, false).await?;
    let permission = if s.user_id == post.user_id { perm::DELETE_POST } else { perm::DELETE_OTHERS_POSTS };
    if !app.channel_perm(&s, &post.channel_id, permission).await?.0 {
        return Err(AppError::forbidden_perm(permission));
    }
    app.delete_post(&post_id, &s.user_id).await?;
    Ok(ok())
}

async fn save_pinned(app: &State, s: &Session, post_id: &str, pinned: bool) -> ApiResult<Json<Value>> {
    if !is_valid_id(post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    let post =
        app.single_post(post_id, false).await.map_err(|_| AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT))?;
    let c = app.channel_or_404(&post.channel_id).await?;
    if !app.user_read_channel(&s.user_id, &c).await.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    app.patch_post(s, post_id, PostPatch { is_pinned: Some(pinned), ..Default::default() }).await?;
    Ok(ok())
}

async fn pin_post(St(app): St<State>, Auth(s): Auth, Path(post_id): Path<String>) -> ApiResult<Json<Value>> {
    save_pinned(&app, &s, &post_id, true).await
}

async fn unpin_post(St(app): St<State>, Auth(s): Auth, Path(post_id): Path<String>) -> ApiResult<Json<Value>> {
    save_pinned(&app, &s, &post_id, false).await
}

/// `getPinnedPosts`.
async fn pinned_posts(
    St(app): St<State>,
    Auth(s): Auth,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !is_valid_id(&channel_id) {
        return Err(AppError::invalid_url_param("channel_id"));
    }
    let c = app.channel_or_404(&channel_id).await?;
    if !app.user_read_channel(&s.user_id, &c).await.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    let sql = format!(
        "SELECT {}, {} FROM Posts p WHERE p.IsPinned = true AND p.ChannelId = $1 AND p.DeleteAt = 0 ORDER BY p.CreateAt ASC",
        pc("p"),
        reply_count_col("p", false)
    );
    let posts: Vec<WithCount> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&channel_id).fetch_all(&app.db).await?;
    let mut list = PostList::default();
    for WithCount(p) in posts {
        list.add_ordered(p);
    }
    app.filter_membership(&mut list).await?;
    let etag = list_etag(&list);
    if let Some(r) = crate::teams::not_modified(&headers, &etag) {
        return Ok(r);
    }
    let list = app.prepare_list(list).await;
    Ok(crate::teams::with_etag(Json(app.sanitize_list_for_user(list, &s.user_id).await), &etag))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FlaggedQuery {
    team_id: String,
    channel_id: String,
    page: Option<String>,
    per_page: Option<String>,
}

/// `getFlaggedPostsForUser`: `page` is the offset in posts (`GetFlaggedPosts(…, offset, limit)`).
async fn flagged_posts(
    St(app): St<State>,
    Auth(s): Auth,
    Path(user_id): Path<String>,
    Query(q): Query<FlaggedQuery>,
) -> ApiResult<Json<PostList>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    app.require_user(&s, &user_id).await?;
    let (page, per_page) = page_params(q.page.as_deref(), q.per_page.as_deref());
    let mut conds = String::new();
    if !q.channel_id.is_empty() {
        let c = app.channel_or_404(&q.channel_id).await?;
        if !app.user_read_channel(&s.user_id, &c).await.0 {
            return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
        }
        conds = " AND p.ChannelId = $2".into();
    } else if !q.team_id.is_empty() {
        if !app.has_team_perm(&s, &q.team_id, perm::VIEW_TEAM).await {
            return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
        }
        conds = " AND p.ChannelId IN (SELECT Id FROM Channels WHERE TeamId = $2 OR TeamId = '')".into();
    }
    let sql = format!(
        "SELECT {}, {} FROM Posts p
         WHERE p.Id IN (SELECT Name FROM Preferences WHERE UserId = $1 AND Category = 'flagged_post') AND p.DeleteAt = 0{conds}
           AND p.ChannelId IN (SELECT ChannelId FROM ChannelMembers WHERE UserId = $1)
         ORDER BY p.CreateAt DESC LIMIT $3 OFFSET $4",
        pc("p"),
        reply_count_col("p", false)
    );
    let filter = if !q.channel_id.is_empty() { q.channel_id.clone() } else { q.team_id.clone() };
    let posts: Vec<WithCount> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&user_id)
        .bind(&filter)
        .bind(per_page)
        .bind(page)
        .fetch_all(&app.db)
        .await?;
    let mut list = PostList::default();
    for WithCount(p) in posts {
        list.add_ordered(p);
    }
    app.filter_membership(&mut list).await?;
    let list = app.prepare_list(list).await;
    Ok(Json(app.sanitize_list_for_user(list, &s.user_id).await))
}

/// `getEditHistoryForPost`: the author's earlier versions, newest first.
async fn edit_history(St(app): St<State>, Auth(s): Auth, Path(post_id): Path<String>) -> ApiResult<Json<Vec<Post>>> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    // A missing post is a permission error, as in Go.
    let post = app.single_post(&post_id, false).await.map_err(|_| AppError::forbidden_perm(perm::EDIT_POST))?;
    if !app.channel_perm(&s, &post.channel_id, perm::EDIT_POST).await?.0 || post.user_id != s.user_id {
        return Err(AppError::forbidden_perm(perm::EDIT_POST));
    }
    let sql = format!(
        "SELECT {}, 0::bigint AS replycount FROM Posts p WHERE p.OriginalId = $1 ORDER BY p.EditAt DESC",
        pc("p")
    );
    let posts: Vec<WithCount> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&post_id).fetch_all(&app.db).await?;
    if posts.is_empty() {
        // `GetEditHistoryForPost`: a post never edited is a NotFound from the store, answered as app.post.get.app_error.
        return Err(AppError::not_found("app.post.get.app_error", "Unable to get the post."));
    }
    let mut out = vec![];
    for WithCount(p) in posts {
        out.push(
            app.prepare_post(&p, Prep { include_deleted: true, retain_content: true, ..Default::default() }).await,
        );
    }
    Ok(Json(out))
}

/// `restorePostVersion`: the post's message and files as a version had them.
async fn restore_version(
    St(app): St<State>,
    Auth(s): Auth,
    Path((post_id, version_id)): Path<(String, String)>,
) -> ApiResult<Json<Post>> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    if !is_valid_id(&version_id) {
        return Err(AppError::invalid_url_param("restore_version_id"));
    }
    // The version first, then `postPatchChecks`; anything missing is a permission error, as in Go.
    let forbidden = || AppError::forbidden_perm(perm::EDIT_POST);
    let v = app.get_single_post(&version_id, true).await?.ok_or_else(forbidden)?;
    if v.user_id != s.user_id {
        return Err(forbidden());
    }
    let post = app.single_post(&post_id, false).await.map_err(|_| forbidden())?;
    let permission = if s.user_id == post.user_id { perm::EDIT_POST } else { perm::EDIT_OTHERS_POSTS };
    if !app.channel_perm(&s, &post.channel_id, permission).await?.0 {
        return Err(AppError::forbidden_perm(permission));
    }
    check_create_post(&app, &s, &post.channel_id).await?;
    if edit_time_expired(&app, &post) {
        return Err(AppError::bad_request(
            "api.post.update_post.permissions_time_limit.app_error",
            "Post edit is only allowed for a limited time.",
        ));
    }
    let v = Some(v).filter(|v| v.original_id == post_id).ok_or_else(|| {
        AppError::bad_request(
            "app.post.restore_post_version.not_an_history_item.app_error",
            "The provided post version is not part of the post's edit history.",
        )
    })?;
    if v.delete_at == 0 {
        return Err(AppError::bad_request("app.post.restore_post_version.not_valid_post_history_item.app_error", ""));
    }
    let patch = PostPatch { message: Some(v.message), file_ids: Some(v.file_ids.0), ..Default::default() };
    Ok(Json(app.patch_post(&s, &post_id, patch).await?))
}

/// `getPostInfo`.
async fn post_info(St(app): St<State>, Auth(s): Auth, Path(post_id): Path<String>) -> ApiResult<Json<Value>> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    let post = app.single_post(&post_id, false).await?;
    let c = app.channel_or_404(&post.channel_id).await?;
    let not_found = || AppError::not_found("app.post.get.app_error", "Unable to get the post.");
    let mut team: Option<(String, String, bool)> = None;
    let mut joined_team = false;
    if !c.team_id.is_empty() {
        let t: (String, String, bool) =
            sqlx::query_as("SELECT Id, DisplayName, COALESCE(AllowOpenInvite, false) FROM Teams WHERE Id = $1")
                .bind(&c.team_id)
                .fetch_optional(&app.db)
                .await?
                .ok_or_else(not_found)?;
        joined_team = s.team_members.iter().any(|m| m.team_id == c.team_id && m.delete_at == 0);
        let can = joined_team
            || app
                .user_team_perm(&s.user_id, &t.0, if t.2 { perm::JOIN_PUBLIC_TEAMS } else { perm::JOIN_PRIVATE_TEAMS })
                .await;
        if !can {
            return Err(not_found());
        }
        team = Some(t);
    }
    let (mut ok, joined) = app.user_read_channel(&s.user_id, &c).await;
    if !ok && c.r#type == channel::OPEN {
        ok = app.has_team_perm(&s, &c.team_id, perm::JOIN_PUBLIC_CHANNELS).await
            || team.as_ref().is_some_and(|t| t.2) && app.has_perm(&s, perm::JOIN_PUBLIC_TEAMS).await;
    }
    if !ok {
        return Err(not_found());
    }
    let mut info = json!({
        "channel_id": c.id, "channel_type": c.r#type, "channel_display_name": c.display_name,
        "has_joined_channel": joined, "team_id": "", "team_type": "", "team_display_name": "", "has_joined_team": false,
    });
    if let Some((id, name, open)) = team {
        info["team_id"] = id.into();
        info["team_type"] = (if open { "O" } else { "I" }).into();
        info["team_display_name"] = name.into();
        info["has_joined_team"] = joined_team.into();
    }
    Ok(Json(info))
}

// ---------- unread ----------

impl App {
    /// `countMentionsFromPost`: mentions of the user from `post` on (DM/GM: every post of others).
    async fn count_mentions_from_post(&self, user: &User, post: &Post) -> ApiResult<(i64, i64, i64)> {
        let c = self.channel_or_404(&post.channel_id).await?;
        let since = post.create_at - 1;
        let jl: Vec<String> = kind::JOIN_LEAVE_TYPES.iter().map(|s| s.to_string()).collect();
        if c.is_group_or_direct() {
            let (n, nr): (i64, i64) = sqlx::query_as(
                "SELECT COUNT(*), COUNT(*) FILTER (WHERE RootId = '') FROM Posts WHERE ChannelId = $1 AND CreateAt > $2
                 AND NOT (Type = ANY($3)) AND DeleteAt = 0 AND UserId != $4",
            )
            .bind(&c.id)
            .bind(since)
            .bind(&jl)
            .bind(&user.id)
            .fetch_one(&self.db)
            .await?;
            let urgent: i64 = if self.post_priority_on() {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM PostsPriority pp JOIN Posts p ON p.Id = pp.PostId WHERE pp.Priority = 'urgent'
                     AND p.ChannelId = $1 AND p.CreateAt > $2 AND p.DeleteAt = 0 AND p.UserId != $3",
                )
                .bind(&c.id)
                .bind(since)
                .bind(&user.id)
                .fetch_one(&self.db)
                .await?
            } else {
                0
            };
            return Ok((n, nr, urgent));
        }
        let props = self.channel_notify_props(&c.id).await?;
        let mut kw = crate::notify::Keywords::default();
        kw.add_user(user, props.get(&user.id), Some(crate::model::status::ONLINE), true);
        let comments = user.notify_prop("comments");
        let check_comments = comments == "root" || comments == "any";
        let mut by_thread: HashMap<String, bool> = HashMap::new();
        let thread =
            self.get_post_thread(&post.id, &GetPostsOpts { user_id: user.id.clone(), ..Default::default() }).await?;
        let (mut count, mut count_root, mut urgent) = (0i64, 0i64, 0i64);
        let mut root_mentions: Vec<String> = vec![];
        if crate::notify::is_post_mention(user, post, &kw, &thread.posts, &mut by_thread, check_comments) {
            count += 1;
            if post.root_id.is_empty() {
                count_root += 1;
                root_mentions.push(post.id.clone());
            }
        }
        let mut page = 0;
        loop {
            let o = GetPostsOpts {
                channel_id: c.id.clone(),
                post_id: post.id.clone(),
                page,
                per_page: 200,
                user_id: user.id.clone(),
                ..Default::default()
            };
            let list = self.get_posts_around(&o, false).await?;
            for id in &list.order {
                let p = &list.posts[id];
                if crate::notify::is_post_mention(user, p, &kw, &list.posts, &mut by_thread, check_comments) {
                    count += 1;
                    if p.root_id.is_empty() {
                        count_root += 1;
                        root_mentions.push(id.clone());
                    }
                }
            }
            if list.order.len() < 200 {
                break;
            }
            page += 1;
        }
        if self.post_priority_on() {
            urgent = self
                .priority_for_posts(&root_mentions)
                .await?
                .values()
                .filter(|p| p.priority.as_deref() == Some("urgent"))
                .count() as i64;
        }
        Ok((count, count_root, urgent))
    }

    /// `GetAllChannelMembersNotifyPropsForChannel` through its cache.
    pub async fn channel_notify_props(
        &self,
        channel_id: &str,
    ) -> ApiResult<Arc<HashMap<String, crate::model::StringMap>>> {
        if let Some(m) = self.caches.members_notify_props.get(channel_id) {
            return Ok(m);
        }
        let rows: Vec<(String, Option<crate::model::StringMap>)> =
            sqlx::query_as("SELECT UserId, NotifyProps FROM ChannelMembers WHERE ChannelId = $1")
                .bind(channel_id)
                .fetch_all(&self.db)
                .await?;
        let m: HashMap<_, _> = rows.into_iter().map(|(u, p)| (u, p.unwrap_or_default())).collect();
        let m = Arc::new(m);
        self.caches.members_notify_props.put(channel_id.to_string(), m.clone());
        Ok(m)
    }

    /// `MarkChannelAsUnreadFromPost`.
    pub async fn mark_unread_from_post(&self, post_id: &str, user_id: &str, crt_supported: bool) -> ApiResult<Value> {
        let post = self.single_post(post_id, false).await?;
        let user = self.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
        let crt = crt_supported && self.crt_for_user(user_id).await;
        if !crt && !post.root_id.is_empty() {
            // markChannelAsUnreadFromPostCRTUnsupported for a reply: the thread, not the channel.
            let root = self.single_post(&post.root_id, false).await?;
            let c = self.channel_or_404(&post.channel_id).await?;
            if self.cfg.bool("ServiceSettings.ThreadAutoFollow") {
                let mut m = match self.thread_membership(user_id, &post.root_id).await? {
                    Some(m) => m,
                    None => {
                        self.maintain_membership(
                            user_id,
                            &post.root_id,
                            crate::threads::MembershipOpts {
                                following: true,
                                update_following: true,
                                ..Default::default()
                            },
                        )
                        .await?
                    }
                };
                m.following = true;
                m.last_viewed = post.create_at - 1;
                m.unread_mentions = self.count_thread_mentions(&user, &root, &c.team_id, post.create_at - 1).await?;
                self.update_membership(&m).await?;
                if let Some(t) = self.thread_for_user(&m, true).await? {
                    self.hub.publish(
                        "thread_updated",
                        json!({"thread": serde_json::to_string(&t).unwrap()}),
                        Broadcast { team_id: c.team_id.clone(), user_id: user_id.into(), ..Default::default() },
                    );
                }
            }
            crate::push::update_badge(self, user_id).await;
            return Ok(json!({"team_id": c.team_id, "channel_id": c.id, "msg_count": 0, "mention_count": 0,
                "mention_count_root": 0, "urgent_mention_count": 0, "msg_count_root": 0, "last_viewed_at": 0}));
        }
        let (mentions, mentions_root, urgent) = self.count_mentions_from_post(&user, &post).await?;
        let unread_at = post.create_at - 1;
        let jl: Vec<String> = kind::JOIN_LEAVE_TYPES.iter().map(|s| s.to_string()).collect();
        let (unread, unread_root): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COUNT(*) FILTER (WHERE RootId = '') FROM Posts WHERE ChannelId = $1 AND CreateAt > $2
             AND NOT (Type = ANY($3)) AND DeleteAt = 0",
        )
        .bind(&post.channel_id)
        .bind(unread_at)
        .bind(&jl)
        .fetch_one(&self.db)
        .await?;
        sqlx::query(
            "UPDATE ChannelMembers SET MentionCount = $1, MentionCountRoot = $2, UrgentMentionCount = $3,
                 MsgCount = (SELECT TotalMsgCount FROM Channels WHERE Id = $6) - $4,
                 MsgCountRoot = (SELECT TotalMsgCountRoot FROM Channels WHERE Id = $6) - $5,
                 LastViewedAt = $7, LastUpdateAt = $8
             WHERE UserId = $9 AND ChannelId = $6",
        )
        .bind(mentions)
        .bind(mentions_root)
        .bind(urgent)
        .bind(unread)
        .bind(unread_root)
        .bind(&post.channel_id)
        .bind(unread_at)
        .bind(now_ms())
        .bind(user_id)
        .execute(&self.db)
        .await?;
        let r: (String, String, String, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT COALESCE(c.TeamId, ''), cm.UserId, cm.ChannelId, COALESCE(cm.MsgCount, 0), COALESCE(cm.MentionCount, 0),
                    COALESCE(cm.MentionCountRoot, 0), COALESCE(cm.UrgentMentionCount, 0), COALESCE(cm.MsgCountRoot, 0),
                    COALESCE(cm.LastViewedAt, 0)
             FROM ChannelMembers cm LEFT JOIN Channels c ON c.Id = cm.ChannelId
             WHERE cm.UserId = $1 AND cm.ChannelId = $2 AND c.DeleteAt = 0",
        )
        .bind(user_id)
        .bind(&post.channel_id)
        .fetch_one(&self.db)
        .await?;
        let state = json!({
            "team_id": r.0, "user_id": r.1, "channel_id": r.2, "msg_count": r.3, "mention_count": r.4,
            "mention_count_root": r.5, "urgent_mention_count": r.6, "msg_count_root": r.7, "last_viewed_at": r.8,
        });
        self.caches.invalidate_members_for_user(user_id);
        self.hub.publish(
            "post_unread",
            json!({"msg_count": r.3, "msg_count_root": r.7, "mention_count": r.4, "mention_count_root": r.5,
                   "urgent_mention_count": r.6, "last_viewed_at": r.8, "post_id": post_id}),
            Broadcast { team_id: r.0.clone(), channel_id: r.2.clone(), user_id: user_id.into(), ..Default::default() },
        );
        crate::push::update_badge(self, user_id).await;
        Ok(state)
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UnreadBody {
    collapsed_threads_supported: bool,
}

async fn set_unread(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, post_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    let b: UnreadBody = serde_json::from_slice(&body).unwrap_or_default();
    if s.user_id != user_id && !app.user_perm(&s, &user_id).await {
        return Err(AppError::forbidden_perm(perm::EDIT_OTHER_USERS));
    }
    if !app.read_post_perm(&s, &post_id).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    Ok(Json(app.mark_unread_from_post(&post_id, &user_id, b.collapsed_threads_supported).await?))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReminderBody {
    target_time: i64,
}

/// `SetPostReminder`: saved; the reminder job posts it when due.
async fn set_reminder(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, post_id)): Path<(String, String)>,
    Body(b): Body<ReminderBody>,
) -> ApiResult<Json<Value>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    if s.user_id != user_id && !app.user_perm(&s, &user_id).await {
        return Err(AppError::forbidden_perm(perm::EDIT_OTHER_USERS));
    }
    if !app.read_post_perm(&s, &post_id).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    let post = app.single_post(&post_id, false).await?;
    // Go takes any target time (setPostReminder decodes it and stores it as is); a past one fires at the next run.
    sqlx::query(
        "INSERT INTO PostReminders (PostId, UserId, TargetTime) VALUES ($1, $2, $3)
         ON CONFLICT (PostId, UserId) DO UPDATE SET TargetTime = EXCLUDED.TargetTime",
    )
    .bind(&post.id)
    .bind(&user_id)
    .bind(b.target_time)
    .execute(&app.db)
    .await?;
    // The acknowledgement, to the user alone (`SetPostReminder`): an ephemeral post in the thread of the reminded post,
    // with what the client needs to render it in the user's timezone. GetPostReminderMetadata: the channel, its
    // team's name ('' for DMs) and the author's username.
    let (team_name, username): (String, String) = sqlx::query_as(
        "SELECT COALESCE(t.Name, ''), u.Username FROM Posts p JOIN Channels c ON p.ChannelId = c.Id
         LEFT JOIN Teams t ON c.TeamId = t.Id JOIN Users u ON p.UserId = u.Id WHERE p.Id = $1",
    )
    .bind(&post.id)
    .fetch_one(&app.db)
    .await?;
    let site_url = app.cfg.str("ServiceSettings.SiteURL").to_string();
    let permalink = if team_name.is_empty() {
        format!("{site_url}/pl/{}", post.id)
    } else {
        format!("{site_url}/{team_name}/pl/{}", post.id)
    };
    // time.RFC822 in UTC: "02 Jan 06 15:04 UTC".
    let parsed = chrono::DateTime::from_timestamp(b.target_time, 0)
        .map(|t| t.format("%d %b %y %H:%M UTC").to_string())
        .unwrap_or_default();
    let mut eph = Post {
        user_id: user_id.clone(),
        root_id: if post.root_id.is_empty() { post.id.clone() } else { post.root_id.clone() },
        channel_id: post.channel_id.clone(),
        message: format!("You will be reminded about {permalink} by @{username} at {parsed}"),
        ..Default::default()
    };
    eph.props.0.insert("target_time".into(), json!(b.target_time));
    eph.props.0.insert("team_name".into(), json!(team_name));
    eph.props.0.insert("post_id".into(), json!(post.id));
    eph.props.0.insert("username".into(), json!(username));
    eph.props.0.insert("type".into(), json!("reminder"));
    app.send_ephemeral(&user_id, eph).await;
    Ok(ok())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AroundQuery {
    limit_before: Option<String>,
    limit_after: Option<String>,
    #[serde(rename = "skipFetchThreads")]
    skip_fetch_threads: String,
    #[serde(rename = "collapsedThreads")]
    collapsed_threads: String,
    #[serde(rename = "collapsedThreadsExtended")]
    collapsed_threads_extended: String,
}

/// `getPostsForChannelAroundLastUnread`.
async fn posts_around_unread(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, channel_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(q): Query<AroundQuery>,
) -> ApiResult<Response> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&channel_id) {
        return Err(AppError::invalid_url_param("channel_id"));
    }
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    app.require_user(&s, &user_id).await?;
    let c = app.channel_or_404(&channel_id).await?;
    if !app.can_read_channel(&s, &c).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    // `web.Params`: a bad or negative limit is the default, a big one the maximum.
    let num = |v: &Option<String>| match v.as_deref().and_then(|x| x.parse::<i64>().ok()) {
        Some(n) if n >= 0 => n.min(200),
        _ => 60,
    };
    let (before, after) = (num(&q.limit_before), num(&q.limit_after));
    if after == 0 {
        return Err(AppError::invalid_url_param("limit_after"));
    }
    let crt = parse_bool(&q.collapsed_threads).unwrap_or(false);
    let o = GetPostsOpts {
        channel_id: channel_id.clone(),
        skip_fetch_threads: parse_bool(&q.skip_fetch_threads).unwrap_or(false),
        collapsed_threads: crt,
        collapsed_threads_extended: parse_bool(&q.collapsed_threads_extended).unwrap_or(false),
        user_id: user_id.clone(),
        exclude_membership: excludes_membership(&c),
        ..Default::default()
    };
    let last_viewed: i64 =
        sqlx::query_scalar("SELECT COALESCE(LastViewedAt, 0) FROM ChannelMembers WHERE ChannelId = $1 AND UserId = $2")
            .bind(&channel_id)
            .bind(&user_id)
            .fetch_optional(&app.db)
            .await?
            .ok_or_else(|| {
                AppError::not_found(
                    "app.channel.get_member.missing.app_error",
                    "No channel member found for that user ID and channel ID.",
                )
            })?;
    // GetPostsForChannelAroundLastUnread: the first unread post, then posts before and after it. A channel never
    // viewed (LastViewedAt 0) has no such post in Go: the handler then answers the latest page, as when all is read.
    let root = if crt { " AND RootId = ''" } else { "" };
    let first: Option<(String, i64)> = if last_viewed == 0 {
        None
    } else {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT Id, CreateAt FROM Posts WHERE ChannelId = $1 AND CreateAt > $2 AND DeleteAt = 0{root}{} ORDER BY CreateAt ASC LIMIT 1",
        exclude_membership("Posts", o.exclude_membership)
    )))
        .bind(&channel_id)
        .bind(last_viewed)
        .fetch_optional(&app.db)
        .await?
    };
    // The latest page, which nothing unread falls back to, carries the channel's ETag.
    let mut etag = None;
    let mut list = match first {
        None => {
            let e = app.posts_etag(&c).await;
            if let Some(r) = crate::teams::not_modified(&headers, &e) {
                return Ok(r);
            }
            etag = Some(e);
            let mut l = app.get_posts(&GetPostsOpts { per_page: before, ..o.clone() }).await?;
            l.next_post_id.clear();
            l
        }
        Some((id, _)) => {
            let mut l = app
                .get_post_thread(
                    &id,
                    &GetPostsOpts {
                        skip_fetch_threads: o.skip_fetch_threads,
                        collapsed_threads: crt,
                        collapsed_threads_extended: o.collapsed_threads_extended,
                        user_id: user_id.clone(),
                        ..Default::default()
                    },
                )
                .await?;
            l.order = vec![id.clone()];
            let older = app
                .get_posts_around(&GetPostsOpts { post_id: id.clone(), per_page: before, ..o.clone() }, true)
                .await?;
            let newer = app
                .get_posts_around(&GetPostsOpts { post_id: id.clone(), per_page: after - 1, ..o.clone() }, false)
                .await?;
            let mut order = newer.order.clone();
            order.push(id);
            order.extend(older.order.clone());
            l.posts.extend(older.posts);
            l.posts.extend(newer.posts);
            l.order = order;
            l
        }
    };
    let next_set = list.order.is_empty();
    let first_post = list.order.first().and_then(|i| list.posts.get(i)).map(|p| p.create_at);
    let last_post = list.order.last().and_then(|i| list.posts.get(i)).map(|p| p.create_at);
    if !next_set && let Some(t) = first_post {
        list.next_post_id = app.post_id_around(&channel_id, t, false, crt, o.exclude_membership).await;
    }
    if let Some(t) = last_post {
        list.prev_post_id = app.post_id_around(&channel_id, t, true, crt, o.exclude_membership).await;
    }
    let list = app.prepare_list(list).await;
    let body = Json(app.sanitize_list_for_user(list, &s.user_id).await);
    Ok(match etag {
        Some(e) => crate::teams::with_etag(body, &e),
        None => body.into_response(),
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TypingBody {
    channel_id: String,
    parent_id: String,
}

/// `publishUserTyping` (REST): another user's typing only for system admins, and the user (not the session) must
/// be able to post in the channel.
async fn user_typing(
    St(app): St<State>,
    BusyAuth(s): BusyAuth,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let user_id = crate::preferences::user_param(&s, &user_id)?;
    let b: TypingBody = crate::app::decode_json(&body, "typing_request")?;
    if user_id != s.user_id && !app.has_perm(&s, perm::MANAGE_SYSTEM).await {
        return Err(AppError::forbidden_perm(perm::MANAGE_SYSTEM));
    }
    if !app.user_channel_perm(&user_id, &b.channel_id, perm::CREATE_POST).await.0 {
        return Err(AppError::forbidden_perm(perm::CREATE_POST));
    }
    crate::hub::publish_typing(&app, &user_id, &b.channel_id, &b.parent_id);
    Ok(ok())
}

/// `SendTestMessage`: the system bot's message in its DM with the user, notifying even when muted.
async fn test_notification(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    let bot: Option<(String,)> = sqlx::query_as(
        "SELECT u.Id FROM Users u JOIN Bots b ON b.UserId = u.Id WHERE u.Username = 'system-bot' AND u.DeleteAt = 0",
    )
    .fetch_optional(&app.db)
    .await?;
    let Some((bot,)) = bot else {
        return Err(AppError::internal("app.notifications.send_test_message.errors.no_bot", "no system bot"));
    };
    let name = channel::direct_name(&s.user_id, &bot);
    let c: Option<(String,)> = sqlx::query_as("SELECT Id FROM Channels WHERE Name = $1 AND Type = 'D'")
        .bind(&name)
        .fetch_optional(&app.db)
        .await?;
    let Some((cid,)) = c else {
        return Err(AppError::internal(
            "app.notifications.send_test_message.errors.no_channel",
            "no direct channel with the system bot",
        ));
    };
    let c = app.channel_or_404(&cid).await?;
    let post = Post {
        channel_id: cid,
        message: "This is a test notification. If you received this as a push notification on your phone, your notifications are working.".into(),
        user_id: bot,
        ..Default::default()
    };
    app.create_post_in(post, &c, CreateFlags { force_notification: true, ..Default::default() }, None)
        .await
        .map_err(|e| AppError::internal("app.notifications.send_test_message.errors.create_post", e.detailed_error))?;
    Ok(ok())
}

#[cfg(test)]
mod hashtag_tests {
    use super::parse_hashtags;

    #[test]
    fn hashtags() {
        for (text, want) in [
            ("#ab #a1 #Ab #тег", "#ab #a1 #Ab #тег"),
            ("##double (#ab) [#ab] #ab. #ab, #ab!", "#double #ab #ab #ab #ab #ab"),
            ("#a #12ab #_x # foo#bar", ""),
            ("#tag #tag #TAG", "#tag #tag #TAG"),
        ] {
            assert_eq!(parse_hashtags(text), want, "{text}");
        }
        let many: Vec<String> = (0..150).map(|i| format!("#tag{i:03}")).collect();
        let out = parse_hashtags(&many.join(" "));
        assert_eq!(out.len(), 991);
        assert!(out.ends_with("#tag123"));
    }
}

#[cfg(test)]
mod etag_tests {
    use super::{list_etag, post_etag};
    use crate::model::{Post, PostList};

    #[test]
    fn etags() {
        let v = crate::SERVER_VERSION;
        let p = |id: &str, t: i64| Post { id: id.into(), update_at: t, ..Default::default() };
        assert_eq!(post_etag(&p("a", 5)), format!("{v}.a.5"));
        let mut l = PostList::default();
        assert_eq!(list_etag(&l), format!("{v}..0.0"));
        for x in [p("b", 7), p("c", 7), p("a", 3)] {
            l.add_ordered(x);
        }
        assert_eq!(list_etag(&l), format!("{v}.b.c.7"));
    }
}
