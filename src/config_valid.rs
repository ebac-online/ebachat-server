//! `Config.IsValid`: the checks a config must pass to be saved, in Go's order.

// Based on Mattermost server/public/model/config.go (IsValid and the sections' isValid), Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use axum::http::StatusCode;
use serde_json::Value;

use crate::app::AppError;

type R = Result<(), AppError>;

/// A config with its defaults set, read by `Section.Setting`.
struct C<'a>(&'a Value);

impl C<'_> {
    fn v(&self, p: &str) -> &Value {
        static NULL: Value = Value::Null;
        p.split('.').try_fold(self.0, |v, k| v.get(k)).unwrap_or(&NULL)
    }
    fn s(&self, p: &str) -> &str {
        self.v(p).as_str().unwrap_or("")
    }
    fn b(&self, p: &str) -> bool {
        self.v(p).as_bool().unwrap_or(false)
    }
    fn i(&self, p: &str) -> i64 {
        self.v(p).as_i64().unwrap_or(0)
    }
    fn list(&self, p: &str) -> Vec<String> {
        self.v(p)
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    }
}

fn e(id: &str) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, id, id)
}

fn ep(id: &str, args: &[(&str, &str)]) -> AppError {
    let mut err = e(id);
    if let Some(m) = crate::i18n::tr(id, args) {
        err.message = m;
    }
    err
}

fn check(ok: bool, err: impl FnOnce() -> AppError) -> R {
    if ok { Ok(()) } else { Err(err()) }
}

fn positive(c: &C, p: &str) -> R {
    check(c.i(p) > 0, || ep("model.config.is_valid.positive_number.app_error", &[("Setting", p)]))
}

fn non_negative(c: &C, p: &str) -> R {
    check(c.i(p) >= 0, || ep("model.config.is_valid.non_negative_number.app_error", &[("Setting", p)]))
}

fn port_ok(n: i64, zero: bool) -> bool {
    if zero { (0..=65535).contains(&n) } else { (1..=65535).contains(&n) }
}

fn port_str_ok(s: &str, zero: bool) -> bool {
    s.parse::<i64>().is_ok_and(|n| port_ok(n, zero))
}

fn port(c: &C, p: &str, zero: bool) -> R {
    check(port_ok(c.i(p), zero), || ep("model.config.is_valid.port.app_error", &[("Setting", p)]))
}

/// `IsValidEmail`: lower case, one address.
fn email_ok(s: &str) -> bool {
    let Some((local, domain)) = s.rsplit_once('@') else { return false };
    s == s.to_lowercase()
        && !local.is_empty()
        && !domain.is_empty()
        && !s.chars().any(|c| c.is_whitespace() || "<>,;\"()[]\\".contains(c))
}

/// `IsValidHTTPURL`.
fn http_url_ok(s: &str) -> bool {
    (s.starts_with("http://") || s.starts_with("https://"))
        && reqwest::Url::parse(s).is_ok_and(|u| u.host_str().is_some_and(|h| !h.is_empty()))
}

/// `url.ParseRequestURI`: an absolute URL or an absolute path.
fn request_uri_ok(s: &str) -> bool {
    s.starts_with('/') || reqwest::Url::parse(s).is_ok()
}

/// `time.Parse("15:04", s)`.
fn clock_ok(s: &str) -> bool {
    let Some((h, m)) = s.split_once(':') else { return false };
    h.len() <= 2 && m.len() == 2 && h.parse::<u32>().is_ok_and(|h| h < 24) && m.parse::<u32>().is_ok_and(|m| m < 60)
}

/// `isValidLogLevelName`: mlog's standard levels.
fn log_level_ok(s: &str) -> bool {
    matches!(s.to_lowercase().as_str(), "panic" | "fatal" | "error" | "warn" | "info" | "debug" | "trace" | "stdlog")
}

/// The advanced logging setting must be JSON (an object, or empty).
fn advanced_logging(c: &C, p: &str) -> R {
    let v = c.v(p);
    let ok = match v {
        Value::Null | Value::Object(_) => true,
        Value::String(s) => s.trim().is_empty() || serde_json::from_str::<serde_json::Map<String, Value>>(s).is_ok(),
        _ => false,
    };
    check(ok, || ep("model.config.is_valid.log.advanced_logging.json", &[("Error", "invalid JSON")]))
}

