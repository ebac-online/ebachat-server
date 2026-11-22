//! Post search in the database (Postgres full text), as Mattermost does it without a search engine.

// Based on Mattermost server/channels/api4/post.go (searchPosts), app/post.go (SearchPostsForUser),
// public/model/search_params.go and store/sqlstore/post_store.go (search), Copyright (c) 2015-present
// Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::extract::{Path, State as St};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use regex::Regex;
use serde::Deserialize;

use crate::app::{ApiResult, App, AppError, BusyAuth, State, is_valid_id};
use crate::db;
use crate::model::{Post, PostList, Session};
use crate::perm;
use crate::posts::WithCount;

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/posts/search", post(search_all))
        .route("/api/v4/teams/{team_id}/posts/search", post(search_team))
}

/// Go's default `per_page` for post search.
const SEARCH_PER_PAGE: i64 = 60;
/// The most posts one search page answers.
const SEARCH_MAX_PER_PAGE: i64 = 1000;
/// How deep paging goes: the newest this many matches.
const SEARCH_MAX_WINDOW: i64 = 10_000;

/// A value bound to the search query, in placeholder order.
enum Bind {
    Text(String),
    Arr(Vec<String>),
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct SearchBody {
    terms: Option<String>,
    is_or_search: Option<bool>,
    time_zone_offset: Option<i64>,
    include_deleted_channels: Option<bool>,
    page: Option<i64>,
    per_page: Option<i64>,
}

/// `SearchParams`.
#[derive(Default, Clone, Debug)]
pub struct Params {
    pub terms: String,
    pub excluded_terms: String,
    pub is_hashtag: bool,
    pub in_channels: Vec<String>,
    pub excluded_channels: Vec<String>,
    pub from_users: Vec<String>,
    pub excluded_users: Vec<String>,
    pub after_date: String,
    pub excluded_after_date: String,
    pub before_date: String,
    pub excluded_before_date: String,
    pub on_date: String,
    pub excluded_date: String,
    pub or_terms: bool,
    pub include_deleted_channels: bool,
    pub tz_offset: i64,
}

static PUNC_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^[^\pL\d\s#"]+"#).unwrap());
static PUNC_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[^\pL\p{M}\d\s*"]+$"#).unwrap());
static HASHTAG_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#{2,}").unwrap());
static VALID_HASHTAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#\pL[\pL\d\-_.]*[\pL\d])$").unwrap());
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"("[^"]*")"#).unwrap());
static WILDCARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*($| )").unwrap());
static NUMERIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+(:\*)?$").unwrap());

/// `splitWords`: quoted phrases kept together.
fn split_words(text: &str) -> Vec<String> {
    let mut words = vec![];
    let mut in_quote = false;
    let mut loc = 0;
    let b = text.as_bytes();
    for (i, c) in text.char_indices() {
        if c == '"' {
            if in_quote {
                words.push(text[loc..=i].to_string());
                in_quote = false;
                loc = i + 1;
            } else {
                let start = if i > 0 && b[i - 1] == b'-' { i - 1 } else { i };
                words.extend(text[loc..start].split_whitespace().map(str::to_string));
                in_quote = true;
                loc = start;
            }
        }
    }
    words.extend(text[loc..].split_whitespace().map(str::to_string));
    words
}

/// `ParseSearchParams`.
pub fn parse(text: &str, tz: i64) -> Vec<Params> {
    let input = split_words(text);
    let mut words: Vec<(String, bool)> = vec![];
    let mut flags: Vec<(&str, String, bool)> = vec![];
    let mut skip = false;
    const FLAGS: [&str; 7] = ["from", "channel", "in", "before", "after", "on", "ext"];
    for (i, word) in input.iter().enumerate() {
        if skip {
            skip = false;
            continue;
        }
        let mut is_flag = false;
        if let Some(colon) = word.find(':') {
            let (name, exclude) = match word.strip_prefix('-') {
                Some(w) => (&w[..colon - 1], true),
                None => (&word[..colon], false),
            };
            let value = &word[colon + 1..];
            for f in FLAGS {
                if name.eq_ignore_ascii_case(f) {
                    if !value.is_empty() {
                        flags.push((f, value.to_string(), exclude));
                        is_flag = true;
                    } else if i < input.len() - 1 {
                        flags.push((f, input[i + 1].clone(), exclude));
                        skip = true;
                        is_flag = true;
                    }
                    if is_flag {
                        break;
                    }
                }
            }
        }
        if !is_flag {
            let exclude = word.starts_with('-');
            let w = PUNC_START.replace_all(word, "");
            let w = PUNC_END.replace_all(&w, "");
            let w = HASHTAG_START.replace_all(&w, "#").into_owned();
            if !w.is_empty() {
                words.push((w, exclude));
            }
        }
    }
    let (mut tags, mut ex_tags, mut plain, mut ex_plain) = (vec![], vec![], vec![], vec![]);
    for (w, ex) in words {
        match (VALID_HASHTAG.is_match(&w), ex) {
            (true, true) => ex_tags.push(w),
            (true, false) => tags.push(w),
            (false, true) => ex_plain.push(w),
            (false, false) => plain.push(w),
        }
    }
    let mut base = Params { tz_offset: tz, ..Default::default() };
    for (name, value, ex) in flags {
        match (name, ex) {
            ("in" | "channel", false) => base.in_channels.push(value),
            ("in" | "channel", true) => base.excluded_channels.push(value),
            ("from", false) => base.from_users.push(value),
            ("from", true) => base.excluded_users.push(value),
            ("after", false) => base.after_date = value,
            ("after", true) => base.excluded_after_date = value,
            ("before", false) => base.before_date = value,
            ("before", true) => base.excluded_before_date = value,
            ("on", false) => base.on_date = value,
            ("on", true) => base.excluded_date = value,
            _ => {}
        }
    }
    let mut out = vec![];
    let (p, xp, t, xt) = (plain.join(" "), ex_plain.join(" "), tags.join(" "), ex_tags.join(" "));
    if !p.is_empty() || !xp.is_empty() {
        out.push(Params { terms: p.clone(), excluded_terms: xp.clone(), ..base.clone() });
    }
    if !t.is_empty() || !xt.is_empty() {
        out.push(Params { terms: t.clone(), excluded_terms: xt.clone(), is_hashtag: true, ..base.clone() });
    }
    let has_filters = !base.in_channels.is_empty()
        || !base.from_users.is_empty()
        || !base.excluded_channels.is_empty()
        || !base.excluded_users.is_empty()
        || !base.after_date.is_empty()
        || !base.excluded_after_date.is_empty()
        || !base.before_date.is_empty()
        || !base.excluded_before_date.is_empty()
        || !base.on_date.is_empty()
        || !base.excluded_date.is_empty();
    if p.is_empty() && t.is_empty() && xp.is_empty() && xt.is_empty() && has_filters {
        out.push(base);
    }
    out
}

/// Days since 1970-01-01 of a civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// A `YYYY-M-D` date's day number, or None.
fn parse_day(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

fn start_of_day(day: i64, tz: i64) -> i64 {
    (day * 86400 - tz) * 1000
}

fn end_of_day(day: i64, tz: i64) -> i64 {
    start_of_day(day, tz) + 86_399_999
}

fn today() -> i64 {
    crate::app::now_ms().div_euclid(86_400_000)
}

/// `neutralizeNonWordHyphens`.
fn neutralize_hyphens(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let word = |c: char| c.is_alphanumeric();
    c.iter()
        .enumerate()
        .map(
            |(i, &ch)| {
                if ch == '-' && !(i > 0 && word(c[i - 1]) && i + 1 < c.len() && word(c[i + 1])) { ' ' } else { ch }
            },
        )
        .collect()
}

/// `expandNumericTerms`.
fn expand_numeric(s: &str) -> String {
    let mut in_quote = false;
    s.split_whitespace()
        .map(|f| {
            let out = if !in_quote && NUMERIC.is_match(f) { format!("({f}|-{f})") } else { f.to_string() };
            if f.matches('"').count() % 2 == 1 {
                in_quote = !in_quote;
            }
            out
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `removeNonAlphaNumericUnquotedTerms`.
fn drop_symbol_terms(s: &str) -> String {
    s.split(' ')
        .filter(|w| (w.len() >= 2 && w.starts_with('"') && w.ends_with('"')) || w.chars().any(char::is_alphanumeric))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

impl App {
    /// The `WHERE` group of one set of params (`SqlPostStore.search`), its values pushed to `binds`; None when the set
    /// asks for nothing. `allowed` keeps the channel permission answers across sets.
    async fn search_cond(
        &self,
        team_id: &str,
        user_id: &str,
        p: &Params,
        binds: &mut Vec<Bind>,
        allowed: &mut HashMap<String, bool>,
    ) -> ApiResult<Option<String>> {
        if p.terms.is_empty()
            && p.excluded_terms.is_empty()
            && p.in_channels.is_empty()
            && p.excluded_channels.is_empty()
            && p.from_users.is_empty()
            && p.excluded_users.is_empty()
            && p.on_date.is_empty()
            && p.after_date.is_empty()
            && p.before_date.is_empty()
        {
            return Ok(None);
        }
        fn put(b: Bind, binds: &mut Vec<Bind>) -> String {
            binds.push(b);
            format!("${}", binds.len())
        }
        let first = binds.len();
        let mut conds: Vec<String> = vec![];
        // The channels searched: the user's (as Go's subquery), then FilterPostsByChannelPermissions, so a page is cut
        // in SQL from posts the user may read and the OFFSET is exact.
        let channels = self.searchable_channels(team_id, user_id, p, allowed).await?;
        conds.push(format!("q2.ChannelId = ANY({})", put(Bind::Arr(channels), binds)));
        if !p.from_users.is_empty() || !p.excluded_users.is_empty() {
            let mut sub = if team_id.is_empty() {
                "SELECT Id FROM Users WHERE true".to_string()
            } else {
                format!(
                    "SELECT Users.Id FROM Users, TeamMembers WHERE TeamMembers.TeamId = {} AND Users.Id = TeamMembers.UserId",
                    put(Bind::Text(team_id.into()), binds)
                )
            };
            if !p.from_users.is_empty() {
                sub += &format!(" AND Users.Id = ANY({})", put(Bind::Arr(p.from_users.clone()), binds));
            }
            if !p.excluded_users.is_empty() {
                sub += &format!(" AND NOT (Users.Id = ANY({}))", put(Bind::Arr(p.excluded_users.clone()), binds));
            }
            conds.push(format!("q2.UserId IN ({sub})"));
        }
        let tz = p.tz_offset;
        if !p.on_date.is_empty() {
            let (a, b) = parse_day(&p.on_date).map(|d| (start_of_day(d, tz), end_of_day(d, tz))).unwrap_or((0, 0));
            conds.push(format!("q2.CreateAt BETWEEN {a} AND {b}"));
        } else {
            if !p.excluded_date.is_empty() {
                let (a, b) =
                    parse_day(&p.excluded_date).map(|d| (start_of_day(d, tz), end_of_day(d, tz))).unwrap_or((0, 0));
                conds.push(format!("q2.CreateAt NOT BETWEEN {a} AND {b}"));
            }
            if !p.after_date.is_empty() {
                let d = parse_day(&p.after_date).unwrap_or_else(today) + 1;
                conds.push(format!("q2.CreateAt >= {}", start_of_day(d, tz)));
            }
            if !p.before_date.is_empty() {
                let v = parse_day(&p.before_date).map(|d| end_of_day(d - 1, tz)).unwrap_or(0);
                conds.push(format!("q2.CreateAt <= {v}"));
            }
            if !p.excluded_after_date.is_empty() {
                let d = parse_day(&p.excluded_after_date).unwrap_or_else(today) + 1;
                conds.push(format!("q2.CreateAt < {}", start_of_day(d, tz)));
            }
            if !p.excluded_before_date.is_empty() {
                let v = parse_day(&p.excluded_before_date).map(|d| end_of_day(d - 1, tz)).unwrap_or(0);
                conds.push(format!("q2.CreateAt > {v}"));
            }
        }
        let search_type = if p.is_hashtag { "Hashtags" } else { "Message" };
        let mut terms = p.terms.clone();
        let mut excluded = p.excluded_terms.clone();
        for c in ["<", ">", "+", "(", ")", "~", ":"] {
            if !p.is_hashtag {
                terms = terms.replace(c, " ");
            }
            excluded = excluded.replace(c, " ");
        }
        if !(terms.is_empty() && excluded.is_empty()) {
            let terms = expand_numeric(&WILDCARD.replace_all(&neutralize_hyphens(&terms), ":* "));
            let excluded = expand_numeric(&WILDCARD.replace_all(&neutralize_hyphens(&excluded), ":* "));
            let spaces = |s: &str, or: bool| -> String {
                if s.is_empty() {
                    return String::new();
                }
                let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
                let s = QUOTED.replace_all(&s, |c: &regex::Captures| c[0].replace(' ', "<->"));
                s.replace(' ', if or { "|" } else { "&" })
            };
            let mut q = spaces(&terms, p.or_terms);
            let ex = spaces(&excluded, true);
            if !ex.is_empty() {
                q += &format!(" &!({ex})");
            }
            // Go runs each set as its own query and a set whose query fails (a tsquery Postgres rejects, as `-#tag`
            // alone gives) logs it and finds nothing, the other sets still do: here the set's group is dropped.
            // The database's default_text_search_config, which its full-text indexes are built with.
            let cfg = crate::file_api::text_search_config(self).await;
            let check = format!("SELECT to_tsquery('{cfg}', $1)");
            if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(check)).bind(&q).execute(&self.db).await {
                eprintln!("query error searching posts: {e}");
                binds.truncate(first);
                return Ok(None);
            }
            let q = put(Bind::Text(q), binds);
            conds.push(format!("to_tsvector('{cfg}', q2.{search_type}) @@ to_tsquery('{cfg}', {q})"));
        }
        if p.is_hashtag {
            // Go keeps a post only if one of its hashtags is one of the terms, case-insensitively (strings.ToUpper);
            // here Postgres' upper() on both sides. Both split on every space as Go's strings.Split, so "" splits to
            // [""] (string_to_array gives {} for it).
            let tags: Vec<String> = p.terms.split(' ').map(str::to_string).collect();
            conds.push(format!(
                "EXISTS (SELECT 1 FROM unnest(CASE WHEN q2.Hashtags = '' THEN ARRAY[''] ELSE string_to_array(q2.Hashtags, ' ') END) t \
                 WHERE upper(t) IN (SELECT upper(x) FROM unnest({}::text[]) x))",
                put(Bind::Arr(tags), binds)
            ));
        }
        Ok(Some(conds.join(" AND ")))
    }

    /// The channels one set of params searches: the user's channels (not archived unless asked, of the team or
    /// teamless, `in:` and `-in:`), less those the user may not read (`FilterPostsByChannelPermissions`).
    async fn searchable_channels(
        &self,
        team_id: &str,
        user_id: &str,
        p: &Params,
        allowed: &mut HashMap<String, bool>,
    ) -> ApiResult<Vec<String>> {
        let mut sql =
            "SELECT Id FROM Channels, ChannelMembers WHERE Id = ChannelId AND ChannelMembers.UserId = $1".to_string();
        let mut n = 1;
        if !p.include_deleted_channels {
            sql += " AND Channels.DeleteAt = 0";
        }
        if !team_id.is_empty() {
            n += 1;
            sql += &format!(" AND (TeamId = ${n} OR TeamId = '')");
        }
        if !p.in_channels.is_empty() {
            n += 1;
            sql += &format!(" AND Id = ANY(${n})");
        }
        if !p.excluded_channels.is_empty() {
            n += 1;
            sql += &format!(" AND NOT (Id = ANY(${n}))");
        }
        let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql)).bind(user_id);
        if !team_id.is_empty() {
            q = q.bind(team_id);
        }
        if !p.in_channels.is_empty() {
            q = q.bind(&p.in_channels);
        }
        if !p.excluded_channels.is_empty() {
            q = q.bind(&p.excluded_channels);
        }
        let ids = q.fetch_all(&self.db).await?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let ok = match allowed.get(&id) {
                Some(v) => *v,
                None => {
                    let v = match self.channel(&id).await? {
                        Some(c) => self.user_read_channel(user_id, &c).await.0,
                        None => false,
                    };
                    allowed.insert(id.clone(), v);
                    v
                }
            };
            if ok {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// `SearchPostsForUser`, paged for real (see below).
    async fn search_posts(&self, s: &Session, team_id: &str, b: SearchBody) -> ApiResult<PostList> {
        let terms = b.terms.unwrap_or_default();
        let params = parse(terms.trim(), b.time_zone_offset.unwrap_or(0));
        if !self.cfg.bool("ServiceSettings.EnablePostSearch") {
            return Err(AppError::new(
                StatusCode::NOT_IMPLEMENTED,
                "store.sql_post.search.disabled",
                "Searching has been disabled on this server.",
            ));
        }
        let mut list = PostList::default();
        // Pages for real, unlike Go's database search (LIMIT 100 whatever per_page asks, and nothing at all past
        // page 0), so matches older than the newest hundred can be reached. The answer keeps Go's shape and the
        // clients' paging works unchanged: the web app asks page+1 until a page comes back empty, EBAChat until one
        // is shorter than per_page (docs/SEARCH.md).
        let page = b.page.unwrap_or(0).max(0);
        let per_page = match b.per_page.unwrap_or(SEARCH_PER_PAGE) {
            n if n <= 0 => SEARCH_PER_PAGE,
            n => n.min(SEARCH_MAX_PER_PAGE),
        };
        let skip = page.saturating_mul(per_page);
        if skip >= SEARCH_MAX_WINDOW {
            return Ok(list);
        }
        let limit = per_page.min(SEARCH_MAX_WINDOW - skip);
        // Every set of terms is one OR'ed group of a single query, and every filter Go applies after its query
        // (channel permissions, hashtag match, burn-on-read) is in the query too: the page is cut by LIMIT/OFFSET.
        let mut binds: Vec<Bind> = vec![];
        let mut groups: Vec<String> = vec![];
        let mut allowed: HashMap<String, bool> = HashMap::new();
        for mut p in params {
            if p.terms == "*" {
                continue;
            }
            p.or_terms = b.is_or_search.unwrap_or(false);
            p.include_deleted_channels = b.include_deleted_channels.unwrap_or(false);
            p.in_channels =
                self.channel_names_to_ids(&p.in_channels, &s.user_id, team_id, p.include_deleted_channels).await;
            p.excluded_channels =
                self.channel_names_to_ids(&p.excluded_channels, &s.user_id, team_id, p.include_deleted_channels).await;
            p.from_users = self.usernames_to_ids(&p.from_users).await;
            p.excluded_users = self.usernames_to_ids(&p.excluded_users).await;
            p.terms = drop_symbol_terms(&p.terms);
            if let Some(g) = self.search_cond(team_id, &s.user_id, &p, &mut binds, &mut allowed).await? {
                groups.push(format!("({g})"));
            }
        }
        if groups.is_empty() {
            return Ok(list);
        }
        // The matches always come through the full-text index (idx_posts_message_txt, idx_posts_hashtags_txt), then
        // are sorted: a cost bound by the number of matches on every page. Left to itself the planner may instead walk
        // CreateAt from the newest post computing each one's tsvector until a page of matches turns up, quick for a
        // word all over the recent posts but a scan of the whole table for one whose matches are all old (a word in
        // 30 000 old posts of the bench: 900 ms against 30 ms). MATERIALIZED keeps the two apart.
        // The reply count only for the page's rows.
        let sql = format!(
            "WITH m AS MATERIALIZED (
                 SELECT q2.Id, q2.CreateAt FROM Posts q2
                 WHERE q2.DeleteAt = 0 AND q2.Type NOT LIKE 'system_%' AND q2.Type != 'card' AND q2.Type != '{}' AND ({}))
             SELECT {}, (SELECT COUNT(*) FROM Posts WHERE Posts.RootId = (CASE WHEN q2.RootId = '' THEN q2.Id ELSE q2.RootId END) AND Posts.DeleteAt = 0) AS replycount
             FROM Posts q2 JOIN (SELECT Id FROM m ORDER BY CreateAt DESC, Id LIMIT {limit} OFFSET {skip}) pg ON q2.Id = pg.Id
             ORDER BY q2.CreateAt DESC, q2.Id",
            crate::model::post_kind::BURN_ON_READ,
            groups.join(" OR "),
            db::cols::<Post>("q2"),
        );
        let mut q = sqlx::query_as::<_, WithCount>(sqlx::AssertSqlSafe(sql));
        for b in &binds {
            q = match b {
                Bind::Text(t) => q.bind(t),
                Bind::Arr(a) => q.bind(a),
            };
        }
        match q.fetch_all(&self.db).await {
            Ok(posts) => {
                for w in posts {
                    list.add_ordered(w.0);
                }
            }
            Err(e) => eprintln!("query error searching posts: {e}"),
        }
        Ok(list)
    }

    /// `convertChannelNamesToChannelIds`: `~name`, `@user` (an existing direct channel), or as given.
    async fn channel_names_to_ids(
        &self,
        names: &[String],
        user_id: &str,
        team_id: &str,
        include_deleted: bool,
    ) -> Vec<String> {
        let mut out = vec![];
        for n in names {
            let clean = n.trim_start_matches('~');
            let id: Option<String> = if let Some(u) = clean.strip_prefix('@') {
                if u.contains(',') {
                    None
                } else {
                    let other: Option<String> = sqlx::query_scalar("SELECT Id FROM Users WHERE Username = $1")
                        .bind(u)
                        .fetch_optional(&self.db)
                        .await
                        .ok()
                        .flatten();
                    match other {
                        Some(o) => sqlx::query_scalar("SELECT Id FROM Channels WHERE Name = $1 AND Type = 'D'")
                            .bind(crate::model::channel::direct_name(user_id, &o))
                            .fetch_optional(&self.db)
                            .await
                            .ok()
                            .flatten(),
                        None => None,
                    }
                }
            } else {
                let del = if include_deleted { "" } else { " AND DeleteAt = 0" };
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT Id FROM Channels WHERE Name = $1 AND (TeamId = $2 OR TeamId = ''){del}"
                )))
                .bind(clean)
                .bind(team_id)
                .fetch_optional(&self.db)
                .await
                .ok()
                .flatten()
            };
            out.push(id.unwrap_or_else(|| n.clone()));
        }
        out
    }

    async fn usernames_to_ids(&self, names: &[String]) -> Vec<String> {
        let mut out = vec![];
        for n in names {
            let u = n.trim_start_matches('@').to_lowercase();
            let id: Option<String> = sqlx::query_scalar("SELECT Id FROM Users WHERE Username = $1")
                .bind(&u)
                .fetch_optional(&self.db)
                .await
                .ok()
                .flatten();
            out.push(id.unwrap_or_else(|| n.clone()));
        }
        out
    }
}

async fn search(app: &State, s: &Session, team_id: &str, b: SearchBody) -> ApiResult<Response> {
    if b.terms.as_deref().unwrap_or("").is_empty() {
        return Err(AppError::invalid_param("terms"));
    }
    let list = app.search_posts(s, team_id, b).await?;
    let list = app.prepare_list(list).await;
    let list = app.sanitize_list_for_user(list, &s.user_id).await;
    // `PostSearchResults`: the PostList, then `matches` (none from the database search).
    #[derive(serde::Serialize)]
    struct Results<'a> {
        #[serde(flatten)]
        list: &'a crate::model::post::PostList,
        matches: Option<()>,
    }
    let mut r = Json(Results { list: &list, matches: None }).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-store, must-revalidate"));
    Ok(r)
}

fn body(raw: &[u8]) -> ApiResult<SearchBody> {
    serde_json::from_slice(raw).map_err(|e| {
        AppError::bad_request("api.post.search_posts.invalid_body.app_error", "Unable to parse the body.")
            .detail(e.to_string())
    })
}

async fn search_all(St(app): St<State>, BusyAuth(s): BusyAuth, raw: axum::body::Bytes) -> ApiResult<Response> {
    search(&app, &s, "", body(&raw)?).await
}

async fn search_team(
    St(app): St<State>,
    BusyAuth(s): BusyAuth,
    Path(team_id): Path<String>,
    raw: axum::body::Bytes,
) -> ApiResult<Response> {
    if !is_valid_id(&team_id) {
        return Err(AppError::invalid_url_param("team_id"));
    }
    if !app.has_team_perm(&s, &team_id, perm::VIEW_TEAM).await {
        return Err(AppError::forbidden_perm(perm::VIEW_TEAM));
    }
    search(&app, &s, &team_id, body(&raw)?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_terms_and_hashtags() {
        let p = parse("hello -world in:town-square from:@bob #tag \"exact phrase\"", 0);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].terms, "hello \"exact phrase\"");
        assert_eq!(p[0].excluded_terms, "world");
        assert_eq!(p[0].in_channels, vec!["town-square"]);
        assert_eq!(p[0].from_users, vec!["@bob"]);
        assert!(p[1].is_hashtag && p[1].terms == "#tag");
    }

    #[test]
    fn day_bounds() {
        let d = parse_day("2024-1-2").unwrap();
        assert_eq!(start_of_day(d, 0), 1_704_153_600_000);
    }
}
