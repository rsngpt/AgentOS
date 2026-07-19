//! `agentosd` — the Agent OS daemon.
//!
//! Owns all sandbox state. Clients (CLI, GUI) connect over a Unix domain
//! socket (`~/.agentos/agentosd.sock`, mode 0600) speaking JSON-RPC 2.0.
//! Every microVM's VMM runs as a *child process* of this daemon so the kill
//! switch is a plain SIGKILL with no cooperation required from the guest.

mod monitor;
mod proxy;
mod registry;
mod rpc;

use std::path::PathBuf;

use tokio::net::UnixListener;
use tracing::{info, warn};

/// `~/.agentos` — daemon socket, images, and per-sandbox state live here.
fn agentos_home() -> PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    PathBuf::from(home).join(".agentos")
}

#[tokio::main]
async fn main() -> anyhow_lite::Result<()> {
    tracing_subscriber::fmt().init();

    let home = agentos_home();
    std::fs::create_dir_all(&home)?;
    let sock_path = home.join("agentosd.sock");
    // Stale socket from a previous run; safe to remove, we are the only binder.
    let _ = std::fs::remove_file(&sock_path);

    let listener = UnixListener::bind(&sock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;
    }
    info!(socket = %sock_path.display(), "agentosd listening");

    let registry = registry::Registry::new();

    loop {
        let (stream, _) = listener.accept().await?;
        let registry = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = rpc::serve_connection(stream, registry).await {
                warn!(error = %e, "client connection ended with error");
            }
        });
    }
}

/// Minimal stand-in for `anyhow` to keep scaffold dependencies lean.
mod anyhow_lite {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}
