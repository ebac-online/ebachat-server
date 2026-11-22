//! Permanent deletion of a user, a team or a channel (`?permanent=true` with `EnableAPIUserDeletion`,
//! `EnableAPITeamDeletion`, `EnableAPIChannelDeletion`): every row Go removes, in Go's order.

// Based on Mattermost server/channels/app/user.go (PermanentDeleteUser), app/team.go (PermanentDeleteTeam),
// app/channel.go (PermanentDeleteChannel, cleanupChannelAccessControlPolicy), app/file.go
// (RemoveFilesFromFileStore) and the store/sqlstore functions they call, Copyright (c) 2015-present Mattermost,
// Inc. Licensed under AGPL-3.0.

use axum::http::StatusCode;
use serde_json::json;

use crate::app::{ApiResult, App, AppError, now_ms};
use crate::model::{Channel, Team, User};

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

fn internal(id: &str) -> impl Fn(sqlx::Error) -> AppError + '_ {
    move |e| AppError::internal(id, e.to_string())
}

/// `permanentDeleteThreads`, `permanentDeleteReactions`, `permanentDeleteTemporaryPosts`,
/// `permanentDeleteReadReceipts` for these posts.
async fn delete_post_data(tx: &mut Tx<'_>, ids: &[String], threads: bool) -> Result<(), sqlx::Error> {
    if threads {
        sqlx::query("DELETE FROM Threads WHERE PostId = ANY($1)").bind(ids).execute(&mut **tx).await?;
        sqlx::query("DELETE FROM ThreadMemberships WHERE PostId = ANY($1)").bind(ids).execute(&mut **tx).await?;
    }
    sqlx::query("DELETE FROM Reactions WHERE PostId = ANY($1)").bind(ids).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM TemporaryPosts WHERE PostId = ANY($1)").bind(ids).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM ReadReceipts WHERE PostId = ANY($1)").bind(ids).execute(&mut **tx).await?;
    Ok(())
}

