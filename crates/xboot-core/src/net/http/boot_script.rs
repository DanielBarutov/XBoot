//! iPXE boot script generator (phase 06c).
//! One pure function — input IQN + server IP, output script text.

/// Generate an iPXE script that sanboots from the given iSCSI target.
///
/// `keep-san 1` keeps the SAN drive registered in the iBFT on every exit path
/// of `sanboot`, so the Windows kernel can always find the boot target when it
/// takes over from the firmware. The image itself must still be patched for
/// boot-start NIC drivers — see `Patcher/README.md`.
pub fn generate(iqn: &str, server_ip: &str) -> String {
    format!("#!ipxe\nset initiator-iqn {iqn}\nset keep-san 1\nsanboot iscsi:{server_ip}::::{iqn}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_sanboot_script() {
        let script = generate("iqn.2026-06.dev.xboot:pc-01", "192.168.1.10");
        assert!(script.contains("#!ipxe"));
        assert!(script.contains("set initiator-iqn iqn.2026-06.dev.xboot:pc-01"));
        assert!(script.contains("sanboot iscsi:192.168.1.10::::iqn.2026-06.dev.xboot:pc-01"));
    }

    #[test]
    fn keeps_san_registered_for_windows_handoff() {
        let script = generate("iqn.2026-06.dev.xboot:pc-01", "192.168.1.10");
        let keep_san = script.lines().position(|l| l == "set keep-san 1");
        let sanboot = script.lines().position(|l| l.starts_with("sanboot "));
        assert!(keep_san.is_some(), "script must set keep-san");
        assert!(keep_san < sanboot, "keep-san must be set before sanboot");
    }

    #[test]
    fn generates_script_with_mac_iqn() {
        let script = generate("iqn.2026-06.dev.xboot:aa-bb-cc-dd-ee-01", "10.0.0.1");
        assert!(script.contains("set initiator-iqn iqn.2026-06.dev.xboot:aa-bb-cc-dd-ee-01"));
        assert!(
            script.contains("sanboot iscsi:10.0.0.1::::iqn.2026-06.dev.xboot:aa-bb-cc-dd-ee-01")
        );
    }

    #[test]
    fn output_ends_with_newline() {
        let script = generate("iqn.2026-06.dev.xboot:test", "1.2.3.4");
        assert!(script.ends_with('\n'));
    }
}
