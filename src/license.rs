//! The license, as a Team Edition server handles one: the signature is checked against the public keys the Go
//! server embeds (not an enterprise feature), and a valid license is stored (`Licenses` row, `ActiveLicenseId`)
//! and served by `GET /license/client`. Licensed features themselves are not ported: the server keeps answering
//! as Team Edition does.

// Based on Mattermost server/channels/utils/license.go, license_public_key.go (and its public keys),
// channels/app/platform/license.go, channels/api4/license.go and public/model/license.go, Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::sync::RwLock;

use axum::body::Body;
use axum::extract::State as St;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::{ApiResult, App, AppError, Auth, MaybeAuth, State, now_ms};

const PRODUCTION_KEY: &str = include_str!("../data/license/public-key.txt");
const TEST_KEY: &str = include_str!("../data/license/public-key-test.txt");

pub fn routes() -> Router<State> {
    Router::new()
        .route("/api/v4/license", post(add_license).delete(remove_license))
        .route("/api/v4/license/preview", post(preview_license))
        .route("/api/v4/license/client", axum::routing::get(client_license))
}

// ---------- the model ----------

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Customer {
    pub id: String,
    pub name: String,
    pub email: String,
    pub company: String,
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Features {
    pub users: Option<i64>,
    pub ldap: Option<bool>,
    pub ldap_groups: Option<bool>,
    pub mfa: Option<bool>,
    pub google_oauth: Option<bool>,
    pub office365_oauth: Option<bool>,
    pub openid: Option<bool>,
    pub compliance: Option<bool>,
    pub cluster: Option<bool>,
    pub metrics: Option<bool>,
    pub mhpns: Option<bool>,
    pub saml: Option<bool>,
    pub elastic_search: Option<bool>,
    pub announcement: Option<bool>,
    pub theme_management: Option<bool>,
    pub email_notification_contents: Option<bool>,
    pub data_retention: Option<bool>,
    pub message_export: Option<bool>,
    pub custom_permissions_schemes: Option<bool>,
    pub custom_terms_of_service: Option<bool>,
    pub guest_accounts: Option<bool>,
    pub guest_accounts_permissions: Option<bool>,
    pub id_loaded: Option<bool>,
    pub lock_teammate_name_display: Option<bool>,
    pub enterprise_plugins: Option<bool>,
    pub advanced_logging: Option<bool>,
    pub cloud: Option<bool>,
    pub shared_channels: Option<bool>,
    pub remote_cluster_service: Option<bool>,
    pub outgoing_oauth_connections: Option<bool>,
    pub auto_translation: Option<bool>,
    pub future_features: Option<bool>,
}

impl Features {
    /// `Features.SetDefaults`.
    fn set_defaults(&mut self) {
        let ff = *self.future_features.get_or_insert(true);
        self.users.get_or_insert(0);
        for f in [
            &mut self.ldap,
            &mut self.ldap_groups,
            &mut self.mfa,
            &mut self.google_oauth,
            &mut self.office365_oauth,
            &mut self.openid,
            &mut self.compliance,
            &mut self.cluster,
            &mut self.metrics,
            &mut self.mhpns,
            &mut self.saml,
            &mut self.elastic_search,
        ] {
            f.get_or_insert(ff);
        }
        self.announcement.get_or_insert(true);
        self.theme_management.get_or_insert(true);
        for f in [
            &mut self.email_notification_contents,
            &mut self.data_retention,
            &mut self.message_export,
            &mut self.custom_permissions_schemes,
            &mut self.guest_accounts,
            &mut self.guest_accounts_permissions,
            &mut self.custom_terms_of_service,
            &mut self.id_loaded,
            &mut self.lock_teammate_name_display,
            &mut self.enterprise_plugins,
            &mut self.advanced_logging,
        ] {
            f.get_or_insert(ff);
        }
        self.cloud.get_or_insert(false);
        for f in [&mut self.shared_channels, &mut self.remote_cluster_service, &mut self.outgoing_oauth_connections] {
            f.get_or_insert(ff);
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct LicenseLimits {
    pub post_history: i64,
    pub board_cards: i64,
    pub playbook_runs: i64,
    #[serde(rename = "call_duration")]
    pub call_duration_seconds: i64,
    pub agents_prompts: i64,
    pub push_notifications: i64,
}

/// `model.License`, its JSON in Go's field order.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct License {
    pub id: String,
    pub issued_at: i64,
    pub starts_at: i64,
    pub expires_at: i64,
    pub customer: Option<Customer>,
    pub features: Option<Features>,
    pub sku_name: String,
    pub sku_short_name: String,
    pub is_trial: bool,
    pub is_gov_sku: bool,
    pub is_non_production: bool,
    pub is_seat_count_enforced: bool,
    pub extra_users: Option<i64>,
    pub signup_jwt: Option<String>,
    pub limits: Option<LicenseLimits>,
    pub add_ons: Option<Vec<String>>,
}

impl License {
    fn is_cloud(&self) -> bool {
        self.features.as_ref().and_then(|f| f.cloud).unwrap_or(false)
    }

    /// `utils.GetClientLicense`.
    fn client(&self) -> Value {
        let f = self.features.clone().unwrap_or_default();
        let b = |v: Option<bool>| v.unwrap_or(false).to_string();
        let c = self.customer.clone().unwrap_or_default();
        json!({
            "IsLicensed": "true",
            "Id": self.id,
            "SkuName": self.sku_name,
            "SkuShortName": self.sku_short_name,
            "Users": f.users.unwrap_or(0).to_string(),
            "LDAP": b(f.ldap),
            "LDAPGroups": b(f.ldap_groups),
            "MFA": b(f.mfa),
            "SAML": b(f.saml),
            "Cluster": b(f.cluster),
            "Metrics": b(f.metrics),
            "GoogleOAuth": b(f.google_oauth),
            "Office365OAuth": b(f.office365_oauth),
            "OpenId": b(f.openid),
            "Compliance": b(f.compliance),
            "MHPNS": b(f.mhpns),
            "Announcement": b(f.announcement),
            "Elasticsearch": b(f.elastic_search),
            "DataRetention": b(f.data_retention),
            "IDLoadedPushNotifications": b(f.id_loaded),
            "IssuedAt": self.issued_at.to_string(),
            "StartsAt": self.starts_at.to_string(),
            "ExpiresAt": self.expires_at.to_string(),
            "Name": c.name,
            "Email": c.email,
            "Company": c.company,
            "EmailNotificationContents": b(f.email_notification_contents),
            "MessageExport": b(f.message_export),
            "CustomPermissionsSchemes": b(f.custom_permissions_schemes),
            "GuestAccounts": b(f.guest_accounts),
            "GuestAccountsPermissions": b(f.guest_accounts_permissions),
            "CustomTermsOfService": b(f.custom_terms_of_service),
            "LockTeammateNameDisplay": b(f.lock_teammate_name_display),
            "Cloud": b(f.cloud),
            "SharedChannels": b(f.shared_channels),
            "RemoteClusterService": b(f.remote_cluster_service),
            "OutgoingOAuthConnections": b(f.outgoing_oauth_connections),
            "IsTrial": self.is_trial.to_string(),
            "IsGovSku": self.is_gov_sku.to_string(),
            "IsNonProduction": self.is_non_production.to_string(),
            "AddOns": self.add_ons.clone().unwrap_or_default().join(","),
        })
    }
}

/// The license in use (`licenseValue`); none on a Team Edition server until one is added.
static CURRENT: RwLock<Option<License>> = RwLock::new(None);

pub fn current() -> Option<License> {
    CURRENT.read().unwrap().clone()
}

/// `ClientLicense`.
pub fn client() -> Value {
    current().map_or_else(|| json!({"IsLicensed": "false"}), |l| l.client())
}

/// `GetSanitizedClientLicense`.
pub fn sanitized_client() -> Value {
    let mut v = client();
    if let Some(m) = v.as_object_mut() {
        for k in ["Id", "Name", "Email", "IssuedAt", "StartsAt", "ExpiresAt", "SkuName"] {
            m.remove(k);
        }
    }
    v
}

// ---------- validation ----------

pub enum Invalid {
    /// Not base64, too short, a bad signature: `InvalidLicenseError`.
    Bad(String),
    /// Signed with the production key, on a test or dev server.
    ProductionInTest,
    /// Signed with the test key, on a production server.
    TestInProduction,
}

/// `model.GetServiceEnvironment`.
pub fn service_environment() -> &'static str {
    match std::env::var("MM_SERVICEENVIRONMENT").unwrap_or_default().to_lowercase().as_str() {
        "dev" => "dev",
        "test" => "test",
        _ => "production",
    }
}

/// `verifyLicenseSignature`: RSA PKCS#1 v1.5 over SHA-512. `Ok(false)` is a signature mismatch.
fn verify(pem: &str, digest: &[u8], sig: &[u8]) -> Result<bool, String> {
    use rsa::pkcs8::DecodePublicKey;
    let key =
        rsa::PublicKey::from_public_key_pem(pem).map_err(|e| format!("encountered error parsing public key: {e}"))?;
    Ok(key.verify(rsa::Pkcs1v15Sign::new::<rsa::sha2::Sha512>(), digest, sig).is_ok())
}

/// `LicenseValidatorImpl.ValidateLicense`: the license's JSON, if its signature is good for this environment.
pub fn validate(signed: &[u8]) -> Result<String, Invalid> {
    use base64::Engine;
    // Go's StdEncoding: padded, line breaks skipped, any trailing bits.
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
    );
    let clean: Vec<u8> = signed.iter().copied().filter(|b| *b != b'\r' && *b != b'\n').collect();
    let mut decoded =
        engine.decode(&clean).map_err(|e| Invalid::Bad(format!("encountered error decoding license: {e}")))?;
    while decoded.last() == Some(&0) {
        decoded.pop();
    }
    if decoded.len() <= 256 {
        return Err(Invalid::Bad("Signed license not long enough".into()));
    }
    let (plain, sig) = decoded.split_at(decoded.len() - 256);
    use sha2::Digest;
    let digest = sha2::Sha512::digest(plain);
    let env = service_environment();
    let (primary, alternate) =
        if env == "production" { (PRODUCTION_KEY, TEST_KEY) } else { (TEST_KEY, PRODUCTION_KEY) };
    if !verify(primary, &digest, sig).map_err(Invalid::Bad)? {
        if verify(alternate, &digest, sig).map_err(Invalid::Bad)? {
            return Err(if env == "production" { Invalid::TestInProduction } else { Invalid::ProductionInTest });
        }
        return Err(Invalid::Bad("Invalid signature: crypto/rsa: verification error".into()));
    }
    Ok(String::from_utf8_lossy(plain).into_owned())
}

/// `NewLicenseValidationAppError`.
fn validation_error(e: Invalid) -> AppError {
    match e {
        Invalid::ProductionInTest => AppError::bad_request(
            "api.license.add_license.wrong_environment_production.app_error",
            "",
        )
        .detail(
            "license is a production license but the server is running in a test or development service environment",
        ),
        Invalid::TestInProduction => AppError::bad_request(
            "api.license.add_license.wrong_environment_test.app_error",
            "",
        )
        .detail(
            "license is a test or development license but the server is running in a production service environment",
        ),
        Invalid::Bad(d) => AppError::bad_request("api.license.add_license.invalid.app_error", "").detail(d),
    }
}

fn unmarshal(s: &str) -> ApiResult<License> {
    serde_json::from_str(s)
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "api.unmarshal_error", "").detail(e.to_string()))
}

