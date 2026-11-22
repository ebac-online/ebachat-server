//! Bulk export (`export_process` jobs): the instance (or some teams and channels of it) as Mattermost's JSONL
//! import format, zipped with the attachments and profile pictures asked for, written to
//! `ExportSettings.Directory/<job id>_export.zip`. Also the routes over that directory and the import one
//! (`/exports`, `/imports`).

// Based on Mattermost server/channels/app/export.go, export_converters.go, imports/import_types.go,
// jobs/export_process/worker.go, api4/export.go, api4/import.go, app/import.go (ListImports), and the export queries
// of store/sqlstore (team, channel, user, bot, post, thread, reaction, scheme, role, emoji stores), Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;

use axum::extract::{Path, State as St};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, State};
use crate::jobs::{self, Job};

type Res<T> = Result<T, String>;

// ---------- the import line types, in Go's field order ----------

#[derive(Serialize, Default)]
struct Line {
    #[serde(rename = "type")]
    t: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<RoleData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scheme: Option<SchemeData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<Obj>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<Obj>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<UserData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bot: Option<Obj>,
    #[serde(skip_serializing_if = "Option::is_none")]
    post: Option<PostData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_channel: Option<Obj>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_post: Option<PostData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    emoji: Option<Obj>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<Obj>,
}

/// An ordered JSON object (Go structs keep their field order; `serde_json::Map` would sort): each value kept as
/// the JSON Go would write for it.
#[derive(Default)]
struct Obj(Vec<(&'static str, Raw)>);

type Raw = Box<serde_json::value::RawValue>;

/// A value as Go's JSON, kept verbatim.
fn raw<T: Serialize>(v: &T) -> Raw {
    serde_json::value::RawValue::from_string(String::from_utf8(go_json(v)).unwrap_or_default())
        .unwrap_or_else(|_| raw_null())
}

fn raw_null() -> Raw {
    serde_json::value::RawValue::from_string("null".into()).expect("null is JSON")
}

impl Obj {
    fn new() -> Obj {
        Obj(vec![])
    }
    fn f(mut self, k: &'static str, v: impl Serialize) -> Obj {
        self.0.push((k, raw(&v)));
        self
    }
    /// `omitempty` on a pointer: left out when none.
    fn o<T: Serialize>(self, k: &'static str, v: Option<T>) -> Obj {
        match v {
            Some(v) => self.f(k, v),
            None => self,
        }
    }
}

impl Serialize for Obj {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

#[derive(Serialize, Clone)]
struct RoleData {
    name: String,
    display_name: String,
    description: String,
    permissions: Vec<String>,
    scheme_managed: bool,
}

#[derive(Serialize)]
struct SchemeData {
    name: String,
    display_name: String,
    description: String,
    scope: String,
    default_team_admin_role: Option<RoleData>,
    default_team_user_role: Option<RoleData>,
    default_channel_admin_role: Option<RoleData>,
    default_channel_user_role: Option<RoleData>,
    default_team_guest_role: Option<RoleData>,
    default_channel_guest_role: Option<RoleData>,
}

/// `UserImportData`.
#[derive(Serialize, Default)]
struct UserData {
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_image: Option<String>,
    username: String,
    email: String,
    auth_service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_data: Option<String>,
    nickname: String,
    first_name: String,
    last_name: String,
    position: String,
    roles: String,
    locale: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    feature_enabled_markdown_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    formatting: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    show_unread_section: Option<String>,
    delete_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    send_on_ctrl_enter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_block_ctrl_enter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    show_join_leave: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    show_unread_scroll_position: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_drafts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_visible_dms_gms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name_format: Option<String>,
    /// A pointer to the memberships: `null` when there are none.
    #[serde(skip_serializing_if = "Option::is_none")]
    teams: Option<Option<Vec<Obj>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    military_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_previews: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    collapse_consecutive_messages: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    colorize_usernames: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_display_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tutorial_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email_interval: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notify_props: Option<Obj>,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_status: Option<Obj>,
}

/// `PostImportData` and `DirectPostImportData` (`channel_members` instead of team and channel; reactions,
/// replies and attachments without `omitempty`).
#[derive(Default)]
struct PostData {
    direct: bool,
    team: String,
    channel: String,
    channel_members: Vec<String>,
    user: String,
    r#type: String,
    message: String,
    props: Value,
    create_at: i64,
    edit_at: i64,
    flagged_by: Vec<String>,
    reactions: Option<Raw>,
    replies: Option<Raw>,
    attachments: Option<Vec<Obj>>,
    is_pinned: bool,
    thread_followers: Option<Vec<Obj>>,
}

impl Serialize for PostData {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut o = Obj::new();
        if self.direct {
            o = o.f("channel_members", &self.channel_members);
        } else {
            o = o.f("team", &self.team).f("channel", &self.channel);
        }
        o = o
            .f("user", &self.user)
            .f("type", &self.r#type)
            .f("message", &self.message)
            .f("props", &self.props)
            .f("create_at", self.create_at)
            .f("edit_at", self.edit_at)
            .f("flagged_by", &self.flagged_by);
        if self.direct {
            o = o.f("reactions", &self.reactions).f("replies", &self.replies).f("attachments", &self.attachments);
        } else {
            o = o
                .o("reactions", self.reactions.as_ref())
                .o("replies", self.replies.as_ref())
                .o("attachments", self.attachments.as_ref());
        }
        o = o.f("is_pinned", self.is_pinned).o("thread_followers", self.thread_followers.as_ref());
        o.serialize(s)
    }
}

/// `json.Marshal`'s escaping: `<`, `>`, `&`, U+2028 and U+2029 as `\u` escapes, `\b` and `\f` too.
struct GoFormatter;

impl serde_json::ser::Formatter for GoFormatter {
    fn write_string_fragment<W: ?Sized + std::io::Write>(&mut self, w: &mut W, fragment: &str) -> std::io::Result<()> {
        let mut start = 0;
        for (i, c) in fragment.char_indices() {
            let esc = match c {
                '<' => "\\u003c",
                '>' => "\\u003e",
                '&' => "\\u0026",
                '\u{2028}' => "\\u2028",
                '\u{2029}' => "\\u2029",
                _ => continue,
            };
            w.write_all(&fragment.as_bytes()[start..i])?;
            w.write_all(esc.as_bytes())?;
            start = i + c.len_utf8();
        }
        w.write_all(&fragment.as_bytes()[start..])
    }
    fn write_char_escape<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        e: serde_json::ser::CharEscape,
    ) -> std::io::Result<()> {
        use serde_json::ser::CharEscape::*;
        let s = match e {
            Quote => "\\\"".to_string(),
            ReverseSolidus => "\\\\".to_string(),
            LineFeed => "\\n".to_string(),
            CarriageReturn => "\\r".to_string(),
            Tab => "\\t".to_string(),
            Backspace => "\\u0008".to_string(),
            FormFeed => "\\u000c".to_string(),
            AsciiControl(b) => format!("\\u{b:04x}"),
            _ => String::new(),
        };
        w.write_all(s.as_bytes())
    }
}

/// A value as Go's `json.Marshal` writes it.
pub fn go_json<T: Serialize>(v: &T) -> Vec<u8> {
    let mut out = vec![];
    let mut ser = serde_json::Serializer::with_formatter(&mut out, GoFormatter);
    let _ = v.serialize(&mut ser);
    out
}

// ---------- the export ----------

/// The zip being written, in a temporary file.
struct Out {
    zip: zip::ZipWriter<std::fs::File>,
}

impl Out {
    /// `exportWriteLine`.
    fn line(&mut self, l: &Line) -> Res<()> {
        let mut b = go_json(l);
        b.push(b'\n');
        self.zip
            .write_all(&b)
            .map_err(|e| jobs::go_error("BulkExport", "app.export.export_write_line.io_writer.error", &e.to_string()))
    }
}

struct Ctx<'a> {
    app: &'a App,
    job: &'a mut Job,
}

impl Ctx<'_> {
    /// `updateJobProgress`: a counter in the job's data, saved while in progress.
    async fn progress(&mut self, key: &str, v: usize) {
        self.job.data_mut().insert(key.into(), v.to_string());
        let _ = jobs::update_optimistically(self.app, self.job, jobs::IN_PROGRESS).await;
    }
}

