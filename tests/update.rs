//! CLI parsing/version gates on a private executable and isolated HOME/XDG.
#![cfg(unix)]
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[test]
fn update_cli_is_explicit_and_version_is_side_effect_free() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/update-cli-fixtures")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(root.join("home/.local/bin")).unwrap();
    let exe = root.join("home/.local/bin/muqun-gateway");
    fs::copy(env!("CARGO_BIN_EXE_muqun-gateway"), &exe).unwrap();
    let invoke = |args: &[&str]| {
        Command::new(&exe)
            .args(args)
            .env_clear()
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("state"))
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let version = invoke(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("muqun-gateway {}\n", env!("CARGO_PKG_VERSION"))
    );
    let help = invoke(&["update", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8(help.stdout).unwrap().contains("--check"));
    for args in [
        &["update", "--version", "v0.1.0"][..],
        &["update", "--url", "http://127.0.0.1/"][..],
        &["update", "--prerelease"][..],
    ] {
        assert!(
            !invoke(args).status.success(),
            "no downgrade/source override/prerelease option"
        );
    }
    assert!(!root.join("config").exists());
    assert!(!root.join("state").exists());
    fs::remove_dir_all(root).unwrap();
}
