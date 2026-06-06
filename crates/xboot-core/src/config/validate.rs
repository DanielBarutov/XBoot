use std::collections::{HashMap, HashSet};

use crate::config::{Config, Disk, DiskType};

/// True if `s` is a MAC address of the form `XX:XX:XX:XX:XX:XX` (hex, colon-separated).
pub fn is_valid_mac(s: &str) -> bool {
    let octets: Vec<&str> = s.split(':').collect();
    octets.len() == 6
        && octets
            .iter()
            .all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit()))
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("duplicate disk id: {0}")]
    DuplicateDiskId(String),
    #[error("'{reference}' (referenced by {context}) is not a defined disk")]
    UnknownDisk { reference: String, context: String },
    #[error("disk '{id}' is {actual:?} but {context} requires {expected:?}")]
    WrongDiskType {
        id: String,
        expected: DiskType,
        actual: DiskType,
        context: String,
    },
    #[error("invalid MAC address: {0}")]
    InvalidMac(String),
    #[error("duplicate client MAC address: {0}")]
    DuplicateMac(String),
    #[error("boot.http_port must not be zero")]
    HttpPortZero,
    #[error("boot.http_script_url must start with http:// or https://: {0}")]
    InvalidHttpUrl(String),
    #[error("boot.{field} must not be empty")]
    EmptyBootField { field: String },
    #[error("boot.tftp_root does not exist or is not a directory: {0}")]
    TftpRootNotDirectory(String),
    #[error("boot.{field} '{value}' resolves outside tftp_root")]
    BootFileOutsideRoot { field: String, value: String },
}

/// Collection of validation errors with a readable multi-line `Display`.
#[derive(Debug, PartialEq, Eq)]
pub struct ValidationErrors(pub Vec<ValidationError>);

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "  - {e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

fn check_ref(
    errors: &mut Vec<ValidationError>,
    by_id: &HashMap<&str, &Disk>,
    reference: &str,
    expected: DiskType,
    context: &str,
) {
    match by_id.get(reference) {
        None => errors.push(ValidationError::UnknownDisk {
            reference: reference.to_string(),
            context: context.to_string(),
        }),
        Some(disk) if disk.disk_type != expected => errors.push(ValidationError::WrongDiskType {
            id: disk.id.clone(),
            expected,
            actual: disk.disk_type,
            context: context.to_string(),
        }),
        Some(_) => {}
    }
}

fn check_profile(
    errors: &mut Vec<ValidationError>,
    by_id: &HashMap<&str, &Disk>,
    system: &str,
    games: &[String],
    writeback: &str,
    who: &str,
) {
    check_ref(
        errors,
        by_id,
        system,
        DiskType::Image,
        &format!("{who}.system"),
    );
    for g in games {
        check_ref(errors, by_id, g, DiskType::Game, &format!("{who}.games"));
    }
    check_ref(
        errors,
        by_id,
        writeback,
        DiskType::Writeback,
        &format!("{who}.writeback"),
    );
}

fn check_boot(errors: &mut Vec<ValidationError>, boot: &crate::config::BootConfig) {
    if boot.http_port == 0 {
        errors.push(ValidationError::HttpPortZero);
    }

    // Validate tftp_root is a directory
    if !boot.tftp_root.is_dir() {
        errors.push(ValidationError::TftpRootNotDirectory(
            boot.tftp_root.display().to_string(),
        ));
    }

    if boot.bios_filename.is_empty() {
        errors.push(ValidationError::EmptyBootField {
            field: "bios_filename".to_string(),
        });
    } else if !is_safe_filename(&boot.bios_filename) {
        errors.push(ValidationError::BootFileOutsideRoot {
            field: "bios_filename".to_string(),
            value: boot.bios_filename.clone(),
        });
    }

    if boot.uefi_filename.is_empty() {
        errors.push(ValidationError::EmptyBootField {
            field: "uefi_filename".to_string(),
        });
    } else if !is_safe_filename(&boot.uefi_filename) {
        errors.push(ValidationError::BootFileOutsideRoot {
            field: "uefi_filename".to_string(),
            value: boot.uefi_filename.clone(),
        });
    }

    let url = &boot.http_script_url;
    if url.is_empty() {
        errors.push(ValidationError::EmptyBootField {
            field: "http_script_url".to_string(),
        });
    } else if !(url.starts_with("http://") || url.starts_with("https://")) {
        errors.push(ValidationError::InvalidHttpUrl(url.clone()));
    }
}

/// Reject paths that try to escape tftp_root: absolute paths, `..` segments.
fn is_safe_filename(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // Reject absolute paths (Unix and Windows)
    if name.starts_with('/') || name.starts_with('\\') {
        return false;
    }
    // Reject Windows drive-letter paths (e.g. "C:foo")
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return false;
    }
    // Reject any path component that tries parent traversal
    for component in name.split(['/', '\\']) {
        if component == ".." || component == "." {
            return false;
        }
    }
    true
}

