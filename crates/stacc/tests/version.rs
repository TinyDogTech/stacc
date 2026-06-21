use std::process::Command;

// STA-116: `--version` must report the bumped crate version, not stay stuck at
// the published 0.1.0. The git-build suffix `(<sha>[-dirty])` is stamped by
// build.rs and is environment-dependent (absent when `.git` is absent), so it is
// not asserted here; the version number is the regression this guards.
#[test]
fn version_reports_the_bumped_crate_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_stacc"))
        .arg("--version")
        .output()
        .expect("run stacc --version");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = env!("CARGO_PKG_VERSION");
    assert!(
        stdout.starts_with(&format!("stacc {expected}")),
        "expected the {expected} version line, got: {stdout}"
    );
}
