//! The plugin marketplace: the plugins `PluginSettings.MarketplaceURL` lists for this server (with
//! `EnableRemoteMarketplace`) merged with the installed ones, and installing the latest compatible version of one
//! after checking its signature.

use base64::Engine as _;
use bytes::Bytes;
use serde_json::{Value as J, json};

use super::{available, manifest_id, schema};
use crate::app::{ApiResult, App, AppError, State};

/// What the marketplace may send for one bundle at most.
const MAX_BUNDLE: usize = 1 << 30;

/// The filter of `GET /plugins/marketplace`.
#[derive(Default)]
pub struct Filter {
    pub filter: String,
    pub local_only: bool,
    pub remote_only: bool,
}

/// The query this server sends the marketplace, sorted by key.
fn base_query(plugin_id: &str) -> String {
    let platform = format!(
        "{}-{}",
        match std::env::consts::OS {
            "macos" => "darwin",
            o => o,
        },
        match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            a => a,
        }
    );
    let mut v = crate::gourl::Values::default();
    for (k, val) in [
        ("page", "0"),
        ("filter", ""),
        ("server_version", crate::SERVER_VERSION),
        ("build_enterprise_ready", "false"),
        ("enterprise_plugins", "false"),
        ("cloud", "false"),
        ("local_only", "false"),
        ("remote_only", "false"),
        ("platform", &platform),
        ("plugin_id", plugin_id),
        ("return_all_versions", "false"),
    ] {
        v.add(k, val);
    }
    v.encode()
}

/// The marketplace's plugins (for one plugin id, or all).
async fn remote(app: &App, plugin_id: &str) -> Result<Vec<J>, String> {
    let base = app.cfg.str("PluginSettings.MarketplaceURL").trim_end_matches('/');
    let url =
        reqwest::Url::parse(&format!("{base}/api/v1/plugins?{}", base_query(plugin_id))).map_err(|e| e.to_string())?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    let client = if local {
        app.trusted_client()
    } else {
        if !app.untrusted_url_allowed(&url) {
            return Err("address forbidden".into());
        }
        app.integration_client()
    };
    let r = client.get(url).timeout(std::time::Duration::from_secs(30)).send().await.map_err(|e| e.to_string())?;
    if r.status() != reqwest::StatusCode::OK {
        return Err(format!("failed with status code {}", r.status().as_u16()));
    }
    let body = r.bytes().await.map_err(|e| e.to_string())?;
    let list: Option<Vec<J>> = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    Ok(list.unwrap_or_default().into_iter().filter(|p| p.get("manifest").is_some_and(|m| m.is_object())).collect())
}

/// `GET /plugins/marketplace`: by name, `null` when none matches.
pub async fn list(app: &State, f: &Filter) -> ApiResult<J> {
    let mut plugins: Vec<J> = vec![];
    if app.cfg.bool("PluginSettings.EnableRemoteMarketplace") && !f.local_only {
        let remote = remote(app, "")
            .await
            .map_err(|e| AppError::internal("app.plugin.marketplace_client.failed_to_fetch", e))?;
        plugins = remote.iter().map(|p| schema::normalize("BaseMarketplacePlugin", p)).collect();
        for p in plugins.iter_mut() {
            if let Some(o) = p.as_object_mut() {
                o.insert("installed_version".into(), J::String(String::new()));
            }
        }
    }
    if !f.remote_only {
        for (dir, m) in available(app) {
            let id = manifest_id(&m).to_string();
            let version = m.get("version").cloned().unwrap_or(J::String(String::new()));
            if let Some(p) =
                plugins.iter_mut().find(|p| p.pointer("/manifest/id").and_then(J::as_str) == Some(id.as_str()))
            {
                p["installed_version"] = version;
                continue;
            }
            let s = |k: &str| m.get(k).and_then(J::as_str).unwrap_or("").to_string();
            let mut local = json!({
                "homepage_url": s("homepage_url"), "icon_data": icon(&dir, &s("icon_path")), "download_url": "",
                "release_notes_url": s("release_notes_url"), "hosting": "", "author_type": "", "release_stage": "",
                "enterprise": false, "signature": "", "manifest": m,
            });
            if app.cfg.bool("PluginSettings.EnableRemoteMarketplace") {
                local["labels"] = json!([{"name": "Local", "description": "This plugin is not listed in the marketplace", "url": "", "color": ""}]);
            }
            let mut local = schema::normalize("BaseMarketplacePlugin", &local);
            if let Some(o) = local.as_object_mut() {
                o.insert("installed_version".into(), version);
            }
            plugins.push(local);
        }
    }
    let wanted = f.filter.trim().to_lowercase();
    let field = |p: &J, k: &str| p.pointer(&format!("/manifest/{k}")).and_then(J::as_str).unwrap_or("").to_lowercase();
    let mut out: Vec<J> = plugins
        .into_iter()
        .filter(|p| {
            wanted.is_empty()
                || field(p, "id") == wanted
                || field(p, "name").contains(&wanted)
                || field(p, "description").contains(&wanted)
        })
        .collect();
    out.sort_by_key(|p| field(p, "name"));
    Ok(if out.is_empty() { J::Null } else { J::Array(out) })
}

