//! Slash commands: `POST /commands/execute` (custom team commands first, then the built-ins of
//! [`crate::commands_builtin`]), the command's response posted or shown as Go's `HandleCommandResponse` does, the
//! command lists and autocomplete, and the `response_url` an integration answers later (`/hooks/commands/{id}`).

// Based on Mattermost server/channels/api4/command.go, app/command.go, app/command_autocomplete.go, app/slack.go,
// app/webhook.go (command webhooks), web/webhook.go and model/command_response.go, message_attachment.go,
// integration_action.go (GenerateTriggerId), Copyright (c) 2015-present Mattermost, Inc. Licensed under
// AGPL-3.0. data/builtin_commands.json is the built-in commands as Go 12.1.0 lists them.

use std::sync::OnceLock;

use crate::app::Query;
use axum::body::Bytes;
use axum::extract::{Path, State as St};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::{ApiResult, App, AppError, Auth, Body, State, is_valid_id, new_id, now_ms};
use crate::model::{Post, Session, channel};
use crate::perm;
use crate::posts::CreateFlags;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/commands", get(list_commands))
        .route("/api/v4/commands/execute", post(execute))
        .route("/api/v4/teams/{team_id}/commands/autocomplete", get(list_autocomplete))
        .route("/api/v4/teams/{team_id}/commands/autocomplete_suggestions", get(suggestions))
        .route("/hooks/commands/{id}", post(command_webhook))
}

// ---------- models ----------

/// `model.Command`: a row of `Commands`, or a built-in as Go lists it.
#[ebachat_macros::model(table = "commands")]
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub id: String,
    pub token: String,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    pub creator_id: String,
    pub team_id: String,
    pub trigger: String,
    pub method: String,
    pub username: String,
    pub icon_url: String,
    pub auto_complete: bool,
    pub auto_complete_desc: String,
    pub auto_complete_hint: String,
    pub display_name: String,
    pub description: String,
    pub url: String,
    pub plugin_id: String,
    #[mm(transient, omitempty)]
    pub autocomplete_data: Option<Value>,
    #[mm(transient, omitempty)]
    pub autocomplete_icon_data: String,
}

impl Command {
    /// `Command.Sanitize`.
    fn sanitize(&mut self) {
        self.token.clear();
        self.creator_id.clear();
        self.method.clear();
        self.url.clear();
        self.username.clear();
        self.icon_url.clear();
    }
}

/// The built-ins Go registers on a server without a license, plugins or testing (`commandProviders`).
pub(crate) fn builtins() -> &'static [Command] {
    static B: OnceLock<Vec<Command>> = OnceLock::new();
    B.get_or_init(|| serde_json::from_str(include_str!("../data/builtin_commands.json")).unwrap_or_default())
}

/// `model.CommandArgs` as the client sends it, plus what the server adds.
#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct CommandArgs {
    pub channel_id: String,
    pub team_id: String,
    pub root_id: String,
    pub command: String,
    #[serde(skip)]
    pub user_id: String,
    #[serde(skip)]
    pub site_url: String,
    #[serde(skip)]
    pub trigger_id: String,
}

fn nd<'de, D: serde::Deserializer<'de>, T: Deserialize<'de> + Default>(d: D) -> Result<T, D::Error> {
    crate::model::null_default(d)
}

/// `model.CommandResponse`.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct CommandResponse {
    #[serde(deserialize_with = "nd")]
    pub response_type: String,
    #[serde(deserialize_with = "nd")]
    pub text: String,
    #[serde(deserialize_with = "nd")]
    pub username: String,
    #[serde(deserialize_with = "nd")]
    pub channel_id: String,
    #[serde(deserialize_with = "nd")]
    pub icon_url: String,
    #[serde(rename = "type", deserialize_with = "nd")]
    pub kind: String,
    pub props: Option<Map<String, Value>>,
    #[serde(deserialize_with = "nd")]
    pub goto_location: String,
    #[serde(deserialize_with = "nd")]
    pub trigger_id: String,
    #[serde(deserialize_with = "nd")]
    pub skip_slack_parsing: bool,
    pub attachments: Option<Vec<Value>>,
    pub extra_responses: Option<Vec<CommandResponse>>,
}

pub const EPHEMERAL: &str = "ephemeral";
pub const IN_CHANNEL: &str = "in_channel";

pub fn ephemeral(text: impl Into<String>) -> CommandResponse {
    CommandResponse { response_type: EPHEMERAL.into(), text: text.into(), ..Default::default() }
}

/// `args.T(id)` in English.
pub fn tr(id: &str) -> String {
    crate::i18n::tr(id, &[]).unwrap_or_else(|| id.to_string())
}

/// `args.T(id, params)` in English.
pub fn trp(id: &str, params: &[(&str, &str)]) -> String {
    crate::i18n::tr(id, params).unwrap_or_else(|| id.to_string())
}

fn with_trigger(mut e: AppError, trigger: &str) -> AppError {
    if let Some(m) = crate::i18n::tr(&e.id, &[("Trigger", trigger)]) {
        e.message = m;
    }
    e
}

// ---------- execute ----------

/// `GetProtocol` + host: the site URL a request came through (`GetSiteURLHeader`).
fn site_url_header(h: &HeaderMap) -> String {
    let proto =
        if h.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https") { "https" } else { "http" };
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    format!("{proto}://{host}")
}

/// `executeCommand`.
async fn execute(
    St(app): St<State>,
    Auth(s): Auth,
    headers: HeaderMap,
    Body(mut args): Body<CommandArgs>,
) -> ApiResult<Json<CommandResponse>> {
    if args.command.chars().count() <= 1 || !args.command.starts_with('/') || !is_valid_id(&args.channel_id) {
        return Err(AppError::bad_request("api.command.execute_command.start.app_error", ""));
    }
    if !app.channel_perm(&s, &args.channel_id, perm::CREATE_POST).await?.0 {
        return Err(AppError::forbidden_perm(perm::CREATE_POST));
    }
    let ch = app.channel_or_404(&args.channel_id).await?;
    if ch.delete_at != 0 {
        return Err(AppError::bad_request("api.command.execute_command.deleted.error", ""));
    }
    if ch.r#type != channel::DIRECT && ch.r#type != channel::GROUP {
        args.team_id = ch.team_id.clone();
    } else {
        if app.restricted_dm(&ch).await? {
            return Err(AppError::bad_request("api.command.execute_command.restricted_dm.error", ""));
        }
        if !s.team_members.iter().any(|t| t.team_id == args.team_id) && !app.has_perm(&s, perm::CREATE_POST).await {
            return Err(AppError::forbidden_perm(perm::CREATE_POST));
        }
    }
    args.user_id = s.user_id.clone();
    args.site_url = site_url_header(&headers);
    Ok(Json(execute_command(&app, &s, args).await?))
}

