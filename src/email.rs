//! Email as Go sends it: the transactional emails (welcome, verification, password reset and change notices,
//! invitations, deactivation, tokens), notification emails for mentions and their batching, and the rate limits
//! on invitations.

// Based on Mattermost server/channels/app/email/{email.go,email_batching.go,notification_email.go,service.go,
// utils.go}, server/channels/app/notification_email.go and server/channels/utils/time.go, Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::app::{ApiResult, App, AppError, State, now_ms};
use crate::db::Table;
use crate::email_template::{V, render};
use crate::i18n::tl;
use crate::mail::{Mail, SmtpConfig};
use crate::model::{Post, User, channel};
use crate::push::PostNotification;

pub const TOKEN_PASSWORD_RECOVERY: &str = "password_recovery";
pub const TOKEN_VERIFY_EMAIL: &str = "verify_email";
pub const TOKEN_TEAM_INVITATION: &str = "team_invitation";
pub const TOKEN_GUEST_INVITATION: &str = "guest_invitation";
pub const TOKEN_GUEST_MAGIC_LINK_INVITATION: &str = "guest_magic_link_invitation";
pub const TOKEN_GUEST_MAGIC_LINK: &str = "guest_magic_link";

/// `a.Srv().Go`: email work after the answer, logged when it fails.
pub fn go<F, Fut>(app: &App, what: &'static str, f: F)
where
    F: FnOnce(State) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    if let Some(state) = app.hub.app() {
        tokio::spawn(async move {
            if let Err(e) = f(state).await {
                eprintln!("Failed to send {what}: {e}");
            }
        });
    }
}

/// `GetSiteURL`.
pub fn site_url(app: &App) -> String {
    app.cfg.str("ServiceSettings.SiteURL").to_string()
}

/// The site's name in emails: `TeamSettings.SiteName`, or the fallback (`crate::brand`).
fn site_name(_app: &App) -> String {
    crate::brand::name().to_string()
}

/// The locale of `i18n.T` (the server's) when none is given.
fn locale_or_server(app: &App, locale: &str) -> String {
    if locale.is_empty() {
        app.cfg.str("LocalizationSettings.DefaultServerLocale").to_string()
    } else {
        locale.to_string()
    }
}

/// `condenseSiteURL`: the host and path without the scheme.
fn condense(site: &str) -> String {
    let rest = site.split_once("://").map(|(_, r)| r).unwrap_or(site);
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, String::new()));
    if path.is_empty() || path == "/" { host.to_string() } else { format!("{host}{}", path.trim_end_matches('/')) }
}

/// `model.NewRandomString`: Mattermost's base32 alphabet over random bytes (ids are the same encoding).
pub fn random_string(len: usize) -> String {
    let mut out = String::with_capacity(len + 8);
    while out.len() < len {
        let id = crate::app::new_id();
        out.push_str(&id[..25]);
    }
    out.truncate(len);
    out
}

/// `Token().Save(model.NewToken(type, extra))`: the new token string.
pub async fn save_token(app: &App, kind: &str, extra: &str) -> ApiResult<String> {
    let token = random_string(64);
    sqlx::query("INSERT INTO Tokens (Token, CreateAt, Type, Extra) VALUES ($1, $2, $3, $4)")
        .bind(&token)
        .bind(now_ms())
        .bind(kind)
        .bind(extra)
        .execute(&app.db)
        .await?;
    Ok(token)
}

/// `model.MapToJSON`: keys sorted.
pub fn map_to_json(m: &BTreeMap<String, String>) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

// ---------- sending ----------

/// `mailServiceConfig(replyTo)`.
pub fn smtp_config(app: &App, reply_to: &str) -> SmtpConfig {
    SmtpConfig::from_config(app.cfg.get("EmailSettings"), &site_url(app), reply_to)
}

/// `sendMail` / `SendMailWithEmbeddedFiles`: an SMTP session of its own. The SendGrid category header needs a
/// cloud license, so none is set.
pub async fn send_mail(app: &App, m: Mail) -> Result<(), String> {
    let cfg = smtp_config(app, "");
    crate::mail::send(&m, &cfg).await
}

fn mail(to: &str, subject: String, html: String) -> Mail {
    Mail { to: to.to_string(), subject, html, ..Default::default() }
}

/// The template data: `NewEmailTemplateData(locale)` — the props every email has.
pub fn template_props(app: &App, locale: &str) -> BTreeMap<String, V> {
    let loc = locale_or_server(app, locale);
    let t = |id: &str| tl(&loc, id, &[]);
    let org = app.cfg.str("EmailSettings.FeedbackOrganization");
    let organization = if org.is_empty() { String::new() } else { t("api.templates.email_organization") + org };
    let mut p = BTreeMap::new();
    p.insert("EmailInfo1".into(), V::from(t("api.templates.email_info1")));
    p.insert("EmailInfo2".into(), V::from(t("api.templates.email_info2")));
    p.insert("EmailInfo3".into(), V::from(tl(&loc, "api.templates.email_info3", &[("SiteName", &site_name(app))])));
    p.insert("SupportEmail".into(), V::from(app.cfg.str("SupportSettings.SupportEmail")));
    p.insert("Footer".into(), V::from(t("api.templates.email_footer")));
    // Go's api.templates.email_footer_v2 is Mattermost, Inc.'s copyright and address: none here.
    p.insert("FooterV2".into(), V::from(""));
    p.insert("Organization".into(), V::from(organization));
    p
}

fn render_props(name: &str, props: BTreeMap<String, V>) -> String {
    let mut d = BTreeMap::new();
    d.insert("Props".to_string(), V::Map(props));
    d.insert("HTML".to_string(), V::Map(BTreeMap::new()));
    render(name, &V::Map(d))
}

/// A notice with a title, an info line and the warning (`email_change_body`, `password_change_body`, ...).
#[allow(clippy::too_many_arguments)]
async fn send_notice(
    app: &App,
    to: &str,
    locale: &str,
    subject: String,
    template: &str,
    title: String,
    info: String,
    warning_id: &str,
    category: &str,
) -> Result<(), String> {
    let mut p = template_props(app, locale);
    p.insert("SiteURL".into(), V::from(site_url(app)));
    p.insert("Title".into(), V::from(title));
    p.insert("Info".into(), V::from(info));
    p.insert("Warning".into(), V::from(tl(locale, warning_id, &[])));
    let _ = category;
    send_mail(app, mail(to, subject, render_props(template, p))).await
}

/// `SendChangeUsernameEmail`.
pub async fn send_change_username(app: &App, new_username: &str, email: &str, locale: &str) -> Result<(), String> {
    let sn = site_name(app);
    let subject = tl(locale, "api.templates.username_change_subject", &[("SiteName", &sn), ("TeamDisplayName", &sn)]);
    let title = tl(locale, "api.templates.username_change_body.title", &[]);
    let info = tl(
        locale,
        "api.templates.username_change_body.info",
        &[("TeamDisplayName", &sn), ("NewUsername", new_username)],
    );
    send_notice(
        app,
        email,
        locale,
        subject,
        "email_change_body",
        title,
        info,
        "api.templates.email_warning",
        "ChangeUsernameEmail",
    )
    .await
}

/// `SendEmailChangeVerifyEmail`.
pub async fn send_email_change_verify(app: &App, new_email: &str, locale: &str, token: &str) -> Result<(), String> {
    let sn = site_name(app);
    let site = site_url(app);
    let link = format!("{site}/do_verify_email?token={token}&email={}", query_escape(new_email));
    let subject =
        tl(locale, "api.templates.email_change_verify_subject", &[("SiteName", &sn), ("TeamDisplayName", &sn)]);
    let mut p = template_props(app, locale);
    let t = |id: &str| tl(locale, id, &[]);
    p.insert("SiteURL".into(), V::from(site.clone()));
    p.insert("Title".into(), V::from(t("api.templates.email_change_verify_body.title")));
    p.insert(
        "Info".into(),
        V::from(tl(locale, "api.templates.email_change_verify_body.info", &[("TeamDisplayName", &sn)])),
    );
    p.insert("VerifyUrl".into(), V::from(link));
    p.insert("VerifyButton".into(), V::from(t("api.templates.email_change_verify_body.button")));
    p.insert("QuestionTitle".into(), V::from(t("api.templates.questions_footer.title")));
    p.insert("EmailInfo1".into(), V::from(t("api.templates.email_us_anytime_at")));
    // Go names feedback@mattermost.com here; this server, the site's support address (the template shows the
    // line only when there is one).
    p.insert("SupportEmail".into(), V::from(app.cfg.str("SupportSettings.SupportEmail")));
    p.insert("FooterV2".into(), V::from(""));
    send_mail(app, mail(new_email, subject, render_props("email_change_verify_body", p))).await
}

