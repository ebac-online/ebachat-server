//! Detached OpenPGP signatures of plugin bundles, checked against the built-in plugin signing keys
//! (`data/plugin_keys/`; the second one behind the `EnableMFIPluginSignaturePublicKey` flag), then the files of
//! `PluginSettings.SignaturePublicKeyFiles` (next to `config.json` unless absolute). Keys and signatures may be
//! armored or binary.

use std::io::Cursor;

use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};

use crate::app::App;

const PRODUCTION_KEY: &[u8] = include_bytes!("../../data/plugin_keys/production.asc");
const MFI_KEY: &[u8] = include_bytes!("../../data/plugin_keys/mfi.asc");

/// Whether `signature` signs `bundle` with one of the trusted keys.
pub fn verify(app: &App, bundle: &[u8], signature: &[u8]) -> bool {
    if verify_with(PRODUCTION_KEY, bundle, signature) {
        return true;
    }
    if app.cfg.feature("EnableMFIPluginSignaturePublicKey") && verify_with(MFI_KEY, bundle, signature) {
        return true;
    }
    let files = app.cfg.get("PluginSettings.SignaturePublicKeyFiles").as_array().cloned().unwrap_or_default();
    for name in files.iter().filter_map(|f| f.as_str()) {
        let path = std::path::Path::new(name);
        let path = if path.is_absolute() { path.to_path_buf() } else { crate::logs::find_dir("config").join(name) };
        match std::fs::read(&path) {
            Ok(key) if verify_with(&key, bundle, signature) => return true,
            Ok(_) => {}
            Err(_) => crate::logs::warn(
                "Unable to read configured signature public key file",
                &[("public_key_path", name.into())],
            ),
        }
    }
    false
}

/// Whether `signature` signs `data` with `key` (its primary key or one of its subkeys).
fn verify_with(key: &[u8], data: &[u8], signature: &[u8]) -> bool {
    let Ok((key, _)) = SignedPublicKey::from_reader_single(Cursor::new(key)) else { return false };
    let Ok((sig, _)) = DetachedSignature::from_reader_single(Cursor::new(signature)) else { return false };
    sig.verify(&key, data).is_ok() || key.public_subkeys.iter().any(|s| sig.verify(&s.key, data).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test key and a detached signature of `DATA` made with it (gpg, binary and armored).
    const KEY: &[u8] = include_bytes!("../../tests/data/plugin-test-key.asc");
    const SIG: &[u8] = include_bytes!("../../tests/data/plugin-test.sig");
    const SIG_ARMORED: &[u8] = include_bytes!("../../tests/data/plugin-test.sig.asc");
    const DATA: &[u8] = b"a plugin bundle\n";

    /// A real bundle and its signature (`PLUGIN_BUNDLE`, `PLUGIN_SIGNATURE`) against the built-in keys.
    #[test]
    #[ignore]
    fn real_bundle() {
        let read = |k: &str| std::fs::read(std::env::var(k).unwrap()).unwrap();
        let (bundle, sig) = (read("PLUGIN_BUNDLE"), read("PLUGIN_SIGNATURE"));
        assert!(verify_with(PRODUCTION_KEY, &bundle, &sig));
        assert!(!verify_with(PRODUCTION_KEY, &bundle[1..], &sig));
    }

    #[test]
    fn detached_signatures() {
        assert!(verify_with(KEY, DATA, SIG));
        assert!(verify_with(KEY, DATA, SIG_ARMORED));
        assert!(!verify_with(KEY, b"another bundle\n", SIG));
        assert!(!verify_with(PRODUCTION_KEY, DATA, SIG));
        assert!(!verify_with(KEY, DATA, b"not a signature"));
        // The built-in keys parse.
        assert!(SignedPublicKey::from_reader_single(Cursor::new(PRODUCTION_KEY)).is_ok());
        assert!(SignedPublicKey::from_reader_single(Cursor::new(MFI_KEY)).is_ok());
    }
}