fn split_names(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

fn db(where_: &str, id: &str) -> impl Fn(sqlx::Error) -> String {
    let (w, i) = (where_.to_string(), id.to_string());
    move |e| jobs::go_error(&w, &i, &e.to_string())
}

/// The export worker: options from the job's data, the archive written to the export directory.
pub async fn export_job(app: &State, job: &mut Job) -> Res<()> {
    let flag = |k: &str| job.get(k) == Some("true");
    let opts = Opts {
        attachments: flag("include_attachments"),
        archived: flag("include_archived_channels"),
        pictures: flag("include_profile_pictures"),
        roles: flag("include_roles_and_schemes"),
        emoji: flag("include_custom_emoji"),
        teams: split_names(job.get("team_name").unwrap_or("")),
        channels: split_names(job.get("channel_name").unwrap_or("")),
    };
    let out_path = app.cfg.str("ExportSettings.Directory").to_string();
    let name = format!("{}_export.zip", job.id);
    let tmp = std::env::temp_dir().join(format!("export-{}.zip", job.id));
    let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut out = Out { zip: zip::ZipWriter::new(file) };
    let r = bulk_export(app, job, &mut out, &opts).await;
    let finished = out.zip.finish().map_err(|e| e.to_string());
    let r = r.and(finished.map(|_| ()));
    if r.is_ok() {
        // WriteExportFileContext
        let data = std::fs::read(&tmp).map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&tmp);
        let data = data?;
        app.files
            .write(&crate::files::clean(&format!("{out_path}/{name}")), data.into())
            .await
            .map_err(|e| jobs::go_error("WriteExportFile", "api.file.write_file.app_error", &e.to_string()))?;
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

struct Opts {
    attachments: bool,
    archived: bool,
    pictures: bool,
    roles: bool,
    emoji: bool,
    teams: Vec<String>,
    channels: Vec<String>,
}

/// `BulkExport`.
async fn bulk_export(app: &App, job: &mut Job, out: &mut Out, o: &Opts) -> Res<()> {
    if !o.channels.is_empty() && o.teams.is_empty() {
        return Err(jobs::go_error(
            "BulkExport",
            "app.export.channel_requires_team.error",
            "ChannelName requires TeamName to be set",
        ));
    }
    let deflate = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    out.zip
        .start_file("import.jsonl", deflate)
        .map_err(|e| jobs::go_error("BulkExport", "app.export.zip_create.error", &e.to_string()))?;
    job.data_mut();
    let mut c = Ctx { app, job };
    export_version(out, &o.teams, &o.channels)?;
    if o.roles {
        export_roles_and_schemes(&mut c, out).await?;
    }
    let team_names = export_teams(&mut c, out, &o.teams).await?;
    export_channels(&mut c, out, &team_names, o.archived, &o.channels).await?;
    let mut pictures = export_users(&mut c, out, o).await?;
    pictures.extend(export_bots(&mut c, out, o).await?);
    let mut attachments = export_posts(&mut c, out, o).await?;
    let mut emoji_paths = vec![];
    if o.emoji || (o.teams.is_empty() && o.channels.is_empty()) {
        emoji_paths = export_emoji(&mut c, out).await?;
    }
    let mut direct_attachments = vec![];
    if o.teams.is_empty() {
        export_direct_channels(&mut c, out, o.archived).await?;
        direct_attachments = export_direct_posts(&mut c, out, o.attachments, o.archived).await?;
    }
    if o.attachments {
        let mut warnings = vec![];
        let total = attachments.len() + direct_attachments.len() + emoji_paths.len();
        attachments.extend(direct_attachments);
        for p in &attachments {
            if let Err(e) = export_file(app, out, p).await {
                warnings.push(format!("Unable to export file attachment, attachment path: {p} , error: {e}"));
            }
        }
        for p in &emoji_paths {
            let _ = export_file(app, out, p).await;
        }
        if !warnings.is_empty() {
            let deflate = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            out.zip.start_file("warnings.txt", deflate).map_err(|e| e.to_string())?;
            for w in &warnings {
                out.zip.write_all(format!("{w}\n").as_bytes()).map_err(|e| e.to_string())?;
            }
            c.progress("num_warnings", warnings.len()).await;
        }
        c.progress("attachments_exported", total).await;
    }
    if o.pictures {
        for p in &pictures {
            let _ = export_file(app, out, p).await;
        }
        c.progress("profile_pictures_exported", pictures.len()).await;
    }
    Ok(())
}

/// `exportFile`: stored, under `data/`.
async fn export_file(app: &App, out: &mut Out, path: &str) -> Res<()> {
    let data = app
        .files
        .read(path)
        .await
        .map_err(|e| jobs::go_error("ReadFile", "api.file.read_file.app_error", &e.to_string()))?;
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    out.zip.start_file(format!("data/{}", crate::files::clean(path)), stored).map_err(|e| {
        jobs::go_error("exportFileAttachment", "app.export.export_attachment.zip_create_header.error", &e.to_string())
    })?;
    out.zip.write_all(&data).map_err(|e| {
        jobs::go_error("exportFileAttachment", "app.export.export_attachment.copy_file.error", &e.to_string())
    })
}

/// `exportVersion`.
fn export_version(out: &mut Out, teams: &[String], channels: &[String]) -> Res<()> {
    let created = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
    let created = go_rfc3339_nano(&created);
    let mut info = Obj::new()
        .f("generator", "mattermost-server")
        .f("version", format!("{} ({}, enterprise: false)", crate::SERVER_VERSION, crate::BUILD_HASH))
        .f("created", created);
    if !teams.is_empty() {
        let mut add = Obj::new().f("team_name", teams.join(","));
        if !channels.is_empty() {
            add = add.f("channel_name", channels.join(","));
        }
        info = info.f("additional", add);
    }
    out.line(&Line { t: "version", version: Some(1), info: Some(info), ..Default::default() })
}

/// Go's `time.RFC3339Nano`: the fraction without trailing zeros, `Z` only for UTC.
fn go_rfc3339_nano(s: &str) -> String {
    let Some((date, rest)) = s.split_once('.') else { return s.to_string() };
    let split = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let (frac, zone) = rest.split_at(split);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() { format!("{date}{zone}") } else { format!("{date}.{frac}{zone}") }
}

type RoleRow = (String, String, String, String, bool);

fn role_data(r: &RoleRow) -> RoleData {
    RoleData {
        name: r.0.clone(),
        display_name: r.1.clone(),
        description: r.2.clone(),
        permissions: r.3.split_whitespace().map(str::to_string).collect(),
        scheme_managed: r.4,
    }
}

/// `exportRolesAndSchemes`: team schemes, channel schemes (with their roles), then every other role.
async fn export_roles_and_schemes(c: &mut Ctx<'_>, out: &mut Out) -> Res<()> {
    let roles: Vec<RoleRow> = sqlx::query_as(
        "SELECT Name, COALESCE(DisplayName, ''), COALESCE(Description, ''), COALESCE(Permissions, ''), COALESCE(SchemeManaged, false) FROM Roles",
    )
    .fetch_all(&c.app.db)
    .await
    .map_err(db("exportRolesAndSchemes", "app.role.get_all.app_error"))?;
    let by_name: HashMap<&str, &RoleRow> = roles.iter().map(|r| (r.0.as_str(), r)).collect();
    let mut scheme_roles: HashSet<String> = HashSet::new();
    for scope in ["team", "channel"] {
        let mut cnt = 0;
        loop {
            type S = (
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
            );
            let page: Vec<S> = sqlx::query_as(
                "SELECT COALESCE(Name, ''), COALESCE(DisplayName, ''), COALESCE(Description, ''), COALESCE(Scope, ''),
                 COALESCE(DefaultTeamAdminRole, ''), COALESCE(DefaultTeamUserRole, ''), COALESCE(DefaultTeamGuestRole, ''),
                 COALESCE(DefaultChannelAdminRole, ''), COALESCE(DefaultChannelUserRole, ''), COALESCE(DefaultChannelGuestRole, ''),
                 COALESCE(DefaultPlaybookAdminRole, ''), COALESCE(DefaultPlaybookMemberRole, ''), COALESCE(DefaultRunAdminRole, ''),
                 COALESCE(DefaultRunMemberRole, '')
                 FROM Schemes WHERE DeleteAt = 0 AND Scope = $1 ORDER BY CreateAt DESC LIMIT 100 OFFSET $2",
            )
            .bind(scope)
            .bind(cnt as i64)
            .fetch_all(&c.app.db)
            .await
            .map_err(db("exportSchemes", "app.scheme.get_all_page.app_error"))?;
            for s in &page {
                let role = |n: &str| by_name.get(n).map(|r| role_data(r));
                if s.3 == "team" {
                    for r in [&s.4, &s.5, &s.6, &s.10, &s.11, &s.12, &s.13] {
                        scheme_roles.insert(r.clone());
                    }
                }
                for r in [&s.7, &s.8, &s.9] {
                    scheme_roles.insert(r.clone());
                }
                let team = s.3 == "team";
                let data = SchemeData {
                    name: s.0.clone(),
                    display_name: s.1.clone(),
                    description: s.2.clone(),
                    scope: s.3.clone(),
                    default_team_admin_role: if team { role(&s.4) } else { None },
                    default_team_user_role: if team { role(&s.5) } else { None },
                    default_channel_admin_role: role(&s.7),
                    default_channel_user_role: role(&s.8),
                    default_team_guest_role: if team { role(&s.6) } else { None },
                    default_channel_guest_role: role(&s.9),
                };
                out.line(&Line { t: "scheme", scheme: Some(data), ..Default::default() })?;
            }
            cnt += page.len();
            c.progress(&format!("{scope}_schemes_exported"), cnt).await;
            if page.len() < 100 {
                break;
            }
        }
    }
    let mut cnt = 0;
    for r in &roles {
        if !scheme_roles.contains(&r.0) {
            out.line(&Line { t: "role", role: Some(role_data(r)), ..Default::default() })?;
            cnt += 1;
        }
    }
    c.progress("roles_exported", cnt).await;
    Ok(())
}

const ZERO: &str = "00000000000000000000000000";

fn team_line(name: &str, display: &str, t: &str, desc: &str, open: bool, scheme: Option<String>) -> Line {
    let team = Obj::new()
        .f("name", name)
        .f("display_name", display)
        .f("type", t)
        .f("description", desc)
        .f("allow_open_invite", open)
        .o("scheme", scheme);
    Line { t: "team", team: Some(team), ..Default::default() }
}

/// `exportAllTeams`: the teams asked for, or every team not deleted.
async fn export_teams(c: &mut Ctx<'_>, out: &mut Out, filters: &[String]) -> Res<HashSet<String>> {
    type T = (String, String, String, String, String, bool, i64, Option<String>);
    const Q: &str =
        "SELECT t.Id, t.Name, COALESCE(t.DisplayName, ''), COALESCE(t.Type::text, ''), COALESCE(t.Description, ''),
        COALESCE(t.AllowOpenInvite, false), t.DeleteAt, s.Name FROM Teams t LEFT JOIN Schemes s ON t.SchemeId = s.Id";
    let mut names = HashSet::new();
    if !filters.is_empty() {
        for f in filters {
            let t: Option<T> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{Q} WHERE t.Name = $1")))
                .bind(f)
                .fetch_optional(&c.app.db)
                .await
                .map_err(db("exportAllTeams", "app.team.get_by_name.app_error"))?;
            let Some(t) = t else {
                return Err(jobs::go_error("exportAllTeams", "app.export.export_all_teams.team_not_found.error", ""));
            };
            if t.6 != 0 {
                return Err(jobs::go_error("exportAllTeams", "app.export.export_all_teams.team_deleted.error", ""));
            }
            names.insert(t.1.clone());
            out.line(&team_line(&t.1, &t.2, &t.3, &t.4, t.5, t.7))?;
        }
        return Ok(names);
    }
    let (mut after, mut cnt) = (ZERO.to_string(), 0);
    loop {
        let page: Vec<T> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{Q} WHERE t.Id > $1 ORDER BY t.Id LIMIT 1000")))
            .bind(&after)
            .fetch_all(&c.app.db)
            .await
            .map_err(db("exportAllTeams", "app.team.get_all.app_error"))?;
        if page.is_empty() {
            break;
        }
        cnt += page.len();
        c.progress("teams_exported", cnt).await;
        for t in page {
            after = t.0.clone();
            if t.6 != 0 {
                continue;
            }
            names.insert(t.1.clone());
            out.line(&team_line(&t.1, &t.2, &t.3, &t.4, t.5, t.7))?;
        }
    }
    Ok(names)
}

/// `exportAllChannels`: public and private channels of the exported teams.
async fn export_channels(
    c: &mut Ctx<'_>,
    out: &mut Out,
    teams: &HashSet<String>,
    archived: bool,
    filters: &[String],
) -> Res<()> {
    type Ch = (String, String, String, String, String, String, i64, String, Option<String>);
    let (mut after, mut cnt) = (ZERO.to_string(), 0);
    let wanted: HashSet<&String> = filters.iter().collect();
    let mut found: HashSet<String> = HashSet::new();
    loop {
        let page: Vec<Ch> = sqlx::query_as(
            "SELECT c.Id, c.Name, COALESCE(c.DisplayName, ''), c.Type::text, COALESCE(c.Header, ''), COALESCE(c.Purpose, ''), c.DeleteAt,
             t.Name, s.Name FROM Channels c JOIN Teams t ON c.TeamId = t.Id LEFT JOIN Schemes s ON c.SchemeId = s.Id
             WHERE c.Id > $1 AND c.Type IN ('O', 'P') ORDER BY c.Id LIMIT 1000",
        )
        .bind(&after)
        .fetch_all(&c.app.db)
        .await
        .map_err(db("exportAllChannels", "app.channel.get_all.app_error"))?;
        if page.is_empty() {
            break;
        }
        cnt += page.len();
        c.progress("channels_exported", cnt).await;
        for ch in page {
            after = ch.0.clone();
            if (ch.6 != 0 && !archived) || !teams.contains(&ch.7) || (!wanted.is_empty() && !wanted.contains(&ch.1)) {
                continue;
            }
            found.insert(ch.1.clone());
            let o = Obj::new()
                .f("team", &ch.7)
                .f("name", &ch.1)
                .f("display_name", &ch.2)
                .f("type", &ch.3)
                .f("header", &ch.4)
                .f("purpose", &ch.5)
                .o("scheme", ch.8.clone())
                .f("deleted_at", ch.6);
            out.line(&Line { t: "channel", channel: Some(o), ..Default::default() })?;
        }
    }
    for n in filters {
        if !found.contains(n) {
            return Err(jobs::go_error(
                "exportAllChannels",
                "app.export.export_all_channels.channel_not_found.error",
                &format!("channel {n:?} not found in any of the specified teams"),
            ));
        }
    }
    Ok(())
}

/// `exportablePreferences`: (category, name) to the import field.
fn exportable(cat: &str, name: &str) -> Option<&'static str> {
    Some(match (cat, name) {
        ("theme", "") => "Theme",
        ("advanced_settings", "feature_enabled_markdown_preview") => "UseMarkdownPreview",
        ("advanced_settings", "formatting") => "UseFormatting",
        ("advanced_settings", "send_on_ctrl_enter") => "SendOnCtrlEnter",
        ("advanced_settings", "code_block_ctrl_enter") => "CodeBlockCtrlEnter",
        ("advanced_settings", "join_leave") => "ShowJoinLeave",
        ("advanced_settings", "unread_scroll_position") => "ShowUnreadScrollPosition",
        ("advanced_settings", "sync_drafts") => "SyncDrafts",
        ("sidebar_settings", "show_unread_section") => "ShowUnreadSection",
        ("sidebar_settings", "limit_visible_dms_gms") => "LimitVisibleDmsGms",
        ("display_settings", "use_military_time") => "UseMilitaryTime",
        ("display_settings", "collapse_previews") => "CollapsePreviews",
        ("display_settings", "message_display") => "MessageDisplay",
        ("display_settings", "channel_display_mode") => "ChannelDisplayMode",
        ("display_settings", "collapse_consecutive_messages") => "CollapseConsecutive",
        ("display_settings", "colorize_usernames") => "ColorizeUsernames",
        ("display_settings", "name_format") => "NameFormat",
        ("tutorial_step", "") => "TutorialStep",
        ("notifications", "email_interval") => "EmailInterval",
        _ => return None,
    })
}

async fn ids(app: &App, sql: &str, binds: &[&str], w: &str, id: &str) -> Res<Vec<String>> {
    let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql.to_string()));
    for b in binds {
        q = q.bind(*b);
    }
    q.fetch_all(&app.db).await.map_err(db(w, id))
}