/// `SendEmailChangeEmail`: the old address hears of the new one.
pub async fn send_email_change(app: &App, old_email: &str, new_email: &str, locale: &str) -> Result<(), String> {
    let sn = site_name(app);
    let subject = tl(locale, "api.templates.email_change_subject", &[("SiteName", &sn), ("TeamDisplayName", &sn)]);
    let title = tl(locale, "api.templates.email_change_body.title", &[]);
    let info = tl(locale, "api.templates.email_change_body.info", &[("TeamDisplayName", &sn), ("NewEmail", new_email)]);
    send_notice(
        app,
        old_email,
        locale,
        subject,
        "email_change_body",
        title,
        info,
        "api.templates.email_warning",
        "EmailChangeEmail",
    )
    .await
}

/// `url.QueryEscape`.
pub fn query_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// `SendVerifyEmail`.
pub async fn send_verify(app: &App, email: &str, locale: &str, token: &str, redirect: &str) -> Result<(), String> {
    let site = site_url(app);
    let mut link = format!("{site}/do_verify_email?token={token}&email={}", query_escape(email));
    if !redirect.is_empty() {
        link.push_str(&format!("&redirect_to={redirect}"));
    }
    let server = condense(&site);
    let t = |id: &str| tl(locale, id, &[]);
    let subject = tl(locale, "api.templates.verify_subject", &[("SiteName", &site_name(app))]);
    let mut p = template_props(app, locale);
    p.insert("SiteURL".into(), V::from(site.clone()));
    p.insert("Title".into(), V::from(t("api.templates.verify_body.title")));
    p.insert("SubTitle1".into(), V::from(t("api.templates.verify_body.subTitle1")));
    p.insert("ServerURL".into(), V::from(tl(locale, "api.templates.verify_body.serverURL", &[("ServerURL", &server)])));
    p.insert("SubTitle2".into(), V::from(t("api.templates.verify_body.subTitle2")));
    p.insert("ButtonURL".into(), V::from(link));
    p.insert("Button".into(), V::from(t("api.templates.verify_body.button")));
    p.insert("Info".into(), V::from(t("api.templates.verify_body.info")));
    p.insert("Info1".into(), V::from(t("api.templates.verify_body.info1")));
    p.insert("QuestionTitle".into(), V::from(t("api.templates.questions_footer.title")));
    p.insert("QuestionInfo".into(), V::from(t("api.templates.questions_footer.info")));
    send_mail(app, mail(email, subject, render_props("verify_body", p))).await
}

/// `SendSignInChangeEmail`.
pub async fn send_sign_in_change(app: &App, email: &str, method: &str, locale: &str) -> Result<(), String> {
    let sn = site_name(app);
    let subject = tl(locale, "api.templates.signin_change_email.subject", &[("SiteName", &sn)]);
    let title = tl(locale, "api.templates.signin_change_email.body.title", &[]);
    let info = tl(locale, "api.templates.signin_change_email.body.info", &[("SiteName", &sn), ("Method", method)]);
    send_notice(
        app,
        email,
        locale,
        subject,
        "signin_change_body",
        title,
        info,
        "api.templates.email_warning",
        "SignInChangeEmail",
    )
    .await
}

/// `SendWelcomeEmail`: none when the user opted out; an error when neither notifications nor verification are on.
pub async fn send_welcome(app: &App, u: &User, redirect: &str) -> Result<(), String> {
    if u.disable_welcome_email {
        return Ok(());
    }
    let require_verification = app.cfg.bool("EmailSettings.RequireEmailVerification");
    if !app.cfg.bool("EmailSettings.SendEmailNotifications") && !require_verification {
        return Err("send email notifications and require email verification is disabled in the system console".into());
    }
    let locale = u.locale.as_str();
    let site = site_url(app);
    let server = condense(&site);
    let t = |id: &str| tl(locale, id, &[]);
    let subject = tl(locale, "api.templates.welcome_subject", &[("SiteName", &site_name(app)), ("ServerURL", &server)]);
    let mut p = template_props(app, locale);
    p.insert("SiteURL".into(), V::from(site.clone()));
    p.insert("Title".into(), V::from(t("api.templates.welcome_body.title")));
    p.insert("SubTitle1".into(), V::from(t("api.templates.welcome_body.subTitle1")));
    p.insert(
        "ServerURL".into(),
        V::from(tl(locale, "api.templates.welcome_body.serverURL", &[("ServerURL", &server)])),
    );
    p.insert("SubTitle2".into(), V::from(t("api.templates.welcome_body.subTitle2")));
    p.insert("Button".into(), V::from(t("api.templates.welcome_body.button")));
    p.insert("Info".into(), V::from(t("api.templates.welcome_body.info")));
    p.insert("Info1".into(), V::from(t("api.templates.welcome_body.info1")));
    let download = app.cfg.str("NativeAppSettings.AppDownloadLink");
    if !download.is_empty() {
        p.insert("AppDownloadTitle".into(), V::from(t("api.templates.welcome_body.app_download_title")));
        p.insert("AppDownloadInfo".into(), V::from(t("api.templates.welcome_body.app_download_info")));
        p.insert("AppDownloadButton".into(), V::from(t("api.templates.welcome_body.app_download_button")));
        p.insert("AppDownloadLink".into(), V::from(download));
    }
    if !u.email_verified && require_verification {
        let token = create_verify_email_token(app, &u.id, &u.email).await.map_err(|e| e.message.clone())?;
        let mut link = format!("{site}/do_verify_email?token={token}&email={}", query_escape(&u.email));
        if !redirect.is_empty() {
            link.push_str(&format!("&redirect_to={redirect}"));
        }
        p.insert("ButtonURL".into(), V::from(link));
    }
    send_mail(app, mail(&u.email, subject, render_props("welcome_body", p))).await
}

/// `SendPasswordChangeEmail`.
pub async fn send_password_change(app: &App, email: &str, method: &str, locale: &str) -> Result<(), String> {
    let sn = site_name(app);
    let site = site_url(app);
    let subject = tl(locale, "api.templates.password_change_subject", &[("SiteName", &sn), ("TeamDisplayName", &sn)]);
    let title = tl(locale, "api.templates.password_change_body.title", &[]);
    let info = tl(
        locale,
        "api.templates.password_change_body.info",
        &[("TeamDisplayName", &sn), ("TeamURL", &site), ("Method", method)],
    );
    send_notice(
        app,
        email,
        locale,
        subject,
        "password_change_body",
        title,
        info,
        "api.templates.email_warning",
        "PasswordChangeEmail",
    )
    .await
}

/// `SendUserAccessTokenAddedEmail`.
pub async fn send_access_token_added(app: &App, email: &str, locale: &str) -> Result<(), String> {
    let sn = site_name(app);
    let site = site_url(app);
    let subject = tl(locale, "api.templates.user_access_token_subject", &[("SiteName", &sn)]);
    let title = tl(locale, "api.templates.user_access_token_body.title", &[]);
    let info = tl(locale, "api.templates.user_access_token_body.info", &[("SiteName", &sn), ("SiteURL", &site)]);
    send_notice(
        app,
        email,
        locale,
        subject,
        "password_change_body",
        title,
        info,
        "api.templates.email_warning",
        "UserAccessTokenAddedEmail",
    )
    .await
}

