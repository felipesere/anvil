//! End-to-end: the binary must explain itself when the config is missing.

use std::process::Command;

#[test]
fn missing_config_explains_itself() {
    let home = std::env::temp_dir().join(format!("anvil-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no config file"), "{err}");
    assert!(err.contains(".config/anvil/config.yaml"), "{err}");
    assert!(err.contains("team:"), "should print an example: {err}");
}