/// Validate referential integrity, disk-type/role consistency, and MAC formats.
pub fn validate(cfg: &Config) -> Result<(), ValidationErrors> {
    let mut errors: Vec<ValidationError> = Vec::new();

    let mut by_id: HashMap<&str, &Disk> = HashMap::new();
    for disk in &cfg.disks {
        if by_id.insert(disk.id.as_str(), disk).is_some() {
            errors.push(ValidationError::DuplicateDiskId(disk.id.clone()));
        }
    }

    let mut seen_macs: HashSet<&str> = HashSet::new();
    for c in &cfg.clients {
        if !is_valid_mac(&c.mac) {
            errors.push(ValidationError::InvalidMac(c.mac.clone()));
        } else if !seen_macs.insert(c.mac.as_str()) {
            errors.push(ValidationError::DuplicateMac(c.mac.clone()));
        }
        let who = format!("client '{}'", c.name.as_deref().unwrap_or(&c.mac));
        check_profile(&mut errors, &by_id, &c.system, &c.games, &c.writeback, &who);
    }

    if let Some(d) = &cfg.client_defaults {
        check_profile(
            &mut errors,
            &by_id,
            &d.system,
            &d.games,
            &d.writeback,
            "client_defaults",
        );
    }

    if let Some(boot) = &cfg.boot {
        check_boot(&mut errors, boot);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationErrors(errors))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> crate::config::Config {
        toml::from_str(s).unwrap()
    }

    const VALID: &str = r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
[[disk]]
id = "g1"
type = "game"
backing = "y"
ram_cache = "1GB"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
games = ["g1"]
writeback = "wb"
"#;

    #[test]
    fn validates_mac() {
        assert!(is_valid_mac("AA:BB:CC:DD:EE:01"));
        assert!(!is_valid_mac("AABBCCDDEEFF"));
        assert!(!is_valid_mac("ZZ:BB:CC:DD:EE:01"));
    }

    #[test]
    fn valid_config_passes() {
        assert!(validate(&parse(VALID)).is_ok());
    }

    #[test]
    fn detects_unknown_disk() {
        let cfg = parse(
            r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
writeback = "missing-wb"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::UnknownDisk { reference, .. } if reference == "missing-wb"
        )));
    }

    #[test]
    fn detects_wrong_disk_type() {
        let cfg = parse(
            r#"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "wb"
writeback = "wb"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::WrongDiskType {
                expected: DiskType::Image,
                ..
            }
        )));
    }

    #[test]
    fn detects_duplicate_disk_id() {
        let cfg = parse(
            r#"
[[disk]]
id = "a"
type = "image"
backing = "x"
ram_cache = "1GB"
[[disk]]
id = "a"
type = "game"
backing = "y"
ram_cache = "1GB"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.contains(&ValidationError::DuplicateDiskId("a".to_string())));
    }

    #[test]
    fn detects_invalid_mac() {
        let cfg = parse(
            r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "NOT-A-MAC"
system = "img"
writeback = "wb"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::InvalidMac(m) if m == "NOT-A-MAC")));
    }

    #[test]
    fn detects_duplicate_mac() {
        let cfg = parse(
            r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
writeback = "wb"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
writeback = "wb"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.contains(&ValidationError::DuplicateMac(
            "AA:BB:CC:DD:EE:01".to_string()
        )));
    }

    #[test]
    fn validates_client_defaults() {
        let cfg = parse(
            r#"
[client_defaults]
system = "ghost"
writeback = "ghost"
"#,
        );
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn accepts_valid_boot_section() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = parse(&format!(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "{}"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
            tmp.path().display()
        ));
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn rejects_non_http_boot_url() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = parse(&format!(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "{}"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "tftp://192.168.1.10/boot.ipxe"
"#,
            tmp.path().display()
        ));
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::InvalidHttpUrl(u) if u.starts_with("tftp://"))));
    }

    #[test]
    fn rejects_empty_uefi_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = parse(&format!(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "{}"
bios_filename   = "undionly.kpxe"
uefi_filename   = ""
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
            tmp.path().display()
        ));
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.iter().any(
            |e| matches!(e, ValidationError::EmptyBootField { field } if field == "uefi_filename")
        ));
    }

    #[test]
    fn rejects_empty_boot_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = parse(&format!(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "{}"
bios_filename   = ""
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
            tmp.path().display()
        ));
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.iter().any(
            |e| matches!(e, ValidationError::EmptyBootField { field } if field == "bios_filename")
        ));
    }

    #[test]
    fn rejects_zero_http_port() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = parse(&format!(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
http_port       = 0
tftp_root       = "{}"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
            tmp.path().display()
        ));
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::HttpPortZero)));
    }

    #[test]
    fn rejects_missing_tftp_root() {
        let cfg = parse(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "/nonexistent/path/that/is/not/a/dir"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::TftpRootNotDirectory(_))));
    }

    #[test]
    fn rejects_parent_traversal_bios_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = parse(&format!(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "{}"
bios_filename   = "../secret"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
            tmp.path().display()
        ));
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::BootFileOutsideRoot { field, .. } if field == "bios_filename"
        )));
    }

    #[test]
    fn rejects_absolute_bios_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = parse(&format!(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "{}"
bios_filename   = "/etc/passwd"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
            tmp.path().display()
        ));
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::BootFileOutsideRoot { field, .. } if field == "bios_filename"
        )));
    }

    #[test]
    fn accepts_valid_tftp_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = parse(&format!(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "{}"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
            tmp.path().display()
        ));
        assert!(validate(&cfg).is_ok());
    }
}