/// `Config.IsValid` over a config with its defaults set.
pub fn is_valid(cfg: &Value) -> R {
    let c = &C(cfg);
    check(!(c.s("ServiceSettings.SiteURL").is_empty() && c.b("EmailSettings.EnableEmailBatching")), || {
        e("model.config.is_valid.site_url_email_batching.app_error")
    })?;
    check(!(c.b("ClusterSettings.Enable") && c.b("EmailSettings.EnableEmailBatching")), || {
        e("model.config.is_valid.cluster_email_batching.app_error")
    })?;
    // MetricsSettings
    let ids = c.list("MetricsSettings.ClientSideUserIds");
    check(ids.len() <= 5, || {
        ep(
            "model.config.is_valid.metrics_client_side_user_ids.app_error",
            &[("MaxLength", "5"), ("CurrentLength", &ids.len().to_string())],
        )
    })?;
    for id in &ids {
        check(crate::app::is_valid_id(id), || {
            ep("model.config.is_valid.metrics_client_side_user_id.app_error", &[("Id", id)])
        })?;
    }
    // ClusterSettings
    check(port_ok(c.i("ClusterSettings.GossipPort"), false), || {
        e("model.config.is_valid.cluster_gossip_port.app_error")
    })?;
    // CacheSettings
    let cache = c.s("CacheSettings.CacheType");
    check(cache == "lru" || cache == "redis", || e("model.config.is_valid.cache_type.app_error"))?;
    check(!(cache == "redis" && c.s("CacheSettings.RedisAddress").is_empty()), || {
        e("model.config.is_valid.empty_redis_address.app_error")
    })?;
    check(!(cache == "redis" && c.i("CacheSettings.RedisDB") < 0), || {
        e("model.config.is_valid.invalid_redis_db.app_error")
    })?;
    team_settings(c)?;
    check(c.i("ExperimentalSettings.LinkMetadataTimeoutMilliseconds") > 0, || {
        e("model.config.is_valid.link_metadata_timeout.app_error")
    })?;
    support_settings(c)?;
    positive(c, "AnnouncementSettings.NoticesFetchFrequency")?;
    sql_settings(c)?;
    file_settings(c)?;
    email_settings(c)?;
    ldap_settings(c)?;
    saml_settings(c)?;
    openid_settings(c)?;
    intune_settings(c)?;
    for p in [
        "NativeAppSettings.AppDownloadLink",
        "NativeAppSettings.AndroidAppDownloadLink",
        "NativeAppSettings.IosAppDownloadLink",
    ] {
        let l = c.s(p);
        if !l.is_empty() {
            let ok = reqwest::Url::parse(l)
                .is_ok_and(|u| !u.scheme().is_empty() && u.host_str().is_some_and(|h| !h.is_empty()));
            check(ok, || e("model.config.is_valid.native_app_settings.download_link.app_error"))?;
        }
    }
    if c.b("IntuneSettings.Enable") {
        match c.s("IntuneSettings.AuthService") {
            "office365" => check(c.b("Office365Settings.Enable"), || {
                e("model.config.is_valid.intune_requires_office365.app_error")
            })?,
            "saml" => check(c.b("SamlSettings.Enable"), || e("model.config.is_valid.intune_requires_saml.app_error"))?,
            _ => {}
        }
    }
    let len = c.i("PasswordSettings.MinimumLength");
    check((5..=72).contains(&len), || {
        ep("model.config.is_valid.password_length.app_error", &[("MinLength", "5"), ("MaxLength", "72")])
    })?;
    check(c.i("RateLimitSettings.MemoryStoreSize") > 0, || e("model.config.is_valid.rate_mem.app_error"))?;
    check(c.i("RateLimitSettings.PerSec") > 0, || e("model.config.is_valid.rate_sec.app_error"))?;
    check(c.i("RateLimitSettings.MaxBurst") > 0, || e("model.config.is_valid.max_burst.app_error"))?;
    service_settings(c)?;
    elasticsearch_settings(c)?;
    data_retention_settings(c)?;
    // LogSettings
    for (on, level) in
        [("LogSettings.EnableConsole", "LogSettings.ConsoleLevel"), ("LogSettings.EnableFile", "LogSettings.FileLevel")]
    {
        check(!c.b(on) || log_level_ok(c.s(level)), || {
            ep("model.config.is_valid.log_level.app_error", &[("Setting", level), ("Value", c.s(level))])
        })?;
    }
    non_negative(c, "LogSettings.MaxFieldSize")?;
    advanced_logging(c, "LogSettings.AdvancedLoggingJSON")?;
    // ExperimentalAuditSettings
    if c.b("ExperimentalAuditSettings.FileEnabled") {
        let f = c.s("ExperimentalAuditSettings.FileName");
        check(!f.is_empty(), || e("model.config.is_valid.experimental_audit_settings.file_name_empty"))?;
        check(!f.ends_with('\\'), || e("model.config.is_valid.experimental_audit_settings.file_name_is_directory"))?;
    }
    advanced_logging(c, "ExperimentalAuditSettings.AdvancedLoggingJSON")?;
    // LocalizationSettings
    let locales = c.s("LocalizationSettings.AvailableLocales");
    check(locales.is_empty() || locales.contains(c.s("LocalizationSettings.DefaultClientLocale")), || {
        e("model.config.is_valid.localization.available_locales.app_error")
    })?;
    auto_translation_settings(c)?;
    message_export_settings(c)?;
    let scheme = regex::Regex::new(r"(?i)^\s*[A-Za-z][A-Za-z0-9.+-]*\s*$").expect("regex");
    for s in c.list("DisplaySettings.CustomURLSchemes") {
        check(scheme.is_match(&s), || {
            ep("model.config.is_valid.display.custom_url_schemes.app_error", &[("Scheme", &s)])
        })?;
    }
    if c.b("ImageProxySettings.Enable") {
        match c.s("ImageProxySettings.ImageProxyType") {
            "local" => {}
            "atmos/camo" => return Err(e("model.config.is_valid.atmos_camo_image_proxy_removed.app_error")),
            _ => return Err(e("model.config.is_valid.image_proxy_type.app_error")),
        }
    }
    check(!c.s("ImportSettings.Directory").is_empty(), || e("model.config.is_valid.import.directory.app_error"))?;
    check(c.i("ImportSettings.RetentionDays") > 0, || {
        e("model.config.is_valid.import.retention_days_too_low.app_error")
    })?;
    check(!c.s("ExportSettings.Directory").is_empty(), || e("model.config.is_valid.export.directory.app_error"))?;
    check(c.i("ExportSettings.RetentionDays") > 0, || {
        e("model.config.is_valid.export.retention_days_too_low.app_error")
    })?;
    let domain = regex::Regex::new(
        r"^(([a-zA-Z]{1})|([a-zA-Z]{1}[a-zA-Z]{1})|([a-zA-Z]{1}[0-9]{1})|([0-9]{1}[a-zA-Z]{1})|([a-zA-Z0-9][a-zA-Z0-9-_]{1,61}[a-zA-Z0-9]))\.([a-zA-Z]{2,6}|[a-zA-Z0-9-]{2,30}\.[a-zA-Z]{2,3})$",
    )
    .expect("regex");
    for d in c.list("WranglerSettings.AllowedEmailDomain") {
        check(domain.is_match(&d) || d == "localhost", || {
            e("model.config.is_valid.move_thread.domain_invalid.app_error")
        })?;
    }
    for p in [
        "ConnectedWorkspacesSettings.GlobalUserSyncBatchSize",
        "ConnectedWorkspacesSettings.MaxPostsPerSync",
        "ConnectedWorkspacesSettings.MemberSyncBatchSize",
    ] {
        check(c.i(p) > 0, || ep("model.config.is_valid.connected_workspaces.batch_size.app_error", &[("Setting", p)]))?;
    }
    content_flagging_settings(c)?;
    delivery_and_recap_settings(c)?;
    if c.b("MobileEphemeralModeSettings.Enable") {
        for (p, max, id) in [
            ("DisconnectionTimeoutSeconds", 600, "disconnection_timeout"),
            ("OfflinePersistenceTimerHours", 72, "offline_persistence"),
            ("AutoCacheCleanupDays", 60, "auto_cache_cleanup"),
        ] {
            let n = c.i(&format!("MobileEphemeralModeSettings.{p}"));
            check((0..=max).contains(&n), || {
                ep(
                    &format!("model.config.is_valid.mobile_ephemeral_mode.{id}.app_error"),
                    &[("Min", "0"), ("Max", &max.to_string())],
                )
            })?;
        }
    }
    check(
        !(c.b("GuestAccountsSettings.EnableGuestMagicLink")
            && c.b("GuestAccountsSettings.EnforceMultifactorAuthentication")),
        || {
            e(
                "model.config.is_valid.guest_accounts.cannot_enforce_multifactor_authentication_when_guest_magic_link_is_enabled.app_error",
            )
        },
    )?;
    check(c.i("AccessControlSettings.SyncJobIntervalSeconds") >= 60, || {
        e("model.config.is_valid.access_control_sync_interval.app_error")
    })?;
    check(c.i("AccessControlSettings.AttributeRefreshIntervalSeconds") >= 0, || {
        e("model.config.is_valid.access_control_attribute_refresh_interval.app_error")
    })?;
    // FeatureFlags.isValid: retired features.
    check(!c.b("FeatureFlags.AppsEnabled"), || e("model.config.is_valid.feature_flags.apps_enabled.app_error"))?;
    check(!c.b("FeatureFlags.MoveThreadsEnabled"), || {
        e("model.config.is_valid.feature_flags.move_threads_enabled.app_error")
    })?;
    Ok(())
}