/// `SendUserAccessTokenRotatedEmail`.
pub async fn send_access_token_rotated(app: &App, email: &str, locale: &str) -> Result<(), String> {
    let sn = site_name(app);
    let site = site_url(app);
    let subject = tl(locale, "api.templates.user_access_token_rotated_subject", &[("SiteName", &sn)]);
    let title = tl(locale, "api.templates.user_access_token_rotated_body.title", &[]);
    let info =
        tl(locale, "api.templates.user_access_token_rotated_body.info", &[("SiteName", &sn), ("SiteURL", &site)]);
    send_notice(
        app,
        email,
        locale,
        subject,
        "password_change_body",
        title,
        info,
        "api.templates.email_warning",
        "UserAccessTokenRotatedEmail",
    )
    .await
}

/// `SendPasswordResetEmail`.
pub async fn send_password_reset(app: &App, email: &str, token: &str, locale: &str) -> Result<(), String> {
    let site = site_url(app);
    let link = format!("{site}/reset_password_complete?token={}", query_escape(token));
    let t = |id: &str| tl(locale, id, &[]);
    let subject = tl(locale, "api.templates.reset_subject", &[("SiteName", &site_name(app))]);
    let mut p = template_props(app, locale);
    p.insert("SiteURL".into(), V::from(site));
    p.insert("Title".into(), V::from(t("api.templates.reset_body.title")));
    p.insert("SubTitle".into(), V::from(t("api.templates.reset_body.subTitle")));
    p.insert("Info".into(), V::from(t("api.templates.reset_body.info")));
    p.insert("ButtonURL".into(), V::from(link));
    p.insert("Button".into(), V::from(t("api.templates.reset_body.button")));
    p.insert("QuestionTitle".into(), V::from(t("api.templates.questions_footer.title")));
    p.insert("QuestionInfo".into(), V::from(t("api.templates.questions_footer.info")));
    send_mail(app, mail(email, subject, render_props("reset_body", p))).await
}

/// `SendMfaChangeEmail`.
pub async fn send_mfa_change(app: &App, email: &str, activated: bool, locale: &str) -> Result<(), String> {
    let site = site_url(app);
    let subject = tl(locale, "api.templates.mfa_change_subject", &[("SiteName", &site_name(app))]);
    let (info, title) = if activated {
        ("api.templates.mfa_activated_body.info", "api.templates.mfa_activated_body.title")
    } else {
        ("api.templates.mfa_deactivated_body.info", "api.templates.mfa_deactivated_body.title")
    };
    let info = tl(locale, info, &[("SiteURL", &site)]);
    let title = tl(locale, title, &[]);
    send_notice(
        app,
        email,
        locale,
        subject,
        "mfa_change_body",
        title,
        info,
        "api.templates.email_warning",
        "MfaChangeEmail",
    )
    .await
}

/// `SendDeactivateAccountEmail`.
pub async fn send_deactivate(app: &App, email: &str, locale: &str) -> Result<(), String> {
    let site = site_url(app);
    let server = condense(&site);
    let subject =
        tl(locale, "api.templates.deactivate_subject", &[("SiteName", &site_name(app)), ("ServerURL", &server)]);
    let title = tl(locale, "api.templates.deactivate_body.title", &[("ServerURL", &server)]);
    let info = tl(locale, "api.templates.deactivate_body.info", &[("SiteURL", &site)]);
    send_notice(
        app,
        email,
        locale,
        subject,
        "deactivate_body",
        title,
        info,
        "api.templates.deactivate_body.warning",
        "DeactivateAccountEmail",
    )
    .await
}

/// `SendNotificationMail` (plugins' `SendMail`).
pub async fn send_notification_mail(app: &App, to: &str, subject: &str, html: &str) -> Result<(), String> {
    if !app.cfg.bool("EmailSettings.SendEmailNotifications") {
        return Ok(());
    }
    send_mail(app, mail(to, subject.to_string(), html.to_string())).await
}

// ---------- tokens ----------

/// `CreateVerifyEmailToken`: the user's other verification tokens go first.
pub async fn create_verify_email_token(app: &App, user_id: &str, email: &str) -> ApiResult<String> {
    let extra = format!(
        r#"{{"UserId":{},"Email":{}}}"#,
        serde_json::to_string(user_id).unwrap(),
        serde_json::to_string(email).unwrap()
    );
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT Token, COALESCE(Extra, '') FROM Tokens WHERE Type = $1")
        .bind(TOKEN_VERIFY_EMAIL)
        .fetch_all(&app.db)
        .await
        .map_err(|e| AppError::internal("api.user.invalidate_verify_email_tokens.error", e.to_string()))?;
    for (token, extra) in rows {
        let v: Value = serde_json::from_str(&extra).unwrap_or_default();
        if v["UserId"].as_str() == Some(user_id) {
            let _ = sqlx::query("DELETE FROM Tokens WHERE Token = $1").bind(&token).execute(&app.db).await;
        }
    }
    save_token(app, TOKEN_VERIFY_EMAIL, &extra)
        .await
        .map_err(|e| AppError::internal("app.recover.save.app_error", e.detailed_error))
}

/// `SendEmailVerification`: a verify email to a user who never logged in (no status), a change-verify email
/// otherwise.
pub async fn send_email_verification(app: &App, u: &User, new_email: &str, redirect: &str) -> ApiResult<()> {
    let token = create_verify_email_token(app, &u.id, new_email).await?;
    let has_status = app.status(&u.id).await?.is_some();
    if !has_status {
        return send_verify(app, new_email, &u.locale, &token, redirect)
            .await
            .map_err(|e| AppError::internal("api.user.send_verify_email_and_forget.failed.error", e));
    }
    send_email_change_verify(app, new_email, &u.locale, &token)
        .await
        .map_err(|e| AppError::internal("api.user.send_email_change_verify_email_and_forget.error", e))
}

/// `CreatePasswordRecoveryToken`: the user's other recovery tokens go first.
pub async fn create_password_recovery_token(app: &App, user_id: &str, email: &str) -> ApiResult<String> {
    let extra = format!(
        r#"{{"UserId":{},"Email":{}}}"#,
        serde_json::to_string(user_id).unwrap(),
        serde_json::to_string(email).unwrap()
    );
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT Token, COALESCE(Extra, '') FROM Tokens WHERE Type = $1")
        .bind(TOKEN_PASSWORD_RECOVERY)
        .fetch_all(&app.db)
        .await?;
    for (token, extra) in rows {
        let v: Value = serde_json::from_str(&extra).unwrap_or_default();
        if v["UserId"].as_str() == Some(user_id) {
            let _ = sqlx::query("DELETE FROM Tokens WHERE Token = $1").bind(&token).execute(&app.db).await;
        }
    }
    save_token(app, TOKEN_PASSWORD_RECOVERY, &extra)
        .await
        .map_err(|e| AppError::internal("app.recover.save.app_error", e.detailed_error))
}

// ---------- rate limits ----------

/// `throttled`'s GCRA limiter: `per` per `period` with a burst, by key.
struct Limiter {
    emission_ms: i64,
    tolerance_ms: i64,
    tat: Mutex<HashMap<String, i64>>,
}

impl Limiter {
    fn new(per: i64, period_ms: i64, burst: i64) -> Limiter {
        let emission_ms = period_ms / per;
        Limiter { emission_ms, tolerance_ms: emission_ms * (burst + 1), tat: Mutex::new(HashMap::new()) }
    }
    /// Whether `quantity` more are refused now.
    fn limited(&self, key: &str, quantity: i64) -> bool {
        let now = now_ms();
        let mut m = self.tat.lock().unwrap();
        if m.len() > 65_536 {
            m.retain(|_, t| *t > now);
        }
        let tat = m.get(key).copied().unwrap_or(now).max(now);
        let new_tat = tat + self.emission_ms * quantity;
        if now < new_tat - self.tolerance_ms {
            return true;
        }
        m.insert(key.to_string(), new_tat);
        false
    }
}

