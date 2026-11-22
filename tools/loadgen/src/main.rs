//! Seeds the dev stand through the Go server's API and runs one load against a server, Go or Rust, measuring the
//! server process (RSS, CPU) and Postgres (CPU) while it runs.
//!
//!   loadgen seed [users] [channels] [posts]          against :8065, writes dev/run/tokens.tsv
//!   loadgen run <url> <pid> <mode> [rps] [seconds]   mode: read | full (full also creates posts)
//!   loadgen steps <url> <pid> <rps:pps,...> [seconds] every user online on a websocket; per step, production's
//!                                                     mix (with posts) at rps, plus pps posts per second into
//!                                                     `bench-public`, the channel every user is in; reports
//!                                                     latency, CPU and how fast `posted` reached every socket
//!
//! Environment: LG_SEED_URL (seed's server, default http://localhost:8065), LG_DB (the database, default the dev
//! stand's), LG_TOKENS (default dev/run/tokens.tsv), LG_PG_CONTAINER (the Postgres container, default
//! ebachat-dev-db-1).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string())
}
fn tokens_path() -> String {
    env_or("LG_TOKENS", "dev/run/tokens.tsv")
}
fn db_url() -> String {
    env_or("LG_DB", "postgres://mmuser:mmpass@localhost:5433/mattermost")
}
const PASSWORD: &str = "Bench-pass-123";

// ---------- a small deterministic RNG, so both servers get the same sequence ----------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

async fn api(
    c: &Client,
    method: reqwest::Method,
    url: &str,
    token: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let mut rq = c.request(method, url).bearer_auth(token);
    if let Some(b) = body {
        rq = rq.json(b);
    }
    let r = rq.send().await.map_err(|e| e.to_string())?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if status.is_success() { Ok(v) } else { Err(format!("{status} {v}")) }
}

async fn login(c: &Client, base: &str, user: &str, password: &str) -> Result<(String, String), String> {
    let r = c
        .post(format!("{base}/api/v4/users/login"))
        .json(&json!({"login_id": user, "password": password}))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let token = r.headers().get("token").ok_or(format!("login {user}: {}", r.status()))?.to_str().unwrap().to_string();
    let u: Value = r.json().await.map_err(|e| e.to_string())?;
    Ok((u["id"].as_str().unwrap().to_string(), token))
}

// ---------- seed ----------

