//! Plugin bundles (`.tar.gz`) installed and removed through the API. A bundle is unpacked into the plugins
//! directory and kept in the file store as `plugins/<id>.tar.gz`; at start, the stored bundles the directory
//! lacks are unpacked again, so the directory needs no volume of its own.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::{Value as J, json};

use super::{PLUGINS, available, manifest_id};
use crate::app::{ApiResult, AppError, State};

const STORE_DIR: &str = "plugins";
/// What a bundle may unpack to at most.
const MAX_UNPACKED: u64 = 2 << 30;
const MAX_ENTRIES: usize = 50_000;

fn store_path(id: &str) -> String {
    format!("{STORE_DIR}/{id}.tar.gz")
}

/// A plugin id: 3 to 190 letters, digits, `.`, `-` and `_`.
pub fn valid_id(id: &str) -> bool {
    (3..=190).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Unpacks a `.tar.gz` into `dest`: directories and regular files (executable or not), every path inside `dest`;
/// links and other entries are skipped.
fn unpack(bundle: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bundle));
    let (mut total, mut count) = (0u64, 0usize);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        count += 1;
        if count > MAX_ENTRIES {
            return Err("too many files".into());
        }
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if path.is_absolute() || path.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
            return Err(format!("invalid path {}", path.display()));
        }
        // Normal components only: `./jira/...` and the bundle's `.` itself.
        let to = dest.join(path.components().filter(|c| matches!(c, Component::Normal(_))).collect::<PathBuf>());
        match entry.header().entry_type() {
            tar::EntryType::Directory => std::fs::create_dir_all(&to).map_err(|e| e.to_string())?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let size = entry.header().size().map_err(|e| e.to_string())?;
                total = total.saturating_add(size);
                if total > MAX_UNPACKED {
                    return Err("bundle too large".into());
                }
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let mut f = std::fs::File::create(&to).map_err(|e| e.to_string())?;
                std::io::copy(&mut (&mut entry).take(size), &mut f).map_err(|e| e.to_string())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let exec = entry.header().mode().unwrap_or(0) & 0o111 != 0;
                    let mode = if exec { 0o755 } else { 0o644 };
                    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}

/// The bundle unpacked into `tmp`: its manifest and the directory holding it (the bundle's only top directory,
/// if it has one).
fn extract(bundle: &[u8], tmp: &Path) -> ApiResult<(J, PathBuf)> {
    let bad = |id: &str, detail: String| AppError::bad_request(id, "").detail(detail);
    unpack(bundle, tmp).map_err(|e| bad("app.plugin.extract.app_error", e))?;
    let entries: Vec<_> = std::fs::read_dir(tmp)
        .map_err(|e| AppError::internal("app.plugin.filesystem.app_error", e.to_string()))?
        .flatten()
        .collect();
    let root = match entries.as_slice() {
        [only] if only.path().is_dir() => only.path(),
        _ => tmp.to_path_buf(),
    };
    let manifest: J = std::fs::read(root.join("plugin.json"))
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        .map_err(|e| bad("app.plugin.manifest.app_error", e))?;
    let manifest = super::schema::normalize("Manifest", &manifest);
    if !valid_id(manifest_id(&manifest)) {
        let e = AppError::bad_request("app.plugin.invalid_id.app_error", "").with_params(&[
            ("Min", "3"),
            ("Max", "190"),
            ("Regex", "^[a-zA-Z0-9-_\\.]+$"),
        ]);
        return Err(e);
    }
    Ok((manifest, root))
}

/// Installs a bundle: unpacked into the plugins directory (replacing an installed plugin of the same id only when
/// `replace`), started if enabled, and kept in the file store. Its manifest.
pub async fn install(app: &State, bundle: Bytes, signature: Option<Bytes>, replace: bool) -> ApiResult<J> {
    let tmp = std::env::temp_dir().join(format!("plugintmp-{}", crate::app::new_id()));
    let r = install_locally(app, bundle.clone(), replace, tmp.clone()).await;
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    let manifest = r?;
    let id = manifest_id(&manifest).to_string();
    crate::logs::info("Persisting plugin to filestore", &[("plugin_id", id.clone().into())]);
    if let Some(sig) = signature {
        app.files
            .write(&format!("{}.sig", store_path(&id)), sig)
            .await
            .map_err(|e| AppError::internal("app.plugin.store_signature.app_error", e.to_string()))?;
    }
    app.files
        .write(&store_path(&id), bundle)
        .await
        .map_err(|e| AppError::internal("app.plugin.store_bundle.app_error", e.to_string()))?;
    super::statuses_changed(app);
    Ok(manifest)
}

