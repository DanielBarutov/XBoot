#![no_main]
use libfuzzer_sys::fuzz_target;
use xboot_core::storage::vhd::footer::VhdFooter;

fuzz_target!(|data: &[u8]| {
    let _ = VhdFooter::parse(data);
});
