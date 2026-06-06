use std::process::Command;

#[test]
fn exits_2_without_arguments() {
    let bin = env!("CARGO_BIN_EXE_xboot");
    let output = Command::new(bin).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn loads_a_valid_config_and_exits_0() {
    let bin = env!("CARGO_BIN_EXE_xboot");
    let dir = std::env::temp_dir().join(format!("xboot-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // ClientManager::new() opens Image/Game backing stores, so the file must exist.
    let img_path = dir.join("img.raw");
    std::fs::write(&img_path, vec![0u8; 1024]).unwrap();

    let path = dir.join("c.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "/tmp"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"

[[disk]]
id = "img"
type = "image"
backing = "{img_path}"
ram_cache = "1GB"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
writeback = "wb"
"#,
            img_path = img_path.display(),
        ),
    )
    .unwrap();

    let output = Command::new(bin).arg(&path).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn missing_config_exits_with_error() {
    let bin = env!("CARGO_BIN_EXE_xboot");
    let output = Command::new(bin)
        .arg("/no/such/xboot/config.toml")
        .output()
        .unwrap();
    // Error path: non-zero, and distinct from the usage exit code (2).
    assert_eq!(output.status.code(), Some(1));
}