fn arch(archived: bool) -> &'static str {
    if archived { "" } else { "AND Channels.DeleteAt = 0" }
}

async fn post_authors_for_team(app: &App, team: &str, archived: bool) -> Res<Vec<String>> {
    let sql = format!(
        "SELECT DISTINCT Posts.UserId FROM Posts INNER JOIN Channels ON Posts.ChannelId = Channels.Id INNER JOIN Teams ON Channels.TeamId = Teams.Id
         WHERE Teams.Name = $1 AND Posts.DeleteAt = 0 {} AND Teams.DeleteAt = 0",
        arch(archived)
    );
    ids(app, &sql, &[team], "exportAllUsers", "app.post.get_author_ids_for_team.app_error").await
}

/// Authors (`Posts`), reactors (`Reactions`) or followers (`ThreadMemberships`) in a channel of a team.
async fn channel_ids(app: &App, table: &str, team: &str, channel: &str, archived: bool, id: &str) -> Res<Vec<String>> {
    let join = match table {
        "Posts" => "",
        "Reactions" => "INNER JOIN Posts ON Reactions.PostId = Posts.Id",
        _ => "INNER JOIN Posts ON ThreadMemberships.PostId = Posts.Id",
    };
    let sql = format!(
        "SELECT DISTINCT {table}.UserId FROM {table} {join} INNER JOIN Channels ON Posts.ChannelId = Channels.Id
         INNER JOIN Teams ON Channels.TeamId = Teams.Id WHERE Teams.Name = $1 AND Channels.Name = $2 AND Posts.DeleteAt = 0 {} AND Teams.DeleteAt = 0",
        arch(archived)
    );
    ids(app, &sql, &[team, channel], "exportAllUsers", id).await
}

type UserRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    bool,
);

/// `exportAllUsers`: every user (bots apart), with preferences, notify props, custom status and memberships;
/// for scoped exports only users related to the scope, with memberships synthesized for post authors who left.
async fn export_users(c: &mut Ctx<'_>, out: &mut Out, o: &Opts) -> Res<Vec<String>> {
    let app = c.app;
    let mut pictures = vec![];
    let mut authors_by_team: HashMap<String, HashSet<String>> = HashMap::new();
    let mut author_ids: HashSet<String> = HashSet::new();
    for t in &o.teams {
        let ids = post_authors_for_team(app, t, o.archived).await?;
        author_ids.extend(ids.iter().cloned());
        authors_by_team.insert(t.clone(), ids.into_iter().collect());
    }
    let mut related: HashSet<String> = HashSet::new();
    let mut authors_by_channel: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut members_by_channel: HashMap<(String, String), HashSet<String>> = HashMap::new();
    if !o.channels.is_empty() {
        for t in &o.teams {
            let team_id: Option<String> = sqlx::query_scalar("SELECT Id FROM Teams WHERE Name = $1")
                .bind(t)
                .fetch_optional(&app.db)
                .await
                .map_err(db("exportAllUsers", "app.team.get_by_name.app_error"))?;
            let team_id =
                team_id.ok_or_else(|| jobs::go_error("exportAllUsers", "app.team.get_by_name.app_error", ""))?;
            for ch in &o.channels {
                let sql = format!(
                    "SELECT Id FROM Channels WHERE TeamId = $1 AND Name = $2{}",
                    if o.archived { "" } else { " AND DeleteAt = 0" }
                );
                let Some(cid) = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql))
                    .bind(&team_id)
                    .bind(ch)
                    .fetch_optional(&app.db)
                    .await
                    .map_err(db("exportAllUsers", "app.channel.get_by_name.app_error"))?
                else {
                    continue;
                };
                let members = ids(
                    app,
                    "SELECT UserId FROM ChannelMembers WHERE ChannelId = $1",
                    &[&cid],
                    "exportAllUsers",
                    "app.channel.get_member_ids.app_error",
                )
                .await?;
                related.extend(members.iter().cloned());
                members_by_channel.insert((t.clone(), ch.clone()), members.into_iter().collect());
                let authors =
                    channel_ids(app, "Posts", t, ch, o.archived, "app.post.get_author_ids_for_channel.app_error")
                        .await?;
                related.extend(authors.iter().cloned());
                authors_by_channel.insert((t.clone(), ch.clone()), authors.into_iter().collect());
                related.extend(
                    channel_ids(
                        app,
                        "Reactions",
                        t,
                        ch,
                        o.archived,
                        "app.reaction.get_author_ids_for_channel.app_error",
                    )
                    .await?,
                );
                related.extend(
                    channel_ids(
                        app,
                        "ThreadMemberships",
                        t,
                        ch,
                        o.archived,
                        "app.thread.get_follower_ids_for_channel.app_error",
                    )
                    .await?,
                );
            }
        }
    }
    let (mut after, mut cnt) = (ZERO.to_string(), 0);
    loop {
        let page: Vec<UserRow> = sqlx::query_as(
            "SELECT u.Id, u.Username, COALESCE(u.Email, ''), COALESCE(u.AuthService, ''), u.AuthData, COALESCE(u.Nickname, ''),
             COALESCE(u.FirstName, ''), COALESCE(u.LastName, ''), COALESCE(u.Position, ''), COALESCE(u.Roles, ''), COALESCE(u.Locale, ''),
             u.DeleteAt, COALESCE(u.Props::text, '{}'), COALESCE(u.NotifyProps::text, '{}'), b.UserId IS NOT NULL
             FROM Users u LEFT JOIN Bots b ON b.UserId = u.Id WHERE u.Id > $1 ORDER BY u.Id ASC LIMIT 1000",
        )
        .bind(&after)
        .fetch_all(&app.db)
        .await
        .map_err(db("exportAllUsers", "app.user.get.app_error"))?;
        if page.is_empty() {
            break;
        }
        cnt += page.len();
        c.progress("users_exported", cnt).await;
        for u in page {
            after = u.0.clone();
            if u.14 || (!o.channels.is_empty() && !related.contains(&u.0)) {
                continue;
            }
            let mut user = user_data(app, &u).await?;
            if o.pictures {
                let p = format!("users/{}/profile.png", u.0);
                if app.files.exists(&p).await {
                    user.profile_image = Some(p.clone());
                    pictures.push(p);
                }
            }
            let mut members = memberships(app, &u.0, o.archived, &o.teams, &o.channels).await?;
            if o.channels.is_empty() && !o.teams.is_empty() && members.is_empty() && !author_ids.contains(&u.0) {
                continue;
            }
            if !o.teams.is_empty() {
                let real: HashSet<String> = members.iter().map(|m| m.0.clone()).collect();
                for t in &o.teams {
                    if real.contains(t) {
                        continue;
                    }
                    let in_channel = |ch: &String| {
                        let k = (t.clone(), ch.clone());
                        authors_by_channel.get(&k).is_some_and(|s| s.contains(&u.0))
                            || members_by_channel.get(&k).is_some_and(|s| s.contains(&u.0))
                    };
                    let author =
                        authors_by_team.get(t).is_some_and(|s| s.contains(&u.0)) || o.channels.iter().any(in_channel);
                    if !author {
                        continue;
                    }
                    let synth: Vec<Obj> = o
                        .channels
                        .iter()
                        .filter(|ch| in_channel(ch))
                        .map(|ch| Obj::new().f("name", ch).f("roles", "channel_user"))
                        .collect();
                    members.push((
                        t.clone(),
                        Obj::new()
                            .f("name", t)
                            .f("roles", "team_user")
                            .o("channels", (!synth.is_empty()).then_some(synth)),
                    ));
                }
            }
            user.teams = Some((!members.is_empty()).then(|| members.into_iter().map(|m| m.1).collect()));
            out.line(&Line { t: "user", user: Some(user), ..Default::default() })?;
        }
    }
    Ok(pictures)
}

/// `importLineFromUser` with the exportable preferences, notify props and custom status.
async fn user_data(app: &App, u: &UserRow) -> Res<UserData> {
    let prefs: Vec<(String, String, String)> =
        sqlx::query_as("SELECT Category, Name, COALESCE(Value, '') FROM Preferences WHERE UserId = $1")
            .bind(&u.0)
            .fetch_all(&app.db)
            .await
            .map_err(db("GetPreferencesForUser", "app.preference.get_all.app_error"))?;
    let mut p: HashMap<&'static str, Option<String>> = HashMap::new();
    for (cat, name, mut value) in prefs {
        let name = if cat == "tutorial_step" { String::new() } else { name };
        if cat == "notifications" && name == "email_interval" {
            value = match value.as_str() {
                "30" => "immediately".into(),
                "900" => "fifteen".into(),
                "3600" => "hour".into(),
                "0" => String::new(),
                _ => value,
            };
        }
        if let Some(k) = exportable(&cat, &name) {
            p.insert(k, (!value.is_empty()).then_some(value));
        }
    }
    let mut g = |k: &str| p.remove(k).flatten();
    let magic = u.3 == "magic_link";
    let notify: BTreeMap<String, String> = serde_json::from_str(&u.13).unwrap_or_default();
    let np = |k: &str| notify.get(k).cloned();
    let props: BTreeMap<String, String> = serde_json::from_str(&u.12).unwrap_or_default();
    let custom = props
        .get("customStatus")
        .and_then(|s| serde_json::from_str::<Option<serde_json::Map<String, Value>>>(s).ok().flatten())
        .map(|m| {
            let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let exp = m.get("expires_at").and_then(Value::as_str).unwrap_or("0001-01-01T00:00:00Z").to_string();
            Obj::new().f("emoji", s("emoji")).f("text", s("text")).f("duration", s("duration")).f("expires_at", exp)
        });
    Ok(UserData {
        username: u.1.clone(),
        email: u.2.clone(),
        auth_service: (!u.3.is_empty() && !magic).then(|| u.3.clone()),
        auth_data: if magic { None } else { u.4.clone() },
        nickname: u.5.clone(),
        first_name: u.6.clone(),
        last_name: u.7.clone(),
        position: u.8.clone(),
        roles: u.9.clone(),
        locale: u.10.clone(),
        feature_enabled_markdown_preview: g("UseMarkdownPreview"),
        formatting: g("UseFormatting"),
        show_unread_section: g("ShowUnreadSection"),
        theme: g("Theme"),
        military_time: g("UseMilitaryTime"),
        link_previews: g("CollapsePreviews"),
        message_display: g("MessageDisplay"),
        colorize_usernames: g("ColorizeUsernames"),
        channel_display_mode: g("ChannelDisplayMode"),
        tutorial_step: g("TutorialStep"),
        email_interval: g("EmailInterval"),
        name_format: g("NameFormat"),
        send_on_ctrl_enter: g("SendOnCtrlEnter"),
        show_join_leave: g("ShowJoinLeave"),
        sync_drafts: g("SyncDrafts"),
        show_unread_scroll_position: g("ShowUnreadScrollPosition"),
        limit_visible_dms_gms: g("LimitVisibleDmsGms"),
        code_block_ctrl_enter: g("CodeBlockCtrlEnter"),
        delete_at: u.11,
        notify_props: Some(
            Obj::new()
                .f("desktop", np("desktop"))
                .f("desktop_sound", np("desktop_sound"))
                .f("email", np("email"))
                .f("mobile", np("push"))
                .f("mobile_push_status", np("push_status"))
                .f("channel", np("channel"))
                .f("comments", np("comments"))
                .f("mention_keys", np("mention_keys")),
        ),
        custom_status: custom,
        ..Default::default()
    })
}

