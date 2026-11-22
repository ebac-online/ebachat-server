//! Startup migrations (src/migrate.rs) against a real Postgres: an empty database comes out with the schema and the
//! `db_migrations` rows the Go server leaves (compared with the stand's database, which Go migrated), concurrent
//! runs wait on morph's lock, a second run does nothing, our own set is recorded apart, a failing migration leaves
//! no record, an expired lock is taken over, and Go's pre-migration renumbers 142-144.
//!
//! `DATABASE_URL=postgres://mmuser:mmpass@localhost:5433/mattermost cargo test --release --test migrations -- --ignored --nocapture`
//! (the stand's database is only read; the test creates and drops `migtest_*` databases next to it).

use std::time::{Duration, Instant};

use ebachat_server::migrate::{self, Set};
use sqlx::PgPool;

const TIMEOUT: i64 = 600;

fn url_for(db: &str) -> String {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL of the stand's database");
    let (base, _) = url.rsplit_once('/').unwrap();
    format!("{base}/{db}")
}

/// One copy of the stand's database the tests copy from (CREATE DATABASE ... TEMPLATE needs a template nobody is
/// connected to).
static SNAP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
const SNAP_DB: &str = "migtest_snap";

async fn snapshot() {
    SNAP.get_or_init(|| async {
        let stand = std::env::var("DATABASE_URL").unwrap();
        let stand = stand.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
        let admin = PgPool::connect(&url_for("postgres")).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {SNAP_DB} WITH (FORCE)")))
            .execute(&admin)
            .await
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {SNAP_DB} TEMPLATE {stand}")))
            .execute(&admin)
            .await
            .unwrap();
    })
    .await;
}

/// A database copied from the snapshot under a lock, so concurrent tests never copy it while another is connected.
static COPY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn fresh(name: &str, template: Option<&str>) -> PgPool {
    snapshot().await;
    let _g = COPY.lock().await;
    let admin = PgPool::connect(&url_for("postgres")).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
    let create = match template {
        Some(t) => format!("CREATE DATABASE {name} TEMPLATE {t}"),
        None => format!("CREATE DATABASE {name}"),
    };
    sqlx::raw_sql(sqlx::AssertSqlSafe(create)).execute(&admin).await.unwrap();
    PgPool::connect(&url_for(name)).await.unwrap()
}

async fn drop_db(pool: PgPool, name: &str) {
    pool.close().await;
    let admin = PgPool::connect(&url_for("postgres")).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}

/// What the schema is: columns, indexes, constraints, views, functions and types, as sorted text lines.
async fn schema(db: &PgPool) -> Vec<String> {
    let q = r#"
        SELECT 'column ' || table_name || '.' || column_name || ' ' || ordinal_position || ' ' || data_type || ' '
               || coalesce(character_maximum_length::text, '') || ' ' || is_nullable || ' ' || coalesce(column_default, '')
          FROM information_schema.columns WHERE table_schema = current_schema()
        UNION ALL SELECT 'index ' || indexdef FROM pg_indexes WHERE schemaname = current_schema()
        UNION ALL SELECT 'constraint ' || conrelid::regclass || ' ' || pg_get_constraintdef(oid) FROM pg_constraint
          WHERE connamespace = current_schema()::regnamespace
        UNION ALL SELECT 'view ' || viewname || ' ' || definition FROM pg_views WHERE schemaname = current_schema()
        UNION ALL SELECT 'function ' || p.proname || ' ' || pg_get_functiondef(p.oid) FROM pg_proc p
          WHERE p.pronamespace = current_schema()::regnamespace AND p.prokind = 'f'
        UNION ALL SELECT 'type ' || t.typname || ' ' || coalesce(string_agg(e.enumlabel, ',' ORDER BY e.enumsortorder), '')
          FROM pg_type t LEFT JOIN pg_enum e ON e.enumtypid = t.oid
          WHERE t.typnamespace = current_schema()::regnamespace AND t.typtype IN ('e', 'd', 'c') AND t.typrelid = 0
          GROUP BY t.typname
    "#;
    let mut rows: Vec<String> = sqlx::query_scalar(q).fetch_all(db).await.unwrap();
    // Our own bookkeeping table is not part of the Go schema, nor is the bench's pg_stat_statements extension.
    rows.retain(|r| !r.contains("ebachat_migrations") && !r.contains("pg_stat_statements"));
    rows.sort();
    rows
}

