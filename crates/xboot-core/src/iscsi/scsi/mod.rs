//! SCSI command execution (phase 05b).
//!
//! Pure, synchronous bridge from a decoded `ScsiCommand` (CDB + LUN) to a
//! `ScsiOutcome` (status + data + sense), executed against a per-LUN `Volume`.
//! No wire framing, no sequence numbers, no Data-In chunking — that is 05c.

mod cdb;
mod sense;