/// `ExecuteCommand`: custom commands of the team, which may override built-ins, then the built-ins.
pub async fn execute_command(app: &State, s: &Session, mut args: CommandArgs) -> ApiResult<CommandResponse> {
    app.execute_command_in(app, s, &mut args).await
}

impl App {
    async fn execute_command_in(&self, app: &State, s: &Session, args: &mut CommandArgs) -> ApiResult<CommandResponse> {
        let (trigger, message) = match args.command.char_indices().find(|(_, c)| c.is_whitespace()) {
            Some((i, c)) => (args.command[..i].to_lowercase(), args.command[i + c.len_utf8()..].to_string()),
            None => (args.command.to_lowercase(), String::new()),
        };
        let Some(trigger) = trigger.strip_prefix('/').map(str::to_string) else {
            return Err(with_trigger(
                AppError::bad_request("api.command.execute_command.format.app_error", ""),
                &trigger,
            ));
        };
        let (client_trigger_id, trigger_id) = self.generate_trigger_id(&args.user_id, &args.channel_id).await;
        args.trigger_id = trigger_id;

        // Plugins can override built-in and custom commands.
        if let Some((cmd, mut response)) = self.try_plugin_command(s, args).await? {
            response.trigger_id = client_trigger_id;
            return self.handle_command_response(&cmd, args, response, true).await;
        }
        if let Some((cmd, mut response)) = self.try_custom_command(args, &trigger, &message).await? {
            response.trigger_id = client_trigger_id;
            return self.handle_command_response(&cmd, args, response, false).await;
        }
        if let Some(response) = crate::commands_builtin::run(app, s, args, &trigger, &message).await {
            return self.handle_command_response(&Command::default(), args, response, true).await;
        }
        let mut shown = trigger;
        if shown.len() > 512 {
            let mut end = 512;
            while !shown.is_char_boundary(end) {
                end -= 1;
            }
            shown.truncate(end);
            shown += "...";
        }
        Err(with_trigger(AppError::not_found("api.command.execute_command.not_found.app_error", ""), &shown))
    }

    /// The command of an active plugin, with the message's mentions of team members and
    /// public channels.
    async fn try_plugin_command(
        &self,
        s: &Session,
        args: &CommandArgs,
    ) -> ApiResult<Option<(Command, CommandResponse)>> {
        let first = args.command.split(' ').next().unwrap_or("");
        let trigger = first.get(1..).unwrap_or("").to_lowercase();
        let plugins = &crate::plugins::PLUGINS;
        let Some(cmd) = plugins.commands(&args.team_id).into_iter().find(|c| c.trigger == trigger) else {
            return Ok(None);
        };
        let Some(plugin) = plugins.get(&cmd.plugin_id) else { return Ok(None) };
        let crashed = |id: &str| {
            AppError::new(StatusCode::INTERNAL_SERVER_ERROR, id, "")
                .with_params(&[("Command", &trigger), ("PluginId", &cmd.plugin_id)])
                .detail(format!("err= Plugin {} crashed due to /{trigger} command", cmd.plugin_id))
        };
        let Some(p) = &plugin.process else { return Ok(None) };
        if p.ping().await.is_err() {
            return Err(crashed("model.plugin_command_error.error.app_error"));
        }
        let users = self.mentions_to_team_members(&args.command, &args.team_id).await;
        let channels = self.mentions_to_public_channels(&args.command, &args.team_id).await;
        let (response, err) = crate::plugins::execute_command(p, s, args, users, channels)
            .await
            .map_err(|_| crashed("model.plugin_command_crash.error.app_error"))?;
        if let Some(e) = err {
            return Err(e);
        }
        let mut cmd = cmd;
        cmd.plugin_id.clear();
        Ok(response.map(|r| (cmd, r)))
    }

    /// `tryExecuteCustomCommand`.
    async fn try_custom_command(
        &self,
        args: &CommandArgs,
        trigger: &str,
        message: &str,
    ) -> ApiResult<Option<(Command, CommandResponse)>> {
        if !self.cfg.bool("ServiceSettings.EnableCommands") {
            return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.command.disabled.app_error", ""));
        }
        let cmds = team_commands(self, &args.team_id).await?;
        let team = self.team(&args.team_id).await.map_err(|e| {
            if e.status_code == 404 { AppError::not_found("app.team.get.find.app_error", "") } else { e }
        })?;
        let user = self.user(&args.user_id).await?.ok_or_else(crate::users::missing_account)?;
        let channel = self.channel_or_404(&args.channel_id).await?;
        let Some(cmd) = cmds.into_iter().rfind(|c| c.trigger == trigger) else { return Ok(None) };

        let mut p: Vec<(String, String)> = vec![
            ("token".into(), cmd.token.clone()),
            ("team_id".into(), cmd.team_id.clone()),
            ("team_domain".into(), team.name.clone()),
            ("channel_id".into(), args.channel_id.clone()),
            ("channel_name".into(), channel.name.clone()),
            ("user_id".into(), args.user_id.clone()),
            ("user_name".into(), user.username.clone()),
            ("command".into(), format!("/{trigger}")),
            ("text".into(), message.to_string()),
            ("trigger_id".into(), args.trigger_id.clone()),
            ("root_id".into(), args.root_id.clone()),
        ];
        for (name, id) in self.mentions_to_team_members(message, &team.id).await {
            p.push(("user_mentions".into(), name));
            p.push(("user_mentions_ids".into(), id));
        }
        for (name, id) in self.mentions_to_public_channels(message, &team.id).await {
            p.push(("channel_mentions".into(), name));
            p.push(("channel_mentions_ids".into(), id));
        }
        let site_url = self.cfg.str("ServiceSettings.SiteURL").to_string();
        if site_url.is_empty() {
            return Err(AppError::bad_request("api.command.execute_command.site_url_required.app_error", ""));
        }
        let hook_id = new_id();
        sqlx::query(
            "INSERT INTO CommandWebhooks (Id, CreateAt, CommandId, UserId, ChannelId, RootId, UseCount)
             VALUES ($1, $2, $3, $4, $5, $6, 0)",
        )
        .bind(&hook_id)
        .bind(now_ms())
        .bind(&cmd.id)
        .bind(&args.user_id)
        .bind(&args.channel_id)
        .bind(&args.root_id)
        .execute(&self.db)
        .await
        .map_err(|e| {
            with_trigger(AppError::internal("api.command.execute_command.failed.app_error", e.to_string()), trigger)
        })?;
        p.push(("response_url".into(), format!("{site_url}/hooks/commands/{hook_id}")));
        let response = self.do_command_request(&cmd, &p).await?;
        Ok(Some((cmd, response)))
    }

