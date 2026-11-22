//! The writes of bulk import, one function per line type, as Go's importer makes them: straight to the stores
//! (security and permission checks bypassed, validity enforced), through the app functions Go calls where it calls
//! them (team and channel creation, user updates, roles, bots, profile images, uploads).

// Based on Mattermost server/channels/app/import_functions.go and app/import.go (the SSO preflight), and the store
// functions they call in store/sqlstore (post, thread, team, channel, user, role, scheme, reaction, preference,
// file info, emoji, bot and channel member history stores), Copyright (c) 2015-present Mattermost, Inc. Licensed
// under AGPL-3.0.

use std::collections::{HashMap, HashSet};

use crate::app::{is_valid_id, new_id, now_ms};
use crate::bots::Bot;
use crate::bulk_import::limits::*;
use crate::bulk_import::*;
use crate::db::{self, Table};
use crate::model::{
    Channel, ChannelMember, Emoji, FileInfo, Post, Preference, Reaction, Role, Scheme, Session, Team, TeamMember,
    ThreadMembership, User,
};

fn warn(msg: &str) {
    crate::logs::warn(msg, &[]);
}

fn info(msg: &str) {
    crate::logs::info(msg, &[]);
}

/// `store.NewErrNotFound(...).Error()`.
fn not_found(resource: &str, id: &str) -> String {
    format!("resource \"{resource}\" not found, id: {id}")
}

/// A preference row.
fn pref(user_id: &str, category: &str, name: &str, value: &str) -> Preference {
    Preference { user_id: user_id.into(), category: category.into(), name: name.into(), value: value.into() }
}

/// `MaxEmojiFileSize`.
const MAX_EMOJI_FILE_SIZE: u64 = 1 << 19;

/// `model.ChannelModeratedPermissionsMap`'s keys: what a channel scheme's member roles keep (`filterModerated`).
const MODERATED: &[&str] = &[
    "create_post",
    "add_reaction",
    "remove_reaction",
    "manage_public_channel_members",
    "manage_private_channel_members",
    "use_channel_mentions",
    "add_bookmark_public_channel",
    "edit_bookmark_public_channel",
    "delete_bookmark_public_channel",
    "order_bookmark_public_channel",
    "add_bookmark_private_channel",
    "edit_bookmark_private_channel",
    "delete_bookmark_private_channel",
    "order_bookmark_private_channel",
];

/// `getPostStrID`: a post before it has an id.
fn post_key(p: &Post) -> String {
    format!("{}{}{}", p.create_at, p.channel_id, p.message)
}

/// A post of a line with what it was made from.
struct PostAndData {
    post: Post,
    flagged_by: Option<Vec<String>>,
    reactions: Option<Vec<ReactionData>>,
    replies: Option<Vec<ReplyData>>,
    team_id: String,
    line: usize,
}

type LineResult = Result<(), (usize, IErr)>;

