//! Pure decision core: given a parsed request + boot config, decide what to
//! offer. No sockets, fully unit-testable. Returns `None` to stay silent.

use std::net::Ipv4Addr;

use crate::config::BootConfig;
use crate::net::dhcp::packet::DhcpMessage;

/// Client system architecture (option 93) codes we support.
pub const ARCH_BIOS_X86: u16 = 0x0000;
pub const ARCH_UEFI_X64: u16 = 0x0007;
pub const ARCH_UEFI_X64_ALT: u16 = 0x0009;

/// What to put in the reply: the bootfile (option 67) and, on the firmware arm,
/// the next-server IP (siaddr / option 66). `None` next-server = iPXE HTTP arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferPlan {
    pub bootfile: String,
    pub next_server: Option<Ipv4Addr>,
}

/// Decide the boot plan for a request. `None` means "stay silent".
pub fn plan(msg: &DhcpMessage, cfg: &BootConfig) -> Option<OfferPlan> {
    // iPXE arm — option 77 user class contains "iPXE". Breaks the chainload loop
    // by handing iPXE the HTTP script instead of the iPXE binary again.
    if is_ipxe(msg) {
        let mac = format_mac(&msg.chaddr[..(msg.hlen as usize).min(16)]);
        return Some(OfferPlan {
            bootfile: format!("{}?mac={}", cfg.http_script_url, mac),
            next_server: None,
        });
    }

    // Firmware arm — branch on architecture (option 93).
    match msg.client_arch()? {
        ARCH_BIOS_X86 => Some(OfferPlan {
            bootfile: cfg.bios_filename.clone(),
            next_server: Some(cfg.server_ip),
        }),
        ARCH_UEFI_X64 | ARCH_UEFI_X64_ALT => Some(OfferPlan {
            bootfile: cfg.uefi_filename.clone(),
            next_server: Some(cfg.server_ip),
        }),
        _ => None,
    }
}

/// True if the user-class option carries "iPXE" (plain or RFC-3004 length-prefixed).
fn is_ipxe(msg: &DhcpMessage) -> bool {
    matches!(msg.user_class(), Some(uc) if uc.windows(4).any(|w| w == b"iPXE"))
}

/// Lower-case colon-separated MAC from the leading hardware-address bytes.
fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BootConfig;
    use crate::net::dhcp::options::{CLIENT_SYSTEM_ARCH, USER_CLASS};
    use crate::net::dhcp::packet::{DhcpMessage, DhcpOption};
    use std::net::{IpAddr, Ipv4Addr};

    fn cfg() -> BootConfig {
        BootConfig {
            server_ip: Ipv4Addr::new(192, 168, 1, 10),
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            tftp_root: "/tmp".into(),
            bios_filename: "undionly.kpxe".to_string(),
            uefi_filename: "ipxe.efi".to_string(),
            http_script_url: "http://192.168.1.10/boot.ipxe".to_string(),
        }
    }

    fn msg(options: Vec<DhcpOption>) -> DhcpMessage {
        DhcpMessage {
            op: 1,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 1,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: [
                0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
            options,
        }
    }

    #[test]
    fn bios_arch_gets_bios_filename() {
        let m = msg(vec![DhcpOption {
            code: CLIENT_SYSTEM_ARCH,
            data: vec![0x00, 0x00],
        }]);
        let plan = plan(&m, &cfg()).unwrap();
        assert_eq!(plan.bootfile, "undionly.kpxe");
        assert_eq!(plan.next_server, Some(Ipv4Addr::new(192, 168, 1, 10)));
    }

    #[test]
    fn uefi_arch_gets_uefi_filename() {
        for arch in [[0x00, 0x07], [0x00, 0x09]] {
            let m = msg(vec![DhcpOption {
                code: CLIENT_SYSTEM_ARCH,
                data: arch.to_vec(),
            }]);
            let plan = plan(&m, &cfg()).unwrap();
            assert_eq!(plan.bootfile, "ipxe.efi");
            assert_eq!(plan.next_server, Some(Ipv4Addr::new(192, 168, 1, 10)));
        }
    }

    #[test]
    fn ipxe_user_class_gets_http_url_with_mac() {
        let m = msg(vec![
            DhcpOption {
                code: USER_CLASS,
                data: b"iPXE".to_vec(),
            },
            DhcpOption {
                code: CLIENT_SYSTEM_ARCH,
                data: vec![0x00, 0x07],
            },
        ]);
        let plan = plan(&m, &cfg()).unwrap();
        assert_eq!(
            plan.bootfile,
            "http://192.168.1.10/boot.ipxe?mac=aa:bb:cc:dd:ee:01"
        );
        assert_eq!(plan.next_server, None);
    }

    #[test]
    fn unknown_arch_is_none() {
        let m = msg(vec![DhcpOption {
            code: CLIENT_SYSTEM_ARCH,
            data: vec![0x00, 0x42],
        }]);
        assert_eq!(plan(&m, &cfg()), None);
    }

    #[test]
    fn no_arch_no_ipxe_is_none() {
        let m = msg(vec![]);
        assert_eq!(plan(&m, &cfg()), None);
    }

    #[test]
    fn ipxe_rfc3004_length_prefixed_user_class() {
        // RFC 3004 encodes user-class as length-prefixed strings: \x04iPXE
        let m = msg(vec![
            DhcpOption {
                code: USER_CLASS,
                data: b"\x04iPXE".to_vec(),
            },
            DhcpOption {
                code: CLIENT_SYSTEM_ARCH,
                data: vec![0x00, 0x07],
            },
        ]);
        let plan = plan(&m, &cfg()).unwrap();
        assert!(plan.bootfile.starts_with("http://"));
        assert_eq!(plan.next_server, None);
    }
}
