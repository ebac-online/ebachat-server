//! Mattermost's in-memory caches in front of the database: the store's `localcachelayer` and the platform's
//! session and status caches, with Go's sizes and lifetimes. Writes invalidate them; there is one node, so no
//! cluster messages.

// Based on Mattermost server/channels/store/localcachelayer/layer.go and channels/app/platform/service.go,
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::{Channel, Emoji, FileInfo, Reaction, Role, Scheme, Session, Status, StringMap, User};

/// An LRU of `size` entries that also drop `ttl` after they were put, as Go's `cache.LRU`.
pub struct Lru<K: Hash + Eq, V> {
    m: Mutex<lru::LruCache<K, (V, Instant)>>,
    ttl: Option<Duration>,
    /// Go's `MemCacheHit`, `MemCacheMiss` and `MemCacheInvalidation` counters.
    hits: AtomicU64,
    misses: AtomicU64,
    invalidations: AtomicU64,
}

/// A cache's size and counters.
#[derive(serde::Serialize)]
pub struct Stats {
    pub len: usize,
    pub hits: u64,
    pub misses: u64,
    pub invalidations: u64,
}

impl<K: Hash + Eq, V: Clone> Lru<K, V> {
    pub fn new(size: usize, ttl: Option<Duration>) -> Self {
        Lru {
            m: Mutex::new(lru::LruCache::new(NonZeroUsize::new(size).unwrap())),
            ttl,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            invalidations: AtomicU64::new(0),
        }
    }
    pub fn stats(&self) -> Stats {
        Stats {
            len: self.m.lock().unwrap().len(),
            hits: self.hits.load(Relaxed),
            misses: self.misses.load(Relaxed),
            invalidations: self.invalidations.load(Relaxed),
        }
    }
    pub fn get<Q>(&self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut m = self.m.lock().unwrap();
        let Some((v, at)) = m.get(k) else {
            self.misses.fetch_add(1, Relaxed);
            return None;
        };
        if self.ttl.is_some_and(|t| at.elapsed() > t) {
            m.pop(k);
            self.misses.fetch_add(1, Relaxed);
            return None;
        }
        self.hits.fetch_add(1, Relaxed);
        Some(v.clone())
    }
    pub fn put(&self, k: K, v: V) {
        self.m.lock().unwrap().put(k, (v, Instant::now()));
    }
    /// An entry that lives `ttl` (shorter than the cache's own), as Go's `SetWithExpiry`.
    pub fn put_for(&self, k: K, v: V, ttl: Duration) {
        let at = match self.ttl {
            Some(t) if t > ttl => Instant::now().checked_sub(t - ttl).unwrap_or_else(Instant::now),
            _ => Instant::now(),
        };
        self.m.lock().unwrap().put(k, (v, at));
    }
    pub fn remove<Q>(&self, k: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.invalidations.fetch_add(1, Relaxed);
        self.m.lock().unwrap().pop(k);
    }
    pub fn clear(&self) {
        self.invalidations.fetch_add(1, Relaxed);
        self.m.lock().unwrap().clear();
    }
    /// The values that have not expired, as Go's `Scan` with `GetMulti`.
    pub fn values(&self) -> Vec<V> {
        let m = self.m.lock().unwrap();
        m.iter()
            .filter(|(_, (_, at))| !self.ttl.is_some_and(|t| at.elapsed() > t))
            .map(|(_, (v, _))| v.clone())
            .collect()
    }
    /// Drops every entry `f` says to.
    pub fn remove_where(&self, f: impl Fn(&K, &V) -> bool)
    where
        K: Clone,
    {
        let mut m = self.m.lock().unwrap();
        let gone: Vec<K> = m.iter().filter(|(k, (v, _))| f(k, v)).map(|(k, _)| k.clone()).collect();
        self.invalidations.fetch_add(gone.len() as u64, Relaxed);
        for k in gone {
            m.pop(&k);
        }
    }
}

const MIN: Duration = Duration::from_secs(60);
const SESSION_CACHE_SIZE: usize = 35000;
const CHANNEL_CACHE_SIZE: usize = 25000;

/// A user's channel memberships: channel id → roles (`GetAllChannelMembersForUser`).
pub type MemberRoles = Arc<HashMap<String, String>>;