/// `buildUserTeamAndChannelMemberships`: (team name, membership) of teams not left, with the channels.
async fn memberships(
    app: &App,
    user_id: &str,
    archived: bool,
    teams: &[String],
    channels: &[String],
) -> Res<Vec<(String, Obj)>> {
    type TM = (String, String, String, i64, bool, Option<bool>, Option<bool>);
    let tms: Vec<TM> = sqlx::query_as(
        "SELECT tm.TeamId, t.Name, COALESCE(tm.Roles, ''), tm.DeleteAt, (tm.SchemeGuest IS NOT NULL AND tm.SchemeGuest), tm.SchemeUser, tm.SchemeAdmin
         FROM TeamMembers tm JOIN Teams t ON tm.TeamId = t.Id WHERE tm.UserId = $1 AND t.DeleteAt = 0",
    )
    .bind(user_id)
    .fetch_all(&app.db)
    .await
    .map_err(db("buildUserTeamAndChannelMemberships", "app.team.get_members.app_error"))?;
    let favorites: HashSet<String> =
        sqlx::query_scalar("SELECT Name FROM Preferences WHERE UserId = $1 AND Category = 'favorite_channel'")
            .bind(user_id)
            .fetch_all(&app.db)
            .await
            .map_err(db("GetPreferenceByCategoryForUser", "app.preference.get_category.app_error"))?
            .into_iter()
            .collect();
    let mut out = vec![];
    for (team_id, team_name, roles, delete_at, sg, su, sa) in tms {
        if delete_at != 0 || (!teams.is_empty() && !teams.contains(&team_name)) {
            continue;
        }
        let mut r: Vec<&str> = roles.split_whitespace().collect();
        if sa.unwrap_or(false) {
            r.push("team_admin");
        }
        if su.unwrap_or(false) {
            r.push("team_user");
        }
        if sg {
            r.push("team_guest");
        }
        type CM = (String, String, String, i64, i64, i64, i64, i64, i64, String, Option<bool>, Option<bool>, bool);
        let cms: Vec<CM> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT cm.ChannelId, c.Name, COALESCE(cm.Roles, ''), COALESCE(cm.LastViewedAt, 0), COALESCE(cm.MsgCount, 0),
             COALESCE(cm.MentionCount, 0), COALESCE(cm.MentionCountRoot, 0), COALESCE(cm.UrgentMentionCount, 0), COALESCE(cm.MsgCountRoot, 0),
             COALESCE(cm.NotifyProps::text, '{{}}'), cm.SchemeUser, cm.SchemeAdmin, (cm.SchemeGuest IS NOT NULL AND cm.SchemeGuest)
             FROM ChannelMembers cm INNER JOIN Channels c ON cm.ChannelId = c.Id WHERE cm.UserId = $1 AND c.TeamId = $2{}
             AND c.Type::text NOT IN ('S')",
            if archived { "" } else { " AND c.DeleteAt = 0" }
        )))
        .bind(user_id)
        .bind(&team_id)
        .fetch_all(&app.db)
        .await
        .map_err(db("buildUserChannelMemberships", "app.channel.get_members.app_error"))?;
        let mut chans = vec![];
        for m in cms {
            if !channels.is_empty() && !channels.contains(&m.1) {
                continue;
            }
            let mut cr: Vec<&str> = m.2.split_whitespace().collect();
            if m.11.unwrap_or(false) {
                cr.push("channel_admin");
            }
            if m.10.unwrap_or(false) {
                cr.push("channel_user");
            }
            if m.12 {
                cr.push("channel_guest");
            }
            let props: BTreeMap<String, String> = serde_json::from_str(&m.9).unwrap_or_default();
            let np = Obj::new()
                .f("desktop", props.get("desktop"))
                .f("mobile", props.get("push"))
                .f("mark_unread", props.get("mark_unread"));
            chans.push(
                Obj::new()
                    .f("name", &m.1)
                    .f("roles", cr.join(" "))
                    .f("notify_props", np)
                    .f("favorite", favorites.contains(&m.0))
                    .f("mention_count", m.5)
                    .f("mention_count_root", m.6)
                    .f("urgend_mention_count", m.7)
                    .f("msg_count", m.4)
                    .f("msg_count_root", m.8)
                    .f("last_viewed_at", m.3),
            );
        }
        let theme: Option<String> =
            sqlx::query_scalar("SELECT Value FROM Preferences WHERE UserId = $1 AND Category = 'theme' AND Name = $2")
                .bind(user_id)
                .bind(&team_id)
                .fetch_optional(&app.db)
                .await
                .ok()
                .flatten();
        out.push((
            team_name.clone(),
            Obj::new()
                .f("name", &team_name)
                .f("roles", r.join(" "))
                .o("theme", theme)
                .f("channels", (!chans.is_empty()).then_some(chans)),
        ));
    }
    Ok(out)
}

/// `exportAllBots`: for scoped exports only bots that posted in the scope.
async fn export_bots(c: &mut Ctx<'_>, out: &mut Out, o: &Opts) -> Res<Vec<String>> {
    let app = c.app;
    let mut pictures = vec![];
    let mut authors: Option<HashSet<String>> = None;
    if !o.channels.is_empty() {
        let mut s = HashSet::new();
        for t in &o.teams {
            for ch in &o.channels {
                s.extend(
                    channel_ids(app, "Posts", t, ch, o.archived, "app.post.get_author_ids_for_channel.app_error")
                        .await?,
                );
            }
        }
        authors = Some(s);
    } else if !o.teams.is_empty() {
        let mut s = HashSet::new();
        for t in &o.teams {
            s.extend(post_authors_for_team(app, t, o.archived).await?);
        }
        authors = Some(s);
    }
    let (mut after, mut cnt) = (String::new(), 0);
    loop {
        let page: Vec<(String, String, String, String, String, i64)> = sqlx::query_as(
            "SELECT b.UserId, u.Username, COALESCE(u.FirstName, ''), COALESCE(b.Description, ''), COALESCE(b.OwnerId, ''), b.DeleteAt
             FROM Bots b JOIN Users u ON u.Id = b.UserId WHERE b.UserId > $1 ORDER BY b.UserId ASC LIMIT 1000",
        )
        .bind(&after)
        .fetch_all(&app.db)
        .await
        .map_err(db("exportAllBots", "app.user.get.app_error"))?;
        cnt += page.len();
        c.progress("bots_exported", cnt).await;
        let n = page.len();
        for b in page {
            after = b.0.clone();
            if authors.as_ref().is_some_and(|a| !a.contains(&b.0)) {
                continue;
            }
            let owner: Option<String> = sqlx::query_scalar("SELECT Username FROM Users WHERE Id = $1")
                .bind(&b.4)
                .fetch_optional(&app.db)
                .await
                .map_err(db("exportAllBots", "app.user.get.app_error"))?;
            let mut bot = Obj::new();
            if o.pictures {
                let p = format!("users/{}/profile.png", b.0);
                if app.files.exists(&p).await {
                    bot = bot.f("profile_image", &p);
                    pictures.push(p);
                }
            }
            bot = bot
                .f("username", &b.1)
                .f("owner", owner.unwrap_or_else(|| b.4.clone()))
                .f("display_name", &b.2)
                .f("description", &b.3)
                .f("delete_at", b.5);
            out.line(&Line { t: "bot", bot: Some(bot), ..Default::default() })?;
        }
        if n < 1000 {
            break;
        }
    }
    Ok(pictures)
}

const POST_COLS: &str = "p.Id, p.UserId, p.ChannelId, COALESCE(p.Type, ''), COALESCE(p.Message, ''), COALESCE(p.Props::text, '{}'),
    p.CreateAt, p.EditAt, COALESCE(p.IsPinned, false), COALESCE(p.HasReactions, false), COALESCE(p.FileIds, '[]'), p.DeleteAt";
const FLAGGED: &str = "COALESCE(json_agg(u1.username) FILTER (WHERE u1.username IS NOT NULL), '[]')::text";