impl Ctx {
    /// `importLine`: every line type but posts.
    pub async fn import_line(&self, line: LineData) -> IResult<()> {
        let null = |t: &str| Err(IErr::bad(&format!("app.import.import_line.null_{t}.error")));
        match line.r#type.as_str() {
            "role" => match line.role {
                Some(r) => self.import_role(&r).await,
                None => null("role"),
            },
            "scheme" => match line.scheme {
                Some(s) => self.import_scheme(&s).await,
                None => null("scheme"),
            },
            "team" => match line.team {
                Some(t) => self.import_team(&t).await,
                None => null("team"),
            },
            "channel" => match line.channel {
                Some(mut c) => self.import_channel(&mut c).await,
                None => null("channel"),
            },
            "user" => match line.user {
                Some(u) => self.import_user(&u).await,
                None => null("user"),
            },
            "bot" => match line.bot {
                Some(b) => self.import_bot(&b).await,
                None => null("bot"),
            },
            "direct_channel" => match line.direct_channel {
                Some(d) => self.import_direct_channel(&d).await,
                None => null("direct_channel"),
            },
            "emoji" => match line.emoji {
                Some(e) => self.import_emoji(&e).await,
                None => null("emoji"),
            },
            t => Err(IErr::bad("app.import.import_line.unknown_line_type.error").p("Type", t)),
        }
    }

    // ---------- lookups ----------

    async fn user_by_username(&self, name: &str) -> IResult<Option<User>> {
        Ok(db::one::<User>(&self.app.db, "Username = $1", &name.to_lowercase()).await?)
    }

    async fn user_by_auth(&self, auth_data: &str, service: &str) -> IResult<Option<User>> {
        let sql = format!("{} WHERE AuthData = $1 AND AuthService = $2", User::SELECT);
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(auth_data).bind(service).fetch_optional(&self.app.db).await?)
    }

    async fn team_by_name(&self, name: &str) -> IResult<Option<Team>> {
        Ok(db::one::<Team>(&self.app.db, "Name = $1", name).await?)
    }

    /// `App.GetSchemeByName`.
    async fn scheme_by_name(&self, name: &str) -> IResult<Scheme> {
        match db::one::<Scheme>(&self.app.db, "Name = $1", name).await {
            Ok(Some(s)) => Ok(s),
            Ok(None) => Err(IErr::new("GetSchemeByName", "app.scheme.get.app_error", 404)
                .wrap(not_found("Scheme", &format!("schemeName={name}")))),
            Err(e) => Err(IErr::new("GetSchemeByName", "app.scheme.get.app_error", 500).wrap(e)),
        }
    }

    /// `getUsersByUsernames`: by username (as given), then through the remap; all must be found unless scoped.
    async fn users_by_usernames(&self, names: &[String]) -> IResult<HashMap<String, User>> {
        let mut unique: Vec<String> = vec![];
        for n in names {
            if !unique.contains(n) {
                unique.push(n.clone());
            }
        }
        let some_missing = || IErr::bad("app.import.get_users_by_username.some_users_not_found.error");
        let found = self.profiles_by_usernames(&unique).await.map_err(|e| some_missing().wrap(e.text()))?;
        let mut users: HashMap<String, User> = found.into_iter().map(|u| (u.username.to_lowercase(), u)).collect();
        let (mut missed_src, mut missed_dest) = (vec![], vec![]);
        for src in &unique {
            let l = src.to_lowercase();
            if !users.contains_key(&l)
                && let Some(dest) = self.report.lookup(src)
            {
                missed_src.push(l);
                missed_dest.push(dest);
            }
        }
        if !missed_dest.is_empty() {
            let remapped = self
                .profiles_by_usernames(&missed_dest)
                .await
                .map_err(|e| some_missing().status(500).wrap(e.text()))?;
            let by_dest: HashMap<String, User> = remapped.into_iter().map(|u| (u.username.to_lowercase(), u)).collect();
            for (src, dest) in missed_src.into_iter().zip(missed_dest) {
                if let Some(u) = by_dest.get(&dest.to_lowercase()) {
                    users.insert(src, u.clone());
                }
            }
        }
        if users.len() != unique.len() && !self.deactivate_missing_users {
            return Err(some_missing());
        }
        Ok(users)
    }

    /// `GetProfilesByUsernames`: the names as given.
    async fn profiles_by_usernames(&self, names: &[String]) -> IResult<Vec<User>> {
        let sql = format!("{} WHERE Username = ANY($1) ORDER BY Username ASC", User::SELECT);
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(names).fetch_all(&self.app.db).await?)
    }

    /// `prefetchUsersByUsernames`: best effort.
    async fn prefetch_users(&self, names: &[String], users: &mut HashMap<String, User>) {
        if names.is_empty() {
            return;
        }
        if let Ok(found) = self.profiles_by_usernames(names).await {
            for u in found {
                users.insert(u.username.to_lowercase(), u);
            }
        }
    }

    /// `getTeamsByNames`: all of them or an error.
    async fn teams_by_names(&self, names: &[String]) -> IResult<HashMap<String, Team>> {
        let mut unique: Vec<String> = vec![];
        for n in names {
            if !unique.contains(n) {
                unique.push(n.clone());
            }
        }
        let err = || IErr::bad("app.import.get_teams_by_names.some_teams_not_found.error");
        let sql = format!("{} WHERE Name = ANY($1)", Team::SELECT);
        let teams: Vec<Team> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&unique)
            .fetch_all(&self.app.db)
            .await
            .map_err(|e| err().wrap(e))?;
        if teams.len() != unique.len() {
            return Err(err().wrap(not_found("Team", &format!("nameIn=[{}]", names.join(" ")))));
        }
        Ok(teams.into_iter().map(|t| (t.name.to_lowercase(), t)).collect())
    }

    /// `getChannelsByNames`: open and private channels of the team, archived too.
    async fn channels_by_names(&self, names: &[String], team_id: &str) -> IResult<HashMap<String, Channel>> {
        let sql = format!("{} WHERE Name = ANY($1) AND Type IN ('O', 'P') AND TeamId = $2", Channel::SELECT);
        let channels: Vec<Channel> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(names)
            .bind(team_id)
            .fetch_all(&self.app.db)
            .await
            .map_err(|e| IErr::bad("app.import.get_teams_by_names.some_teams_not_found.error").wrap(e))?;
        Ok(channels.into_iter().map(|c| (c.name.to_lowercase(), c)).collect())
    }

    async fn channel_by_name_include_deleted(&self, team_id: &str, name: &str) -> IResult<Option<Channel>> {
        let sql = format!("{} WHERE TeamId = $1 AND Name = $2", Channel::SELECT);
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(team_id).bind(name).fetch_optional(&self.app.db).await?)
    }

    /// `SqlPreferenceStore.Save`: upserted in one transaction, no events and no sidebar sync.
    async fn save_preferences(&self, prefs: &mut [Preference]) -> Result<(), String> {
        let mut tx = self.app.db.begin().await.map_err(|e| e.to_string())?;
        for p in prefs.iter_mut() {
            crate::preferences::pre_update(p);
            crate::preferences::is_valid(p).map_err(|e| e.message)?;
            sqlx::query(
                "INSERT INTO Preferences (UserId, Category, Name, Value) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (userid, category, name) DO UPDATE SET Value = $4",
            )
            .bind(&p.user_id)
            .bind(&p.category)
            .bind(&p.name)
            .bind(&p.value)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("failed to save Preference: {e}"))?;
        }
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    // ---------- roles and schemes ----------

    async fn import_role(&self, d: &RoleData) -> IResult<()> {
        info(&format!("Validating role role_name={}", d.name.as_deref().unwrap_or_default()));
        validate_role(d)?;
        if self.dry_run {
            return Ok(());
        }
        let name = d.name.clone().unwrap_or_default();
        info(&format!("Importing role role_name={name}"));
        let mut role =
            self.app.roles(&[name.as_str()]).await?.into_iter().next().map(|r| (*r).clone()).unwrap_or_default();
        role.name = name;
        if let Some(v) = &d.display_name {
            role.display_name = v.clone();
        }
        if let Some(v) = &d.description {
            role.description = v.clone();
        }
        if let Some(v) = &d.permissions {
            role.permissions = crate::model::Spaced(v.clone());
        }
        if let Some(v) = d.scheme_managed {
            role.scheme_managed = v;
        }
        if role.id.is_empty() {
            // CreateRole
            role.create_at = 0;
            role.update_at = 0;
            role.delete_at = 0;
            role.built_in = false;
            role.scheme_managed = false;
            self.create_role(&mut role, None).await.map_err(|e| match e {
                RoleErr::Invalid(m) => IErr::new("CreateRole", "app.role.save.invalid_role.app_error", 400).wrap(m),
                RoleErr::Db(m) => IErr::new("CreateRole", "app.role.save.insert.app_error", 500).wrap(m),
            })?;
        } else {
            self.app.update_role(&mut role).await?;
        }
        self.app.caches.role.remove(&role.name);
        self.app.caches.role_permissions.clear();
        Ok(())
    }

    /// `SqlRoleStore.createRole`.
    async fn create_role(&self, r: &mut Role, tx: Option<&mut sqlx::PgConnection>) -> Result<(), RoleErr> {
        crate::roles::role_is_valid(r).map_err(RoleErr::Invalid)?;
        r.id = new_id();
        r.create_at = now_ms();
        r.update_at = r.create_at;
        let q = sqlx::query(
            "INSERT INTO Roles (Id, Name, DisplayName, Description, Permissions, CreateAt, UpdateAt, DeleteAt, SchemeManaged,
                 BuiltIn, SchemeId)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(&r.id)
        .bind(&r.name)
        .bind(&r.display_name)
        .bind(&r.description)
        .bind(crate::roles::go_permissions(&r.permissions.0))
        .bind(r.create_at)
        .bind(r.update_at)
        .bind(r.delete_at)
        .bind(r.scheme_managed)
        .bind(r.built_in)
        .bind(&r.scheme_id);
        let res = match tx {
            Some(tx) => q.execute(tx).await,
            None => q.execute(&self.app.db).await,
        };
        res.map(drop).map_err(|e| RoleErr::Db(format!("failed to save Role: {e}")))
    }

    async fn import_scheme(&self, d: &SchemeData) -> IResult<()> {
        info(&format!("Validating scheme scheme_name={}", d.name.as_deref().unwrap_or_default()));
        validate_scheme(d)?;
        if self.dry_run {
            return Ok(());
        }
        let name = d.name.clone().unwrap_or_default();
        let scope = d.scope.clone().unwrap_or_default();
        info(&format!("Importing scheme scheme_name={name}"));
        let mut scheme = match self.scheme_by_name(&name).await {
            Ok(s) if s.scope != scope => {
                return Err(IErr::bad("app.import.import_scheme.scope_change.error").p("SchemeName", &s.name));
            }
            Ok(s) => s,
            Err(_) => Scheme::default(),
        };
        scheme.name = name;
        scheme.display_name = d.display_name.clone().unwrap_or_default();
        scheme.scope = scope.clone();
        if let Some(v) = &d.description {
            scheme.description = v.clone();
        }
        let scheme =
            if scheme.id.is_empty() { self.create_scheme(scheme).await? } else { self.update_scheme(scheme).await? };
        self.app.caches.scheme.remove(&scheme.id);

        let with_name = |r: &Option<RoleData>, name: &str, default_display: Option<&str>| -> RoleData {
            let mut r = r.clone().unwrap_or_else(|| RoleData {
                display_name: default_display.map(str::to_string),
                scheme_managed: Some(true),
                ..Default::default()
            });
            r.name = Some(name.to_string());
            r
        };
        if scheme.scope == "team" {
            self.import_role(&with_name(&d.default_team_admin_role, &scheme.default_team_admin_role, None)).await?;
            self.import_role(&with_name(&d.default_team_user_role, &scheme.default_team_user_role, None)).await?;
            self.import_role(&with_name(
                &d.default_team_guest_role,
                &scheme.default_team_guest_role,
                Some("Team Guest Role for Scheme"),
            ))
            .await?;
        }
        if scheme.scope == "team" || scheme.scope == "channel" {
            self.import_role(&with_name(&d.default_channel_admin_role, &scheme.default_channel_admin_role, None))
                .await?;
            self.import_role(&with_name(&d.default_channel_user_role, &scheme.default_channel_user_role, None)).await?;
            self.import_role(&with_name(
                &d.default_channel_guest_role,
                &scheme.default_channel_guest_role,
                Some("Channel Guest Role for Scheme"),
            ))
            .await?;
        }
        Ok(())
    }

    /// `App.CreateScheme` → `SqlSchemeStore.createScheme`: the scheme's roles from the built-in ones, then the row.
    async fn create_scheme(&self, mut s: Scheme) -> IResult<Scheme> {
        let save_err = |e: String| IErr::new("CreateScheme", "app.scheme.save.app_error", 500).wrap(e);
        s.id = new_id();
        if s.name.is_empty() {
            s.name = new_id();
        }
        let defaults = [
            "team_admin",
            "team_user",
            "team_guest",
            "channel_admin",
            "channel_user",
            "channel_guest",
            "playbook_admin",
            "playbook_member",
            "run_admin",
            "run_member",
        ];
        let names: Vec<String> = defaults.iter().map(|s| s.to_string()).collect();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT Name, COALESCE(Permissions, '') FROM Roles WHERE Name = ANY($1)")
                .bind(&names)
                .fetch_all(&self.app.db)
                .await
                .map_err(|e| save_err(e.to_string()))?;
        let perms: HashMap<String, Vec<String>> =
            rows.into_iter().map(|(n, p)| (n, p.split_whitespace().map(str::to_string).collect())).collect();
        if perms.len() != defaults.len() {
            return Err(save_err("createScheme: unable to retrieve default scheme roles".into()));
        }
        let mut tx = self.app.db.begin().await?;
        let role = |display: String, permissions: Vec<String>| Role {
            name: new_id(),
            display_name: display,
            permissions: crate::model::Spaced(permissions),
            scheme_managed: true,
            scheme_id: Some(s.id.clone()),
            ..Default::default()
        };
        let mut made: Vec<(&str, Role)> = vec![];
        if s.scope == "team" {
            for (key, label, base) in [
                ("team_admin", "Team Admin Role for Scheme", "team_admin"),
                ("team_user", "Team User Role for Scheme", "team_user"),
                ("team_guest", "Team Guest Role for Scheme", "team_guest"),
                ("playbook_admin", "Playbook Admin Role for Scheme", "playbook_admin"),
                ("playbook_member", "Playbook Member Role for Scheme", "playbook_member"),
                ("run_admin", "Run Admin Role for Scheme", "run_admin"),
                ("run_member", "Run Member Role for Scheme", "run_member"),
            ] {
                made.push((key, role(format!("{label} {}", s.name), perms[base].clone())));
            }
        }
        if s.scope == "team" || s.scope == "channel" {
            let channel = s.scope == "channel";
            let moderated =
                |p: &Vec<String>| p.iter().filter(|x| MODERATED.contains(&x.as_str())).cloned().collect::<Vec<_>>();
            let admin = if channel { vec![] } else { perms["channel_admin"].clone() };
            let user = if channel { moderated(&perms["channel_user"]) } else { perms["channel_user"].clone() };
            let guest = if channel { moderated(&perms["channel_guest"]) } else { perms["channel_guest"].clone() };
            made.push(("channel_admin", role(format!("Channel Admin Role for Scheme {}", s.name), admin)));
            made.push(("channel_user", role(format!("Channel User Role for Scheme {}", s.name), user)));
            made.push(("channel_guest", role(format!("Channel Guest Role for Scheme {}", s.name), guest)));
        }
        for (key, mut r) in made {
            self.create_role(&mut r, Some(&mut tx)).await.map_err(|e| match e {
                RoleErr::Invalid(m) | RoleErr::Db(m) => save_err(format!("unable to create Role: {m}")),
            })?;
            let slot = match key {
                "team_admin" => &mut s.default_team_admin_role,
                "team_user" => &mut s.default_team_user_role,
                "team_guest" => &mut s.default_team_guest_role,
                "playbook_admin" => &mut s.default_playbook_admin_role,
                "playbook_member" => &mut s.default_playbook_member_role,
                "run_admin" => &mut s.default_run_admin_role,
                "run_member" => &mut s.default_run_member_role,
                "channel_admin" => &mut s.default_channel_admin_role,
                "channel_user" => &mut s.default_channel_user_role,
                _ => &mut s.default_channel_guest_role,
            };
            *slot = r.name;
        }
        s.create_at = now_ms();
        s.update_at = s.create_at;
        if !scheme_valid(&s) {
            return Err(IErr::new("CreateScheme", "app.scheme.save.invalid_scheme.app_error", 400)
                .wrap(format!("invalid input: entity: Scheme field: <any> value: {s:?}")));
        }
        sqlx::query(
            "INSERT INTO Schemes (Id, Name, DisplayName, Description, Scope, DefaultTeamAdminRole, DefaultTeamUserRole,
                 DefaultTeamGuestRole, DefaultChannelAdminRole, DefaultChannelUserRole, DefaultChannelGuestRole, CreateAt,
                 UpdateAt, DeleteAt, DefaultPlaybookAdminRole, DefaultPlaybookMemberRole, DefaultRunAdminRole,
                 DefaultRunMemberRole)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
        )
        .bind(&s.id)
        .bind(&s.name)
        .bind(&s.display_name)
        .bind(&s.description)
        .bind(&s.scope)
        .bind(&s.default_team_admin_role)
        .bind(&s.default_team_user_role)
        .bind(&s.default_team_guest_role)
        .bind(&s.default_channel_admin_role)
        .bind(&s.default_channel_user_role)
        .bind(&s.default_channel_guest_role)
        .bind(s.create_at)
        .bind(s.update_at)
        .bind(s.delete_at)
        .bind(&s.default_playbook_admin_role)
        .bind(&s.default_playbook_member_role)
        .bind(&s.default_run_admin_role)
        .bind(&s.default_run_member_role)
        .execute(&mut *tx)
        .await
        .map_err(|e| save_err(format!("failed to save Scheme: {e}")))?;
        tx.commit().await?;
        Ok(s)
    }

    /// `App.UpdateScheme` → `SqlSchemeStore.Save` of an existing scheme.
    async fn update_scheme(&self, mut s: Scheme) -> IResult<Scheme> {
        if !scheme_valid(&s) || !is_valid_id(&s.id) {
            return Err(IErr::new("UpdateScheme", "app.scheme.save.invalid_scheme.app_error", 400)
                .wrap(format!("invalid input: entity: Scheme field: <any> value: {s:?}")));
        }
        s.update_at = now_ms();
        let n = sqlx::query(
            "UPDATE Schemes SET UpdateAt = $2, CreateAt = $3, DeleteAt = $4, Name = $5, DisplayName = $6, Description = $7,
                 Scope = $8, DefaultTeamAdminRole = $9, DefaultTeamUserRole = $10, DefaultTeamGuestRole = $11,
                 DefaultChannelAdminRole = $12, DefaultChannelUserRole = $13, DefaultChannelGuestRole = $14,
                 DefaultPlaybookMemberRole = $15, DefaultPlaybookAdminRole = $16, DefaultRunMemberRole = $17,
                 DefaultRunAdminRole = $18
             WHERE Id = $1",
        )
        .bind(&s.id)
        .bind(s.update_at)
        .bind(s.create_at)
        .bind(s.delete_at)
        .bind(&s.name)
        .bind(&s.display_name)
        .bind(&s.description)
        .bind(&s.scope)
        .bind(&s.default_team_admin_role)
        .bind(&s.default_team_user_role)
        .bind(&s.default_team_guest_role)
        .bind(&s.default_channel_admin_role)
        .bind(&s.default_channel_user_role)
        .bind(&s.default_channel_guest_role)
        .bind(&s.default_playbook_member_role)
        .bind(&s.default_playbook_admin_role)
        .bind(&s.default_run_member_role)
        .bind(&s.default_run_admin_role)
        .execute(&self.app.db)
        .await
        .map_err(|e| IErr::new("UpdateScheme", "app.scheme.save.app_error", 500).wrap(format!("failed to update Scheme: {e}")))?
        .rows_affected();
        if n != 1 {
            return Err(IErr::new("UpdateScheme", "app.scheme.save.app_error", 500).wrap("no record to update"));
        }
        Ok(s)
    }

    // ---------- teams and channels ----------

    async fn import_team(&self, d: &TeamData) -> IResult<()> {
        info(&format!("Validating team team_name={}", d.name.as_deref().unwrap_or_default()));
        validate_team(d)?;
        if self.dry_run {
            return Ok(());
        }
        info(&format!("Importing team team_name={}", d.name.as_deref().unwrap_or_default()));
        let name = d.name.as_deref().unwrap_or_default().to_lowercase();
        let existing = self.team_by_name(&name).await?;
        let is_new = existing.is_none();
        let mut team = existing.unwrap_or_else(|| Team { name: name.clone(), ..Default::default() });
        // An existing team of a migration into a named destination keeps the display name it has there.
        if self.destination_team.is_empty() || is_new {
            team.display_name = d.display_name.clone().unwrap_or_default();
        }
        team.r#type = d.r#type.clone().unwrap_or_default();
        if let Some(v) = &d.description {
            team.description = v.clone();
        }
        if let Some(v) = d.allow_open_invite {
            team.allow_open_invite = v;
        }
        if let Some(sn) = &d.scheme {
            let s = self.scheme_by_name(sn).await?;
            if s.delete_at != 0 {
                return Err(IErr::bad("app.import.import_team.scheme_deleted.error"));
            }
            if s.scope != "team" {
                return Err(IErr::bad("app.import.import_team.scheme_wrong_scope.error"));
            }
            team.scheme_id = Some(s.id);
        }
        if is_new {
            self.app.create_team_row(&mut team).await?;
        } else {
            self.app.update_team_row(&mut team).await?;
        }
        Ok(())
    }

    async fn import_channel(&self, d: &mut ChannelData) -> IResult<()> {
        info(&format!("Validating channel channel_name={}", d.name.as_deref().unwrap_or_default()));
        validate_channel(d)?;
        if self.dry_run {
            return Ok(());
        }
        let team_name = d.team.as_deref().unwrap_or_default().to_lowercase();
        let channel_name = d.name.as_deref().unwrap_or_default().to_lowercase();
        info(&format!("Importing channel channel_name={}", d.name.as_deref().unwrap_or_default()));
        let Some(team) = self.team_by_name(&team_name).await? else {
            return Err(IErr::bad("app.import.import_channel.team_not_found.error")
                .p("TeamName", &team_name)
                .wrap(not_found("Team", &format!("name={team_name}"))));
        };
        let mut channel = self
            .channel_by_name_include_deleted(&team.id, &channel_name)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| Channel { name: channel_name.clone(), ..Default::default() });
        channel.team_id = team.id.clone();
        channel.display_name = d.display_name.clone().unwrap_or_default();
        channel.r#type = d.r#type.clone().unwrap_or_default();
        if let Some(v) = &d.header {
            channel.header = v.clone();
        }
        if let Some(v) = &d.purpose {
            channel.purpose = v.clone();
        }
        if let Some(sn) = &d.scheme {
            let s = self.scheme_by_name(sn).await?;
            if s.delete_at != 0 {
                return Err(IErr::bad("app.import.import_channel.scheme_deleted.error"));
            }
            if s.scope != "channel" {
                return Err(IErr::bad("app.import.import_channel.scheme_wrong_scope.error"));
            }
            channel.scheme_id = Some(s.id);
        }
        let mut existing_delete_at = 0;
        if channel.id.is_empty() {
            // CreateChannel without a member.
            channel.display_name = channel.display_name.trim().to_string();
            channel.default_category_name = channel.default_category_name.trim().to_string();
            if self.app.save_channel(&mut channel, true).await?.is_err() {
                return Err(IErr::new("CreateChannel", "store.sql_channel.save_channel.exists.app_error", 400));
            }
        } else {
            existing_delete_at = channel.delete_at;
            channel.delete_at = 0;
            self.app.update_channel(&mut channel).await?;
        }
        let delete_at = match d.deleted_at {
            Some(t) if t > 0 => t,
            _ => existing_delete_at,
        };
        if delete_at > 0 {
            self.set_channel_delete_at(&channel, delete_at)
                .await
                .map_err(|e| IErr::new("BulkImport", "app.import.import_channel.deleting.app_error", 500).wrap(e))?;
        }
        Ok(())
    }

    /// `SqlChannelStore.Delete`: `DeleteAt` and `UpdateAt` set, the public channel row too.
    async fn set_channel_delete_at(&self, c: &Channel, t: i64) -> Result<(), sqlx::Error> {
        let mut tx = self.app.db.begin().await?;
        sqlx::query("UPDATE Channels SET DeleteAt = $1, UpdateAt = $1 WHERE Id = $2")
            .bind(t)
            .bind(&c.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE PublicChannels SET DeleteAt = $1 WHERE Id = $2")
            .bind(t)
            .bind(&c.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.app.invalidate_channel_cache(c);
        Ok(())
    }

    // ---------- users ----------

    async fn import_user(&self, d: &UserData) -> IResult<()> {
        let app = &self.app;
        info(&format!("Validating user user_name={}", d.username.as_deref().unwrap_or_default()));
        validate_user(d)?;
        if self.dry_run {
            return Ok(());
        }
        let username = d.username.clone().unwrap_or_default();
        let email = d.email.clone().unwrap_or_default();
        info(&format!("Importing user user_name={username}"));
        let (mut user_changed, mut notify_changed, mut roles_changed, mut auth_changed, mut verified_changed) =
            (false, false, false, false, false);
        let mut create_deactivated = false;
        let inactive = self.imported_users == "inactive";
        let fresh =
            || User { notify_props: sqlx::types::Json(crate::user_edit::default_notify_props()), ..Default::default() };
        let has_sso = d.auth_service.is_some() && d.auth_data.as_deref().is_some_and(|a| !a.is_empty());
        let mut matched_by_auth = false;
        let mut user = if has_sso {
            let (data, service) = (d.auth_data.clone().unwrap_or_default(), d.auth_service.clone().unwrap_or_default());
            match self
                .user_by_auth(&data, &service)
                .await
                .map_err(|e| IErr::new("importUser", "app.user.get_by_auth.app_error", 500).wrap(e.text()))?
            {
                Some(u) => {
                    matched_by_auth = true;
                    if u.username != username {
                        self.report.add(&username, &u.username);
                    }
                    u
                }
                None if self.deactivate_missing_users => {
                    warn(&format!(
                        "auth_data match failed during scoped import; creating a shell account to preserve posts username={username} auth_service={service} identity_match=not_found imported_users={}",
                        self.imported_users
                    ));
                    create_deactivated = inactive;
                    user_changed = true;
                    fresh()
                }
                None => {
                    match self.user_by_username(&username).await.map_err(|e| {
                        IErr::new("importUser", "app.user.get_by_username.app_error", 500).wrap(e.text())
                    })? {
                        Some(u) => u,
                        None => {
                            user_changed = true;
                            fresh()
                        }
                    }
                }
            }
        } else {
            match self
                .user_by_username(&username)
                .await
                .map_err(|e| IErr::new("importUser", "app.user.get_by_username.app_error", 500).wrap(e.text()))?
            {
                Some(u) => u,
                None => {
                    if self.deactivate_missing_users {
                        create_deactivated = inactive;
                        info(&format!(
                            "User not found on destination during scoped import; creating to preserve post authorship username={username} imported_users={}",
                            self.imported_users
                        ));
                    }
                    user_changed = true;
                    fresh()
                }
            }
        };

        if !matched_by_auth {
            user.username = username.clone();
        }
        let preserve_email = matched_by_auth && user.auth_service == "saml";
        if !preserve_email && user.email != email {
            user_changed = true;
            verified_changed = true;
            user.email = email.to_lowercase();
        }
        let mut password = String::new();
        let mut auth_service = String::new();
        let mut auth_data: Option<String> = None;
        if let Some(s) = &d.auth_service {
            if user.auth_service != *s {
                auth_changed = true;
            }
            auth_service = s.clone();
        }
        if let Some(a) = &d.auth_data {
            if user.auth_data.as_deref() != Some(a.as_str()) {
                auth_changed = true;
            }
            auth_data = Some(a.clone());
        } else if let Some(p) = &d.password {
            password = p.clone();
        } else {
            password = generate_password(app.cfg.i64("PasswordSettings.MinimumLength").max(0) as usize);
        }
        user.password = password.clone();
        user.auth_service = auth_service.clone();
        user.auth_data = auth_data.clone();
        if !user.email_verified {
            user.email_verified = true;
            verified_changed = true;
        }
        for (v, field) in [
            (&d.nickname, &mut user.nickname),
            (&d.first_name, &mut user.first_name),
            (&d.last_name, &mut user.last_name),
            (&d.position, &mut user.position),
        ] {
            if let Some(v) = v
                && field != v
            {
                *field = v.clone();
                user_changed = true;
            }
        }
        let locale =
            d.locale.clone().unwrap_or_else(|| app.cfg.str("LocalizationSettings.DefaultClientLocale").to_string());
        if user.locale != locale {
            user.locale = locale;
            user_changed = true;
        }
        if let Some(v) = d.delete_at
            && user.delete_at != v
        {
            user.delete_at = v;
            user_changed = true;
        }
        let mut roles = String::new();
        if let Some(r) = &d.roles {
            if user.roles != *r {
                roles = r.clone();
                roles_changed = true;
            }
        } else if user.roles.is_empty() {
            roles = "system_user".into();
            roles_changed = true;
        }
        user.roles = roles.clone();

        if let Some(np) = &d.notify_props {
            for (v, key) in [
                (&np.desktop, "desktop"),
                (&np.desktop_sound, "desktop_sound"),
                (&np.email, "email"),
                (&np.mobile, "push"),
                (&np.mobile_push_status, "push_status"),
                (&np.channel_trigger, "channel"),
                (&np.comments_trigger, "comments"),
                (&np.mention_keys, "mention_keys"),
            ] {
                if let Some(v) = v
                    && user.notify_props.0.get(key) != Some(v)
                {
                    user.notify_props.0.insert(key.into(), v.clone());
                    notify_changed = true;
                }
            }
            if np.mention_keys.is_none() {
                // UpdateMentionKeysFromUsername("")
                let keys: Vec<String> = crate::user_edit::mention_keys(
                    user.notify_props.0.get("mention_keys").map(String::as_str).unwrap_or(""),
                )
                .into_iter()
                .filter(|k| k != "@")
                .collect();
                let v = if keys.is_empty() { String::new() } else { format!(",{}", keys.join(",")) };
                user.notify_props.0.insert("mention_keys".into(), v);
            }
        }
        if let Some(cs) = &d.custom_status {
            crate::user_edit::set_custom_status_prop(&mut user, cs);
        }

        let saved: User = if user.id.is_empty() {
            if create_deactivated {
                user.props.0.insert("importedInactive".into(), "true".into());
            }
            // UserService.CreateUser from an import: the store's save, then the email verified.
            if user.auth_service.is_empty() {
                crate::user_edit::check_password(app, &user.password)?;
            }
            if let Err(e) = app.save_user_row(&mut user).await {
                let e = IErr::from(e);
                if self.deactivate_missing_users && e.id() == "app.user.save.username_exists.app_error" {
                    if let Ok(Some(existing)) = self.user_by_username(&username).await {
                        warn(&format!(
                            "Scoped import: attributing source user's content to the destination account holding the same username — not a verified identity match, review if the two instances don't share username conventions source_username={username} matched_dest_username={}",
                            existing.username
                        ));
                        self.report.add(&username, &existing.username);
                    }
                } else if self.deactivate_missing_users && e.id() == "app.user.save.email_exists.app_error" {
                    warn(&format!(
                        "Scoped import: source user's email already belongs to a differently-named destination account; leaving the source user unresolved rather than attributing their content to it — resolve the collision and re-import if this content matters source_username={username}"
                    ));
                }
                return Err(e.at("importUser"));
            }
            if user.email_verified
                && let Err(e) =
                    sqlx::query("UPDATE Users SET Email = lower($1), EmailVerified = true, UpdateAt = $2 WHERE Id = $3")
                        .bind(&user.email)
                        .bind(now_ms())
                        .bind(&user.id)
                        .execute(&app.db)
                        .await
            {
                warn(&format!("Failed to set email verified err={e}"));
            }
            app.invalidate_user(&user.id);
            let mut tutorial = [pref(&user.id, "tutorial_step", &user.id, "0")];
            if let Err(e) = self.save_preferences(&mut tutorial).await {
                warn(&format!("Encountered error saving tutorial preference err={e}"));
            }
            let mut u = app.user(&user.id).await?.map(|u| (*u).clone()).unwrap_or(user);
            if create_deactivated && u.delete_at == 0 {
                u = app.update_active(u, false).await?;
            }
            u
        } else {
            let mut s: Option<User> = None;
            if user_changed {
                s = Some(app.update_user(user.clone(), false, false).await?);
            }
            if roles_changed {
                let current = app.user(&user.id).await?.ok_or_else(crate::users::missing_account)?;
                s = Some(app.update_user_roles((*current).clone(), &roles, false).await?);
            }
            if notify_changed {
                sqlx::query("UPDATE Users SET NotifyProps = $1, UpdateAt = $2 WHERE Id = $3")
                    .bind(&user.notify_props)
                    .bind(now_ms())
                    .bind(&user.id)
                    .execute(&app.db)
                    .await
                    .map_err(|e| IErr::new("UpdateUser", "app.user.update.finding.app_error", 500).wrap(e))?;
                app.invalidate_user(&user.id);
                s = app.user(&user.id).await?.map(|u| (*u).clone());
            }
            if !password.is_empty() && d.password.is_some() {
                self.update_password(&user, &password).await?;
            } else if auth_changed {
                app.revoke_all_sessions(&user.id).await?;
                app.invalidate_user(&user.id);
                let now = now_ms();
                // UpdateAuthData: the email too when there is one.
                let r = sqlx::query(
                    "UPDATE Users SET Password = '', LastPasswordUpdate = $1, UpdateAt = $1, FailedAttempts = 0, AuthService = $2,
                         AuthData = $3, Email = CASE WHEN $5 = '' THEN Email ELSE lower($5) END
                     WHERE Id = $4",
                )
                    .bind(now)
                    .bind(&auth_service)
                    .bind(&auth_data)
                    .bind(&user.id)
                    .bind(&user.email)
                    .execute(&app.db)
                    .await;
                if let Err(e) = r {
                    let id = if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
                        ("app.user.update_auth_data.email_exists.app_error", 400)
                    } else {
                        ("app.user.update_auth_data.app_error", 500)
                    };
                    return Err(IErr::new("importUser", id.0, id.1).wrap(e));
                }
                app.invalidate_user(&user.id);
            }
            if verified_changed {
                sqlx::query("UPDATE Users SET Email = lower($1), EmailVerified = true, UpdateAt = $2 WHERE Id = $3")
                    .bind(&user.email)
                    .bind(now_ms())
                    .bind(&user.id)
                    .execute(&app.db)
                    .await
                    .map_err(|e| IErr::new("VerifyUserEmail", "app.user.verify_email.app_error", 500).wrap(e))?;
                app.invalidate_user(&user.id);
                app.publish_user_updated(&user.id).await;
            }
            s.unwrap_or(user)
        };

        if let Some(path) = &d.profile_image {
            self.import_profile_image(&saved.id, path, &d.profile_image_data).await?;
        }

        // Preferences.
        let id = saved.id.as_str();
        let mut prefs: Vec<Preference> = vec![];
        for (v, category, name) in [
            (&d.theme, "theme", ""),
            (&d.use_military_time, "display_settings", "use_military_time"),
            (&d.collapse_previews, "display_settings", "collapse_previews"),
            (&d.message_display, "display_settings", "message_display"),
            (&d.collapse_consecutive, "display_settings", "collapse_consecutive_messages"),
            (&d.colorize_usernames, "display_settings", "colorize_usernames"),
            (&d.channel_display_mode, "display_settings", "channel_display_mode"),
            (&d.tutorial_step, "tutorial_step", id),
            (&d.use_markdown_preview, "advanced_settings", "feature_enabled_markdown_preview"),
            (&d.use_formatting, "advanced_settings", "formatting"),
            (&d.show_unread_section, "sidebar_settings", "show_unread_section"),
            (&d.send_on_ctrl_enter, "advanced_settings", "send_on_ctrl_enter"),
            (&d.code_block_ctrl_enter, "advanced_settings", "code_block_ctrl_enter"),
            (&d.show_join_leave, "advanced_settings", "join_leave"),
            (&d.show_unread_scroll_position, "advanced_settings", "unread_scroll_position"),
            (&d.sync_drafts, "advanced_settings", "sync_drafts"),
            (&d.limit_visible_dms_gms, "sidebar_settings", "limit_visible_dms_gms"),
            (&d.name_format, "display_settings", "name_format"),
        ] {
            if let Some(v) = v {
                prefs.push(pref(id, category, name, v));
            }
        }
        let email_off = saved.notify_props.0.get("email").map(String::as_str) == Some("false");
        if d.email_interval.is_some() || email_off {
            let secs = if email_off {
                "0"
            } else {
                match d.email_interval.as_deref() {
                    Some("immediately") => "30",
                    Some("fifteen") => "900",
                    Some("hour") => "3600",
                    _ => "",
                }
            };
            if !secs.is_empty() {
                prefs.push(pref(id, "notifications", "email_interval", secs));
            }
        }
        if !prefs.is_empty() {
            self.save_preferences(&mut prefs)
                .await
                .map_err(|e| IErr::new("BulkImport", "app.import.import_user.save_preferences.error", 500).wrap(e))?;
        }
        self.import_user_teams(&saved, d.teams.as_deref()).await
    }

    /// `App.UpdatePassword`.
    async fn update_password(&self, user: &User, password: &str) -> IResult<()> {
        crate::user_edit::check_password(&self.app, password)?;
        if user.remote_id.as_deref().is_some_and(|r| !r.is_empty()) {
            return Err(IErr::new("UpdatePassword", "api.user.update_password.failed.app_error", 500));
        }
        let now = now_ms();
        sqlx::query(
            "UPDATE Users SET Password = $1, LastPasswordUpdate = $2, UpdateAt = $2, AuthData = NULL, AuthService = '',
                 FailedAttempts = 0 WHERE Id = $3",
        )
        .bind(crate::password::hash(password).await)
        .bind(now)
        .bind(&user.id)
        .execute(&self.app.db)
        .await
        .map_err(|e| IErr::new("UpdatePassword", "api.user.update_password.failed.app_error", 500).wrap(e))?;
        self.app.invalidate_user(&user.id);
        self.app.revoke_all_sessions(&user.id).await?;
        Ok(())
    }

    /// `importProfileImage`.
    async fn import_profile_image(&self, user_id: &str, path: &str, data: &Option<ZipRef>) -> IResult<()> {
        let max = self.app.cfg.i64("FileSettings.MaxFileSize").max(0) as u64;
        let mut bytes = match data {
            Some(z) => read_zip_file(&self.archive, &z.name, Some(max)).await.map_err(|e| {
                IErr::new("importProfileImage", "app.import.profile_image.open.app_error", 500)
                    .p("FileName", &z.name)
                    .wrap(e)
            })?,
            None => std::fs::read(path)
                .map_err(|e| IErr::new("importProfileImage", "app.import.profile_image.open.app_error", 500).wrap(e))?,
        };
        bytes.truncate(max as usize);
        self.app.set_profile_image_from_bytes(user_id, &bytes).await.map_err(|e| {
            if e.id == "api.user.upload_profile_user.decode.app_error"
                || e.id == "api.user.upload_profile_user.check_image_limits.app_error"
            {
                IErr::new("SetProfileImage", "api.user.upload_profile_user.check_image_limits.app_error", 400)
                    .wrap(e.detailed_error)
            } else {
                IErr::from(e)
            }
        })
    }

    /// `UserIsInAdminRoleGroup`: a member of a group synced to the team as admins.
    async fn in_admin_role_group(&self, user_id: &str, team_id: &str) -> IResult<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM GroupMembers gm
                 JOIN GroupTeams gt ON gt.GroupId = gm.GroupId AND gt.TeamId = $2 AND gt.DeleteAt = 0 AND gt.SchemeAdmin
                 JOIN UserGroups ug ON ug.Id = gm.GroupId AND ug.DeleteAt = 0
                 WHERE gm.UserId = $1 AND gm.DeleteAt = 0)",
        )
        .bind(user_id)
        .bind(team_id)
        .fetch_one(&self.app.db)
        .await?)
    }

    async fn import_user_teams(&self, user: &User, data: Option<&[UserTeamData]>) -> IResult<()> {
        let Some(data) = data else { return Ok(()) };
        let app = &self.app;
        let names: Vec<String> = data.iter().filter_map(|t| t.name.clone()).collect();
        let all_teams = self.teams_by_names(&names).await?;
        let guest = user.is_guest();
        let existing: HashSet<String> = sqlx::query_scalar("SELECT TeamId FROM TeamMembers WHERE UserId = $1")
            .bind(&user.id)
            .fetch_all(&app.db)
            .await
            .map_err(|e| IErr::new("importUserTeams", "app.team.get_members.app_error", 500).wrap(e))?
            .into_iter()
            .collect();
        let mut theme_prefs: HashMap<String, Vec<Preference>> = HashMap::new();
        let mut channels: HashMap<String, Vec<UserChannelData>> = HashMap::new();
        let mut roles_by_team: HashMap<String, String> = HashMap::new();
        let mut flags: HashMap<String, (bool, bool, bool)> = HashMap::new();
        let (mut new_members, mut old_members): (Vec<TeamMember>, Vec<TeamMember>) = (vec![], vec![]);
        for t in data {
            let name = t.name.as_deref().unwrap_or_default().to_lowercase();
            let Some(team) = all_teams.get(&name) else {
                return Err(IErr::bad("app.import.get_teams_by_names.some_teams_not_found.error"));
            };
            if let Some(theme) = &t.theme {
                theme_prefs.entry(team.id.clone()).or_default().push(pref(&user.id, "theme", &team.id, theme));
            }
            let (mut g, mut u, mut a) = (false, true, false);
            if let Some(raw) = &t.roles {
                let mut explicit = vec![];
                for r in raw.split_whitespace() {
                    match r {
                        "team_guest" => {
                            g = true;
                            u = false;
                        }
                        "team_user" => u = true,
                        "team_admin" => a = true,
                        r => explicit.push(r),
                    }
                }
                roles_by_team.insert(team.id.clone(), explicit.join(" "));
            }
            flags.insert(team.id.clone(), (g, u, a));
            let mut member = TeamMember {
                team_id: team.id.clone(),
                user_id: user.id.clone(),
                scheme_guest: guest,
                scheme_user: !guest,
                scheme_admin: team.email == user.email && !guest,
                create_at: now_ms(),
                ..Default::default()
            };
            if !guest {
                member.scheme_admin = member.scheme_admin || self.in_admin_role_group(&user.id, &team.id).await?;
            }
            let list = channels.entry(team.id.clone()).or_default();
            list.extend(t.channels.iter().flatten().cloned());
            if !guest {
                list.push(UserChannelData { name: Some("town-square".into()), ..Default::default() });
            }
            if existing.contains(&team.id) {
                old_members.push(member);
            } else {
                new_members.push(member);
            }
        }

        // UpdateMultipleMembers
        for m in &old_members {
            sqlx::query(
                "UPDATE TeamMembers SET Roles = $3, DeleteAt = $4, CreateAt = $5, SchemeGuest = $6, SchemeUser = $7,
                     SchemeAdmin = $8
                 WHERE TeamId = $1 AND UserId = $2",
            )
            .bind(&m.team_id)
            .bind(&m.user_id)
            .bind(&m.explicit_roles)
            .bind(m.delete_at)
            .bind(m.create_at)
            .bind(m.scheme_guest)
            .bind(m.scheme_user)
            .bind(m.scheme_admin)
            .execute(&app.db)
            .await
            .map_err(|e| IErr::new("importUserTeams", "app.team.save_member.save.app_error", 500).wrap(e))?;
        }
        // SaveMultipleMembers
        if !new_members.is_empty() {
            let max = app.cfg.i64("TeamSettings.MaxUsersPerTeam");
            if max >= 0 {
                let mut per_team: HashMap<&str, i64> = HashMap::new();
                for m in &new_members {
                    *per_team.entry(m.team_id.as_str()).or_default() += 1;
                }
                for (team_id, n) in per_team {
                    let count: i64 = sqlx::query_scalar(
                        "SELECT COUNT(0) FROM TeamMembers JOIN Users ON TeamMembers.UserId = Users.Id
                         WHERE TeamMembers.TeamId = $1 AND TeamMembers.DeleteAt = 0 AND Users.DeleteAt = 0",
                    )
                    .bind(team_id)
                    .fetch_one(&app.db)
                    .await
                    .map_err(|e| {
                        IErr::new("BulkImport", "app.import.import_user_teams.save_members.error", 500).wrap(e)
                    })?;
                    if count + n > max {
                        return Err(IErr::bad("app.import.import_user_teams.save_members.max_accounts.app_error")
                            .wrap(format!(
                                "limit exceeded: what: TeamMember count: {} metadata: team members limit exceeded",
                                count + n
                            )));
                    }
                }
            }
            let mut tx = app.db.begin().await?;
            for m in &new_members {
                if let Err(e) = db::insert(&mut *tx, m).await {
                    if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
                        return Err(IErr::bad("app.import.import_user_teams.save_members.conflict.app_error").wrap(e));
                    }
                    return Err(IErr::new("BulkImport", "app.import.import_user_teams.save_members.error", 500).wrap(e));
                }
            }
            tx.commit().await?;
        }
        app.invalidate_user_teams(&user.id).await;

        for m in new_members.iter().chain(old_members.iter()) {
            let roles = roles_by_team.get(&m.team_id).cloned().unwrap_or_default();
            if !roles.is_empty() {
                self.update_team_member_roles(&m.team_id, &user.id, &roles).await?;
            }
            let (g, u, a) = flags.get(&m.team_id).copied().unwrap_or((false, true, false));
            if let Err(e) = self.update_team_member_scheme_roles(&m.team_id, &user.id, g, u, m.scheme_admin || a).await
            {
                warn(&format!(
                    "Error updating team member scheme roles team_id={} user_id={} err={}",
                    m.team_id,
                    user.id,
                    e.text()
                ));
            }
        }

        for team in all_teams.values() {
            if let Some(p) = theme_prefs.get_mut(&team.id) {
                self.save_preferences(p).await.map_err(|e| {
                    IErr::new("BulkImport", "app.import.import_user_teams.save_preferences.error", 500).wrap(e)
                })?;
            }
            let list = channels.remove(&team.id).unwrap_or_default();
            self.import_user_channels(user, team, &list).await?;
        }
        Ok(())
    }

    async fn team_member_row(&self, team_id: &str, user_id: &str) -> IResult<Option<TeamMember>> {
        let sql = format!("{} WHERE TeamId = $1 AND UserId = $2", TeamMember::SELECT);
        let m: Option<TeamMember> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(team_id).bind(user_id).fetch_optional(&self.app.db).await?;
        // The stored roles are the explicit ones.
        Ok(m.map(|mut m| {
            m.explicit_roles = m.roles.clone();
            m
        }))
    }

    async fn write_team_member(&self, m: &TeamMember) -> IResult<()> {
        sqlx::query(
            "UPDATE TeamMembers SET Roles = $3, DeleteAt = $4, CreateAt = $5, SchemeGuest = $6, SchemeUser = $7, SchemeAdmin = $8
             WHERE TeamId = $1 AND UserId = $2",
        )
        .bind(&m.team_id)
        .bind(&m.user_id)
        .bind(&m.explicit_roles)
        .bind(m.delete_at)
        .bind(m.create_at)
        .bind(m.scheme_guest)
        .bind(m.scheme_user)
        .bind(m.scheme_admin)
        .execute(&self.app.db)
        .await
        .map_err(|e| IErr::new("UpdateTeamMemberRoles", "app.team.save_member.save.app_error", 500).wrap(e))?;
        self.app.clear_sessions_for_user(&m.user_id);
        // sendUpdatedTeamMemberEvent: both role updates of the import tell the member.
        if let Ok(Some(tm)) = self.app.team_member(&m.team_id, &m.user_id).await {
            self.app.hub.publish(
                "memberrole_updated",
                serde_json::json!({"member": serde_json::to_string(&tm).unwrap_or_default()}),
                crate::hub::Broadcast { user_id: m.user_id.clone(), ..Default::default() },
            );
        }
        Ok(())
    }

    /// `updateTeamMemberRolesInternal` with the scheme user role allowed to be unset (bulk import).
    async fn update_team_member_roles(&self, team_id: &str, user_id: &str, new_roles: &str) -> IResult<()> {
        let mut m = self
            .team_member_row(team_id, user_id)
            .await?
            .ok_or_else(|| IErr::new("UpdateTeamMemberRoles", "app.team.get_member.missing.app_error", 404))?;
        let [guest_role, user_role, admin_role] = self.app.scheme_roles_for_team(team_id).await?;
        let prev_guest = m.scheme_guest;
        let (mut g, mut u, mut a) = (false, false, false);
        let mut explicit = vec![];
        for name in new_roles.split_whitespace() {
            let role = self.role_by_name(name).await?;
            if !role.scheme_managed {
                explicit.push(name);
            } else if name == admin_role {
                a = true;
            } else if name == user_role {
                u = true;
            } else if name == guest_role {
                g = true;
            } else {
                return Err(IErr::new(
                    "UpdateTeamMemberRoles",
                    "api.channel.update_team_member_roles.scheme_role.app_error",
                    400,
                )
                .detail(format!("role_name={name}")));
            }
        }
        if g && u {
            return Err(IErr::new(
                "UpdateTeamMemberRoles",
                "api.team.update_team_member_roles.guest_and_user.app_error",
                400,
            ));
        }
        if g && a {
            return Err(IErr::new(
                "UpdateTeamMemberRoles",
                "api.team.update_team_member_roles.guest_and_admin.app_error",
                400,
            ));
        }
        if prev_guest != g {
            return Err(IErr::new(
                "UpdateTeamMemberRoles",
                "api.channel.update_team_member_roles.changing_guest_role.app_error",
                400,
            ));
        }
        (m.scheme_guest, m.scheme_user, m.scheme_admin) = (g, u, a);
        m.explicit_roles = explicit.join(" ");
        self.write_team_member(&m).await
    }

    /// `UpdateTeamMemberSchemeRoles`.
    async fn update_team_member_scheme_roles(
        &self,
        team_id: &str,
        user_id: &str,
        g: bool,
        u: bool,
        a: bool,
    ) -> IResult<()> {
        let mut m = self
            .team_member_row(team_id, user_id)
            .await?
            .ok_or_else(|| IErr::new("GetTeamMember", "app.team.get_member.missing.app_error", 404))?;
        let e = |id: &str| Err(IErr::new("UpdateTeamMemberSchemeRoles", id, 400));
        if m.scheme_guest {
            return e("api.team.update_team_member_roles.guest.app_error");
        }
        if g {
            return e("api.team.update_team_member_roles.user_and_guest.app_error");
        }
        if !u {
            return e("api.team.update_team_member_roles.unset_user_scheme.app_error");
        }
        (m.scheme_guest, m.scheme_user, m.scheme_admin) = (g, u, a);
        self.write_team_member(&m).await
    }

    /// `GetRoleByName` (errors made 400, as the member role updates do).
    async fn role_by_name(&self, name: &str) -> IResult<Role> {
        match self.app.roles(&[name]).await?.into_iter().next() {
            Some(r) => Ok((*r).clone()),
            None => Err(IErr::new("GetRoleByName", "app.role.get_by_name.app_error", 400)
                .wrap(not_found("Role", &format!("name={name}")))),
        }
    }

    async fn import_user_channels(&self, user: &User, team: &Team, data: &[UserChannelData]) -> IResult<()> {
        let app = &self.app;
        let names: Vec<String> = data.iter().filter_map(|c| c.name.clone()).collect();
        let all = self.channels_by_names(&names, &team.id).await?;
        let guest = user.is_guest();
        let existing: HashSet<String> = sqlx::query_scalar(
            "SELECT cm.ChannelId FROM ChannelMembers cm JOIN Channels c ON c.Id = cm.ChannelId
             WHERE cm.UserId = $1 AND (c.TeamId = $2 OR c.TeamId = '' OR c.TeamId IS NULL)",
        )
        .bind(&user.id)
        .bind(&team.id)
        .fetch_all(&app.db)
        .await
        .map_err(|e| IErr::new("importUserChannels", "app.channel.get_members.app_error", 500).wrap(e))?
        .into_iter()
        .collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut roles_by_channel: HashMap<String, String> = HashMap::new();
        let mut flags: HashMap<String, (bool, bool, bool)> = HashMap::new();
        let mut favorites: HashMap<String, Vec<Preference>> = HashMap::new();
        let (mut new_members, mut old_members): (Vec<ChannelMember>, Vec<ChannelMember>) = (vec![], vec![]);
        for c in data {
            let name = c.name.clone().unwrap_or_default();
            let Some(channel) = all.get(&name.to_lowercase()) else {
                if self.deactivate_missing_users {
                    warn(&format!(
                        "Skipping channel membership for a channel not found on destination during scoped import channel={name} user={}",
                        user.username
                    ));
                    continue;
                }
                return Err(IErr::new("BulkImport", "app.import.import_user_channels.channel_not_found.error", 500));
            };
            if seen.contains(&channel.id) && name == "town-square" {
                continue;
            }
            let (mut g, mut u, mut a) = (false, true, false);
            if let Some(raw) = &c.roles {
                let mut explicit = vec![];
                for r in raw.split_whitespace() {
                    match r {
                        "channel_guest" => {
                            g = true;
                            u = false;
                        }
                        "channel_user" => u = true,
                        "channel_admin" => a = true,
                        r => explicit.push(r),
                    }
                }
                roles_by_channel.insert(channel.id.clone(), explicit.join(" "));
            }
            flags.insert(channel.id.clone(), (g, u, a));
            if c.favorite == Some(true) {
                favorites.entry(channel.id.clone()).or_default().push(pref(
                    &user.id,
                    "favorite_channel",
                    &channel.id,
                    "true",
                ));
            }
            let mut m = ChannelMember {
                channel_id: channel.id.clone(),
                user_id: user.id.clone(),
                notify_props: crate::model::channel::default_member_notify_props(),
                scheme_guest: guest,
                scheme_user: !guest,
                ..Default::default()
            };
            if !guest {
                m.scheme_admin = self.in_admin_role_group(&user.id, &team.id).await?;
            }
            if let (Some(mc), Some(mcr)) = (c.mention_count, c.mention_count_root) {
                m.mention_count = mc;
                m.mention_count_root = mcr;
            }
            if let Some(v) = c.urgent_mention_count {
                m.urgent_mention_count = v;
            }
            if let (Some(mc), Some(mcr)) = (c.msg_count, c.msg_count_root) {
                m.msg_count = mc;
                m.msg_count_root = mcr;
            }
            if let Some(v) = c.last_viewed_at {
                m.last_viewed_at = v;
            }
            if let Some(np) = &c.notify_props {
                for (v, key) in [(&np.desktop, "desktop"), (&np.mobile, "push"), (&np.mark_unread, "mark_unread")] {
                    if let Some(v) = v {
                        m.notify_props.0.insert(key.into(), v.clone());
                    }
                }
            }
            seen.insert(channel.id.clone());
            if existing.contains(&channel.id) {
                old_members.push(m);
            } else {
                new_members.push(m);
            }
        }
        for m in &old_members {
            app.update_member(m).await.map_err(|e| {
                if e.status_code == 404 {
                    IErr::new("importUserChannels", "app.channel.get_member.missing.app_error", 404).wrap(e.message)
                } else {
                    IErr::new("importUserChannels", "app.channel.get_member.app_error", 500).wrap(e.message)
                }
            })?;
        }
        for m in &new_members {
            app.save_member(m.clone()).await.map_err(|e| {
                if e.id == "app.channel.save_member.exists.app_error" {
                    IErr::new("importUserChannels", "app.channel.save_member.exists.app_error", 400)
                } else {
                    IErr::new("importUserChannels", "app.channel.create_direct_channel.internal_error", 500)
                        .wrap(e.detailed_error)
                }
            })?;
        }
        app.invalidate_channel_cache_for_user(&user.id);
        for m in new_members.iter().chain(old_members.iter()) {
            app.invalidate_channel_members_cache(&m.channel_id);
            app.caches.member_count.remove(&m.channel_id);
            app.caches.members_notify_props.remove(&m.channel_id);
        }
        for m in new_members.iter().chain(old_members.iter()) {
            let roles = roles_by_channel.get(&m.channel_id).cloned().unwrap_or_default();
            if !roles.is_empty() {
                self.update_channel_member_roles(&m.channel_id, &user.id, &roles).await?;
            }
            let (g, u, a) = flags.get(&m.channel_id).copied().unwrap_or((false, true, false));
            if let Err(e) = app.update_member_scheme_roles(&m.channel_id, &user.id, g, u, a).await {
                warn(&format!(
                    "Error updating channel member scheme roles channel_id={} user_id={} err={}",
                    m.channel_id, user.id, e.message
                ));
            }
        }
        for c in all.values() {
            if let Some(p) = favorites.get_mut(&c.id) {
                self.save_preferences(p).await.map_err(|e| {
                    IErr::new("BulkImport", "app.import.import_user_channels.save_preferences.error", 500).wrap(e)
                })?;
            }
        }
        Ok(())
    }

    /// `updateChannelMemberRolesInternal` with the scheme user role allowed to be unset (bulk import).
    async fn update_channel_member_roles(&self, channel_id: &str, user_id: &str, new_roles: &str) -> IResult<()> {
        let mut m = self.app.channel_member_or_404(channel_id, user_id).await?;
        let [guest_role, user_role, admin_role] = self.app.scheme_roles_for_channel(channel_id).await?;
        let prev_guest = m.scheme_guest;
        let (mut g, mut u, mut a) = (false, false, false);
        let mut explicit = vec![];
        let scheme_role = |name: &str| {
            Err(IErr::new(
                "UpdateChannelMemberRoles",
                "api.channel.update_channel_member_roles.scheme_role.app_error",
                400,
            )
            .detail(format!("role_name={name}")))
        };
        for name in new_roles.split_whitespace() {
            let role = self.role_by_name(name).await?;
            if !role.scheme_managed {
                if crate::schemes::is_builtin(name)
                    && !["channel_guest", "channel_user", "channel_admin"].contains(&name)
                {
                    return scheme_role(name);
                }
                explicit.push(name);
            } else if name == admin_role {
                a = true;
            } else if name == user_role {
                u = true;
            } else if name == guest_role {
                g = true;
            } else {
                return scheme_role(name);
            }
        }
        let e = |id: &str| Err(IErr::new("UpdateChannelMemberRoles", id, 400));
        if u && g {
            return e("api.channel.update_channel_member_roles.guest_and_user.app_error");
        }
        if g && a {
            return e("api.channel.update_channel_member_roles.guest_and_admin.app_error");
        }
        if prev_guest != g {
            return e("api.channel.update_channel_member_roles.changing_guest_role.app_error");
        }
        (m.scheme_guest, m.scheme_user, m.scheme_admin) = (g, u, a);
        m.explicit_roles = explicit.join(" ");
        self.app.update_channel_member(&m).await?;
        Ok(())
    }

    // ---------- bots, emoji ----------

    async fn import_bot(&self, d: &BotData) -> IResult<()> {
        let app = &self.app;
        info(&format!("Validating bot user_name={}", d.username.as_deref().unwrap_or_default()));
        validate_bot(d)?;
        if self.dry_run {
            return Ok(());
        }
        let username = d.username.clone().unwrap_or_default();
        info(&format!("Importing bot user_name={username}"));
        let existing_id: Option<String> = sqlx::query_scalar(
            "SELECT b.UserId FROM Bots b JOIN Users u ON u.Id = b.UserId WHERE u.Username = lower($1)",
        )
        .bind(&username)
        .fetch_optional(&app.db)
        .await
        .map_err(|e| IErr::new("importBot", "app.import.import_bot.lookup_error", 500).wrap(e))?;
        let mut changed = false;
        let mut bot = match &existing_id {
            Some(id) => app.bot(id, true).await?,
            None => {
                changed = true;
                Bot::default()
            }
        };
        bot.username = username.clone();
        if let Some(v) = &d.description
            && bot.description != *v
        {
            bot.description = v.clone();
            changed = true;
        }
        if let Some(v) = &d.display_name
            && bot.display_name != *v
        {
            bot.display_name = v.clone();
            changed = true;
        }
        if let Some(owner) = &d.owner {
            match self.user_by_username(owner).await {
                Ok(Some(u)) => bot.owner_id = u.id,
                Ok(None) => {
                    warn(&format!(
                        "Bot owner did not resolve to a user during import; keeping the value as the owner id bot_username={username} owner={owner}"
                    ));
                    bot.owner_id = owner.clone();
                }
                Err(e) => {
                    return Err(IErr::new("importBot", "app.import.import_bot.owner_could_not_found.error", 500)
                        .p("Owner", owner)
                        .wrap(e.text()));
                }
            }
        }
        let saved = if bot.user_id.is_empty() {
            match app.create_bot(&Session::default(), bot.clone()).await {
                Ok(b) => b,
                Err(e) if e.id == "app.user.save.username_exists.app_error" => {
                    info(&format!(
                        "CreateBot failed with username conflict during import, recovering by linking existing user bot_username={username}"
                    ));
                    let Some(existing) = self.user_by_username(&username).await? else {
                        return Err(IErr::new("importBot", "app.import.import_bot.user_not_found.error", 500)
                            .wrap(not_found("User", &format!("username={username}"))));
                    };
                    app.revoke_all_sessions(&existing.id).await?;
                    app.invalidate_user(&existing.id);
                    bot.user_id = existing.id.clone();
                    match app.save_bot(&existing.id, &username, &bot.owner_id, &bot.description).await {
                        Ok(b) => b,
                        Err(save_err) => {
                            warn(&format!(
                                "Bot record save failed during import recovery, attempting update user_id={} err={}",
                                existing.id, save_err.detailed_error
                            ));
                            app.store_bot(&bot).await.map_err(|e| {
                                IErr::new("importBot", "app.bot.update.internal_error", 500).wrap(e.detailed_error)
                            })?
                        }
                    }
                }
                Err(e) => return Err(e.into()),
            }
        } else if changed {
            app.store_bot(&bot)
                .await
                .map_err(|e| IErr::new("importBot", "app.bot.update.app_error", 500).wrap(e.detailed_error))?
        } else {
            bot
        };
        // The display name is the bot user's first name.
        if let Some(dn) = &d.display_name
            && !saved.user_id.is_empty()
        {
            match app.user(&saved.user_id).await {
                Ok(Some(u)) if u.first_name != *dn => {
                    let mut u = (*u).clone();
                    u.first_name = dn.clone();
                    app.update_user(u, false, false).await?;
                }
                Ok(_) => {}
                Err(e) => warn(&format!(
                    "Failed to fetch bot user for DisplayName update user_id={} err={}",
                    saved.user_id, e.detailed_error
                )),
            }
        }
        if let Some(path) = &d.profile_image {
            self.import_profile_image(&saved.user_id, path, &d.profile_image_data).await?;
        }
        Ok(())
    }

    async fn import_emoji(&self, d: &EmojiData) -> IResult<()> {
        let app = &self.app;
        info(&format!("Validating emoji emoji_name={}", d.name.as_deref().unwrap_or_default()));
        if let Err(e) = validate_emoji(d) {
            if e.id() == "model.emoji.system_emoji_name.app_error" {
                warn(&format!(
                    "Skipping emoji import due to name conflict with system emoji emoji_name={}",
                    d.name.as_deref().unwrap_or_default()
                ));
                return Ok(());
            }
            return Err(e);
        }
        if self.dry_run {
            return Ok(());
        }
        let name = d.name.clone().unwrap_or_default();
        info(&format!("Importing emoji emoji_name={name}"));
        let existing: Option<Emoji> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("{} WHERE Name = $1 AND DeleteAt = 0", Emoji::SELECT)))
                .bind(&name)
                .fetch_optional(&app.db)
                .await
                .map_err(|e| IErr::new("importEmoji", "app.emoji.get_by_name.app_error", 500).wrap(e))?;
        let exists = existing.is_some();
        let emoji = existing.unwrap_or_else(|| {
            let now = now_ms();
            Emoji { id: new_id(), name: name.clone(), create_at: now, update_at: now, ..Default::default() }
        });
        let bad_file = |e: String| IErr::bad("app.import.emoji.bad_file.error").p("EmojiName", &name).wrap(e);
        let bytes = match &d.data {
            Some(z) => read_zip_file(&self.archive, &z.name, Some(MAX_EMOJI_FILE_SIZE)).await.map_err(bad_file)?,
            None => std::fs::read(d.image.as_deref().unwrap_or_default()).map_err(|e| bad_file(e.to_string()))?,
        };
        if bytes.len() as u64 > MAX_EMOJI_FILE_SIZE {
            return Err(IErr::new("WriteFile", "api.file.write_file.app_error", 500)
                .wrap("unable write the data in the file: read limit exceeded"));
        }
        app.files
            .write(&format!("emoji/{}/image", emoji.id), bytes.into())
            .await
            .map_err(|e| IErr::new("WriteFile", "api.file.write_file.app_error", 500).wrap(e))?;
        if !exists {
            db::insert(&app.db, &emoji)
                .await
                .map_err(|e| IErr::new("importEmoji", "api.emoji.create.internal_error", 400).wrap(e))?;
        }
        app.caches.emoji_by_name.remove(emoji.name.as_str());
        app.caches.emoji_by_id.remove(emoji.id.as_str());
        Ok(())
    }

    // ---------- direct channels ----------

    /// `createDirectChannel` without its events: the channel and both memberships, or the one there is.
    async fn create_direct_channel(&self, a: &str, b: &str) -> IResult<Channel> {
        let app = &self.app;
        let users = app.users(&[a.to_string(), b.to_string()]).await?;
        let invalid = || {
            IErr::new("CreateDirectChannel", "api.channel.create_direct_channel.invalid_user.app_error", 400)
                .detail(format!("No users found for ids: {a}. {b}"))
        };
        let user = users.iter().find(|u| u.id == a).ok_or_else(invalid)?.clone();
        let other = users.iter().find(|u| u.id == b).ok_or_else(invalid)?.clone();
        let remote = |u: &User| u.remote_id.as_deref().is_some_and(|r| !r.is_empty());
        let mut c = Channel {
            name: crate::model::channel::direct_name(&other.id, &user.id),
            r#type: "D".into(),
            shared: Some(remote(&user) || remote(&other)),
            creator_id: user.id.clone(),
            ..Default::default()
        };
        if let Err(existing) = app.save_channel(&mut c, false).await? {
            return Ok(existing);
        }
        let mut pair = vec![user.clone()];
        if other.id != user.id {
            pair.push(other.clone());
        }
        for u in &pair {
            app.save_member(ChannelMember {
                channel_id: c.id.clone(),
                user_id: u.id.clone(),
                scheme_guest: u.is_guest(),
                scheme_user: !u.is_guest(),
                notify_props: crate::model::channel::default_member_notify_props(),
                ..Default::default()
            })
            .await?;
        }
        for u in &pair {
            app.log_join(&u.id, &c.id).await.map_err(|e| {
                IErr::new(
                    "createDirectChannelWithUser",
                    "app.channel_member_history.log_join_event.internal_error",
                    500,
                )
                .wrap(e.detailed_error)
            })?;
        }
        Ok(c)
    }

    /// `createGroupChannel` without its events: the channel and the memberships, or the one there is.
    async fn create_group_channel(&self, ids: &[String]) -> IResult<Channel> {
        let app = &self.app;
        if ids.len() > GROUP_MAX_USERS || ids.len() < GROUP_MIN_USERS {
            return Err(IErr::new("CreateGroupChannel", "api.channel.create_group.bad_size.app_error", 400));
        }
        let users = crate::users::load_users(app, "u.id = ANY($1)", ids).await?;
        if users.len() != ids.len() {
            return Err(IErr::new("CreateGroupChannel", "api.channel.create_group.bad_user.app_error", 400)
                .detail(format!("user_ids={}", serde_json::to_string(ids).unwrap_or_default())));
        }
        let mut names: Vec<&str> = users.iter().map(|u| u.username.as_str()).collect();
        names.sort();
        let mut display = names.join(", ");
        if display.len() > CHANNEL_NAME_MAX {
            let mut end = CHANNEL_NAME_MAX;
            while !display.is_char_boundary(end) {
                end -= 1;
            }
            display.truncate(end);
        }
        let shared = users.iter().any(|u| u.remote_id.as_deref().is_some_and(|r| !r.is_empty()));
        let mut c = Channel {
            name: crate::channels::group_name(ids),
            display_name: display,
            r#type: "G".into(),
            shared: Some(shared),
            ..Default::default()
        };
        if let Err(existing) = app.save_channel(&mut c, true).await? {
            return Ok(existing);
        }
        for u in &users {
            app.save_member(ChannelMember {
                channel_id: c.id.clone(),
                user_id: u.id.clone(),
                scheme_guest: u.is_guest(),
                scheme_user: !u.is_guest(),
                notify_props: crate::model::channel::default_member_notify_props(),
                ..Default::default()
            })
            .await?;
            app.log_join(&u.id, &c.id).await.map_err(|e| {
                IErr::new("createGroupChannel", "app.channel_member_history.log_join_event.internal_error", 500)
                    .wrap(e.detailed_error)
            })?;
        }
        Ok(c)
    }

    async fn import_direct_channel(&self, d: &DirectChannelData) -> IResult<()> {
        let app = &self.app;
        validate_direct_channel(d)?;
        if self.dry_run {
            return Ok(());
        }
        let members: Vec<String> = match (&d.participants, &d.members) {
            (Some(p), _) => p.iter().map(|m| m.username.clone().unwrap_or_default()).collect(),
            (None, Some(m)) => m.clone(),
            (None, None) => return Err(IErr::bad("app.import.import_direct_channel.no_members.error")),
        };
        let user_map = self.users_by_usernames(&members).await?;
        let mut ids = vec![];
        for name in &members {
            match user_map.get(&name.to_lowercase()) {
                Some(u) => ids.push(u.id.clone()),
                None => warn(&format!(
                    "Skipping direct channel member not found on destination during scoped import username={name}"
                )),
            }
        }
        let channel = match ids.len() {
            2 => self.create_direct_channel(&ids[0], &ids[1]).await.map_err(|e| {
                IErr::bad("app.import.import_direct_channel.create_direct_channel.error").wrap(e.text())
            })?,
            n if n > 2 => self
                .create_group_channel(&ids)
                .await
                .map_err(|e| IErr::bad("app.import.import_direct_channel.create_group_channel.error").wrap(e.text()))?,
            _ => {
                warn("Not enough users to create a direct channel during scoped import");
                return Ok(());
            }
        };
        // The memberships there are (the first page of 100 holds them all, when anyone active is a member).
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ChannelMembers cm JOIN Users u ON u.Id = cm.UserId WHERE cm.ChannelId = $1 AND u.DeleteAt = 0",
        )
        .bind(&channel.id)
        .fetch_one(&app.db)
        .await
        .map_err(|e| IErr::bad("app.import.import_direct_channel.get_channel_members.error").wrap(e))?;
        let existing: HashMap<String, i64> = if active > 0 {
            sqlx::query_as::<_, (String, i64)>(
                "SELECT UserId, COALESCE(LastViewedAt, 0) FROM ChannelMembers WHERE ChannelId = $1 LIMIT 100",
            )
            .bind(&channel.id)
            .fetch_all(&app.db)
            .await
            .map_err(|e| IErr::bad("app.import.import_direct_channel.get_channel_members.error").wrap(e))?
            .into_iter()
            .collect()
        } else {
            HashMap::new()
        };
        let participants = d.participants.as_deref().unwrap_or_default();
        let candidates: Vec<String> = participants
            .iter()
            .filter_map(|m| user_map.get(&m.username.as_deref().unwrap_or_default().to_lowercase()))
            .filter(|u| !existing.contains_key(&u.id))
            .map(|u| u.id.clone())
            .collect();
        let left: HashSet<String> = if candidates.is_empty() {
            HashSet::new()
        } else {
            sqlx::query_scalar(
                "SELECT DISTINCT UserId FROM ChannelMemberHistory WHERE ChannelId = $1 AND UserId = ANY($2)",
            )
            .bind(&channel.id)
            .bind(&candidates)
            .fetch_all(&app.db)
            .await
            .map_err(|e| {
                IErr::new("BulkImport", "app.import.import_direct_channel.get_ever_members.error", 500).wrap(e)
            })?
            .into_iter()
            .collect()
        };
        let mut updates = vec![];
        for p in participants {
            let Some(u) = user_map.get(&p.username.as_deref().unwrap_or_default().to_lowercase()) else { continue };
            let mut m = ChannelMember {
                notify_props: crate::model::channel::default_member_notify_props(),
                scheme_guest: u.is_guest(),
                scheme_user: !u.is_guest(),
                ..Default::default()
            };
            if let Some(v) = p.last_viewed_at {
                m.last_viewed_at = v;
            }
            if let Some(v) = p.msg_count {
                m.msg_count = v;
            }
            if let Some(v) = p.mention_count {
                m.mention_count = v;
            }
            if let Some(v) = p.mention_count_root {
                m.mention_count_root = v;
            }
            if let Some(v) = p.urgent_mention_count {
                m.urgent_mention_count = v;
            }
            if let Some(v) = p.msg_count_root {
                m.msg_count_root = v;
            }
            if let Some(v) = p.scheme_user {
                m.scheme_user = v;
            }
            if let Some(v) = p.scheme_admin {
                m.scheme_admin = v;
            }
            if let Some(v) = p.scheme_guest {
                m.scheme_guest = v;
            }
            if let Some(np) = &p.notify_props {
                for (v, key) in [
                    (&np.desktop, "desktop"),
                    (&np.mark_unread, "mark_unread"),
                    (&np.mobile, "push"),
                    (&np.email, "email"),
                    (&np.ignore_channel_mentions, "ignore_channel_mentions"),
                    (&np.channel_auto_follow_threads, "channel_auto_follow_threads"),
                ] {
                    if let Some(v) = v {
                        m.notify_props.0.insert(key.into(), v.clone());
                    }
                }
            }
            match existing.get(&u.id) {
                Some(last_viewed) if *last_viewed > m.last_viewed_at => continue,
                Some(_) => {}
                None => {
                    if left.contains(&u.id) {
                        continue;
                    }
                    let row = ChannelMember {
                        user_id: u.id.clone(),
                        channel_id: channel.id.clone(),
                        notify_props: crate::model::channel::default_member_notify_props(),
                        scheme_user: !u.is_guest(),
                        scheme_guest: u.is_guest(),
                        ..Default::default()
                    };
                    match app.save_member(row).await {
                        Ok(_) => {
                            if let Err(e) = app.log_join(&u.id, &channel.id).await {
                                warn(&format!(
                                    "Failed to log channel member history join event during import err={}",
                                    e.detailed_error
                                ));
                            }
                        }
                        Err(e) if e.id == "app.channel.save_member.exists.app_error" => {}
                        Err(e) => {
                            return Err(IErr::new(
                                "BulkImport",
                                "app.import.import_direct_channel.save_member.error",
                                500,
                            )
                            .wrap(e.detailed_error));
                        }
                    }
                }
            }
            m.user_id = u.id.clone();
            m.channel_id = channel.id.clone();
            updates.push(m);
        }
        for m in &updates {
            app.update_member(m).await.map_err(|e| {
                IErr::bad("app.import.import_direct_channel.create_group_channel.error").wrap(e.message)
            })?;
        }
        if !updates.is_empty() {
            app.invalidate_channel_members_cache(&channel.id);
            app.caches.members_notify_props.remove(&channel.id);
            for m in &updates {
                app.invalidate_channel_cache_for_user(&m.user_id);
            }
        }

        let mut prefs = vec![];
        for name in d.shown_by.iter().flatten() {
            let Some(shower) = user_map.get(&name.to_lowercase()) else { continue };
            match channel.r#type.as_str() {
                "D" => {
                    let other = user_map
                        .iter()
                        .find(|(k, _)| *k != name)
                        .map(|(_, u)| u.id.clone())
                        .unwrap_or_else(|| shower.id.clone());
                    prefs.push(pref(&shower.id, "direct_channel_show", &other, "true"));
                }
                "G" => prefs.push(pref(&shower.id, "group_channel_show", &channel.id, "true")),
                _ => {}
            }
        }
        for name in d.favorited_by.iter().flatten() {
            if let Some(u) = user_map.get(&name.to_lowercase()) {
                prefs.push(pref(&u.id, "favorite_channel", &channel.id, "true"));
            }
        }
        if !prefs.is_empty() {
            self.save_preferences(&mut prefs)
                .await
                .map_err(|e| IErr::new("importDirectChannel", "app.preference.save.updating.app_error", 400).wrap(e))?;
        }
        if let Some(h) = &d.header {
            let mut c = channel.clone();
            c.header = h.clone();
            app.store_update_channel(&mut c).await.map_err(|e| {
                IErr::bad("app.import.import_direct_channel.update_header_failed.error").wrap(e.message)
            })?;
        }
        Ok(())
    }

    // ---------- posts ----------

    async fn posts_created_at(&self, channel_id: &str, create_at: i64) -> Result<Vec<Post>, sqlx::Error> {
        let sql = format!("{} WHERE CreateAt = $1 AND ChannelId = $2", Post::SELECT);
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(create_at).bind(channel_id).fetch_all(&self.app.db).await
    }

    /// `SqlPostStore.SaveMultiple`: the posts, their threads; then the channels' counters and the roots' `UpdateAt`.
    /// The index of the post that failed, when there is one.
    async fn save_posts(&self, posts: &mut [Post]) -> Result<(), (Option<usize>, IErr)> {
        let app = &self.app;
        let max = app.max_post_size().await as usize;
        struct Counts {
            all: i64,
            root: i64,
            last: i64,
            last_root: i64,
        }
        let mut channels: Vec<(String, Counts)> = vec![];
        let mut roots: Vec<(String, i64)> = vec![];
        for (i, p) in posts.iter_mut().enumerate() {
            if !p.id.is_empty() && !p.is_remote() {
                return Err((
                    Some(i),
                    IErr::new("importMultiplePostLines", "app.post.save.existing.app_error", 400)
                        .wrap(format!("invalid input: entity: Post field: id value: {}", p.id)),
                ));
            }
            // Post.PreSave leaves RemoteId nil: an imported post's RemoteId is NULL, as Go's SaveMultiple writes it.
            p.pre_save(now_ms());
            app.validate_post(p, max).map_err(|e| (Some(i), IErr::from(e)))?;
            let count = if p.excludes_from_message_count() { 0 } else { 1 };
            let root = p.root_id.is_empty();
            match channels.iter_mut().find(|(c, _)| *c == p.channel_id) {
                None => channels.push((
                    p.channel_id.clone(),
                    Counts {
                        all: count,
                        root: if root { count } else { 0 },
                        last: p.create_at,
                        last_root: if root { p.create_at } else { 0 },
                    },
                )),
                Some((_, c)) => {
                    c.all += count;
                    c.last = c.last.max(p.create_at);
                    if root {
                        c.root += count;
                        c.last_root = c.last_root.max(p.create_at);
                    }
                }
            }
            if !root {
                match roots.iter_mut().find(|(r, _)| *r == p.root_id) {
                    None => roots.push((p.root_id.clone(), p.create_at)),
                    Some((_, t)) => *t = (*t).max(p.create_at),
                }
            }
        }
        let fail = |e: sqlx::Error| {
            (
                None,
                IErr::new("importMultiplePostLines", "app.post.save.app_error", 500)
                    .wrap(format!("failed to save Post: {e}")),
            )
        };
        let mut tx = app.db.begin().await.map_err(fail)?;
        for p in posts.iter() {
            p.bind(sqlx::query(Post::INSERT)).execute(&mut *tx).await.map_err(fail)?;
        }
        // updateThreadsFromPosts
        let mut by_root: Vec<(String, Vec<&Post>)> = vec![];
        for p in posts.iter().filter(|p| !p.root_id.is_empty()) {
            match by_root.iter_mut().find(|(r, _)| *r == p.root_id) {
                Some((_, v)) => v.push(p),
                None => by_root.push((p.root_id.clone(), vec![p])),
            }
        }
        let thread_err = |e: sqlx::Error| {
            (
                None,
                IErr::new("importMultiplePostLines", "app.post.save.app_error", 500)
                    .wrap(format!("update thread from posts failed: {e}")),
            )
        };
        for (root, replies) in &by_root {
            let found: Option<(sqlx::types::Json<Vec<String>>, i64, i64)> = sqlx::query_as(
                "SELECT COALESCE(Participants, '[]'), COALESCE(ReplyCount, 0), COALESCE(LastReplyAt, 0) FROM Threads WHERE PostId = $1",
            )
            .bind(root)
            .fetch_optional(&mut *tx)
            .await
            .map_err(thread_err)?;
            match found {
                None => {
                    let parts: Vec<String> = sqlx::query_scalar(
                        "SELECT UserId FROM Posts WHERE RootId = $1 AND DeleteAt = 0 GROUP BY UserId ORDER BY MAX(CreateAt) ASC",
                    )
                    .bind(root)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(thread_err)?;
                    let (count, last): (i64, i64) = sqlx::query_as(
                        "SELECT COUNT(Id), COALESCE(MAX(CreateAt), 0) FROM Posts WHERE RootId = $1 AND DeleteAt = 0",
                    )
                    .bind(root)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(thread_err)?;
                    let team: String = sqlx::query_scalar("SELECT COALESCE(TeamId, '') FROM Channels WHERE Id = $1")
                        .bind(&replies[0].channel_id)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(thread_err)?
                        .unwrap_or_default();
                    sqlx::query(
                        "INSERT INTO Threads (PostId, ChannelId, ReplyCount, LastReplyAt, Participants, ThreadTeamId)
                         VALUES ($1, $2, $3, $4, $5, $6)",
                    )
                    .bind(root)
                    .bind(&replies[0].channel_id)
                    .bind(count)
                    .bind(last)
                    .bind(sqlx::types::Json(parts))
                    .bind(team)
                    .execute(&mut *tx)
                    .await
                    .map_err(thread_err)?;
                }
                Some((mut parts, mut count, mut last)) => {
                    for r in replies {
                        count += 1;
                        parts.0.retain(|u| *u != r.user_id);
                        parts.0.push(r.user_id.clone());
                        last = last.max(r.create_at);
                    }
                    sqlx::query("UPDATE Threads SET ChannelId = $1, ReplyCount = $2, LastReplyAt = $3, Participants = $4 WHERE PostId = $5")
                        .bind(&replies[0].channel_id)
                        .bind(count)
                        .bind(last)
                        .bind(parts)
                        .bind(root)
                        .execute(&mut *tx)
                        .await
                        .map_err(thread_err)?;
                }
            }
        }
        tx.commit().await.map_err(fail)?;
        for (channel_id, c) in &channels {
            if let Err(e) = sqlx::query(
                "UPDATE Channels SET LastPostAt = GREATEST($1, LastPostAt), LastRootPostAt = GREATEST($2, LastRootPostAt),
                     TotalMsgCount = TotalMsgCount + $3, TotalMsgCountRoot = TotalMsgCountRoot + $4
                 WHERE Id = $5",
            )
            .bind(c.last)
            .bind(c.last_root)
            .bind(c.all)
            .bind(c.root)
            .bind(channel_id)
            .execute(&app.db)
            .await
            {
                warn(&format!("Error updating Channel LastPostAt. err={e}"));
            }
            app.invalidate_channel_posts(channel_id);
            if let Ok(ch) = app.channel_or_404(channel_id).await {
                app.invalidate_channel_cache(&ch);
            }
        }
        for (root, t) in &roots {
            if let Err(e) =
                sqlx::query("UPDATE Posts SET UpdateAt = $1 WHERE Id = $2").bind(t).bind(root).execute(&app.db).await
            {
                warn(&format!("Error updating Post UpdateAt. err={e}"));
            }
        }
        Ok(())
    }

    /// `SqlPostStore.OverwriteMultiple`.
    async fn overwrite_posts(&self, posts: &mut [Post]) -> Result<(), (Option<usize>, IErr)> {
        let app = &self.app;
        let max = app.max_post_size().await as usize;
        let update_at = now_ms();
        let err = |i: Option<usize>, e: String| {
            (i, IErr::new("importMultiplePostLines", "app.post.overwrite.app_error", 500).wrap(e))
        };
        for (i, p) in posts.iter_mut().enumerate() {
            p.update_at = update_at;
            app.validate_post(p, max).map_err(|e| err(Some(i), e.message))?;
        }
        let mut tx = app.db.begin().await.map_err(|e| err(None, e.to_string()))?;
        for (i, p) in posts.iter().enumerate() {
            sqlx::query(
                "UPDATE Posts SET CreateAt = $2, UpdateAt = $3, EditAt = $4, DeleteAt = $5, IsPinned = $6, UserId = $7,
                     ChannelId = $8, RootId = $9, OriginalId = $10, Message = $11, Type = $12, Props = $13, Hashtags = $14,
                     Filenames = $15, FileIds = $16, HasReactions = $17, RemoteId = $18
                 WHERE Id = $1",
            )
            .bind(&p.id)
            .bind(p.create_at)
            .bind(p.update_at)
            .bind(p.edit_at)
            .bind(p.delete_at)
            .bind(p.is_pinned)
            .bind(&p.user_id)
            .bind(&p.channel_id)
            .bind(&p.root_id)
            .bind(&p.original_id)
            .bind(&p.message)
            .bind(&p.r#type)
            .bind(&p.props)
            .bind(&p.hashtags)
            .bind(&p.filenames)
            .bind(&p.file_ids)
            .bind(p.has_reactions)
            .bind(&p.remote_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| err(Some(i), format!("failed to update Post with id={}: {e}", p.id)))?;
            // Go updates the thread of the post's own id here, which only roots have.
            if !p.root_id.is_empty() {
                sqlx::query("UPDATE Threads SET LastReplyAt = $1 WHERE PostId = $2")
                    .bind(update_at)
                    .bind(&p.id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| err(Some(i), format!("failed to update Threads with postid={}: {e}", p.id)))?;
            }
        }
        tx.commit().await.map_err(|e| err(None, e.to_string()))?;
        for p in posts.iter() {
            app.invalidate_channel_posts(&p.channel_id);
        }
        Ok(())
    }

    /// `SqlThreadStore.SaveMultipleMemberships`.
    async fn save_thread_memberships(&self, members: &mut [ThreadMembership]) -> Result<(), String> {
        if members.is_empty() {
            return Ok(());
        }
        for m in members.iter_mut() {
            if !is_valid_id(&m.post_id) || !is_valid_id(&m.user_id) {
                return Err("invalid thread membership".into());
            }
            m.last_updated = now_ms();
        }
        let mut tx = self.app.db.begin().await.map_err(|e| e.to_string())?;
        for m in members.iter() {
            db::insert(&mut *tx, m).await.map_err(|e| format!("failed to save thread memberships: {e}"))?;
        }
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// `SqlThreadStore.MaintainMultipleFromImport`: an imported follower follows, with its last view and mentions,
    /// unless its membership changed after that view; the thread's participants include it.
    async fn maintain_thread_memberships(&self, members: &[ThreadMembership]) -> Result<(), String> {
        let mut tx = self.app.db.begin().await.map_err(|e| e.to_string())?;
        for m in members {
            let now = now_ms();
            let found: Option<i64> = sqlx::query_scalar(
                "SELECT COALESCE(LastUpdated, 0) FROM ThreadMemberships WHERE PostId = $1 AND UserId = $2",
            )
            .bind(&m.post_id)
            .bind(&m.user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| format!("failed to get thread membership: {e}"))?;
            match found {
                Some(last_updated) if last_updated > m.last_viewed => continue,
                Some(_) => {
                    sqlx::query(
                        "UPDATE ThreadMemberships SET Following = true, LastViewed = $3, LastUpdated = $4, UnreadMentions = $5
                         WHERE PostId = $1 AND UserId = $2",
                    )
                    .bind(&m.post_id)
                    .bind(&m.user_id)
                    .bind(m.last_viewed)
                    .bind(now)
                    .bind(m.unread_mentions)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())?;
                }
                None => {
                    let row = ThreadMembership { following: true, last_updated: now, ..m.clone() };
                    db::insert(&mut *tx, &row).await.map_err(|e| e.to_string())?;
                }
            }
            sqlx::query("UPDATE Threads SET Participants = Participants || $1::jsonb WHERE PostId = $2 AND NOT Participants ? $3")
                .bind(sqlx::types::Json(vec![m.user_id.clone()]))
                .bind(&m.post_id)
                .bind(&m.user_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// `extractThreadMembers`.
    async fn thread_members(
        &self,
        followers: &[ThreadFollowerData],
        users: &HashMap<String, User>,
        post: &Post,
        line: usize,
    ) -> Result<Vec<ThreadMembership>, (usize, IErr)> {
        let mut out = vec![];
        for f in followers {
            let name = f.user.clone().unwrap_or_default();
            let user = match users.get(&name.to_lowercase()) {
                Some(u) => u.clone(),
                None => {
                    let lookup = self.report.lookup(&name).unwrap_or_else(|| name.clone());
                    match self.user_by_username(&lookup).await {
                        Ok(Some(u)) => u,
                        Ok(None) if self.deactivate_missing_users => {
                            warn(&format!(
                                "Skipping thread follower not found on destination during scoped import username={name}"
                            ));
                            continue;
                        }
                        Ok(None) => {
                            return Err((
                                line,
                                IErr::new(
                                    "importMultiplePostLines",
                                    "app.import.get_users_by_username.some_users_not_found.error",
                                    400,
                                )
                                .wrap(format!(
                                    "failed to find User: {}",
                                    not_found("User", &format!("username={lookup}"))
                                )),
                            ));
                        }
                        Err(e) => {
                            return Err((
                                line,
                                IErr::new("extractThreadMembers", "app.user.get_by_username.app_error", 500)
                                    .wrap(e.text()),
                            ));
                        }
                    }
                }
            };
            out.push(ThreadMembership {
                post_id: post.id.clone(),
                user_id: user.id,
                following: true,
                last_viewed: f.last_viewed.unwrap_or(0),
                unread_mentions: f.unread_mentions.unwrap_or(0),
                ..Default::default()
            });
        }
        Ok(out)
    }

    /// `SqlReactionStore.Save`: the reaction (or its revival), the post marked as having reactions.
    async fn save_reaction(&self, mut r: Reaction) -> IResult<()> {
        let now = now_ms();
        if r.create_at == 0 {
            r.create_at = now;
        }
        r.update_at = now;
        r.delete_at = 0;
        if r.remote_id.is_none() {
            r.remote_id = Some(String::new());
        }
        let invalid =
            |f: &str| Err(IErr::new("Reaction.IsValid", &format!("model.reaction.is_valid.{f}.app_error"), 400));
        if !is_valid_id(&r.user_id) {
            return invalid("user_id");
        }
        if !is_valid_id(&r.post_id) {
            return invalid("post_id");
        }
        let name_ok = !r.emoji_name.is_empty()
            && r.emoji_name.chars().count() <= EMOJI_NAME_MAX
            && r.emoji_name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'+' || b == b'_');
        if !name_ok {
            return invalid("emoji_name");
        }
        let wrap = |e: sqlx::Error| {
            IErr::new("importReaction", "app.reaction.save.save.app_error", 500)
                .wrap(format!("failed while saving reaction or updating post: {e}"))
        };
        let mut tx = self.app.db.begin().await.map_err(wrap)?;
        if r.channel_id.is_empty() {
            let c: Option<String> = sqlx::query_scalar("SELECT ChannelId FROM Posts WHERE Id = $1")
                .bind(&r.post_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(wrap)?;
            let Some(c) = c else {
                return Err(IErr::new("importReaction", "app.reaction.save.save.app_error", 500)
                    .wrap(not_found("Post", &r.post_id)));
            };
            r.channel_id = c;
        }
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
                    .await
                    .map_err(wrap)?;
                tx.commit().await.map_err(wrap)?;
            }
            Err(e) if e.as_database_error().is_some_and(|d| d.is_unique_violation()) => {}
            Err(e) => return Err(wrap(e)),
        }
        self.app.caches.reactions.remove(&r.post_id);
        self.app.invalidate_channel_posts(&r.channel_id);
        Ok(())
    }

    /// `importReaction`.
    async fn import_reaction(&self, d: &ReactionData, post: &Post) -> IResult<()> {
        validate_reaction(d, post.create_at)?;
        let name = d.user.clone().unwrap_or_default();
        let lookup_err =
            |e: IErr| IErr::new("importReaction", "app.user.get_by_username.app_error", 500).wrap(e.text());
        let mut user = self.user_by_username(&name).await.map_err(lookup_err)?;
        if user.is_none()
            && let Some(dest) = self.report.lookup(&name)
        {
            user = self.user_by_username(&dest).await.map_err(lookup_err)?;
        }
        let Some(user) = user else {
            if self.deactivate_missing_users {
                warn(&format!(
                    "Skipping reaction from user not found on destination during scoped import username={name}"
                ));
                return Ok(());
            }
            return Err(IErr::bad("app.import.import_post.user_not_found.error").p("Username", &name).wrap(format!(
                "failed to find User: {}",
                not_found("User", &format!("username={}", name.to_lowercase()))
            )));
        };
        self.save_reaction(Reaction {
            user_id: user.id,
            post_id: post.id.clone(),
            emoji_name: d.emoji_name.clone().unwrap_or_default(),
            create_at: d.create_at.unwrap_or_default(),
            ..Default::default()
        })
        .await
    }

    /// `importAttachment`: a file of the post, kept when the post has it already (same name, size and content).
    async fn import_attachment(&self, d: &AttachmentData, post: &Post, team_id: &str) -> IResult<FileInfo> {
        let app = &self.app;
        let path = d.path.clone().unwrap_or_default();
        let bad_file = |e: String| IErr::bad("app.import.attachment.bad_file.error").p("FilePath", &path).wrap(e);
        let max = app.cfg.i64("FileSettings.MaxFileSize").max(0) as u64;
        let (name, size, bytes) = match &d.data {
            Some(z) => {
                info(&format!("Preparing file upload from ZIP file_name={} file_size={}", z.name, z.size));
                let limit = if z.size > max { Some(0) } else { None };
                let bytes = read_zip_file(&self.archive, &z.name, limit).await.map_err(bad_file)?;
                (z.name.clone(), z.size as i64, bytes)
            }
            None => {
                let meta = std::fs::metadata(&path).map_err(|e| bad_file(format!("open {path}: {e}")))?;
                let bytes = if meta.len() > max {
                    vec![]
                } else {
                    std::fs::read(&path).map_err(|e| bad_file(format!("open {path}: {e}")))?
                };
                info(&format!("Preparing file upload from file system file_name={path} file_size={}", meta.len()));
                (path.clone(), meta.len() as i64, bytes)
            }
        };
        let base = name.rsplit('/').next().unwrap_or(&name).to_string();
        if !post.id.is_empty() {
            let old: Vec<FileInfo> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{} WHERE PostId = $1 AND DeleteAt = 0 ORDER BY CreateAt",
                FileInfo::SELECT
            )))
            .bind(&post.id)
            .fetch_all(&app.db)
            .await
            .map_err(|e| IErr::bad("app.import.attachment.file_upload.error").p("FilePath", &path).wrap(e))?;
            for f in old {
                if f.name != base || f.size != size {
                    continue;
                }
                let stored =
                    app.files.read(&f.path).await.map_err(|e| {
                        IErr::bad("app.import.attachment.file_upload.error").p("FilePath", &path).wrap(e)
                    })?;
                use sha2::Digest;
                if sha2::Sha256::digest(&stored) == sha2::Sha256::digest(&bytes) {
                    info(&format!(
                        "Skipping uploading of file because name already exists and content matches file_name={name}"
                    ));
                    return Ok(f);
                }
                info(&format!("File contents don't match, will re-upload file_name={name}"));
                break;
            }
        }
        info(&format!("Uploading file with name file_name={name}"));
        let upload = crate::file_api::Upload {
            name: base,
            channel_id: post.channel_id.rsplit('/').next().unwrap_or(&post.channel_id).to_string(),
            user_id: post.user_id.clone(),
            timestamp: post.create_at,
            content_length: size,
            team_id: team_id.to_string(),
            extract: Some(self.extract_content),
        };
        let stream = futures_util::stream::once(async move { Ok::<bytes::Bytes, String>(bytes.into()) });
        crate::file_api::upload_file_x(app, upload, Box::pin(stream)).await.map_err(|e| {
            crate::logs::error(&format!("Failed to upload file err={} file_name={name}", e.message), &[]);
            IErr::from(e)
        })
    }

    /// `uploadAttachments`: the ids of the post's files after the import (failures only logged).
    async fn upload_attachments(
        &self,
        attachments: &Option<Vec<AttachmentData>>,
        post: &Post,
        team_id: &str,
    ) -> Option<Vec<String>> {
        let list = attachments.as_ref()?;
        let mut ids: Vec<String> = vec![];
        for a in list {
            match self.import_attachment(a, post, team_id).await {
                Ok(f) => {
                    if !ids.contains(&f.id) {
                        ids.push(f.id);
                    }
                }
                Err(e) => match &a.path {
                    Some(p) => warn(&format!("failed to import attachment path={p} error={}", e.text())),
                    None => warn(&format!("failed to import attachment; path was nil error={}", e.text())),
                },
            }
        }
        Some(ids)
    }

    /// The post's files set to what the import gave it; the ones it no longer has deleted.
    async fn set_post_files(&self, post: &mut Post, attachments: &Option<Vec<AttachmentData>>, team_id: &str) {
        let ids = self.upload_attachments(attachments, post, team_id).await.unwrap_or_default();
        for old in post.file_ids.0.clone() {
            if !ids.contains(&old)
                && let Err(e) = sqlx::query("DELETE FROM FileInfo WHERE Id = $1").bind(&old).execute(&self.app.db).await
            {
                warn(&format!("Error while permanently deleting file info file_id={old} err={e}"));
            }
        }
        post.file_ids = crate::model::FileIds(ids, false);
    }

    /// `updateFileInfoWithPostId`.
    async fn attach_files(&self, post: &Post) {
        for id in &post.file_ids.0 {
            let r = sqlx::query(
                "UPDATE FileInfo SET PostId = $1, ChannelId = $2 WHERE Id = $3 AND PostId = '' AND (CreatorId = $4 OR CreatorId = 'nouser')",
            )
            .bind(&post.id)
            .bind(&post.channel_id)
            .bind(id)
            .bind(&post.user_id)
            .execute(&self.app.db)
            .await;
            match r {
                Ok(r) if r.rows_affected() == 0 => crate::logs::error(
                    &format!(
                        "Error attaching files to post. post_id={} err={}",
                        post.id,
                        not_found("FileInfo", &format!("<{id}, {}, {}>", post.id, post.user_id))
                    ),
                    &[],
                ),
                Ok(_) => {}
                Err(e) => {
                    crate::logs::error(&format!("Error attaching files to post. post_id={} err={e}", post.id), &[])
                }
            }
        }
        if !post.file_ids.0.is_empty() {
            self.app.caches.file_infos.remove(&post.id);
        }
    }

    /// The flagged-post preferences of a post.
    async fn save_flags(
        &self,
        post_id: &str,
        flagged_by: &Option<Vec<String>>,
        users: &HashMap<String, User>,
    ) -> IResult<()> {
        let mut prefs: Vec<Preference> = flagged_by
            .iter()
            .flatten()
            .filter_map(|n| users.get(&n.to_lowercase()))
            .map(|u| pref(&u.id, "flagged_post", post_id, "true"))
            .collect();
        if !prefs.is_empty() {
            self.save_preferences(&mut prefs)
                .await
                .map_err(|e| IErr::new("BulkImport", "app.import.import_post.save_preferences.error", 500).wrap(e))?;
        }
        Ok(())
    }

    /// `importReplies`.
    async fn import_replies(&self, data: &[ReplyData], post: &Post, team_id: &str) -> IResult<()> {
        let max = self.app.max_post_size().await as usize;
        let mut names = vec![];
        for r in data {
            validate_reply(r, post.create_at, max)?;
            names.push(r.user.clone().unwrap_or_default());
            names.extend(r.flagged_by.iter().flatten().cloned());
        }
        let users = self.users_by_usernames(&names).await?;
        let mut with_data: Vec<(Post, &ReplyData)> = vec![];
        let mut create: Vec<(Post, &ReplyData)> = vec![];
        let mut overwrite: Vec<(Post, &ReplyData)> = vec![];
        for r in data {
            let uname = r.user.clone().unwrap_or_default();
            let Some(user) = users.get(&uname.to_lowercase()) else {
                warn(&format!(
                    "Skipping reply from user not found on destination during scoped import username={uname}"
                ));
                continue;
            };
            let mut create_at = r.create_at.unwrap_or_default();
            if create_at < post.create_at {
                warn(&format!(
                    "Reply CreateAt is before parent post CreateAt, setting it to parent post CreateAt reply_create_at={create_at} parent_create_at={}",
                    post.create_at
                ));
                create_at = post.create_at;
            }
            let message = r.message.clone().unwrap_or_default();
            let existing = self
                .posts_created_at(&post.channel_id, create_at)
                .await
                .map_err(|e| IErr::new("importReplies", "app.post.get_posts_created_at.app_error", 500).wrap(e))?;
            let mut reply =
                existing.into_iter().find(|p| p.message == message && p.root_id == post.id).unwrap_or_default();
            reply.user_id = user.id.clone();
            reply.channel_id = post.channel_id.clone();
            reply.root_id = post.id.clone();
            reply.message = message;
            reply.create_at = create_at;
            if let Some(p) = &r.props {
                reply.props = sqlx::types::Json(p.clone());
            }
            if let Some(t) = &r.r#type {
                reply.r#type = t.clone();
            }
            if let Some(t) = r.edit_at {
                reply.edit_at = t;
            }
            if let Some(v) = r.is_pinned {
                reply.is_pinned = v;
            }
            self.set_post_files(&mut reply, &r.attachments, team_id).await;
            if reply.id.is_empty() {
                create.push((reply, r));
            } else {
                overwrite.push((reply, r));
            }
        }
        let mut reactions: Vec<(Post, &Vec<ReactionData>)> = vec![];
        if !create.is_empty() {
            let mut posts: Vec<Post> = create.iter().map(|(p, _)| p.clone()).collect();
            if let Err((_, e)) = self.save_posts(&mut posts).await {
                return Err(e.at("importReplies"));
            }
            // Reactions go to the created reply of the same CreateAt (the last data of that time wins).
            let mut by_time: HashMap<i64, &Vec<ReactionData>> = HashMap::new();
            for (p, r) in &create {
                if let Some(rs) = r.reactions.as_ref().filter(|v| !v.is_empty()) {
                    by_time.insert(p.create_at, rs);
                }
            }
            for (p, (_, r)) in posts.iter().zip(create.iter()) {
                if let Some(rs) = by_time.get(&p.create_at) {
                    reactions.push((p.clone(), rs));
                }
                with_data.push((p.clone(), r));
            }
        }
        let mut over: Vec<Post> = overwrite.iter().map(|(p, _)| p.clone()).collect();
        if let Err((_, e)) = self.overwrite_posts(&mut over).await {
            return Err(e.at("importReplies"));
        }
        for (p, (_, r)) in over.iter().zip(overwrite.iter()) {
            if let Some(rs) = r.reactions.as_ref().filter(|v| !v.is_empty()) {
                reactions.push((p.clone(), rs));
            }
            with_data.push((p.clone(), r));
        }
        for (p, rs) in &reactions {
            for r in rs.iter() {
                self.import_reaction(r, p).await?;
            }
        }
        for (p, r) in &with_data {
            self.attach_files(p).await;
            self.save_flags(&p.id, &r.flagged_by, &users).await?;
        }
        Ok(())
    }

    /// `importMultiplePostLines`: the error and the line that caused it, when known (0 otherwise).
    pub async fn import_multiple_post_lines(&self, lines: Vec<WorkLine>) -> LineResult {
        if lines.is_empty() {
            return Ok(());
        }
        info(&format!("Validating post lines count={} first_line={}", lines.len(), lines[0].line));
        let max = self.app.max_post_size().await as usize;
        for l in &lines {
            if let Some(p) = &l.data.post {
                validate_post(p, max).map_err(|e| (l.line, e))?;
            }
        }
        if self.dry_run {
            return Ok(());
        }
        info(&format!("Importing post lines count={} first_line={}", lines.len(), lines[0].line));
        let posts: Vec<(&PostData, usize)> =
            lines.iter().filter_map(|l| l.data.post.as_ref().map(|p| (p, l.line))).collect();
        let mut names = vec![];
        let mut follower_names = vec![];
        let mut team_names = vec![];
        for (p, _) in &posts {
            names.push(p.user.clone().unwrap_or_default());
            names.extend(p.flagged_by.iter().flatten().cloned());
            follower_names.extend(p.thread_followers.iter().flatten().map(|f| f.user.clone().unwrap_or_default()));
            team_names.push(p.team.clone().unwrap_or_default());
        }
        let mut users = self.users_by_usernames(&names).await.map_err(|e| (0, e))?;
        self.prefetch_users(&follower_names, &mut users).await;
        let teams = self.teams_by_names(&team_names).await.map_err(|e| (0, e))?;

        // getChannelsForPosts
        let mut channels: HashMap<(String, String), Option<Channel>> = HashMap::new();
        for (p, _) in &posts {
            let team_name = p.team.as_deref().unwrap_or_default().to_lowercase();
            let Some(team) = teams.get(&team_name) else { continue };
            let channel = p.channel.clone().unwrap_or_default();
            let key = (team_name, channel.to_lowercase());
            if channels.contains_key(&key) {
                continue;
            }
            match self.channel_by_name_include_deleted(&team.id, &channel).await {
                Ok(Some(c)) if self.deactivate_missing_users && c.delete_at != 0 => {
                    channels.insert(key, None);
                }
                Ok(Some(c)) => {
                    channels.insert(key, Some(c));
                }
                Ok(None) if self.deactivate_missing_users => {
                    channels.insert(key, None);
                }
                Ok(None) => {
                    return Err((
                        0,
                        IErr::bad("app.import.import_post.channel_not_found.error")
                            .p("ChannelName", &channel)
                            .wrap(not_found("Channel", &format!("TeamId={}&Name={channel}", team.id))),
                    ));
                }
                Err(e) => {
                    return Err((
                        0,
                        IErr::bad("app.import.import_post.channel_not_found.error")
                            .p("ChannelName", &channel)
                            .wrap(e.text()),
                    ));
                }
            }
        }

        let mut with_data: Vec<PostAndData> = vec![];
        let mut create: Vec<(Post, usize)> = vec![];
        let mut overwrite: Vec<(Post, usize)> = vec![];
        let mut members_to_create: HashMap<String, Vec<ThreadMembership>> = HashMap::new();
        let mut members_to_overwrite: Vec<ThreadMembership> = vec![];
        for (p, line) in &posts {
            let team_name = p.team.as_deref().unwrap_or_default().to_lowercase();
            let Some(team) = teams.get(&team_name) else {
                warn(&format!(
                    "Skipping post for team not found on destination during scoped import team={team_name} line_number={line}"
                ));
                continue;
            };
            let Some(Some(channel)) =
                channels.get(&(team_name.clone(), p.channel.as_deref().unwrap_or_default().to_lowercase()))
            else {
                warn(&format!(
                    "Skipping post for channel not found on destination during scoped import channel={} line_number={line}",
                    p.channel.as_deref().unwrap_or_default()
                ));
                continue;
            };
            let Some(user) = users.get(&p.user.as_deref().unwrap_or_default().to_lowercase()) else {
                warn(&format!(
                    "Skipping post from user not found on destination during scoped import username={} line_number={line}",
                    p.user.as_deref().unwrap_or_default()
                ));
                continue;
            };
            let message = p.message.clone().unwrap_or_default();
            let create_at = p.create_at.unwrap_or_default();
            let existing = self.posts_created_at(&channel.id, create_at).await.map_err(|e| {
                (*line, IErr::new("importMultiplePostLines", "app.post.get_posts_created_at.app_error", 500).wrap(e))
            })?;
            let mut post = existing.into_iter().find(|x| x.message == message).unwrap_or_default();
            post.channel_id = channel.id.clone();
            post.message = message;
            post.user_id = user.id.clone();
            post.create_at = create_at;
            post.hashtags = crate::posts::parse_hashtags(&post.message);
            if let Some(t) = &p.r#type {
                post.r#type = t.clone();
            }
            if let Some(t) = p.edit_at {
                post.edit_at = t;
            }
            if let Some(v) = &p.props {
                post.props = sqlx::types::Json(v.clone());
            }
            if let Some(v) = p.is_pinned {
                post.is_pinned = v;
            }
            if let Some(followers) = &p.thread_followers {
                let members = self.thread_members(followers, &users, &post, *line).await?;
                if post.id.is_empty() {
                    members_to_create.insert(post_key(&post), members);
                } else {
                    members_to_overwrite.extend(members);
                }
            }
            self.set_post_files(&mut post, &p.attachments, &team.id).await;
            with_data.push(PostAndData {
                post: post.clone(),
                flagged_by: p.flagged_by.clone(),
                reactions: p.reactions.clone(),
                replies: p.replies.clone(),
                team_id: team.id.clone(),
                line: *line,
            });
            if post.id.is_empty() {
                create.push((post, *line));
            } else {
                overwrite.push((post, *line));
            }
        }
        self.finish_posts(create, overwrite, members_to_create, members_to_overwrite, with_data, &users).await
    }

    /// The posts of a batch saved and overwritten, then their thread followers, flags, reactions, replies and
    /// files (the second half of both post importers).
    async fn finish_posts(
        &self,
        create: Vec<(Post, usize)>,
        overwrite: Vec<(Post, usize)>,
        members_to_create: HashMap<String, Vec<ThreadMembership>>,
        members_to_overwrite: Vec<ThreadMembership>,
        mut with_data: Vec<PostAndData>,
        users: &HashMap<String, User>,
    ) -> LineResult {
        let line_of =
            |list: &[(Post, usize)], i: Option<usize>| i.and_then(|i| list.get(i)).map(|(_, l)| *l).unwrap_or(0);
        if !create.is_empty() {
            let mut posts: Vec<Post> = create.iter().map(|(p, _)| p.clone()).collect();
            // The keys before saving: the id is the only thing saving changes in them... and UpdateAt.
            if let Err((i, e)) = self.save_posts(&mut posts).await {
                return Err((line_of(&create, i), e));
            }
            let mut members = vec![];
            for p in &posts {
                if let Some(ms) = members_to_create.get(&post_key(p)) {
                    for m in ms {
                        members.push(ThreadMembership { post_id: p.id.clone(), ..m.clone() });
                    }
                }
            }
            if let Err(e) = self.save_thread_memberships(&mut members).await {
                return Err((
                    0,
                    IErr::new("importMultiplePostLines", "app.post.save.thread_membership.app_error", 500).wrap(e),
                ));
            }
            // The created posts get their ids in the data list (matched by their key).
            let mut by_key: HashMap<String, &Post> = posts.iter().map(|p| (post_key(p), p)).collect();
            for d in with_data.iter_mut().filter(|d| d.post.id.is_empty()) {
                if let Some(p) = by_key.remove(&post_key(&d.post)) {
                    d.post = p.clone();
                }
            }
        }
        let mut over: Vec<Post> = overwrite.iter().map(|(p, _)| p.clone()).collect();
        if let Err((i, e)) = self.overwrite_posts(&mut over).await {
            return Err((line_of(&overwrite, i), e));
        }
        if let Err(e) = self.maintain_thread_memberships(&members_to_overwrite).await {
            return Err((
                0,
                IErr::new("importMultiplePostLines", "app.post.save.thread_membership.app_error", 500).wrap(e),
            ));
        }
        for d in &with_data {
            self.save_flags(&d.post.id, &d.flagged_by, users).await.map_err(|e| (d.line, e))?;
            for r in d.reactions.iter().flatten() {
                self.import_reaction(r, &d.post).await.map_err(|e| (d.line, e))?;
            }
            if let Some(replies) = d.replies.as_ref().filter(|r| !r.is_empty()) {
                self.import_replies(replies, &d.post, &d.team_id).await.map_err(|e| (d.line, e))?;
            }
            self.attach_files(&d.post).await;
        }
        Ok(())
    }

    /// `importMultipleDirectPostLines`.
    pub async fn import_multiple_direct_post_lines(&self, lines: Vec<WorkLine>) -> LineResult {
        if lines.is_empty() {
            return Ok(());
        }
        let max = self.app.max_post_size().await as usize;
        for l in &lines {
            if let Some(p) = &l.data.direct_post {
                validate_direct_post(p, max).map_err(|e| (l.line, e))?;
            }
        }
        if self.dry_run {
            return Ok(());
        }
        let posts: Vec<(&DirectPostData, usize)> =
            lines.iter().filter_map(|l| l.data.direct_post.as_ref().map(|p| (p, l.line))).collect();
        let mut names = vec![];
        let mut follower_names = vec![];
        for (p, _) in &posts {
            names.push(p.user.clone().unwrap_or_default());
            names.extend(p.flagged_by.iter().flatten().cloned());
            follower_names.extend(p.thread_followers.iter().flatten().map(|f| f.user.clone().unwrap_or_default()));
            names.extend(p.channel_members.iter().flatten().cloned());
        }
        let mut users = self.users_by_usernames(&names).await.map_err(|e| (0, e))?;
        self.prefetch_users(&follower_names, &mut users).await;

        let mut with_data: Vec<PostAndData> = vec![];
        let mut create: Vec<(Post, usize)> = vec![];
        let mut overwrite: Vec<(Post, usize)> = vec![];
        let mut members_to_create: HashMap<String, Vec<ThreadMembership>> = HashMap::new();
        let mut members_to_overwrite: Vec<ThreadMembership> = vec![];
        for (p, line) in &posts {
            let mut ids = vec![];
            for name in p.channel_members.iter().flatten() {
                match users.get(&name.to_lowercase()) {
                    Some(u) => ids.push(u.id.clone()),
                    None => warn(&format!(
                        "Skipping direct post channel member not found on destination during scoped import username={name} line_number={line}"
                    )),
                }
            }
            let channel = match ids.len() {
                2 => self.app.get_or_create_direct(&Session::default(), &ids[0], &ids[1]).await.map_err(|e| {
                    (
                        *line,
                        IErr::bad("app.import.import_direct_post.create_direct_channel.error")
                            .wrap(IErr::from(e).text()),
                    )
                })?,
                n if n > 2 => self.create_group_channel(&ids).await.map_err(|e| {
                    (*line, IErr::bad("app.import.import_direct_post.create_group_channel.error").wrap(e.text()))
                })?,
                _ => {
                    warn(&format!("Not enough users to create a direct channel line_number={line}"));
                    continue;
                }
            };
            let Some(user) = users.get(&p.user.as_deref().unwrap_or_default().to_lowercase()) else {
                warn(&format!(
                    "Skipping direct post from user not found on destination during scoped import username={} line_number={line}",
                    p.user.as_deref().unwrap_or_default()
                ));
                continue;
            };
            let message = p.message.clone().unwrap_or_default();
            let create_at = p.create_at.unwrap_or_default();
            let existing = self
                .posts_created_at(&channel.id, create_at)
                .await
                .map_err(|e| (*line, IErr::bad("app.post.get_posts_created_at.app_error").status(500).wrap(e)))?;
            let mut post = existing.into_iter().find(|x| x.message == message).unwrap_or_default();
            post.channel_id = channel.id.clone();
            post.message = message;
            post.user_id = user.id.clone();
            post.create_at = create_at;
            post.hashtags = crate::posts::parse_hashtags(&post.message);
            if let Some(t) = &p.r#type {
                post.r#type = t.clone();
            }
            if let Some(t) = p.edit_at {
                post.edit_at = t;
            }
            if let Some(v) = &p.props {
                post.props = sqlx::types::Json(v.clone());
            }
            if let Some(v) = p.is_pinned {
                post.is_pinned = v;
            }
            if let Some(followers) = &p.thread_followers {
                let members = self.thread_members(followers, &users, &post, *line).await?;
                if post.id.is_empty() {
                    members_to_create.insert(post_key(&post), members);
                } else {
                    members_to_overwrite.extend(members);
                }
            }
            self.set_post_files(&mut post, &p.attachments, "noteam").await;
            with_data.push(PostAndData {
                post: post.clone(),
                flagged_by: p.flagged_by.clone(),
                reactions: p.reactions.clone(),
                replies: p.replies.clone(),
                team_id: "noteam".into(),
                line: *line,
            });
            if post.id.is_empty() {
                create.push((post, *line));
            } else {
                overwrite.push((post, *line));
            }
        }
        self.finish_posts(create, overwrite, members_to_create, members_to_overwrite, with_data, &users).await
    }

    // ---------- scoped imports: SSO preflight ----------

    /// The user lines of the JSONL (after the version line), parsed (bad lines skipped), with their count of lines.
    async fn user_lines(&self, jsonl: &str) -> (Vec<UserData>, usize) {
        let mut a = self.archive.clone();
        let name = jsonl.to_string();
        tokio::task::spawn_blocking(move || {
            use std::io::BufRead;
            let Ok(f) = a.by_name(&name) else { return (vec![], 0) };
            let r = std::io::BufReader::new(f);
            let mut out = vec![];
            let mut n = 0;
            for line in r.split(b'\n') {
                let Ok(line) = line else { break };
                n += 1;
                if n == 1 {
                    continue;
                }
                if let Ok(l) = crate::bulk_import::decode_line(&line)
                    && l.r#type == "user"
                    && let Some(u) = l.user
                {
                    out.push(u);
                }
            }
            (out, n)
        })
        .await
        .unwrap_or_default()
    }

    /// `checkSSOProviderConfig`: a scoped import of SSO users needs their providers enabled here.
    pub async fn check_sso_provider_config(&self, jsonl: &str, skip_preflight: bool) -> IResult<()> {
        let cfg = &self.app.cfg;
        let (users, _) = self.user_lines(jsonl).await;
        let mut seen = HashSet::new();
        for u in users {
            let Some(svc) = u.auth_service.filter(|s| !s.is_empty()) else { continue };
            if !seen.insert(svc.clone()) {
                continue;
            }
            let fail = match svc.as_str() {
                "ldap" if !cfg.bool("LdapSettings.Enable") => {
                    Some("export contains LDAP users but LDAP is not enabled on the destination")
                }
                "ldap" if cfg.str("LdapSettings.IdAttribute").is_empty() => Some(
                    "LdapSettings.IdAttribute is empty on the destination — auth_data matching will fail for LDAP users",
                ),
                "saml" if !cfg.bool("SamlSettings.Enable") => {
                    Some("export contains SAML users but SAML is not enabled on the destination")
                }
                "gitlab" if !cfg.bool("GitLabSettings.Enable") => {
                    Some("export contains GitLab OAuth users but GitLab OAuth is not enabled on the destination")
                }
                "google" if !cfg.bool("GoogleSettings.Enable") => {
                    Some("export contains Google OAuth users but Google OAuth is not enabled on the destination")
                }
                "office365" if !cfg.bool("Office365Settings.Enable") => Some(
                    "export contains Office 365 OAuth users but Office 365 OAuth is not enabled on the destination",
                ),
                "openid" if !cfg.bool("OpenIdSettings.Enable") => {
                    Some("export contains OpenID users but OpenID is not enabled on the destination")
                }
                _ => None,
            };
            if let Some(msg) = fail {
                if skip_preflight {
                    warn(&format!(
                        "PREFLIGHT (skipped): {msg} — proceeding anyway as --skip-preflight was set auth_service={svc}"
                    ));
                } else {
                    return Err(IErr::bad("app.import.preflight.auth_provider_not_configured.error")
                        .p("Service", &svc)
                        .p("Detail", msg));
                }
            }
        }
        Ok(())
    }

    /// `preCreateSSOUsers`: the SSO users of a scoped import made (or found) before the main pass; the number of
    /// lines of the file.
    pub async fn pre_create_sso_users(&self, jsonl: &str) -> usize {
        let (users, n) = self.user_lines(jsonl).await;
        for u in users {
            let (Some(username), Some(_), Some(_), Some(data)) = (&u.username, &u.email, &u.auth_service, &u.auth_data)
            else {
                continue;
            };
            if data.is_empty() || self.dry_run {
                continue;
            }
            if let Err(e) = self.pre_create_sso_user(&u).await {
                warn(&format!(
                    "preCreateSSOUsers: failed to pre-create SSO user; main pass will handle it username={username} auth_service={} err={}",
                    u.auth_service.as_deref().unwrap_or_default(),
                    e.text()
                ));
            }
        }
        n
    }

    /// `preCreateSSOUser`.
    async fn pre_create_sso_user(&self, d: &UserData) -> IResult<()> {
        let (username, email, service, data) = (
            d.username.clone().unwrap_or_default(),
            d.email.clone().unwrap_or_default(),
            d.auth_service.clone().unwrap_or_default(),
            d.auth_data.clone().unwrap_or_default(),
        );
        if self
            .user_by_auth(&data, &service)
            .await
            .map_err(|e| IErr::new("preCreateSSOUser", "app.user.get_by_auth.app_error", 500).wrap(e.text()))?
            .is_some()
        {
            return Ok(());
        }
        if !self.deactivate_missing_users
            && let Some(existing) = self
                .user_by_username(&username)
                .await
                .map_err(|e| IErr::new("preCreateSSOUser", "app.user.get_by_username.app_error", 500).wrap(e.text()))?
        {
            if !existing.auth_service.is_empty() && existing.auth_service != service {
                return Ok(());
            }
            let now = now_ms();
            sqlx::query(
                "UPDATE Users SET Password = '', LastPasswordUpdate = $1, UpdateAt = $1, FailedAttempts = 0, AuthService = $2,
                     AuthData = $3, Email = lower($4)
                 WHERE Id = $5",
            )
            .bind(now)
            .bind(&service)
            .bind(&data)
            .bind(&existing.email)
            .bind(&existing.id)
            .execute(&self.app.db)
            .await
            .map_err(|e| IErr::new("preCreateSSOUser", "app.user.update_auth_data.app_error", 500).wrap(e))?;
            self.app.invalidate_user(&existing.id);
            return Ok(());
        }
        let inactive = self.deactivate_missing_users && self.imported_users == "inactive";
        let mut u = User {
            username,
            email,
            auth_service: service,
            auth_data: Some(data),
            roles: "system_user".into(),
            notify_props: sqlx::types::Json(crate::user_edit::default_notify_props()),
            ..Default::default()
        };
        if let Some(t) = d.delete_at.filter(|t| *t > 0) {
            u.delete_at = t;
        } else if inactive {
            u.delete_at = now_ms();
        }
        if inactive {
            u.props.0.insert("importedInactive".into(), "true".into());
        }
        self.app
            .save_user_row(&mut u)
            .await
            .map_err(|e| IErr::new("preCreateSSOUser", "app.user.save.app_error", 500).wrap(e.message))?;
        self.app.invalidate_user(&u.id);
        Ok(())
    }
}

/// `Scheme.IsValidForCreate`, as far as the importer can get it wrong.
fn scheme_valid(s: &Scheme) -> bool {
    let n = s.display_name.chars().count();
    n > 0
        && n <= 128
        && valid_scheme_name(&s.name)
        && s.description.chars().count() <= 1024
        && ["team", "channel", "playbook", "run"].contains(&s.scope.as_str())
}

enum RoleErr {
    Invalid(String),
    Db(String),
}
