//! HTTP boot-script server (phase 06c): hyper 1.x listener, one endpoint,
//! MAC→Client resolution, ScsiTarget assembly, script generation.
//! Returns text/plain iPXE scripts.

pub mod boot_script;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::sync::RwLock;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::iscsi::registry::TargetRegistry;
use crate::iscsi::scsi::{LogicalUnit, ScsiTarget};
use crate::manager::{ArcStore, ClientManager};
use crate::volume::{RamOverlay, Volume};

/// Start the HTTP boot-script server. Binds to `boot.http_bind:boot.http_port`.
/// Runs until the listener is closed or errors.
pub async fn serve(
    cfg: Arc<Config>,
    manager: Arc<ClientManager>,
    registry: Arc<RwLock<TargetRegistry>>,
    token: CancellationToken,
) -> std::io::Result<()> {
    let boot = cfg
        .boot
        .as_ref()
        .expect("http::serve called without [boot] section");
    let addr = SocketAddr::new(boot.http_bind, boot.http_port);
    let listener = TcpListener::bind(addr).await?;

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _peer) = result?;
                let cfg = cfg.clone();
                let manager = manager.clone();
                let registry = registry.clone();

                tokio::spawn(async move {
                    let svc =
                        service_fn(move |req| handle(req, cfg.clone(), manager.clone(), registry.clone()));
                    if let Err(e) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await
                    {
                        eprintln!("http: connection error: {e}");
                    }
                });
            }
            _ = token.cancelled() => {
                tracing::info!("http: shutting down");
                return Ok(());
            }
        }
    }
}

/// Generate an IQN for a client using its name (preferred) or MAC.
fn client_iqn(client: &crate::manager::ClientConfig) -> String {
    let suffix = client.name.as_deref().unwrap_or(&client.mac);
    format!("iqn.2026-06.dev.xboot:{suffix}")
}

/// Handle one HTTP request.
async fn handle(
    req: Request<Incoming>,
    cfg: Arc<Config>,
    manager: Arc<ClientManager>,
    registry: Arc<RwLock<TargetRegistry>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Only GET /boot.ipxe
    if req.method() != Method::GET || req.uri().path() != "/boot.ipxe" {
        return Ok(not_found());
    }

    // Parse ?mac=...
    let mac = req.uri().query().and_then(parse_mac_param);
    let mac = match mac {
        Some(m) => m,
        None => return Ok(bad_request("Missing 'mac' parameter")),
    };

    // Resolve MAC → client config.
    let client = match manager.resolve(&mac) {
        Some(c) => c.clone(),
        None => return Ok(not_found()),
    };

    let boot = cfg.boot.as_ref().expect("boot section must be present");
    let iqn = client_iqn(&client);

    // Build ScsiTarget: LUN 0 = system, LUN 1.. = games.
    let mut luns: Vec<Option<LogicalUnit>> = Vec::new();

    if let Some(store) = manager.get_store(&client.system_disk_id) {
        let vol = Volume::new(Box::new(ArcStore(store)), Box::new(RamOverlay::new()));
        luns.push(Some(LogicalUnit::new(vol).with_serial(&iqn)));
    }

    for game_id in &client.game_disk_ids {
        if let Some(store) = manager.get_store(game_id) {
            let vol = Volume::new(Box::new(ArcStore(store)), Box::new(RamOverlay::new()));
            luns.push(Some(LogicalUnit::new(vol)));
        }
    }

    let target = ScsiTarget::new(luns);

    // Register in the iSCSI registry.
    {
        registry.write().unwrap().insert(&iqn, target);
    }

    // Generate the boot script.
    let server_ip = boot.server_ip.to_string();
    let script = boot_script::generate(&iqn, &server_ip);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from(script)))
        .unwrap())
}

/// Extract `mac=...` from a query string. Percent-decodes the value.
fn parse_mac_param(query: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("mac=") {
            return Some(decode_percent(value));
        }
    }
    None
}

/// Minimal percent-decoder for URL-encoded hex values like %3A.
fn decode_percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4 | lo) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from("Not Found")))
        .unwrap()
}

