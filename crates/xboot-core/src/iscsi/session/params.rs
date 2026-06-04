//! Operational parameters negotiated during login (RFC 7143 subset).
#![allow(dead_code)] // wired into Connection in Task 3

/// Operational parameters. We start at RFC defaults and fold in the initiator's
/// offered keys with the correct per-key rule (min for sizes, AND/OR for the
/// two boolean flow keys, declarative for MaxRecvDataSegmentLength).
#[derive(Debug, Clone, Copy)]
pub struct SessionParams {
    /// Most bytes the *initiator* will accept in one Data-In data segment.
    pub max_recv_data_segment_length: u32,
    /// Max unsolicited (immediate + first burst) write bytes per command.
    pub first_burst_length: u32,
    /// Max bytes per solicited (R2T) burst.
    pub max_burst_length: u32,
    /// May the initiator send immediate data with the command?
    pub immediate_data: bool,
    /// Must the target solicit *all* write data via R2T (no unsolicited burst)?
    pub initial_r2t: bool,
}

impl Default for SessionParams {
    fn default() -> Self {
        Self {
            max_recv_data_segment_length: 8192,
            first_burst_length: 65536,
            max_burst_length: 262144,
            immediate_data: true,
            initial_r2t: true,
        }
    }
}

impl SessionParams {
    /// Fold one negotiated `Key=Value` pair into the parameters. Unknown keys are
    /// ignored. Malformed numeric values are ignored (keep the current value).
    pub fn negotiate(&mut self, key: &str, value: &str) {
        match key {
            "MaxRecvDataSegmentLength" => {
                if let Ok(v) = value.parse() {
                    self.max_recv_data_segment_length = v; // declarative: take theirs
                }
            }
            "FirstBurstLength" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.first_burst_length = self.first_burst_length.min(v);
                }
            }
            "MaxBurstLength" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.max_burst_length = self.max_burst_length.min(v);
                }
            }
            "ImmediateData" => self.immediate_data &= value.eq_ignore_ascii_case("Yes"),
            "InitialR2T" => {
                self.initial_r2t = self.initial_r2t && value.eq_ignore_ascii_case("Yes")
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_rfc() {
        let p = SessionParams::default();
        assert_eq!(p.max_recv_data_segment_length, 8192);
        assert_eq!(p.first_burst_length, 65536);
        assert_eq!(p.max_burst_length, 262144);
        assert!(p.immediate_data);
        assert!(p.initial_r2t); // RFC default Yes until negotiated down
    }

    #[test]
    fn numeric_keys_take_the_minimum() {
        let mut p = SessionParams::default();
        // Initiator offers a smaller MaxBurstLength -> we take theirs.
        p.negotiate("MaxBurstLength", "16384");
        assert_eq!(p.max_burst_length, 16384);
        // Initiator offers a larger value than ours -> we keep ours (min).
        p.negotiate("FirstBurstLength", "1048576");
        assert_eq!(p.first_burst_length, 65536);
    }

    #[test]
    fn immediate_data_is_anded_initial_r2t_is_ored() {
        let mut p = SessionParams::default();
        p.negotiate("ImmediateData", "No"); // ours Yes AND theirs No -> No
        assert!(!p.immediate_data);
        let mut p2 = SessionParams::default();
        p2.negotiate("InitialR2T", "No"); // ours Yes OR theirs No -> No
        assert!(!p2.initial_r2t);
    }

    #[test]
    fn max_recv_is_declarative_per_direction() {
        let mut p = SessionParams::default();
        // The initiator declares the most it will accept; we remember it verbatim.
        p.negotiate("MaxRecvDataSegmentLength", "4096");
        assert_eq!(p.max_recv_data_segment_length, 4096);
    }

    #[test]
    fn unknown_key_is_ignored() {
        let mut p = SessionParams::default();
        p.negotiate("X-com.example-thing", "whatever"); // must not panic
        assert_eq!(p.max_burst_length, 262144);
    }
}
