#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The TFTP packet parser must never panic, regardless of input.
    let _ = xboot_core::net::tftp::packet::parse(data);
});
