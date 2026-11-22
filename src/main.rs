use std::time::Duration;

use ebachat_server::{api, app::App, config::Config, db_url};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cfg = Config::load().await?;
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| db_url(cfg.str("SqlSettings.DataSource")));
    // SqlSettings.MaxOpenConns, unless DB_MAX_CONNECTIONS says otherwise.
    let max: u32 = std::env::var("DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| u32::try_from(cfg.i64("SqlSettings.MaxOpenConns")).ok().filter(|&n| n > 0).unwrap_or(100));
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await?;
    let listen = std::env::var("LISTEN").unwrap_or_else(|_| {
        let l = cfg.str("ServiceSettings.ListenAddress");
        if l.starts_with(':') { format!("0.0.0.0{l}") } else { l.to_string() }
    });
    ebachat_server::logs::configure(cfg.value());
    ebachat_server::logs::initializing();
    // As Go's store does at startup: pending schema migrations (Mattermost's, then ours) before anything reads.
    ebachat_server::migrate::run(&db, cfg.i64("SqlSettings.MigrationsStatementTimeoutSeconds")).await?;
    cfg.diagnostic_id = sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = 'DiagnosticId'")
        .fetch_optional(&db)
        .await?
        .unwrap_or_default();
    let app = App::new(db, cfg)?;
    app.hub.set_app(&app);
    ebachat_server::license::load(&app).await;
    ebachat_server::status::spawn_jobs(app.clone());
    ebachat_server::reminders::spawn(app.clone());
    ebachat_server::job_workers::startup(&app).await;
    ebachat_server::jobs::spawn(app.clone());
    ebachat_server::plugins::start(app.clone());
    ebachat_server::logs::startup(&app).await;
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    eprintln!("listening on {listen}");
    ebachat_server::logs::info(&format!("Server is listening on {}", listener.local_addr()?), &[]);
    axum::serve(listener, api::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
    Ok(())
}
