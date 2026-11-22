//! Reactions to posts, through the reaction cache.

// Based on Mattermost server/channels/api4/reaction.go, app/reaction.go and store/sqlstore/reaction_store.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashSet;
use std::sync::LazyLock;

use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, Body, State, is_valid_id, now_ms};
use crate::hub::Broadcast;
use crate::model::{Post, Reaction};
use crate::perm;
use crate::users::ok;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/reactions", post(save_reaction))
        .route("/api/v4/posts/{post_id}/reactions", get(get_reactions))
        .route("/api/v4/users/{user_id}/posts/{post_id}/reactions/{emoji_name}", delete(delete_reaction))
}

/// `model.SystemEmojis`' names.
static SYSTEM_EMOJI: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| include_str!("system_emoji.txt").lines().filter(|l| !l.is_empty()).collect());

/// `GetSystemEmojiId` found.
pub fn is_system_emoji(name: &str) -> bool {
    SYSTEM_EMOJI.contains(name)
}

impl App {
    fn send_reaction_event(&self, event: &str, r: &Reaction, post: &Post) {
        self.hub.publish(
            event,
            json!({"reaction": serde_json::to_string(r).unwrap()}),
            Broadcast { channel_id: post.channel_id.clone(), ..Default::default() },
        );
    }

    /// `SaveReactionForPost`.
    pub async fn save_reaction(&self, mut r: Reaction) -> ApiResult<Reaction> {
        r.emoji_name = r.emoji_name.to_lowercase();
        let post = self.single_post(&r.post_id, false).await?;
        if !is_system_emoji(&r.emoji_name)
            && self.emojis_by_names(std::slice::from_ref(&r.emoji_name)).await?.is_empty()
        {
            return Err(AppError::not_found("app.emoji.get_by_name.no_result", "Unable to find the emoji."));
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM Reactions WHERE PostId = $1 AND EmojiName = $2 AND COALESCE(DeleteAt, 0) = 0)",
        )
        .bind(&r.post_id)
        .bind(&r.emoji_name)
        .fetch_one(&self.db)
        .await?;
        if !exists {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(DISTINCT EmojiName) FROM Reactions WHERE PostId = $1 AND DeleteAt = 0",
            )
            .bind(&r.post_id)
            .fetch_one(&self.db)
            .await?;
            if n >= self.cfg.i64("ServiceSettings.UniqueEmojiReactionLimitPerPost") {
                return Err(AppError::bad_request(
                    "app.reaction.save.save.too_many_reactions",
                    "Reaction limit has been reached for this post.",
                ));
            }
        }
        let c = self.channel_or_404(&post.channel_id).await?;
        if c.delete_at > 0 {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "api.reaction.save.archived_channel.app_error",
                "You cannot react in an archived channel.",
            ));
        }
        r.channel_id = post.channel_id.clone();
        // Reaction.PreSave
        let now = now_ms();
        if r.create_at == 0 {
            r.create_at = now;
        }
        r.update_at = now;
        r.delete_at = 0;
        if r.remote_id.is_none() {
            r.remote_id = Some(String::new());
        }
        let mut tx = self.db.begin().await?;
        let ins = sqlx::query(
            "INSERT INTO Reactions (UserId, PostId, EmojiName, CreateAt, UpdateAt, DeleteAt, RemoteId, ChannelId)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (UserId, PostId, EmojiName) DO UPDATE SET UpdateAt = $5, DeleteAt = $6, RemoteId = $7, ChannelId = $8",
        )
        .bind(&r.user_id)
        .bind(&r.post_id)
        .bind(&r.emoji_name)
        .bind(r.create_at)
        .bind(r.update_at)
        .bind(r.delete_at)
        .bind(&r.remote_id)
        .bind(&r.channel_id)
        .execute(&mut *tx)
        .await;
        match ins {
            Ok(_) => {
                sqlx::query("UPDATE Posts SET HasReactions = true, UpdateAt = $1 WHERE Id = $2")
                    .bind(now_ms())
                    .bind(&r.post_id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
            }
            Err(e) => {
                let unique = e.as_database_error().is_some_and(|d| d.is_unique_violation());
                if !unique {
                    return Err(AppError::internal("app.reaction.save.save.app_error", e.to_string()));
                }
            }
        }
        self.caches.reactions.remove(&r.post_id);
        self.invalidate_channel_posts(&c.id);
        self.send_reaction_event("reaction_added", &r, &post);
        Ok(r)
    }

    /// `DeleteReactionForPost`.
    pub async fn delete_reaction(&self, mut r: Reaction) -> ApiResult<()> {
        r.emoji_name = r.emoji_name.to_lowercase();
        let post = self.single_post(&r.post_id, false).await?;
        let c = self.channel_or_404(&post.channel_id).await?;
        if c.delete_at > 0 {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "api.reaction.delete.archived_channel.app_error",
                "You cannot remove a reaction in an archived channel.",
            ));
        }
        let now = now_ms();
        r.update_at = now;
        if r.remote_id.is_none() {
            r.remote_id = Some(String::new());
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE Reactions SET UpdateAt = $1, DeleteAt = $1, RemoteId = $2 WHERE PostId = $3 AND UserId = $4 AND EmojiName = $5")
            .bind(now)
            .bind(&r.remote_id)
            .bind(&r.post_id)
            .bind(&r.user_id)
            .bind(&r.emoji_name)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::internal("app.reaction.delete_all_with_emoji_name.get_reactions.app_error", e.to_string()))?;
        sqlx::query(
            "UPDATE Posts SET UpdateAt = $1, HasReactions = (SELECT count(0) > 0 FROM Reactions WHERE PostId = $2 AND COALESCE(DeleteAt, 0) = 0)
             WHERE Id = $2",
        )
        .bind(now_ms())
        .bind(&r.post_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.caches.reactions.remove(&r.post_id);
        self.invalidate_channel_posts(&c.id);
        self.send_reaction_event("reaction_removed", &r, &post);
        Ok(())
    }
}