    /// `DoCommandRequest`: the integration's answer, JSON or plain text.
    async fn do_command_request(&self, cmd: &Command, p: &[(String, String)]) -> ApiResult<CommandResponse> {
        let failed = |d: String| {
            with_trigger(AppError::internal("api.command.execute_command.failed.app_error", d), &cmd.trigger)
        };
        let encoded = form_encode(p);
        let mut url = reqwest::Url::parse(&cmd.url).map_err(|e| failed(e.to_string()))?;
        if !self.untrusted_url_allowed(&url) {
            return Err(failed("address forbidden".into()));
        }
        let timeout = std::time::Duration::from_secs(
            self.cfg.i64("ServiceSettings.OutgoingIntegrationRequestsTimeout").max(1) as u64,
        );
        let client = self.integration_client();
        let mut req = if cmd.method == "G" {
            let q = match url.query() {
                Some(q) if !q.is_empty() => format!("{q}&{encoded}"),
                _ => encoded,
            };
            url.set_query(Some(&q));
            client.get(url)
        } else {
            client.post(url).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(encoded)
        };
        req = req.header(header::ACCEPT, "application/json").timeout(timeout);
        if !cmd.token.is_empty() {
            req = req.header(header::AUTHORIZATION, format!("Token {}", cmd.token));
        }
        let resp = req.send().await.map_err(|e| failed(e.to_string()))?;
        let status = resp.status();
        let ctype = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = read_limited(resp, 1024 * 1024).await;
        if !status.is_success() {
            let mut e =
                AppError::internal("api.command.execute_command.failed_resp.app_error", String::from_utf8_lossy(&body));
            let st = format!("{} {}", status.as_u16(), status.canonical_reason().unwrap_or(""));
            if let Some(m) = crate::i18n::tr(&e.id, &[("Trigger", &cmd.trigger), ("Status", &st)]) {
                e.message = m;
            }
            return Err(e);
        }
        if status == StatusCode::NO_CONTENT || status == StatusCode::RESET_CONTENT {
            return Ok(CommandResponse::default());
        }
        response_from_body(&ctype, &body).map_err(failed)
    }

    /// `MentionsToTeamMembers`: `@name`s of the message that are members of the team.
    async fn mentions_to_team_members(&self, message: &str, team_id: &str) -> Vec<(String, String)> {
        let mut out = vec![];
        for mention in possible_at_mentions(message) {
            let mut name = mention;
            loop {
                if let Ok(Some(u)) = user_by_username(self, &name).await {
                    if matches!(self.team_member(team_id, &u.id).await, Ok(Some(m)) if m.delete_at == 0) {
                        out.push((name, u.id.clone()));
                    }
                    break;
                }
                // Not a user: maybe with trailing punctuation (`trimUsernameSpecialChar`).
                match name.chars().last() {
                    Some('.' | '-' | '_') => {
                        name.pop();
                    }
                    _ => break,
                }
            }
        }
        out
    }

    /// `MentionsToPublicChannels`: `~name`s of the message that are public channels of the team.
    async fn mentions_to_public_channels(&self, message: &str, team_id: &str) -> Vec<(String, String)> {
        let mut out = vec![];
        for name in channel_mentions(message) {
            if let Ok(Some(c)) = self.channel_by_name(team_id, &name, false, true).await
                && c.r#type == channel::OPEN
            {
                out.push((name, c.id.clone()));
            }
        }
        out
    }

    // ---------- the response ----------

    /// `HandleCommandResponse`: the response and its extra responses posted; a failed one fails the command.
    pub async fn handle_command_response(
        &self,
        cmd: &Command,
        args: &CommandArgs,
        mut response: CommandResponse,
        builtin: bool,
    ) -> ApiResult<CommandResponse> {
        let trigger = args.command.split(' ').next().unwrap_or("").get(1..).unwrap_or("").to_lowercase();
        let mut failed = self.handle_command_response_post(cmd, args, &mut response, builtin).await.is_err();
        if let Some(extra) = response.extra_responses.as_mut() {
            for r in extra.iter_mut() {
                failed |= self.handle_command_response_post(cmd, args, r, builtin).await.is_err();
            }
        }
        if failed {
            return Err(with_trigger(
                AppError::internal("api.command.execute_command.create_post_failed.app_error", ""),
                &trigger,
            ));
        }
        Ok(response)
    }

    /// `HandleCommandResponsePost`.
    async fn handle_command_response_post(
        &self,
        cmd: &Command,
        args: &CommandArgs,
        r: &mut CommandResponse,
        builtin: bool,
    ) -> ApiResult<()> {
        let mut post = Post {
            channel_id: args.channel_id.clone(),
            root_id: args.root_id.clone(),
            user_id: args.user_id.clone(),
            r#type: r.kind.clone(),
            ..Default::default()
        };
        if let Some(props) = &r.props {
            post.props.0 = props.clone();
        }
        if !r.channel_id.is_empty() {
            if self.channel_member(&r.channel_id, &args.user_id).await?.is_none() {
                return Err(AppError::forbidden().with_id("api.command.command_post.forbidden.app_error"));
            }
            post.channel_id = r.channel_id.clone();
        }
        let mut bot_post = !builtin;
        if self.cfg.bool("ServiceSettings.EnablePostUsernameOverride") {
            let name = if !cmd.username.is_empty() { &cmd.username } else { &r.username };
            if !name.is_empty() {
                post.props.0.insert("override_username".into(), name.clone().into());
                bot_post = true;
            }
        }
        if self.cfg.bool("ServiceSettings.EnablePostIconOverride") {
            let icon = if !cmd.icon_url.is_empty() { &cmd.icon_url } else { &r.icon_url };
            if !icon.is_empty() {
                post.props.0.insert("override_icon_url".into(), icon.clone().into());
                bot_post = true;
            }
        }
        if bot_post {
            post.props.0.insert("from_webhook".into(), "true".into());
        }
        if !r.skip_slack_parsing {
            r.text = self.process_slack_text(&r.text).await;
            if let Some(a) = r.attachments.take() {
                r.attachments = Some(self.process_message_attachments(a).await);
            }
        }
        self.create_command_post(post, r).await.map(drop)
    }

