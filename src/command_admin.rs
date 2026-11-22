//! Custom slash commands: creating, reading, updating, moving to another team, deleting, and a new token.

// Based on Mattermost server/channels/api4/command.go, app/command.go, public/model/command.go and
// store/sqlstore/command_store.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, State, is_valid_id, new_id, now_ms};
use crate::commands::Command;
use crate::db::{self, Table};
use crate::team_admin::decode;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/commands", post(create_command))
        .route("/api/v4/commands/{command_id}", put(update_command).get(get_command).delete(delete_command))
        .route("/api/v4/commands/{command_id}/move", put(move_command))
        .route("/api/v4/commands/{command_id}/regen_token", put(regen_token))
}

const MANAGE_OWN: &str = "manage_own_slash_commands";
const MANAGE_OTHERS: &str = "manage_others_slash_commands";

fn bad(id: &str) -> AppError {
    AppError::bad_request(id, "")
}

fn disabled() -> AppError {
    AppError::new(StatusCode::NOT_IMPLEMENTED, "api.command.disabled.app_error", "")
}

/// `SetCommandNotFoundError`: also what someone who may not see the command gets.
fn not_found() -> AppError {
    AppError::not_found("store.sql_command.save.get.app_error", "")
}

/// `IsValidHTTPURL`.
pub fn valid_http_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://"))
        && reqwest::Url::parse(u).is_ok_and(|p| p.host_str().is_some_and(|h| !h.is_empty()))
}

/// `IsValidPluginId`.
fn valid_plugin_id(id: &str) -> bool {
    let n = id.chars().count();
    (3..=190).contains(&n) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// `Command.IsValid`.
fn command_is_valid(c: &Command) -> ApiResult<()> {
    let e = |f: &str| bad(&format!("model.command.is_valid.{f}.app_error"));
    if !is_valid_id(&c.id) {
        return Err(e("id"));
    }
    if c.token.len() != 26 {
        return Err(e("token"));
    }
    if c.create_at == 0 {
        return Err(e("create_at"));
    }
    if c.update_at == 0 {
        return Err(e("update_at"));
    }
    if c.creator_id.is_empty() && !valid_plugin_id(&c.plugin_id) {
        return Err(e("plugin_id"));
    }
    if c.plugin_id.is_empty() && !is_valid_id(&c.creator_id) {
        return Err(e("user_id"));
    }
    if !c.creator_id.is_empty() && !c.plugin_id.is_empty() {
        return Err(e("plugin_id").detail("command cannot have both a CreatorId and a PluginId"));
    }
    if !is_valid_id(&c.team_id) {
        return Err(e("team_id"));
    }
    let t = &c.trigger;
    if t.is_empty()
        || t.len() > 128
        || t.starts_with('/')
        || !t.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b))
    {
        return Err(e("trigger"));
    }
    if c.url.is_empty() || c.url.len() > 1024 {
        return Err(e("url"));
    }
    if !valid_http_url(&c.url) {
        return Err(e("url_http"));
    }
    if c.method != "G" && c.method != "P" {
        return Err(e("method"));
    }
    if c.display_name.len() > 64 {
        return Err(e("display_name"));
    }
    if c.description.len() > 128 {
        return Err(e("description"));
    }
    Ok(())
}

impl App {
    fn commands_on(&self) -> ApiResult<()> {
        if self.cfg.bool("ServiceSettings.EnableCommands") { Ok(()) } else { Err(disabled()) }
    }

    /// `GetCommand`: a command that is not deleted.
    async fn command(&self, id: &str) -> ApiResult<Command> {
        self.commands_on()?;
        let sql = format!("{} WHERE Id = $1 AND DeleteAt = 0", Command::SELECT);
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .map_err(|e| AppError::internal("app.command.getcommand.internal_error", e.to_string()))?
            .ok_or_else(|| {
                AppError::not_found("store.sql_command.get.missing.app_error", "").with_param("command_id", id)
            })
    }

