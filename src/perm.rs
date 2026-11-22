//! Roles and permissions: `RolesGrantPermission` over cached roles, the scheme-derived roles of team and channel
//! members, and the session checks of `app/authorization.go`.

// Based on Mattermost server/channels/app/authorization.go, store/sqlstore/team_store.go (getTeamRoles) and
// channel_store.go (getChannelRoles, GetAllChannelMembersForUser), Copyright (c) 2015-present Mattermost, Inc.
// Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::app::{ApiResult, App, AppError};
use crate::cache::MemberRoles;
use crate::db::{self, Table};
use crate::model::{ChannelMember, Role, Session, TeamMember};

pub const CHANNEL_GUEST: &str = "channel_guest";
pub const CHANNEL_USER: &str = "channel_user";
pub const CHANNEL_ADMIN: &str = "channel_admin";
pub const TEAM_GUEST: &str = "team_guest";
pub const TEAM_USER: &str = "team_user";
pub const TEAM_ADMIN: &str = "team_admin";
pub const SYSTEM_ADMIN: &str = "system_admin";

/// Scheme defaults that apply to a member: guest, user and admin roles of the channel scheme and of the team
/// scheme, empty where there is no scheme.
#[derive(Default, Debug, Clone)]
pub struct SchemeDefaults {
    pub team: [String; 3],
    pub channel: [String; 3],
}

/// The joins and columns that bring a member's scheme defaults: `ts` the team scheme, `cs` the channel scheme.
/// Use with `FROM ChannelMembers cm JOIN Channels c ON c.id = cm.channelid` + [`CHANNEL_SCHEME_JOINS`].
pub const CHANNEL_SCHEME_COLS: &str = "ts.defaultchannelguestrole AS ts_g, ts.defaultchanneluserrole AS ts_u, \
     ts.defaultchanneladminrole AS ts_a, cs.defaultchannelguestrole AS cs_g, cs.defaultchanneluserrole AS cs_u, \
     cs.defaultchanneladminrole AS cs_a";
pub const CHANNEL_SCHEME_JOINS: &str = "LEFT JOIN Schemes cs ON c.schemeid = cs.id \
     LEFT JOIN Teams t ON c.teamid = t.id LEFT JOIN Schemes ts ON t.schemeid = ts.id";
/// Use with `FROM TeamMembers tm` + [`TEAM_SCHEME_JOINS`].
pub const TEAM_SCHEME_COLS: &str = "ts.defaultteamguestrole AS ts_g, ts.defaultteamuserrole AS ts_u, \
     ts.defaultteamadminrole AS ts_a";
pub const TEAM_SCHEME_JOINS: &str = "LEFT JOIN Teams t ON tm.teamid = t.id LEFT JOIN Schemes ts ON t.schemeid = ts.id";

fn opt(row: &PgRow, c: &str) -> String {
    row.try_get::<Option<String>, _>(c).ok().flatten().unwrap_or_default()
}

impl SchemeDefaults {
    pub fn from_row(row: &PgRow) -> SchemeDefaults {
        SchemeDefaults {
            team: [opt(row, "ts_g"), opt(row, "ts_u"), opt(row, "ts_a")],
            channel: [opt(row, "cs_g"), opt(row, "cs_u"), opt(row, "cs_a")],
        }
    }
}

/// A row with its scheme defaults.
pub struct WithScheme<T>(pub T, pub SchemeDefaults);

impl<'r, T: sqlx::FromRow<'r, PgRow>> sqlx::FromRow<'r, PgRow> for WithScheme<T> {
    fn from_row(row: &'r PgRow) -> sqlx::Result<Self> {
        Ok(WithScheme(T::from_row(row)?, SchemeDefaults::from_row(row)))
    }
}