    /// `CreateCommandPost`: in the channel, or to the user only.
    async fn create_command_post(&self, mut post: Post, r: &mut CommandResponse) -> ApiResult<Post> {
        post.message = if r.skip_slack_parsing { r.text.clone() } else { parse_slack_links_to_markdown(&r.text) };
        post.create_at = now_ms();
        if post.r#type.starts_with("system_") {
            return Err(AppError::bad_request("api.context.invalid_param.app_error", "")
                .with_id("api.context.invalid_param.app_error"));
        }
        // Go converts the response's own attachments (shared pointers): the answer shows them converted too.
        if let Some(a) = r.attachments.take() {
            r.attachments = Some(parse_message_attachment(&mut post, a));
        }
        if r.response_type == IN_CHANNEL {
            let p = &mut post.props.0;
            let from_webhook = p.remove("from_webhook").is_some_and(|v| v == "true");
            let mut take = |k: &str| match p.remove(k) {
                Some(Value::String(s)) => s,
                _ => String::new(),
            };
            let (mut name, mut icon, mut emoji) =
                (take("override_username"), take("override_icon_url"), take("override_icon_emoji"));
            if !self.cfg.bool("ServiceSettings.EnablePostUsernameOverride") {
                name.clear();
            }
            if !self.cfg.bool("ServiceSettings.EnablePostIconOverride") {
                icon.clear();
                emoji.clear();
            }
            let c = self.channel_or_404(&post.channel_id).await?;
            let flags = CreateFlags {
                trigger_webhooks: true,
                set_online: true,
                from_incoming_webhook: from_webhook,
                override_username: name,
                override_icon_url: icon,
                override_icon_emoji: emoji,
                ..Default::default()
            };
            return self.create_post_in(post, &c, flags, None).await;
        }
        if (r.response_type.is_empty() || r.response_type == EPHEMERAL)
            && (!r.text.is_empty() || r.attachments.is_some())
        {
            let user = post.user_id.clone();
            return Ok(self.send_ephemeral(&user, post).await);
        }
        Ok(post)
    }

    /// `ProcessSlackText`: `<!channel>` → `@channel`, `<@userid>` → `@username`.
    pub async fn process_slack_text(&self, text: &str) -> String {
        let mut text = text.replace("<!channel>", "@channel").replace("<!here>", "@here").replace("<!all>", "@all");
        static RE: OnceLock<regex::Regex> = OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"<@([a-zA-Z0-9]+)>").unwrap());
        let ids: Vec<String> = re.captures_iter(&text).map(|c| c[1].to_string()).collect();
        if !ids.is_empty()
            && let Ok(users) = self.users(&ids).await
        {
            for u in users {
                text = text.replace(&format!("<@{}>", u.id), &format!("@{}", u.username));
            }
        }
        text
    }

    /// `ProcessMessageAttachments`.
    pub async fn process_message_attachments(&self, attachments: Vec<Value>) -> Vec<Value> {
        let mut out = Vec::with_capacity(attachments.len());
        for mut a in stringify_field_values(attachments) {
            for k in ["pretext", "text", "title"] {
                if let Some(Value::String(s)) = a.get(k) {
                    let t = self.process_slack_text(s).await;
                    a[k] = Value::String(t);
                }
            }
            if let Some(Value::Array(fields)) = a.get_mut("fields") {
                for f in fields.iter_mut() {
                    if let Some(Value::String(v)) = f.get("value") {
                        let t = self.process_slack_text(v).await;
                        f["value"] = Value::String(t);
                    }
                }
            }
            out.push(a);
        }
        out
    }

    /// `model.GenerateTriggerId`: (the client's id, the signed trigger id the integration gets).
    async fn generate_trigger_id(&self, user_id: &str, channel_id: &str) -> (String, String) {
        match self.signing_key().await {
            Ok(key) => crate::integrations::generate_trigger_id(user_id, channel_id, &key),
            Err(_) => (crate::app::new_id(), String::new()),
        }
    }
}

/// `url.Values.Encode`: sorted by key, values in order.
fn form_encode(p: &[(String, String)]) -> String {
    let mut p: Vec<&(String, String)> = p.iter().collect();
    p.sort_by(|a, b| a.0.cmp(&b.0));
    p.iter().map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v))).collect::<Vec<_>>().join("&")
}

/// Go's `url.QueryEscape`.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn read_limited(resp: reqwest::Response, max: usize) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out = vec![];
    let mut s = resp.bytes_stream();
    while let Some(Ok(chunk)) = s.next().await {
        let room = max - out.len();
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= max {
            break;
        }
    }
    out
}

/// `CommandResponseFromHTTPBody`.
fn response_from_body(ctype: &str, body: &[u8]) -> Result<CommandResponse, String> {
    if ctype.split(';').next().unwrap_or("").trim() == "application/json" {
        let mut r: CommandResponse = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        r.attachments = r.attachments.map(stringify_field_values);
        if let Some(extra) = r.extra_responses.as_mut() {
            for x in extra.iter_mut() {
                x.attachments = x.attachments.take().map(stringify_field_values);
            }
        }
        return Ok(r);
    }
    Ok(CommandResponse { text: String::from_utf8_lossy(body).into_owned(), ..Default::default() })
}

