//! iPXE boot script generator (phase 06c).
//! One pure function — input IQN + server IP, output script text.

/// Generate an iPXE script that sanboots from the given iSCSI target.
pub fn generate(iqn: &str, server_ip: &str) -> String {
    format!("#!ipxe\nset initiator-iqn {iqn}\nsanboot iscsi:{server_ip}::::{iqn}\n")
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
