//! Post reminders ("Remind me"): every 5 minutes the due ones are taken out of `PostReminders` and the system bot
//! sends each user a direct message with a link to the post.

// Based on Mattermost server/channels/app/post.go (CheckPostReminders), app/bot.go (GetSystemBot) and
// store/sqlstore/post_store.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::app::{ApiResult, App, AppError, State, new_id, now_ms};
use crate::model::{Post, Session};
use crate::posts::CreateFlags;

const BOT_USERNAME: &str = "system-bot";
const INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The job, on the 5-minute boundaries as Go's `CreateRecurringTaskFromNextIntervalTime`.
pub fn spawn(app: State) {
    tokio::spawn(async move {
        let period = INTERVAL.as_millis() as i64;
        let wait = period - now_ms().rem_euclid(period);
        tokio::time::sleep(Duration::from_millis(wait as u64)).await;
        let mut t = tokio::time::interval(INTERVAL);
        loop {
            t.tick().await;
            if let Err(e) = app.check_post_reminders().await {
                eprintln!("post reminders: {} {}", e.id, e.detailed_error);
            }
        }
    });
}

impl App {
    /// `GetSystemBot` (`GetOrCreateSystemOwnedBot`): its user id, made as Go makes it if missing.
    pub async fn system_bot(&self) -> ApiResult<String> {
        let found: Option<(String, i64)> = sqlx::query_as(
            "SELECT u.Id, COALESCE(b.DeleteAt, 0) FROM Users u JOIN Bots b ON b.UserId = u.Id WHERE u.Username = $1",
        )
        .bind(BOT_USERNAME)
        .fetch_optional(&self.db)
        .await?;
        if let Some((id, deleted)) = found {
            if deleted != 0 {
                // Auto-heal: enabled again, as UpdateBotActive.
                let now = now_ms();
                sqlx::query("UPDATE Bots SET DeleteAt = 0, UpdateAt = $1 WHERE UserId = $2")
                    .bind(now)
                    .bind(&id)
                    .execute(&self.db)
                    .await?;
                sqlx::query("UPDATE Users SET DeleteAt = 0, UpdateAt = $1 WHERE Id = $2")
                    .bind(now)
                    .bind(&id)
                    .execute(&self.db)
                    .await?;
                self.invalidate_user(&id);
            }
            return Ok(id);
        }
        let owner: Option<String> = sqlx::query_scalar(
            "SELECT Id FROM Users WHERE Roles LIKE '%system_admin%' AND DeleteAt = 0 ORDER BY Username LIMIT 1",
        )
        .fetch_optional(&self.db)
        .await?;
        let owner = owner.ok_or_else(|| AppError::internal("app.bot.get_system_bot.empty_admin_list.app_error", ""))?;
        let id = new_id();
        let now = now_ms();
        let notify = r#"{"channel":"true","comments":"never","desktop":"mention","desktop_sound":"true","desktop_threads":"all","email":"true","email_threads":"all","first_name":"false","mention_keys":"","push":"mention","push_status":"online","push_threads":"all","channel_mention_auto_follow_threads":"true"}"#;
        sqlx::query(
            "INSERT INTO Users (Id, CreateAt, UpdateAt, DeleteAt, Username, Password, AuthData, AuthService, Email, EmailVerified,
                 Nickname, FirstName, LastName, Position, Roles, AllowMarketing, Props, NotifyProps, LastPasswordUpdate,
                 LastPictureUpdate, FailedAttempts, Locale, Timezone, MfaActive, MfaSecret, LastLogin, MfaUsedTimestamps)
             VALUES ($1, $2, $2, 0, $3, '', NULL, '', $4, false, '', 'System', '', '', 'system_user', false, '{}', $5::jsonb,
                 $2, 0, 0, 'en', '{\"automaticTimezone\":\"\",\"manualTimezone\":\"\",\"useAutomaticTimezone\":\"true\"}', false, '', 0, NULL)",
        )
        .bind(&id)
        .bind(now)
        .bind(BOT_USERNAME)
        .bind(format!("{BOT_USERNAME}@localhost"))
        .bind(notify)
        .execute(&self.db)
        .await?;
        sqlx::query("INSERT INTO Bots (UserId, Description, OwnerId, CreateAt, UpdateAt, DeleteAt, LastIconUpdate) VALUES ($1, '', $2, $3, $3, 0, 0)")
            .bind(&id)
            .bind(&owner)
            .bind(now)
            .execute(&self.db)
            .await?;
        Ok(id)
    }

    /// `CheckPostReminders`: the due reminders are deleted as they are read (lost if sending fails, as in Go).
    pub async fn check_post_reminders(&self) -> ApiResult<()> {
        let bot = self.system_bot().await?;
        let due: Vec<(String, String)> =
            sqlx::query_as("DELETE FROM PostReminders WHERE TargetTime <= $1 RETURNING PostId, UserId")
                .bind(now_ms() / 1000)
                .fetch_all(&self.db)
                .await?;
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (post_id, user_id) in due {
            grouped.entry(user_id).or_default().push(post_id);
        }
        let site = self.cfg.str("ServiceSettings.SiteURL").to_string();
        let bot_user = self.user(&bot).await?.ok_or_else(crate::users::missing_account)?;
        let bot_session = Session { user_id: bot.clone(), roles: bot_user.roles.clone(), ..Default::default() };
        for (user_id, posts) in grouped {
            let ch = self.get_or_create_direct(&bot_session, &user_id, &bot).await?;
            for post_id in posts {
                let meta: Option<(String, String)> = sqlx::query_as(
                    "SELECT COALESCE(t.Name, ''), u.Username FROM Posts p JOIN Channels c ON p.ChannelId = c.Id
                     LEFT JOIN Teams t ON c.TeamId = t.Id JOIN Users u ON p.UserId = u.Id WHERE p.Id = $1",
                )
                .bind(&post_id)
                .fetch_optional(&self.db)
                .await?;
                let Some((team, username)) = meta else { continue };
                let message = crate::i18n::tr(
                    "app.post_reminder_dm",
                    &[("SiteURL", &site), ("TeamName", &team), ("PostId", &post_id), ("Username", &username)],
                )
                .unwrap_or_default();
                let mut dm = Post {
                    channel_id: ch.id.clone(),
                    message,
                    r#type: "reminder".into(),
                    user_id: bot.clone(),
                    ..Default::default()
                };
                for (k, v) in [("team_name", &team), ("post_id", &post_id), ("username", &username)] {
                    dm.props.0.insert(k.into(), Value::String(v.clone()));
                }
                if let Err(e) =
                    self.create_post_in(dm, &ch, CreateFlags { set_online: true, ..Default::default() }, None).await
                {
                    eprintln!("post reminder for {user_id}: {} {}", e.id, e.detailed_error);
                }
            }
        }
        Ok(())
    }
}
