use serde::{Deserialize, Deserializer};

/// A size in bytes, deserialized from strings like `"8GB"`, `"512MB"`, `"1024"`.
/// Unit suffixes are powers of 1024 (KB = 1024, MB = 1024², GB = 1024³, TB = 1024⁴).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum SizeParseError {
    #[error("invalid size value: '{0}'")]
    Invalid(String),
    #[error("size value overflows u64")]
    Overflow,
}

/// Parse a human-readable size string into bytes.
pub fn parse_size(s: &str) -> Result<u64, SizeParseError> {
    let trimmed = s.trim();
    let upper = trimmed.to_ascii_uppercase();

    let (num_part, mult): (&str, u64) = if let Some(n) = upper.strip_suffix("TB") {
        (n, 1024u64.pow(4))
    } else if let Some(n) = upper.strip_suffix("GB") {
        (n, 1024u64.pow(3))
    } else if let Some(n) = upper.strip_suffix("MB") {
        (n, 1024u64.pow(2))
    } else if let Some(n) = upper.strip_suffix("KB") {
        (n, 1024)
    } else if let Some(n) = upper.strip_suffix('B') {
        (n, 1)
    } else {
        (upper.as_str(), 1)
    };

    let value: u64 = num_part
        .trim()
        .parse()
        .map_err(|_| SizeParseError::Invalid(trimmed.to_string()))?;

    value.checked_mul(mult).ok_or(SizeParseError::Overflow)
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_size(&s)
            .map(ByteSize)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("8GB").unwrap(), 8 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("2tb").unwrap(), 2 * 1024u64.pow(4));
        assert_eq!(parse_size(" 4MB ").unwrap(), 4 * 1024 * 1024);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_size("abc").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("1.5GB").is_err());
    }
}