#[cfg(test)]
mod ldap_tests {
    use super::ldap_filter_ok;

    #[test]
    fn filters() {
        for ok in ["(a=b)", "(&(a=b)(c=*))", "(|(a=b*c)(!(d=e)))", "(cn:dn:=x)", "(a=\\2a)", "((a=b))", "(a~=b)"] {
            assert!(ldap_filter_ok(ok), "{ok}");
        }
        for bad in ["a=b", "(a=b", "(ab)", "(a=b))", "(a=\\zz)", "(&(a=b)", "(a=\\2)", ""] {
            assert!(!ldap_filter_ok(bad), "{bad}");
        }
    }
}

fn team_settings(c: &C) -> R {
    check(c.i("TeamSettings.MaxUsersPerTeam") > 0, || e("model.config.is_valid.max_users.app_error"))?;
    check(c.i("TeamSettings.MaxChannelsPerTeam") > 0, || e("model.config.is_valid.max_channels.app_error"))?;
    check(c.i("TeamSettings.UserStatusAwayTimeout") > 0, || {
        e("model.config.is_valid.user_status_away_timeout.app_error")
    })?;
    check(c.i("TeamSettings.MaxNotificationsPerChannel") > 0, || {
        e("model.config.is_valid.max_notify_per_channel.app_error")
    })?;
    check(matches!(c.s("TeamSettings.RestrictDirectMessage"), "any" | "team"), || {
        e("model.config.is_valid.restrict_direct_message.app_error")
    })?;
    check(matches!(c.s("TeamSettings.TeammateNameDisplay"), "full_name" | "nickname_full_name" | "username"), || {
        e("model.config.is_valid.teammate_name_display.app_error")
    })?;
    check(c.s("TeamSettings.SiteName").len() <= 30, || {
        ep("model.config.is_valid.sitename_length.app_error", &[("MaxLength", "30")])
    })?;
    check(matches!(c.s("TeamSettings.LockProfileFieldsForEmailUsers"), "none" | "name_and_username" | "all"), || {
        e("model.config.is_valid.lock_profile_fields.app_error")
    })
}

fn support_settings(c: &C) -> R {
    let email = c.s("SupportSettings.SupportEmail");
    check(email.is_empty() || email_ok(&email.to_lowercase()), || {
        ep("model.config.is_valid.email_address.app_error", &[("Setting", "SupportSettings.SupportEmail")])
    })?;
    non_negative(c, "SupportSettings.CustomTermsOfServiceReAcceptancePeriod")?;
    match c.s("SupportSettings.ReportAProblemType") {
        "email" => {
            let m = c.v("SupportSettings.ReportAProblemMail");
            check(!m.is_null(), || e("model.config.is_valid.report_a_problem_mail.missing.app_error"))?;
            check(email_ok(&c.s("SupportSettings.ReportAProblemMail").to_lowercase()), || {
                e("model.config.is_valid.report_a_problem_mail.invalid.app_error")
            })
        }
        "link" => {
            let l = c.v("SupportSettings.ReportAProblemLink");
            check(!l.is_null(), || e("model.config.is_valid.report_a_problem_link.missing.app_error"))?;
            check(http_url_ok(c.s("SupportSettings.ReportAProblemLink")), || {
                e("model.config.is_valid.report_a_problem_link.invalid.app_error")
            })
        }
        _ => Ok(()),
    }
}

fn sql_settings(c: &C) -> R {
    check(c.s("SqlSettings.DriverName") == "postgres", || e("model.config.is_valid.sql_driver.app_error"))?;
    check(c.i("SqlSettings.MaxIdleConns") > 0, || e("model.config.is_valid.sql_idle.app_error"))?;
    check(c.i("SqlSettings.ConnMaxLifetimeMilliseconds") >= 0, || {
        e("model.config.is_valid.sql_conn_max_lifetime_milliseconds.app_error")
    })?;
    check(c.i("SqlSettings.ConnMaxIdleTimeMilliseconds") >= 0, || {
        e("model.config.is_valid.sql_conn_max_idle_time_milliseconds.app_error")
    })?;
    check(c.i("SqlSettings.QueryTimeout") > 0, || e("model.config.is_valid.sql_query_timeout.app_error"))?;
    check(c.i("SqlSettings.AnalyticsQueryTimeout") > 0, || {
        e("model.config.is_valid.sql_analytics_query_timeout.app_error")
    })?;
    check(c.i("SqlSettings.MigrationsStatementTimeoutSeconds") >= 0, || {
        e("model.config.is_valid.sql_migrations_statement_timeout_seconds.app_error")
    })?;
    check(c.i("SqlSettings.ReplicaMonitorIntervalSeconds") > 0, || {
        e("model.config.is_valid.sql_replica_monitor_interval_seconds.app_error")
    })?;
    check(!c.s("SqlSettings.DataSource").is_empty(), || e("model.config.is_valid.sql_data_src.app_error"))?;
    check(c.i("SqlSettings.MaxOpenConns") > 0, || e("model.config.is_valid.sql_max_conn.app_error"))
}

const STORAGE_CLASSES: &[&str] = &[
    "STANDARD",
    "REDUCED_REDUNDANCY",
    "STANDARD_IA",
    "ONEZONE_IA",
    "INTELLIGENT_TIERING",
    "GLACIER",
    "DEEP_ARCHIVE",
    "OUTPOSTS",
    "GLACIER_IR",
    "SNOW",
    "EXPRESS_ONEZONE",
];

