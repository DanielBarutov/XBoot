//! Thin async I/O layer: two UDP listeners (`:67` proxy OFFER, `:4011` PXE Boot
//! Server ACK) sharing the pure decision/codec core. Survives any single bad
//! datagram and keeps serving.

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
    /// `:67` — answers DISCOVER with a proxy OFFER.
    Proxy,
    /// `:4011` — answers REQUEST with a boot-service ACK.
    BootService,
}

/// Start the proxyDHCP service: bind both listeners and serve until one errors
/// or the cancellation token fires.
pub async fn serve(cfg: BootConfig, token: CancellationToken) -> std::io::Result<()> {
    let cfg = Arc::new(cfg);
    let proxy = bind_socket(cfg.bind, 67).await?;
    let bootsvc = bind_socket(cfg.bind, 4011).await?;
    tokio::select! {
        r = listener_loop(proxy, cfg.clone(), Role::Proxy, token.child_token()) => r,
        r = listener_loop(bootsvc, cfg.clone(), Role::BootService, token.child_token()) => r,
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

/// Receive → handle → send, forever (or until the token fires).
/// A bad datagram never breaks the loop.
pub(crate) async fn listener_loop(
    sock: UdpSocket,
    cfg: Arc<BootConfig>,
    role: Role,
    token: CancellationToken,
) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            result = sock.recv_from(&mut buf) => {
                let (n, from) = result?;
                if let Some((reply, dest)) = handle_datagram(&buf[..n], &cfg, role, from) {
                    sock.send_to(&packet::encode(&reply), dest).await?;
                }
            }
            _ = token.cancelled() => {
                tracing::info!("dhcp listener ({role:?}): shutting down");
                return Ok(());
            }
        }
    }
}

/// Pure request→reply decision (no sockets). `None` = drop silently.
fn handle_datagram(
    data: &[u8],
    cfg: &BootConfig,
    role: Role,
    from: SocketAddr,
) -> Option<(DhcpMessage, SocketAddr)> {
    let req = match packet::parse(data) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "dhcp: dropping malformed datagram ({} bytes): {e}",
                data.len()
            );
            return None;
        }
    };
    if !is_pxe_request(&req, role) {
        return None;
    }
    let plan = decide::plan(&req, cfg)?;
    let (mtype, vendor) = match role {
        Role::Proxy => (
            msg_type::OFFER,
            options::pxe_offer_vendor_opts(cfg.server_ip),
        ),
        Role::BootService => (msg_type::ACK, options::pxe_ack_vendor_opts()),
    };
    let reply = build_reply(&req, &plan, cfg, mtype, vendor, role);
    Some((reply, reply_dest(&req, role, from)))
}

/// Guard: only DISCOVER (proxy) / REQUEST (boot service) carrying a PXEClient
/// vendor class. Everything else is left to the network's real DHCP server.
fn is_pxe_request(req: &DhcpMessage, role: Role) -> bool {
    let want = match role {
        Role::Proxy => msg_type::DISCOVER,
        Role::BootService => msg_type::REQUEST,
    };
    if req.message_type() != Some(want) {
        return false;
    }
    matches!(req.vendor_class(), Some(v) if v.starts_with(b"PXEClient"))
}

/// Assemble a proxy reply: `yiaddr = 0` always; echo xid/chaddr/flags; carry the
/// PXE options the firmware needs to accept the offer.
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
    // Server ID only for BootService (port 4011), not for proxy (port 67)
    if role == Role::BootService {
        opts.push(DhcpOption {
            code: options::SERVER_ID,
            data: cfg.server_ip.octets().to_vec(),
        });
    }
    opts.push(DhcpOption {
        code: options::VENDOR_CLASS_ID,
        data: b"PXEClient".to_vec(),
    });

    // Always include boot file and TFTP server for PXE clients
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

    opts.push(DhcpOption {
        code: options::VENDOR_SPECIFIC,
        data: vendor43,
    });

    // Add standard DHCP options for full DHCP server (not just proxy)
    opts.push(DhcpOption {
        code: 1, // Subnet Mask
        data: Ipv4Addr::new(255, 255, 255, 0).octets().to_vec(),
    });
    opts.push(DhcpOption {
        code: 3, // Default Gateway (router IP)
        data: cfg.server_ip.octets().to_vec(),
    });
    opts.push(DhcpOption {
        code: 6, // Domain Name Servers
        data: cfg.server_ip.octets().to_vec(),
    });
    opts.push(DhcpOption {
        code: 51, // IP Address Lease Time (3600 seconds = 1 hour)
        data: 3600u32.to_be_bytes().to_vec(),
    });

    DhcpMessage {
        op: BOOTREPLY,
        htype: req.htype,
        hlen: req.hlen,
        hops: 0,
        xid: req.xid,
        secs: 0,
        flags: req.flags,
        ciaddr: Ipv4Addr::UNSPECIFIED,
        yiaddr: plan.yiaddr,
        siaddr: plan.next_server.unwrap_or(Ipv4Addr::UNSPECIFIED),
        giaddr: req.giaddr,
        chaddr: req.chaddr,
        options: opts,
    }
}

