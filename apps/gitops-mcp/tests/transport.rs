//! End-to-end: a push whose transport never answers must come back as an error, not hang
//! the server.
//!
//! The remote is an `ssh://` URL whose `core.sshCommand` is a local script standing in for
//! ssh, so these exercise the real git transport path with no network and no credentials.

mod sandbox;

use std::time::Duration;

use sandbox::{Sandbox, Server};
use serde_json::json;

/// Point `origin` at a bare repository reached through `ssh_script`.
fn ssh_remote(sandbox: &Sandbox, ssh_script: &str) {
    let bare = sandbox.home_path().join("origin.git");
    std::fs::create_dir_all(&bare).unwrap();
    sandbox.run_in(&bare, &["init", "-q", "--bare"]);
    let ssh = sandbox.home_path().join("fake-ssh");
    std::fs::write(&ssh, ssh_script).unwrap();
    sandbox::set_executable(&ssh, true);
    sandbox.git(&["config", "core.sshCommand", ssh.to_str().unwrap()]);
    // Skip git's `-G` probe of an unknown ssh, so the script sees the real connection.
    sandbox.git(&["config", "ssh.variant", "ssh"]);
    let url = format!("ssh://fake-host{}", bare.display());
    sandbox.git(&["remote", "add", "origin", &url]);
}

fn seed(server: &mut Server, sandbox: &Sandbox) {
    let result = server.call(
        "commit",
        json!({"message": "chore: seed", "all": true, "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(!result.is_error(), "commit failed: {}", result.text());
}

/// ssh asks for a passphrase or a host-key confirmation on `/dev/tty` whenever it has a
/// controlling terminal. An MCP client running in a terminal hands the server its own,
/// which nobody reads on ssh's behalf: the prompt waits forever and stderr stays empty.
#[test]
fn a_transport_that_prompts_on_the_terminal_fails_instead_of_hanging() {
    let sandbox = Sandbox::new();
    ssh_remote(
        &sandbox,
        "#!/bin/sh\n\
         if (exec 3</dev/tty) 2>/dev/null; then\n\
         \x20 printf 'Enter passphrase for key: ' >/dev/tty\n\
         \x20 read -r answer </dev/tty\n\
         fi\n\
         echo 'fake-ssh: no terminal to prompt on' >&2\n\
         exit 255\n",
    );
    let mut server = sandbox.server_with(&[], true);
    seed(&mut server, &sandbox);

    let (_server, result) = server.call_within(
        "push",
        json!({"cwd": sandbox.repo.to_str().unwrap()}),
        Duration::from_secs(60),
    );
    assert!(result.is_error(), "{}", result.text());
    assert!(
        result.text().contains("fake-ssh: no terminal to prompt on"),
        "the transport's own error is reported: {}",
        result.text()
    );
}

/// A transport that stalls outright is killed at the network timeout, and what it printed
/// before stalling is in the error.
#[test]
fn a_stalled_transport_is_killed_and_reported() {
    let sandbox = Sandbox::new();
    ssh_remote(
        &sandbox,
        "#!/bin/sh\n\
         echo \"fake-ssh: connecting to $1\" >&2\n\
         exec sleep 600\n",
    );
    let mut server = sandbox.server_with(&[("GITOPS_MCP_NETWORK_TIMEOUT_SECS", "2")], false);
    seed(&mut server, &sandbox);

    let (_server, result) = server.call_within(
        "push",
        json!({"cwd": sandbox.repo.to_str().unwrap()}),
        Duration::from_secs(60),
    );
    let text = result.text();
    assert!(result.is_error(), "{text}");
    assert!(text.contains("fake-ssh: connecting to fake-host"), "{text}");
    assert!(text.contains("was killed"), "{text}");
    assert!(text.contains("GITOPS_MCP_NETWORK_TIMEOUT_SECS"), "{text}");
}
