//! tokio TCP adapter — the only async code in the iSCSI stack. Accepts connections,
//! frames PDUs across reads, and drives a `Connection`.

use crate::iscsi::registry::TargetRegistry;
use crate::iscsi::session::{Connection, Stage};
use crate::iscsi::{decode, PduError, BHS_LEN};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Accept connections forever, one task per connection. Shuts down gracefully
/// when the `CancellationToken` is fired.
pub async fn serve(
    listener: TcpListener,
    registry: Arc<RwLock<TargetRegistry>>,
    token: CancellationToken,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (sock, _peer) = result?;
                let reg = registry.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, reg).await;
                });
            }
            _ = token.cancelled() => {
                tracing::info!("iscsi: shutting down");
                return Ok(());
            }
        }
    }
}

/// Drive one connection: read bytes, frame PDUs, hand each to the state machine,
/// write the responses, and stop when the session enters Closing.
async fn handle_conn(
    mut sock: TcpStream,
    registry: Arc<RwLock<TargetRegistry>>,
) -> std::io::Result<()> {
    let mut conn = Connection::new(registry);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];

    loop {
        // Drain every complete PDU currently in `buf`.
        loop {
            match decode(&buf) {
                Ok((req, used)) => {
                    let bhs = buf[..BHS_LEN].to_vec();
                    for out in conn.handle(req, &bhs) {
                        sock.write_all(&out.encode()).await?;
                    }
                    buf.drain(..used);
                    if conn.stage() == Stage::Closing {
                        sock.flush().await?;
                        return Ok(());
                    }
                }
                Err(PduError::ShortHeader) | Err(PduError::ShortData) => break,
                Err(_structural) => {
                    // Malformed framing: best effort close.
                    sock.flush().await?;
                    return Ok(());
                }
            }
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(()); // peer closed
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iscsi::scsi::{LogicalUnit, ScsiTarget};
    use crate::iscsi::testkit;
    use crate::storage::BackingStore;
    use crate::volume::{RamOverlay, Volume};
    use std::io;

    const IQN: &str = "iqn.2026-06.dev.xboot:client-01";

    struct MemStore(Vec<u8>);
    impl BackingStore for MemStore {
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let s = offset as usize;
            buf.copy_from_slice(&self.0[s..s + buf.len()]);
            Ok(())
        }
    }

    fn registry() -> Arc<RwLock<TargetRegistry>> {
        let vol = Volume::new(
            Box::new(MemStore(vec![0u8; 4096])),
            Box::new(RamOverlay::new()),
        );
        let mut reg = TargetRegistry::new();
        reg.insert(IQN, ScsiTarget::new(vec![Some(LogicalUnit::new(vol))]));
        Arc::new(RwLock::new(reg))
    }

    #[tokio::test]
    async fn loopback_login_write_read_logout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reg = registry();
        let token = CancellationToken::new();
        tokio::spawn(async move { serve(listener, reg, token).await });

        let mut sock = TcpStream::connect(addr).await.unwrap();

        // Login.
        sock.write_all(&testkit::login_pdu(true, 1, 1, &[("TargetName", IQN)]))
            .await
            .unwrap();
        let resp = read_one_pdu(&mut sock).await;
        assert_eq!(resp[0] & 0x3f, crate::iscsi::opcode::LOGIN_RESP);

        // WRITE block 2 = 0x99 (immediate).
        let payload = vec![0x99u8; 512];
        sock.write_all(&testkit::write10_immediate_pdu(50, 2, 1, &payload))
            .await
            .unwrap();
        let resp = read_one_pdu(&mut sock).await;
        assert_eq!(resp[0] & 0x3f, crate::iscsi::opcode::SCSI_RESP);
        assert_eq!(resp[3], 0x00); // GOOD

        // READ block 2 back.
        sock.write_all(&testkit::read10_pdu(51, 2, 1))
            .await
            .unwrap();
        let resp = read_one_pdu(&mut sock).await;
        assert_eq!(resp[0] & 0x3f, crate::iscsi::opcode::DATA_IN);
        assert_eq!(&resp[BHS_LEN..BHS_LEN + 512], &payload[..]);
    }

    fn multi_registry(n: u8) -> Arc<RwLock<TargetRegistry>> {
        let mut reg = TargetRegistry::new();
        for i in 0..n {
            let vol = Volume::new(
                Box::new(MemStore(vec![0u8; 4096])),
                Box::new(RamOverlay::new()),
            );
            reg.insert(
                format!("{IQN}-{i}"),
                ScsiTarget::new(vec![Some(LogicalUnit::new(vol))]),
            );
        }
        Arc::new(RwLock::new(reg))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_clients_are_isolated() {
        let n: u8 = 8;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let token = CancellationToken::new();
        tokio::spawn(serve(listener, multi_registry(n), token));

        let mut handles = Vec::new();
        for i in 0..n {
            handles.push(tokio::spawn(async move {
                let iqn = format!("{IQN}-{i}");
                let mut sock = TcpStream::connect(addr).await.unwrap();
                sock.write_all(&testkit::login_pdu(true, 1, 1, &[("TargetName", &iqn)]))
                    .await
                    .unwrap();
                let _ = read_one_pdu(&mut sock).await;

                let byte = 0x10 + i;
                let payload = vec![byte; 512];
                sock.write_all(&testkit::write10_immediate_pdu(1, 0, 1, &payload))
                    .await
                    .unwrap();
                let _ = read_one_pdu(&mut sock).await;

                sock.write_all(&testkit::read10_pdu(2, 0, 1)).await.unwrap();
                let resp = read_one_pdu(&mut sock).await;
                // Each client must read back exactly its own byte.
                assert!(
                    resp[BHS_LEN..BHS_LEN + 512].iter().all(|&b| b == byte),
                    "client {i} saw cross-talk"
                );
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    /// Read exactly one PDU (BHS + data + pad) off the socket.
    async fn read_one_pdu(sock: &mut TcpStream) -> Vec<u8> {
        let mut buf = vec![0u8; BHS_LEN];
        sock.read_exact(&mut buf).await.unwrap();
        let dlen = ((buf[5] as usize) << 16) | ((buf[6] as usize) << 8) | buf[7] as usize;
        let pad = (4 - dlen % 4) % 4;
        let mut rest = vec![0u8; dlen + pad];
        if !rest.is_empty() {
            sock.read_exact(&mut rest).await.unwrap();
        }
        buf.extend_from_slice(&rest);
        buf
    }
}