async fn records(db: &PgPool, table: &str) -> Vec<(i64, String)> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT version, name FROM {table} ORDER BY version, name")))
        .fetch_all(db)
        .await
        .unwrap()
}

fn ours(files: &[(&str, &str)]) -> Vec<Set> {
    vec![Set { table: "ebachat_migrations", files: migrate::parse(files).unwrap() }]
}

#[tokio::test]
#[ignore]
async fn an_empty_database_comes_out_as_go_leaves_it() {
    snapshot().await;
    // Go's result, read from the snapshot while no test copies it.
    let (want, expected) = {
        let _g = COPY.lock().await;
        let stand = PgPool::connect(&url_for(SNAP_DB)).await.unwrap();
        let r = (records(&stand, "db_migrations").await, schema(&stand).await);
        stand.close().await;
        r
    };
    let db = fresh("migtest_fresh", None).await;
    let sets = migrate::builtin().unwrap();
    // Two servers starting at once: one migrates, the other waits on the lock and then finds nothing pending.
    let (a, b) = tokio::join!(migrate::run_sets(&db, &sets, TIMEOUT), migrate::run_sets(&db, &sets, TIMEOUT));
    a.unwrap();
    b.unwrap();
    assert_eq!(records(&db, "db_migrations").await, want, "db_migrations as Go's");
    let got = schema(&db).await;
    let missing: Vec<_> = expected.iter().filter(|r| !got.contains(r)).collect();
    let extra: Vec<_> = got.iter().filter(|r| !expected.contains(r)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "schema differs from Go's:\n missing {missing:#?}\n extra {extra:#?}"
    );
    // The lock row is gone; a second run applies nothing.
    let locks: i64 = sqlx::query_scalar("SELECT count(*) FROM db_lock").fetch_one(&db).await.unwrap();
    assert_eq!(locks, 0);
    migrate::run_sets(&db, &sets, TIMEOUT).await.unwrap();
    assert_eq!(records(&db, "db_migrations").await, want);
    drop_db(db, "migtest_fresh").await;
}

#[tokio::test]
#[ignore]
async fn our_migrations_are_recorded_apart_and_fail_cleanly() {
    let db = fresh("migtest_ours", Some(SNAP_DB)).await;
    let before = records(&db, "db_migrations").await;
    let good = [
        (
            "000001_ebachat_test_concurrently.up.sql",
            "-- morph:nontransactional\nCREATE INDEX CONCURRENTLY IF NOT EXISTS ebachat_test_a ON Posts (UpdateAt, Id);",
        ),
        (
            "000002_ebachat_test_two.up.sql",
            "CREATE INDEX IF NOT EXISTS ebachat_test_b ON Posts (EditAt);\nCOMMENT ON INDEX ebachat_test_b IS 'test';",
        ),
        ("000002_ebachat_test_two.down.sql", "DROP INDEX IF EXISTS ebachat_test_b;"),
    ];
    migrate::run_sets(&db, &ours(&good), TIMEOUT).await.unwrap();
    let idx: Vec<String> =
        sqlx::query_scalar("SELECT indexname FROM pg_indexes WHERE indexname LIKE 'ebachat_test_%' ORDER BY 1")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(idx, ["ebachat_test_a", "ebachat_test_b"]);
    assert_eq!(
        records(&db, "ebachat_migrations").await,
        [(1, "ebachat_test_concurrently".to_string()), (2, "ebachat_test_two".to_string())]
    );
    assert_eq!(records(&db, "db_migrations").await, before, "Mattermost's record untouched");
    // Again: nothing pending.
    migrate::run_sets(&db, &ours(&good), TIMEOUT).await.unwrap();
    assert_eq!(records(&db, "ebachat_migrations").await.len(), 2);

    // A failing migration: its statements roll back, it is not recorded, the error says which, the lock is let go.
    let bad = [
        good[0],
        good[1],
        (
            "000003_ebachat_test_bad.up.sql",
            "CREATE INDEX ebachat_test_c ON Posts (Type);\nSELECT * FROM no_such_table;",
        ),
    ];
    let err = migrate::run_sets(&db, &ours(&bad), TIMEOUT).await.unwrap_err().to_string();
    assert!(err.contains("3 ebachat_test_bad"), "{err}");
    let c: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_indexes WHERE indexname = 'ebachat_test_c'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(c, 0, "rolled back");
    assert_eq!(records(&db, "ebachat_migrations").await.len(), 2);
    let t = Instant::now();
    migrate::run_sets(&db, &ours(&good), TIMEOUT).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(1), "the lock was released");
    drop_db(db, "migtest_ours").await;
}