fn file_settings(c: &C) -> R {
    let f = |k: &str| format!("FileSettings.{k}");
    check(c.i(&f("MaxFileSize")) > 0, || e("model.config.is_valid.max_file_size.app_error"))?;
    positive(c, &f("MaxImageResolution"))?;
    check(c.i(&f("ExtractContentTimeout")) >= 0, || e("model.config.is_valid.extract_content_timeout.app_error"))?;
    let min_part = (5 * 1024 * 1024).to_string();
    for k in ["AmazonS3UploadPartSizeBytes", "ExportAmazonS3UploadPartSizeBytes"] {
        check(c.i(&f(k)) >= 5 * 1024 * 1024, || {
            ep("model.config.is_valid.s3_upload_part_size.app_error", &[("Setting", &f(k)), ("Min", &min_part)])
        })?;
    }
    let drivers = ["local", "amazons3", "azure"];
    check(drivers.contains(&c.s(&f("DriverName"))), || e("model.config.is_valid.file_driver.app_error"))?;
    check(drivers.contains(&c.s(&f("ExportDriverName"))), || e("model.config.is_valid.export_file_driver.app_error"))?;
    let salt = c.s(&f("PublicLinkSalt"));
    check(salt.is_empty() || salt.len() >= 32, || e("model.config.is_valid.file_salt.app_error"))?;
    let dir = c.s(&f("Directory"));
    check(!dir.is_empty(), || e("model.config.is_valid.directory.app_error"))?;
    let trimmed = |k: &str| -> R {
        let v = c.s(&f(k));
        check(v.trim() == v, || {
            ep("model.config.is_valid.directory_whitespace.app_error", &[("Setting", &f(k)), ("Value", v)])
        })
    };
    trimmed("Directory")?;
    let dec = c.i(&f("MaxImageDecoderConcurrency"));
    check(!(dec < -1 || dec == 0), || {
        ep("model.config.is_valid.image_decoder_concurrency.app_error", &[("Value", &dec.to_string())])
    })?;
    check(c.i(&f("AmazonS3RequestTimeoutMilliseconds")) > 0, || {
        ep("model.config.is_valid.amazons3_timeout.app_error", &[("Value", &dec.to_string())])
    })?;
    const MAX_AZURE: i64 = 10 * 60 * 1000;
    let account = regex::Regex::new("^[a-z0-9]{3,24}$").expect("regex");
    for (prefix, timeout_id, mode_id) in [
        ("", "model.config.is_valid.azure_timeout.app_error", "model.config.is_valid.azure_auth_mode.app_error"),
        (
            "Export",
            "model.config.is_valid.export_azure_timeout.app_error",
            "model.config.is_valid.export_azure_auth_mode.app_error",
        ),
    ] {
        if prefix == "Export" {
            let class = c.s(&f("ExportAmazonS3StorageClass"));
            check(class.is_empty() || STORAGE_CLASSES.contains(&class), || {
                ep("model.config.is_valid.storage_class.app_error", &[("Value", class)])
            })?;
        }
        let t = c.i(&f(&format!("{prefix}AzureRequestTimeoutMilliseconds")));
        check(t > 0 && t <= MAX_AZURE, || ep(timeout_id, &[("Value", &t.to_string())]))?;
        let mode = c.s(&f(&format!("{prefix}AzureAuthMode")));
        check(mode == "shared_key" || mode == "default_credential", || ep(mode_id, &[("Value", mode)]))?;
        let cloud_key = f(&format!("{prefix}AzureCloud"));
        let cloud = c.s(&cloud_key);
        check(matches!(cloud, "commercial" | "government" | "custom" | ""), || {
            ep("model.config.is_valid.azure_cloud.app_error", &[("Setting", &cloud_key), ("Value", cloud)])
        })?;
        let endpoint_key = f(&format!("{prefix}AzureEndpoint"));
        check(!(cloud == "custom" && c.s(&endpoint_key).is_empty()), || {
            ep("model.config.is_valid.azure_custom_endpoint.app_error", &[("Setting", &endpoint_key)])
        })?;
        let driver = c.s(&f(&format!("{prefix}DriverName")));
        let account_key = f(&format!("{prefix}AzureStorageAccount"));
        check(!(driver == "azure" && cloud != "custom" && !account.is_match(c.s(&account_key))), || {
            ep(
                "model.config.is_valid.azure_storage_account.app_error",
                &[("Setting", &account_key), ("Value", c.s(&account_key))],
            )
        })?;
        if prefix.is_empty() {
            let class = c.s(&f("AmazonS3StorageClass"));
            check(class.is_empty() || STORAGE_CLASSES.contains(&class), || {
                ep("model.config.is_valid.storage_class.app_error", &[("Value", class)])
            })?;
        }
        trimmed(&format!("{prefix}AmazonS3PathPrefix"))?;
        trimmed(&format!("{prefix}AzurePathPrefix"))?;
        let azure_prefix_key = f(&format!("{prefix}AzurePathPrefix"));
        check(!c.s(&azure_prefix_key).contains(".."), || {
            ep(
                "model.config.is_valid.directory_traversal.app_error",
                &[("Setting", &azure_prefix_key), ("Value", c.s(&azure_prefix_key))],
            )
        })?;
    }
    trimmed("ExportDirectory")
}

fn email_settings(c: &C) -> R {
    check(matches!(c.s("EmailSettings.ConnectionSecurity"), "" | "TLS" | "STARTTLS" | "PLAIN"), || {
        e("model.config.is_valid.email_security.app_error")
    })?;
    for k in ["FeedbackEmail", "ReplyToAddress"] {
        let v = c.s(&format!("EmailSettings.{k}"));
        check(v.is_empty() || email_ok(&v.to_lowercase()), || {
            ep("model.config.is_valid.email_address.app_error", &[("Setting", &format!("EmailSettings.{k}"))])
        })?;
    }
    check(port_str_ok(c.s("EmailSettings.SMTPPort"), false), || {
        ep("model.config.is_valid.port.app_error", &[("Setting", "EmailSettings.SMTPPort")])
    })?;
    check(c.i("EmailSettings.EmailBatchingBufferSize") > 0, || {
        e("model.config.is_valid.email_batching_buffer_size.app_error")
    })?;
    check(c.i("EmailSettings.EmailBatchingInterval") >= 30, || {
        e("model.config.is_valid.email_batching_interval.app_error")
    })?;
    check(c.i("EmailSettings.PushNotificationBuffer") > 0, || {
        e("model.config.is_valid.email_push_notification_buffer.app_error")
    })?;
    check(matches!(c.s("EmailSettings.EmailNotificationContentsType"), "full" | "generic"), || {
        e("model.config.is_valid.email_notification_contents_type.app_error")
    })
}

