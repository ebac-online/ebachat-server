//! The compiler's version for the startup log line where Go prints its runtime version (`go_version`).

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = std::process::Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=EBACHAT_RUSTC_VERSION={version}");
    println!("cargo:rerun-if-changed=build.rs");
    migrations();
}

/// `migrations.rs` in OUT_DIR: every file of `migrations/postgres` (Mattermost's) and `migrations/ebachat` (ours) as
/// `(file name, contents)`, embedded as the Go server embeds its own (`src/migrate.rs`).
fn migrations() {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("migrations");
    let mut out = String::new();
    for (konst, dir) in [("STOCK", "postgres"), ("EBACHAT", "ebachat")] {
        let path = root.join(dir);
        println!("cargo:rerun-if-changed={}", path.display());
        let mut files: Vec<_> = std::fs::read_dir(&path)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "sql"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        out += &format!("pub static {konst}: &[(&str, &str)] = &[\n");
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            out += &format!("    ({name:?}, include_str!({:?})),\n", f.display().to_string());
        }
        out += "];\n";
    }
    std::fs::write(std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("migrations.rs"), out).unwrap();
}
