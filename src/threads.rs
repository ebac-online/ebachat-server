//! Collapsed reply threads: a user's followed threads, their memberships and read state.
//!
//! Go's queries join Threads to the user's ThreadMemberships in a way the planner answers with sequential scans
//! of Threads and PostsPriority (production: billions of rows a day). These start from the user's memberships
//! (index on UserId) and reach Threads, Posts and PostsPriority by primary key, with the same results.

// Based on Mattermost server/channels/api4/user.go (threads), app/user.go (GetThreadsForUser, UpdateThread*),
// app/post.go (countThreadMentions) and store/sqlstore/thread_store.go, Copyright (c) 2015-present Mattermost,
// Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;

use crate::app::Query;
use axum::extract::{Path, State as St};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::app::{ApiResult, App, AppError, Auth, State, is_valid_id, now_ms};
use crate::db::{self, Table};
use crate::hub::Broadcast;
use crate::model::{Post, ThreadMembership, ThreadResponse, Threads, User};
use crate::notify::Keywords;
use crate::perm;
use crate::posts::page_params;
use crate::users::ok;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/users/{user_id}/teams/{team_id}/threads", get(get_threads))
        .route("/api/v4/users/{user_id}/teams/{team_id}/threads/read", put(read_all))
        .route("/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}", get(get_thread))
        .route("/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/following", put(follow).delete(unfollow))
        .route("/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/read/{timestamp}", put(read_thread))
        .route("/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/set_unread/{post_id}", post(set_unread))
}

/// `ThreadMembershipOpts`.
#[derive(Default, Clone, Copy)]
pub struct MembershipOpts {
    pub following: bool,
    pub increment_mentions: bool,
    pub update_following: bool,
    pub update_viewed: bool,
    pub update_participants: bool,
}

/// `GetUserThreadsOpts`.
#[derive(Default, Clone)]
pub struct ThreadsOpts {
    pub since: i64,
    pub before: String,
    pub after: String,
    pub page_size: i64,
    pub unread: bool,
    pub extended: bool,
    pub deleted: bool,
    pub totals_only: bool,
    pub threads_only: bool,
    pub exclude_direct: bool,
}

/// The user's followed threads in the team, from ThreadMemberships (by UserId) to Threads (by PostId);
/// binds `$1` user, `$2` team.
fn followed(team: bool, o: &ThreadsOpts) -> String {
    let team_cond = match (team, o.exclude_direct) {
        (false, _) => "",
        (true, true) => " AND t.ThreadTeamId = $2",
        (true, false) => " AND (t.ThreadTeamId = $2 OR t.ThreadTeamId = '')",
    };
    let deleted = if o.deleted { "" } else { " AND COALESCE(t.ThreadDeleteAt, 0) = 0" };
    format!(
        "SELECT tm.PostId, tm.UserId, tm.LastViewed, tm.LastUpdated, tm.UnreadMentions, t.ChannelId,
                COALESCE(t.ReplyCount, 0) AS ReplyCount, COALESCE(t.LastReplyAt, 0) AS LastReplyAt,
                COALESCE(t.Participants, '[]') AS Participants, COALESCE(t.ThreadDeleteAt, 0) AS ThreadDeleteAt
         FROM ThreadMemberships tm
         CROSS JOIN LATERAL (SELECT * FROM Threads WHERE Threads.PostId = tm.PostId) t
         WHERE tm.UserId = $1 AND tm.Following = true{team_cond}{deleted}
           AND (t.ThreadTeamId = '' OR EXISTS (SELECT 1 FROM ChannelMembers cm WHERE cm.ChannelId = t.ChannelId AND cm.UserId = tm.UserId))"
    )
}

/// A followed thread with its root post.
struct Joined {
    post: Post,
    post_id: String,
    reply_count: i64,
    last_reply_at: i64,
    last_viewed: i64,
    unread_mentions: i64,
    unread_replies: i64,
    participants: Vec<String>,
    delete_at: i64,
    is_urgent: bool,
}