/// `LicenseFromBytes`.
fn license_from_bytes(b: &[u8]) -> ApiResult<License> {
    unmarshal(&validate(b).map_err(validation_error)?)
}

// ---------- the stored license ----------

/// `SetLicense`: the license in use, and the listeners' `license_changed` (with the sanitized client license).
fn set_license(app: &App, license: Option<License>) -> bool {
    let ok = license.is_some();
    *CURRENT.write().unwrap() = license.map(|mut l| {
        l.features.get_or_insert_with(Default::default).set_defaults();
        l
    });
    app.hub.publish("license_changed", json!({"license": sanitized_client()}), Default::default());
    ok
}

/// `PlatformService.InvalidateAllCaches`: every cache cleared, every websocket's session reloaded, and the license
/// loaded again (`LoadLicense`, whose `SetLicense` sends `license_changed` to everyone even when nothing changed).
pub async fn invalidate_all_caches(app: &App) {
    app.caches.clear_all();
    app.hub.invalidate_all();
    // load() sets the license without the listeners (it also runs at start, with nobody to tell).
    if load(app).await {
        app.hub.publish("license_changed", json!({"license": sanitized_client()}), Default::default());
    }
}

/// `InvalidateAllCaches` right after the license was saved or removed: `LoadLicense` would set the same license again.
fn reloaded_caches(app: &App) {
    app.caches.clear_all();
    app.hub.invalidate_all();
    set_license(app, current());
}

