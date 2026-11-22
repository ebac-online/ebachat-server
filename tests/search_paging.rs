//! Post search pages for real (docs/SEARCH.md): LIMIT/OFFSET in SQL over posts the user may read, so pages neither
//! overlap nor skip, whatever `per_page`, ties on CreateAt included, and the filters Go applies after its query
//! (channel permissions, hashtag match, burn-on-read) never shorten or shift a page.
//!
//! Needs a stand database with the bench users (bench0101 / Bench-pass-123); it writes its own channels and posts,
//! so run it on a copy:
//! `MM_CONFIG=dev/bench/run/config.json DATABASE_URL=postgres://mmuser:mmpass@localhost:5433/searchtest
//!  cargo test --release --test search_paging -- --ignored --nocapture`

use ebachat_server::api;
use ebachat_server::app::{App, new_id};
use ebachat_server::config::Config;
use serde_json::{Value, json};

struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Client {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self.http.request(method, format!("{}/api/v4{path}", self.base)).bearer_auth(&self.token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(s < 300, "POST {path}: {s} {v}");
        v
    }
    async fn search(&self, team: &str, terms: &str, page: i64, per_page: i64, deleted: bool) -> Vec<Value> {
        let r = self
            .post(
                &format!("/teams/{team}/posts/search"),
                json!({"terms": terms, "is_or_search": false, "include_deleted_channels": deleted,
                       "time_zone_offset": 0, "page": page, "per_page": per_page}),
            )
            .await;
        r["order"].as_array().unwrap().iter().map(|id| r["posts"][id.as_str().unwrap()].clone()).collect()
    }
    /// Every page until an empty one, as the web app pages; asserts each page but the last is full.
    async fn all_pages(&self, team: &str, terms: &str, per_page: i64, deleted: bool) -> Vec<Value> {
        let mut out = vec![];
        let mut page = 0;
        loop {
            let got = self.search(team, terms, page, per_page, deleted).await;
            if got.is_empty() {
                return out;
            }
            assert!(
                got.len() as i64 <= per_page,
                "page {page} of {terms:?}: {} posts over per_page {per_page}",
                got.len()
            );
            if (got.len() as i64) < per_page {
                // A short page is the last (EBAChat stops here): the next one must be empty.
                assert!(
                    self.search(team, terms, page + 1, per_page, deleted).await.is_empty(),
                    "a page after a short one"
                );
                out.extend(got);
                return out;
            }
            out.extend(got);
            page += 1;
        }
    }
}

/// A word the English stemmer leaves alone and no other post has.
fn word(prefix: &str) -> String {
    let letters: String = new_id().chars().filter(|c| c.is_ascii_alphabetic()).take(10).collect();
    format!("{prefix}{letters}")
}