/// `getTeamRoles` / `getChannelRoles`: scheme flags from legacy role names, explicit roles, and the roles with the
/// scheme's (or the built-in) guest, user and admin roles added.
fn derive(flags: [bool; 3], builtin: [&str; 3], defaults: [&str; 3], roles: &str) -> ([bool; 3], String, String) {
    let mut flags = flags;
    let mut out: Vec<&str> = vec![];
    let mut explicit: Vec<&str> = vec![];
    for r in roles.split_whitespace() {
        match builtin.iter().position(|b| *b == r) {
            Some(i) => flags[i] = true,
            None => {
                explicit.push(r);
                out.push(r);
            }
        }
    }
    for i in 0..3 {
        if flags[i] {
            let implied = if defaults[i].is_empty() { builtin[i] } else { defaults[i] };
            if !out.contains(&implied) {
                out.push(implied);
            }
        }
    }
    (flags, out.join(" "), explicit.join(" "))
}

fn pick<'a>(channel: &'a str, team: &'a str) -> &'a str {
    if channel.is_empty() { team } else { channel }
}

impl WithScheme<ChannelMember> {
    pub fn into_member(self) -> ChannelMember {
        let WithScheme(mut m, d) = self;
        let defaults =
            [pick(&d.channel[0], &d.team[0]), pick(&d.channel[1], &d.team[1]), pick(&d.channel[2], &d.team[2])];
        let (f, roles, explicit) = derive(
            [m.scheme_guest, m.scheme_user, m.scheme_admin],
            [CHANNEL_GUEST, CHANNEL_USER, CHANNEL_ADMIN],
            defaults,
            &m.roles,
        );
        [m.scheme_guest, m.scheme_user, m.scheme_admin] = f;
        (m.roles, m.explicit_roles) = (roles, explicit);
        m
    }
}

impl WithScheme<TeamMember> {
    pub fn into_member(self) -> TeamMember {
        let WithScheme(mut m, d) = self;
        let (f, roles, explicit) = derive(
            [m.scheme_guest, m.scheme_user, m.scheme_admin],
            [TEAM_GUEST, TEAM_USER, TEAM_ADMIN],
            [&d.team[0], &d.team[1], &d.team[2]],
            &m.roles,
        );
        [m.scheme_guest, m.scheme_user, m.scheme_admin] = f;
        (m.roles, m.explicit_roles) = (roles, explicit);
        m
    }
}

/// `SELECT` of channel members with their derived roles; append `WHERE ...` on `cm.`/`c.`.
pub fn channel_members_select() -> String {
    format!(
        "SELECT {}, {CHANNEL_SCHEME_COLS} FROM ChannelMembers cm JOIN Channels c ON c.id = cm.channelid {CHANNEL_SCHEME_JOINS}",
        db::cols::<ChannelMember>("cm")
    )
}

/// `SELECT` of team members with their derived roles; append `WHERE ...` on `tm.`.
pub fn team_members_select() -> String {
    format!("SELECT {}, {TEAM_SCHEME_COLS} FROM TeamMembers tm {TEAM_SCHEME_JOINS}", db::cols::<TeamMember>("tm"))
}

pub async fn channel_members(app: &App, cond: &str, args: &[&str]) -> ApiResult<Vec<ChannelMember>> {
    let sql = format!("{} WHERE {cond}", channel_members_select());
    let mut q = sqlx::query_as::<_, WithScheme<ChannelMember>>(sqlx::AssertSqlSafe(sql));
    for a in args {
        q = q.bind(*a);
    }
    Ok(q.fetch_all(&app.db).await?.into_iter().map(|w| w.into_member()).collect())
}

pub async fn team_members(app: &App, cond: &str, args: &[&str]) -> ApiResult<Vec<TeamMember>> {
    let sql = format!("{} WHERE {cond}", team_members_select());
    let mut q = sqlx::query_as::<_, WithScheme<TeamMember>>(sqlx::AssertSqlSafe(sql));
    for a in args {
        q = q.bind(*a);
    }
    Ok(q.fetch_all(&app.db).await?.into_iter().map(|w| w.into_member()).collect())
}

// ---------- roles ----------