/// `LoadLicense`, at start: `MM_LICENSE`, else the active license of the database (or the license file, saved).
/// Whether a license (or none) was set, which is when Go's `SetLicense` runs its listeners.
pub async fn load(app: &App) -> bool {
    if let Ok(s) = std::env::var("MM_LICENSE")
        && !s.is_empty()
    {
        match license_from_bytes(s.as_bytes()) {
            Ok(l) => {
                *CURRENT.write().unwrap() = Some(l).map(|mut l| {
                    l.features.get_or_insert_with(Default::default).set_defaults();
                    l
                });
                crate::logs::info("License key from ENV is valid, unlocking enterprise features.", &[]);
                return true;
            }
            Err(e) => crate::logs::error("Failed to read license set in environment.", &[("error", e.id.into())]),
        }
        return false;
    }
    let mut id: String = sqlx::query_scalar("SELECT Value FROM Systems WHERE Name = 'ActiveLicenseId'")
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    if !crate::app::is_valid_id(&id) {
        let loc = app.cfg.str("ServiceSettings.LicenseFileLocation");
        let file = if loc.is_empty() {
            crate::logs::find_dir("config").join("mattermost.mattermost-license")
        } else {
            loc.into()
        };
        match std::fs::read(&file) {
            Err(e) => crate::logs::warn(
                "Failed to get license from disk",
                &[("error", format!("We could not find the license key on disk at {}: {e}", file.display()).into())],
            ),
            Ok(b) => match validate(&b).ok().and_then(|s| serde_json::from_str::<License>(&s).ok()) {
                None => crate::logs::warn("Failed to get license from disk", &[("error", "invalid license".into())]),
                Some(l) => match save(app, &b).await {
                    Ok(_) => id = l.id,
                    Err(e) => {
                        crate::logs::error("Failed to save license key loaded from disk.", &[("error", e.id.into())])
                    }
                },
            },
        }
    }
    let bytes: Option<String> = sqlx::query_scalar("SELECT Bytes FROM Licenses WHERE Id = $1")
        .bind(&id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten();
    let Some(bytes) = bytes else {
        *CURRENT.write().unwrap() = None;
        return true;
    };
    if let Ok(l) = license_from_bytes(bytes.as_bytes()) {
        *CURRENT.write().unwrap() = Some(l).map(|mut l| {
            l.features.get_or_insert_with(Default::default).set_defaults();
            l
        });
        crate::logs::info("License key is valid, unlocking enterprise features.", &[]);
        true
    } else {
        crate::logs::info("License key is invalid.", &[]);
        false
    }
}

/// `PlatformService.SaveLicense`.
async fn save(app: &App, bytes: &[u8]) -> ApiResult<License> {
    let s = validate(bytes).map_err(validation_error)?;
    let mut license = unmarshal(&s)?;
    let invalid =
        |d: &str| AppError::bad_request("api.license.add_license.invalid.app_error", "").detail(d.to_string());
    let Some(f) = &license.features else { return Err(invalid("license.Features is nil")) };
    let Some(users) = f.users else { return Err(invalid("license.Features.Users is nil")) };
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM Users LEFT JOIN Bots ON Users.Id = Bots.UserId WHERE Users.DeleteAt = 0 \
         AND (Users.RemoteId = '' OR Users.RemoteId IS NULL) AND Bots.UserId IS NULL",
    )
    .fetch_one(&app.db)
    .await
    .map_err(|e| AppError::bad_request("api.license.add_license.invalid_count.app_error", "").detail(e.to_string()))?;
    let single_guests: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (SELECT cm.UserId FROM ChannelMembers cm JOIN Users u ON cm.UserId = u.Id \
         JOIN Channels c ON cm.ChannelId = c.Id WHERE u.Roles ILIKE '%system_guest%' AND u.DeleteAt = 0 \
         AND c.DeleteAt = 0 AND c.Type IN ('O', 'P') GROUP BY cm.UserId HAVING COUNT(cm.ChannelId) = 1) AS single_channel_guests",
    )
    .fetch_one(&app.db)
    .await
    .ok();
    let effective = single_guests.map_or(count, |g| (count - g).max(0));
    if effective > users {
        return Err(AppError::bad_request("api.license.add_license.unique_users.app_error", "")
            .with_params(&[("Users", &users.to_string()), ("Count", &effective.to_string())]));
    }
    if license.expires_at < now_ms() {
        return Err(AppError::bad_request("api.license.add_license.expired.app_error", ""));
    }
    set_license(app, Some(license.clone()));
    license.features.get_or_insert_with(Default::default).set_defaults();
    // `LicenseRecord.IsValid`, then the row (kept if it exists).
    let record_err = if !crate::app::is_valid_id(&license.id) {
        Some("model.license_record.is_valid.id.app_error")
    } else if bytes.is_empty() || bytes.len() > 10000 {
        Some("model.license_record.is_valid.bytes.app_error")
    } else {
        None
    };
    let saved = match record_err {
        Some(id) => Err(AppError::bad_request(id, "")),
        None => {
            sqlx::query("INSERT INTO Licenses (Id, CreateAt, Bytes) VALUES ($1, $2, $3) ON CONFLICT (Id) DO NOTHING")
                .bind(&license.id)
                .bind(now_ms())
                .bind(String::from_utf8_lossy(bytes).as_ref())
                .execute(&app.db)
                .await
                .map(|_| ())
                .map_err(|e| AppError::internal("api.license.add_license.save.app_error", e.to_string()))
        }
    };
    if let Err(e) = saved {
        let _ = remove(app).await;
        return Err(e);
    }
    if let Err(e) = sqlx::query(
        "INSERT INTO Systems (Name, Value) VALUES ('ActiveLicenseId', $1) ON CONFLICT (Name) DO UPDATE SET Value = $1",
    )
    .bind(&license.id)
    .execute(&app.db)
    .await
    {
        let _ = remove(app).await;
        return Err(AppError::internal("api.license.add_license.save_active.app_error", e.to_string()));
    }
    if !license.is_cloud() {
        let _ = sqlx::query("DELETE FROM Systems WHERE Name = 'HostedPurchaseNeedsScreening'").execute(&app.db).await;
    }
    // InvalidateAllCaches; its LoadLicense finds the license just saved (not reloaded here: load can call save).
    reloaded_caches(app);
    Ok(license)
}

