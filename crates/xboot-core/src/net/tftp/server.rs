//! Tokio TFTP server: port 69 listener + per-transfer ephemeral sockets with
//! exponential-backoff retransmission (RFC 1350 §4).

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time;
use tokio_util::sync::CancellationToken;

use crate::net::tftp::packet::{self, Packet};

/// Default initial retransmission timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);
/// Maximum number of retransmission attempts.
const DEFAULT_MAX_RETRIES: u32 = 5;
/// Maximum blksize the server will agree to.
const DEFAULT_MAX_BLKSIZE: u16 = 1468;

/// A configured TFTP server.
#[derive(Clone)]
pub struct TftpServer {
    bind: IpAddr,
    tftp_root: PathBuf,
    transfer_timeout: Duration,
    max_retries: u32,
    max_blksize: u16,
}

impl TftpServer {
    /// Create a new TFTP server with default parameters.
    pub fn new(bind: IpAddr, tftp_root: PathBuf) -> Self {
        Self {
            bind,
            tftp_root,
            transfer_timeout: DEFAULT_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            max_blksize: DEFAULT_MAX_BLKSIZE,
        }
    }
}

/// Start the TFTP service: bind port 69 and serve until an error or cancellation.
pub async fn serve(cfg: TftpServer, token: CancellationToken) -> io::Result<()> {
    let sock = UdpSocket::bind(SocketAddr::new(cfg.bind, 69)).await?;
    let cfg = Arc::new(cfg);
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            result = sock.recv_from(&mut buf) => {
                let (n, client_addr) = result?;
                let datagram = buf[..n].to_vec();
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let _ = handle_request(datagram, client_addr, &cfg).await;
                });
            }
            _ = token.cancelled() => {
                tracing::info!("tftp: shutting down");
                return Ok(());
            }
        }
    }
}

/// Process one TFTP request (called from a spawned task).
async fn handle_request(
    datagram: Vec<u8>,
    client_addr: SocketAddr,
    cfg: &TftpServer,
) -> io::Result<()> {
    let pkt = match packet::parse(&datagram) {
        Ok(pkt) => pkt,
        Err(_) => return Ok(()), // silently ignore malformed packets
    };

    match pkt {
        Packet::Rrq {
            filename,
            mode: _,
            options,
        } => handle_rrq(filename, options, client_addr, cfg).await,
        Packet::Wrq { .. } => {
            // Server is read-only
            let ephemeral = UdpSocket::bind(SocketAddr::new(cfg.bind, 0)).await?;
            let err = packet::encode_error(packet::ERR_ACCESS_VIOLATION, "Write access denied");
            ephemeral.send_to(&err, client_addr).await?;
            Ok(())
        }
        _ => Ok(()), // ACK/ERROR on port 69 → ignore (shouldn't happen in normal flow)
    }
}