    /// `validateCommandTriggerUniqueness`: neither a built-in's trigger nor another command's of the team.
    async fn check_trigger_unique(&self, team_id: &str, trigger: &str, exclude: &str) -> ApiResult<()> {
        let trigger = trigger.to_lowercase();
        let dup = || bad("api.command.duplicate_trigger.app_error");
        if crate::commands::builtins().iter().any(|b| b.trigger.to_lowercase() == trigger) {
            return Err(dup());
        }
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT Id, Trigger FROM Commands WHERE TeamId = $1 AND DeleteAt = 0")
                .bind(team_id)
                .fetch_all(&self.db)
                .await
                .map_err(|e| {
                    AppError::internal("app.command.validatecommandtriggeruniqueness.internal_error", e.to_string())
                })?;
        if rows.iter().any(|(id, t)| id != exclude && t.to_lowercase() == trigger) {
            return Err(dup());
        }
        Ok(())
    }

    /// `SqlCommandStore.Update`: every column but the deletion time.
    async fn update_command_row(&self, c: &mut Command, internal: &str) -> ApiResult<()> {
        c.update_at = now_ms();
        command_is_valid(c)?;
        sqlx::query(
            "UPDATE Commands SET Token = $2, CreateAt = $3, UpdateAt = $4, CreatorId = $5, TeamId = $6, Method = $7,
                 Username = $8, IconURL = $9, AutoComplete = $10, AutoCompleteDesc = $11, AutoCompleteHint = $12,
                 DisplayName = $13, Description = $14, URL = $15, PluginId = $16, Trigger = $17
             WHERE Id = $1",
        )
        .bind(&c.id)
        .bind(&c.token)
        .bind(c.create_at)
        .bind(c.update_at)
        .bind(&c.creator_id)
        .bind(&c.team_id)
        .bind(&c.method)
        .bind(&c.username)
        .bind(&c.icon_url)
        .bind(c.auto_complete)
        .bind(&c.auto_complete_desc)
        .bind(&c.auto_complete_hint)
        .bind(&c.display_name)
        .bind(&c.description)
        .bind(&c.url)
        .bind(&c.plugin_id)
        .bind(&c.trigger)
        .execute(&self.db)
        .await
        .map_err(|e| AppError::internal(internal, e.to_string()))?;
        Ok(())
    }

    /// The checks every command route but create shares: the command, `manage_own` in its team (a 404 without
    /// it), and `manage_others` for someone else's. The routes that answer a permission error (all but the read)
    /// write Go's audit row for a refusal.
    async fn own_command(&self, s: &crate::model::Session, id: &str, others_404: bool) -> ApiResult<Command> {
        let c = self.command(id).await.map_err(|_| not_found())?;
        let audit = !others_404;
        if !self.has_team_perm(s, &c.team_id, MANAGE_OWN).await {
            if audit {
                crate::audit::log(self, Some(s), "fail - inappropriate permissions").await;
            }
            return Err(not_found());
        }
        if s.user_id != c.creator_id && !self.has_team_perm(s, &c.team_id, MANAGE_OTHERS).await {
            if audit {
                crate::audit::log(self, Some(s), "fail - inappropriate permissions").await;
            }
            return Err(if others_404 { not_found() } else { AppError::forbidden_perm(MANAGE_OTHERS) });
        }
        Ok(c)
    }
}

fn require_command_id(id: &str) -> ApiResult<()> {
    if is_valid_id(id) { Ok(()) } else { Err(AppError::invalid_url_param("command_id")) }
}

/// `createCommand`: owned by the session's user, or by another with `manage_others_slash_commands`.
async fn create_command(St(app): St<State>, Auth(s): Auth, body: Bytes) -> ApiResult<Response> {
    let mut c: Command = decode(&body, "command")?;
    crate::audit::log(&app, Some(&s), "attempt").await;
    if !app.has_team_perm(&s, &c.team_id, MANAGE_OWN).await {
        return Err(AppError::forbidden_perm(MANAGE_OWN));
    }
    let mut creator = s.user_id.clone();
    if !c.creator_id.is_empty() && c.creator_id != s.user_id {
        if !app.has_team_perm(&s, &c.team_id, MANAGE_OTHERS).await {
            crate::audit::log(&app, Some(&s), "fail - inappropriate permissions").await;
            return Err(AppError::forbidden_perm(MANAGE_OTHERS));
        }
        app.user(&c.creator_id).await?.ok_or_else(crate::users::missing_account)?;
        creator = c.creator_id.clone();
    }
    c.creator_id = creator;
    // CreateCommand
    app.commands_on()?;
    c.trigger = c.trigger.to_lowercase();
    app.check_trigger_unique(&c.team_id, &c.trigger, "").await?;
    if !c.id.is_empty() {
        return Err(AppError::internal(
            "app.command.createcommand.internal_error",
            format!("invalid CommandId {}", c.id),
        ));
    }
    c.id = new_id();
    if c.token.is_empty() {
        c.token = new_id();
    }
    c.create_at = now_ms();
    c.update_at = c.create_at;
    command_is_valid(&c)?;
    db::insert(&app.db, &c)
        .await
        .map_err(|e| AppError::internal("app.command.createcommand.internal_error", e.to_string()))?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok((StatusCode::CREATED, Json(c)).into_response())
}

