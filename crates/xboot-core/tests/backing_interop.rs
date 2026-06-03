//! Interop: read images produced by `qemu-img` and compare against the source
//! bytes. Skipped automatically if `qemu-img` is not on PATH.

use std::process::Command;

fn qemu_img_available() -> bool {
    Command::new("qemu-img")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a raw source file of `size` bytes with deterministic content.
fn make_source(dir: &std::path::Path, size: usize) -> (std::path::PathBuf, Vec<u8>) {
    let mut data = vec![0u8; size];
    let mut x: u64 = 0x1234_5678;
    for b in data.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xFF) as u8;
    }
    let src = dir.join("src.raw");
    std::fs::write(&src, &data).unwrap();
    (src, data)
}

fn convert(src: &std::path::Path, out: &std::path::Path, fmt: &str, extra: &[&str]) {
    let status = Command::new("qemu-img")
        .args(["convert", "-f", "raw", "-O", fmt])
        .args(extra)
        .arg(src)
        .arg(out)
        .status()
        .unwrap();
    assert!(status.success(), "qemu-img convert to {fmt} failed");
}

fn assert_reads_match(image: &std::path::Path, expected: &[u8]) {
    let store = xboot_core::storage::open_backing(image).unwrap();
    assert_eq!(store.size_bytes(), expected.len() as u64);
    let mut buf = vec![0u8; expected.len()];
    store.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, expected, "image bytes differ from source");
}

#[test]
fn qemu_dynamic_vhd_matches_source() {
    if !qemu_img_available() {
        eprintln!("skipping: qemu-img not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (src, data) = make_source(dir.path(), 8 * 1024 * 1024);
    let out = dir.path().join("dyn.vhd");
    // subformat=dynamic, force_size keeps the virtual size exact.
    convert(&src, &out, "vpc", &["-o", "subformat=dynamic,force_size=on"]);
    assert_reads_match(&out, &data);
}

#[test]
fn qemu_fixed_vhd_matches_source() {
    if !qemu_img_available() {
        eprintln!("skipping: qemu-img not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (src, data) = make_source(dir.path(), 4 * 1024 * 1024);
    let out = dir.path().join("fixed.vhd");
    convert(&src, &out, "vpc", &["-o", "subformat=fixed,force_size=on"]);
    assert_reads_match(&out, &data);
}

#[test]
fn qemu_vhdx_matches_source() {
    if !qemu_img_available() {
        eprintln!("skipping: qemu-img not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (src, data) = make_source(dir.path(), 8 * 1024 * 1024);
    let out = dir.path().join("img.vhdx");
    convert(&src, &out, "vhdx", &["-o", "subformat=dynamic"]);
    assert_reads_match(&out, &data);
}