fn per_hour() -> &'static Limiter {
    static L: OnceLock<Limiter> = OnceLock::new();
    L.get_or_init(|| Limiter::new(20, 3_600_000, 20))
}

fn per_minute() -> &'static Limiter {
    static L: OnceLock<Limiter> = OnceLock::new();
    L.get_or_init(|| Limiter::new(60, 60_000, 20))
}

/// Why invitations were not sent (`email.RateLimitExceededError`, `SendMailError`).
#[derive(Debug, PartialEq)]
pub enum InviteError {
    RateLimited,
    SendFailed,
}

// ---------- invitations ----------

/// `InviteEmailData`.
pub struct InviteData<'a> {
    pub team: &'a crate::model::Team,
    pub channels: Vec<(String, String)>,
    pub sender_name: String,
    pub sender_id: String,
    pub sender_image: Option<Vec<u8>>,
    pub invites: Vec<String>,
    pub profiles: HashMap<String, (String, String, String)>,
    pub message: String,
    pub reminder_interval: Option<String>,
    pub error_when_not_sent: bool,
    pub is_system_admin: bool,
    pub is_first_admin: bool,
}

fn started_by_role(first_admin: bool, admin: bool) -> &'static str {
    if first_admin {
        "fa"
    } else if admin {
        "sa"
    } else {
        "su"
    }
}

/// `url.Values.Encode` of the signup link: keys sorted, values query-escaped.
fn encode_query(pairs: &mut [(&str, String)]) -> String {
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs.iter().map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v))).collect::<Vec<_>>().join("&")
}

/// bluemonday's strict policy: every tag dropped, the text kept and escaped.
pub fn strip_tags(s: &str) -> String {
    use html5ever::tendril::TendrilSink;
    let dom = html5ever::parse_document(markup5ever_rcdom::RcDom::default(), Default::default()).one(s);
    fn text(n: &markup5ever_rcdom::Handle, out: &mut String) {
        match &n.data {
            markup5ever_rcdom::NodeData::Text { contents } => out.push_str(&contents.borrow()),
            markup5ever_rcdom::NodeData::Element { name, .. } if matches!(&*name.local, "script" | "style") => {}
            _ => {
                for c in n.children.borrow().iter() {
                    text(c, out);
                }
            }
        }
    }
    let mut out = String::new();
    text(&dom.document, &mut out);
    html_escape::encode_text(&out).replace('"', "&#34;").replace('\'', "&#39;")
}

fn invite_props(app: &App, sender_name: &str) -> BTreeMap<String, V> {
    let t = |id: &str| tl(&locale_or_server(app, ""), id, &[]);
    let mut p = template_props(app, "");
    p.insert("SubTitle".into(), V::from(t("api.templates.invite_body.subTitle")));
    p.insert("Button".into(), V::from(t("api.templates.invite_body.button")));
    p.insert("SenderName".into(), V::from(sender_name));
    p.insert("InviteFooterTitle".into(), V::from(t("api.templates.invite_body_footer.title")));
    p.insert("InviteFooterInfo".into(), V::from(t("api.templates.invite_body_footer.info")));
    p.insert("InviteFooterLearnMore".into(), V::from(t("api.templates.invite_body_footer.learn_more")));
    p
}

fn post_data(sender_name: &str, message: &str, photo: &str) -> V {
    let mut m = BTreeMap::new();
    m.insert("SenderName".to_string(), V::from(sender_name));
    m.insert("Message".to_string(), V::Html(message.to_string()));
    m.insert("SenderPhoto".to_string(), V::from(photo));
    V::Map(m)
}

/// `SendInviteEmails` (no channels) and `SendInviteEmailsToTeamAndChannels`: one token and email per address.
/// The failed addresses with their errors when `error_when_not_sent`.
pub async fn send_invites(app: &App, d: InviteData<'_>) -> Result<Vec<(String, String)>, InviteError> {
    if per_hour().limited(&d.sender_id, d.invites.len() as i64) {
        eprintln!("rate limit exceeded user_id={} team_id={}", d.sender_id, d.team.id);
        return Err(InviteError::RateLimited);
    }
    let server = locale_or_server(app, "");
    let site = site_url(app);
    let sn = site_name(app);
    let with_channels = !d.channels.is_empty();
    let count = d.channels.len().to_string();
    let (subject, title_base, category) = if !with_channels {
        let s = tl(
            &server,
            "api.templates.invite_subject",
            &[("SenderName", &d.sender_name), ("TeamDisplayName", &d.team.display_name), ("SiteName", &sn)],
        );
        let t = tl(
            &server,
            "api.templates.invite_body.title",
            &[("SenderName", &d.sender_name), ("TeamDisplayName", &d.team.display_name)],
        );
        (s, t, "InviteEmail")
    } else if d.channels.len() == 1 {
        let ch = &d.channels[0].1;
        let s = tl(
            &server,
            "api.templates.invite_team_and_channel_subject",
            &[
                ("SenderName", &d.sender_name),
                ("TeamDisplayName", &d.team.display_name),
                ("ChannelName", ch),
                ("SiteName", &sn),
            ],
        );
        let t = tl(
            &server,
            "api.templates.invite_team_and_channel_body.title",
            &[("SenderName", &d.sender_name), ("ChannelName", ch), ("TeamDisplayName", &d.team.display_name)],
        );
        (s, t, "InviteEmailToTeamsAndChannels")
    } else {
        let s = tl(
            &server,
            "api.templates.invite_team_and_channels_subject",
            &[
                ("SenderName", &d.sender_name),
                ("TeamDisplayName", &d.team.display_name),
                ("ChannelsLen", &count),
                ("SiteName", &sn),
            ],
        );
        let t = tl(
            &server,
            "api.templates.invite_team_and_channels_body.title",
            &[("SenderName", &d.sender_name), ("ChannelsLen", &count), ("TeamDisplayName", &d.team.display_name)],
        );
        (s, t, "InviteEmailToTeamsAndChannels")
    };
    let _ = category;
    let message = if with_channels && !d.message.is_empty() { strip_tags(&d.message) } else { d.message.clone() };
    let mut failed = vec![];
    let mut title = title_base.clone();
    for invite in d.invites.iter().filter(|i| !i.is_empty()) {
        let mut p = invite_props(app, &d.sender_name);
        p.insert("SiteURL".into(), V::from(site.clone()));
        let mut extra: BTreeMap<String, String> = BTreeMap::new();
        extra.insert("teamId".into(), d.team.id.clone());
        extra.insert("email".into(), invite.clone());
        let mut props: BTreeMap<String, String> = BTreeMap::new();
        props.insert("email".into(), invite.clone());
        props.insert("display_name".into(), d.team.display_name.clone());
        props.insert("name".into(), d.team.name.clone());
        if let Some((u, f, l)) = d.profiles.get(invite) {
            for (k, v) in [("username", u), ("first_name", f), ("last_name", l)] {
                extra.insert(k.into(), v.clone());
                props.insert(k.into(), v.clone());
            }
        }
        if with_channels {
            p.insert("Message".into(), V::from(message.clone()));
            extra.insert("channels".into(), d.channels.iter().map(|c| c.0.clone()).collect::<Vec<_>>().join(" "));
            extra.insert("senderId".into(), d.sender_id.clone());
        }
        if let Some(interval) = &d.reminder_interval {
            // Go prefixes the title once more for every address when inviting to channels; once here.
            let reminder = tl(&server, "api.templates.invite_body.title.reminder", &[]);
            title = format!("{reminder}: {title_base}");
            props.insert("reminder_interval".into(), interval.clone());
        }
        p.insert("Title".into(), V::from(title.clone()));
        let token = match save_token(app, TOKEN_TEAM_INVITATION, &map_to_json(&extra)).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Failed to send invite email successfully: {}", e.detailed_error);
                continue;
            }
        };
        let token_data = map_to_json(&props);
        let sbr = started_by_role(d.is_first_admin, d.is_system_admin);
        let button = if with_channels {
            format!("{site}/signup_user_complete/?d={}&t={}&sbr={sbr}", query_escape(&token_data), query_escape(&token))
        } else {
            let mut q = [
                ("d", token_data.clone()),
                ("t", token.clone()),
                ("md", "email".to_string()),
                ("sbr", sbr.to_string()),
            ];
            format!("{site}/signup_user_complete/?{}", encode_query(&mut q))
        };
        p.insert("ButtonURL".into(), V::from(button));
        let mut embedded = vec![];
        if with_channels {
            let mut photo = "";
            if !message.is_empty()
                && let Some(img) = &d.sender_image
            {
                photo = "user-avatar.png";
                embedded.push(("user-avatar.png".to_string(), img.clone()));
            }
            p.insert("Posts".into(), V::List(vec![post_data(&d.sender_name, &message, photo)]));
        }
        let m = Mail {
            to: invite.clone(),
            subject: subject.clone(),
            html: render_props("invite_body", p),
            embedded,
            ..Default::default()
        };
        if let Err(e) = send_mail(app, m).await {
            eprintln!("Failed to send invite email successfully: {e}");
            if d.error_when_not_sent {
                if with_channels {
                    failed.push((invite.clone(), e));
                } else {
                    return Err(InviteError::SendFailed);
                }
            }
        }
    }
    Ok(failed)
}

