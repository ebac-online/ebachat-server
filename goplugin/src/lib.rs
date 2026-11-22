//! Runs Mattermost server plugins from the host side: the plugin process, HashiCorp go-plugin's net/rpc protocol
//! over yamux, and Go's gob encoding.

pub mod gob;
pub mod host;
pub mod rpc;

/// `GOPLUGIN_TRACE=1`: every gob message and broker step on stderr.
pub(crate) fn trace(msg: impl FnOnce() -> String) {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var_os("GOPLUGIN_TRACE").is_some());
    if *ON {
        eprintln!("[goplugin] {}", msg());
    }
}