fn ids(posts: &[Value]) -> Vec<String> {
    posts.iter().map(|p| p["id"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
#[ignore]
async fn search_pages_are_exact() {
    let cfg = Config::load().await.expect("MM_CONFIG");
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (a copy of the stand database)");
    let db = sqlx::PgPool::connect(&url).await.unwrap();
    let app = App::new(db.clone(), cfg).unwrap();
    app.hub.set_app(&app);
    let caches = app.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, api::router(app.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
            .unwrap();
    });

    let http = reqwest::Client::new();
    let r = http
        .post(format!("{base}/api/v4/users/login"))
        .json(&json!({"login_id": "bench0101", "password": "Bench-pass-123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "login bench0101");
    let token = r.headers()["Token"].to_str().unwrap().to_string();
    let me: Value = r.json().await.unwrap();
    let c = Client { base, token, http };
    let (_, teams) = c.call(reqwest::Method::GET, &format!("/users/{}/teams", me["id"].as_str().unwrap()), None).await;
    let team = teams[0]["id"].as_str().unwrap().to_string();

    let channel = |kind: &'static str| {
        let c = &c;
        let team = team.clone();
        async move {
            let name = word("sp");
            c.post("/channels", json!({"team_id": team, "name": name, "display_name": name, "type": kind})).await["id"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let say = |ch: String, msg: String, root: Option<String>| {
        let c = &c;
        async move {
            c.post("/posts", json!({"channel_id": ch, "message": msg, "root_id": root.unwrap_or_default()})).await["id"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };

    let w = word("zq");
    let tag = word("Tg");
    let open = channel("O").await;
    let private = channel("P").await;
    let archived = channel("O").await;
    let noread = channel("P").await;

    // 45 posts with the word, 10 of them sharing one CreateAt; one root gets 2 replies with the word too.
    let mut visible: Vec<String> = vec![];
    for i in 0..45 {
        visible.push(say(open.clone(), format!("{w} number {i}"), None).await);
    }
    let root = visible[3].clone();
    for i in 0..2 {
        visible.push(say(open.clone(), format!("{w} reply {i}"), Some(root.clone())).await);
    }
    sqlx::query("UPDATE Posts SET CreateAt = (SELECT CreateAt FROM Posts WHERE Id = $1) WHERE Id = ANY($2)")
        .bind(&visible[20])
        .bind(&visible[10..20])
        .execute(&db)
        .await
        .unwrap();
    // Burn-on-read: never in search.
    let bor = say(open.clone(), format!("{w} burn"), None).await;
    sqlx::query("UPDATE Posts SET Type = 'burn_on_read' WHERE Id = $1").bind(&bor).execute(&db).await.unwrap();
    // A private channel the user left, an archived channel.
    for i in 0..3 {
        say(private.clone(), format!("{w} private {i}"), None).await;
    }
    let (s, _) = c
        .call(reqwest::Method::DELETE, &format!("/channels/{private}/members/{}", me["id"].as_str().unwrap()), None)
        .await;
    assert_eq!(s, 200, "leave the private channel");
    let mut in_archived = vec![];
    for i in 0..3 {
        in_archived.push(say(archived.clone(), format!("{w} archived {i}"), None).await);
    }
    let (s, _) = c.call(reqwest::Method::DELETE, &format!("/channels/{archived}"), None).await;
    assert_eq!(s, 200, "archive the channel");
    // A private channel the user is still a member of but may not read (no channel roles): FilterPostsByChannelPermissions.
    for i in 0..3 {
        say(noread.clone(), format!("{w} unreadable {i}"), None).await;
    }
    sqlx::query("UPDATE ChannelMembers SET Roles = '', SchemeUser = false, SchemeAdmin = false WHERE ChannelId = $1 AND UserId = $2")
        .bind(&noread)
        .bind(me["id"].as_str().unwrap())
        .execute(&db)
        .await
        .unwrap();
    caches.caches.clear_all();
    caches.hub.invalidate_all();
    // Hashtags: 4 with the tag only (cased differently), 2 with the tag and the word, 1 with a longer tag.
    let mut tagged = vec![];
    for i in 0..4 {
        tagged.push(say(open.clone(), format!("#{tag} only {i}"), None).await);
    }
    let mut both = vec![];
    for i in 0..2 {
        both.push(say(open.clone(), format!("#{} and {w} {i}", tag.to_uppercase()), None).await);
    }
    say(open.clone(), format!("#{tag}x longer"), None).await;

    // The expected order: CreateAt newest first, ties by Id.
    let order = |ids: Vec<String>| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT Id FROM Posts WHERE Id = ANY($1) ORDER BY CreateAt DESC, Id")
                .bind(&ids)
                .fetch_all(&db)
                .await
                .unwrap()
        }
    };
    let words = order([visible.clone(), both.clone()].concat()).await;
    let words_deleted = order([visible.clone(), both.clone(), in_archived.clone()].concat()).await;
    let union = order([visible.clone(), both.clone(), tagged.clone()].concat()).await;
    let tags_only = order([tagged.clone(), both.clone()].concat()).await;
    assert_eq!(words.len(), 49);

    for per_page in [1, 7, 20, 48, 49, 60] {
        assert_eq!(ids(&c.all_pages(&team, &w, per_page, false).await), words, "{w} per_page {per_page}");
    }
    for per_page in [5, 20] {
        assert_eq!(
            ids(&c.all_pages(&team, &w, per_page, true).await),
            words_deleted,
            "with archived, per_page {per_page}"
        );
        assert_eq!(
            ids(&c.all_pages(&team, &format!("#{}", tag.to_lowercase()), per_page, false).await),
            tags_only,
            "hashtag"
        );
        assert_eq!(
            ids(&c.all_pages(&team, &format!("{w} #{tag}"), per_page, false).await),
            union,
            "word and hashtag, per_page {per_page}"
        );
    }
    // Only an excluded hashtag: that set's query fails (Postgres rejects the tsquery), as in Go it then finds
    // nothing and the word's set still answers, the tagged posts included.
    assert_eq!(ids(&c.all_pages(&team, &format!("{w} -#{tag}"), 20, false).await), words, "word, excluded hashtag");
    // A page alone, in the middle: posts 14..21 of the order.
    assert_eq!(ids(&c.search(&team, &w, 2, 7, false).await), words[14..21].to_vec());
    // Past the end, and past the paging window.
    assert!(c.search(&team, &w, 100, 7, false).await.is_empty());
    assert!(c.search(&team, &w, 10_000, 1, false).await.is_empty());
    // Reply counts as Go's: the thread's replies, on the root and on each reply.
    let all = c.all_pages(&team, &w, 60, false).await;
    for p in &all {
        let thread = if p["root_id"].as_str().unwrap().is_empty() {
            p["id"].as_str().unwrap()
        } else {
            p["root_id"].as_str().unwrap()
        };
        let want = if thread == root { 2 } else { 0 };
        assert_eq!(p["reply_count"].as_i64().unwrap(), want, "reply_count of {}", p["id"]);
    }
}