/// `SendGuestInviteEmails`.
#[allow(clippy::too_many_arguments)]
pub async fn send_guest_invites(
    app: &App,
    team: &crate::model::Team,
    channel_ids: &[String],
    sender_name: &str,
    sender_id: &str,
    sender_image: Option<Vec<u8>>,
    invites: &[String],
    message: &str,
    error_when_not_sent: bool,
    is_system_admin: bool,
    is_first_admin: bool,
    magic_link: bool,
) -> Result<(), InviteError> {
    if per_hour().limited(sender_id, invites.len() as i64) {
        eprintln!("rate limit exceeded user_id={sender_id} team_id={}", team.id);
        return Err(InviteError::RateLimited);
    }
    let server = locale_or_server(app, "");
    let site = site_url(app);
    let sn = site_name(app);
    let message = if message.is_empty() { String::new() } else { strip_tags(message) };
    for invite in invites.iter().filter(|i| !i.is_empty()) {
        let subject = tl(
            &server,
            "api.templates.invite_guest_subject",
            &[("SenderName", sender_name), ("TeamDisplayName", &team.display_name), ("SiteName", &sn)],
        );
        let mut p = invite_props(app, sender_name);
        p.insert("SiteURL".into(), V::from(site.clone()));
        p.insert(
            "Title".into(),
            V::from(tl(
                &server,
                "api.templates.invite_body.title",
                &[("SenderName", sender_name), ("TeamDisplayName", &team.display_name)],
            )),
        );
        p.insert("SubTitle".into(), V::from(tl(&server, "api.templates.invite_body_guest.subTitle", &[])));
        p.insert("Message".into(), V::from(message.clone()));
        let mut extra: BTreeMap<String, String> = BTreeMap::new();
        extra.insert("teamId".into(), team.id.clone());
        extra.insert("channels".into(), channel_ids.join(" "));
        extra.insert("email".into(), invite.clone());
        extra.insert("guest".into(), "true".into());
        extra.insert("senderId".into(), sender_id.to_string());
        let kind = if magic_link { TOKEN_GUEST_MAGIC_LINK_INVITATION } else { TOKEN_GUEST_INVITATION };
        let mut props: BTreeMap<String, String> = BTreeMap::new();
        props.insert("email".into(), invite.clone());
        props.insert("display_name".into(), team.display_name.clone());
        props.insert("name".into(), team.name.clone());
        let token = match save_token(app, kind, &map_to_json(&extra)).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Failed to send invite email successfully: {}", e.detailed_error);
                continue;
            }
        };
        let button = if magic_link {
            format!("{site}/landing#/login/one_time_link?t={}", query_escape(&token))
        } else {
            format!(
                "{site}/signup_user_complete/?d={}&t={}&sbr={}",
                query_escape(&map_to_json(&props)),
                query_escape(&token),
                started_by_role(is_first_admin, is_system_admin)
            )
        };
        p.insert("ButtonURL".into(), V::from(button));
        let mut embedded = vec![];
        let mut photo = "";
        if !message.is_empty()
            && let Some(img) = &sender_image
        {
            photo = "user-avatar.png";
            embedded.push(("user-avatar.png".to_string(), img.clone()));
        }
        p.insert("Posts".into(), V::List(vec![post_data(sender_name, &message, photo)]));
        let m =
            Mail { to: invite.clone(), subject, html: render_props("invite_body", p), embedded, ..Default::default() };
        if let Err(e) = send_mail(app, m).await {
            eprintln!("Failed to send invite email successfully: {e}");
            if error_when_not_sent {
                return Err(InviteError::SendFailed);
            }
        }
    }
    Ok(())
}

/// `SendMagicLinkEmailSelfService`: a login link for a magic-link guest, one a minute per address.
pub async fn send_magic_link_self_service(app: &App, email: &str) -> Result<(), InviteError> {
    if per_minute().limited(email, 1) {
        return Err(InviteError::RateLimited);
    }
    if email.is_empty() {
        return Ok(());
    }
    let server = locale_or_server(app, "");
    let t = |id: &str| tl(&server, id, &[]);
    let site = site_url(app);
    let subject = tl(&server, "api.templates.guest_magic_link_subject", &[("SiteName", &site_name(app))]);
    let mut p = template_props(app, "");
    p.insert("SiteURL".into(), V::from(site.clone()));
    p.insert("Title".into(), V::from(t("api.templates.guest_magic_link_body.title")));
    p.insert("SubTitle".into(), V::from(t("api.templates.guest_magic_link_body.subtitle")));
    p.insert("Button".into(), V::from(t("api.templates.invite_body.button")));
    p.insert("InviteFooterTitle".into(), V::from(t("api.templates.guest_magic_link_body.footer.title")));
    p.insert("InviteFooterInfo".into(), V::from(t("api.templates.guest_magic_link_body.footer.info")));
    let existing: Option<(String, i64)> =
        sqlx::query_as("SELECT Token, CreateAt FROM Tokens WHERE Type = $1 AND Extra::jsonb->>'email' = $2 LIMIT 1")
            .bind(TOKEN_GUEST_MAGIC_LINK)
            .bind(email)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
    let token = match existing {
        // Token.IsExpired: 48 hours.
        Some((t, at)) if now_ms() <= at + 48 * 3_600_000 => t,
        _ => {
            let mut extra = BTreeMap::new();
            extra.insert("email".to_string(), email.to_string());
            match save_token(app, TOKEN_GUEST_MAGIC_LINK, &map_to_json(&extra)).await {
                Ok(t) => t,
                Err(_) => return Err(InviteError::SendFailed),
            }
        }
    };
    p.insert("ButtonURL".into(), V::from(format!("{site}/landing#/login/one_time_link?t={}", query_escape(&token))));
    if let Err(e) = send_mail(app, mail(email, subject, render_props("invite_body", p))).await {
        eprintln!("Failed to send guest magic link email successfully: {e}");
    }
    Ok(())
}

// ---------- post time ----------

/// `utils.FormattedPostTime`.
pub struct PostTime {
    pub year: String,
    pub month: String,
    pub day: String,
    pub hour: String,
    pub minute: String,
    pub zone: String,
}

/// The server's own zone (`time.Local`): `TZ`, or `/etc/localtime`, or UTC.
fn local_tz() -> chrono_tz::Tz {
    static TZ: OnceLock<chrono_tz::Tz> = OnceLock::new();
    *TZ.get_or_init(|| {
        if let Ok(tz) = std::env::var("TZ")
            && let Ok(z) = tz.trim_start_matches(':').parse()
        {
            return z;
        }
        if let Ok(p) = std::fs::read_link("/etc/localtime")
            && let Some(name) = p.to_string_lossy().split("zoneinfo/").nth(1)
            && let Ok(z) = name.parse()
        {
            return z;
        }
        chrono_tz::UTC
    })
}