/// One load at a time per key: callers that miss while a load is under way wait for it and read what it put,
/// instead of each asking the database. Without it a slow database turns every miss into one more query (each
/// `posted` delivery checks the recipient's memberships), and the queries that pile up keep it slow: the members
/// cache never filled again at 800 req/s once Postgres had stalled. Go loads memberships once per connection in
/// its hub, so it has no such pile-up.
#[derive(Default)]
pub struct Flights(Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

pub struct Flight<'a> {
    flights: &'a Flights,
    key: String,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl Flights {
    /// Waits for any load of `key` under way, then holds the key until the returned flight is dropped.
    pub async fn begin(&self, key: &str) -> Flight<'_> {
        let lock = self.0.lock().unwrap().entry(key.to_string()).or_default().clone();
        let _guard = lock.lock_owned().await;
        Flight { flights: self, key: key.to_string(), _guard }
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut m = self.flights.0.lock().unwrap();
        // The map's and this guard's references only: nobody waits, the key can go.
        if m.get(&self.key).is_some_and(|l| Arc::strong_count(l) <= 2) {
            m.remove(&self.key);
        }
    }
}

pub struct Caches {
    // platform
    pub session: Lru<String, Arc<Session>>,
    pub status: Lru<String, Status>,
    // app
    pub seen_pending_post_ids: Lru<String, String>,
    /// `platform.LinkCache`: link metadata by `{hash(url, hour):x}` (see `opengraph.rs`).
    pub open_graph: Lru<String, Arc<crate::file_api::LinkMeta>>,
    // store: users
    pub user: Lru<String, Arc<User>>,
    pub all_profiles: Lru<(), Arc<Vec<Arc<User>>>>,
    pub profiles_in_channel: Lru<String, Arc<HashMap<String, Arc<User>>>>,
    pub profiles_in_channel_loads: Flights,
    // store: channels
    pub channel: Lru<String, Arc<Channel>>,
    pub channel_by_name: Lru<String, Arc<Channel>>,
    pub member_count: Lru<String, i64>,
    pub guest_count: Lru<String, i64>,
    pub pinned_count: Lru<String, i64>,
    pub members_for_user: Lru<String, MemberRoles>,
    pub members_for_user_loads: Flights,
    pub members_notify_props: Lru<String, Arc<HashMap<String, StringMap>>>,
    // store: posts
    pub last_post_time: Lru<String, i64>,
    /// First pages (`{channel}{30|60}`) as `GetPosts` returned them.
    pub last_posts: Lru<String, Arc<serde_json::Value>>,
    // store: teams
    pub team_ids_for_user: Lru<String, Arc<Vec<String>>>,
    // store: roles, schemes
    pub role: Lru<String, Arc<Role>>,
    /// `RolePermission`: `ChannelHigherScopedPermissions` by the sorted role names.
    pub role_permissions: Lru<String, crate::perm::HigherScoped>,
    pub scheme: Lru<String, Arc<Scheme>>,
    // store: post extras
    pub reactions: Lru<String, Arc<Vec<Reaction>>>,
    pub file_infos: Lru<String, Arc<Vec<FileInfo>>>,
    pub emoji_by_id: Lru<String, Option<Arc<Emoji>>>,
    pub emoji_by_name: Lru<String, Option<Arc<Emoji>>>,
    pub terms_of_service: Lru<String, Arc<serde_json::Value>>,
    /// `Webhook`: incoming webhooks by id.
    pub webhook: Lru<String, Arc<crate::webhooks::IncomingWebhook>>,
}

impl Caches {
    pub fn new(session_minutes: u64) -> Caches {
        let m = |n: u64| Some(MIN * n as u32);
        Caches {
            session: Lru::new(SESSION_CACHE_SIZE, m(session_minutes.max(1))),
            status: Lru::new(SESSION_CACHE_SIZE, m(30)),
            seen_pending_post_ids: Lru::new(25000, Some(Duration::from_secs(30))),
            open_graph: Lru::new(10000, m(60)),
            user: Lru::new(20000, m(30)),
            all_profiles: Lru::new(1, m(30)),
            profiles_in_channel: Lru::new(CHANNEL_CACHE_SIZE, m(15)),
            profiles_in_channel_loads: Flights::default(),
            channel: Lru::new(CHANNEL_CACHE_SIZE, m(15)),
            channel_by_name: Lru::new(CHANNEL_CACHE_SIZE, m(15)),
            member_count: Lru::new(CHANNEL_CACHE_SIZE, m(30)),
            guest_count: Lru::new(CHANNEL_CACHE_SIZE, m(30)),
            pinned_count: Lru::new(CHANNEL_CACHE_SIZE, m(30)),
            members_for_user: Lru::new(SESSION_CACHE_SIZE, m(15)),
            members_for_user_loads: Flights::default(),
            members_notify_props: Lru::new(SESSION_CACHE_SIZE, m(30)),
            last_post_time: Lru::new(25000, m(15)),
            last_posts: Lru::new(20000, m(30)),
            team_ids_for_user: Lru::new(20000, m(30)),
            role: Lru::new(20000, m(30)),
            role_permissions: Lru::new(20000, m(30)),
            scheme: Lru::new(20000, m(30)),
            reactions: Lru::new(20000, m(30)),
            file_infos: Lru::new(25000, m(30)),
            emoji_by_id: Lru::new(5000, m(30)),
            emoji_by_name: Lru::new(5000, m(30)),
            terms_of_service: Lru::new(20000, m(30)),
            webhook: Lru::new(25000, m(15)),
        }
    }