/// Where to send the reply: relay agent (giaddr) → unicast to giaddr:67;
/// broadcast-flagged proxy offer → 255.255.255.255:68; otherwise unicast back
/// to the sender (covers the `:4011` path and loopback tests).
fn reply_dest(req: &DhcpMessage, role: Role, from: SocketAddr) -> SocketAddr {
    if req.giaddr != Ipv4Addr::UNSPECIFIED {
        SocketAddr::new(IpAddr::V4(req.giaddr), 67)
    } else if role == Role::Proxy && (req.flags & BROADCAST_FLAG) != 0 {
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
    use crate::net::dhcp::packet::{self, DhcpMessage, DhcpOption, BOOTREPLY};
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UdpSocket;

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

    /// Build a DISCOVER from a client. `flags = 0` so the reply is unicast back
    /// to our ephemeral source port (makes loopback testing possible).
    fn discover(arch: Option<[u8; 2]>, ipxe: bool) -> Vec<u8> {
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
        if ipxe {
            options.push(DhcpOption {
                code: options::USER_CLASS,
                data: b"iPXE".to_vec(),
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
    fn build_reply_is_a_proxy_offer() {
        let cfg = test_cfg();
        let req = packet::parse(&discover(Some([0x00, 0x00]), false)).unwrap();
        let plan = crate::net::dhcp::decide::plan(&req, &cfg).unwrap();
        let reply = build_reply(
            &req,
            &plan,
            &cfg,
            msg_type::OFFER,
            options::pxe_offer_vendor_opts(cfg.server_ip),
            Role::Proxy,
        );
        assert_eq!(reply.op, BOOTREPLY);
        assert_ne!(reply.yiaddr, Ipv4Addr::UNSPECIFIED); // assign an IP
        assert_eq!(reply.xid, 0x1234); // echoed
        assert_eq!(reply.siaddr, Ipv4Addr::new(192, 168, 1, 10));
        assert_eq!(reply.message_type(), Some(msg_type::OFFER));
        assert_eq!(reply.option(options::SERVER_ID), None); // no Server ID in proxy OFFER
        // Check standard DHCP options
        assert_eq!(reply.option(1), Some([255, 255, 255, 0].as_ref())); // Subnet Mask
        assert_eq!(reply.option(3), Some([192, 168, 1, 10].as_ref())); // Default Gateway
        // Bootfile and TFTP server ARE included (no Server ID in proxy OFFER tells client it's proxyDHCP)
        assert_eq!(
            reply.option(options::BOOTFILE_NAME),
            Some(b"undionly.kpxe".as_ref())
        );
        assert_eq!(
            reply.option(options::VENDOR_CLASS_ID),
            Some(b"PXEClient".as_ref())
        );
        assert_eq!(
            reply.option(options::TFTP_SERVER_NAME),
            Some(b"192.168.1.10".as_ref())
        );
        assert!(reply.option(options::VENDOR_SPECIFIC).is_some());
    }

    async fn recv_reply(client: &UdpSocket) -> DhcpMessage {
        let mut buf = [0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
            .await
            .expect("reply within timeout")
            .expect("recv ok");
        packet::parse(&buf[..n]).unwrap()
    }

    #[tokio::test]
    async fn two_stage_chainload_over_loopback() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let cfg = Arc::new(test_cfg());
        tokio::spawn(async move {
            let _ = listener_loop(sock, cfg, Role::Proxy, CancellationToken::new()).await;
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Stage 1: firmware DISCOVER (BIOS) — bootfile included.
        client
            .send_to(&discover(Some([0x00, 0x00]), false), addr)
            .await
            .unwrap();
        let r1 = recv_reply(&client).await;
        assert_eq!(
            r1.option(options::BOOTFILE_NAME),
            Some(b"undionly.kpxe".as_ref())
        );
        assert_eq!(r1.siaddr, Ipv4Addr::new(192, 168, 1, 10)); // has next-server

        // Stage 2: iPXE re-DISCOVER — HTTP URL boot file.
        client
            .send_to(&discover(Some([0x00, 0x07]), true), addr)
            .await
            .unwrap();
        let r2 = recv_reply(&client).await;
        assert_eq!(
            r2.option(options::BOOTFILE_NAME),
            Some(b"http://192.168.1.10/boot.ipxe?mac=aa:bb:cc:dd:ee:01".as_ref())
        );
        assert_eq!(r2.siaddr, Ipv4Addr::UNSPECIFIED); // no next-server on the HTTP arm
    }

    #[tokio::test]
    async fn non_pxe_datagram_is_ignored() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let cfg = Arc::new(test_cfg());
        tokio::spawn(async move {
            let _ = listener_loop(sock, cfg, Role::Proxy, CancellationToken::new()).await;
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // A DISCOVER without the PXEClient vendor class → must be dropped silently.
        let mut msg = packet::parse(&discover(Some([0x00, 0x00]), false)).unwrap();
        msg.options.retain(|o| o.code != options::VENDOR_CLASS_ID);
        client.send_to(&packet::encode(&msg), addr).await.unwrap();

        let mut buf = [0u8; 2048];
        let got =
            tokio::time::timeout(Duration::from_millis(300), client.recv_from(&mut buf)).await;
        assert!(got.is_err(), "server must not reply to non-PXE traffic");
    }
}