/// `[]*MessageAttachment` decoded as Go decodes it: every field of the struct, missing ones zero; then
/// `StringifyMessageAttachmentFieldValue` (a field value that is not a string becomes one, `%v`).
fn stringify_field_values(attachments: Vec<Value>) -> Vec<Value> {
    const STRINGS: [&str; 13] = [
        "fallback",
        "color",
        "pretext",
        "author_name",
        "author_link",
        "author_icon",
        "title",
        "title_link",
        "text",
        "image_url",
        "thumb_url",
        "footer",
        "footer_icon",
    ];
    attachments
        .into_iter()
        .filter(|a| !a.is_null())
        .map(|a| {
            let mut out = Map::new();
            out.insert("id".into(), a.get("id").filter(|v| v.is_i64()).cloned().unwrap_or(json!(0)));
            for k in STRINGS {
                out.insert(k.into(), a.get(k).filter(|v| v.is_string()).cloned().unwrap_or(json!("")));
            }
            let fields: Option<Vec<Value>> = a.get("fields").and_then(Value::as_array).map(|fs| {
                fs.iter()
                    .filter(|f| !f.is_null())
                    .map(|f| {
                        let value = match f.get("value") {
                            Some(Value::Number(n)) => Value::String(n.to_string()),
                            Some(Value::Bool(b)) => Value::String(b.to_string()),
                            Some(v) => v.clone(),
                            None => Value::Null,
                        };
                        let short = match f.get("short") {
                            Some(Value::Bool(b)) => *b,
                            Some(Value::String(s)) => s == "true",
                            _ => false,
                        };
                        json!({"title": f.get("title").and_then(Value::as_str).unwrap_or(""), "value": value, "short": short})
                    })
                    .collect()
            });
            out.insert("fields".into(), fields.map(Value::Array).unwrap_or(Value::Null));
            out.insert("ts".into(), a.get("ts").cloned().unwrap_or(Value::Null));
            if let Some(actions) = a.get("actions").filter(|v| v.as_array().is_some_and(|x| !x.is_empty())) {
                out.insert("actions".into(), actions.clone());
            }
            Value::Object(out)
        })
        .collect()
}

/// `ParseSlackLinksToMarkdown`: `<url|text>` → `[text](url)`.
pub fn parse_slack_links_to_markdown(text: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<([^<\|]+)\|([^>]+)>").unwrap()).replace_all(text, "[$2]($1)").into_owned()
}

/// `ParseMessageAttachment`: the attachments in `props.attachments`, their links as Markdown.
pub fn parse_message_attachment(post: &mut Post, attachments: Vec<Value>) -> Vec<Value> {
    if post.r#type.is_empty() {
        post.r#type = "slack_attachment".into();
    }
    let mut out = vec![];
    for mut a in attachments {
        if a.is_null() {
            continue;
        }
        for k in ["text", "pretext"] {
            if let Some(Value::String(s)) = a.get(k) {
                let t = parse_slack_links_to_markdown(s);
                a[k] = Value::String(t);
            }
        }
        if let Some(Value::Array(fields)) = a.get_mut("fields") {
            for f in fields.iter_mut() {
                if let Some(Value::String(v)) = f.get("value") {
                    let t = parse_slack_links_to_markdown(v);
                    f["value"] = Value::String(t);
                }
            }
        }
        out.push(a);
    }
    post.props.0.insert("attachments".into(), Value::Array(out.clone()));
    out
}

/// `possibleAtMentions`: `@name`s, normalized, each once.
fn possible_at_mentions(message: &str) -> Vec<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\B@[[:alnum:]][[:alnum:]\.\-_:]*").unwrap());
    let mut out: Vec<String> = vec![];
    if !message.contains('@') {
        return out;
    }
    for m in re.find_iter(message) {
        let name = m.as_str()[1..].to_lowercase();
        if !out.contains(&name) && crate::user_edit::valid_username(&name) {
            out.push(name);
        }
    }
    out
}

/// `model.ChannelMentions`: `~name`s, each once.
fn channel_mentions(message: &str) -> Vec<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\B~[a-zA-Z0-9\-_]+").unwrap());
    let mut out: Vec<String> = vec![];
    if !message.contains('~') {
        return out;
    }
    for m in re.find_iter(message) {
        let name = m.as_str()[1..].to_string();
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// `UserStore.GetByUsername`.
pub async fn user_by_username(app: &App, name: &str) -> ApiResult<Option<std::sync::Arc<crate::model::User>>> {
    let found = crate::users::load_users(app, "u.username = ANY($1)", &[name.to_lowercase()]).await?;
    Ok(found.into_iter().next().map(std::sync::Arc::new))
}

/// `CommandStore.GetByTeam`.
async fn team_commands(app: &App, team_id: &str) -> ApiResult<Vec<Command>> {
    use crate::db::Table;
    let sql = format!("{} WHERE TeamId = $1 AND DeleteAt = 0", Command::SELECT);
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(team_id).fetch_all(&app.db).await?)
}

// ---------- lists and autocomplete ----------

impl App {
    /// `ListAutocompleteCommands`: built-ins and the team's commands that autocomplete, sanitized.
    async fn autocomplete_commands(&self, team_id: &str) -> ApiResult<Vec<Command>> {
        let mut out: Vec<Command> = vec![];
        let mut seen: Vec<String> = vec![];
        if !self.cfg.bool("TeamSettings.EnableCustomUserStatuses") {
            seen.push("status".into());
        }
        for mut c in crate::plugins::PLUGINS.commands(team_id) {
            if c.auto_complete && !seen.contains(&c.trigger) {
                c.plugin_id.clear();
                seen.push(c.trigger.clone());
                out.push(c);
            }
        }
        if self.cfg.bool("ServiceSettings.EnableCommands") {
            for mut c in team_commands(self, team_id).await? {
                if c.auto_complete && !seen.contains(&c.trigger) {
                    c.sanitize();
                    seen.push(c.trigger.clone());
                    out.push(c);
                }
            }
        }
        for c in builtins() {
            if !seen.contains(&c.trigger) && builtin_autocompletes(self, c) {
                seen.push(c.trigger.clone());
                out.push(builtin_listed(c));
            }
        }
        Ok(out)
    }

    /// `ListAllCommandsByUser`: built-ins, then the team's commands (those of `creator` when given).
    async fn all_commands(&self, team_id: &str, creator: Option<&str>) -> ApiResult<Vec<Command>> {
        let mut out: Vec<Command> = vec![];
        let mut seen: Vec<String> = vec![];
        for c in builtins() {
            if c.trigger == "status" && !self.cfg.bool("TeamSettings.EnableCustomUserStatuses")
                || !builtin_autocompletes(self, c)
            {
                continue;
            }
            seen.push(c.trigger.clone());
            out.push(builtin_listed(c));
        }
        for mut c in crate::plugins::PLUGINS.commands(team_id) {
            if !seen.contains(&c.trigger) {
                c.plugin_id.clear();
                seen.push(c.trigger.clone());
                out.push(c);
            }
        }
        if self.cfg.bool("ServiceSettings.EnableCommands") {
            for mut c in team_commands(self, team_id).await? {
                if seen.contains(&c.trigger) || creator.is_some_and(|u| c.creator_id != u) {
                    continue;
                }
                c.sanitize();
                seen.push(c.trigger.clone());
                out.push(c);
            }
        }
        Ok(out)
    }
}

