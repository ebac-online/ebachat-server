//! Rows read and written by `#[model]` against the Go server on the same database (`dev/docker-compose.yml`).
//! Skipped when the stand is not running.

use ebachat_server::{db, model::*};
use serde_json::{Value, json};
use sqlx::PgPool;

const MM: &str = "http://localhost:8065/api/v4";
const DB: &str = "postgres://mmuser:mmpass@localhost:5433/mattermost";

struct Stand {
    http: reqwest::Client,
    token: String,
    db: PgPool,
}

impl Stand {
    async fn up() -> Option<Stand> {
        let http = reqwest::Client::new();
        if http.get(format!("{MM}/system/ping")).send().await.is_err() {
            eprintln!("stand is not running: docker compose -f dev/docker-compose.yml up -d");
            return None;
        }
        let creds = json!({"login_id": "admin", "password": "Admin-pass-123"});
        let mut r = http.post(format!("{MM}/users/login")).json(&creds).send().await.unwrap();
        if !r.status().is_success() {
            // The first user of a fresh server becomes system admin.
            let u = json!({"username": "admin", "email": "admin@example.com", "password": "Admin-pass-123"});
            // Both tests may get here at once: one creates, the other's create fails, both log in.
            let _ = http.post(format!("{MM}/users")).json(&u).send().await;
            r = http.post(format!("{MM}/users/login")).json(&creds).send().await.unwrap();
        }
        let token = r.headers()["token"].to_str().unwrap().to_string();
        Some(Stand { http, token, db: PgPool::connect(DB).await.unwrap() })
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut rq = self.http.request(method, format!("{MM}{path}")).bearer_auth(&self.token);
        if let Some(b) = body {
            rq = rq.json(&b);
        }
        let r = rq.send().await.unwrap();
        let status = r.status();
        let v: Value = r.json().await.unwrap();
        assert!(status.is_success(), "{path}: {status} {v}");
        v
    }
    async fn get(&self, path: &str) -> Value {
        self.call(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    /// A team and its town-square, made once.
    async fn channel(&self) -> String {
        let team = self.get("/teams/name/compat").await;
        let team_id = team["id"].as_str().unwrap();
        self.get(&format!("/teams/{team_id}/channels/name/town-square")).await["id"].as_str().unwrap().to_string()
    }
}

/// Field-by-field difference, for a readable failure.
fn diff(go: &Value, rust: &Value) -> Vec<String> {
    let (Some(g), Some(r)) = (go.as_object(), rust.as_object()) else {
        return if go == rust { vec![] } else { vec![format!("{go} != {rust}")] };
    };
    let mut out = vec![];
    for (k, gv) in g {
        match r.get(k) {
            None => out.push(format!("{k}: go {gv}, rust missing")),
            Some(rv) if rv != gv => out.push(format!("{k}: go {gv}, rust {rv}")),
            _ => {}
        }
    }
    for (k, rv) in r {
        if !g.contains_key(k) {
            out.push(format!("{k}: go missing, rust {rv}"));
        }
    }
    out
}

#[tokio::test]
async fn user_row_reads_as_go_sends_it() {
    let Some(s) = Stand::up().await else { return };
    let me = s.get("/users/me").await;
    let id = me["id"].as_str().unwrap();
    let mut u: User = db::get(&s.db, id).await.unwrap().unwrap();
    // getUser on oneself: Sanitize with every option, see app.SanitizeProfile.
    u.sanitize_own();
    let mut rust = serde_json::to_value(&u).unwrap();
    // Login writes UpdateAt (SqlUserStore.UpdateLastLogin) but Go answers /users/me from its user cache, one login
    // behind the database.
    rust["update_at"] = me["update_at"].clone();
    let d = diff(&me, &rust);
    assert!(d.is_empty(), "GET /users/me differs:\n{}", d.join("\n"));
    // Field order too: the bytes, not just the values.
    assert_eq!(
        me.as_object().unwrap().keys().collect::<Vec<_>>(),
        rust.as_object().unwrap().keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn post_written_by_rust_reads_in_go() {
    let Some(s) = Stand::up().await else { return };
    if s.get("/teams/name/compat/exists").await["exists"] != json!(true) {
        s.post("/teams", json!({"name": "compat", "display_name": "Compat", "type": "O"})).await;
    }
    let channel = s.channel().await;
    let me = s.get("/users/me").await;

    // Go writes a post, Rust reads it.
    let go = s.post("/posts", json!({"channel_id": channel, "message": "written by go"})).await;
    let p: Post = db::get(&s.db, go["id"].as_str().unwrap()).await.unwrap().unwrap();
    assert_eq!(p.message, "written by go");

    // Rust writes a post, Go reads it.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let id = format!("{:x}", now).repeat(3)[..26].to_string();
    let mut props = serde_json::Map::new();
    props.insert("from_rust".into(), json!(true));
    let mine = Post {
        id: id.clone(),
        create_at: now,
        update_at: now,
        user_id: me["id"].as_str().unwrap().into(),
        channel_id: channel.clone(),
        message: "written by rust".into(),
        props: sqlx::types::Json(props),
        file_ids: FileIds::default(),
        filenames: JsonText(vec![]),
        ..Default::default()
    };
    db::insert(&s.db, &mine).await.unwrap();
    let back = s.get(&format!("/posts/{id}")).await;
    let mut rust = serde_json::to_value(&mine).unwrap();
    // Transient fields Go fills when it sends a post.
    for k in ["metadata", "reply_count", "last_reply_at", "participants", "pending_post_id"] {
        if let Some(v) = back.get(k) {
            rust[k] = v.clone();
        }
    }
    let d = diff(&back, &rust);
    assert!(d.is_empty(), "GET /posts/{{id}} of a Rust-written post differs:\n{}", d.join("\n"));
}