async fn install_locally(app: &State, bundle: Bytes, replace: bool, tmp: PathBuf) -> ApiResult<J> {
    let (manifest, root) = tokio::task::spawn_blocking(move || extract(&bundle, &tmp))
        .await
        .map_err(|e| AppError::internal("app.plugin.extract.app_error", e.to_string()))??;
    let id = manifest_id(&manifest).to_string();
    crate::logs::info("Installing extracted plugin", &[("plugin_id", id.clone().into())]);
    let _one = PLUGINS.sync.lock().await;
    if let Some((dir, existing)) = available(app).into_iter().find(|(_, m)| manifest_id(m) == id) {
        if !replace {
            return Err(conflict(&existing, &manifest));
        }
        super::deactivate(app, &id).await;
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|e| AppError::internal("app.plugin.install_id_failed_remove.app_error", e.to_string()))?;
    }
    let dest = plugins_dir(app).join(&id);
    let from = root.clone();
    let to = dest.clone();
    tokio::task::spawn_blocking(move || copy_dir(&from, &to))
        .await
        .map_err(|e| AppError::internal("app.plugin.mvdir.app_error", e.to_string()))?
        .map_err(|e| AppError::internal("app.plugin.mvdir.app_error", e.to_string()))?;
    if super::enabled(app, &id) {
        let a = super::activate(app, &dest, &manifest)
            .await
            .map_err(|e| AppError::internal("app.plugin.restart.app_error", format!("{e:#}")))?;
        if a.bundle.is_some() {
            app.hub.publish("plugin_enabled", json!({"manifest": super::client_manifest(&a)}), Default::default());
        }
    }
    Ok(manifest)
}

/// The answer to installing a plugin already installed, without `force`: both versions for the confirmation.
fn conflict(existing: &J, uploaded: &J) -> AppError {
    let s = |m: &J, k: &str| m.get(k).and_then(J::as_str).unwrap_or("").to_string();
    let or_existing = |k: &str| Some(s(uploaded, k)).filter(|v| !v.is_empty()).unwrap_or_else(|| s(existing, k));
    let (from, to) = (s(existing, "version"), s(uploaded, "version"));
    let direction = version_direction(&from, &to);
    let mut e = AppError::new(StatusCode::BAD_REQUEST, "app.plugin.install_id.app_error", "").detail(format!(
        "plugin {:?} conflict: existing version {from:?}, uploaded version {to:?} ({direction})",
        s(uploaded, "id")
    ));
    e.props = Some(Box::new(
        [
            ("plugin_id", s(uploaded, "id")),
            ("plugin_name", or_existing("name")),
            ("homepage_url", or_existing("homepage_url")),
            ("existing_version", from),
            ("uploaded_version", to),
            ("version_direction", direction.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    ));
    e
}

/// A strict semver version: major, minor, patch and the pre-release part.
fn semver(v: &str) -> Option<(u64, u64, u64, Option<String>)> {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p.split('+').next().unwrap_or("").to_string())),
        None => (v.split('+').next().unwrap_or(""), None),
    };
    let number =
        |x: &str| (!x.is_empty() && !(x.len() > 1 && x.starts_with('0'))).then(|| x.parse::<u64>().ok()).flatten();
    let mut n = core.split('.').map(number);
    let v = (n.next()??, n.next()??, n.next()??, pre);
    n.next().is_none().then_some(v)
}

pub fn strict_semver(v: &str) -> bool {
    semver(v).is_some()
}

/// Whether `to` is the same version as `from`, newer or older, by strict semver.
fn version_direction(from: &str, to: &str) -> &'static str {
    let (Some(a), Some(b)) = (semver(from), semver(to)) else { return "unknown" };
    let rank = |p: &Option<String>| p.clone().map_or((1, String::new()), |s| (0, s));
    match ((b.0, b.1, b.2), rank(&b.3)).cmp(&((a.0, a.1, a.2), rank(&a.3))) {
        std::cmp::Ordering::Equal => "same",
        std::cmp::Ordering::Greater => "upgrade",
        std::cmp::Ordering::Less => "downgrade",
    }
}

fn plugins_dir(app: &State) -> PathBuf {
    PathBuf::from(match app.cfg.str("PluginSettings.Directory") {
        "" => "./plugins",
        d => d,
    })
}

/// Removes an installed plugin: disabled, stopped, deleted from the plugins directory and the file store.
pub async fn remove(app: &State, id: &str) -> ApiResult<()> {
    let not_installed = || AppError::not_found("app.plugin.not_installed.app_error", "Plugin is not installed.");
    if !available(app).iter().any(|(_, m)| manifest_id(m) == id) {
        return Err(not_installed());
    }
    super::http::save_state(app, id, false).await?;
    let _one = PLUGINS.sync.lock().await;
    let (dir, _) = available(app).into_iter().find(|(_, m)| manifest_id(m) == id).ok_or_else(not_installed)?;
    super::deactivate(app, id).await;
    PLUGINS.remove_commands(id);
    tokio::fs::remove_dir_all(&dir)
        .await
        .map_err(|e| AppError::internal("app.plugin.remove.app_error", e.to_string()))?;
    for p in [store_path(id), format!("{}.sig", store_path(id))] {
        if app.files.exists(&p).await {
            app.files
                .remove(&p)
                .await
                .map_err(|e| AppError::internal("app.plugin.remove_bundle.app_error", e.to_string()))?;
        }
    }
    super::statuses_changed(app);
    Ok(())
}