async fn seed(users: usize, channels: usize, posts: usize) {
    let base: &'static str = env_or("LG_SEED_URL", "http://localhost:8065").leak();
    let c = Client::new();
    let admin = match login(&c, base, "admin", "Admin-pass-123").await {
        Ok((_, t)) => t,
        Err(_) => {
            // A fresh database: the first user created becomes the system admin.
            let body = json!({"username": "admin", "email": "admin@example.com", "password": "Admin-pass-123"});
            c.post(format!("{base}/api/v4/users")).json(&body).send().await.expect("create admin");
            login(&c, base, "admin", "Admin-pass-123").await.expect("admin login").1
        }
    };
    let g = move |p: &str| format!("{base}/api/v4{p}");
    use reqwest::Method as M;

    let team = match api(&c, M::GET, &g("/teams/name/bench"), &admin, None).await {
        Ok(t) => t,
        Err(_) => api(
            &c,
            M::POST,
            &g("/teams"),
            &admin,
            Some(&json!({"name": "bench", "display_name": "Bench", "type": "O"})),
        )
        .await
        .unwrap(),
    };
    let team_id = team["id"].as_str().unwrap().to_string();

    let mut ids = vec![];
    for i in 0..users {
        let name = format!("bench{i:04}");
        let u = match api(&c, M::GET, &g(&format!("/users/username/{name}")), &admin, None).await {
            Ok(u) => u,
            Err(_) => {
                let body = json!({"username": name, "email": format!("{name}@example.com"), "password": PASSWORD});
                api(&c, M::POST, &g("/users"), &admin, Some(&body)).await.unwrap()
            }
        };
        ids.push(u["id"].as_str().unwrap().to_string());
    }
    for chunk in ids.chunks(100) {
        let body: Vec<Value> = chunk.iter().map(|u| json!({"team_id": team_id, "user_id": u})).collect();
        api(&c, M::POST, &g(&format!("/teams/{team_id}/members/batch")), &admin, Some(&json!(body))).await.unwrap();
    }
    eprintln!("{} users in team", ids.len());

    let mut rng = Rng(42);
    let mut chans = vec![];
    for i in 0..channels {
        let name = format!("bench-{i:02}");
        let ch = match api(&c, M::GET, &g(&format!("/teams/{team_id}/channels/name/{name}")), &admin, None).await {
            Ok(ch) => ch,
            Err(_) => {
                let body = json!({"team_id": team_id, "name": name, "display_name": format!("Bench {i}"), "type": "O"});
                api(&c, M::POST, &g("/channels"), &admin, Some(&body)).await.unwrap()
            }
        };
        let cid = ch["id"].as_str().unwrap().to_string();
        // About a third of the users in each channel.
        let members: Vec<String> = ids.iter().filter(|_| rng.below(3) == 0).cloned().collect();
        for chunk in members.chunks(100) {
            let body = json!({"user_ids": chunk});
            if let Err(e) = api(&c, M::POST, &g(&format!("/channels/{cid}/members")), &admin, Some(&body)).await {
                eprintln!("add members: {e}");
            }
        }
        chans.push(cid);
    }
    eprintln!("{} channels", chans.len());

    // The public channel every user is in: one post reaches every socket.
    let public = match api(&c, M::GET, &g(&format!("/teams/{team_id}/channels/name/bench-public")), &admin, None).await
    {
        Ok(ch) => ch,
        Err(_) => {
            let body = json!({"team_id": team_id, "name": "bench-public", "display_name": "Bench public", "type": "O"});
            api(&c, M::POST, &g("/channels"), &admin, Some(&body)).await.unwrap()
        }
    };
    let pid = public["id"].as_str().unwrap().to_string();
    for chunk in ids.chunks(100) {
        if let Err(e) =
            api(&c, M::POST, &g(&format!("/channels/{pid}/members")), &admin, Some(&json!({"user_ids": chunk}))).await
        {
            eprintln!("add members to bench-public: {e}");
        }
    }
    eprintln!("bench-public: {} members", ids.len());

    let mut tokens = vec![];
    for i in 0..users {
        match login(&c, base, &format!("bench{i:04}"), PASSWORD).await {
            Ok(t) => tokens.push(t),
            Err(e) => eprintln!("skipped: {e}"),
        }
    }
    let tp = tokens_path();
    if let Some(d) = std::path::Path::new(&tp).parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(&tp, tokens.iter().map(|(u, t)| format!("{u}\t{t}\n")).collect::<String>()).unwrap();
    eprintln!("tokens in {tp}");

    let member_of = memberships(&tokens).await;
    let done = Arc::new(AtomicU64::new(0));
    let mut tasks = vec![];
    for w in 0..32u64 {
        let (c, tokens, member_of, done) = (c.clone(), tokens.clone(), member_of.clone(), done.clone());
        tasks.push(tokio::spawn(async move {
            let mut rng = Rng(1000 + w);
            for _ in 0..posts / 32 {
                let (u, t) = rng.pick(&tokens);
                let Some(chs) = member_of.get(u) else { continue };
                let body = json!({"channel_id": rng.pick(chs), "message": format!("seed message {}", rng.next())});
                if api(&c, M::POST, &g("/posts"), t, Some(&body)).await.is_ok() {
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if n % 2000 == 0 {
                        eprintln!("{n} posts");
                    }
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    eprintln!("{} posts", done.load(Ordering::Relaxed));
}

async fn memberships(tokens: &[(String, String)]) -> Arc<HashMap<String, Vec<String>>> {
    let db = sqlx::PgPool::connect(&db_url()).await.unwrap();
    let users: Vec<&String> = tokens.iter().map(|(u, _)| u).collect();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT cm.UserId, cm.ChannelId FROM ChannelMembers cm JOIN Channels c ON c.Id = cm.ChannelId
         WHERE cm.UserId = ANY($1) AND c.Type = 'O'",
    )
    .bind(users)
    .fetch_all(&db)
    .await
    .unwrap();
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for (u, c) in rows {
        m.entry(u).or_default().push(c);
    }
    for v in m.values_mut() {
        v.sort();
    }
    Arc::new(m)
}

fn read_tokens() -> Vec<(String, String)> {
    std::fs::read_to_string(tokens_path())
        .expect("run `loadgen seed` first")
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(a, b)| (a.to_string(), b.to_string())))
        .collect()
}

// ---------- measurement ----------

/// RSS (KiB) and CPU time (s) of a process: from /proc on Linux (`ps` has whole seconds there), else `ps`.
fn proc_stat(pid: u32) -> Option<(u64, f64)> {
    if let Ok(st) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // After the command's closing parenthesis: field 3 is the state; utime and stime are fields 14 and 15.
        let f: Vec<&str> = st.rsplit_once(')')?.1.split_whitespace().collect();
        let ticks: f64 = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let rss = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?.split_whitespace().next()?.parse().ok()?;
        return Some((rss, ticks / 100.0));
    }
    let o = std::process::Command::new("ps").args(["-o", "rss=,time=", "-p", &pid.to_string()]).output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout);
    let mut it = s.split_whitespace();
    let rss = it.next()?.parse().ok()?;
    // [[dd-]hh:]mm:ss.cc
    let t = it.next()?;
    let mut secs = 0.0;
    for part in t.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some((rss, secs))
}

/// CPU time (s) the Postgres container has used, from its cgroup.
fn pg_cpu() -> f64 {
    let o = std::process::Command::new("docker")
        .args(["exec", &env_or("LG_PG_CONTAINER", "ebachat-dev-db-1"), "cat", "/sys/fs/cgroup/cpu.stat"])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    s.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) / 1e6
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Op {
    StatusIds,
    View,
    PostsSince,
    UsersIds,
    Me,
    CreatePost,
}

/// Production's mix (4 h of mm.ebac.app, docs/prod-traffic.tsv) over the endpoints both servers have.
fn mix(full: bool) -> Vec<(Op, u32)> {
    let mut m = vec![(Op::StatusIds, 345), (Op::View, 266), (Op::PostsSince, 198), (Op::UsersIds, 135), (Op::Me, 30)];
    if full {
        m.push((Op::CreatePost, 157));
    }
    m
}

struct Stats {
    lat: Mutex<HashMap<Op, Vec<u32>>>,
    errors: Mutex<HashMap<Op, (u64, String)>>,
}

async fn run(base: String, pid: u32, full: bool, rps: u32, seconds: u64) {
    let tokens = read_tokens();
    let member_of = memberships(&tokens).await;
    let c = Client::builder().pool_max_idle_per_host(256).build().unwrap();
    let (rss0, _) = proc_stat(pid).expect("server pid");

    // A websocket per user, kept open and read.
    let ws_url = base.replacen("http", "ws", 1) + "/api/v4/websocket";
    let events = Arc::new(AtomicU64::new(0));
    let mut sockets = vec![];
    for (_, t) in &tokens {
        let mut rq = ws_url.as_str().into_client_request().unwrap();
        rq.headers_mut().insert("Authorization", format!("Bearer {t}").parse().unwrap());
        match tokio_tungstenite::connect_async(rq).await {
            Ok((ws, _)) => {
                let events = events.clone();
                sockets.push(tokio::spawn(async move {
                    let (_w, mut r) = ws.split();
                    while let Some(Ok(_)) = r.next().await {
                        events.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            Err(e) => eprintln!("websocket: {e}"),
        }
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (rss_ws, _) = proc_stat(pid).unwrap();

    let stats = Arc::new(Stats { lat: Default::default(), errors: Default::default() });
    let ops = mix(full);
    let total_w: u32 = ops.iter().map(|o| o.1).sum();
    let all_users: Arc<Vec<String>> = Arc::new(tokens.iter().map(|(u, _)| u.clone()).collect());
    let tokens = Arc::new(tokens);

    // Warm-up at the same rate, then the measured window.
    let mut rng = Rng(7);
    let mut measured = None;
    let mut samples = vec![];
    let interval = Duration::from_secs_f64(1.0 / rps as f64);
    let warmup = 15;
    let start = Instant::now();
    let mut tick = tokio::time::interval(interval);
    let mut last_sample = Instant::now();
    let mut sent = 0u64;
    loop {
        tick.tick().await;
        let el = start.elapsed().as_secs();
        if el >= warmup && measured.is_none() {
            stats.lat.lock().unwrap().clear();
            stats.errors.lock().unwrap().clear();
            measured = Some((Instant::now(), proc_stat(pid).unwrap().1, pg_cpu(), events.load(Ordering::Relaxed)));
            sent = 0;
        }
        if el >= warmup + seconds {
            break;
        }
        if measured.is_some() && last_sample.elapsed() >= Duration::from_secs(1) {
            samples.push(proc_stat(pid).unwrap().0);
            last_sample = Instant::now();
        }
        let mut w = rng.below(total_w as usize) as u32;
        let op = ops
            .iter()
            .find(|(_, x)| {
                if w < *x {
                    true
                } else {
                    w -= x;
                    false
                }
            })
            .unwrap()
            .0;
        let i = rng.below(tokens.len());
        let seed = rng.next();
        let (c, tokens, member_of, all_users, stats, base) =
            (c.clone(), tokens.clone(), member_of.clone(), all_users.clone(), stats.clone(), base.clone());
        sent += 1;
        tokio::spawn(async move {
            let mut rng = Rng(seed | 1);
            let (u, t) = &tokens[i];
            let chs = member_of.get(u).cloned().unwrap_or_default();
            let g = |p: &str| format!("{base}/api/v4{p}");
            use reqwest::Method as M;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
            let some_users =
                |rng: &mut Rng, n: usize| -> Vec<String> { (0..n).map(|_| rng.pick(&all_users).clone()).collect() };
            let t0 = Instant::now();
            let r = match op {
                Op::StatusIds => {
                    api(&c, M::POST, &g("/users/status/ids"), t, Some(&json!(some_users(&mut rng, 50)))).await
                }
                Op::UsersIds => api(&c, M::POST, &g("/users/ids"), t, Some(&json!(some_users(&mut rng, 20)))).await,
                Op::Me => api(&c, M::GET, &g("/users/me"), t, None).await,
                Op::View => {
                    let body = json!({"channel_id": rng.pick(&chs), "prev_channel_id": rng.pick(&chs), "collapsed_threads_supported": true});
                    api(&c, M::POST, &g("/channels/members/me/view"), t, Some(&body)).await
                }
                Op::PostsSince => {
                    api(&c, M::GET, &g(&format!("/channels/{}/posts?since={}", rng.pick(&chs), now - 60_000)), t, None)
                        .await
                }
                Op::CreatePost => {
                    let body = json!({"channel_id": rng.pick(&chs), "message": format!("load message {}", rng.next())});
                    api(&c, M::POST, &g("/posts"), t, Some(&body)).await
                }
            };
            let us = t0.elapsed().as_micros() as u32;
            match r {
                Ok(_) => stats.lat.lock().unwrap().entry(op).or_default().push(us),
                Err(e) => {
                    let mut er = stats.errors.lock().unwrap();
                    let x = er.entry(op).or_insert((0, e));
                    x.0 += 1;
                }
            }
        });
    }
    let (t0, cpu0, pg0, ev0) = measured.unwrap();
    let wall = t0.elapsed().as_secs_f64();
    let (rss_end, cpu1) = proc_stat(pid).unwrap();
    let pg1 = pg_cpu();
    let ev = events.load(Ordering::Relaxed) - ev0;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let rss_max = samples.iter().copied().max().unwrap_or(0);
    let rss_avg = samples.iter().sum::<u64>() / samples.len().max(1) as u64;
    let mb = |k: u64| k as f64 / 1024.0;
    println!(
        "server        {base}  pid {pid}  mode {}  target {rps} rps  {seconds} s (+{warmup} s warm-up)",
        if full { "full" } else { "read" }
    );
    println!("websockets    {} open, {ev} events received", sockets.len());
    println!(
        "RSS MB        start {:.0}  with websockets {:.0}  load avg {:.0}  max {:.0}  end {:.0}",
        mb(rss0),
        mb(rss_ws),
        mb(rss_avg),
        mb(rss_max),
        mb(rss_end)
    );
    println!("server CPU    {:.2} s = {:.1}% of a core", cpu1 - cpu0, (cpu1 - cpu0) / wall * 100.0);
    println!("postgres CPU  {:.2} s = {:.1}% of a core", pg1 - pg0, (pg1 - pg0) / wall * 100.0);
    let lat = stats.lat.lock().unwrap();
    let ok: usize = lat.values().map(Vec::len).sum();
    println!("requests      sent {sent}, ok {ok} = {:.1} rps", ok as f64 / wall);
    println!("{:<12} {:>7} {:>8} {:>8} {:>8}", "op", "ok", "p50 ms", "p95 ms", "p99 ms");
    let mut keys: Vec<_> = lat.keys().copied().collect();
    keys.sort();
    for k in keys {
        let mut v = lat[&k].clone();
        v.sort();
        let p = |q: f64| v[((v.len() as f64 * q) as usize).min(v.len() - 1)] as f64 / 1000.0;
        println!("{:<12} {:>7} {:>8.2} {:>8.2} {:>8.2}", format!("{k:?}"), v.len(), p(0.5), p(0.95), p(0.99));
    }
    for (k, (n, e)) in stats.errors.lock().unwrap().iter() {
        println!("errors {k:?}: {n}, e.g. {}", e.chars().take(200).collect::<String>());
    }
    for s in sockets {
        s.abort();
    }
}

// ---------- steps: many clients online, a channel they are all in, load in steps ----------

/// A latency histogram in microseconds, lock-free: 16 buckets per power of two (about 4% resolution).
struct Hist(Vec<AtomicU64>);
impl Hist {
    fn new() -> Hist {
        Hist((0..640).map(|_| AtomicU64::new(0)).collect())
    }
    fn idx(us: u64) -> usize {
        if us < 16 {
            return us as usize;
        }
        let l = 63 - us.leading_zeros() as usize; // >= 4
        let sub = ((us >> (l - 4)) - 16) as usize;
        (16 + (l - 4) * 16 + sub).min(639)
    }
    fn value(i: usize) -> f64 {
        if i < 16 {
            return i as f64;
        }
        let (l, sub) = ((i - 16) / 16 + 4, (i - 16) % 16);
        // The middle of the bucket.
        ((16 + sub) as f64 + 0.5) * (1u64 << (l - 4)) as f64
    }
    fn add(&self, us: u64) {
        self.0[Self::idx(us)].fetch_add(1, Ordering::Relaxed);
    }
    fn count(&self) -> u64 {
        self.0.iter().map(|b| b.load(Ordering::Relaxed)).sum()
    }
    /// The q-quantile in milliseconds.
    fn q(&self, q: f64) -> f64 {
        let n = self.count();
        if n == 0 {
            return f64::NAN;
        }
        let want = ((n as f64 * q).ceil() as u64).max(1);
        let mut acc = 0;
        for (i, b) in self.0.iter().enumerate() {
            acc += b.load(Ordering::Relaxed);
            if acc >= want {
                return Self::value(i) / 1000.0;
            }
        }
        f64::NAN
    }
}

const OPS: [Op; 6] = [Op::StatusIds, Op::View, Op::PostsSince, Op::UsersIds, Op::Me, Op::CreatePost];

/// What one step saw: request latency per operation, errors, and the public posts' delivery.
struct Step {
    lat: Vec<Hist>,
    errors: Mutex<HashMap<Op, (u64, String)>>,
    public_ok: AtomicU64,
    public_err: AtomicU64,
    public_lat: Hist,
    delivery: Hist,
    events: AtomicU64,
}
impl Step {
    fn new() -> Step {
        Step {
            lat: OPS.iter().map(|_| Hist::new()).collect(),
            errors: Default::default(),
            public_ok: AtomicU64::new(0),
            public_err: AtomicU64::new(0),
            public_lat: Hist::new(),
            delivery: Hist::new(),
            events: AtomicU64::new(0),
        }
    }
}

fn now_us() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64
}

/// The send time a public post carries in its message (`lgts<µs>`), found in the raw `posted` event.
fn marker(text: &str) -> Option<u64> {
    let i = text.find("lgts")? + 4;
    let d: String = text[i..].chars().take_while(char::is_ascii_digit).collect();
    d.parse().ok()
}

/// Total and idle jiffies of the whole machine, from /proc/stat (zeros elsewhere).
fn machine_cpu() -> (f64, f64) {
    let s = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let v: Vec<f64> =
        s.lines().next().unwrap_or("").split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 5 {
        return (0.0, 0.0);
    }
    (v.iter().sum(), v[3] + v[4])
}

async fn public_channel() -> String {
    let db = sqlx::PgPool::connect(&db_url()).await.unwrap();
    sqlx::query_scalar("SELECT Id FROM Channels WHERE Name = 'bench-public' AND DeleteAt = 0")
        .fetch_one(&db)
        .await
        .expect("no bench-public channel: run `loadgen seed` again")
}

async fn steps(base: String, pid: u32, plan: Vec<(u32, u32)>, seconds: u64) {
    let tokens = read_tokens();
    let member_of = memberships(&tokens).await;
    let public = Arc::new(public_channel().await);
    let c = Client::builder().pool_max_idle_per_host(1024).build().unwrap();

    // Index 0 is the warm-up; step i is plan[i - 1].
    let all: Arc<Vec<Step>> = Arc::new((0..=plan.len()).map(|_| Step::new()).collect());
    let cur = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (rss0, _) = proc_stat(pid).expect("server pid");
    let ws_url = base.replacen("http", "ws", 1) + "/api/v4/websocket";
    let t_conn = Instant::now();
    let mut sockets = vec![];
    for (_, t) in &tokens {
        let mut rq = ws_url.as_str().into_client_request().unwrap();
        rq.headers_mut().insert("Authorization", format!("Bearer {t}").parse().unwrap());
        match tokio_tungstenite::connect_async(rq).await {
            Ok((ws, _)) => {
                let (all, cur) = (all.clone(), cur.clone());
                sockets.push(tokio::spawn(async move {
                    let (_w, mut r) = ws.split();
                    while let Some(Ok(m)) = r.next().await {
                        let st = &all[cur.load(Ordering::Relaxed)];
                        st.events.fetch_add(1, Ordering::Relaxed);
                        if let Ok(text) = m.to_text()
                            // Go writes `{"event": "posted", ...` (with a space), Rust without.
                            && text.contains("\"posted\"")
                            && let Some(sent) = marker(text)
                        {
                            st.delivery.add(now_us().saturating_sub(sent));
                        }
                    }
                }));
            }
            Err(e) => eprintln!("websocket: {e}"),
        }
    }
    let online = sockets.len() as u64;
    eprintln!("{online} websockets in {:.1} s", t_conn.elapsed().as_secs_f64());
    tokio::time::sleep(Duration::from_secs(5)).await;
    let (rss_ws, _) = proc_stat(pid).unwrap_or_default();
    println!(
        "server {base}  pid {pid}  {online} clients online  RSS start {} MB, with websockets {} MB",
        rss0 / 1024,
        rss_ws / 1024
    );
    println!(
        "{:>5} {:>4} | {:>7} {:>5} | {:>6} {:>6} {:>6} | {:>5} {:>8} {:>8} {:>8} {:>9} | {:>6} {:>6} {:>5} {:>5} {:>5}",
        "rps",
        "pps",
        "ok rps",
        "err",
        "p50",
        "p99",
        "post99",
        "pub/s",
        "dlv p50",
        "dlv p99",
        "dlv max",
        "dlv/exp",
        "srv %",
        "pg %",
        "RSS",
        "mach%",
        "lg %"
    );

    let ops = mix(true);
    let total_w: u32 = ops.iter().map(|o| o.1).sum();
    let all_users: Arc<Vec<String>> = Arc::new(tokens.iter().map(|(u, _)| u.clone()).collect());
    let tokens = Arc::new(tokens);
    let mut rng = Rng(7);
    let mut stages = vec![(plan[0].0, plan[0].1, 10u64)];
    stages.extend(plan.iter().map(|&(r, p)| (r, p, seconds)));
    for (si, &(rps, pps, secs)) in stages.iter().enumerate() {
        cur.store(si, Ordering::Relaxed);
        let st_cpu0 = (
            proc_stat(pid).map(|p| p.1).unwrap_or(0.0),
            pg_cpu(),
            machine_cpu(),
            proc_stat(std::process::id()).map(|p| p.1).unwrap_or(0.0),
        );
        let t0 = Instant::now();
        let end = t0 + Duration::from_secs(secs);
        let mut rss_max = 0;

        // Public posts on their own clock.
        let poster = (pps > 0).then(|| {
            let (c, tokens, all, public, base) = (c.clone(), tokens.clone(), all.clone(), public.clone(), base.clone());
            let mut rng = Rng(99 + si as u64);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / pps as f64));
                while Instant::now() < end {
                    tick.tick().await;
                    let (_, t) = rng.pick(&tokens).clone();
                    let (c, all, public, base) = (c.clone(), all.clone(), public.clone(), base.clone());
                    tokio::spawn(async move {
                        let sent = now_us();
                        let body =
                            json!({"channel_id": *public, "message": format!("public lgts{sent} hello everyone")});
                        let r = api(&c, reqwest::Method::POST, &format!("{base}/api/v4/posts"), &t, Some(&body)).await;
                        let st = &all[si];
                        match r {
                            Ok(_) => {
                                st.public_ok.fetch_add(1, Ordering::Relaxed);
                                st.public_lat.add(now_us() - sent);
                            }
                            Err(e) => {
                                st.public_err.fetch_add(1, Ordering::Relaxed);
                                if st.public_err.load(Ordering::Relaxed) == 1 {
                                    eprintln!("public post: {}", e.chars().take(160).collect::<String>());
                                }
                            }
                        }
                    });
                }
            })
        });

        let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / rps.max(1) as f64));
        let mut last_sample = Instant::now();
        while Instant::now() < end {
            tick.tick().await;
            if last_sample.elapsed() >= Duration::from_secs(1) {
                rss_max = rss_max.max(proc_stat(pid).map(|p| p.0).unwrap_or(0));
                last_sample = Instant::now();
            }
            if rps == 0 {
                continue;
            }
            let mut w = rng.below(total_w as usize) as u32;
            let op = ops
                .iter()
                .find(|(_, x)| {
                    if w < *x {
                        true
                    } else {
                        w -= x;
                        false
                    }
                })
                .unwrap()
                .0;
            let i = rng.below(tokens.len());
            let seed = rng.next();
            let (c, tokens, member_of, all_users, all, base) =
                (c.clone(), tokens.clone(), member_of.clone(), all_users.clone(), all.clone(), base.clone());
            tokio::spawn(async move {
                let mut rng = Rng(seed | 1);
                let (u, t) = &tokens[i];
                let chs = member_of.get(u).cloned().unwrap_or_default();
                if chs.is_empty() {
                    return;
                }
                let g = |p: &str| format!("{base}/api/v4{p}");
                use reqwest::Method as M;
                let now = (now_us() / 1000) as i64;
                let some_users =
                    |rng: &mut Rng, n: usize| -> Vec<String> { (0..n).map(|_| rng.pick(&all_users).clone()).collect() };
                let t0 = Instant::now();
                let r = match op {
                    Op::StatusIds => {
                        api(&c, M::POST, &g("/users/status/ids"), t, Some(&json!(some_users(&mut rng, 50)))).await
                    }
                    Op::UsersIds => api(&c, M::POST, &g("/users/ids"), t, Some(&json!(some_users(&mut rng, 20)))).await,
                    Op::Me => api(&c, M::GET, &g("/users/me"), t, None).await,
                    Op::View => {
                        let body = json!({"channel_id": rng.pick(&chs), "prev_channel_id": rng.pick(&chs), "collapsed_threads_supported": true});
                        api(&c, M::POST, &g("/channels/members/me/view"), t, Some(&body)).await
                    }
                    Op::PostsSince => {
                        api(
                            &c,
                            M::GET,
                            &g(&format!("/channels/{}/posts?since={}", rng.pick(&chs), now - 60_000)),
                            t,
                            None,
                        )
                        .await
                    }
                    Op::CreatePost => {
                        let body =
                            json!({"channel_id": rng.pick(&chs), "message": format!("load message {}", rng.next())});
                        api(&c, M::POST, &g("/posts"), t, Some(&body)).await
                    }
                };
                let st = &all[si];
                match r {
                    Ok(_) => st.lat[OPS.iter().position(|o| *o == op).unwrap()].add(t0.elapsed().as_micros() as u64),
                    Err(e) => st.errors.lock().unwrap().entry(op).or_insert((0, e)).0 += 1,
                }
            });
        }
        if let Some(p) = poster {
            let _ = p.await;
        }
        let wall = t0.elapsed().as_secs_f64();
        // Let the last posts' events arrive before the next step takes over the counters.
        tokio::time::sleep(Duration::from_millis(if pps > 0 { 1500 } else { 200 })).await;
        let Some((_, srv1)) = proc_stat(pid) else {
            println!("server pid {pid} gone (or no file descriptors left): stopping");
            break;
        };
        let (pg1, m1, lg1) = (pg_cpu(), machine_cpu(), proc_stat(std::process::id()).map(|p| p.1).unwrap_or(0.0));
        if si == 0 {
            continue;
        }
        let st = &all[si];
        let ok: u64 = st.lat.iter().map(Hist::count).sum();
        let err: u64 =
            st.errors.lock().unwrap().values().map(|e| e.0).sum::<u64>() + st.public_err.load(Ordering::Relaxed);
        // All operations together, then POST /posts alone.
        let merged = Hist::new();
        for h in &st.lat {
            for (i, b) in h.0.iter().enumerate() {
                merged.0[i].fetch_add(b.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
        let post = &st.lat[5];
        let pub_ok = st.public_ok.load(Ordering::Relaxed);
        let dlv = st.delivery.count();
        let (mt, mi) = (m1.0 - st_cpu0.2.0, m1.1 - st_cpu0.2.1);
        let dmax = st
            .delivery
            .0
            .iter()
            .rposition(|b| b.load(Ordering::Relaxed) > 0)
            .map(|i| Hist::value(i) / 1000.0)
            .unwrap_or(f64::NAN);
        println!(
            "{:>5} {:>4} | {:>7.1} {:>5} | {:>6.2} {:>6.2} {:>6.2} | {:>5.1} {:>8.2} {:>8.2} {:>8.1} {:>9} | {:>6.0} {:>6.0} {:>5} {:>5.0} {:>5.0}",
            rps,
            pps,
            ok as f64 / wall,
            err,
            merged.q(0.5),
            merged.q(0.99),
            post.q(0.99),
            pub_ok as f64 / wall,
            st.delivery.q(0.5),
            st.delivery.q(0.99),
            dmax,
            format!("{}/{}", dlv, pub_ok * online),
            (srv1 - st_cpu0.0) / wall * 100.0,
            (pg1 - st_cpu0.1) / wall * 100.0,
            rss_max / 1024,
            if mt > 0.0 { (mt - mi) / mt * 100.0 } else { f64::NAN },
            (lg1 - st_cpu0.3) / wall * 100.0,
        );
        for (k, (n, e)) in st.errors.lock().unwrap().iter() {
            println!("      errors {k:?}: {n}, e.g. {}", e.chars().take(160).collect::<String>());
        }
    }
    for s in sockets {
        s.abort();
    }
}
#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let n = |i: usize, d: usize| a.get(i).and_then(|v| v.parse().ok()).unwrap_or(d);
    match a.get(1).map(String::as_str) {
        Some("seed") => seed(n(2, 300), n(3, 40), n(4, 30000)).await,
        Some("run") => {
            let base = a[2].clone();
            let pid: u32 = a[3].parse().expect("pid");
            run(base, pid, a.get(4).map(String::as_str) == Some("full"), n(5, 100) as u32, n(6, 120) as u64).await
        }
        Some("steps") => {
            let pid: u32 = a[3].parse().expect("pid");
            let plan: Vec<(u32, u32)> = a[4]
                .split(',')
                .map(|s| {
                    let (r, p) = s.split_once(':').unwrap_or((s, "0"));
                    (r.parse().expect("rps"), p.parse().expect("pps"))
                })
                .collect();
            steps(a[2].clone(), pid, plan, n(5, 30) as u64).await
        }
        _ => eprintln!(
            "usage: loadgen seed [users] [channels] [posts] | run <url> <pid> <read|full> [rps] [seconds] | steps <url> <pid> <rps:pps,...> [seconds]"
        ),
    }
}
