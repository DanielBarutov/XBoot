#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    // open_backing takes a path; write the input to a temp file first.
    if let Ok(mut f) = tempfile::NamedTempFile::new() {
        if f.write_all(data).is_ok() {
            let _ = xboot_core::storage::open_backing(f.path());
        }
    }
});