fn bad_request(msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Full::new(Bytes::from(msg.to_string())))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BootConfig, ByteSize, Client, Config, Disk, DiskType};
    use std::net::{IpAddr, Ipv4Addr};

    fn test_cfg(tmp_dir: &std::path::Path) -> Config {
        let img = tmp_dir.join("img.raw");
        let game = tmp_dir.join("game.raw");
        std::fs::write(&img, vec![0u8; 4096]).unwrap();
        std::fs::write(&game, vec![0u8; 4096]).unwrap();

        Config {
            disks: vec![
                Disk {
                    id: "img".into(),
                    disk_type: DiskType::Image,
                    backing: img.display().to_string(),
                    mode: None,
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
                Disk {
                    id: "game1".into(),
                    disk_type: DiskType::Game,
                    backing: game.display().to_string(),
                    mode: None,
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
                Disk {
                    id: "wb".into(),
                    disk_type: DiskType::Writeback,
                    backing: tmp_dir.display().to_string(),
                    mode: None,
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
            ],
            clients: vec![Client {
                mac: "aa:bb:cc:dd:ee:01".into(),
                name: Some("pc-01".into()),
                system: "img".into(),
                games: vec!["game1".into()],
                writeback: "wb".into(),
            }],
            client_defaults: None,
            boot: Some(BootConfig {
                server_ip: Ipv4Addr::new(192, 168, 1, 10),
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                http_bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                http_port: 0, // OS picks a free port
                iscsi_port: 3260,
                tftp_root: "/tmp".into(),
                bios_filename: "undionly.kpxe".into(),
                uefi_filename: "ipxe.efi".into(),
                http_script_url: "http://192.168.1.10/boot.ipxe".into(),
            }),
        }
    }

    /// Start the server on a random port, return the bound address.
    async fn start_server(
        cfg: Config,
        manager: Arc<ClientManager>,
        registry: Arc<RwLock<TargetRegistry>>,
    ) -> SocketAddr {
        let cfg = Arc::new(cfg);
        let boot = cfg.boot.as_ref().unwrap();
        let listener = TcpListener::bind(SocketAddr::new(boot.http_bind, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (stream, _peer) = listener.accept().await.unwrap();
                let cfg = cfg.clone();
                let manager = manager.clone();
                let registry = registry.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        handle(req, cfg.clone(), manager.clone(), registry.clone())
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });

        addr
    }

    #[tokio::test]
    async fn get_boot_ipxe_returns_200_with_script() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(RwLock::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager.clone(), registry.clone()).await;

        let url = format!("http://{}/boot.ipxe?mac=aa:bb:cc:dd:ee:01", addr);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(body.contains("#!ipxe"));
        assert!(body.contains("iqn.2026-06.dev.xboot:pc-01"));
        assert!(body.contains("sanboot iscsi:192.168.1.10"));
    }

    #[tokio::test]
    async fn missing_mac_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(RwLock::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager, registry).await;

        let url = format!("http://{}/boot.ipxe", addr);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn unknown_mac_without_defaults_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(RwLock::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager, registry).await;

        let url = format!("http://{}/boot.ipxe?mac=ff:ff:ff:ff:ff:ff", addr);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn wrong_path_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(RwLock::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager, registry).await;

        let url = format!("http://{}/other", addr);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn post_method_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(RwLock::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager, registry).await;

        let client = reqwest::Client::new();
        let url = format!("http://{}/boot.ipxe?mac=aa:bb:cc:dd:ee:01", addr);
        let resp = client.post(&url).send().await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn happy_path_registers_target_in_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(RwLock::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager.clone(), registry.clone()).await;

        let url = format!("http://{}/boot.ipxe?mac=aa:bb:cc:dd:ee:01", addr);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);

        // Verify the target was registered.
        assert!(registry
            .read()
            .unwrap()
            .get("iqn.2026-06.dev.xboot:pc-01")
            .is_some());
    }

    #[test]
    fn parse_mac_extracts_value() {
        assert_eq!(
            parse_mac_param("mac=aa:bb:cc:dd:ee:01"),
            Some("aa:bb:cc:dd:ee:01".into())
        );
        assert_eq!(
            parse_mac_param("other=foo&mac=11:22:33:44:55:66"),
            Some("11:22:33:44:55:66".into())
        );
        assert_eq!(parse_mac_param("mac="), Some(String::new()));
        assert_eq!(parse_mac_param(""), None);
        assert_eq!(parse_mac_param("foo=bar"), None);
    }

    #[test]
    fn parse_mac_decodes_percent_encoded_colon() {
        assert_eq!(
            parse_mac_param("mac=aa%3Abb%3Acc%3Add%3Aee%3A01"),
            Some("aa:bb:cc:dd:ee:01".into())
        );
    }

    #[test]
    fn decode_percent_handles_plain_and_encoded() {
        assert_eq!(decode_percent("hello"), "hello");
        assert_eq!(decode_percent("ab%3Acd"), "ab:cd");
        assert_eq!(decode_percent("%20"), " ");
        assert_eq!(decode_percent("%gg"), "%gg"); // invalid hex left as-is
    }

    #[test]
    fn client_iqn_uses_name_when_present() {
        let c = crate::manager::ClientConfig {
            mac: "aa:bb:cc:dd:ee:01".into(),
            name: Some("pc-01".into()),
            system_disk_id: "img".into(),
            game_disk_ids: vec![],
            writeback_disk_id: "wb".into(),
        };
        assert_eq!(client_iqn(&c), "iqn.2026-06.dev.xboot:pc-01");
    }

    #[test]
    fn client_iqn_falls_back_to_mac() {
        let c = crate::manager::ClientConfig {
            mac: "aa-bb-cc-dd-ee-01".into(),
            name: None,
            system_disk_id: "img".into(),
            game_disk_ids: vec![],
            writeback_disk_id: "wb".into(),
        };
        assert_eq!(client_iqn(&c), "iqn.2026-06.dev.xboot:aa-bb-cc-dd-ee-01");
    }
}