type PostRow = (String, String, String, String, String, String, i64, i64, bool, bool, String, i64, String, String);

fn has_files(ids: &str) -> bool {
    serde_json::from_str::<Vec<String>>(ids).is_ok_and(|v| !v.is_empty())
}

/// `exportAllPosts`: root posts of the exported channels with replies, reactions, followers, attachments.
async fn export_posts(c: &mut Ctx<'_>, out: &mut Out, o: &Opts) -> Res<Vec<String>> {
    let mut attachments = vec![];
    let mut cnt = 0;
    let filters: Vec<(String, String)> = if o.teams.is_empty() {
        vec![(String::new(), String::new())]
    } else if o.channels.is_empty() {
        o.teams.iter().map(|t| (t.clone(), String::new())).collect()
    } else {
        o.teams.iter().flat_map(|t| o.channels.iter().map(move |ch| (t.clone(), ch.clone()))).collect()
    };
    for (team, channel) in filters {
        let mut after = ZERO.to_string();
        loop {
            let posts = parents_for_export(c.app, &after, o.archived, &team, &channel).await?;
            if posts.is_empty() {
                break;
            }
            cnt += posts.len();
            c.progress("posts_exported", cnt).await;
            for (p, team_name, channel_name) in posts {
                after = p.0.clone();
                if p.11 != 0 {
                    continue;
                }
                let (replies, reply_files) = post_replies(c.app, &p.0).await?;
                if o.attachments {
                    attachments.extend(reply_files);
                }
                let followers = followers(c.app, &p.0).await?;
                // An empty list without reactions; Go's nil slice (null) when the reactors are all gone.
                let reactions = if p.9 { nil_if_empty(reactions(c.app, &p.0).await?) } else { raw(&Vec::<Obj>::new()) };
                let files = if has_files(&p.10) { Some(post_files(c.app, &p.0).await?) } else { None };
                if o.attachments
                    && let Some(f) = &files
                {
                    attachments.extend(f.iter().cloned());
                }
                let post = PostData {
                    team: team_name,
                    channel: channel_name,
                    user: p.12.clone(),
                    r#type: p.3.clone(),
                    message: p.4.clone(),
                    props: serde_json::from_str(&p.5).unwrap_or(Value::Null),
                    create_at: p.6,
                    edit_at: p.7,
                    is_pinned: p.8,
                    flagged_by: serde_json::from_str(&p.13).unwrap_or_default(),
                    replies: Some(nil_if_empty(replies)),
                    reactions: Some(reactions),
                    attachments: files.map(|f| f.into_iter().map(|p| Obj::new().f("path", p)).collect()),
                    thread_followers: (!followers.is_empty()).then_some(followers),
                    ..Default::default()
                };
                out.line(&Line { t: "post", post: Some(post), ..Default::default() })?;
            }
        }
    }
    Ok(attachments)
}

/// `GetParentsForExportAfter`: the next page of root posts (skipping pages whose channels are all excluded).
async fn parents_for_export(
    app: &App,
    after: &str,
    archived: bool,
    team: &str,
    channel: &str,
) -> Res<Vec<(PostRow, String, String)>> {
    let mut after = after.to_string();
    let mut cond = "Teams.DeleteAt = 0".to_string();
    if !archived {
        cond += " AND Channels.DeleteAt = 0";
    }
    if !team.is_empty() {
        cond += " AND Teams.Name = $2";
    }
    if !channel.is_empty() {
        cond += if team.is_empty() { " AND Channels.Name = $2" } else { " AND Channels.Name = $3" };
    }
    let fail = db("GetParentsForExportAfter", "app.post.get_posts.app_error");
    loop {
        let root_ids: Vec<String> = if team.is_empty() && channel.is_empty() {
            sqlx::query_scalar("SELECT Id FROM Posts WHERE Posts.Id > $1 AND Posts.RootId = '' AND Posts.DeleteAt = 0 ORDER BY Posts.Id LIMIT 1000")
                .bind(&after)
                .fetch_all(&app.db)
                .await
                .map_err(&fail)?
        } else {
            let sql = format!(
                "SELECT Posts.Id FROM Posts INNER JOIN Channels ON Posts.ChannelId = Channels.Id INNER JOIN Teams ON Channels.TeamId = Teams.Id
                 WHERE Posts.Id > $1 AND Posts.RootId = '' AND Posts.DeleteAt = 0 AND {cond} ORDER BY Posts.Id LIMIT 1000"
            );
            let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql)).bind(&after);
            for b in [team, channel].into_iter().filter(|b| !b.is_empty()) {
                q = q.bind(b);
            }
            q.fetch_all(&app.db).await.map_err(&fail)?
        };
        let Some(last) = root_ids.last().cloned() else { return Ok(vec![]) };
        let shift = |s: &str| s.replace("$3", "$4").replace("$2", "$3");
        let sql = format!(
            "SELECT {POST_COLS}, Users.Username, {FLAGGED}, Teams.Name, Channels.Name
             FROM (SELECT * FROM Posts WHERE Id = ANY($1)) p
             LEFT JOIN Preferences ON p.Id = Preferences.Name LEFT JOIN Users u1 ON Preferences.UserId = u1.Id
             INNER JOIN Channels ON p.ChannelId = Channels.Id INNER JOIN Teams ON Channels.TeamId = Teams.Id
             INNER JOIN Users ON p.UserId = Users.Id
             WHERE {} GROUP BY p.Id, p.UserId, p.ChannelId, p.Type, p.Message, p.Props::text, p.CreateAt, p.EditAt, p.IsPinned,
             p.HasReactions, p.FileIds, p.DeleteAt, Users.Username, Teams.Name, Channels.Name ORDER BY p.Id",
            shift(&cond).replace("$3", "$2").replace("$4", "$3")
        );
        type Full = (
            String,
            String,
            String,
            String,
            String,
            String,
            i64,
            i64,
            bool,
            bool,
            String,
            i64,
            String,
            String,
            String,
            String,
        );
        let mut q = sqlx::query_as::<_, Full>(sqlx::AssertSqlSafe(sql)).bind(&root_ids);
        for b in [team, channel].into_iter().filter(|b| !b.is_empty()) {
            q = q.bind(b);
        }
        let rows = q.fetch_all(&app.db).await.map_err(&fail)?;
        if rows.is_empty() {
            after = last;
            continue;
        }
        return Ok(rows
            .into_iter()
            .map(|r| ((r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10, r.11, r.12, r.13), r.14, r.15))
            .collect());
    }
}

/// `buildPostReplies`: the replies (not deleted) with reactions and attachments, and their files.
async fn post_replies(app: &App, root: &str) -> Res<(Vec<Obj>, Vec<String>)> {
    let sql = format!(
        "SELECT {POST_COLS}, u2.Username, {FLAGGED} FROM Posts p LEFT JOIN Preferences ON p.Id = Preferences.Name
         LEFT JOIN Users u1 ON Preferences.UserId = u1.Id INNER JOIN Users u2 ON p.UserId = u2.Id
         WHERE p.RootId = $1 AND p.DeleteAt = 0 GROUP BY p.Id, u2.Username ORDER BY p.Id"
    );
    let rows: Vec<PostRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(root)
        .fetch_all(&app.db)
        .await
        .map_err(db("buildPostReplies", "app.post.get_posts.app_error"))?;
    let (mut replies, mut files) = (vec![], vec![]);
    for r in rows {
        let flagged: Vec<String> = serde_json::from_str(&r.13).unwrap_or_default();
        let mut o = Obj::new()
            .f("user", &r.12)
            .f("type", &r.3)
            .f("message", &r.4)
            .f("props", serde_json::from_str::<Value>(&r.5).unwrap_or(Value::Null))
            .f("create_at", r.6)
            .f("edit_at", r.7)
            .f("flagged_by", flagged);
        if r.9 {
            o = o.f("reactions", nil_if_empty(reactions(app, &r.0).await?));
        }
        if has_files(&r.10) {
            let f = post_files(app, &r.0).await?;
            o = o.f("attachments", f.iter().map(|p| Obj::new().f("path", p)).collect::<Vec<_>>());
            files.extend(f);
        }
        o = o.f("is_pinned", r.8);
        replies.push(o);
    }
    Ok((replies, files))
}

/// A Go slice built by appending: `null` when nothing was appended.
fn nil_if_empty(v: Vec<Obj>) -> Raw {
    if v.is_empty() { raw_null() } else { raw(&v) }
}

/// `buildThreadFollowers`.
async fn followers(app: &App, post_id: &str) -> Res<Vec<Obj>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT Users.Username, COALESCE(ThreadMemberships.LastViewed, 0), COALESCE(ThreadMemberships.UnreadMentions, 0)
         FROM ThreadMemberships INNER JOIN Users ON ThreadMemberships.UserId = Users.Id WHERE PostId = $1 AND Following = true",
    )
    .bind(post_id)
    .fetch_all(&app.db)
    .await
    .map_err(db("buildThreadFollowers", "app.thread.get_threadmembers_for_export.app_error"))?;
    Ok(rows.into_iter().map(|r| Obj::new().f("user", r.0).f("last_viewed", r.1).f("unread_mentions", r.2)).collect())
}