/// Unpacks the bundles of the file store whose plugin the plugins directory lacks.
pub async fn unpack_stored(app: &State) {
    let Ok(files) = app.files.list(STORE_DIR).await else { return };
    let installed: Vec<String> = available(app).iter().map(|(_, m)| manifest_id(m).to_string()).collect();
    for (path, _) in files {
        let Some(id) = path.rsplit('/').next().and_then(|f| f.strip_suffix(".tar.gz")) else { continue };
        if installed.iter().any(|i| i == id) || !valid_id(id) {
            continue;
        }
        let r = async {
            let bundle = app.files.read(&path).await.map_err(|e| e.to_string())?;
            if app.cfg.bool("PluginSettings.RequirePluginSignature") {
                let sig = app.files.read(&format!("{path}.sig")).await.map_err(|e| e.to_string())?;
                let (a, b) = (app.clone(), bundle.clone());
                if !tokio::task::spawn_blocking(move || super::signature::verify(&a, &b, &sig)).await.unwrap_or(false) {
                    return Err("invalid plugin signature".to_string());
                }
            }
            let tmp = std::env::temp_dir().join(format!("plugintmp-{}", crate::app::new_id()));
            let dest = plugins_dir(app);
            let r = tokio::task::spawn_blocking({
                let tmp = tmp.clone();
                move || {
                    let (m, root) = extract(&bundle, &tmp).map_err(|e| format!("{}: {}", e.id, e.detailed_error))?;
                    copy_dir(&root, &dest.join(manifest_id(&m))).map_err(|e| e.to_string())
                }
            })
            .await
            .map_err(|e| e.to_string());
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            r?
        }
        .await;
        match r {
            Ok(()) => crate::logs::info("Unpacked plugin from the file store", &[("plugin_id", id.into())]),
            Err(e) => crate::logs::error(
                "Failed to unpack plugin from the file store",
                &[("plugin_id", id.into()), ("error", e.into())],
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(vec![], flate2::Compression::fast()));
        for (path, data, mode) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            let dir = path.ends_with('/');
            h.set_entry_type(if dir { tar::EntryType::Directory } else { tar::EntryType::Regular });
            // Written raw: `set_path` refuses `..`, which is what is tested.
            let name = h.as_old_mut().name.as_mut();
            name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn version_directions() {
        assert_eq!(version_direction("1.1.1", "1.1.1"), "same");
        assert_eq!(version_direction("1.1.1", "1.2.0"), "upgrade");
        assert_eq!(version_direction("2.0.0", "1.9.9"), "downgrade");
        assert_eq!(version_direction("1.0.0-rc.1", "1.0.0"), "upgrade");
        assert_eq!(version_direction("v1.0.0", "1.0.0"), "unknown");
        assert!(
            strict_semver("2.1.0") && strict_semver("1.0.0-rc.1") && !strict_semver("1.0") && !strict_semver("01.0.0")
        );
    }

    /// A real bundle (`PLUGIN_BUNDLE`) unpacks.
    #[test]
    #[ignore]
    fn real_bundle_unpacks() {
        let bundle = std::fs::read(std::env::var("PLUGIN_BUNDLE").unwrap()).unwrap();
        let dir = std::env::temp_dir().join(format!("ebachat-real-{}", crate::app::new_id()));
        let r = extract(&bundle, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let (m, _) = r.map_err(|e| e.detailed_error).unwrap();
        assert!(!manifest_id(&m).is_empty());
    }

    #[test]
    fn unpacks_inside_only() {
        let dir = std::env::temp_dir().join(format!("ebachat-unpack-{}", crate::app::new_id()));
        let good = bundle(&[("p/plugin.json", br#"{"id": "com.x.y"}"#, 0o644), ("p/server/run", b"x", 0o755)]);
        let (m, root) = extract(&good, &dir).unwrap();
        assert_eq!(m["id"], "com.x.y");
        assert_eq!(root, dir.join("p"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("p/server/run")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        let evil = bundle(&[("../escape", b"x", 0o644)]);
        let dir2 = dir.join("second");
        assert_eq!(extract(&evil, &dir2).unwrap_err().id, "app.plugin.extract.app_error");
        assert!(!dir.join("escape").exists());
        // As `tar -C dir .` writes it: `./` first, then `./p/...`.
        let dotted =
            bundle(&[("./", b"", 0o700), ("./p/", b"", 0o755), ("./p/plugin.json", br#"{"id": "com.x.z"}"#, 0o600)]);
        let (m, root) = extract(&dotted, &dir.join("dotted")).unwrap();
        assert_eq!((m["id"].as_str(), root), (Some("com.x.z"), dir.join("dotted/p")));
        let bad_id = bundle(&[("plugin.json", br#"{"id": "a/b"}"#, 0o644)]);
        assert_eq!(extract(&bad_id, &dir.join("third")).unwrap_err().id, "app.plugin.invalid_id.app_error");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