fn ldap_settings(c: &C) -> R {
    check(matches!(c.s("LdapSettings.ConnectionSecurity"), "" | "TLS" | "STARTTLS"), || {
        e("model.config.is_valid.ldap_security.app_error")
    })?;
    let on = c.b("LdapSettings.Enable");
    if on || c.b("LdapSettings.EnableSync") {
        port(c, "LdapSettings.LdapPort", false)?;
        positive(c, "LdapSettings.QueryTimeout")?;
    }
    check(c.i("LdapSettings.SyncIntervalMinutes") > 0, || e("model.config.is_valid.ldap_sync_interval.app_error"))?;
    check(c.i("LdapSettings.MaxPageSize") >= 0, || e("model.config.is_valid.ldap_max_page_size.app_error"))?;
    if on {
        check(!c.s("LdapSettings.LdapServer").is_empty(), || e("model.config.is_valid.ldap_server"))?;
        check(c.i("LdapSettings.MaximumLoginAttempts") > 0, || {
            e("model.config.is_valid.ldap_max_login_attempts.app_error")
        })?;
        for (k, id) in [
            ("BaseDN", "model.config.is_valid.ldap_basedn"),
            ("EmailAttribute", "model.config.is_valid.ldap_email"),
            ("UsernameAttribute", "model.config.is_valid.ldap_username"),
            ("IdAttribute", "model.config.is_valid.ldap_id"),
            ("LoginIdAttribute", "model.config.is_valid.ldap_login_id"),
        ] {
            check(!c.s(&format!("LdapSettings.{k}")).is_empty(), || e(id))?;
        }
        for (k, id) in [
            ("UserFilter", "ent.ldap.validate_filter.app_error"),
            ("GuestFilter", "ent.ldap.validate_guest_filter.app_error"),
            ("AdminFilter", "ent.ldap.validate_admin_filter.app_error"),
            ("GroupFilter", "ent.ldap.validate_group_filter.app_error"),
        ] {
            let v = c.s(&format!("LdapSettings.{k}"));
            check(v.is_empty() || ldap_filter_ok(v), || e(id))?;
        }
    }
    Ok(())
}

/// `ldap.CompileFilter` (github.com/mattermost/ldap, MIT): whether the filter compiles.
pub fn ldap_filter_ok(f: &str) -> bool {
    f.starts_with('(') && ldap_compile(f, 1).is_some_and(|p| p == f.len())
}

fn ldap_compile_set(f: &str, mut pos: usize) -> Option<usize> {
    while pos < f.len() && f.as_bytes()[pos] == b'(' {
        pos = ldap_compile(f, pos + 1)?;
    }
    if pos >= f.len() {
        return None;
    }
    Some(pos + 1)
}