/// `BuildPostReactions`: reactions of users that still exist.
async fn reactions(app: &App, post_id: &str) -> Res<Vec<Obj>> {
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT u.Username, r.CreateAt, r.EmojiName FROM Reactions r JOIN Users u ON u.Id = r.UserId
         WHERE r.PostId = $1 AND COALESCE(r.DeleteAt, 0) = 0 ORDER BY r.CreateAt",
    )
    .bind(post_id)
    .fetch_all(&app.db)
    .await
    .map_err(db("BuildPostReactions", "app.reaction.get_for_post.app_error"))?;
    Ok(rows.into_iter().map(|r| Obj::new().f("user", r.0).f("create_at", r.1).f("emoji_name", r.2)).collect())
}

/// `buildPostAttachments`: the paths of the post's files.
async fn post_files(app: &App, post_id: &str) -> Res<Vec<String>> {
    sqlx::query_scalar("SELECT COALESCE(Path, '') FROM FileInfo WHERE PostId = $1 AND DeleteAt = 0 ORDER BY CreateAt")
        .bind(post_id)
        .fetch_all(&app.db)
        .await
        .map_err(db("buildPostAttachments", "app.file_info.get_for_post.app_error"))
}

/// `exportCustomEmoji` into the archive: `emoji/<id>/image`.
async fn export_emoji(c: &mut Ctx<'_>, out: &mut Out) -> Res<Vec<String>> {
    let (mut page, mut cnt, mut paths) = (0i64, 0, vec![]);
    loop {
        let list: Vec<(String, String)> =
            sqlx::query_as("SELECT Id, Name FROM Emoji WHERE DeleteAt = 0 ORDER BY Name LIMIT 100 OFFSET $1")
                .bind(page * 100)
                .fetch_all(&c.app.db)
                .await
                .map_err(db("GetEmojiList", "app.emoji.get_list.internal_error"))?;
        if list.is_empty() {
            break;
        }
        cnt += list.len();
        c.progress("emojis_exported", cnt).await;
        page += 1;
        for (id, name) in list {
            let path = format!("emoji/{id}/image");
            paths.push(path.clone());
            let e = Obj::new().f("name", name).f("image", path);
            out.line(&Line { t: "emoji", emoji: Some(e), ..Default::default() })?;
        }
    }
    Ok(paths)
}

type MemberRow = (String, String, String, i64, i64, i64, i64, i64, i64, String, Option<bool>, Option<bool>, bool);

/// `exportAllDirectChannels`: direct and group channels with their participants (inconsistent ones skipped and
/// listed in the job's `skipped_direct_channels`).
async fn export_direct_channels(c: &mut Ctx<'_>, out: &mut Out, archived: bool) -> Res<()> {
    let app = c.app;
    let (mut after, mut cnt) = (ZERO.to_string(), 0);
    loop {
        let sql = format!(
            "SELECT Id, Name, Type::text, COALESCE(Header, ''), DeleteAt FROM Channels WHERE Id > $1 AND Type IN ('D', 'G'){} ORDER BY Id LIMIT 1000",
            if archived { "" } else { " AND DeleteAt = 0" }
        );
        let chans: Vec<(String, String, String, String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&after)
            .fetch_all(&app.db)
            .await
            .map_err(db("exportAllDirectChannels", "app.channel.get_all_direct.app_error"))?;
        if chans.is_empty() {
            break;
        }
        cnt += chans.len();
        c.progress("direct_channels_exported", cnt).await;
        let ids: Vec<String> = chans.iter().map(|c| c.0.clone()).collect();
        let members: Vec<MemberRow> = sqlx::query_as(
            "SELECT cm.ChannelId, u.Username, cm.UserId, COALESCE(cm.LastViewedAt, 0), COALESCE(cm.MsgCount, 0), COALESCE(cm.MsgCountRoot, 0),
             COALESCE(cm.MentionCount, 0), COALESCE(cm.MentionCountRoot, 0), COALESCE(cm.UrgentMentionCount, 0), COALESCE(cm.NotifyProps::text, '{}'),
             cm.SchemeUser, cm.SchemeAdmin, (cm.SchemeGuest IS NOT NULL AND cm.SchemeGuest)
             FROM ChannelMembers cm JOIN Users u ON u.Id = cm.UserId WHERE cm.ChannelId = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&app.db)
        .await
        .map_err(db("exportAllDirectChannels", "app.channel.get_all_direct.app_error"))?;
        let mut by_channel: HashMap<&str, Vec<&MemberRow>> = HashMap::new();
        for m in &members {
            by_channel.entry(m.0.as_str()).or_default().push(m);
        }
        for ch in &chans {
            after = ch.0.clone();
            if ch.4 != 0 {
                continue;
            }
            let Some(ms) = by_channel.get(ch.0.as_str()) else { continue };
            let skip = match ch.2.as_str() {
                "G" => {
                    let mut users: Vec<&str> = ms.iter().map(|m| m.2.as_str()).collect();
                    users.sort();
                    ch.1 != group_name(&users)
                }
                _ => {
                    let other = ch.1.split_once("__").map(|(_, b)| b).unwrap_or("");
                    !other.is_empty() && ms.len() == 1 && ch.1.split_once("__").is_some_and(|(a, b)| a != b)
                }
            };
            if skip {
                let d = c.job.data_mut();
                let prev = d.get("skipped_direct_channels").cloned().unwrap_or_default();
                d.insert("skipped_direct_channels".into(), format!("{prev},{}", ch.0));
                continue;
            }
            let favorited: Vec<String> = sqlx::query_scalar(
                "SELECT u.Username FROM Preferences p JOIN Users u ON u.Id = p.UserId WHERE p.Category = 'favorite_channel' AND p.Name = $1 AND p.Value = 'true'",
            )
            .bind(&ch.0)
            .fetch_all(&app.db)
            .await
            .map_err(db("buildFavoritedByList", "app.preference.get_category.app_error"))?;
            let mut shown = vec![];
            if ch.2 == "G" {
                for m in ms.iter() {
                    let n: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM Preferences WHERE UserId = $1 AND Category = 'group_channel_show' AND Name = $2 AND Value = 'true')",
                    )
                    .bind(&m.2)
                    .bind(&ch.0)
                    .fetch_one(&app.db)
                    .await
                    .map_err(db("buildShownByList", "app.preference.get_category.app_error"))?;
                    if n {
                        shown.push(m.1.clone());
                    }
                }
            } else {
                for (i, m) in ms.iter().enumerate() {
                    let other = if ms.len() == 2 { ms[(i + 1) % 2] } else { m };
                    let n: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM Preferences WHERE UserId = $1 AND Category = 'direct_channel_show' AND Name = $2 AND Value = 'true')",
                    )
                    .bind(&m.2)
                    .bind(&other.2)
                    .fetch_one(&app.db)
                    .await
                    .map_err(db("buildShownByList", "app.preference.get_category.app_error"))?;
                    if n {
                        shown.push(m.1.clone());
                    }
                }
            }
            let mut parts: Vec<&MemberRow> = ms.clone();
            if parts.len() == 1 {
                parts.push(parts[0]);
            }
            let participants: Vec<Obj> = parts.iter().map(|m| participant(m)).collect();
            let mut o = Obj::new().f("participants", participants);
            if !favorited.is_empty() {
                o = o.f("favorited_by", favorited);
            }
            if !shown.is_empty() {
                o = o.f("shown_by", shown);
            }
            o = o.f("header", &ch.3);
            out.line(&Line { t: "direct_channel", direct_channel: Some(o), ..Default::default() })?;
        }
    }
    Ok(())
}