/// Role name → (the built-in channel role it stands for, the higher scope's permissions).
pub type HigherScoped = Arc<HashMap<String, (String, Vec<String>)>>;

/// Channel-scoped permissions in `AllPermissions` order, and whether each is channel-moderated
/// (`ChannelModeratedPermissionsMap`); dumped from the Go model.
const CHANNEL_PERMISSIONS: &str = include_str!("../data/channel_permissions.txt");

/// `Role.MergeChannelHigherScopedPermissions`.
fn merged_permissions(role_id: &str, higher: &[String], own: &[String]) -> Vec<String> {
    let mut out = vec![];
    for line in CHANNEL_PERMISSIONS.lines() {
        let Some((id, moderated)) = line.split_once(' ') else { continue };
        let on_higher = higher.iter().any(|p| p == id);
        if role_id == CHANNEL_ADMIN && on_higher {
            out.push(id.to_string());
            continue;
        }
        if moderated == "true" {
            if on_higher && own.iter().any(|p| p == id) {
                out.push(id.to_string());
            }
        } else if on_higher {
            out.push(id.to_string());
        }
    }
    out
}

impl App {
    /// `GetRolesByNames` through the role cache.
    pub async fn roles(&self, names: &[&str]) -> ApiResult<Vec<Arc<Role>>> {
        let mut out = Vec::with_capacity(names.len());
        let mut missing = vec![];
        for n in names {
            match self.caches.role.get(*n) {
                Some(r) => out.push(r),
                None => missing.push(n.to_string()),
            }
        }
        if !missing.is_empty() {
            let sql = format!("{} WHERE name = ANY($1)", Role::SELECT);
            let found: Vec<Role> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(&missing).fetch_all(&self.db).await?;
            for r in found {
                let r = Arc::new(r);
                self.caches.role.put(r.name.clone(), r.clone());
                out.push(r);
            }
        }
        self.merge_channel_higher_scoped(out).await
    }

    /// `mergeChannelHigherScopedPermissions`: the roles of channel schemes keep a moderated permission only if
    /// the scheme's higher scope (the team's scheme, or the built-in channel roles) has it too.
    pub(crate) async fn merge_channel_higher_scoped(&self, roles: Vec<Arc<Role>>) -> ApiResult<Vec<Arc<Role>>> {
        let mut names: Vec<String> = roles.iter().filter(|r| r.scheme_managed).map(|r| r.name.clone()).collect();
        if names.is_empty() {
            return Ok(roles);
        }
        names.sort();
        let higher = self.channel_higher_scoped_permissions(names).await?;
        if higher.is_empty() {
            return Ok(roles);
        }
        Ok(roles
            .into_iter()
            .map(|r| match higher.get(&r.name).filter(|_| r.scheme_managed) {
                Some((role_id, perms)) => {
                    let mut m = (*r).clone();
                    m.permissions.0 = merged_permissions(role_id, perms, &r.permissions.0);
                    Arc::new(m)
                }
                None => r,
            })
            .collect())
    }