/// `PlatformService.RemoveLicense`: nothing without a license.
async fn remove(app: &App) -> ApiResult<()> {
    if current().is_none() {
        return Ok(());
    }
    crate::logs::info("Remove license.", &[("id", "ActiveLicenseId".into())]);
    sqlx::query(
        "INSERT INTO Systems (Name, Value) VALUES ('ActiveLicenseId', '') ON CONFLICT (Name) DO UPDATE SET Value = ''",
    )
    .execute(&app.db)
    .await
    .map_err(|e| AppError::internal("app.system.save.app_error", e.to_string()))?;
    set_license(app, None);
    reloaded_caches(app);
    Ok(())
}

// ---------- routes ----------

/// `parseLicenseFileFromRequest`: the first file of the form's `license` field.
async fn license_file(app: &App, headers: &HeaderMap, body: Body) -> ApiResult<(Vec<u8>, String)> {
    let parse_err = |d: String| AppError::bad_request("api.license.parse_license.parse_form.app_error", "").detail(d);
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let boundary = multer::parse_boundary(ct).map_err(|e| parse_err(e.to_string()))?;
    let max = app.cfg.i64("FileSettings.MaxFileSize").max(0) as u64;
    let constraints =
        multer::Constraints::new().size_limit(multer::SizeLimit::new().whole_stream(max.saturating_add(10 << 20)));
    let mut mp = multer::Multipart::with_constraints(body.into_data_stream(), boundary, constraints);
    let mut found: Option<(Vec<u8>, String)> = None;
    loop {
        match mp.next_field().await {
            Ok(None) => break,
            Err(e) => return Err(parse_err(e.to_string())),
            Ok(Some(field)) => {
                let is_file = field.name() == Some("license") && field.file_name().is_some();
                let name = field.file_name().unwrap_or("").to_string();
                let data = field.bytes().await.map_err(|e| parse_err(e.to_string()))?;
                if is_file && found.is_none() {
                    found = Some((data.to_vec(), name));
                }
            }
        }
    }
    found.ok_or_else(|| AppError::bad_request("api.license.add_license.no_file.app_error", ""))
}