    /// Every cache's size and counters, by name.
    pub fn stats(&self) -> BTreeMap<&'static str, Stats> {
        BTreeMap::from([
            ("session", self.session.stats()),
            ("status", self.status.stats()),
            ("seen_pending_post_ids", self.seen_pending_post_ids.stats()),
            ("open_graph", self.open_graph.stats()),
            ("user", self.user.stats()),
            ("all_profiles", self.all_profiles.stats()),
            ("profiles_in_channel", self.profiles_in_channel.stats()),
            ("channel", self.channel.stats()),
            ("channel_by_name", self.channel_by_name.stats()),
            ("member_count", self.member_count.stats()),
            ("guest_count", self.guest_count.stats()),
            ("pinned_count", self.pinned_count.stats()),
            ("members_for_user", self.members_for_user.stats()),
            ("members_notify_props", self.members_notify_props.stats()),
            ("last_post_time", self.last_post_time.stats()),
            ("last_posts", self.last_posts.stats()),
            ("team_ids_for_user", self.team_ids_for_user.stats()),
            ("role", self.role.stats()),
            ("role_permissions", self.role_permissions.stats()),
            ("scheme", self.scheme.stats()),
            ("reactions", self.reactions.stats()),
            ("file_infos", self.file_infos.stats()),
            ("emoji_by_id", self.emoji_by_id.stats()),
            ("emoji_by_name", self.emoji_by_name.stats()),
            ("terms_of_service", self.terms_of_service.stats()),
            ("webhook", self.webhook.stats()),
        ])
    }

    /// `InvalidateAllCaches`: every store, session, status and link cache emptied (not the pending post ids,
    /// which guard against duplicate posts rather than cache the database).
    pub fn clear_all(&self) {
        self.session.clear();
        self.status.clear();
        self.open_graph.clear();
        self.user.clear();
        self.all_profiles.clear();
        self.profiles_in_channel.clear();
        self.channel.clear();
        self.channel_by_name.clear();
        self.member_count.clear();
        self.guest_count.clear();
        self.pinned_count.clear();
        self.members_for_user.clear();
        self.members_notify_props.clear();
        self.last_post_time.clear();
        self.last_posts.clear();
        self.team_ids_for_user.clear();
        self.role.clear();
        self.role_permissions.clear();
        self.scheme.clear();
        self.reactions.clear();
        self.file_infos.clear();
        self.emoji_by_id.clear();
        self.emoji_by_name.clear();
        self.terms_of_service.clear();
        self.webhook.clear();
    }

    // ---- Go's invalidation entry points ----

    /// `InvalidateProfileCacheForUser` and `InvalidateProfilesInChannelCacheByUser`.
    pub fn invalidate_user(&self, user_id: &str) {
        self.user.remove(user_id);
        self.all_profiles.clear();
        self.profiles_in_channel.remove_where(|_, m| m.contains_key(user_id));
    }
    /// `InvalidateAllChannelMembersForUser`.
    pub fn invalidate_members_for_user(&self, user_id: &str) {
        self.members_for_user.remove(user_id);
        self.members_for_user.remove(&format!("{user_id}_deleted"));
    }
    /// `InvalidateChannel` and `InvalidateChannelByName`.
    pub fn invalidate_channel(&self, c: &Channel) {
        self.channel.remove(&c.id);
        self.channel_by_name.remove(&format!("{}{}", c.team_id, c.name));
    }
    /// After a membership change in a channel: counts, notify props, profiles in it.
    pub fn invalidate_channel_members(&self, channel_id: &str) {
        self.member_count.remove(channel_id);
        self.guest_count.remove(channel_id);
        self.members_notify_props.remove(channel_id);
        self.profiles_in_channel.remove(channel_id);
    }
    /// `InvalidateLastPostTimeCache`: the time and the cached first pages.
    pub fn invalidate_last_post_time(&self, channel_id: &str) {
        self.last_post_time.remove(channel_id);
        self.last_posts.remove(&format!("{channel_id}30"));
        self.last_posts.remove(&format!("{channel_id}60"));
    }
    pub fn invalidate_post_extras(&self, post_id: &str) {
        self.reactions.remove(post_id);
        self.file_infos.remove(post_id);
    }
}

/// Sorted for stable output.
pub fn sorted<V: Clone>(m: &HashMap<String, V>) -> BTreeMap<String, V> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}