async fn get_command(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Command>> {
    require_command_id(&id)?;
    let c = app.command(&id).await.map_err(|_| not_found())?;
    if !app.has_team_perm(&s, &c.team_id, crate::perm::VIEW_TEAM).await {
        return Err(not_found());
    }
    Ok(Json(app.own_command(&s, &id, true).await?))
}

/// `updateCommand`: the body's fields over the command; its id, token, team, creator and times are kept.
async fn update_command(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Command>> {
    require_command_id(&id)?;
    let mut c: Command = decode(&body, "command")?;
    if c.id != id {
        return Err(AppError::invalid_param("command"));
    }
    crate::audit::log(&app, Some(&s), "attempt").await;
    let old = app.command(&id).await.map_err(|_| not_found())?;
    if c.team_id != old.team_id {
        return Err(bad("api.command.team_mismatch.app_error").detail(format!("user_id={}", s.user_id)));
    }
    let old = app.own_command(&s, &id, false).await?;
    app.commands_on()?;
    c.trigger = c.trigger.to_lowercase();
    c.token = old.token;
    c.create_at = old.create_at;
    c.delete_at = old.delete_at;
    c.creator_id = old.creator_id;
    c.plugin_id = old.plugin_id;
    c.team_id = old.team_id;
    app.check_trigger_unique(&c.team_id, &c.trigger, &c.id).await?;
    app.update_command_row(&mut c, "app.command.updatecommand.internal_error").await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(Json(c))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MoveRequest {
    team_id: String,
}

/// `moveCommand`: to a team where both the session and the command's creator may manage commands.
async fn move_command(
    St(app): St<State>,
    Auth(s): Auth,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_command_id(&id)?;
    let m: MoveRequest = decode(&body, "team_id")?;
    crate::audit::log(&app, Some(&s), "attempt").await;
    let team = app.team(&m.team_id).await?;
    if !app.has_team_perm(&s, &team.id, MANAGE_OWN).await {
        crate::audit::log(&app, Some(&s), "fail - inappropriate permissions").await;
        return Err(AppError::forbidden_perm(MANAGE_OWN));
    }
    let mut c = app.own_command(&s, &id, false).await?;
    if !app.user_team_perm(&c.creator_id, &team.id, MANAGE_OWN).await {
        crate::audit::log(&app, Some(&s), "fail - command creator does not have permission to new team").await;
        return Err(bad("api.command.move_command.creator_no_permission.app_error")
            .detail(format!("creator_id={} team_id={}", c.creator_id, team.id)));
    }
    app.check_trigger_unique(&team.id, &c.trigger, &c.id).await?;
    c.team_id = team.id.clone();
    app.update_command_row(&mut c, "app.command.movecommand.internal_error").await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(Json(json!({"status": "OK"})))
}

async fn delete_command(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    require_command_id(&id)?;
    crate::audit::log(&app, Some(&s), "attempt").await;
    let c = app.own_command(&s, &id, false).await?;
    app.commands_on()?;
    let now = now_ms();
    sqlx::query("UPDATE Commands SET DeleteAt = $1, UpdateAt = $1 WHERE Id = $2")
        .bind(now)
        .bind(&c.id)
        .execute(&app.db)
        .await
        .map_err(|e| AppError::internal("app.command.deletecommand.internal_error", e.to_string()))?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(Json(json!({"status": "OK"})))
}

async fn regen_token(St(app): St<State>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    require_command_id(&id)?;
    crate::audit::log(&app, Some(&s), "attempt").await;
    let mut c = app.own_command(&s, &id, false).await?;
    app.commands_on()?;
    c.token = new_id();
    app.update_command_row(&mut c, "app.command.regencommandtoken.internal_error").await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(Json(json!({"token": c.token})))
}
