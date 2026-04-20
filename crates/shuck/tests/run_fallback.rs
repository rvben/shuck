use assert_cmd::Command;

#[test]
fn run_without_rootfs_and_no_default_hints_pull() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("shuck").unwrap();
    cmd.env("SHUCK_DATA_DIR", tmp.path())
        .env("HOME", tmp.path())
        .arg("run");
    let output = cmd.output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("shuck images pull"),
        "stderr did not hint at `shuck images pull`:\n{stderr}"
    );
}
