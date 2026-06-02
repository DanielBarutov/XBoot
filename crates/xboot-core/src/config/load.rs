use std::path::Path;

use crate::config::{validate, Config, ValidationErrors};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config validation failed:\n{0}")]
    Validation(#[from] ValidationErrors),
}

/// Read, parse, and validate a config file. Returns a fully-checked [`Config`].
pub fn load_from_path(path: &Path) -> Result<Config, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let cfg: Config = toml::from_str(&text)?;
    validate(&cfg)?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
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
"#;

    #[test]
    fn loads_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, VALID).unwrap();
        let cfg = load_from_path(&path).unwrap();
        assert_eq!(cfg.disks.len(), 2);
        assert_eq!(cfg.clients.len(), 1);
    }

    #[test]
    fn missing_file_is_io_error() {
        let err = load_from_path(std::path::Path::new("/no/such/file.toml")).unwrap_err();
        assert!(matches!(err, LoadError::Io { .. }));
    }

    #[test]
    fn invalid_config_is_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(
            &path,
            r#"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "ghost"
writeback = "ghost"
"#,
        )
        .unwrap();
        let err = load_from_path(&path).unwrap_err();
        assert!(matches!(err, LoadError::Validation(_)));
    }

    #[test]
    fn malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.toml");
        std::fs::write(&path, "this is = = not toml").unwrap();
        let err = load_from_path(&path).unwrap_err();
        assert!(matches!(err, LoadError::Parse(_)));
    }
}
