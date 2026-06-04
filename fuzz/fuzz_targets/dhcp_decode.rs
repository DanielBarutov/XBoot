#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The DHCP packet parser must never panic, regardless of input.
    let _ = xboot_core::net::dhcp::packet::parse(data);
});