/// `GetFormattedPostTime`: in the user's zone, 12- or 24-hour.
pub fn post_time(u: &User, create_at: i64, military: bool, locale: &str) -> PostTime {
    use chrono::{Datelike, TimeZone, Timelike};
    let tzp = &u.timezone.0;
    let pref = if tzp.get("useAutomaticTimezone").map(String::as_str) == Some("true") {
        tzp.get("automaticTimezone").cloned().unwrap_or_default()
    } else {
        tzp.get("manualTimezone").cloned().unwrap_or_default()
    };
    let tz: chrono_tz::Tz = pref.parse().unwrap_or_else(|_| local_tz());
    let t = tz.timestamp_opt(create_at / 1000, 0).single().unwrap_or_else(|| tz.timestamp_opt(0, 0).unwrap());
    let months = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (hour, period) = if military {
        (format!("{:02}", t.hour()), String::new())
    } else {
        let h = t.hour() % 12;
        (if h == 0 { 12 } else { h }.to_string(), format!(" {}", if t.hour() < 12 { "AM" } else { "PM" }))
    };
    PostTime {
        year: t.year().to_string(),
        month: tl(locale, months[t.month0() as usize], &[]),
        day: t.day().to_string(),
        hour,
        minute: format!("{:02}{period}", t.minute()),
        zone: t.format("%Z").to_string(),
    }
}

async fn pref(app: &App, user_id: &str, category: &str, name: &str) -> Option<String> {
    sqlx::query_scalar("SELECT Value FROM Preferences WHERE UserId = $1 AND Category = $2 AND Name = $3")
        .bind(user_id)
        .bind(category)
        .bind(name)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
}

// ---------- notification emails ----------

/// `GetMessageForNotification`: the message as HTML with channel links, or what files were sent.
pub async fn message_for_notification(app: &App, post: &Post, team_name: &str, locale: &str) -> String {
    let site = site_url(app);
    if !post.message.trim().is_empty() || post.file_ids.0.is_empty() {
        let html = crate::markdown::escaped_to_html(&post.message, &site);
        let landing = format!("{site}/landing#/{team_name}");
        return hyperlink_channels(app, &html, team_name, &landing).await;
    }
    let infos = app.file_infos_for_post(&post.id, false).await.unwrap_or_default();
    let mut names = vec![];
    let mut only_images = true;
    for f in infos.iter() {
        names.push(percent_decode(&f.name));
        only_images = only_images && is_image(&f.mime_type, &f.extension);
    }
    let id = if only_images {
        "api.post.get_message_for_notification.images_sent"
    } else {
        "api.post.get_message_for_notification.files_sent"
    };
    let filenames = html_escape::encode_safe(&names.join(", "))
        .replace("&quot;", "&#34;")
        .replace("&#x27;", "&#39;")
        .replace("&#x2F;", "/");
    tl(locale, id, &[("Count", &names.len().to_string()), ("Filenames", &filenames)])
        .replace("[[", "<strong>")
        .replace("]]", "</strong>")
}

fn is_image(mime: &str, ext: &str) -> bool {
    mime.starts_with("image")
        || matches!(ext.to_lowercase().as_str(), "jpg" | "jpeg" | "gif" | "bmp" | "png" | "tiff" | "tif" | "webp")
}

/// `url.QueryUnescape`, the name unchanged when it is not escaped right.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => return s.to_string(),
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// `GenerateHyperlinkForChannels`: `~name` of the team's public channels become links.
async fn hyperlink_channels(app: &App, html: &str, team_name: &str, landing: &str) -> String {
    let p = Post { message: html.to_string(), ..Default::default() };
    let names = crate::notify::channel_mentions(&p);
    if names.is_empty() {
        return html.to_string();
    }
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.Id, c.Name, c.Type::text FROM Channels c JOIN Teams t ON t.Id = c.TeamId
         WHERE t.Name = $1 AND c.Name = ANY($2) AND c.DeleteAt = 0",
    )
    .bind(team_name)
    .bind(&names)
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();
    let mut out = html.to_string();
    for (_, name, kind) in rows {
        if kind == channel::OPEN {
            let link = format!("<a href='{landing}/channels/{name}'>~{name}</a>");
            out = out.replace(&format!("~{name}"), &link);
        }
    }
    out
}

/// `ProcessMessageAttachments`.
fn attachments_data(post: &Post, site: &str) -> V {
    let mut out = vec![];
    for a in crate::integrations::attachments(&post.props.0) {
        let mut m = BTreeMap::new();
        m.insert("Color".to_string(), V::from(a.color.clone()));
        m.insert("Pretext".to_string(), V::Html(crate::markdown::escaped_to_html(&a.pretext, site)));
        m.insert("Text".to_string(), V::Html(crate::markdown::escaped_to_html(&a.text, site)));
        m.insert("AuthorName".to_string(), V::from(a.author_name.clone()));
        m.insert("AuthorLink".to_string(), V::from(a.author_link.clone()));
        m.insert("AuthorIcon".to_string(), V::from(a.author_icon.clone()));
        m.insert("Title".to_string(), V::from(crate::markdown::strip(&a.title)));
        m.insert("TitleLink".to_string(), V::from(a.title_link.clone()));
        m.insert("ImageURL".to_string(), V::from(a.image_url.clone()));
        m.insert("ThumbURL".to_string(), V::from(a.thumb_url.clone()));
        m.insert("Footer".to_string(), V::from(a.footer.clone()));
        m.insert("FooterIcon".to_string(), V::from(a.footer_icon.clone()));
        let mut rows: Vec<V> = vec![];
        let mut short: Vec<V> = vec![];
        let row = |cells: Vec<V>| {
            let mut r = BTreeMap::new();
            r.insert("Cells".to_string(), V::List(cells));
            V::Map(r)
        };
        for f in a.fields.clone().unwrap_or_default() {
            let mut cell = BTreeMap::new();
            cell.insert("Title".to_string(), V::from(f.title.clone()));
            let value = match &f.value {
                Value::String(s) => V::Html(crate::markdown::escaped_to_html(s, site)),
                Value::Null => V::Nil,
                Value::Bool(b) => V::Bool(*b),
                Value::Number(n) => V::Str(n.to_string()),
                other => V::Str(other.to_string()),
            };
            cell.insert("Value".to_string(), value);
            cell.insert("Short".to_string(), V::Bool(f.short));
            if !f.short {
                if !short.is_empty() {
                    rows.push(row(std::mem::take(&mut short)));
                }
                rows.push(row(vec![V::Map(cell)]));
            } else {
                short.push(V::Map(cell));
                if short.len() == 2 {
                    rows.push(row(std::mem::take(&mut short)));
                }
            }
        }
        if !short.is_empty() {
            rows.push(row(short));
        }
        m.insert("FieldRows".to_string(), V::List(rows));
        out.push(V::Map(m));
    }
    V::List(out)
}

fn truncate_name(name: &str, n: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() > n { chars[..n].iter().collect::<String>() + "..." } else { name.to_string() }
}

