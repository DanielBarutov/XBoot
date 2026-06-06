//! Thin async I/O layer: two UDP listeners (`:67` DHCP+proxyDHCP, `:4011` boot
//! server) sharing the pure decision/codec core. Survives any single bad datagram.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::config::BootConfig;
use crate::net::dhcp::decide::{self, OfferPlan};
use crate::net::dhcp::options::{self, msg_type};
use crate::net::dhcp::packet::{self, DhcpMessage, DhcpOption, BOOTREPLY};

/// BOOTP broadcast flag (high bit of the 16-bit flags field).
const BROADCAST_FLAG: u16 = 0x8000;

/// Which listener role a datagram arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `:67` — DHCP server (IP allocation)
    Dhcp,
    /// `:67` — proxyDHCP server (PXE info only)
    ProxyDhcp,
    /// `:4011` — PXE Boot Server (answers REQUEST with ACK)
    BootServer,
}

/// Start the DHCP services: bind port 67 (DHCP+proxyDHCP) and port 4011 (boot server).
pub async fn serve(cfg: BootConfig, token: CancellationToken) -> std::io::Result<()> {
    let cfg = Arc::new(cfg);
    let port67 = bind_socket(cfg.bind, 67).await?;
    let port4011 = bind_socket(cfg.bind, 4011).await?;
    tokio::select! {
        r = listener_loop(port67, cfg.clone(), true, token.child_token()) => r,
        r = listener_loop(port4011, cfg.clone(), false, token.child_token()) => r,
        _ = token.cancelled() => {
            tracing::info!("dhcp: shutting down");
            Ok(())
        }
    }
}

async fn bind_socket(bind: IpAddr, port: u16) -> std::io::Result<UdpSocket> {
    let sock = UdpSocket::bind(SocketAddr::new(bind, port)).await?;
    sock.set_broadcast(true)?;
    Ok(sock)
}

/// Receive → handle → send, forever (or until token fires).
pub(crate) async fn listener_loop(
    sock: UdpSocket,
    cfg: Arc<BootConfig>,
    is_port67: bool,
    token: CancellationToken,
) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            result = sock.recv_from(&mut buf) => {
                let (n, from) = result?;
                if let Some((reply, dest)) = handle_datagram(&buf[..n], &cfg, is_port67, from) {
                    sock.send_to(&packet::encode(&reply), dest).await?;
                }
            }
            _ = token.cancelled() => {
                tracing::info!("dhcp listener (port {}): shutting down", if is_port67 { 67 } else { 4011 });
                return Ok(());
            }
        }
    }
}

/// Pure request→reply decision (no sockets). `None` = drop silently.
fn handle_datagram(data: &[u8], cfg: &BootConfig, is_port67: bool, from: SocketAddr) -> Option<(DhcpMessage, SocketAddr)> {
    let req = match packet::parse(data) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("dhcp: dropping malformed datagram ({} bytes): {e}", data.len());
            return None;
        }
    };

    if is_port67 {
        // Port 67: handle DISCOVER and REQUEST for DHCP and proxyDHCP
        let is_pxe = matches!(req.vendor_class(), Some(v) if v.starts_with(b"PXEClient"));
        let msg_type_val = req.message_type();

        match msg_type_val {
            Some(msg_type::DISCOVER) => {
                // DISCOVER → OFFER
                let plan = decide::plan(&req, cfg)?;
                let vendor = options::pxe_offer_vendor_opts(cfg.server_ip);
                let role = if is_pxe { Role::ProxyDhcp } else { Role::Dhcp };
                let reply = build_reply(&req, &plan, cfg, msg_type::OFFER, vendor, role);
                let dest = reply_dest(&req, from);
                Some((reply, dest))
            }
            Some(msg_type::REQUEST) => {
                // REQUEST → ACK
                let plan = decide::plan(&req, cfg)?;
                let vendor = options::pxe_offer_vendor_opts(cfg.server_ip);
                let role = if is_pxe { Role::ProxyDhcp } else { Role::Dhcp };
                let reply = build_reply(&req, &plan, cfg, msg_type::ACK, vendor, role);
                let dest = reply_dest(&req, from);
                Some((reply, dest))
            }
            _ => None,
        }
    } else {
        // Port 4011: PXE Boot Server — only REQUEST
        if req.message_type() != Some(msg_type::REQUEST) {
            return None;
        }

        let plan = decide::plan(&req, cfg)?;
        let vendor = options::pxe_ack_vendor_opts();
        let reply = build_reply(&req, &plan, cfg, msg_type::ACK, vendor, Role::BootServer);

        Some((reply, SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 68)))
    }
}