/// A local plugin's icon as a data URL, when it is an SVG.
fn icon(dir: &std::path::Path, path: &str) -> String {
    if path.is_empty() || path.split('/').any(|s| s == "..") {
        return String::new();
    }
    match std::fs::read(dir.join(path)) {
        Ok(b) if is_svg(&b) => {
            format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(b))
        }
        _ => String::new(),
    }
}

fn is_svg(b: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&b[..b.len().min(4096)]).to_lowercase();
    head.contains("<svg")
}

/// `POST /plugins/marketplace`: the latest version the marketplace lists for this server, downloaded, its
/// signature checked, installed (replacing the installed version).
pub async fn install(app: &State, id: &str) -> ApiResult<J> {
    let not_found = || AppError::internal("app.plugin.marketplace_plugins.not_found.app_error", "");
    crate::logs::info("Installing plugin from marketplace", &[("plugin_id", id.into())]);
    if !app.cfg.bool("PluginSettings.EnableRemoteMarketplace") {
        return Err(not_found());
    }
    let plugin = match remote(app, id).await {
        Ok(list) if list.len() == 1 => list.into_iter().next().ok_or_else(not_found)?,
        Ok(_) => return Err(not_found()),
        Err(e) => {
            crate::logs::warn("Failed to reach Marketplace to install plugin", &[("error", e.into())]);
            return Err(not_found());
        }
    };
    let s = |k: &str| plugin.get(k).and_then(J::as_str).unwrap_or("").to_string();
    let version = plugin.pointer("/manifest/version").and_then(J::as_str).unwrap_or("");
    if !super::install::strict_semver(version) {
        return Err(AppError::bad_request("app.prepackaged-plugin.invalid_version.app_error", ""));
    }
    let failed = |d: String| AppError::internal("app.plugin.install_marketplace_plugin.app_error", d);
    let url = reqwest::Url::parse(&s("download_url")).map_err(|e| failed(e.to_string()))?;
    if url.scheme() != "https" && !app.cfg.bool("PluginSettings.AllowInsecureDownloadURL") {
        return Err(failed(format!("insecure url not allowed {url}")));
    }
    let bundle = super::http::download(app, &url, MAX_BUNDLE).await.map_err(failed)?;
    let signature = base64::engine::general_purpose::STANDARD.decode(s("signature")).map_err(|e| {
        AppError::new(axum::http::StatusCode::NOT_IMPLEMENTED, "app.plugin.signature_decode.app_error", "")
            .detail(e.to_string())
    })?;
    let checked = {
        let (app, bundle, signature) = (app.clone(), bundle.clone(), signature.clone());
        tokio::task::spawn_blocking(move || super::signature::verify(&app, &bundle, &signature)).await.unwrap_or(false)
    };
    if !checked {
        return Err(AppError::internal("api.plugin.verify_plugin.app_error", ""));
    }
    super::install::install(app, bundle, Some(Bytes::from(signature)), true).await
}