impl App {
    /// `SqlPostStore.PermanentDeleteByUser`: the user's replies (their threads updated), then their root posts with
    /// everything under them, at most 10 batches of 1000.
    async fn delete_posts_by_user(&self, user_id: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.db.begin().await?;
        let replies: Vec<(String, String)> =
            sqlx::query_as("SELECT Id, RootId FROM Posts WHERE UserId = $1 AND RootId != ''")
                .bind(user_id)
                .fetch_all(&mut *tx)
                .await?;
        sqlx::query("DELETE FROM Posts WHERE UserId = $1 AND RootId != ''").bind(user_id).execute(&mut *tx).await?;
        for (_, root) in &replies {
            // updateThreadAfterReplyDeletion
            let left: i64 =
                sqlx::query_scalar("SELECT COUNT(Posts.Id) FROM Posts WHERE Posts.RootId = $1 AND Posts.UserId = $2 AND Posts.DeleteAt = 0")
                    .bind(root)
                    .bind(user_id)
                    .fetch_one(&mut *tx)
                    .await?;
            let participants = if left == 0 { "Participants = Participants - $2::text, " } else { "" };
            let sql = format!(
                "UPDATE Threads SET {participants}LastReplyAt = (SELECT COALESCE(MAX(CreateAt), 0) FROM Posts \
                 WHERE RootId = $1 AND DeleteAt = 0), ReplyCount = (SELECT Count(*) FROM Posts WHERE RootId = $1 \
                 AND DeleteAt = 0) WHERE PostId = $1 AND ReplyCount > 0"
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(root);
            if left == 0 {
                q = q.bind(user_id);
            }
            q.execute(&mut *tx).await?;
        }
        let ids: Vec<String> = replies.into_iter().map(|r| r.0).collect();
        delete_post_data(&mut tx, &ids, false).await?;
        tx.commit().await?;
        for _ in 0..10 {
            let ids: Vec<String> = sqlx::query_scalar("SELECT Id FROM Posts WHERE UserId = $1 LIMIT 1000")
                .bind(user_id)
                .fetch_all(&self.db)
                .await?;
            if ids.is_empty() {
                return Ok(());
            }
            // permanentDelete: the posts' threads, reactions, receipts, their replies, then the posts.
            let mut tx = self.db.begin().await?;
            delete_post_data(&mut tx, &ids, true).await?;
            sqlx::query("DELETE FROM Posts WHERE RootId = ANY($1)").bind(&ids).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM Posts WHERE Id = ANY($1)").bind(&ids).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        Err(sqlx::Error::Protocol(format!(
            "permanently deleting posts for user: limit exceeded (10000), userId={user_id}"
        )))
    }

    /// `SqlReactionStore.PermanentDeleteByUser`: the user's reactions, and `HasReactions` of their posts again.
    async fn delete_reactions_by_user(&self, user_id: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.db.begin().await?;
        let posts: Vec<String> = sqlx::query_scalar("SELECT PostId FROM Reactions WHERE UserId = $1")
            .bind(user_id)
            .fetch_all(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM Reactions WHERE PostId = ANY($1) AND UserId = $2")
            .bind(&posts)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let now = now_ms();
        let mut tx = self.db.begin().await?;
        for p in &posts {
            let _ = sqlx::query(
                "UPDATE Posts SET UpdateAt = $1, HasReactions = (SELECT count(0) > 0 FROM Reactions WHERE PostId = $2 \
                 AND COALESCE(DeleteAt, 0) = 0) WHERE Id = $2",
            )
            .bind(now)
            .bind(p)
            .execute(&mut *tx)
            .await;
        }
        tx.commit().await
    }

    /// `RemoveFilesFromFileStore`: a file that is not there is no error.
    async fn remove_files(&self, infos: &[(String, String, String)]) {
        for (path, thumb, preview) in infos {
            let _ = self.files.remove(path).await;
            if !preview.is_empty() {
                let _ = self.files.remove(preview).await;
            }
            if !thumb.is_empty() {
                let _ = self.files.remove(thumb).await;
            }
        }
    }

    /// `PermanentDeleteUser`. Go answers 202 when the profile image could not be removed.
    pub async fn permanent_delete_user(&self, user: User) -> ApiResult<()> {
        crate::logs::warn(
            "Attempting to permanently delete account",
            &[("user_id", user.id.clone().into()), ("user_email", user.email.clone().into())],
        );
        let (id, email) = (user.id.clone(), user.email.clone());
        self.update_active(user, false).await?;
        let db = &self.db;
        sqlx::query("DELETE FROM Sessions WHERE UserId = $1")
            .bind(&id)
            .execute(db)
            .await
            .map_err(internal("app.session.permanent_delete_sessions_by_user.app_error"))?;
        let mut tx = db.begin().await.map_err(internal("app.user_access_token.delete.app_error"))?;
        sqlx::query("DELETE FROM Sessions s USING UserAccessTokens o WHERE o.Token = s.Token AND o.UserId = $1")
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(internal("app.user_access_token.delete.app_error"))?;
        sqlx::query("DELETE FROM UserAccessTokens WHERE UserId = $1")
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(internal("app.user_access_token.delete.app_error"))?;
        tx.commit().await.map_err(internal("app.user_access_token.delete.app_error"))?;
        for (sql, err) in [
            ("DELETE FROM OAuthAccessData WHERE UserId = $1", "app.oauth.permanent_delete_auth_data_by_user.app_error"),
            (
                "DELETE FROM IncomingWebhooks WHERE UserId = $1",
                "app.webhooks.permanent_delete_incoming_by_user.app_error",
            ),
            (
                "DELETE FROM OutgoingWebhooks WHERE CreatorId = $1",
                "app.webhooks.permanent_delete_outgoing_by_user.app_error",
            ),
            ("DELETE FROM Commands WHERE CreatorId = $1", "app.user.permanentdeleteuser.internal_error"),
            ("DELETE FROM Preferences WHERE UserId = $1", "app.preference.permanent_delete_by_user.app_error"),
            ("DELETE FROM ChannelMembers WHERE UserId = $1", "app.channel.permanent_delete_members_by_user.app_error"),
            ("DELETE FROM GroupMembers WHERE UserId = $1", "app.group.permanent_delete_members_by_user.app_error"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&id).execute(db).await.map_err(internal(err))?;
        }
        self.delete_posts_by_user(&id).await.map_err(internal("app.post.permanent_delete_by_user.app_error"))?;
        self.delete_reactions_by_user(&id)
            .await
            .map_err(internal("app.reaction.permanent_delete_by_user.app_error"))?;
        sqlx::query("DELETE FROM ScheduledPosts WHERE UserId = $1")
            .bind(&id)
            .execute(db)
            .await
            .map_err(internal("app.scheduled_post.permanent_delete_by_user.app_error"))?;
        sqlx::query("DELETE FROM Drafts WHERE UserId = $1")
            .bind(&id)
            .execute(db)
            .await
            .map_err(internal("app.drafts.permanent_delete_by_user.app_error"))?;
        sqlx::query("DELETE FROM Bots WHERE UserId = $1").bind(&id).execute(db).await.map_err(|e| {
            AppError::bad_request("app.bot.permenent_delete.bad_id", "")
                .with_param("user_id", &id)
                .detail(e.to_string())
        })?;
        let infos: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT COALESCE(Path, ''), COALESCE(ThumbnailPath, ''), COALESCE(PreviewPath, '') FROM FileInfo \
             WHERE CreatorId = $1 AND DeleteAt = 0 ORDER BY CreateAt",
        )
        .bind(&id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
        self.remove_files(&infos).await;
        // The profile image directory, when there is a profile image.
        let mut file_errors = false;
        if self.files.exists(&format!("users/{id}/profile.png")).await
            && let Err(e) = self.files.remove_dir(&format!("users/{id}")).await
        {
            file_errors = true;
            crate::logs::warn(
                "Unable to remove profile image directory",
                &[("path", format!("users/{id}").into()), ("error", e.to_string().into())],
            );
        }
        for (sql, err) in [
            ("DELETE from FileInfo WHERE CreatorId = $1", "app.file_info.permanent_delete_by_user.app_error"),
            ("DELETE FROM Users WHERE Id = $1", "app.user.permanent_delete.app_error"),
            ("DELETE FROM Audits WHERE UserId = $1", "app.audit.permanent_delete_by_user.app_error"),
            ("DELETE FROM TeamMembers WHERE UserId = $1", "app.team.remove_member.app_error"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&id).execute(db).await.map_err(internal(err))?;
        }
        // InvalidateCacheForUser
        self.invalidate_user(&id);
        self.invalidate_channel_cache_for_user(&id);
        self.clear_sessions_for_user(&id);
        self.caches.team_ids_for_user.remove(&id);
        if file_errors {
            return Err(AppError::new(StatusCode::ACCEPTED, "app.file_info.permanent_delete_by_user.app_error", "")
                .detail("Couldn't delete profile image of the user."));
        }
        crate::logs::warn("Permanently deleted account", &[("user_email", email.into()), ("user_id", id.into())]);
        Ok(())
    }

    /// `PermanentDeleteChannel`: posts (in batches of 500) with their threads and reactions, members, webhooks,
    /// the channel, its access policy; `channel_deleted`.
    pub async fn permanent_delete_channel(&self, c: &Channel) -> ApiResult<()> {
        let db = &self.db;
        let posts = async {
            let mut tx = db.begin().await?;
            let mut last = String::new();
            loop {
                let ids: Vec<String> = sqlx::query_scalar(
                    "SELECT Id FROM Posts WHERE ChannelId = $1 AND Id > $2 ORDER BY Id ASC LIMIT 500",
                )
                .bind(&c.id)
                .bind(&last)
                .fetch_all(&mut *tx)
                .await?;
                let Some(l) = ids.last() else { break };
                last = l.clone();
                delete_post_data(&mut tx, &ids, true).await?;
                sqlx::query("DELETE FROM Posts WHERE Id = ANY($1)").bind(&ids).execute(&mut *tx).await?;
            }
            tx.commit().await
        };
        posts.await.map_err(internal("app.post.permanent_delete_by_channel.app_error"))?;
        for (sql, err) in [
            ("DELETE FROM ChannelMembers WHERE ChannelId = $1", "app.channel.remove_member.app_error"),
            (
                "DELETE FROM IncomingWebhooks WHERE ChannelId = $1",
                "app.webhooks.permanent_delete_incoming_by_channel.app_error",
            ),
            (
                "DELETE FROM OutgoingWebhooks WHERE ChannelId = $1",
                "app.webhooks.permanent_delete_outgoing_by_channel.app_error",
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&c.id).execute(db).await.map_err(internal(err))?;
        }
        sqlx::query(
            "UPDATE PersistentNotifications SET DeleteAt = $1 FROM Posts WHERE Posts.Id = PersistentNotifications.PostId \
             AND Posts.ChannelId = ANY($2)",
        )
        .bind(now_ms())
        .bind(vec![c.id.clone()])
        .execute(db)
        .await
        .map_err(internal("app.post_persistent_notification.delete_by_channel.app_error"))?;
        let delete_at = now_ms();
        let mut tx = db.begin().await.map_err(internal("app.channel.permanent_delete.app_error"))?;
        sqlx::query("DELETE FROM Channels WHERE Id = $1")
            .bind(&c.id)
            .execute(&mut *tx)
            .await
            .map_err(internal("app.channel.permanent_delete.app_error"))?;
        sqlx::query("DELETE FROM PublicChannels WHERE Id = $1")
            .bind(&c.id)
            .execute(&mut *tx)
            .await
            .map_err(internal("app.channel.permanent_delete.app_error"))?;
        tx.commit().await.map_err(internal("app.channel.permanent_delete.app_error"))?;
        self.delete_access_policy(&c.id).await;
        self.invalidate_channel_cache(c);
        self.invalidate_channel_members_cache(&c.id);
        self.invalidate_channel_posts(&c.id);
        if c.r#type == "S" {
            return Ok(());
        }
        let b = if c.r#type == "O" {
            crate::hub::Broadcast { team_id: c.team_id.clone(), ..Default::default() }
        } else {
            crate::hub::Broadcast { channel_id: c.id.clone(), ..Default::default() }
        };
        self.hub.publish("channel_deleted", json!({"channel_id": c.id, "delete_at": delete_at}), b);
        Ok(())
    }

    /// The access control policy of a channel or team, moved to its history (`AccessControlPolicyStore.Delete`,
    /// the store fallback of a server without access control). A failure is only logged, as Go does.
    async fn delete_access_policy(&self, id: &str) {
        let r = async {
            let mut tx = self.db.begin().await?;
            sqlx::query(
                "INSERT INTO AccessControlPolicyHistory (ID, Name, Type, CreateAt, Revision, Version, Data, Props) \
                 SELECT ID, Name, Type, CreateAt, Revision, Version, Data, Props FROM AccessControlPolicies WHERE ID = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM AccessControlPolicies WHERE ID = $1").bind(id).execute(&mut *tx).await?;
            tx.commit().await
        };
        if let Err(e) = r.await {
            crate::logs::warn(
                "Failed to delete ABAC policy during delete",
                &[("id", id.into()), ("error", e.to_string().into())],
            );
        }
    }

    /// `PermanentDeleteTeam`: marked deleted, then every channel, the members, the commands and the team.
    pub async fn permanent_delete_team(&self, mut team: Team) -> ApiResult<()> {
        team.delete_at = now_ms();
        self.update_team_row(&mut team).await?;
        let channels: Vec<Channel> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{} WHERE TeamId = $1 AND Type IN ('O', 'P', 'G') ORDER BY DisplayName",
            <Channel as crate::db::Table>::SELECT
        )))
        .bind(&team.id)
        .fetch_all(&self.db)
        .await
        .map_err(internal("app.channel.get_channels.get.app_error"))?;
        for c in &channels {
            if let Err(e) = self.permanent_delete_channel(c).await {
                crate::logs::warn(
                    "Error permanently deleting channel during team deletion",
                    &[("channel_id", c.id.clone().into()), ("team_id", team.id.clone().into()), ("error", e.id.into())],
                );
            }
        }
        let spaces: Vec<Channel> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{} WHERE TeamId = $1 AND Type = 'S' ORDER BY Id",
            <Channel as crate::db::Table>::SELECT
        )))
        .bind(&team.id)
        .fetch_all(&self.db)
        .await
        .map_err(internal("app.channel.get_channels.get.app_error"))?;
        for c in &spaces {
            if let Err(e) = self.permanent_delete_channel(c).await {
                crate::logs::warn(
                    "Error permanently deleting space channel during team deletion",
                    &[("channel_id", c.id.clone().into()), ("team_id", team.id.clone().into()), ("error", e.id.into())],
                );
            }
        }
        let members: Vec<String> = sqlx::query_scalar("SELECT UserId FROM TeamMembers WHERE TeamId = $1")
            .bind(&team.id)
            .fetch_all(&self.db)
            .await
            .unwrap_or_default();
        for (sql, err) in [
            ("DELETE FROM TeamMembers WHERE TeamId = $1", "app.team.remove_member.app_error"),
            ("DELETE FROM Commands WHERE TeamId = $1", "app.team.permanentdeleteteam.internal_error"),
            ("DELETE FROM Teams WHERE Id = $1", "app.team.permanent_delete.app_error"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&team.id).execute(&self.db).await.map_err(internal(err))?;
        }
        for u in &members {
            self.caches.team_ids_for_user.remove(u);
            self.clear_sessions_for_user(u);
        }
        self.delete_access_policy(&team.id).await;
        self.send_team_event(&team, "delete_team");
        Ok(())
    }
}