/// Assemble DHCP or proxyDHCP reply.
pub(crate) fn build_reply(
    req: &DhcpMessage,
    plan: &OfferPlan,
    cfg: &BootConfig,
    mtype: u8,
    vendor43: Vec<u8>,
    role: Role,
) -> DhcpMessage {
    let mut opts = vec![
        DhcpOption {
            code: options::MSG_TYPE,
            data: vec![mtype],
        },
    ];

    // Server ID only for regular DHCP, not proxyDHCP
    if role == Role::Dhcp || role == Role::BootServer {
        opts.push(DhcpOption {
            code: options::SERVER_ID,
            data: cfg.server_ip.octets().to_vec(),
        });
    }

    opts.push(DhcpOption {
        code: options::VENDOR_CLASS_ID,
        data: b"PXEClient".to_vec(),
    });

    // Include boot file and TFTP only for proxyDHCP and BootServer roles
    if role == Role::ProxyDhcp || role == Role::BootServer {
        opts.push(DhcpOption {
            code: options::BOOTFILE_NAME,
            data: plan.bootfile.clone().into_bytes(),
        });
        if let Some(ns) = plan.next_server {
            opts.push(DhcpOption {
                code: options::TFTP_SERVER_NAME,
                data: ns.to_string().into_bytes(),
            });
        }
    }

    opts.push(DhcpOption {
        code: options::VENDOR_SPECIFIC,
        data: vendor43,
    });

    // Standard DHCP options for DHCP and proxyDHCP (not for BootServer)
    if role != Role::BootServer {
        opts.push(DhcpOption {
            code: 1, // Subnet Mask
            data: Ipv4Addr::new(255, 255, 255, 0).octets().to_vec(),
        });
        opts.push(DhcpOption {
            code: 3, // Default Gateway
            data: cfg.server_ip.octets().to_vec(),
        });
        opts.push(DhcpOption {
            code: 6, // Domain Name Servers
            data: cfg.server_ip.octets().to_vec(),
        });
        opts.push(DhcpOption {
            code: 51, // IP Address Lease Time (1 hour)
            data: 3600u32.to_be_bytes().to_vec(),
        });
    }

    DhcpMessage {
        op: BOOTREPLY,
        htype: req.htype,
        hlen: req.hlen,
        hops: 0,
        xid: req.xid,
        secs: 0,
        flags: req.flags,
        ciaddr: Ipv4Addr::UNSPECIFIED,
        yiaddr: match role {
            Role::BootServer => Ipv4Addr::UNSPECIFIED,  // No IP for BootServer
            _ => plan.yiaddr,                            // Always assign IP for DHCP/proxyDHCP
        },
        siaddr: plan.next_server.unwrap_or(Ipv4Addr::UNSPECIFIED),
        giaddr: req.giaddr,
        chaddr: req.chaddr,
        options: opts,
    }
}

/// Where to send reply.
fn reply_dest(req: &DhcpMessage, from: SocketAddr) -> SocketAddr {
    if req.giaddr != Ipv4Addr::UNSPECIFIED {
        SocketAddr::new(IpAddr::V4(req.giaddr), 67)
    } else if (req.flags & BROADCAST_FLAG) != 0 {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 68)
    } else {
        from
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BootConfig;
    use crate::net::dhcp::options::{self, msg_type};
    use crate::net::dhcp::packet::{self, DhcpMessage, DhcpOption};
    use std::net::{IpAddr, Ipv4Addr};

    fn test_cfg() -> BootConfig {
        BootConfig {
            server_ip: Ipv4Addr::new(192, 168, 1, 10),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            http_port: 80,
            iscsi_port: 3260,
            tftp_root: "/tmp".into(),
            bios_filename: "undionly.kpxe".to_string(),
            uefi_filename: "ipxe.efi".to_string(),
            http_script_url: "http://192.168.1.10/boot.ipxe".to_string(),
        }
    }

    fn discover(arch: Option<[u8; 2]>) -> Vec<u8> {
        let mut options = vec![
            DhcpOption {
                code: options::MSG_TYPE,
                data: vec![msg_type::DISCOVER],
            },
            DhcpOption {
                code: options::VENDOR_CLASS_ID,
                data: b"PXEClient".to_vec(),
            },
        ];
        if let Some(a) = arch {
            options.push(DhcpOption {
                code: options::CLIENT_SYSTEM_ARCH,
                data: a.to_vec(),
            });
        }
        packet::encode(&DhcpMessage {
            op: 1,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 0x1234,
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
        })
    }

    #[test]
    fn proxy_dhcp_has_ip_boot_info_no_server_id() {
        let cfg = test_cfg();
        let req = packet::parse(&discover(Some([0x00, 0x00]))).unwrap();
        let plan = crate::net::dhcp::decide::plan(&req, &cfg).unwrap();
        let reply = build_reply(
            &req,
            &plan,
            &cfg,
            msg_type::OFFER,
            options::pxe_offer_vendor_opts(cfg.server_ip),
            Role::ProxyDhcp,
        );
        assert_ne!(reply.yiaddr, Ipv4Addr::UNSPECIFIED); // proxyDHCP also assigns IP
        assert_eq!(reply.option(options::SERVER_ID), None); // no Server ID (identifies as proxyDHCP)
        assert_eq!(
            reply.option(options::BOOTFILE_NAME),
            Some(b"undionly.kpxe".as_ref())
        ); // has boot file
        // Should also have DHCP options
        assert_eq!(reply.option(1), Some([255, 255, 255, 0].as_ref())); // Subnet Mask
        assert_eq!(reply.option(3), Some([192, 168, 1, 10].as_ref())); // Gateway
    }

    #[test]
    fn dhcp_has_ip_and_gateway() {
        let cfg = test_cfg();
        let req = packet::parse(&discover(Some([0x00, 0x00]))).unwrap();
        let plan = crate::net::dhcp::decide::plan(&req, &cfg).unwrap();
        let reply = build_reply(
            &req,
            &plan,
            &cfg,
            msg_type::OFFER,
            options::pxe_offer_vendor_opts(cfg.server_ip),
            Role::Dhcp,
        );
        assert_ne!(reply.yiaddr, Ipv4Addr::UNSPECIFIED); // has IP
        assert_eq!(
            reply.option(options::SERVER_ID),
            Some([192, 168, 1, 10].as_ref())
        ); // has Server ID
        assert_eq!(reply.option(1), Some([255, 255, 255, 0].as_ref())); // Subnet Mask
        assert_eq!(reply.option(3), Some([192, 168, 1, 10].as_ref())); // Gateway
    }
}
