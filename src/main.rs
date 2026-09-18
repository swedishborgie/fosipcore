//! IPC Camera WebSocket Server
//!
//! Replaces the obsolete Windows IPCWebComponents.exe by implementing
//! the WebSocket protocol the camera's web UI expects.
//!
//! The camera's address is discovered automatically from the browser's
//! login message (the page loads from the camera, so the browser knows
//! its address). No camera configuration required.

#![allow(dead_code)] // Many protocol constants/types defined for future phases

mod audio;
mod core;
mod net;
mod protocol;
mod proxy;
mod service_manager;
mod state;
mod video;

use anyhow::Result;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

/// Ports the WebSocket server listens on.
///
/// These match the hard-coded values in the camera's JavaScript client:
/// `SERVICE_MANAGER_PORT = 50000` in `var.js`.
/// The core port is returned by the service manager via the port allocator.
///
/// There is no fixed HTTP-FLV port: each logged-in session allocates its
/// own live port from the pool (default range 20000–25999, mirroring the
/// reference's `tid % 6000 + 20000`) and receives it in its own `InitInfo`.
const DEFAULT_SERVICE_MANAGER_PORT: u16 = 50000;
const DEFAULT_SERVICE_VERSION: &str = "2.0.1.1";

/// fosipcore's own version, injected at build time by `build.rs`
/// (from `FOSIPCORE_VERSION` in CI, falling back to the Cargo.toml version).
const VERSION: &str = env!("FOSIPCORE_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,fosipcore=info")),
        )
        .init();

    let service_manager_port: u16 = std::env::var("FOSIPCORE_SERVICE_MANAGER_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_SERVICE_MANAGER_PORT);

    tracing::info!("Starting fosipcore v{VERSION}");
    tracing::info!("Service manager: 127.0.0.1:{}", service_manager_port);
    tracing::info!("Camera address: auto-discovered from client login (no config needed)");

    // Bind listeners to loopback only — the camera's web UI (the sole client)
    // hardcodes 127.0.0.1 for every dial (websocket_service_manager.js,
    // websocket_core.js, FLV playlist URLs). Binding to 0.0.0.0 would let
    // any LAN host use this box as an open proxy to the claimed camera.
    let core_port = 50001;
    let core_listener = TcpListener::bind(format!("127.0.0.1:{core_port}")).await?;

    tracing::info!("Core server: 127.0.0.1:{core_port}");

    // Live-port pool for per-session HTTP-FLV servers. Each session gets
    // its own listener on an allocated port; the active session's port is
    // logged when its login allocates it.
    let pool = video::ports::PortPool::from_env();
    let (pool_base, pool_end) = pool.range();
    tracing::info!("Live port pool: {pool_base}..{pool_end} (one HTTP-FLV listener per session)");

    let sm_listener = TcpListener::bind(format!("127.0.0.1:{service_manager_port}")).await?;

    // Spawn service manager (returns core_port to every client)
    let service_version = DEFAULT_SERVICE_VERSION.to_string();
    tokio::spawn(async move {
        if let Err(e) = service_manager::run(sm_listener, service_version, core_port).await {
            tracing::error!("Service manager error: {}", e);
        }
    });

    // Run core server in main task. Each session's HTTP-FLV listener is
    // spawned by that session's login (see core::handle_login).
    core::run(core_listener, pool).await?;

    Ok(())
}