/// `escapedStringToEncodedBytes`: `\` must be followed by two hex digits.
fn ldap_escapes_ok(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            if i + 3 > b.len() || !b[i + 1].is_ascii_hexdigit() || !b[i + 2].is_ascii_hexdigit() {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

fn ldap_compile(f: &str, pos: usize) -> Option<usize> {
    let c = f.get(pos..)?.chars().next()?;
    match c {
        '(' => ldap_compile(f, pos + 1).map(|p| p + 1),
        '&' | '|' => ldap_compile_set(f, pos + 1),
        '!' => ldap_compile(f, pos + 1),
        _ => {
            // 0: the attribute, 1: an extensible matching rule, 2: the condition.
            let (mut state, mut op, mut extensible) = (0, false, false);
            let mut cond = String::new();
            let mut p = pos;
            while p < f.len() {
                let rest = &f[p..];
                let ch = rest.chars().next()?;
                if ch == ')' {
                    break;
                }
                match state {
                    0 => {
                        let (skip, next, ext) = if rest.starts_with(":dn:=") {
                            (5, 2, true)
                        } else if rest.starts_with(":dn:") {
                            (4, 1, true)
                        } else if rest.starts_with(":=") {
                            (2, 2, true)
                        } else if ch == ':' {
                            (1, 1, true)
                        } else if ch == '=' {
                            (1, 2, false)
                        } else if rest.starts_with(">=") || rest.starts_with("<=") || rest.starts_with("~=") {
                            (2, 2, false)
                        } else {
                            (0, 0, false)
                        };
                        if skip == 0 {
                            p += ch.len_utf8();
                        } else {
                            op = true;
                            extensible = ext;
                            state = next;
                            p += skip;
                        }
                    }
                    1 => {
                        if rest.starts_with(":=") {
                            state = 2;
                            p += 2;
                        } else {
                            p += ch.len_utf8();
                        }
                    }
                    _ => {
                        cond.push(ch);
                        p += ch.len_utf8();
                    }
                }
            }
            if p >= f.len() || !op {
                return None;
            }
            let ok = if !extensible && cond == "*" {
                true
            } else if !extensible && cond.contains('*') {
                cond.split('*').all(ldap_escapes_ok)
            } else {
                ldap_escapes_ok(&cond)
            };
            ok.then_some(p + 1)
        }
    }
}

/// `SSOSettings.isValid` (OpenID): the additional scopes, RFC 6749 scope tokens.
fn openid_settings(c: &C) -> R {
    let scopes = c.s("OpenIdSettings.AdditionalScopes");
    let ok = scopes.len() <= 1024
        && scopes.split(' ').all(|s| {
            s.chars().all(|ch| ch == '\x21' || ('\x23'..='\x5B').contains(&ch) || ('\x5D'..='\x7E').contains(&ch))
        });
    check(ok, || ep("model.config.is_valid.sso_additional_scopes.app_error", &[("MaxLength", "1024")]))
}

/// `ContentFlaggingSettings.IsValid`: known events and targets, reviewers told of a flag, some reasons.
fn content_flagging_settings(c: &C) -> R {
    let mapping = c.v("ContentFlaggingSettings.NotificationSettings.EventTargetMapping");
    if let Some(m) = mapping.as_object() {
        for (event, targets) in m {
            check(matches!(event.as_str(), "flagged" | "assigned" | "removed" | "dismissed"), || {
                e("model.config.is_valid.notification_settings.invalid_event")
            })?;
            for t in targets.as_array().into_iter().flatten() {
                let t = t.as_str().unwrap_or("");
                check(matches!(t, "reviewers" | "author" | "reporter"), || {
                    e("model.config.is_valid.notification_settings.invalid_target").detail(format!("target: {t}"))
                })?;
            }
        }
    }
    let flagged = mapping.get("flagged").and_then(Value::as_array).cloned().unwrap_or_default();
    check(flagged.iter().any(|t| t.as_str() == Some("reviewers")), || {
        e("model.config.is_valid.notification_settings.reviewer_flagged_notification_disabled")
    })?;
    let reasons = c.v("ContentFlaggingSettings.AdditionalSettings.Reasons").as_array().map_or(0, Vec::len);
    check(reasons > 0, || e("model.config.is_valid.content_flagging.reasons_not_set.app_error"))
}

/// `DeliveryTrackingSettings.IsValid` and `AIRecapSettings.IsValid`.
fn delivery_and_recap_settings(c: &C) -> R {
    check(
        c.v("DeliveryTrackingSettings.Enable").is_boolean()
            && c.v("DeliveryTrackingSettings.EnableForAllChannels").is_boolean(),
        || e("model.delivery_tracking.is_valid.missing_toggle.app_error"),
    )?;
    if c.v("AIRecapSettings.DefaultLimits").is_object() {
        for (k, id) in [
            ("MaxRecapsPerDay", "max_recaps_per_day"),
            ("MaxScheduledRecaps", "max_scheduled_recaps"),
            ("MaxChannelsPerRecap", "max_channels_per_recap"),
            ("MaxPostsPerRecap", "max_posts_per_recap"),
            ("MaxTokensPerRecap", "max_tokens_per_recap"),
            ("MaxPostsPerDay", "max_posts_per_day"),
        ] {
            let v = c.v(&format!("AIRecapSettings.DefaultLimits.{k}"));
            check(v.as_i64().is_none_or(|n| n == -1 || n >= 1), || {
                e(&format!("model.config.is_valid.ai_recap.{id}.app_error"))
            })?;
        }
        let v = c.v("AIRecapSettings.DefaultLimits.CooldownMinutes");
        check(v.as_i64().is_none_or(|n| n >= 0), || e("model.config.is_valid.ai_recap.cooldown_minutes.app_error"))?;
    }
    Ok(())
}

fn saml_settings(c: &C) -> R {
    if !c.b("SamlSettings.Enable") {
        return Ok(());
    }
    let s = |k: &str| c.s(&format!("SamlSettings.{k}"));
    check(!s("IdpURL").is_empty() && http_url_ok(s("IdpURL")), || e("model.config.is_valid.saml_idp_url.app_error"))?;
    check(!s("IdpDescriptorURL").is_empty(), || e("model.config.is_valid.saml_idp_descriptor_url.app_error"))?;
    check(!s("IdpCertificateFile").is_empty(), || e("model.config.is_valid.saml_idp_cert.app_error"))?;
    check(!s("EmailAttribute").is_empty(), || e("model.config.is_valid.saml_email_attribute.app_error"))?;
    check(!s("UsernameAttribute").is_empty(), || e("model.config.is_valid.saml_username_attribute.app_error"))?;
    check(!s("ServiceProviderIdentifier").is_empty(), || {
        e("model.config.is_valid.saml_spidentifier_attribute.app_error")
    })?;
    if c.b("SamlSettings.Verify") {
        let u = s("AssertionConsumerServiceURL");
        check(!u.is_empty() && http_url_ok(u), || {
            e("model.config.is_valid.saml_assertion_consumer_service_url.app_error")
        })?;
    }
    if c.b("SamlSettings.Encrypt") {
        check(!s("PrivateKeyFile").is_empty(), || e("model.config.is_valid.saml_private_key.app_error"))?;
        check(!s("PublicCertificateFile").is_empty(), || e("model.config.is_valid.saml_public_cert.app_error"))?;
    }
    check(matches!(s("SignatureAlgorithm"), "RSAwithSHA1" | "RSAwithSHA256" | "RSAwithSHA512"), || {
        e("model.config.is_valid.saml_signature_algorithm.app_error")
    })?;
    check(matches!(s("CanonicalAlgorithm"), "Canonical1.0" | "Canonical1.1"), || {
        e("model.config.is_valid.saml_canonical_algorithm.app_error")
    })?;
    for (k, id) in [
        ("GuestAttribute", "model.config.is_valid.saml_guest_attribute.app_error"),
        ("AdminAttribute", "model.config.is_valid.saml_admin_attribute.app_error"),
    ] {
        let v = s(k);
        check(v.is_empty() || v.split('=').count() == 2, || e(id))?;
    }
    Ok(())
}

fn intune_settings(c: &C) -> R {
    if !c.b("IntuneSettings.Enable") {
        return Ok(());
    }
    let uuid = regex::Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
        .expect("regex");
    let tenant = c.s("IntuneSettings.TenantId");
    check(!tenant.is_empty(), || e("model.config.is_valid.intune_tenant_id.app_error"))?;
    check(uuid.is_match(tenant), || e("model.config.is_valid.intune_tenant_id_format.app_error"))?;
    let client = c.s("IntuneSettings.ClientId");
    check(!client.is_empty(), || e("model.config.is_valid.intune_client_id.app_error"))?;
    check(uuid.is_match(client), || e("model.config.is_valid.intune_client_id_format.app_error"))?;
    let auth = c.s("IntuneSettings.AuthService");
    check(!auth.is_empty(), || e("model.config.is_valid.intune_auth_service.app_error"))?;
    check(auth == "saml" || auth == "office365", || e("model.config.is_valid.intune_auth_service_invalid.app_error"))
}

/// `isDomainName` (Go's `net` rules, simplified to labels of letters, digits, `-` and `_`).
fn domain_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.trim_end_matches('.').split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

fn service_settings(c: &C) -> R {
    let s = |k: &str| c.s(&format!("ServiceSettings.{k}"));
    let i = |k: &str| c.i(&format!("ServiceSettings.{k}"));
    check(!(s("SiteURL").is_empty() && c.b("ServiceSettings.AllowCookiesForSubdomains")), || {
        e("model.config.is_valid.allow_cookies_for_subdomains.app_error")
    })?;
    check(matches!(s("ConnectionSecurity"), "" | "TLS"), || e("model.config.is_valid.webserver_security.app_error"))?;
    if s("ConnectionSecurity") == "TLS" && !c.b("ServiceSettings.UseLetsEncrypt") {
        let exists = |p: &str| !p.is_empty() && std::path::Path::new(p).exists();
        check(exists(s("TLSCertFile")), || e("model.config.is_valid.tls_cert_file_missing.app_error"))?;
        check(exists(s("TLSKeyFile")), || e("model.config.is_valid.tls_key_file_missing.app_error"))?;
    }
    check(matches!(s("TLSMinVer"), "1.0" | "1.1" | "1.2" | "1.3"), || {
        ep("model.config.is_valid.tls_min_ver.app_error", &[("Value", s("TLSMinVer"))])
    })?;
    non_negative(c, "ServiceSettings.TLSStrictTransportMaxAge")?;
    check(matches!(s("WebserverMode"), "gzip" | "uncompressed" | "disabled"), || {
        ep("model.config.is_valid.webserver_mode.app_error", &[("Value", s("WebserverMode"))])
    })?;
    port(c, "ServiceSettings.WebsocketPort", true)?;
    port(c, "ServiceSettings.WebsocketSecurePort", true)?;
    check(i("MaximumPayloadSizeBytes") > 0, || e("model.config.is_valid.max_payload_size.app_error"))?;
    check(i("MaximumURLLength") > 0, || e("model.config.is_valid.max_url_length.app_error"))?;
    for k in [
        "IdleTimeout",
        "SessionLengthMobileInDays",
        "SessionLengthMobileInHours",
        "SessionLengthSSOInDays",
        "SessionLengthSSOInHours",
        "SessionCacheInMinutes",
    ] {
        positive(c, &format!("ServiceSettings.{k}"))?;
    }
    non_negative(c, "ServiceSettings.SessionIdleTimeoutInMinutes")?;
    for k in ["MinimumHashtagLength", "ClusterLogTimeoutMilliseconds", "AWSMeteringTimeoutSeconds"] {
        positive(c, &format!("ServiceSettings.{k}"))?;
    }
    let sid = |k: &str| format!("model.config.is_valid.service_settings.{k}.app_error");
    check(i("FeatureFlagSyncIntervalSeconds") > 0, || e(&sid("feature_flag_sync_interval")))?;
    check(clock_ok(s("RefreshPostStatsRunTime")), || e(&sid("refresh_post_stats_run_time")))?;
    check(i("BurnOnReadDurationSeconds") > 0, || e(&sid("burn_on_read_duration_seconds")))?;
    check(i("BurnOnReadMaximumTimeToLiveSeconds") > 0, || e(&sid("burn_on_read_maximum_time_to_live_seconds")))?;
    check(i("BurnOnReadSchedulerFrequencySeconds") > 0, || e(&sid("burn_on_read_scheduler_frequency_seconds")))?;
    check(i("ReadTimeout") > 0, || e("model.config.is_valid.read_timeout.app_error"))?;
    check(i("WriteTimeout") > 0, || e("model.config.is_valid.write_timeout.app_error"))?;
    check(i("TimeBetweenUserTypingUpdatesMilliseconds") >= 1000, || {
        e("model.config.is_valid.time_between_user_typing.app_error")
    })?;
    check(i("MaximumLoginAttempts") > 0, || e("model.config.is_valid.login_attempts.app_error"))?;
    check(s("SiteURL").is_empty() || request_uri_ok(s("SiteURL")), || e("model.config.is_valid.site_url.app_error"))?;
    check(s("WebsocketURL").is_empty() || request_uri_ok(s("WebsocketURL")), || {
        e("model.config.is_valid.websocket_url.app_error")
    })?;
    let v = s("MinimumDesktopAppVersion");
    let semver_ok = v.split('.').count() == 3 && v.split(['.', '-', '+']).take(3).all(|p| p.parse::<u64>().is_ok());
    check(v.is_empty() || semver_ok, || e("model.config.is_valid.minimum_desktop_app_version.app_error"))?;
    let (host, port) = match s("ListenAddress").rsplit_once(':') {
        Some((h, p)) => (h.trim_start_matches('[').trim_end_matches(']'), p),
        None => ("", ""),
    };
    let host_ok = host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() || domain_ok(host);
    check(host_ok && port_str_ok(port, true), || e("model.config.is_valid.listen_address.app_error"))?;
    check(i("OutgoingIntegrationRequestsTimeout") > 0, || {
        e("model.config.is_valid.outgoing_integrations_request_timeout.app_error")
    })?;
    check(matches!(s("ExperimentalGroupUnreadChannels"), "disabled" | "default_on" | "default_off"), || {
        e("model.config.is_valid.group_unread_channels.app_error")
    })?;
    let crt = s("CollapsedThreads");
    check(crt == "disabled" || c.b("ServiceSettings.ThreadAutoFollow"), || {
        e("model.config.is_valid.collapsed_threads.autofollow.app_error")
    })?;
    check(matches!(crt, "disabled" | "default_on" | "always_on" | "default_off"), || {
        e("model.config.is_valid.collapsed_threads.app_error")
    })?;
    check(i("PersistentNotificationIntervalMinutes") >= 2, || {
        e("model.config.is_valid.persistent_notifications_interval.app_error")
    })?;
    check(i("PersistentNotificationMaxCount") > 0, || {
        e("model.config.is_valid.persistent_notifications_count.app_error")
    })?;
    check(i("PersistentNotificationMaxRecipients") > 0, || {
        e("model.config.is_valid.persistent_notifications_recipients.app_error")
    })?;
    for p in c.list("ServiceSettings.DCRRedirectURIAllowlist") {
        check(!p.trim().is_empty(), || e("model.config.is_valid.dcr_redirect_uri_allowlist.app_error"))?;
    }
    if c.b("ServiceSettings.EnableLocalMode") {
        let parent =
            std::path::Path::new(s("LocalModeSocketLocation")).parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let parent = if parent.as_os_str().is_empty() { std::path::PathBuf::from(".") } else { parent };
        check(parent.exists(), || e("model.config.is_valid.local_mode_socket.app_error"))?;
    }
    let days = i("MaximumPersonalAccessTokenLifetimeDays");
    check((0..=36500).contains(&days), || {
        ep("model.config.is_valid.max_personal_access_token_lifetime_days.app_error", &[("Max", "36500")])
    })
}

fn elasticsearch_settings(c: &C) -> R {
    let b = |k: &str| c.b(&format!("ElasticsearchSettings.{k}"));
    let s = |k: &str| c.s(&format!("ElasticsearchSettings.{k}"));
    let i = |k: &str| c.i(&format!("ElasticsearchSettings.{k}"));
    let id = |k: &str| format!("model.config.is_valid.elastic_search.{k}.app_error");
    check(!(b("EnableIndexing") && s("ConnectionURL").is_empty()), || e(&id("connection_url")))?;
    check(!(b("EnableSearching") && !b("EnableIndexing")), || {
        ep(
            &id("enable_searching"),
            &[
                ("Searching", "ElasticsearchSettings.EnableSearching"),
                ("EnableIndexing", "ElasticsearchSettings.EnableIndexing"),
            ],
        )
    })?;
    check(!(b("EnableCJKAnalyzers") && !b("EnableSearching")), || {
        ep(
            &id("enable_cjk_analyzers"),
            &[
                ("EnableCJKAnalyzers", "ElasticsearchSettings.EnableCJKAnalyzers"),
                ("Searching", "ElasticsearchSettings.EnableSearching"),
            ],
        )
    })?;
    check(!(b("EnableAutocomplete") && !b("EnableIndexing")), || {
        ep(
            &id("enable_autocomplete"),
            &[
                ("Autocomplete", "ElasticsearchSettings.EnableAutocomplete"),
                ("EnableIndexing", "ElasticsearchSettings.EnableIndexing"),
            ],
        )
    })?;
    check(i("AggregatePostsAfterDays") >= 1, || e(&id("aggregate_posts_after_days")))?;
    for k in ["PostIndexShards", "ChannelIndexShards", "UserIndexShards"] {
        check(i(k) >= 1, || ep(&id("index_shards"), &[("Setting", &format!("ElasticsearchSettings.{k}"))]))?;
    }
    for k in ["PostIndexReplicas", "ChannelIndexReplicas", "UserIndexReplicas"] {
        check(i(k) >= 0, || ep(&id("index_replicas"), &[("Setting", &format!("ElasticsearchSettings.{k}"))]))?;
    }
    check(clock_ok(s("PostsAggregatorJobStartTime")), || e(&id("posts_aggregator_job_start_time")))?;
    check(i("LiveIndexingBatchSize") >= 1, || e(&id("live_indexing_batch_size")))?;
    check(i("BatchSize") >= 1, || ep(&id("bulk_indexing_batch_size"), &[("BatchSize", "1")]))?;
    check(i("RequestTimeoutSeconds") >= 1, || e(&id("request_timeout_seconds")))?;
    check(!s("IgnoredPurgeIndexes").split(',').any(|x| x.starts_with('-')), || e(&id("ignored_indexes_dash_prefix")))?;
    check(matches!(s("Backend"), "opensearch" | "elasticsearch"), || e(&id("invalid_backend")))?;
    let (global, prefix) = (s("GlobalSearchPrefix"), s("IndexPrefix"));
    check(global.is_empty() || !prefix.is_empty(), || e(&id("empty_index_prefix")))?;
    check(global.is_empty() || prefix.starts_with(global), || {
        ep(&id("incorrect_search_prefix"), &[("IndexPrefix", prefix), ("GlobalSearchPrefix", global)])
    })
}

fn data_retention_settings(c: &C) -> R {
    let i = |k: &str| c.v(&format!("DataRetentionSettings.{k}")).as_i64();
    let id = |k: &str| format!("model.config.is_valid.data_retention.{k}.app_error");
    let (md, mh, fd, fh) =
        (i("MessageRetentionDays"), i("MessageRetentionHours"), i("FileRetentionDays"), i("FileRetentionHours"));
    check(md.is_some_and(|n| n >= 0), || e(&id("message_retention_days_too_low")))?;
    check(mh.is_some_and(|n| n >= 0), || e(&id("message_retention_hours_too_low")))?;
    check(fd.is_some_and(|n| n >= 0), || e(&id("file_retention_days_too_low")))?;
    check(fh.is_some_and(|n| n >= 0), || e(&id("file_retention_hours_too_low")))?;
    let (md, mh, fd, fh) = (md.unwrap_or(0), mh.unwrap_or(0), fd.unwrap_or(0), fh.unwrap_or(0));
    check(!(md > 0 && mh > 0), || e(&id("message_retention_misconfiguration")))?;
    check(!(fd > 0 && fh > 0), || e(&id("file_retention_misconfiguration")))?;
    check(!(md == 0 && mh == 0), || e(&id("message_retention_both_zero")))?;
    check(!(fd == 0 && fh == 0), || e(&id("file_retention_both_zero")))?;
    check(clock_ok(c.s("DataRetentionSettings.DeletionJobStartTime")), || e(&id("deletion_job_start_time")))?;
    positive(c, "DataRetentionSettings.BatchSize")?;
    non_negative(c, "DataRetentionSettings.TimeBetweenBatchesMilliseconds")?;
    positive(c, "DataRetentionSettings.RetentionIdsBatchSize")
}

fn auto_translation_settings(c: &C) -> R {
    if !c.b("AutoTranslationSettings.Enable") {
        return Ok(());
    }
    let id = |k: &str| format!("model.config.is_valid.autotranslation.{k}.app_error");
    match c.s("AutoTranslationSettings.Provider") {
        "" => return Err(e(&id("provider"))),
        "libretranslate" => {
            check(http_url_ok(c.s("AutoTranslationSettings.LibreTranslate.URL")), || e(&id("libretranslate.url")))?
        }
        "agents" => {
            check(!c.s("AutoTranslationSettings.Agents.LLMServiceID").is_empty(), || e(&id("agents.llm_service_id")))?
        }
        _ => return Err(e(&id("provider.unsupported"))),
    }
    let t = c.v("AutoTranslationSettings.TimeoutMs");
    check(t.is_null() || c.i("AutoTranslationSettings.TimeoutMs") > 0, || e(&id("timeout")))?;
    let w = c.v("AutoTranslationSettings.Workers");
    check(w.is_null() || (1..=64).contains(&c.i("AutoTranslationSettings.Workers")), || e(&id("workers")))
}

fn message_export_settings(c: &C) -> R {
    let id = |k: &str| format!("model.config.is_valid.message_export.{k}.app_error");
    check(!c.v("MessageExportSettings.EnableExport").is_null(), || e(&id("enable")))?;
    if !c.b("MessageExportSettings.EnableExport") {
        return Ok(());
    }
    let from = c.i("MessageExportSettings.ExportFromTimestamp");
    check(from >= 0 && from <= crate::app::now_ms(), || e(&id("export_from")))?;
    check(clock_ok(c.s("MessageExportSettings.DailyRunTime")), || e(&id("daily_runtime")))?;
    check(c.i("MessageExportSettings.BatchSize") >= 0, || e(&id("batch_size")))?;
    let format = c.s("MessageExportSettings.ExportFormat");
    check(matches!(format, "actiance" | "globalrelay" | "csv" | "globalrelay-zip"), || e(&id("export_type")))?;
    if format == "globalrelay" {
        let g = |k: &str| c.s(&format!("MessageExportSettings.GlobalRelaySettings.{k}"));
        let customer = g("CustomerType");
        check(matches!(customer, "A9" | "A10" | "CUSTOM"), || e(&id("global_relay.customer_type")))?;
        if customer == "CUSTOM" {
            check(!g("CustomSMTPServerName").is_empty() && !g("CustomSMTPPort").is_empty(), || {
                e(&id("global_relay.customer_type_custom"))
            })?;
            check(port_str_ok(g("CustomSMTPPort"), false), || e(&id("global_relay.custom_smtp_port")))?;
        }
        check(email_ok(&g("EmailAddress").to_lowercase()), || e(&id("global_relay.email_address")))?;
        check(!g("SMTPUsername").is_empty(), || e(&id("global_relay.smtp_username")))?;
        check(!g("SMTPPassword").is_empty(), || e(&id("global_relay.smtp_password")))?;
    }
    if format == "globalrelay" || format == "globalrelay-zip" {
        let sender = c.s("MessageExportSettings.GlobalRelaySettings.SenderAddress");
        check(sender.is_empty() || email_ok(sender), || e(&id("global_relay.sender_address")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn defaults_are_valid() {
        let mut v = crate::config::defaults().clone();
        // The defaults leave the data source empty; a server has one.
        v["SqlSettings"]["DataSource"] = "postgres://x".into();
        if let Err(e) = super::is_valid(&v) {
            panic!("{}: {}", e.id, e.message);
        }
        v["TeamSettings"]["MaxUsersPerTeam"] = 0.into();
        assert_eq!(super::is_valid(&v).unwrap_err().id, "model.config.is_valid.max_users.app_error");
    }
}