/// `sendNotificationEmail`: batched when the user wants it, otherwise one email per post.
pub async fn send_notification_email(
    app: &App,
    n: &PostNotification,
    user: &User,
    team: crate::model::Team,
    sender_image: Option<Vec<u8>>,
) -> Result<(), String> {
    let c = &n.channel;
    let post = &n.post;
    let mut team = team;
    if c.r#type == channel::DIRECT || c.r#type == channel::GROUP {
        let teams: Vec<crate::model::Team> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{} WHERE Id IN (SELECT TeamId FROM TeamMembers WHERE UserId = $1 AND DeleteAt = 0) AND DeleteAt = 0",
            crate::model::Team::SELECT
        )))
        .bind(&user.id)
        .fetch_all(&app.db)
        .await
        .map_err(|e| format!("unable to get user teams: {e}"))?;
        // Go keeps the team only when the user is not in it (a bug); the user's own team is kept here.
        let found = teams.iter().any(|t| t.id == team.id);
        if !found {
            team = match teams.into_iter().next() {
                Some(t) => t,
                None => crate::model::Team {
                    name: "select_team".into(),
                    display_name: site_name(app),
                    ..Default::default()
                },
            };
        }
    }
    if app.cfg.bool("EmailSettings.EnableEmailBatching") {
        let batched = match pref(app, &user.id, "notifications", "email_interval").await {
            None => true,
            Some(v) => v != "30",
        };
        if batched && add_to_batch(&user.id, post, &team.name) {
            return Ok(());
        }
    }
    let locale = user.locale.as_str();
    let format = crate::push::name_format(app, &user.id).await;
    let military = pref(app, &user.id, "display_settings", "use_military_time").await.as_deref() == Some("true");
    let channel_name = n.channel_name(&format, "");
    let sender_name = n.sender_name(&format, app.cfg.bool("ServiceSettings.EnablePostUsernameOverride"));
    let sn = site_name(app);
    let pt = post_time(user, post.create_at, military, locale);
    let date = [("Month", pt.month.as_str()), ("Day", pt.day.as_str()), ("Year", pt.year.as_str())];
    let subject = match c.r#type.as_str() {
        channel::DIRECT => tl(
            locale,
            "app.notification.subject.direct.full",
            &[&[("SiteName", sn.as_str()), ("SenderDisplayName", &sender_name)][..], &date[..]].concat(),
        ),
        channel::GROUP => tl(
            locale,
            "app.notification.subject.group_message.full",
            &[&[("SiteName", sn.as_str()), ("ChannelName", &channel_name)][..], &date[..]].concat(),
        ),
        _ => {
            let team_name = if app.cfg.bool("EmailSettings.UseChannelInEmailNotifications") {
                format!("{} ({channel_name})", team.display_name)
            } else {
                team.display_name.clone()
            };
            tl(
                locale,
                "app.notification.subject.notification.full",
                &[&[("SiteName", sn.as_str()), ("TeamName", &team_name)][..], &date[..]].concat(),
            )
        }
    };
    let sname = [("SenderName", sender_name.as_str())];
    let (mut title, mut subtitle) = match c.r#type.as_str() {
        channel::DIRECT => (
            tl(locale, "app.notification.body.dm.title", &sname),
            tl(locale, "app.notification.body.dm.subTitle", &sname),
        ),
        channel::GROUP => (
            tl(locale, "app.notification.body.group.title", &sname),
            tl(locale, "app.notification.body.group.subTitle", &sname),
        ),
        _ => (
            tl(locale, "app.notification.body.mention.title", &sname),
            tl(
                locale,
                "app.notification.body.mention.subTitle",
                &[("SenderName", &sender_name), ("ChannelName", &channel_name)],
            ),
        ),
    };
    if !post.root_id.is_empty() && app.is_crt_enabled(&user.id).await {
        title = tl(locale, "app.notification.body.thread.title", &sname);
        subtitle = match c.r#type.as_str() {
            channel::DIRECT => tl(locale, "app.notification.body.thread_dm.subTitle", &sname),
            channel::GROUP => tl(locale, "app.notification.body.thread_gm.subTitle", &sname),
            _ => tl(
                locale,
                "app.notification.body.thread_channel_full.subTitle",
                &[("SenderName", &sender_name), ("ChannelName", &channel_name)],
            ),
        };
    }
    let site = site_url(app);
    let message_html = message_for_notification(app, post, &team.name, locale).await;
    let landing = format!("{site}/landing#/{}", team.name);
    let button_url = if team.name != "select_team" { format!("{landing}/pl/{}", post.id) } else { landing };
    let mut photo = "";
    let mut embedded = vec![];
    if !message_html.is_empty()
        && let Some(img) = sender_image
    {
        photo = "user-avatar.png";
        embedded.push(("user-avatar.png".to_string(), img));
    }
    let mut pdata = BTreeMap::new();
    pdata.insert("SenderName".to_string(), V::from(truncate_name(&sender_name, 22)));
    pdata.insert("SenderPhoto".to_string(), V::from(photo));
    if !message_html.is_empty() {
        pdata.insert("Message".to_string(), V::Html(message_html.clone()));
        let time = tl(
            locale,
            "app.notification.body.dm.time",
            &[("Hour", &pt.hour), ("Minute", &pt.minute), ("TimeZone", &pt.zone)],
        );
        pdata.insert("Time".to_string(), V::from(time));
        pdata.insert("MessageAttachments".to_string(), attachments_data(post, &site));
    }
    if c.r#type != channel::DIRECT && c.r#type != channel::GROUP {
        pdata.insert("ChannelName".to_string(), V::from(channel_name.clone()));
    }
    let mut p = template_props(app, locale);
    let t = |id: &str| tl(locale, id, &[]);
    p.insert("SiteURL".into(), V::from(site.clone()));
    p.insert("ButtonURL".into(), V::from(button_url));
    p.insert("SenderName".into(), V::from(sender_name.clone()));
    p.insert("Button".into(), V::from(t("api.templates.post_body.button")));
    p.insert("NotificationFooterTitle".into(), V::from(t("app.notification.footer.title")));
    p.insert("NotificationFooterInfoLogin".into(), V::from(t("app.notification.footer.infoLogin")));
    p.insert("NotificationFooterInfo".into(), V::from(t("app.notification.footer.info")));
    p.insert("Title".into(), V::from(title));
    p.insert("SubTitle".into(), V::from(subtitle));
    p.insert("Posts".into(), V::List(if message_html.is_empty() { vec![] } else { vec![V::Map(pdata)] }));
    let body = render_props("messages_notification", p);
    let host = crate::mail::hostname_of(&site);
    let mut m = Mail {
        to: user.email.clone(),
        subject: html_escape::decode_html_entities(&subject).into_owned(),
        html: body,
        embedded,
        message_id: format!("<{}@{host}>", post.id),
        ..Default::default()
    };
    if !post.root_id.is_empty() {
        m.in_reply_to = format!("<{}@{host}>", post.root_id);
        m.references = m.in_reply_to.clone();
    }
    match app.hub.app() {
        Some(app2) => {
            tokio::spawn(async move {
                if let Err(e) = send_mail(&app2, m).await {
                    eprintln!("Error while sending the email: {e}");
                }
            });
        }
        None => send_mail(app, m).await?,
    }
    Ok(())
}

// ---------- batching ----------

struct Batched {
    user_id: String,
    post: Post,
    team_name: String,
}

type BatchQueue = (tokio::sync::mpsc::Sender<Batched>, Mutex<Option<tokio::sync::mpsc::Receiver<Batched>>>);
static BATCH: OnceLock<BatchQueue> = OnceLock::new();

/// `EmailBatchingJob.Add`: false when the buffer is full (or batching never started).
fn add_to_batch(user_id: &str, post: &Post, team_name: &str) -> bool {
    let Some((tx, _)) = BATCH.get() else { return false };
    tx.try_send(Batched { user_id: user_id.into(), post: post.clone(), team_name: team_name.into() }).is_ok()
}

