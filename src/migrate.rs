//! Schema migrations at startup, as the Go server runs them (morph v1.1.0 with Mattermost's options), so either
//! server can be the one that brings a database up to date and the other finds it as it expects.
//!
//! - `migrations/postgres/` (Mattermost's, verbatim) are recorded in `db_migrations (version, name)`, exactly as morph
//!   records them: a migration is pending when its name is not there, pending ones run in version order.
//! - `migrations/ebachat/` (ours: indexes and the like the Go server does not see) run the same way after them, and are
//!   recorded in their own `ebachat_migrations`, so their numbers never meet Mattermost's (migrations/README.md).
//!
//! Both run under morph's lock (the `db_lock` row `mm-lock-key`), so a Go and a Rust server starting together never
//! migrate at once. `EBACHAT_MIGRATIONS=off` skips them (Go's `skipMigrations`).

// Based on Mattermost server/channels/store/sqlstore/migrate.go and store.go (preMigration), Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0. Locking and applying as github.com/mattermost/morph v1.1.0
// (drivers/postgres, MIT).

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::json;
use sqlx::PgPool;

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

/// Morph's `TTL` of the lock row, and its refresh interval (half of it).
const LOCK_TTL: i64 = 15;
/// Go's lock key (`morph.WithLock("mm-lock-key")`).
pub const LOCK_KEY: &str = "mm-lock-key";
const NON_TRANSACTIONAL: &str = "-- morph:nontransactional";

/// One migration file.
#[derive(Debug, Clone)]
pub struct Migration {
    pub version: u32,
    pub name: String,
    pub up: bool,
    pub sql: String,
}

/// One set of migrations and the table that records them.
pub struct Set {
    pub table: &'static str,
    pub files: Vec<Migration>,
}

static FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([0-9]+)_(.*)\.(down|up)\.(.*)$").unwrap());

/// morph's `NewMigration`: `<version>_<name>.<up|down>.<ext>`.
pub fn parse(files: &[(&str, &str)]) -> anyhow::Result<Vec<Migration>> {
    files
        .iter()
        .map(|(file, sql)| {
            let c = FILE.captures(file).ok_or_else(|| anyhow::anyhow!("could not parse file: {file}"))?;
            Ok(Migration {
                version: c[1].parse::<u64>()? as u32,
                name: c[2].to_string(),
                up: &c[3] == "up",
                sql: sql.to_string(),
            })
        })
        .collect()
}

/// Mattermost's migrations, then ours.
pub fn builtin() -> anyhow::Result<Vec<Set>> {
    Ok(vec![
        Set { table: "db_migrations", files: parse(STOCK)? },
        Set { table: "ebachat_migrations", files: parse(EBACHAT)? },
    ])
}

/// morph's `computePendingMigrations` and `Apply`: the up migrations whose name is not applied, by version.
pub fn pending(applied: &[String], files: &[Migration]) -> anyhow::Result<Vec<Migration>> {
    if applied.len() > files.len() {
        anyhow::bail!("migration mismatch, there are more migrations applied than those were specified in source");
    }
    let mut out: Vec<Migration> = files.iter().filter(|m| m.up && !applied.contains(&m.name)).cloned().collect();
    out.sort_by_key(|m| m.version);
    Ok(out)
}

/// At startup: Go's `preMigration`, then every set, under the lock.
pub async fn run(db: &PgPool, statement_timeout_secs: i64) -> anyhow::Result<()> {
    if std::env::var("EBACHAT_MIGRATIONS").is_ok_and(|v| v == "off") {
        return Ok(());
    }
    run_sets(db, &builtin()?, statement_timeout_secs).await
}

/// The sets given, in order, under the lock (tests pass their own).
pub async fn run_sets(db: &PgPool, sets: &[Set], statement_timeout_secs: i64) -> anyhow::Result<()> {
    let lock = Lock::acquire(db, LOCK_KEY).await?;
    let result = async {
        pre_migration(db).await?;
        for set in sets {
            apply_set(db, set, statement_timeout_secs).await?;
        }
        anyhow::Ok(())
    }
    .await;
    lock.release().await;
    result
}