impl<'r> sqlx::FromRow<'r, PgRow> for Joined {
    fn from_row(row: &'r PgRow) -> sqlx::Result<Self> {
        let parts: sqlx::types::Json<Vec<String>> = row.try_get("participants")?;
        Ok(Joined {
            post: Post::from_row(row)?,
            post_id: row.try_get("threadpostid")?,
            reply_count: row.try_get("replycount")?,
            last_reply_at: row.try_get("lastreplyat")?,
            last_viewed: row.try_get::<Option<i64>, _>("lastviewed")?.unwrap_or(0),
            unread_mentions: row.try_get::<Option<i64>, _>("unreadmentions")?.unwrap_or(0),
            unread_replies: row.try_get("unreadreplies")?,
            participants: parts.0,
            delete_at: row.try_get("threaddeleteat")?,
            is_urgent: row.try_get("isurgent")?,
        })
    }
}

impl App {
    /// `GetMembershipForUser`.
    pub async fn thread_membership(&self, user_id: &str, post_id: &str) -> ApiResult<Option<ThreadMembership>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{} WHERE PostId = $1 AND UserId = $2",
            ThreadMembership::SELECT
        )))
        .bind(post_id)
        .bind(user_id)
        .fetch_optional(&self.db)
        .await?)
    }

    /// `UpdateMembership`.
    pub async fn update_membership(&self, m: &ThreadMembership) -> ApiResult<()> {
        sqlx::query(
            "UPDATE ThreadMemberships SET Following = $1, LastViewed = $2, LastUpdated = $3, UnreadMentions = $4
             WHERE PostId = $5 AND UserId = $6",
        )
        .bind(m.following)
        .bind(m.last_viewed)
        .bind(m.last_updated)
        .bind(m.unread_mentions)
        .bind(&m.post_id)
        .bind(&m.user_id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// `MaintainMembership`.
    pub async fn maintain_membership(
        &self,
        user_id: &str,
        post_id: &str,
        o: MembershipOpts,
    ) -> ApiResult<ThreadMembership> {
        let mut tx = self.db.begin().await?;
        let now = now_ms();
        let found: Option<ThreadMembership> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{} WHERE PostId = $1 AND UserId = $2 FOR UPDATE",
            ThreadMembership::SELECT
        )))
        .bind(post_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        let m = match found {
            Some(mut m) => {
                let following_changes = o.update_following && m.following != o.following;
                if following_changes || o.increment_mentions || o.update_viewed {
                    if following_changes {
                        m.following = o.following;
                    }
                    if o.update_viewed {
                        m.last_viewed = now;
                        m.unread_mentions = 0;
                    } else if o.increment_mentions {
                        m.unread_mentions += 1;
                    }
                    m.last_updated = now;
                    sqlx::query(
                        "UPDATE ThreadMemberships SET Following = $1, LastViewed = $2, LastUpdated = $3, UnreadMentions = $4
                         WHERE PostId = $5 AND UserId = $6",
                    )
                    .bind(m.following)
                    .bind(m.last_viewed)
                    .bind(m.last_updated)
                    .bind(m.unread_mentions)
                    .bind(post_id)
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await?;
                }
                m
            }
            None => {
                let m = ThreadMembership {
                    post_id: post_id.into(),
                    user_id: user_id.into(),
                    following: o.following,
                    last_updated: now,
                    last_viewed: if o.update_viewed { now } else { 0 },
                    unread_mentions: i64::from(o.increment_mentions),
                };
                sqlx::query(
                    "INSERT INTO ThreadMemberships (PostId, UserId, Following, LastViewed, LastUpdated, UnreadMentions)
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(&m.post_id)
                .bind(&m.user_id)
                .bind(m.following)
                .bind(m.last_viewed)
                .bind(m.last_updated)
                .bind(m.unread_mentions)
                .execute(&mut *tx)
                .await?;
                if o.update_participants {
                    sqlx::query(
                        "UPDATE Threads SET Participants = Participants || $1::jsonb WHERE PostId = $2 AND NOT Participants ? $3",
                    )
                    .bind(sqlx::types::Json(vec![user_id]))
                    .bind(post_id)
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await?;
                }
                m
            }
        };
        tx.commit().await?;
        Ok(m)
    }

    /// The participants of threads as `ThreadResponse` shows them: sanitized profiles, or only ids.
    async fn participants(&self, ids: &[String], extended: bool) -> ApiResult<HashMap<String, User>> {
        let mut out = HashMap::new();
        if extended {
            let show = self.show_cfg();
            for u in self.users(ids).await? {
                let mut u = (*u).clone();
                u.sanitize_profile(show);
                out.insert(u.id.clone(), u);
            }
        } else {
            // `&model.User{Id: id}`, sanitized as the others.
            let show = self.show_cfg();
            for id in ids {
                let mut u = User { id: id.clone(), ..Default::default() };
                u.sanitize_profile(show);
                out.insert(id.clone(), u);
            }
        }
        Ok(out)
    }

    fn to_response(&self, j: Joined, users: &HashMap<String, User>) -> ThreadResponse {
        let mut post = j.post;
        post.sanitize_non_identity_props();
        let post = (!post.id.is_empty()).then_some(post);
        ThreadResponse {
            id: j.post_id,
            reply_count: j.reply_count,
            last_reply_at: j.last_reply_at,
            last_viewed_at: j.last_viewed,
            participants: j.participants.iter().filter_map(|id| users.get(id).cloned()).collect(),
            post,
            unread_replies: j.unread_replies,
            unread_mentions: j.unread_mentions,
            is_urgent: j.is_urgent,
            delete_at: j.delete_at,
        }
    }

    /// `GetThreadForUser`: the thread of a followed membership, None when not followed or gone.
    pub async fn thread_for_user(&self, m: &ThreadMembership, extended: bool) -> ApiResult<Option<ThreadResponse>> {
        if !m.following {
            return Ok(None);
        }
        let urgent = if self.post_priority_on() {
            "COALESCE((SELECT pp.Priority = 'urgent' FROM PostsPriority pp WHERE pp.PostId = t.PostId), false)"
        } else {
            "false"
        };
        let sql = format!(
            "SELECT {}, t.PostId AS threadpostid, COALESCE(t.ReplyCount, 0) AS replycount, COALESCE(t.LastReplyAt, 0) AS lastreplyat,
                    COALESCE(t.Participants, '[]') AS participants, COALESCE(t.ThreadDeleteAt, 0) AS threaddeleteat,
                    $2::bigint AS lastviewed, $3::bigint AS unreadmentions,
                    (SELECT COUNT(r.Id) FROM Posts r WHERE r.RootId = t.PostId AND r.CreateAt > $2 AND r.DeleteAt = 0) AS unreadreplies,
                    {urgent} AS isurgent
             FROM Threads t LEFT JOIN Posts p ON p.Id = t.PostId WHERE t.PostId = $1",
            db::cols::<Post>("p")
        );
        let j: Option<Joined> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&m.post_id)
            .bind(m.last_viewed)
            .bind(m.unread_mentions)
            .fetch_optional(&self.db)
            .await?;
        let Some(j) = j else { return Ok(None) };
        let users = self.participants(&j.participants, extended).await?;
        Ok(Some(self.to_response(j, &users)))
    }

    /// `GetThreadsForUser`: a page of followed threads and the team's totals.
    pub async fn threads_for_user(&self, user_id: &str, team_id: &str, o: &ThreadsOpts) -> ApiResult<Threads> {
        let mut out = Threads::default();
        let base = followed(!team_id.is_empty(), o);
        let priority = self.post_priority_on();
        if !o.threads_only {
            let urgent = if priority {
                "COALESCE(SUM(f.UnreadMentions) FILTER (WHERE EXISTS (SELECT 1 FROM PostsPriority pp WHERE pp.PostId = f.PostId AND pp.Priority = 'urgent')), 0)"
            } else {
                "0::bigint"
            };
            let sql = format!(
                "WITH f AS ({base}) SELECT COUNT(*), COUNT(*) FILTER (WHERE f.LastViewed < f.LastReplyAt),
                        COALESCE(SUM(f.UnreadMentions), 0)::bigint, {urgent}::bigint FROM f"
            );
            let (total, unread, mentions, urgent): (i64, i64, i64, i64) =
                sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(user_id).bind(team_id).fetch_one(&self.db).await?;
            out.total_unread_threads = unread;
            if !o.unread {
                out.total = total;
            }
            out.total_unread_mentions = mentions;
            out.total_unread_urgent_mentions = urgent;
        }
        if !o.totals_only {
            let mut conds = vec![];
            if o.since > 0 {
                conds.push("(f.LastUpdated >= $3 OR f.LastReplyAt >= $3)".to_string());
            }
            if o.unread {
                conds.push("f.LastViewed < f.LastReplyAt".into());
            }
            let mut order = "DESC";
            if !o.before.is_empty() {
                conds.push("f.LastReplyAt < (SELECT LastReplyAt FROM Threads WHERE PostId = $4)".into());
            }
            if !o.after.is_empty() {
                order = "ASC";
                conds.push("f.LastReplyAt > (SELECT LastReplyAt FROM Threads WHERE PostId = $4)".into());
            }
            let wh = if conds.is_empty() { String::new() } else { format!("WHERE {}", conds.join(" AND ")) };
            let urgent = if priority {
                "COALESCE((SELECT pp.Priority = 'urgent' FROM PostsPriority pp WHERE pp.PostId = f.PostId), false)"
            } else {
                "false"
            };
            let del = if o.deleted { "" } else { " AND r.DeleteAt = 0" };
            let sql = format!(
                "WITH f AS ({base})
                 SELECT {}, f.PostId AS threadpostid, f.ReplyCount AS replycount, f.LastReplyAt AS lastreplyat,
                        f.Participants AS participants, f.ThreadDeleteAt AS threaddeleteat, f.LastViewed AS lastviewed,
                        f.UnreadMentions AS unreadmentions,
                        (SELECT COUNT(r.Id) FROM Posts r WHERE r.RootId = f.PostId AND r.CreateAt > f.LastViewed{del}) AS unreadreplies,
                        {urgent} AS isurgent
                 FROM (SELECT * FROM f {wh} ORDER BY f.LastReplyAt {order} LIMIT $5) f JOIN Posts p ON p.Id = f.PostId
                 ORDER BY f.LastReplyAt {order}",
                db::cols::<Post>("p")
            );
            let anchor = if o.before.is_empty() { &o.after } else { &o.before };
            let rows: Vec<Joined> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(user_id)
                .bind(team_id)
                .bind(o.since)
                .bind(anchor)
                .bind(if o.page_size == 0 { 30 } else { o.page_size })
                .fetch_all(&self.db)
                .await?;
            let mut ids: Vec<String> = vec![];
            for j in &rows {
                for p in &j.participants {
                    if !ids.contains(p) {
                        ids.push(p.clone());
                    }
                }
            }
            let users = self.participants(&ids, o.extended).await?;
            out.threads = Some(rows.into_iter().map(|j| self.to_response(j, &users)).collect());
        }
        if o.unread {
            out.total = out.total_unread_threads;
        }
        Ok(out)
    }

    /// `countThreadMentions`: mentions of the user in the thread's replies from `since` on.
    pub async fn count_thread_mentions(&self, user: &User, root: &Post, _team_id: &str, since: i64) -> ApiResult<i64> {
        let c = self.channel_or_404(&root.channel_id).await?;
        let sql = format!("{} WHERE RootId = $1 AND DeleteAt = 0 AND CreateAt >= $2", Post::SELECT);
        let posts: Vec<Post> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&root.id).bind(since).fetch_all(&self.db).await?;
        if c.is_group_or_direct() {
            let other = c.other_user(&user.id).unwrap_or_default();
            return Ok(posts.iter().filter(|p| p.user_id == other).count() as i64);
        }
        let mut k = Keywords::default();
        k.add_user(user, None, Some(crate::model::status::ONLINE), true);
        Ok(posts
            .iter()
            .filter(|p| p.create_at >= since && crate::notify::explicit_mentions(p, &k).users.contains_key(&user.id))
            .count() as i64)
    }

    /// `UpdateThreadReadForUser`: read up to `timestamp`; `thread_read_changed` sent.
    pub async fn update_thread_read(
        &self,
        session_id: &str,
        user_id: &str,
        team_id: &str,
        thread_id: &str,
        timestamp: i64,
    ) -> ApiResult<ThreadResponse> {
        let user = self.user(user_id).await?.ok_or_else(crate::users::missing_account)?;
        let mut m = self.thread_membership(user_id, thread_id).await?.ok_or_else(|| {
            AppError::not_found(
                "app.user.get_thread_membership_for_user.not_found",
                "Unable to get the user thread membership.",
            )
        })?;
        let prev_mentions = m.unread_mentions;
        let prev_replies: i64 =
            sqlx::query_scalar("SELECT COUNT(Id) FROM Posts WHERE RootId = $1 AND CreateAt > $2 AND DeleteAt = 0")
                .bind(thread_id)
                .bind(m.last_viewed)
                .fetch_one(&self.db)
                .await?;
        let root = self.single_post(thread_id, false).await?;
        m.unread_mentions = self.count_thread_mentions(&user, &root, team_id, timestamp).await?;
        self.update_membership(&m).await?;
        m.last_viewed = timestamp;
        sqlx::query("UPDATE ThreadMemberships SET LastViewed = $1, LastUpdated = $2 WHERE UserId = $3 AND PostId = $4")
            .bind(timestamp)
            .bind(now_ms())
            .bind(user_id)
            .bind(thread_id)
            .execute(&self.db)
            .await?;
        let t = self.thread_for_user(&m, false).await?.ok_or_else(|| {
            AppError::not_found(
                "app.user.get_threads_for_user.not_found",
                "User thread does not exist or is not followed",
            )
        })?;
        if t.unread_replies == 0 && self.is_crt_enabled(user_id).await {
            crate::push::clear(self, session_id, user_id, &root.channel_id, thread_id).await;
        }
        self.hub.publish(
            "thread_read_changed",
            json!({"thread_id": thread_id, "timestamp": timestamp, "unread_mentions": m.unread_mentions,
                   "unread_replies": t.unread_replies, "previous_unread_mentions": prev_mentions,
                   "previous_unread_replies": prev_replies, "channel_id": root.channel_id}),
            Broadcast { team_id: team_id.into(), user_id: user_id.into(), ..Default::default() },
        );
        Ok(t)
    }

    /// `UpdateThreadFollowForUser`.
    pub async fn update_thread_follow(
        &self,
        user_id: &str,
        team_id: &str,
        thread_id: &str,
        state: bool,
    ) -> ApiResult<()> {
        self.maintain_membership(
            user_id,
            thread_id,
            MembershipOpts { following: state, update_following: true, update_viewed: state, ..Default::default() },
        )
        .await?;
        let reply_count: i64 = sqlx::query_scalar("SELECT COALESCE(ReplyCount, 0) FROM Threads WHERE PostId = $1")
            .bind(thread_id)
            .fetch_optional(&self.db)
            .await?
            .unwrap_or(0);
        self.hub.publish(
            "thread_follow_changed",
            json!({"thread_id": thread_id, "state": state, "reply_count": reply_count}),
            Broadcast { team_id: team_id.into(), user_id: user_id.into(), ..Default::default() },
        );
        Ok(())
    }

    /// `MarkAllAsReadByChannels`: the user's threads in these channels read now (channels viewed).
    pub async fn mark_threads_read_in_channels(&self, user_id: &str, channel_ids: &[String]) -> ApiResult<()> {
        if channel_ids.is_empty() {
            return Ok(());
        }
        let now = now_ms();
        sqlx::query(
            "UPDATE ThreadMemberships SET LastViewed = $1, UnreadMentions = 0, LastUpdated = $1 FROM Threads
             WHERE ThreadMemberships.UserId = $2 AND Threads.PostId = ThreadMemberships.PostId
               AND Threads.ChannelId = ANY($3) AND Threads.LastReplyAt > ThreadMemberships.LastViewed",
        )
        .bind(now)
        .bind(user_id)
        .bind(channel_ids)
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ThreadsQuery {
    since: String,
    before: String,
    after: String,
    per_page: Option<String>,
    #[serde(rename = "totalsOnly")]
    totals_only: String,
    #[serde(rename = "threadsOnly")]
    threads_only: String,
    #[serde(rename = "excludeDirect")]
    exclude_direct: String,
    deleted: String,
    unread: String,
    extended: String,
}

fn b(s: &str) -> bool {
    matches!(s, "1" | "t" | "T" | "true" | "TRUE" | "True")
}

fn no_store(v: impl serde::Serialize) -> Response {
    let mut r = Json(v).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

async fn check_ids(s: &crate::model::Session, user_id: &str, team_id: &str) -> ApiResult<String> {
    let user_id = s.resolve(user_id);
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    if !is_valid_id(team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    Ok(user_id)
}

async fn get_threads(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id)): Path<(String, String)>,
    Query(q): Query<ThreadsQuery>,
) -> ApiResult<Response> {
    let user_id = check_ids(&s, &user_id, &team_id).await?;
    app.require_user(&s, &user_id).await?;
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    let (_, per_page) = page_params(None, q.per_page.as_deref());
    let mut o =
        ThreadsOpts { page_size: per_page, before: q.before.clone(), after: q.after.clone(), ..Default::default() };
    if !q.since.is_empty() {
        o.since = q.since.parse::<u64>().map_err(|_| AppError::invalid_param("since"))? as i64;
    }
    o.totals_only = b(&q.totals_only);
    o.threads_only = b(&q.threads_only);
    o.exclude_direct = b(&q.exclude_direct);
    if !o.before.is_empty() && !o.after.is_empty() {
        return Err(AppError::bad_request(
            "api.getThreadsForUser.bad_params",
            "Before and After parameters to getThreadsForUser are mutually exclusive",
        ));
    }
    if o.totals_only && o.threads_only {
        return Err(AppError::bad_request(
            "api.getThreadsForUser.bad_only_params",
            "OnlyThreads and OnlyTotals parameters to getThreadsForUser are mutually exclusive",
        ));
    }
    o.deleted = b(&q.deleted);
    o.unread = b(&q.unread);
    o.extended = b(&q.extended);
    let t = app
        .threads_for_user(&user_id, &team_id, &o)
        .await
        .map_err(|e| AppError::internal("app.user.get_threads_for_user.app_error", e.detailed_error))?;
    Ok(no_store(t))
}

async fn get_thread(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id, thread_id)): Path<(String, String, String)>,
    Query(q): Query<ThreadsQuery>,
) -> ApiResult<Response> {
    let user_id = check_ids(&s, &user_id, &team_id).await?;
    if !is_valid_id(&thread_id) {
        return Err(AppError::invalid_url_param("thread_id"));
    }
    app.require_user(&s, &user_id).await?;
    if !app.read_post_perm(&s, &thread_id).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    let m = app.thread_membership(&user_id, &thread_id).await?.ok_or_else(|| {
        AppError::not_found(
            "app.user.get_thread_membership_for_user.not_found",
            "Unable to get the user thread membership.",
        )
    })?;
    let t = app.thread_for_user(&m, b(&q.extended)).await?.ok_or_else(|| {
        AppError::not_found("app.user.get_threads_for_user.not_found", "User thread does not exist or is not followed")
    })?;
    Ok(no_store(t))
}

