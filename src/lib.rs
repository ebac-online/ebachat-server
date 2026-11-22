pub mod admin;
pub mod api;
pub mod app;
pub mod audit;
pub mod bots;
pub mod brand;
pub mod bulk_export;
pub mod bulk_import;
pub mod cache;
pub mod channel_admin;
pub mod channels;
pub mod channels_api;
pub mod command_admin;
pub mod commands;
pub mod commands_builtin;
pub mod compress;
pub mod config;
pub mod config_api;
pub mod config_order;
pub mod config_valid;
pub mod cors;
pub mod cpa;
pub mod db;
pub mod drafts;
pub mod email;
pub mod email_template;
pub mod emoji;
pub mod extract;
pub mod file_api;
pub mod files;
pub mod gojson;
pub mod gourl;
pub mod hub;
pub mod i18n;
pub mod images;
pub mod import_functions;
pub mod integrations;
pub mod job_workers;
pub mod jobs;
pub mod license;
pub mod logs;
pub mod mail;
pub mod markdown;
pub mod migrate;
pub mod model;
pub mod notify;
pub mod oauth;
pub mod oauth_provider;
pub mod opengraph;
pub mod outgoing;
pub mod password;
pub mod perm;
pub mod permanent;
pub mod plugins;
pub mod posts;
pub mod posts_more;
pub mod preferences;
pub mod properties;
pub mod push;
pub mod reactions;
pub mod reminders;
pub mod roles;
pub mod schemes;
pub mod search;
pub mod search_params;
pub mod sidebar;
pub mod sso;
pub mod status;
pub mod system;
pub mod team_admin;
pub mod teams;
pub mod terms;
pub mod threads;
pub mod tokens;
pub mod unlicensed;
pub mod unsupported;
pub mod user_admin;
pub mod user_agent;
pub mod user_edit;
pub mod user_image;
pub mod user_lists;
pub mod users;
pub mod web;
pub mod webhooks;

/// What the API reports as its version: Mattermost's, whose API this is.
pub const SERVER_VERSION: &str = "12.1.0";
pub const SERVER_VERSION_FULL: &str = "12.1.0";
/// The 12.1.0 release's commit, as the client config reports it.
pub const BUILD_HASH: &str = "243e3d0135203ab9166fe8f2873b07f697b06fd4";

/// A Go `MM_SQLSETTINGS_DATASOURCE` DSN for sqlx: parameters it does not know are dropped.
pub fn db_url(dsn: &str) -> String {
    let Some((base, query)) = dsn.split_once('?') else { return dsn.to_string() };
    let kept: Vec<&str> =
        query.split('&').filter(|kv| kv.starts_with("sslmode=") || kv.starts_with("application_name=")).collect();
    if kept.is_empty() { base.to_string() } else { format!("{base}?{}", kept.join("&")) }
}