async fn apply_set(db: &PgPool, set: &Set, timeout: i64) -> anyhow::Result<()> {
    let table = set.table;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE IF NOT EXISTS {table} (version bigint not null primary key, name varchar not null)"
    )))
    .execute(db)
    .await?;
    let applied: Vec<String> =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT name FROM {table}"))).fetch_all(db).await?;
    for m in pending(&applied, &set.files)? {
        let start = std::time::Instant::now();
        crate::logs::info(
            &format!("== {}: migrating (up) ==", m.name),
            &[("table", json!(table)), ("version", json!(m.version))],
        );
        let apply = apply_one(db, table, &m);
        match tokio::time::timeout(Duration::from_secs(timeout.max(1) as u64), apply).await {
            Ok(r) => r.map_err(|e| anyhow::anyhow!("failed to apply migration {} {}: {e}", m.version, m.name))?,
            Err(_) => anyhow::bail!("migration {} {} timed out after {timeout}s", m.version, m.name),
        }
        crate::logs::info(
            &format!("== {}: migrated ({:.4}s) ==", m.name, start.elapsed().as_secs_f64()),
            &[("table", json!(table)), ("version", json!(m.version))],
        );
    }
    Ok(())
}

/// morph's postgres `Apply`: the file and its record in one transaction, or, for a file that starts with
/// `-- morph:nontransactional` (CREATE INDEX CONCURRENTLY), the file alone and then its record.
async fn apply_one(db: &PgPool, table: &str, m: &Migration) -> anyhow::Result<()> {
    let record = format!("INSERT INTO {table} (version, name) VALUES ($1, $2)");
    if m.sql.starts_with(NON_TRANSACTIONAL) {
        let mut c = db.acquire().await?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(m.sql.clone())).execute(&mut *c).await?;
        sqlx::query(sqlx::AssertSqlSafe(record)).bind(m.version as i64).bind(&m.name).execute(&mut *c).await?;
    } else {
        let mut tx = db.begin().await?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(m.sql.clone())).execute(&mut *tx).await?;
        sqlx::query(sqlx::AssertSqlSafe(record)).bind(m.version as i64).bind(&m.name).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Go's `preMigration`: fixes run once before the migrations, each marked done in Systems. On an empty database
/// (no `db_migrations` or no `Systems` yet) there is nothing to fix.
async fn pre_migration(db: &PgPool) -> anyhow::Result<()> {
    let exists = |t: &'static str| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE LOWER(table_name) = $1 AND table_schema = current_schema())",
        )
        .bind(t)
        .fetch_one(db)
        .await
    };
    if !exists("db_migrations").await? || !exists("systems").await? {
        return Ok(());
    }
    // (name, the schema migration that must be applied first, the fix)
    const FIXES: [(&str, i64, &str); 1] = [(
        "renumber_roles_schemeid_migrations",
        144,
        "UPDATE db_migrations
SET Version = CASE
    WHEN Version = 142 AND Name = 'add_schemeid_to_roles'    THEN 156
    WHEN Version = 143 AND Name = 'backfill_roles_schemeid'  THEN 157
    WHEN Version = 144 AND Name = 'add_roles_schemeid_index' THEN 158
END
WHERE (Version, Name) IN (
    (142, 'add_schemeid_to_roles'),
    (143, 'backfill_roles_schemeid'),
    (144, 'add_roles_schemeid_index')
)",
    )];
    for (name, min, fix) in FIXES {
        let applied: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM db_migrations WHERE Version >= $1)")
            .bind(min)
            .fetch_one(db)
            .await?;
        if !applied {
            continue;
        }
        let done: Option<String> =
            sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = $1").bind(name).fetch_optional(db).await?;
        if done.as_deref() == Some("true") {
            continue;
        }
        sqlx::query(fix).execute(db).await?;
        sqlx::query("INSERT INTO Systems (Name, Value) VALUES ($1, $2) ON CONFLICT (Name) DO UPDATE SET Value = $2")
            .bind(name)
            .bind("true")
            .execute(db)
            .await?;
    }
    Ok(())
}

/// morph's postgres `Mutex`: a `db_lock (id, expireat)` row held while refreshed every TTL/2; a row whose expireat
/// has passed (its holder died) is taken over.
pub struct Lock {
    db: PgPool,
    key: String,
    refresh: tokio::task::JoinHandle<()>,
}