/// Handle a read request: validate path, read file, negotiate options, transfer data.
async fn handle_rrq(
    filename: String,
    options: Vec<(String, String)>,
    client_addr: SocketAddr,
    cfg: &TftpServer,
) -> io::Result<()> {
    // 1. Validate and resolve the path
    if !packet::is_safe_path(&filename) {
        return send_error_from(
            cfg.bind,
            client_addr,
            packet::ERR_ACCESS_VIOLATION,
            "Invalid path",
        )
        .await;
    }
    let file_path = cfg.tftp_root.join(&filename);
    // Canonicalize resolves symlinks and ".." that might have slipped through.
    // Then verify it's still inside tftp_root.
    let real_path = match file_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return send_error_from(
                cfg.bind,
                client_addr,
                packet::ERR_FILE_NOT_FOUND,
                "File not found",
            )
            .await;
        }
    };
    let real_root = match cfg.tftp_root.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return send_error_from(cfg.bind, client_addr, packet::ERR_UNDEFINED, "Server error")
                .await;
        }
    };
    if !real_path.starts_with(&real_root) {
        return send_error_from(
            cfg.bind,
            client_addr,
            packet::ERR_ACCESS_VIOLATION,
            "Path traversal denied",
        )
        .await;
    }

    // 2. Read file into memory
    let file_data = match tokio::fs::read(&real_path).await {
        Ok(d) => d,
        Err(_) => {
            return send_error_from(
                cfg.bind,
                client_addr,
                packet::ERR_FILE_NOT_FOUND,
                "File not found",
            )
            .await;
        }
    };

    // 3. Open ephemeral socket
    let sock = UdpSocket::bind(SocketAddr::new(cfg.bind, 0)).await?;

    // 4. Option negotiation
    let blksize = negotiate_blksize(&options, cfg.max_blksize);
    let has_options = !options.is_empty();

    if has_options {
        let mut oack_opts: Vec<(String, String)> = vec![];
        oack_opts.push(("blksize".into(), blksize.to_string()));
        // tsize: return file size if requested (value "0" means "send me the size")
        if options.iter().any(|(k, v)| k == "tsize" && v == "0") {
            oack_opts.push(("tsize".into(), file_data.len().to_string()));
        }
        let oack = packet::encode_oack(&oack_opts);
        sock.send_to(&oack, client_addr).await?;

        // Wait for ACK 0 (client accepts OACK)
        let mut buf = [0u8; 64];
        match time::timeout(cfg.transfer_timeout * 2, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _addr))) => {
                if !matches!(packet::parse(&buf[..n]), Ok(Packet::Ack { block: 0 })) {
                    // Client didn't ACK OACK; give up
                    return Ok(());
                }
            }
            _ => return Ok(()), // timeout or recv error → give up
        }
    }

    // 5. Transfer data
    transfer_blocks(&sock, client_addr, &file_data, blksize, cfg).await
}

/// Negotiate the block size: return the smaller of client's request and server's max.
fn negotiate_blksize(options: &[(String, String)], max_blksize: u16) -> u16 {
    for (key, value) in options {
        if key == "blksize" {
            if let Ok(v) = value.parse::<u16>() {
                if v >= 8 {
                    return v.min(max_blksize);
                }
            }
        }
    }
    512 // default
}

/// Send an ERROR packet from an ephemeral port bound to `bind`.
async fn send_error_from(
    bind: IpAddr,
    client: SocketAddr,
    code: u16,
    message: &str,
) -> io::Result<()> {
    let sock = UdpSocket::bind(SocketAddr::new(bind, 0)).await?;
    let pkt = packet::encode_error(code, message);
    sock.send_to(&pkt, client).await?;
    Ok(())
}

