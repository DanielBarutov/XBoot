use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use xboot_core::iscsi::registry::TargetRegistry;
use xboot_core::manager::ClientManager;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let path = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: xboot <config.toml>");
            return ExitCode::from(2);
        }
    };

    let cfg = match xboot_core::config::load_from_path(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let boot = match &cfg.boot {
        Some(b) => b.clone(),
        None => {
            tracing::error!("[boot] section is required");
            return ExitCode::FAILURE;
        }
    };

    // Build ClientManager — opens all RO backing stores.
    let manager = match ClientManager::new(&cfg) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let registry = Arc::new(RwLock::new(TargetRegistry::new()));
    let token = CancellationToken::new();
    let cfg = Arc::new(cfg);
    let iscsi_addr = SocketAddr::new(boot.bind, boot.iscsi_port);

    tracing::info!(
        server_ip = %boot.server_ip,
        dhcp_bind = %boot.bind,
        http_bind = %boot.http_bind,
        http_port = boot.http_port,
        iscsi_port = boot.iscsi_port,
        tftp_root = %boot.tftp_root.display(),
        "starting xboot services"
    );

    // proxyDHCP: :67 + :4011
    let dhcp = tokio::spawn({
        let boot_cfg = boot.clone();
        let t = token.clone();
        async move {
            if let Err(e) = xboot_core::net::dhcp::server::serve(boot_cfg, t).await {
                tracing::error!("dhcp: {e}");
            }
        }
    });

    // TFTP: :69
    let tftp = tokio::spawn({
        let srv = xboot_core::net::tftp::server::TftpServer::new(boot.bind, boot.tftp_root.clone());
        let t = token.clone();
        async move {
            if let Err(e) = xboot_core::net::tftp::server::serve(srv, t).await {
                tracing::error!("tftp: {e}");
            }
        }
    });

    // HTTP: boot.http_bind:boot.http_port
    let http = tokio::spawn({
        let c = cfg.clone();
        let m = manager.clone();
        let r = registry.clone();
        let t = token.clone();
        async move {
            if let Err(e) = xboot_core::net::http::serve(c, m, r, t).await {
                tracing::error!("http: {e}");
            }
        }
    });

    // iSCSI: boot.bind:boot.iscsi_port
    let iscsi = tokio::spawn({
        let r = registry.clone();
        let t = token.clone();
        async move {
            let listener = match TcpListener::bind(iscsi_addr).await {
                Ok(l) => {
                    tracing::info!("iscsi: listening on {iscsi_addr}");
                    l
                }
                Err(e) => {
                    tracing::error!("iscsi: bind {iscsi_addr}: {e}");
                    return;
                }
            };
            if let Err(e) = xboot_core::iscsi::transport::serve(listener, r, t).await {
                tracing::error!("iscsi: {e}");
            }
        }
    });

    // Ctrl+C → cancel all.
    let ctrl_c = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("SIGINT received, shutting down...");
        token.cancel();
    });

    // Wait for any service to exit (first error or ctrl_c).
    tokio::select! {
        _ = dhcp => {},
        _ = tftp => {},
        _ = http => {},
        _ = iscsi => {},
        _ = ctrl_c => {},
    }

    tracing::info!("xboot stopped");
    ExitCode::SUCCESS
}
