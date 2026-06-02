use std::collections::HashMap;

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
        Some(disk) if disk.disk_type != expected => {
            errors.push(ValidationError::WrongDiskType {
                id: disk.id.clone(),
                expected,
                actual: disk.disk_type,
                context: context.to_string(),
            })
        }
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
    check_ref(errors, by_id, system, DiskType::Image, &format!("{who}.system"));
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

/// Validate referential integrity, disk-type/role consistency, and MAC formats.
pub fn validate(cfg: &Config) -> Result<(), ValidationErrors> {
    let mut errors: Vec<ValidationError> = Vec::new();

    let mut by_id: HashMap<&str, &Disk> = HashMap::new();
    for disk in &cfg.disks {
        if by_id.insert(disk.id.as_str(), disk).is_some() {
            errors.push(ValidationError::DuplicateDiskId(disk.id.clone()));
        }
    }

    for c in &cfg.clients {
        if !is_valid_mac(&c.mac) {
            errors.push(ValidationError::InvalidMac(c.mac.clone()));
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
            ValidationError::WrongDiskType { expected: DiskType::Image, .. }
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
}