#[tokio::test]
#[ignore]
async fn lock_waits_for_a_live_holder_and_takes_over_an_expired_one() {
    let db = fresh("migtest_lock", Some(SNAP_DB)).await;
    let now = ebachat_server::app::now_ms() / 1000;
    // A holder that died: its row expired a minute ago.
    sqlx::query("INSERT INTO db_lock (id, expireat) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET expireat = $2")
        .bind(migrate::LOCK_KEY)
        .bind(now - 60)
        .execute(&db)
        .await
        .unwrap();
    let t = Instant::now();
    migrate::run_sets(&db, &[], TIMEOUT).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(1), "expired lock taken over at once");
    // A live holder (a Go server migrating) for 3 more seconds: the run waits for it.
    sqlx::query("INSERT INTO db_lock (id, expireat) VALUES ($1, $2)")
        .bind(migrate::LOCK_KEY)
        .bind(now + 3)
        .execute(&db)
        .await
        .unwrap();
    let t = Instant::now();
    migrate::run_sets(&db, &[], TIMEOUT).await.unwrap();
    assert!(t.elapsed() >= Duration::from_secs(2), "waited for the holder: {:?}", t.elapsed());
    drop_db(db, "migtest_lock").await;
}

#[tokio::test]
#[ignore]
async fn pre_migration_renumbers_the_v10_11_cherry_picks() {
    let db = fresh("migtest_pre", Some(SNAP_DB)).await;
    // As a v10.11.17 database has them: 156-158 recorded as 142-144 (the real 142-144 not run yet), the fix not done.
    sqlx::query("DELETE FROM db_migrations WHERE Version IN (142, 143, 144)").execute(&db).await.unwrap();
    sqlx::query(
        "UPDATE db_migrations SET Version = Version - 14 WHERE (Version, Name) IN \
         ((156, 'add_schemeid_to_roles'), (157, 'backfill_roles_schemeid'), (158, 'add_roles_schemeid_index'))",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("DELETE FROM Systems WHERE Name = 'renumber_roles_schemeid_migrations'").execute(&db).await.unwrap();
    // Mattermost's files without the real 142-144 (their tables are already in this copy): only the pre-migration acts.
    let real = ["create_content_flagging_tables", "content_flagging_table_index", "add_dcr_fields_to_oauth_apps"];
    let mut files = migrate::builtin().unwrap().remove(0).files;
    files.retain(|m| !real.contains(&m.name.as_str()));
    migrate::run_sets(&db, &[Set { table: "db_migrations", files }], TIMEOUT).await.unwrap();
    let got = records(&db, "db_migrations").await;
    assert!(!got.iter().any(|(v, _)| (142..=144).contains(v)), "nothing left at 142-144");
    for (v, n) in [(156, "add_schemeid_to_roles"), (157, "backfill_roles_schemeid"), (158, "add_roles_schemeid_index")]
    {
        assert!(got.contains(&(v, n.to_string())), "{v} {n} renumbered back");
    }
    let done: String =
        sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = 'renumber_roles_schemeid_migrations'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(done, "true");
    drop_db(db, "migtest_pre").await;
}
