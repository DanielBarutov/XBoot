#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // decode must never panic, regardless of input.
    let _ = xboot_core::iscsi::decode(data);
});