/// Whether a built-in's `GetCommand` sets `AutoComplete`: all do but `/invite_people`
/// (`InvitePeopleProvider.GetCommand`), which needs email notifications, user creation and email invitations on.
/// Both lists drop the built-ins that don't.
fn builtin_autocompletes(app: &App, c: &Command) -> bool {
    c.trigger != "invite_people"
        || app.cfg.bool("EmailSettings.SendEmailNotifications")
            && app.cfg.bool("TeamSettings.EnableUserCreation")
            && app.cfg.bool("ServiceSettings.EnableEmailInvitations")
}

/// A built-in as listed, its texts naming the site (`crate::brand`).
fn builtin_listed(c: &Command) -> Command {
    let mut c = Command { auto_complete: true, ..c.clone() };
    for s in [&mut c.auto_complete_desc, &mut c.auto_complete_hint, &mut c.description, &mut c.display_name] {
        if let std::borrow::Cow::Owned(b) = crate::brand::apply(s) {
            *s = b;
        }
    }
    if let Some(d) = c.autocomplete_data.as_mut() {
        brand_json(d);
    }
    c
}

/// Every string in `v` naming the site.
fn brand_json(v: &mut Value) {
    match v {
        Value::String(s) => {
            if let std::borrow::Cow::Owned(b) = crate::brand::apply(s) {
                *s = b;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(brand_json),
        Value::Object(m) => m.values_mut().for_each(brand_json),
        _ => {}
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListQuery {
    team_id: String,
    custom_only: String,
}

/// `listCommands`.
async fn list_commands(St(app): St<State>, Auth(s): Auth, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Command>>> {
    if q.team_id.is_empty() {
        return Err(AppError::invalid_param("team_id"));
    }
    app.require_team(&s, &q.team_id, perm::VIEW_TEAM).await?;
    let manage_own = app.has_team_perm(&s, &q.team_id, "manage_own_slash_commands").await;
    let manage_others = app.has_team_perm(&s, &q.team_id, "manage_others_slash_commands").await;
    let creator = if manage_others { None } else { Some(s.user_id.as_str()) };
    if q.custom_only == "true" || q.custom_only == "1" {
        if !manage_own {
            return Err(AppError::forbidden_perm("manage_own_slash_commands"));
        }
        if !app.cfg.bool("ServiceSettings.EnableCommands") {
            return Err(AppError::new(StatusCode::NOT_IMPLEMENTED, "api.command.disabled.app_error", ""));
        }
        let cmds = team_commands(&app, &q.team_id).await?;
        return Ok(Json(cmds.into_iter().filter(|c| creator.is_none_or(|u| c.creator_id == u)).collect()));
    }
    if !manage_own {
        return Ok(Json(app.autocomplete_commands(&q.team_id).await?));
    }
    Ok(Json(app.all_commands(&q.team_id, creator).await?))
}

/// `listAutocompleteCommands`.
async fn list_autocomplete(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
) -> ApiResult<Json<Vec<Command>>> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    Ok(Json(app.autocomplete_commands(&team_id).await?))
}

/// `model.AutocompleteSuggestion`.
#[derive(Serialize, Clone)]
#[serde(rename_all = "PascalCase")]
struct Suggestion {
    complete: String,
    suggestion: String,
    hint: String,
    description: String,
    icon_data: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SuggestionQuery {
    user_input: String,
    channel_id: String,
    root_id: String,
}

/// `listCommandAutocompleteSuggestions` / `GetSuggestions`.
async fn suggestions(
    St(app): St<State>,
    Auth(s): Auth,
    Path(team_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<SuggestionQuery>,
) -> ApiResult<Json<Vec<Suggestion>>> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    app.require_team(&s, &team_id, perm::VIEW_TEAM).await?;
    if q.user_input.is_empty() {
        return Err(AppError::invalid_param("userInput"));
    }
    let role = if app.has_perm(&s, perm::MANAGE_SYSTEM).await { "system_admin" } else { "system_user" };
    let input = q.user_input.strip_prefix('/').unwrap_or(&q.user_input);
    let mut cmds = app.autocomplete_commands(&team_id).await?;
    cmds.sort_by_key(|c| c.trigger.to_lowercase());
    let data: Vec<Value> = cmds
        .iter()
        .map(|c| {
            c.autocomplete_data.clone().unwrap_or_else(|| {
                json!({"Trigger": c.trigger, "Hint": c.auto_complete_hint, "HelpText": c.auto_complete_desc,
                       "RoleID": "system_user", "Arguments": [], "SubCommands": []})
            })
        })
        .collect();
    // Dynamic lists come from their plugin: the input is parsed again with each round's lists until none is
    // missing (a list asked for depends on what earlier ones gave).
    let mut lists = Lists::default();
    let mut out = suggest(&data, "", input, role, &lists);
    for _ in 0..4 {
        let need: Vec<ListKey> = lists.need.take().into_iter().filter(|k| !lists.got.contains_key(k)).collect();
        if need.is_empty() {
            break;
        }
        for k in need {
            let items = fetch_list(&app, &s, &k, &team_id, &q, &site_url_header(&headers)).await;
            lists.got.insert(k, items);
        }
        out = suggest(&data, "", input, role, &lists);
    }
    for sug in out.iter_mut() {
        if let Some(c) = cmds.iter().find(|c| sug.complete.starts_with(&c.trigger)) {
            sug.icon_data = c.autocomplete_icon_data.clone();
        }
    }
    Ok(Json(out))
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

fn arr(v: &Value, k: &str) -> Vec<Value> {
    v[k].as_array().cloned().unwrap_or_default()
}

fn sug(complete: String, suggestion: String, hint: String, description: String) -> Suggestion {
    Suggestion { complete, suggestion, hint, description, icon_data: String::new() }
}

/// A dynamic list's fetch URL with the input parsed and still to parse when it was reached.
type ListKey = (String, String, String);

/// The dynamic lists fetched for a parse (none when the fetch failed), and those it reached without them.
#[derive(Default)]
struct Lists {
    got: std::collections::HashMap<ListKey, Option<Vec<Value>>>,
    need: std::cell::RefCell<Vec<ListKey>>,
}

/// The plugin's list items for the input so far.
async fn fetch_list(
    app: &App,
    s: &Session,
    key: &ListKey,
    team_id: &str,
    q: &SuggestionQuery,
    site_url: &str,
) -> Option<Vec<Value>> {
    let (url, parsed, to_parse) = key;
    let mut v = crate::gourl::Values::default();
    for (name, val) in [
        ("user_input", format!("{parsed}{to_parse}").as_str()),
        ("parsed", parsed),
        ("channel_id", &q.channel_id),
        ("team_id", team_id),
        ("root_id", &q.root_id),
        ("parent_id", ""),
        ("user_id", &s.user_id),
        ("site_url", site_url),
    ] {
        v.add(name, val);
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    let full = format!("{url}{sep}{}", v.encode());
    let items = match crate::plugins::http::plugin_request(app, s, "GET", &full, vec![]).await {
        // `null` is an empty list.
        Ok(r) => serde_json::from_slice::<Option<Vec<Value>>>(&r.body).ok().map(Option::unwrap_or_default),
        Err(_) => None,
    };
    if items.is_none() {
        crate::logs::error("Can not fetch dynamic list arguments", &[("url", url.clone().into())]);
    }
    items
}

/// `getSuggestions` over `AutocompleteData` as Go serializes it.
fn suggest(commands: &[Value], parsed: &str, to_parse: &str, role: &str, lists: &Lists) -> Vec<Suggestion> {
    let role_ok = |c: &Value| {
        let r = s(c, "RoleID");
        r == role || role == "system_admin" || role.is_empty()
    };
    let Some(index) = to_parse.find(' ') else {
        return commands
            .iter()
            .filter(|c| s(c, "Trigger").starts_with(&to_parse.to_lowercase()) && role_ok(c))
            .map(|c| sug(format!("{parsed}{}", s(c, "Trigger")), s(c, "Trigger"), s(c, "Hint"), s(c, "HelpText")))
            .collect();
    };
    let mut out = vec![];
    for c in commands {
        if s(c, "Trigger") != to_parse[..index].to_lowercase() || !role_ok(c) {
            continue;
        }
        let rest = &to_parse[index + 1..];
        let parsed = format!("{parsed}{}", &to_parse[..=index]);
        let args = arr(c, "Arguments");
        if args.is_empty() {
            out.extend(suggest(&arr(c, "SubCommands"), &parsed, rest, role, lists));
            continue;
        }
        let (found, _, _, s) = parse_arguments(&args, &parsed, rest, lists);
        if found {
            out.extend(s);
        }
    }
    out
}

type Parsed = (bool, String, String, Vec<Suggestion>);

/// `parseArguments`.
fn parse_arguments(args: &[Value], parsed: &str, to_parse: &str, lists: &Lists) -> Parsed {
    let Some(first) = args.first() else { return (false, parsed.into(), to_parse.into(), vec![]) };
    let mut out = vec![];
    if first["Required"].as_bool().unwrap_or(false) {
        let (found, p, t, s) = parse_argument(first, parsed, to_parse, lists);
        if found {
            return (true, p, t, s);
        }
        return parse_arguments(&args[1..], &p, &t, lists);
    }
    let (mut found_with, mut p_with, mut t_with, s_with) = parse_argument(first, parsed, to_parse, lists);
    if found_with {
        out.extend(s_with);
    } else {
        let (f, p, t, s) = parse_arguments(&args[1..], &p_with, &t_with, lists);
        if f {
            out.extend(s);
        }
        (found_with, p_with, t_with) = (f, p, t);
    }
    let (found_without, p_without, t_without, s_without) = parse_arguments(&args[1..], parsed, to_parse, lists);
    if found_without {
        out.extend(s_without);
    }
    if found_with || found_without {
        return (true, format!("{parsed}{to_parse}"), String::new(), out);
    }
    if p_with != parsed && t_with != to_parse {
        return (false, p_with, t_with, out);
    }
    (found_without, p_without, t_without, out)
}

/// `parseArgument`: named, text, static and dynamic list arguments (a dynamic list not fetched yet is noted in
/// `lists.need` and found nothing).
fn parse_argument(arg: &Value, parsed: &str, to_parse: &str, lists: &Lists) -> Parsed {
    let (mut parsed, mut to_parse) = (parsed.to_string(), to_parse.to_string());
    let name = s(arg, "Name");
    if !name.is_empty() {
        let input = to_parse.strip_prefix(' ').unwrap_or(&to_parse).to_string();
        let named = format!("--{name}");
        let help = s(arg, "HelpText");
        if input.is_empty() {
            let c = format!("{parsed}{to_parse}{named} ");
            return (true, format!("{parsed}{to_parse}"), String::new(), vec![sug(c, named, String::new(), help)]);
        }
        if named.to_lowercase().starts_with(&input.to_lowercase()) {
            let c = format!("{parsed}{to_parse}{} ", &named[input.len()..]);
            return (true, format!("{parsed}{to_parse}"), String::new(), vec![sug(c, named, String::new(), help)]);
        }
        let lower = input.to_lowercase();
        if !lower.starts_with(&format!("{} ", named.to_lowercase())) {
            return (false, format!("{parsed}{to_parse}"), String::new(), vec![]);
        }
        if lower == format!("{} ", named.to_lowercase()) {
            parsed = format!("{parsed}{named} ");
            to_parse = String::new();
        } else {
            to_parse = input[named.len() + 1..].to_string();
            parsed = format!("{parsed}{named} ");
        }
    }
    match s(arg, "Type").as_str() {
        "TextInput" => {
            let input = to_parse.strip_prefix(' ').unwrap_or(&to_parse).to_string();
            let hint = arg["Data"]["Hint"].as_str().unwrap_or("").to_string();
            let help = s(arg, "HelpText");
            let unfinished = || {
                (
                    true,
                    format!("{parsed}{to_parse}"),
                    String::new(),
                    vec![sug(format!("{parsed}{to_parse}"), String::new(), hint.clone(), help.clone())],
                )
            };
            if input.is_empty() {
                return unfinished();
            }
            if let Some(rest) = input.strip_prefix('"') {
                let Some(second) = rest.find('"') else { return unfinished() };
                let mut offset = 2;
                if input.as_bytes().get(second + 2) == Some(&b' ') {
                    offset += 1;
                }
                return (
                    false,
                    format!("{parsed}{}", &input[..second + offset]),
                    input[second + offset..].to_string(),
                    vec![],
                );
            }
            match input.find(' ') {
                None => unfinished(),
                Some(i) => (false, format!("{parsed}{}", &input[..=i]), input[i + 1..].to_string(), vec![]),
            }
        }
        "StaticList" => parse_list_items(&arr(&arg["Data"], "PossibleArguments"), &parsed, &to_parse),
        "DynamicList" => {
            let key = (s(&arg["Data"], "FetchURL"), parsed.clone(), to_parse.clone());
            match lists.got.get(&key) {
                Some(Some(items)) => parse_list_items(items, &parsed, &to_parse),
                Some(None) => (false, parsed, to_parse, vec![]),
                None => {
                    lists.need.borrow_mut().push(key);
                    (false, parsed, to_parse, vec![])
                }
            }
        }
        _ => (false, parsed, to_parse, vec![]),
    }
}

/// `parseListItems`.
fn parse_list_items(items: &[Value], parsed: &str, to_parse: &str) -> Parsed {
    let input = to_parse.strip_prefix(' ').unwrap_or(to_parse);
    let mut max_prefix = String::new();
    for i in items {
        let item = s(i, "Item");
        if input.to_lowercase().starts_with(&format!("{} ", item.to_lowercase())) && max_prefix.len() < item.len() + 1 {
            max_prefix = format!("{item} ");
        }
    }
    if !max_prefix.is_empty() {
        return (
            false,
            format!("{parsed}{}", &input[..max_prefix.len()]),
            input[max_prefix.len()..].to_string(),
            vec![],
        );
    }
    let out = items
        .iter()
        .filter(|i| s(i, "Item").to_lowercase().starts_with(&input.to_lowercase()))
        .map(|i| sug(format!("{parsed}{}", s(i, "Item")), s(i, "Item"), s(i, "Hint"), s(i, "HelpText")))
        .collect();
    (true, format!("{parsed}{to_parse}"), String::new(), out)
}

// ---------- /hooks/commands/{id} ----------

/// `commandWebhook` / `HandleCommandWebhook`: an integration answering a command later, at most 5 times in 30
/// minutes.
async fn command_webhook(St(app): St<State>, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let hook_msg = |mut e: AppError| {
        e.message = crate::i18n::tr(&e.id, &[("hook_id", &id)]).unwrap_or_default();
        e
    };
    let general = |e: AppError| {
        let mut g = AppError::new(
            StatusCode::from_u16(e.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            "web.command_webhook.general.app_error",
            "",
        );
        g.detailed_error = e.message;
        hook_msg(g).into_response()
    };
    let ctype = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let response = match response_from_body(ctype, &body) {
        Ok(r) => r,
        Err(e) => {
            return hook_msg(AppError::bad_request("web.command_webhook.parse.app_error", "").detail(e))
                .into_response();
        }
    };
    let hook: Option<(String, String, String, String)> = match sqlx::query_as(
        "SELECT CommandId, UserId, ChannelId, COALESCE(RootId, '') FROM CommandWebhooks WHERE Id = $1 AND CreateAt > $2",
    )
    .bind(&id)
    .bind(now_ms() - 30 * 60_000)
    .fetch_optional(&app.db)
    .await
    {
        Ok(h) => h,
        Err(e) => return general(e.into()),
    };
    let Some((command_id, user_id, channel_id, root_id)) = hook else {
        return general(AppError::not_found("app.command_webhook.get.missing", ""));
    };
    let cmd: Option<Command> = match crate::db::get(&app.db, &command_id).await {
        Ok(c) => c,
        Err(e) => return general(e.into()),
    };
    let Some(cmd) = cmd else {
        return general(AppError::bad_request("web.command_webhook.command.app_error", ""));
    };
    let used = sqlx::query("UPDATE CommandWebhooks SET UseCount = UseCount + 1 WHERE Id = $1 AND UseCount < 5")
        .bind(&id)
        .execute(&app.db)
        .await;
    match used {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => return general(AppError::bad_request("app.command_webhook.try_use.invalid", "")),
        Err(e) => return general(e.into()),
    }
    let args = CommandArgs { user_id, channel_id, team_id: cmd.team_id.clone(), root_id, ..Default::default() };
    if let Err(e) = app.handle_command_response(&cmd, &args, response, false).await {
        return general(e);
    }
    ([(header::CONTENT_TYPE, "text/plain")], "ok").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_links() {
        assert_eq!(parse_slack_links_to_markdown("see <http://x.y|here> now"), "see [here](http://x.y) now");
    }

    #[test]
    fn suggestions_for_text_argument() {
        let data = vec![
            json!({"Trigger": "rename", "Hint": "[text]", "HelpText": "Rename the channel", "RoleID": "system_user",
            "Arguments": [{"Name": "", "HelpText": "[text]", "Type": "TextInput", "Required": true, "Data": {"Hint": "[text]", "Pattern": ""}}],
            "SubCommands": []}),
        ];
        let s = suggest(&data, "", "rename ", "system_user", &Lists::default());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].complete, "rename ");
        assert_eq!(s[0].hint, "[text]");
        assert!(suggest(&data, "", "ren", "system_user", &Lists::default())[0].complete == "rename");
    }

    /// A dynamic list is asked for, then parsed like a static one once fetched.
    #[test]
    fn suggestions_from_a_dynamic_list() {
        let data = vec![json!({"Trigger": "jira", "RoleID": "system_user", "Arguments": [], "SubCommands": [
            {"Trigger": "assign", "RoleID": "system_user", "SubCommands": [], "Arguments": [
                {"Name": "", "Type": "DynamicList", "Required": true, "Data": {"FetchURL": "/plugins/jira/issues"}}]}]})];
        let mut lists = Lists::default();
        assert!(suggest(&data, "", "jira assign ", "system_user", &lists).is_empty());
        let need = lists.need.take();
        assert_eq!(need, vec![("/plugins/jira/issues".to_string(), "jira assign ".to_string(), String::new())]);
        let items = vec![json!({"Item": "KEY-1", "Hint": "", "HelpText": "first"}), json!({"Item": "OTHER-2"})];
        lists.got.insert(need[0].clone(), Some(items));
        let s = suggest(&data, "", "jira assign ", "system_user", &lists);
        let completes: Vec<&str> = s.iter().map(|x| x.complete.as_str()).collect();
        assert_eq!(completes, ["jira assign KEY-1", "jira assign OTHER-2"]);
    }

    #[test]
    fn form_encoding_is_sorted() {
        let p = vec![("text".to_string(), "a b&c".to_string()), ("command".to_string(), "/x".to_string())];
        assert_eq!(form_encode(&p), "command=%2Fx&text=a+b%26c");
    }
}