async fn save_reaction(St(app): St<State>, Auth(s): Auth, Body(r): Body<Reaction>) -> ApiResult<Json<Reaction>> {
    let mut r = r;
    r.emoji_name = r.emoji_name.to_lowercase();
    if !is_valid_id(&r.user_id) || !is_valid_id(&r.post_id) || r.emoji_name.is_empty() || r.emoji_name.len() > 64 {
        return Err(AppError::bad_request("api.reaction.save_reaction.invalid.app_error", "Reaction is not valid."));
    }
    if r.user_id != s.user_id {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "api.reaction.save_reaction.user_id.app_error",
            "You cannot save reaction for the other user.",
        ));
    }
    if !app.post_channel_perm(&s, &r.post_id, perm::ADD_REACTION).await? {
        return Err(AppError::forbidden_perm(perm::ADD_REACTION));
    }
    Ok(Json(app.save_reaction(r).await?))
}

async fn get_reactions(
    St(app): St<State>,
    Auth(s): Auth,
    Path(post_id): Path<String>,
) -> ApiResult<Json<Option<Vec<Reaction>>>> {
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    if !app.read_post_perm(&s, &post_id).await?.0 {
        return Err(AppError::forbidden_perm(perm::READ_CHANNEL_CONTENT));
    }
    // No reactions is Go's nil slice: `null`.
    let list = app.reactions_for_post(&post_id).await?;
    Ok(Json((!list.is_empty()).then(|| (*list).clone())))
}

async fn delete_reaction(
    St(app): St<State>,
    Auth(s): Auth,
    Path((user_id, post_id, emoji_name)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let user_id = s.resolve(&user_id);
    if !is_valid_id(&user_id) {
        return Err(AppError::invalid_url_param("user_id"));
    }
    if !is_valid_id(&post_id) {
        return Err(AppError::invalid_url_param("post_id"));
    }
    if emoji_name.is_empty() || emoji_name.len() > 64 {
        return Err(AppError::invalid_url_param("emoji_name"));
    }
    if !app.post_channel_perm(&s, &post_id, perm::REMOVE_REACTION).await? {
        return Err(AppError::forbidden_perm(perm::REMOVE_REACTION));
    }
    if user_id != s.user_id && !app.has_perm(&s, perm::REMOVE_OTHERS_REACTIONS).await {
        return Err(AppError::forbidden_perm(perm::REMOVE_OTHERS_REACTIONS));
    }
    app.delete_reaction(Reaction { user_id, post_id, emoji_name, ..Default::default() }).await?;
    Ok(ok())
}
