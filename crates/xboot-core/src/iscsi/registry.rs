//! Maps an iSCSI TargetName (IQN) to the client's SCSI target.

use crate::iscsi::scsi::ScsiTarget;
use std::collections::HashMap;
use std::sync::Arc;

/// Shared, read-mostly map of TargetName -> the client's `ScsiTarget`. Built once
/// at startup (test/config helpers in 05c; real per-MAC binding is phase 07) and
/// shared across all connection tasks behind `Arc`.
#[derive(Default)]
pub struct TargetRegistry {
    targets: HashMap<String, Arc<ScsiTarget>>,
}

impl TargetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a target under its IQN.
    pub fn insert(&mut self, iqn: impl Into<String>, target: ScsiTarget) {
        self.targets.insert(iqn.into(), Arc::new(target));
    }

    /// Resolve a TargetName to its shared target, if registered.
    pub fn get(&self, iqn: &str) -> Option<Arc<ScsiTarget>> {
        self.targets.get(iqn).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iscsi::scsi::{LogicalUnit, ScsiTarget};
    use crate::storage::BackingStore;
    use crate::volume::{RamOverlay, Volume};
    use std::io;

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

    fn one_lun_target() -> ScsiTarget {
        let vol = Volume::new(Box::new(MemStore(vec![0u8; 4096])), Box::new(RamOverlay::new()));
        ScsiTarget::new(vec![Some(LogicalUnit::new(vol))])
    }

    #[test]
    fn lookup_hit_returns_the_target() {
        let mut reg = TargetRegistry::new();
        reg.insert("iqn.2026-06.dev.xboot:client-01", one_lun_target());
        assert!(reg.get("iqn.2026-06.dev.xboot:client-01").is_some());
    }

    #[test]
    fn lookup_miss_is_none() {
        let reg = TargetRegistry::new();
        assert!(reg.get("iqn.2026-06.dev.xboot:nope").is_none());
    }
}
