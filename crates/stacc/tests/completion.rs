//! `stacc completion`: pure stdout script generation, no repo or state needed.

use std::process::{Command, Output};

fn stacc(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stacc"))
        .args(args)
        .output()
        .expect("spawn stacc")
}

/// Run `stacc completion <shell>` and return the emitted script.
fn script_for(shell: &str) -> String {
    let out = stacc(&["completion", shell]);
    assert!(
        out.status.success(),
        "completion {shell} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("script is utf-8")
}

/// The script is non-empty, completes the `stacc` name, and knows the
/// subcommands.
fn assert_shell_shaped(script: &str) {
    assert!(!script.trim().is_empty(), "script is empty");
    assert!(script.contains("stacc"), "script never mentions stacc");
    assert!(script.contains("absorb"), "script misses the absorb subcommand");
    assert!(script.contains("restack"), "script misses the restack subcommand");
}

#[test]
fn bash_emits_a_script() {
    assert_shell_shaped(&script_for("bash"));
}

#[test]
fn zsh_emits_a_script() {
    assert_shell_shaped(&script_for("zsh"));
}

#[test]
fn fish_emits_a_script() {
    assert_shell_shaped(&script_for("fish"));
}

#[test]
fn unknown_shell_is_a_usage_error() {
    let out = stacc(&["completion", "tcsh"]);
    assert!(!out.status.success(), "unknown shell should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("possible values"),
        "stderr should list possible values: {stderr}"
    );
}