    /// `RoleStore.ChannelHigherScopedPermissions` through the role permissions cache, keyed by the sorted names.
    async fn channel_higher_scoped_permissions(&self, names: Vec<String>) -> ApiResult<HigherScoped> {
        let key = names.join("/");
        if let Some(m) = self.caches.role_permissions.get(&key) {
            return Ok(m);
        }
        // `channelHigherScopedPermissionsQuery`, the names bound instead of spliced in.
        let rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT '' , COALESCE(RoleSchemes.DefaultChannelUserRole, ''), COALESCE(RoleSchemes.DefaultChannelAdminRole, ''),
                    '', COALESCE(UserRoles.Permissions, ''), COALESCE(AdminRoles.Permissions, '')
             FROM Schemes AS RoleSchemes
                  JOIN Channels ON Channels.SchemeId = RoleSchemes.Id
                  JOIN Teams ON Teams.Id = Channels.TeamId
                  JOIN Schemes ON Schemes.Id = Teams.SchemeId
                  RIGHT JOIN Roles AS UserRoles ON UserRoles.Name = Schemes.DefaultChannelUserRole
                  RIGHT JOIN Roles AS AdminRoles ON AdminRoles.Name = Schemes.DefaultChannelAdminRole
             WHERE RoleSchemes.DefaultChannelUserRole = ANY($1) OR RoleSchemes.DefaultChannelAdminRole = ANY($1)
             UNION
             SELECT COALESCE(RoleSchemes.DefaultChannelGuestRole, ''), '', '', COALESCE(GuestRoles.Permissions, ''), '', ''
             FROM Schemes AS RoleSchemes
                  JOIN Channels ON Channels.SchemeId = RoleSchemes.Id
                  JOIN Teams ON Teams.Id = Channels.TeamId
                  JOIN Schemes ON Schemes.Id = Teams.SchemeId
                  RIGHT JOIN Roles AS GuestRoles ON GuestRoles.Name = Schemes.DefaultChannelGuestRole
             WHERE RoleSchemes.DefaultChannelGuestRole = ANY($1)
             UNION
             SELECT COALESCE(Schemes.DefaultChannelGuestRole, ''), COALESCE(Schemes.DefaultChannelUserRole, ''),
                    COALESCE(Schemes.DefaultChannelAdminRole, ''), COALESCE(GuestRoles.Permissions, ''),
                    COALESCE(UserRoles.Permissions, ''), COALESCE(AdminRoles.Permissions, '')
             FROM Schemes
                  JOIN Channels ON Channels.SchemeId = Schemes.Id
                  JOIN Teams ON Teams.Id = Channels.TeamId
                  JOIN Roles AS GuestRoles ON GuestRoles.Name = 'channel_guest'
                  JOIN Roles AS UserRoles ON UserRoles.Name = 'channel_user'
                  JOIN Roles AS AdminRoles ON AdminRoles.Name = 'channel_admin'
             WHERE (Schemes.DefaultChannelGuestRole = ANY($1) OR Schemes.DefaultChannelUserRole = ANY($1)
                    OR Schemes.DefaultChannelAdminRole = ANY($1))
               AND (Teams.SchemeId = '' OR Teams.SchemeId IS NULL)",
        )
        .bind(&names)
        .fetch_all(&self.db)
        .await?;
        let mut m = HashMap::new();
        let split = |p: &str| p.split(' ').map(str::to_string).collect::<Vec<_>>();
        for (g, u, a, gp, up, ap) in rows {
            m.insert(g, (CHANNEL_GUEST.to_string(), split(&gp)));
            m.insert(u, (CHANNEL_USER.to_string(), split(&up)));
            m.insert(a, (CHANNEL_ADMIN.to_string(), split(&ap)));
        }
        let m = Arc::new(m);
        self.caches.role_permissions.put(key, m.clone());
        Ok(m)
    }

    /// `RolesGrantPermission`.
    pub async fn roles_grant(&self, roles: &str, permission: &str) -> bool {
        let names: Vec<&str> = roles.split_whitespace().collect();
        if names.is_empty() {
            return false;
        }
        match self.roles(&names).await {
            Ok(rs) => rs.iter().any(|r| r.delete_at == 0 && r.permissions.0.iter().any(|p| p == permission)),
            Err(_) => false,
        }
    }

    /// `SessionHasPermissionTo`.
    pub async fn has_perm(&self, s: &Session, permission: &str) -> bool {
        self.roles_grant(&s.roles, permission).await
    }

    /// `SessionHasPermissionToTeam`.
    pub async fn has_team_perm(&self, s: &Session, team_id: &str, permission: &str) -> bool {
        if team_id.is_empty() {
            return false;
        }
        if let Some(tm) = s.team_members.iter().find(|t| t.team_id == team_id)
            && self.roles_grant(&tm.roles, permission).await
        {
            return true;
        }
        self.roles_grant(&s.roles, permission).await
    }

    /// `GetAllChannelMembersForUser` through its cache: channel id → roles with the scheme's roles added.
    pub async fn member_roles(&self, user_id: &str, include_deleted: bool) -> ApiResult<MemberRoles> {
        let key = if include_deleted { format!("{user_id}_deleted") } else { user_id.to_string() };
        if let Some(m) = self.caches.members_for_user.get(&key) {
            return Ok(m);
        }
        let _load = self.caches.members_for_user_loads.begin(&key).await;
        // Loaded by the flight this one waited for.
        if let Some(m) = self.caches.members_for_user.get(&key) {
            return Ok(m);
        }
        let mut sql = format!(
            "SELECT cm.channelid, cm.roles, cm.schemeguest, cm.schemeuser, cm.schemeadmin, {CHANNEL_SCHEME_COLS}
             FROM ChannelMembers cm JOIN Channels c ON cm.channelid = c.id {CHANNEL_SCHEME_JOINS} WHERE cm.userid = $1"
        );
        if !include_deleted {
            sql += " AND c.deleteat = 0";
        }
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(user_id).fetch_all(&self.db).await?;
        let mut m = HashMap::with_capacity(rows.len());
        for row in rows {
            let d = SchemeDefaults::from_row(&row);
            let flag = |c: &str| row.try_get::<Option<bool>, _>(c).ok().flatten().unwrap_or(false);
            let flags = [flag("schemeguest"), flag("schemeuser"), flag("schemeadmin")];
            let roles: String = db::col(&row, "roles")?;
            // `allChannelMember.Process`: only adds implied roles, keeps legacy names as they are.
            let mut out: Vec<String> = roles.split_whitespace().map(str::to_string).collect();
            for i in 0..3 {
                if flags[i] {
                    let implied = [pick(&d.channel[i], &d.team[i]), [CHANNEL_GUEST, CHANNEL_USER, CHANNEL_ADMIN][i]]
                        .into_iter()
                        .find(|r| !r.is_empty())
                        .unwrap()
                        .to_string();
                    if !out.contains(&implied) {
                        out.push(implied);
                    }
                }
            }
            m.insert(db::col::<String>(&row, "channelid")?, out.join(" "));
        }
        let m = Arc::new(m);
        self.caches.members_for_user.put(key, m.clone());
        Ok(m)
    }

    /// `SessionHasPermissionToChannel`: (allowed, is a member).
    pub async fn channel_perm(&self, s: &Session, channel_id: &str, permission: &str) -> ApiResult<(bool, bool)> {
        if channel_id.is_empty() {
            return Ok((false, false));
        }
        let Some(channel) = self.channel(channel_id).await? else { return Ok((false, false)) };
        let ids = self.member_roles(&s.user_id, true).await?;
        let mut is_member = false;
        if let Some(roles) = ids.get(channel_id) {
            is_member = true;
            if self.roles_grant(roles, permission).await {
                return Ok((true, true));
            }
        }
        if self.roles_grant(&s.roles, crate::perm::MANAGE_SYSTEM).await {
            return Ok((true, is_member));
        }
        if !channel.team_id.is_empty() {
            return Ok((self.has_team_perm(s, &channel.team_id, permission).await, is_member));
        }
        Ok((self.has_perm(s, permission).await, is_member))
    }

    /// The channel permission or 403.
    pub async fn require_channel(&self, s: &Session, channel_id: &str, permission: &str) -> ApiResult<()> {
        if self.channel_perm(s, channel_id, permission).await?.0 {
            Ok(())
        } else {
            Err(AppError::forbidden_perm(permission))
        }
    }
    pub async fn require_team(&self, s: &Session, team_id: &str, permission: &str) -> ApiResult<()> {
        if self.has_team_perm(s, team_id, permission).await {
            Ok(())
        } else {
            Err(AppError::forbidden_perm(permission))
        }
    }
    pub async fn require(&self, s: &Session, permission: &str) -> ApiResult<()> {
        if self.has_perm(s, permission).await { Ok(()) } else { Err(AppError::forbidden_perm(permission)) }
    }

    /// `SessionHasPermissionToUser`: system admins, oneself, or `edit_other_users` over a non-admin.
    pub async fn user_perm(&self, s: &Session, user_id: &str) -> bool {
        if user_id.is_empty() {
            return false;
        }
        if self.has_perm(s, MANAGE_SYSTEM).await || s.user_id == user_id {
            return true;
        }
        if !self.has_perm(s, EDIT_OTHER_USERS).await {
            return false;
        }
        matches!(self.user(user_id).await, Ok(Some(u)) if !u.is_system_admin())
    }
    pub async fn require_user(&self, s: &Session, user_id: &str) -> ApiResult<()> {
        if self.user_perm(s, user_id).await { Ok(()) } else { Err(AppError::forbidden_perm(EDIT_OTHER_USERS)) }
    }
}

