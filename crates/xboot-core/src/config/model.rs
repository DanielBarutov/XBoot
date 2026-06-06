use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

use serde::Deserialize;

use crate::config::ByteSize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskType {
    Image,
    Game,
    Writeback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskMode {
    Readonly,
    Readwrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WritebackPolicy {
    Volatile,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Disk {
    pub id: String,
    #[serde(rename = "type")]
    pub disk_type: DiskType,
    pub backing: String,
    #[serde(default)]
    pub mode: Option<DiskMode>,
    pub ram_cache: ByteSize,
    #[serde(default)]
    pub policy: Option<WritebackPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Client {
    pub mac: String,
    #[serde(default)]
    pub name: Option<String>,
    pub system: String,
    #[serde(default)]
    pub games: Vec<String>,
    pub writeback: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClientDefaults {
    pub system: String,
    #[serde(default)]
    pub games: Vec<String>,
    pub writeback: String,
}

/// Optional `[boot]` section: enables the proxyDHCP / PXE network-boot service.
/// When absent, the DHCP server is not started.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BootConfig {
    /// Our IP — used for next-server (siaddr / option 66) and server-id (option 54).
    pub server_ip: Ipv4Addr,
    /// Listen address for the `:67` and `:4011` UDP sockets.
    pub bind: IpAddr,
    /// Directory from which TFTP files are served (phase 06b).
    pub tftp_root: PathBuf,
    /// TFTP file for legacy BIOS (arch 0x0000), relative to tftp_root.
    pub bios_filename: String,
    /// TFTP file for UEFI x64 (arch 0x0007/0x0009), served by 06b.
    pub uefi_filename: String,
    /// iPXE arm HTTP boot script; `?mac=...` is appended at runtime. Served by 06c.
    pub http_script_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    #[serde(rename = "disk", default)]
    pub disks: Vec<Disk>,
    #[serde(rename = "client", default)]
    pub clients: Vec<Client>,
    #[serde(default)]
    pub client_defaults: Option<ClientDefaults>,
    #[serde(default)]
    pub boot: Option<BootConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ByteSize;

    const SAMPLE: &str = r#"
[[disk]]
id = "win11-master"
type = "image"
backing = "D:/xboot/win11.vhdx"
mode = "readonly"
ram_cache = "8GB"

[[disk]]
id = "games-main"
type = "game"
backing = "E:/games.vhdx"
mode = "readonly"
ram_cache = "16GB"

[[disk]]
id = "wb-nvme"
type = "writeback"
backing = "F:/xboot/writeback/"
ram_cache = "4GB"
policy = "volatile"

[[client]]
mac = "AA:BB:CC:DD:EE:01"
name = "PC-01"
system = "win11-master"
games = ["games-main"]
writeback = "wb-nvme"

[client_defaults]
system = "win11-master"
games = ["games-main"]
writeback = "wb-nvme"
"#;

    #[test]
    fn parses_sample_config() {
        let cfg: Config = toml::from_str(SAMPLE).unwrap();

        assert_eq!(cfg.disks.len(), 3);
        assert_eq!(cfg.disks[0].id, "win11-master");
        assert_eq!(cfg.disks[0].disk_type, DiskType::Image);
        assert_eq!(cfg.disks[0].mode, Some(DiskMode::Readonly));
        assert_eq!(cfg.disks[0].ram_cache, ByteSize(8 * 1024 * 1024 * 1024));
        assert_eq!(cfg.disks[2].policy, Some(WritebackPolicy::Volatile));

        assert_eq!(cfg.clients.len(), 1);
        assert_eq!(cfg.clients[0].games, vec!["games-main".to_string()]);
        assert_eq!(cfg.clients[0].name.as_deref(), Some("PC-01"));

        assert!(cfg.client_defaults.is_some());
    }

    #[test]
    fn parses_boot_section() {
        let cfg: Config = toml::from_str(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "/srv/xboot/tftp"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
        )
        .unwrap();
        let boot = cfg.boot.expect("boot section present");
        assert_eq!(boot.server_ip, Ipv4Addr::new(192, 168, 1, 10));
        assert_eq!(boot.bind, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(boot.tftp_root, PathBuf::from("/srv/xboot/tftp"));
        assert_eq!(boot.bios_filename, "undionly.kpxe");
        assert_eq!(boot.uefi_filename, "ipxe.efi");
        assert_eq!(boot.http_script_url, "http://192.168.1.10/boot.ipxe");
    }

    #[test]
    fn parses_boot_section_windows_tftp_root() {
        let cfg: Config = toml::from_str(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "D:\\xboot\\tftp"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
        )
        .unwrap();
        let boot = cfg.boot.unwrap();
        assert_eq!(boot.tftp_root, PathBuf::from("D:\\xboot\\tftp"));
    }

    #[test]
    fn boot_section_is_optional() {
        let cfg: Config = toml::from_str(
            r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
"#,
        )
        .unwrap();
        assert!(cfg.boot.is_none());
    }

    #[test]
    fn games_default_to_empty() {
        let cfg: Config = toml::from_str(
            r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"

[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
writeback = "img"
"#,
        )
        .unwrap();
        assert!(cfg.clients[0].games.is_empty());
    }
}
