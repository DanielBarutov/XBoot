use std::io;

/// Abstraction over raw packet send/receive, hiding Windows vs Linux differences.
/// Implementations are added in phase 06.
pub trait NetIo: Send + Sync {
    /// Receive the next inbound packet into `buf`; returns the number of bytes read.
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Send one raw packet.
    fn send(&self, packet: &[u8]) -> io::Result<()>;
}
