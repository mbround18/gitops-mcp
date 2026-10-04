//! The binary is an MCP stdio server, so argument handling has to answer and exit
//! rather than block reading the protocol from a terminal.

use std::process::{Command, Stdio};

fn run(arg: &str) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_gitops-mcp"))
        .arg(arg)
        .stdin(Stdio::null())
        .output()
        .expect("spawn gitops-mcp");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[test]
fn help_exits_instead_of_waiting_for_the_protocol() {
    let (ok, stdout) = run("--help");
    assert!(ok, "--help should exit successfully");
    assert!(stdout.contains("Usage: gitops-mcp"), "stdout: {stdout}");
    assert!(stdout.contains("--log"), "stdout: {stdout}");
}

#[test]
fn version_reports_the_crate_version() {
    let (ok, stdout) = run("--version");
    assert!(ok, "--version should exit successfully");
    assert_eq!(
        stdout.trim(),
        format!("gitops-mcp {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn an_unknown_flag_fails_fast() {
    let (ok, _) = run("--bypass-signing");
    assert!(!ok, "unknown flags must be rejected, not ignored");
}