// Permission ids (`model/permission.go`).
pub const MANAGE_SYSTEM: &str = "manage_system";
pub const EDIT_OTHER_USERS: &str = "edit_other_users";
pub const READ_CHANNEL: &str = "read_channel";
pub const READ_CHANNEL_CONTENT: &str = "read_channel_content";
pub const CREATE_POST: &str = "create_post";
pub const CREATE_POST_PUBLIC: &str = "create_post_public";
pub const EDIT_POST: &str = "edit_post";
pub const EDIT_OTHERS_POSTS: &str = "edit_others_posts";
pub const DELETE_POST: &str = "delete_post";
pub const DELETE_OTHERS_POSTS: &str = "delete_others_posts";
pub const ADD_REACTION: &str = "add_reaction";
pub const REMOVE_REACTION: &str = "remove_reaction";
pub const REMOVE_OTHERS_REACTIONS: &str = "remove_others_reactions";
pub const UPLOAD_FILE: &str = "upload_file";
pub const VIEW_TEAM: &str = "view_team";
pub const LIST_TEAM_CHANNELS: &str = "list_team_channels";
pub const JOIN_PUBLIC_CHANNELS: &str = "join_public_channels";
pub const READ_PUBLIC_CHANNEL: &str = "read_public_channel";
pub const READ_PRIVATE_CHANNEL_GROUPS: &str = "read_private_channel_groups";
pub const MANAGE_PUBLIC_CHANNEL_MEMBERS: &str = "manage_public_channel_members";
pub const MANAGE_PRIVATE_CHANNEL_MEMBERS: &str = "manage_private_channel_members";
pub const MANAGE_PUBLIC_CHANNEL_PROPERTIES: &str = "manage_public_channel_properties";
pub const MANAGE_PRIVATE_CHANNEL_PROPERTIES: &str = "manage_private_channel_properties";
pub const MANAGE_CHANNEL_ROLES: &str = "manage_channel_roles";
pub const CREATE_PUBLIC_CHANNEL: &str = "create_public_channel";
pub const CREATE_PRIVATE_CHANNEL: &str = "create_private_channel";
pub const CREATE_DIRECT_CHANNEL: &str = "create_direct_channel";
pub const CREATE_GROUP_CHANNEL: &str = "create_group_channel";
pub const DELETE_PUBLIC_CHANNEL: &str = "delete_public_channel";
pub const DELETE_PRIVATE_CHANNEL: &str = "delete_private_channel";
pub const CONVERT_PUBLIC_CHANNEL_TO_PRIVATE: &str = "convert_public_channel_to_private";
pub const MANAGE_TEAM: &str = "manage_team";
pub const ADD_USER_TO_TEAM: &str = "add_user_to_team";
pub const JOIN_PUBLIC_TEAMS: &str = "join_public_teams";
pub const JOIN_PRIVATE_TEAMS: &str = "join_private_teams";
pub const INVITE_USER: &str = "invite_user";
pub const LIST_USERS_WITHOUT_TEAM: &str = "list_users_without_team";
pub const VIEW_MEMBERS: &str = "view_members";
pub const CREATE_EMOJIS: &str = "create_emojis";
pub const DELETE_EMOJIS: &str = "delete_emojis";
pub const DELETE_OTHERS_EMOJIS: &str = "delete_others_emojis";
pub const USE_CHANNEL_MENTIONS: &str = "use_channel_mentions";
pub const USE_GROUP_MENTIONS: &str = "use_group_mentions";
pub const USE_SLASH_COMMANDS: &str = "use_slash_commands";
pub const GET_PUBLIC_LINK: &str = "get_public_link";
pub const MANAGE_OAUTH: &str = "manage_oauth";
pub const SYSCONSOLE_READ_USERMANAGEMENT_USERS: &str = "sysconsole_read_user_management_users";
pub const SYSCONSOLE_READ_PLUGINS: &str = "sysconsole_read_plugins";
pub const SYSCONSOLE_WRITE_PLUGINS: &str = "sysconsole_write_plugins";