async fn read_all(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let user_id = check_ids(&s, &user_id, &team_id).await?;
    app.require_user(&s, &user_id).await?;
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    let now = now_ms();
    sqlx::query(
        "UPDATE ThreadMemberships SET LastViewed = $1, UnreadMentions = 0, LastUpdated = $1 FROM Threads
         WHERE Threads.PostId = ThreadMemberships.PostId AND ThreadMemberships.UserId = $2
           AND (Threads.ThreadTeamId = $3 OR Threads.ThreadTeamId = '')",
    )
    .bind(now)
    .bind(&user_id)
    .bind(&team_id)
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.user.update_threads_read_for_user.app_error", e.to_string()))?;
    app.hub.publish(
        "thread_read_changed",
        json!({}),
        Broadcast { team_id: team_id.clone(), user_id: user_id.clone(), ..Default::default() },
    );
    Ok(ok())
}

async fn follow_change(
    app: &State,
    s: &crate::model::Session,
    user_id: &str,
    team_id: &str,
    thread_id: &str,
    on: bool,
) -> ApiResult<Json<Value>> {
    let user_id = check_ids(s, user_id, team_id).await?;
    if !is_valid_id(thread_id) {
        return Err(AppError::invalid_url_param("thread_id"));
    }
    app.require_user(s, &user_id).await?;
    if !app.read_post_perm(s, thread_id).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    app.update_thread_follow(&user_id, team_id, thread_id, on)
        .await
        .map_err(|e| AppError::internal("app.user.update_thread_follow_for_user.app_error", e.detailed_error))?;
    Ok(ok())
}

