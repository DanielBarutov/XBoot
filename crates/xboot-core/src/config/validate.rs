/// True if `s` is a MAC address of the form `XX:XX:XX:XX:XX:XX` (hex, colon-separated).
pub fn is_valid_mac(s: &str) -> bool {
    let octets: Vec<&str> = s.split(':').collect();
    octets.len() == 6
        && octets
            .iter()
            .all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_mac() {
        assert!(is_valid_mac("AA:BB:CC:DD:EE:01"));
        assert!(is_valid_mac("aa:bb:cc:dd:ee:ff"));
        assert!(!is_valid_mac("AA:BB:CC:DD:EE"));
        assert!(!is_valid_mac("AABBCCDDEEFF"));
        assert!(!is_valid_mac("ZZ:BB:CC:DD:EE:01"));
        assert!(!is_valid_mac("AA:BBB:CC:DD:EE:01"));
    }
}