/// Go's `json.NewEncoder(w).Encode(license)`.
fn license_json(l: &License) -> Response {
    let mut s = serde_json::to_string(l).unwrap();
    s.push('\n');
    ([(header::CONTENT_TYPE, "application/json")], s).into_response()
}

/// `addLicense`.
async fn add_license(St(app): St<State>, Auth(s): Auth, headers: HeaderMap, body: Body) -> ApiResult<Response> {
    crate::audit::log(&app, Some(&s), "attempt").await;
    app.require_unrestricted(&s, "manage_license_information").await?;
    let (bytes, _) = license_file(&app, &headers, body).await?;
    license_from_bytes(&bytes)?;
    let license = match save(&app, &bytes).await {
        Ok(l) => l,
        Err(e) => {
            let what = match e.id.as_str() {
                "api.license.add_license.expired.app_error" => "failed - expired or non-started license",
                "api.license.add_license.invalid.app_error" => "failed - invalid license",
                _ => "failed - unable to save license",
            };
            crate::audit::log(&app, Some(&s), what).await;
            return Err(e);
        }
    };
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(license_json(&license))
}

/// `previewLicense`: the license read and checked, not saved.
async fn preview_license(St(app): St<State>, Auth(s): Auth, headers: HeaderMap, body: Body) -> ApiResult<Response> {
    app.require_unrestricted(&s, "manage_license_information").await?;
    let (bytes, _) = license_file(&app, &headers, body).await?;
    Ok(license_json(&license_from_bytes(&bytes)?))
}

/// `removeLicense`.
async fn remove_license(St(app): St<State>, Auth(s): Auth) -> ApiResult<Json<Value>> {
    crate::audit::log(&app, Some(&s), "attempt").await;
    app.require_unrestricted(&s, "manage_license_information").await?;
    remove(&app).await?;
    crate::audit::log(&app, Some(&s), "success").await;
    Ok(crate::users::ok())
}

/// `getClientLicense`: the whole client license with `read_license_information`, the sanitized one otherwise.
async fn client_license(St(app): St<State>, MaybeAuth(s): MaybeAuth) -> Response {
    let full = match &s {
        Some(s) => app.has_perm(s, "read_license_information").await,
        None => false,
    };
    let v = if full { client() } else { sanitized_client() };
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}
