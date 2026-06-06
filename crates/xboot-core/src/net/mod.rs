pub mod dhcp;
pub mod tftp;

use std::io;

/// Abstraction over raw packet send/receive, hiding Windows vs Linux differences.
/// Implementations are added in phase 06.
pub trait NetIo: Send + Sync {
    /// Receive the next inbound packet into `buf`; returns the number of bytes read.
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Send one raw packet.
    fn send(&self, packet: &[u8]) -> io::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Loopback;

    impl NetIo for Loopback {
        fn recv(&self, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn send(&self, _packet: &[u8]) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn trait_is_object_safe_and_usable() {
        let io: Box<dyn NetIo> = Box::new(Loopback);
        let mut buf = [0u8; 4];
        assert_eq!(io.recv(&mut buf).unwrap(), 0);
        io.send(&[1, 2, 3]).unwrap();
    }
}