/// `InitEmailBatching`: every `EmailBatchingInterval` seconds, the users whose oldest pending post is older than
/// their interval and who have not read since get one email.
pub fn spawn_batching(app: State) {
    let size = app.cfg.i64("EmailSettings.EmailBatchingBufferSize").max(1) as usize;
    let (tx, rx) = tokio::sync::mpsc::channel(size);
    if BATCH.set((tx, Mutex::new(Some(rx)))).is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut rx = BATCH.get().and_then(|b| b.1.lock().unwrap().take()).expect("batch receiver");
        let mut pending: HashMap<String, Vec<Batched>> = HashMap::new();
        loop {
            let secs = app.cfg.i64("EmailSettings.EmailBatchingInterval").max(1) as u64;
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            if !app.cfg.bool("EmailSettings.EnableEmailBatching") {
                continue;
            }
            while let Ok(b) = rx.try_recv() {
                pending.entry(b.user_id.clone()).or_default().push(b);
            }
            let users: Vec<String> = pending.keys().cloned().collect();
            for uid in users {
                let Some(list) = pending.get(&uid) else { continue };
                if list.is_empty() {
                    pending.remove(&uid);
                    continue;
                }
                let interval: i64 = pref(&app, &uid, "notifications", "email_interval")
                    .await
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(900);
                let start = list[0].post.create_at;
                if now_ms() - start <= interval * 1000 {
                    continue;
                }
                // Go looks up the first notification's team for every team name; read since the batch began
                // anywhere there drops the batch.
                let team_name = list[0].team_name.clone();
                let read: Option<i64> = sqlx::query_scalar(
                    "SELECT MAX(cm.LastViewedAt) FROM ChannelMembers cm JOIN Channels c ON c.Id = cm.ChannelId
                     LEFT JOIN Teams t ON t.Id = c.TeamId
                     WHERE cm.UserId = $1 AND c.Type <> 'S' AND (t.Name = $2 OR c.TeamId = '' OR c.TeamId IS NULL)",
                )
                .bind(&uid)
                .bind(&team_name)
                .fetch_one(&app.db)
                .await
                .ok()
                .flatten();
                let list = pending.remove(&uid).unwrap_or_default();
                if read.is_some_and(|r| r >= start) {
                    continue;
                }
                send_batched(&app, &uid, list).await;
            }
        }
    });
}

/// `sendBatchedEmailNotification`.
async fn send_batched(app: &App, user_id: &str, list: Vec<Batched>) {
    let Ok(Some(user)) = app.user(user_id).await else { return };
    let locale = user.locale.as_str();
    let t = |id: &str| tl(locale, id, &[]);
    let format = app.cfg.str("TeamSettings.TeammateNameDisplay").to_string();
    let site = site_url(app);
    let crt = app.is_crt_enabled(user_id).await;
    let military = pref(app, user_id, "display_settings", "use_military_time").await.as_deref() == Some("true");
    let mut posts = vec![];
    let mut embedded = vec![];
    for (i, b) in list.iter().enumerate() {
        let sender = app.user(&b.post.user_id).await.ok().flatten();
        let Ok(Some(c)) = app.channel(&b.post.channel_id).await else { continue };
        let photo = format!("user-avatar-{i}.png");
        if let Some(s) = &sender {
            embedded.push((photo.clone(), app.profile_image(s).await.0));
        }
        let pt = post_time(&user, b.post.create_at, military, locale);
        let time = tl(
            locale,
            "api.email_batching.send_batched_email_notification.time",
            &[
                ("Hour", &pt.hour),
                ("Minute", &pt.minute),
                ("TimeZone", &pt.zone),
                ("Month", &pt.month),
                ("Day", &pt.day),
                ("Year", &pt.year),
            ],
        );
        let mut channel_name = c.display_name.clone();
        let mut show_icon = true;
        let mut others = 0i64;
        if crt && !b.post.root_id.is_empty() {
            channel_name =
                tl(locale, "api.push_notification.title.collapsed_threads", &[("channelName", &channel_name)]);
            if c.r#type == channel::DIRECT {
                channel_name = t("api.push_notification.title.collapsed_threads_dm");
            }
        }
        if c.r#type == channel::GROUP {
            others = channel_name.split(',').count() as i64 - 1;
            show_icon = false;
            channel_name = truncate_name(&c.display_name, 11);
        }
        let mut m = BTreeMap::new();
        m.insert("SenderPhoto".to_string(), V::from(photo));
        m.insert(
            "SenderName".to_string(),
            V::from(truncate_name(&sender.map(|s| s.display_name(&format)).unwrap_or_default(), 22)),
        );
        m.insert("Time".to_string(), V::from(time));
        m.insert("ChannelName".to_string(), V::from(channel_name));
        m.insert("Message".to_string(), V::Html(message_for_notification(app, &b.post, &b.team_name, locale).await));
        m.insert("MessageURL".to_string(), V::from(format!("{site}/{}/pl/{}", b.team_name, b.post.id)));
        m.insert("ShowChannelIcon".to_string(), V::Bool(show_icon));
        m.insert("OtherChannelMembersCount".to_string(), V::Int(others));
        m.insert("MessageAttachments".to_string(), attachments_data(&b.post, &site));
        posts.push(V::Map(m));
    }
    let pt = post_time(&user, list[0].post.create_at, military, locale);
    let count = list.len().to_string();
    let subject = tl(
        locale,
        "api.email_batching.send_batched_email_notification.subject",
        &[("Count", &count), ("SiteName", &site_name(app)), ("Year", &pt.year), ("Month", &pt.month), ("Day", &pt.day)],
    );
    let mut p = template_props(app, locale);
    p.insert("SiteURL".into(), V::from(site.clone()));
    p.insert(
        "Title".into(),
        V::from(tl(
            locale,
            "api.email_batching.send_batched_email_notification.title",
            &[("Count", &(list.len() - 1).to_string())],
        )),
    );
    p.insert("SubTitle".into(), V::from(t("api.email_batching.send_batched_email_notification.subTitle")));
    p.insert("Button".into(), V::from(t("api.email_batching.send_batched_email_notification.button")));
    p.insert("ButtonURL".into(), V::from(site));
    p.insert("Posts".into(), V::List(posts));
    p.insert("MessageButton".into(), V::from(t("api.email_batching.send_batched_email_notification.messageButton")));
    p.insert("NotificationFooterTitle".into(), V::from(t("app.notification.footer.title")));
    p.insert("NotificationFooterInfoLogin".into(), V::from(t("app.notification.footer.infoLogin")));
    p.insert("NotificationFooterInfo".into(), V::from(t("app.notification.footer.info")));
    let m = Mail {
        to: user.email.clone(),
        subject,
        html: render_props("messages_notification", p),
        embedded,
        ..Default::default()
    };
    if let Err(e) = send_mail(app, m).await {
        eprintln!("Unable to send batched email notification email={} err={e}", user.email);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(condense("http://localhost:8065"), "localhost:8065");
        assert_eq!(condense("https://a.b/sub/"), "a.b/sub");
        assert_eq!(query_escape("a b+c@d"), "a+b%2Bc%40d");
        assert_eq!(random_string(64).len(), 64);
        let l = Limiter::new(20, 3_600_000, 20);
        assert!(!l.limited("k", 21));
        assert!(l.limited("k", 1));
        assert_eq!(strip_tags("<b>hi</b> & <script>x</script>"), "hi &amp; ");
    }

    /// Emails name the site, without Mattermost's footer, address or links (docs/BRANDING.md).
    #[test]
    fn emails_name_the_site() {
        let t = |id: &str| V::from(tl("en", id, &[]));
        let mut p = BTreeMap::new();
        p.insert("SiteURL".into(), V::from("https://chat.example.com"));
        p.insert("Title".into(), t("api.templates.invite_body.title"));
        p.insert("SubTitle".into(), t("api.templates.invite_body.subTitle"));
        p.insert("InviteFooterTitle".into(), t("api.templates.invite_body_footer.title"));
        p.insert("InviteFooterInfo".into(), t("api.templates.invite_body_footer.info"));
        p.insert("InviteFooterLearnMore".into(), t("api.templates.invite_body_footer.learn_more"));
        p.insert("FooterV2".into(), V::from(""));
        let html = render_props("invite_body", p);
        let name = crate::brand::name();
        assert!(html.contains(&format!("Start collaborating with your team on {name}")), "{html}");
        assert!(html.contains(&format!("What is {name}?")));
        assert!(html.contains("href=\"https://chat.example.com\""));
        assert!(!html.contains("Mattermost") && !html.contains("mattermost.com") && !html.contains("Palo Alto"));

        let mut p = BTreeMap::new();
        p.insert("QuestionTitle".into(), t("api.templates.questions_footer.title"));
        p.insert("EmailInfo1".into(), t("api.templates.email_us_anytime_at"));
        p.insert("SupportEmail".into(), V::from(""));
        let html = render_props("email_change_verify_body", p);
        assert!(!html.contains("mailto:") && !html.contains("Mattermost"));
    }
}
