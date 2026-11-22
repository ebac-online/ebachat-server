//! A bulk export run directly, outside the job server, as Go's `mattermost export bulk --all-teams` runs it: a
//! pending job on the shared database would be claimed by whichever server polls first. Needs the stand:
//!
//! ```sh
//! MM_CONFIG=dev/run/go/config.json cargo test --release --test bulk_export_full -- --ignored --nocapture
//! ```
//!
//! The archive lands in `ExportSettings.Directory` as `<job id>_export.zip`; the path is printed.

use ebachat_server::{app::App, config::Config, db_url, jobs::Job};

#[tokio::test]
#[ignore]
async fn full_export() {
    let mut cfg = Config::load().await.unwrap();
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| db_url(cfg.str("SqlSettings.DataSource")));
    let db = sqlx::PgPool::connect(&url).await.unwrap();
    cfg.diagnostic_id = sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = 'DiagnosticId'")
        .fetch_optional(&db)
        .await
        .unwrap()
        .unwrap_or_default();
    let app = App::new(db, cfg).unwrap();
    let mut job = Job { id: ebachat_server::app::new_id(), r#type: "export_process".into(), ..Default::default() };
    let t = std::time::Instant::now();
    ebachat_server::bulk_export::export_job(&app, &mut job).await.unwrap();
    println!(
        "{}/{}_export.zip in {:?}, data {:?}",
        app.cfg.str("ExportSettings.Directory"),
        job.id,
        t.elapsed(),
        job.data
    );
}