/// Transfer file data in blocks with retransmission.
async fn transfer_blocks(
    sock: &UdpSocket,
    client_addr: SocketAddr,
    data: &[u8],
    blksize: u16,
    cfg: &TftpServer,
) -> io::Result<()> {
    let block_size = blksize as usize;
    let mut current: u16 = 1; // block we are currently sending (1-based)
    let mut retries: u32 = 0;
    let mut timeout = cfg.transfer_timeout;

    loop {
        // Slice data for current block
        let start = (current as usize - 1) * block_size;
        let end = (start + block_size).min(data.len());
        let chunk = &data[start..end];

        // Build and send DATA packet
        let data_pkt = packet::encode_data(current, chunk);
        sock.send_to(&data_pkt, client_addr).await?;

        // If last block (chunk < block_size), just send it and wait for final ACK
        if chunk.len() < block_size {
            // Wait for the final ACK with timeout
            let mut buf = [0u8; 64];
            match time::timeout(timeout, sock.recv_from(&mut buf)).await {
                Ok(Ok((n, _addr))) => {
                    if matches!(packet::parse(&buf[..n]), Ok(Packet::Ack { block }) if block == current)
                    {
                        return Ok(());
                    }
                    // Wrong ACK — client may need re-send of last block
                    retries += 1;
                    if retries > cfg.max_retries {
                        return Ok(());
                    }
                    timeout *= 2;
                    continue;
                }
                _ => {
                    retries += 1;
                    if retries > cfg.max_retries {
                        return Ok(());
                    }
                    timeout *= 2;
                    continue;
                }
            }
        }

        // Wait for ACK or timeout
        let mut buf = [0u8; 64];
        match time::timeout(timeout, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _addr))) => match packet::parse(&buf[..n]) {
                Ok(Packet::Ack { block }) if block == current => {
                    // Correct ACK — advance to next block
                    current = current.wrapping_add(1);
                    retries = 0;
                    timeout = cfg.transfer_timeout; // reset timeout
                }
                Ok(Packet::Ack { block }) if block == current.wrapping_sub(1) => {
                    // Sorcerer's Apprentice: duplicate ACK for previous block.
                    // Re-send current DATA (the one the client missed).
                    retries += 1;
                    if retries > cfg.max_retries {
                        return Ok(());
                    }
                    timeout *= 2;
                    // loop back (sends same block again)
                }
                _ => {
                    // Unknown/wrong ACK or non-ACK packet — ignore and re-send
                    retries += 1;
                    if retries > cfg.max_retries {
                        return Ok(());
                    }
                    timeout *= 2;
                }
            },
            Ok(Err(_)) => {
                // recv error
                retries += 1;
                if retries > cfg.max_retries {
                    return Ok(());
                }
                timeout *= 2;
            }
            Err(_) => {
                // Timeout — re-send
                retries += 1;
                if retries > cfg.max_retries {
                    let err_pkt = packet::encode_error(packet::ERR_UNDEFINED, "Transfer timed out");
                    let _ = sock.send_to(&err_pkt, client_addr).await;
                    return Ok(());
                }
                timeout *= 2;
                // loop back (re-sends same block)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    async fn start_test_server(tftp_root: PathBuf) -> SocketAddr {
        let server = TftpServer::new(IpAddr::V4(Ipv4Addr::LOCALHOST), tftp_root);
        let sock = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let addr = sock.local_addr().unwrap();
        // Spawn the server on this random port (simulating :69)
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                let (n, client) = sock.recv_from(&mut buf).await.unwrap();
                let datagram = buf[..n].to_vec();
                let srv = server.clone();
                tokio::spawn(async move {
                    let _ = handle_request(datagram, client, &srv).await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn full_transfer_no_options() {
        let tmp = tempfile::tempdir().unwrap();
        let file_data = vec![0xAB; 1000]; // just under 2 blocks of 512
        tokio::fs::write(tmp.path().join("test.bin"), &file_data)
            .await
            .unwrap();

        let srv_addr = start_test_server(tmp.path().to_path_buf()).await;

        // Build RRQ
        let mut rrq = vec![0x00, 0x01];
        rrq.extend(b"test.bin\0");
        rrq.extend(b"octet\0");

        // Send RRQ
        let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        client.send_to(&rrq, srv_addr).await.unwrap();

        // Receive DATA block 1
        let mut buf = [0u8; 600];
        let (n, _peer) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[0..2], &[0x00, 0x03]); // DATA opcode
        assert_eq!(&buf[2..4], &[0x00, 0x01]); // block 1
        assert_eq!(&buf[4..n], &file_data[..512]);

        // ACK block 1
        let ack = [0x00, 0x04, 0x00, 0x01];
        client.send_to(&ack, _peer).await.unwrap();

        // Receive DATA block 2 (partial = last)
        let mut buf = [0u8; 600];
        let (n, _peer) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[0..2], &[0x00, 0x03]); // DATA
        assert_eq!(&buf[2..4], &[0x00, 0x02]); // block 2
        assert_eq!(&buf[4..n], &file_data[512..]); // remaining 488 bytes
        assert!(n - 4 < 512, "last block must be < 512 bytes");

        // ACK block 2
        let ack = [0x00, 0x04, 0x00, 0x02];
        client.send_to(&ack, _peer).await.unwrap();
    }

    #[tokio::test]
    async fn transfer_with_options_oack() {
        let tmp = tempfile::tempdir().unwrap();
        let file_data = vec![0xCD; 2000];
        tokio::fs::write(tmp.path().join("big.bin"), &file_data)
            .await
            .unwrap();

        let srv_addr = start_test_server(tmp.path().to_path_buf()).await;

        // Build RRQ with options
        let mut rrq = vec![0x00, 0x01];
        rrq.extend(b"big.bin\0");
        rrq.extend(b"octet\0");
        rrq.extend(b"blksize\0");
        rrq.extend(b"1468\0");
        rrq.extend(b"tsize\0");
        rrq.extend(b"0\0");

        let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        client.send_to(&rrq, srv_addr).await.unwrap();

        // Receive OACK
        let mut buf = [0u8; 256];
        let (n, peer) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[0..2], &[0x00, 0x06]); // OACK opcode
        let oack_str = String::from_utf8_lossy(&buf[2..n]);
        assert!(oack_str.contains("blksize"));
        assert!(oack_str.contains("tsize"));
        assert!(oack_str.contains("2000"));

        // ACK 0
        let ack0 = [0x00, 0x04, 0x00, 0x00];
        client.send_to(&ack0, peer).await.unwrap();

        // Receive DATA block 1 (1468 bytes)
        let mut buf = [0u8; 1500];
        let (n, _peer) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[0..2], &[0x00, 0x03]);
        assert_eq!(&buf[2..4], &[0x00, 0x01]);
        assert_eq!(&buf[4..n], &file_data[..1468]);

        // ACK 1
        let ack1 = [0x00, 0x04, 0x00, 0x01];
        client.send_to(&ack1, _peer).await.unwrap();

        // Receive DATA block 2 (532 bytes = last)
        let mut buf = [0u8; 1500];
        let (n, _peer) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[0..2], &[0x00, 0x03]);
        assert_eq!(&buf[2..4], &[0x00, 0x02]);
        assert!(n - 4 < 1468, "last block must be < blksize");
    }

    #[tokio::test]
    async fn file_not_found_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let srv_addr = start_test_server(tmp.path().to_path_buf()).await;

        let mut rrq = vec![0x00, 0x01];
        rrq.extend(b"nonexistent.bin\0");
        rrq.extend(b"octet\0");

        let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        client.send_to(&rrq, srv_addr).await.unwrap();

        let mut buf = [0u8; 256];
        let (_n, _peer) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[0..2], &[0x00, 0x05]); // ERROR opcode
        assert_eq!(&buf[2..4], &[0x00, 0x01]); // code 1 = file not found
    }

    #[tokio::test]
    async fn wrq_returns_access_violation() {
        let tmp = tempfile::tempdir().unwrap();
        let srv_addr = start_test_server(tmp.path().to_path_buf()).await;

        let mut wrq = vec![0x00, 0x02]; // WRQ
        wrq.extend(b"test.bin\0");
        wrq.extend(b"octet\0");

        let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        client.send_to(&wrq, srv_addr).await.unwrap();

        let mut buf = [0u8; 256];
        let (_n, _peer) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[0..2], &[0x00, 0x05]); // ERROR
        assert_eq!(&buf[2..4], &[0x00, 0x02]); // code 2 = access violation
    }

    #[tokio::test]
    async fn retransmits_on_lost_ack() {
        let tmp = tempfile::tempdir().unwrap();
        let file_data = vec![0xEF; 1500]; // 3 blocks of 512
        tokio::fs::write(tmp.path().join("retry.bin"), &file_data)
            .await
            .unwrap();

        let srv_addr = start_test_server(tmp.path().to_path_buf()).await;

        // Build RRQ
        let mut rrq = vec![0x00, 0x01];
        rrq.extend(b"retry.bin\0");
        rrq.extend(b"octet\0");

        let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        client.send_to(&rrq, srv_addr).await.unwrap();

        // Receive DATA block 1
        let mut buf = [0u8; 600];
        let (n, peer) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[2..4], &[0x00, 0x01]);

        // DON'T ACK — wait for retransmission
        // The server should re-send DATA block 1 after ~1 second
        let mut buf2 = [0u8; 600];
        let (n2, _peer2) = client.recv_from(&mut buf2).await.unwrap();
        // Should be the same DATA block 1
        assert_eq!(&buf2[0..4], &buf[0..4]);
        assert_eq!(&buf2[4..n2], &buf[4..n]);

        // Now ACK
        let ack1 = [0x00, 0x04, 0x00, 0x01];
        client.send_to(&ack1, peer).await.unwrap();

        // Continue: receive block 2
        let mut buf3 = [0u8; 600];
        let (_n3, _peer3) = client.recv_from(&mut buf3).await.unwrap();
        assert_eq!(&buf3[2..4], &[0x00, 0x02]);
    }

    #[tokio::test]
    async fn sorcerers_apprentice_duplicate_ack() {
        let tmp = tempfile::tempdir().unwrap();
        let file_data = vec![0x11; 1000]; // 2 blocks
        tokio::fs::write(tmp.path().join("sorc.bin"), &file_data)
            .await
            .unwrap();

        let srv_addr = start_test_server(tmp.path().to_path_buf()).await;

        let mut rrq = vec![0x00, 0x01];
        rrq.extend(b"sorc.bin\0");
        rrq.extend(b"octet\0");

        let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        client.send_to(&rrq, srv_addr).await.unwrap();

        // Receive DATA 1
        let mut buf = [0u8; 600];
        let (_n, peer) = client.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[2..4], &[0x00, 0x01]);

        // ACK 1
        let ack1 = [0x00, 0x04, 0x00, 0x01];
        client.send_to(&ack1, peer).await.unwrap();

        // Send a DUPLICATE ACK 1 (simulating client that didn't get DATA 2)
        client.send_to(&ack1, peer).await.unwrap();

        // Should still receive DATA 2 (server re-sends it due to Sorcerer's Apprentice)
        let mut buf2 = [0u8; 600];
        let (_n2, _peer2) = client.recv_from(&mut buf2).await.unwrap();
        assert_eq!(&buf2[2..4], &[0x00, 0x02]); // server didn't advance to 3
    }

    #[tokio::test]
    async fn concurrent_transfers_isolated() {
        let tmp = tempfile::tempdir().unwrap();
        let file_data = vec![0x42; 800];
        tokio::fs::write(tmp.path().join("conc.bin"), &file_data)
            .await
            .unwrap();

        let srv_addr = start_test_server(tmp.path().to_path_buf()).await;

        // Spawn 3 concurrent clients
        let mut handles = vec![];
        for _ in 0..3 {
            let file_data = file_data.clone();
            handles.push(tokio::spawn(async move {
                let mut rrq = vec![0x00, 0x01];
                rrq.extend(b"conc.bin\0");
                rrq.extend(b"octet\0");

                let client = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                    .await
                    .unwrap();
                client.send_to(&rrq, srv_addr).await.unwrap();

                let mut received = vec![];
                // Receive DATA 1
                let mut buf = [0u8; 600];
                let (n, p) = client.recv_from(&mut buf).await.unwrap();
                let peer = p;
                received.extend_from_slice(&buf[4..n]);
                // ACK 1
                let ack1 = [0x00, 0x04, 0x00, 0x01];
                client.send_to(&ack1, peer).await.unwrap();
                // Receive DATA 2
                let mut buf = [0u8; 600];
                let (n, _) = client.recv_from(&mut buf).await.unwrap();
                received.extend_from_slice(&buf[4..n]);
                // ACK 2
                let ack2 = [0x00, 0x04, 0x00, 0x02];
                client.send_to(&ack2, peer).await.unwrap();

                assert_eq!(received, file_data);
            }));
        }

        for h in handles {
            h.await.unwrap();
        }
    }
}