async fn follow(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id, thread_id)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    follow_change(&app, &s, &user_id, &team_id, &thread_id, true).await
}

async fn unfollow(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id, thread_id)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    follow_change(&app, &s, &user_id, &team_id, &thread_id, false).await
}

async fn read_thread(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id, thread_id, timestamp)): Path<(String, String, String, String)>,
    uri: axum::http::Uri,
) -> ApiResult<Json<ThreadResponse>> {
    // The route's `{timestamp:[0-9]+}`: anything else is no route at all.
    if timestamp.is_empty() || !timestamp.bytes().all(|b| b.is_ascii_digit()) {
        return Err(crate::api::route_not_found(uri.path()));
    }
    let user_id = check_ids(&s, &user_id, &team_id).await?;
    if !is_valid_id(&thread_id) {
        return Err(AppError::invalid_url_param("thread_id"));
    }
    let timestamp: i64 = timestamp.parse().map_err(|_| AppError::invalid_url_param("timestamp"))?;
    app.require_user(&s, &user_id).await?;
    if !app.read_post_perm(&s, &thread_id).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    Ok(Json(app.update_thread_read(&s.id, &user_id, &team_id, &thread_id, timestamp).await?))
}

async fn set_unread(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, team_id, thread_id, post_id)): Path<(String, String, String, String)>,
) -> ApiResult<Json<ThreadResponse>> {
    let user_id = check_ids(&s, &user_id, &team_id).await?;
    if !is_valid_id(&thread_id) {
        return Err(AppError::invalid_url_param("thread_id"));
    }
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    app.require_user(&s, &user_id).await?;
    if !app.read_post_perm(&s, &thread_id).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    app.update_thread_follow(&user_id, &team_id, &thread_id, true).await?;
    // UpdateThreadReadForUserByPost
    let post = app.single_post(&post_id, false).await?;
    if post.root_id != thread_id && post_id != thread_id {
        return Err(AppError::bad_request(
            "app.user.update_thread_read_for_user_by_post.app_error",
            "Unable to update thread read state.",
        ));
    }
    Ok(Json(app.update_thread_read(&s.id, &user_id, &team_id, &thread_id, post.create_at - 1).await?))
}
