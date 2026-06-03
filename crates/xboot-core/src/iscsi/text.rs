//! iSCSI text segments: `Key=Value\0Key=Value\0...`.

/// Parse a text segment into key/value pairs. Splitting is on the *first* `=`.
/// Malformed entries (no `=`, empty key) and empty chunks are skipped. Never
/// panics; non-UTF-8 bytes are replaced (lossy).
pub fn parse_pairs(data: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in data.split(|&b| b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let Some(eq) = chunk.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = &chunk[..eq];
        if key.is_empty() {
            continue;
        }
        let val = &chunk[eq + 1..];
        out.push((
            String::from_utf8_lossy(key).into_owned(),
            String::from_utf8_lossy(val).into_owned(),
        ));
    }
    out
}

/// Encode pairs into a text segment: each `Key=Value` followed by a NUL.
pub fn encode_pairs(pairs: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in pairs {
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
        out.push(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nul_separated_pairs() {
        let data = b"AuthMethod=None\0MaxRecvDataSegmentLength=8192\0";
        let pairs = parse_pairs(data);
        assert_eq!(
            pairs,
            vec![
                ("AuthMethod".to_string(), "None".to_string()),
                ("MaxRecvDataSegmentLength".to_string(), "8192".to_string()),
            ]
        );
    }

    #[test]
    fn value_may_contain_equals_sign() {
        // Only the first '=' separates key from value.
        let pairs = parse_pairs(b"TargetAddress=10.0.0.1:3260,1\0");
        assert_eq!(pairs, vec![("TargetAddress".into(), "10.0.0.1:3260,1".into())]);
    }

    #[test]
    fn skips_malformed_entries() {
        // no '=' -> skipped; empty key -> skipped; trailing empty chunk -> skipped
        let pairs = parse_pairs(b"novalue\0=onlyvalue\0Good=1\0");
        assert_eq!(pairs, vec![("Good".into(), "1".into())]);
    }

    #[test]
    fn encode_appends_nul_after_each_pair() {
        let out = encode_pairs(&[("A".into(), "1".into()), ("B".into(), "2".into())]);
        assert_eq!(out, b"A=1\0B=2\0");
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn pairs_round_trip(
            // keys: non-empty, no '=' and no NUL; values: no NUL.
            pairs in proptest::collection::vec(
                ("[A-Za-z][A-Za-z0-9]{0,15}", "[ -<>-~]{0,32}"),
                0..16,
            )
        ) {
            let pairs: Vec<(String, String)> = pairs.into_iter().collect();
            let encoded = encode_pairs(&pairs);
            let decoded = parse_pairs(&encoded);
            prop_assert_eq!(decoded, pairs);
        }
    }
}
