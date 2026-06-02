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
    let path = dir.join("c.toml");
    std::fs::write(
        &path,
        r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
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
    )
    .unwrap();

    let output = Command::new(bin).arg(&path).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
}