// ---------- by user id (`HasPermissionTo*`), for checks not tied to the request's session ----------

impl App {
    /// `HasPermissionTo`: the user's system roles.
    pub async fn user_has_perm(&self, user_id: &str, permission: &str) -> bool {
        match self.user(user_id).await {
            Ok(Some(u)) => self.roles_grant(&u.roles, permission).await,
            _ => false,
        }
    }

    /// `HasPermissionToTeam`: the team membership's roles, then the system roles.
    pub async fn user_team_perm(&self, user_id: &str, team_id: &str, permission: &str) -> bool {
        if team_id.is_empty() || user_id.is_empty() {
            return false;
        }
        if let Ok(m) = team_members(self, "tm.teamid = $1 AND tm.userid = $2", &[team_id, user_id]).await
            && let Some(m) = m.into_iter().next()
            && m.delete_at == 0
            && self.roles_grant(&m.roles, permission).await
        {
            return true;
        }
        self.user_has_perm(user_id, permission).await
    }

    /// `HasPermissionToChannel`: (allowed, is a member).
    pub async fn user_channel_perm(&self, user_id: &str, channel_id: &str, permission: &str) -> (bool, bool) {
        if channel_id.is_empty() || user_id.is_empty() {
            return (false, false);
        }
        let mut is_member = false;
        if let Ok(ids) = self.member_roles(user_id, true).await
            && let Some(roles) = ids.get(channel_id)
        {
            is_member = true;
            if self.roles_grant(roles, permission).await {
                return (true, true);
            }
        }
        match self.channel(channel_id).await {
            Ok(Some(c)) if !c.team_id.is_empty() => {
                (self.user_team_perm(user_id, &c.team_id, permission).await, is_member)
            }
            _ => (self.user_has_perm(user_id, permission).await, is_member),
        }
    }

    /// `HasPermissionToResolveChannelMention`: read content, or a public channel of a team the user may read.
    pub async fn user_resolve_mention(&self, user_id: &str, channel_id: &str, kind: &str, team_id: &str) -> bool {
        self.user_channel_perm(user_id, channel_id, READ_CHANNEL_CONTENT).await.0
            || (kind == crate::model::channel::OPEN || kind == crate::model::channel::OPEN_BOARD)
                && self.user_team_perm(user_id, team_id, READ_PUBLIC_CHANNEL).await
    }

    /// `HasPermissionToReadChannel`: read content, or a public channel of a team the user may read.
    pub async fn user_read_channel(&self, user_id: &str, c: &crate::model::Channel) -> (bool, bool) {
        let (ok, member) = self.user_channel_perm(user_id, &c.id, READ_CHANNEL_CONTENT).await;
        if ok {
            return (true, member);
        }
        if (c.r#type == crate::model::channel::OPEN || c.r#type == crate::model::channel::OPEN_BOARD)
            && !self.cfg.bool("ComplianceSettings.Enable")
        {
            return (self.user_team_perm(user_id, &c.team_id, READ_PUBLIC_CHANNEL).await, false);
        }
        (false, false)
    }
}