/// `model.GetGroupNameFromUserIds`: the hex SHA-1 of the sorted ids joined.
fn group_name(sorted: &[&str]) -> String {
    use sha1::Digest;
    let d = sha1::Sha1::digest(sorted.concat().as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// `importDirectChannelMembersFromChannelMembers`.
fn participant(m: &MemberRow) -> Obj {
    let props: BTreeMap<String, String> = serde_json::from_str(&m.9).unwrap_or_default();
    let np = Obj::new()
        .f("desktop", props.get("desktop"))
        .f("mobile", props.get("push"))
        .f("mark_unread", props.get("mark_unread"))
        .o("email", props.get("email"))
        .o("ignore_channel_mentions", props.get("ignore_channel_mentions"))
        .o("channel_auto_follow_threads", props.get("channel_auto_follow_threads"));
    let nz = |v: i64| (v != 0).then_some(v);
    Obj::new()
        .f("username", &m.1)
        .f("notify_props", np)
        .o("scheme_user", m.10.filter(|b| *b))
        .o("scheme_admin", m.11.filter(|b| *b))
        .o("scheme_guest", m.12.then_some(true))
        .o("mention_count", nz(m.6))
        .o("mention_count_root", nz(m.7))
        .o("urgend_mention_count", nz(m.8))
        .o("msg_count", nz(m.4))
        .o("msg_count_root", nz(m.5))
        .o("last_viewed_at", nz(m.3))
}

/// `exportAllDirectPosts`.
async fn export_direct_posts(c: &mut Ctx<'_>, out: &mut Out, with_files: bool, archived: bool) -> Res<Vec<String>> {
    let app = c.app;
    let (mut after, mut cnt, mut attachments) = (ZERO.to_string(), 0, vec![]);
    loop {
        let sql = format!(
            "SELECT {POST_COLS}, u2.Username, {FLAGGED} FROM Posts p LEFT JOIN Preferences ON p.Id = Preferences.Name
             LEFT JOIN Users u1 ON Preferences.UserId = u1.Id JOIN Channels ON p.ChannelId = Channels.Id JOIN Users u2 ON p.UserId = u2.Id
             WHERE p.Id > $1 AND p.RootId = '' AND p.DeleteAt = 0 AND Channels.Type IN ('D', 'G'){}
             GROUP BY p.Id, u2.Username ORDER BY p.Id LIMIT 1000",
            if archived { "" } else { " AND Channels.DeleteAt = 0" }
        );
        let posts: Vec<PostRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&after)
            .fetch_all(&app.db)
            .await
            .map_err(db("exportAllDirectPosts", "app.post.get_direct_posts.app_error"))?;
        if posts.is_empty() {
            break;
        }
        cnt += posts.len();
        c.progress("direct_posts_exported", cnt).await;
        let chans: Vec<String> = posts.iter().map(|p| p.2.clone()).collect();
        let members: Vec<(String, String)> =
            sqlx::query_as("SELECT cm.ChannelId, u.Username FROM ChannelMembers cm JOIN Users u ON u.Id = cm.UserId WHERE cm.ChannelId = ANY($1)")
                .bind(&chans)
                .fetch_all(&app.db)
                .await
                .map_err(db("exportAllDirectPosts", "app.post.get_direct_posts.app_error"))?;
        let mut by_channel: HashMap<&str, Vec<String>> = HashMap::new();
        for (ch, u) in &members {
            by_channel.entry(ch.as_str()).or_default().push(u.clone());
        }
        let skipped: HashSet<String> =
            c.job.get("skipped_direct_channels").unwrap_or("").split(',').map(str::to_string).collect();
        for p in posts {
            after = p.0.clone();
            if p.11 != 0 || skipped.contains(&p.2) {
                continue;
            }
            let files = if has_files(&p.10) { post_files(app, &p.0).await? } else { vec![] };
            if with_files {
                attachments.extend(files.iter().cloned());
            }
            let (replies, reply_files) = post_replies(app, &p.0).await?;
            if with_files {
                attachments.extend(reply_files);
            }
            let mut members = by_channel.get(p.2.as_str()).cloned().unwrap_or_default();
            if members.len() == 1 {
                members.push(members[0].clone());
            }
            let followers = followers(app, &p.0).await?;
            let post = PostData {
                direct: true,
                channel_members: members,
                user: p.12.clone(),
                r#type: p.3.clone(),
                message: p.4.clone(),
                props: serde_json::from_str(&p.5).unwrap_or(Value::Null),
                create_at: p.6,
                edit_at: p.7,
                is_pinned: p.8,
                flagged_by: serde_json::from_str(&p.13).unwrap_or_default(),
                replies: Some(nil_if_empty(replies)),
                attachments: (!files.is_empty()).then(|| files.into_iter().map(|f| Obj::new().f("path", f)).collect()),
                thread_followers: (!followers.is_empty()).then_some(followers),
                ..Default::default()
            };
            out.line(&Line { t: "direct_post", direct_post: Some(post), ..Default::default() })?;
        }
    }
    Ok(attachments)
}

// ---------- /exports and /imports (api4/export.go, api4/import.go) ----------

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/exports", get(list_exports))
        .route("/api/v4/exports/{export_name}", get(download_export).delete(delete_export))
        .route("/api/v4/exports/{export_name}/presign-url", axum::routing::post(presign_export))
        .route("/api/v4/imports", get(list_imports))
        .route("/api/v4/imports/{import_name}", axum::routing::delete(delete_import))
}

/// `ListExportDirectory` / `ListDirectory`: the base names in a directory.
async fn base_names(app: &App, dir: &str) -> ApiResult<Vec<String>> {
    let entries = app
        .files
        .list(dir)
        .await
        .map_err(|e| AppError::internal("api.file.list_directory.app_error", e.to_string()))?;
    Ok(entries.into_iter().map(|(p, _)| p.rsplit('/').next().unwrap_or(&p).to_string()).collect())
}

/// `listExports`.
async fn list_exports(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Vec<String>>> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    Ok(Json(base_names(&app, app.cfg.str("ExportSettings.Directory")).await?))
}

/// `{export_name:.+\\.zip}` and `{import_name:.+\\.zip}`: anything else is the router's 404.
fn zip_name(path: &str, name: &str) -> ApiResult<()> {
    // A name is one file of the import/export directory: a decoded `/` or `..` (`..%2Fx.zip`) would reach any zip
    // of the file store. Go's route variable never holds a `/`, so such a path is its 404 too.
    if name.len() < 5 || !name.ends_with(".zip") || name.contains(['/', '\\']) || name.contains("..") {
        return Err(crate::api::route_not_found(path));
    }
    Ok(())
}

/// `generatePresignURLExport`: Cloud only.
async fn presign_export(St(app): St<State>, Auth(s): Auth, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    zip_name(&format!("/api/v4/exports/{name}/presign-url"), &name)?;
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    Err(AppError::new(StatusCode::FORBIDDEN, "app.export.generate_presigned_url.direct_download.app_error", ""))
}

/// `deleteExport`: a missing file is fine.
async fn delete_export(St(app): St<State>, Auth(s): Auth, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    zip_name(&format!("/api/v4/exports/{name}"), &name)?;
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    let p = crate::files::clean(&format!("{}/{name}", app.cfg.str("ExportSettings.Directory")));
    if app.files.exists(&p).await {
        app.files.remove(&p).await.map_err(|e| AppError::internal("api.file.remove_file.app_error", e.to_string()))?;
    }
    Ok(Json(json!({"status": "OK"})))
}

/// `downloadExport`: the zip, as an attachment.
async fn download_export(St(app): St<State>, Auth(s): Auth, Path(name): Path<String>) -> ApiResult<Response> {
    zip_name(&format!("/api/v4/exports/{name}"), &name)?;
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    let p = crate::files::clean(&format!("{}/{name}", app.cfg.str("ExportSettings.Directory")));
    if !app.files.exists(&p).await {
        return Err(AppError::not_found("api.export.export_not_found.app_error", "Unable to find export file."));
    }
    let data =
        app.files.read(&p).await.map_err(|e| AppError::internal("api.file.read_file.app_error", e.to_string()))?;
    Ok((StatusCode::OK, [(header::CONTENT_TYPE, "application/zip".to_string())], data).into_response())
}

/// `listImports`: the files of the import directory, without incomplete uploads.
async fn list_imports(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Vec<String>>> {
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    let names = base_names(&app, app.cfg.str("ImportSettings.Directory")).await?;
    Ok(Json(names.into_iter().filter(|n| !n.ends_with(".tmp")).collect()))
}

/// `deleteImport`: incomplete uploads cannot be deleted here.
async fn delete_import(St(app): St<State>, Auth(s): Auth, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    zip_name(&format!("/api/v4/imports/{name}"), &name)?;
    app.require(&s, crate::perm::MANAGE_SYSTEM).await?;
    let p = crate::files::clean(&format!("{}/{name}", app.cfg.str("ImportSettings.Directory")));
    if app.files.exists(&p).await {
        app.files.remove(&p).await.map_err(|e| AppError::internal("api.file.remove_file.app_error", e.to_string()))?;
    }
    Ok(Json(json!({"status": "OK"})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_escaping() {
        let v = json!({"m": "a<b>&c\u{8}\u{2028}"});
        let bs = char::from(92u8).to_string();
        let want = format!("{{\"m\":\"a{bs}u003cb{bs}u003e{bs}u0026c{bs}u0008{bs}u2028\"}}");
        assert_eq!(String::from_utf8(go_json(&v)).unwrap(), want);
    }

    #[test]
    fn rfc3339_nano() {
        assert_eq!(go_rfc3339_nano("2026-11-20T10:00:00.120000000+03:00"), "2026-11-20T10:00:00.12+03:00");
        assert_eq!(go_rfc3339_nano("2026-11-20T10:00:00.000000000Z"), "2026-11-20T10:00:00Z");
    }

    #[test]
    fn group_names() {
        assert_eq!(group_name(&["a", "b"]).len(), 40);
    }
}
