//! Client manager (phase 06c): MAC → Client resolution + cached RO backing stores.
//! Pure logic, no I/O beyond opening backing stores at construction time.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::storage::BackingStore;

/// Resolved configuration for one client.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub mac: String,
    pub name: Option<String>,
    pub system_disk_id: String,
    pub game_disk_ids: Vec<String>,
    pub writeback_disk_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("client_defaults references unknown disk '{0}'")]
    UnknownDisk(String),
    #[error("failed to open backing store '{path}': {source}")]
    OpenBacking { path: String, source: io::Error },
}

/// Wraps an `Arc<dyn BackingStore>` into `Box<dyn BackingStore>` for Volume::new.
/// Lets many Volumes share the same read-only master via Arc.
pub struct ArcStore(pub Arc<dyn BackingStore>);

impl BackingStore for ArcStore {
    fn size_bytes(&self) -> u64 {
        self.0.size_bytes()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.read_at(offset, buf)
    }
}

/// Maps MAC addresses to client configurations and caches opened RO backing stores.
pub struct ClientManager {
    clients: HashMap<String, Arc<ClientConfig>>,
    defaults: Option<Arc<ClientConfig>>,
    stores: HashMap<String, Arc<dyn BackingStore>>,
}

impl ClientManager {
    /// Build the manager from a validated Config. Opens every image+game backing
    /// store and caches them behind `Arc`.
    pub fn new(cfg: &crate::config::Config) -> Result<Self, BuildError> {
        let mut clients: HashMap<String, Arc<ClientConfig>> = HashMap::new();
        for c in &cfg.clients {
            let cc = Arc::new(ClientConfig {
                mac: c.mac.to_lowercase(),
                name: c.name.clone(),
                system_disk_id: c.system.clone(),
                game_disk_ids: c.games.clone(),
                writeback_disk_id: c.writeback.clone(),
            });
            clients.insert(c.mac.to_lowercase(), cc);
        }

        let defaults = cfg.client_defaults.as_ref().map(|d| {
            Arc::new(ClientConfig {
                mac: String::new(),
                name: None,
                system_disk_id: d.system.clone(),
                game_disk_ids: d.games.clone(),
                writeback_disk_id: d.writeback.clone(),
            })
        });

        let mut stores: HashMap<String, Arc<dyn BackingStore>> = HashMap::new();
        for disk in &cfg.disks {
            if disk.disk_type == crate::config::DiskType::Image
                || disk.disk_type == crate::config::DiskType::Game
            {
                let path = Path::new(&disk.backing);
                let store = crate::storage::open_backing(path).map_err(|source| {
                    BuildError::OpenBacking {
                        path: disk.backing.clone(),
                        source,
                    }
                })?;
                stores.insert(disk.id.clone(), Arc::from(store));
            }
        }

        Ok(Self {
            clients,
            defaults,
            stores,
        })
    }

    /// Resolve a MAC address to a client config. Falls back to `client_defaults`.
    pub fn resolve(&self, mac: &str) -> Option<&ClientConfig> {
        let key = mac.to_lowercase();
        self.clients
            .get(&key)
            .or(self.defaults.as_ref())
            .map(|arc| arc.as_ref())
    }

    /// Get a cached RO backing store by disk ID.
    pub fn get_store(&self, disk_id: &str) -> Option<Arc<dyn BackingStore>> {
        self.stores.get(disk_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ByteSize;
    use crate::config::{BootConfig, Client, ClientDefaults, Config, Disk, DiskType};
    use std::net::{IpAddr, Ipv4Addr};

    fn test_boot() -> BootConfig {
        BootConfig {
            server_ip: Ipv4Addr::new(192, 168, 1, 10),
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            http_bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            http_port: 80,
            iscsi_port: 3260,
            tftp_root: "/tmp".into(),
            bios_filename: "undionly.kpxe".into(),
            uefi_filename: "ipxe.efi".into(),
            http_script_url: "http://192.168.1.10/boot.ipxe".into(),
        }
    }

    fn cfg_with_clients(img_path: &str, game_path: &str, wb_path: &str) -> Config {
        Config {
            disks: vec![
                Disk {
                    id: "img".into(),
                    disk_type: DiskType::Image,
                    backing: img_path.into(),
                    mode: None,
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
                Disk {
                    id: "game1".into(),
                    disk_type: DiskType::Game,
                    backing: game_path.into(),
                    mode: None,
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
                Disk {
                    id: "wb".into(),
                    disk_type: DiskType::Writeback,
                    backing: wb_path.into(),
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
            boot: Some(test_boot()),
        }
    }

    fn make_raw(path: &std::path::Path, size: usize) {
        std::fs::write(path, vec![0u8; size]).unwrap();
    }

    #[test]
    fn resolve_known_mac() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        let game = tmp.path().join("game.raw");
        make_raw(&img, 4096);
        make_raw(&game, 4096);

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &game.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        let c = mgr.resolve("aa:bb:cc:dd:ee:01").unwrap();
        assert_eq!(c.name.as_deref(), Some("pc-01"));
        assert_eq!(c.system_disk_id, "img");
        assert_eq!(c.game_disk_ids, vec!["game1"]);
    }

    #[test]
    fn resolve_case_insensitive_mac() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        make_raw(&img, 4096);

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        assert!(mgr.resolve("AA:BB:CC:DD:EE:01").is_some());
    }

    #[test]
    fn resolve_unknown_mac_without_defaults_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        make_raw(&img, 4096);

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        assert!(mgr.resolve("ff:ff:ff:ff:ff:ff").is_none());
    }

    #[test]
    fn resolve_unknown_mac_falls_back_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        make_raw(&img, 4096);

        let mut cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        cfg.client_defaults = Some(ClientDefaults {
            system: "img".into(),
            games: vec!["game1".into()],
            writeback: "wb".into(),
        });

        let mgr = ClientManager::new(&cfg).unwrap();
        let c = mgr.resolve("ff:ff:ff:ff:ff:ff").unwrap();
        assert_eq!(c.system_disk_id, "img");
    }

    #[test]
    fn get_store_returns_cached_backing() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        std::fs::write(&img, [0xABu8; 4096]).unwrap();

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        let store = mgr.get_store("img").unwrap();
        assert_eq!(store.size_bytes(), 4096);
        let mut buf = [0u8; 4];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0xAB, 0xAB, 0xAB, 0xAB]);
    }

    #[test]
    fn get_store_unknown_id_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        make_raw(&img, 4096);

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        assert!(mgr.get_store("nonexistent").is_none());
    }

    #[test]
    fn arcstore_delegates_to_inner() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("arc.raw");
        std::fs::write(&img, [0xCDu8; 512]).unwrap();

        let store: Arc<dyn BackingStore> = Arc::from(crate::storage::open_backing(&img).unwrap());
        let wrapper = ArcStore(store);
        assert_eq!(wrapper.size_bytes(), 512);
        let mut buf = [0u8; 2];
        wrapper.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0xCD, 0xCD]);
    }
}