impl Lock {
    pub async fn acquire(db: &PgPool, key: &str) -> anyhow::Result<Lock> {
        // Two servers starting on an empty database race on this CREATE ... IF NOT EXISTS, and Postgres fails the
        // loser ("type db_lock already exists"): the table is there all the same.
        if let Err(e) = sqlx::query("CREATE TABLE IF NOT EXISTS db_lock (id varchar(64) PRIMARY KEY, expireat bigint);")
            .execute(db)
            .await
        {
            let exists: bool = sqlx::query_scalar("SELECT to_regclass('db_lock') IS NOT NULL").fetch_one(db).await?;
            if !exists {
                return Err(e.into());
            }
        }
        let mut wait = Duration::ZERO;
        loop {
            tokio::time::sleep(wait).await;
            match Self::try_lock(db, key).await {
                Ok(true) => break,
                Ok(false) => wait = Duration::from_secs(1),
                // morph doubles the wait after an error, up to 5 minutes
                Err(_) => wait = (wait.max(Duration::from_secs(1)) * 2).min(Duration::from_secs(300)),
            }
            wait -= Duration::from_millis(rand::random::<u64>() % 500).min(wait);
        }
        let (db2, key2) = (db.clone(), key.to_string());
        let refresh = tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(LOCK_TTL as u64 * 500));
            t.tick().await;
            loop {
                t.tick().await;
                let r = sqlx::query("UPDATE db_lock SET expireat = expireat + $1 WHERE id = $2")
                    .bind(LOCK_TTL)
                    .bind(&key2)
                    .execute(&db2)
                    .await;
                if r.is_err() {
                    return;
                }
            }
        });
        Ok(Lock { db: db.clone(), key: key.to_string(), refresh })
    }

    /// One attempt: insert the row, or take over an expired one.
    async fn try_lock(db: &PgPool, key: &str) -> anyhow::Result<bool> {
        let now = crate::app::now_ms() / 1000;
        let inserted = sqlx::query("INSERT INTO db_lock (id, expireat) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING")
            .bind(key)
            .bind(now + LOCK_TTL)
            .execute(db)
            .await?
            .rows_affected();
        if inserted == 1 {
            return Ok(true);
        }
        let taken = sqlx::query("UPDATE db_lock SET expireat = $1 WHERE id = $2 AND expireat <= $3")
            .bind(now + LOCK_TTL)
            .bind(key)
            .bind(now)
            .execute(db)
            .await?
            .rows_affected();
        Ok(taken == 1)
    }

    /// Stops refreshing and deletes the row; if the delete fails the row still expires.
    pub async fn release(self) {
        self.refresh.abort();
        let _ = self.refresh.await;
        let _ = sqlx::query("DELETE FROM db_lock WHERE id = $1").bind(&self.key).execute(&self.db).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_as_morph() {
        let m = parse(&[("000001_create_teams.up.sql", "x"), ("000242_create_legacy_attribute_view.down.sql", "y")])
            .unwrap();
        assert_eq!((m[0].version, m[0].name.as_str(), m[0].up), (1, "create_teams", true));
        assert_eq!((m[1].version, m[1].name.as_str(), m[1].up), (242, "create_legacy_attribute_view", false));
        assert!(parse(&[("README.md", "")]).is_err());
    }

    #[test]
    fn pending_by_name_in_version_order() {
        let files = parse(&[
            ("000003_c.up.sql", ""),
            ("000001_a.up.sql", ""),
            ("000001_a.down.sql", ""),
            ("000002_b.up.sql", ""),
        ])
        .unwrap();
        let p = pending(&["a".into()], &files).unwrap();
        assert_eq!(p.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["b", "c"]);
        // Recorded under another number (Go's renumbered 142-144): still applied, by name.
        assert!(pending(&["a".into(), "b".into(), "c".into()], &files).unwrap().is_empty());
        assert!(pending(&(0..5).map(|i| i.to_string()).collect::<Vec<_>>(), &files).is_err());
    }

    #[test]
    fn builtin_sets_parse() {
        let sets = builtin().unwrap();
        assert_eq!(sets[0].table, "db_migrations");
        assert!(sets[0].files.iter().any(|m| m.version == 242 && m.up));
        assert!(sets[0].files.iter().filter(|m| m.sql.starts_with(NON_TRANSACTIONAL)).count() > 0);
    }
}
