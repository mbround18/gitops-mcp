//! End-to-end hook guards, against a real repository with real hooks.
//!
//! The point of these tests is the negative assertion: when a `pre-commit` hook fails,
//! the server surfaces the hook's complaint and leaves the repository and the config
//! exactly as they were. It does not retry with `--no-verify`, disable the hook, or drop
//! signing to get the commit through.
//!
//! Hook semantics per <https://git-scm.com/docs/githooks>: `pre-commit` runs before the
//! commit message is finalized and a non-zero exit aborts the commit.

mod sandbox;

use sandbox::{EMAIL, Sandbox, is_executable, set_executable};
use serde_json::json;

const FAILING_HOOK: &str = "#!/bin/sh\necho 'lint: trailing whitespace in README.md' >&2\nexit 1\n";
const PASSING_HOOK: &str = "#!/bin/sh\nexit 0\n";

fn commit_all(server: &mut sandbox::Server, repo: &std::path::Path) -> sandbox::ToolResult {
    server.call(
        "commit",
        json!({"message": "test: sandbox commit", "all": true, "cwd": repo}),
    )
}

#[test]
fn a_failing_pre_commit_hook_aborts_the_commit_and_explains_why() {
    let sandbox = Sandbox::new();
    sandbox.write_hook(".git/hooks", "pre-commit", FAILING_HOOK);

    let mut server = sandbox.server();
    let result = commit_all(&mut server, &sandbox.repo);
    let text = result.text();

    assert!(result.is_error(), "expected a tool error, got: {text}");
    // The hook's own output is the actionable part.
    assert!(text.contains("trailing whitespace"), "{text}");
    assert!(text.contains("Fix what the hook reports"), "{text}");
    assert!(text.contains("unrelated to signing"), "{text}");
    // The rule is stated without naming the flag that breaks it.
    assert!(!text.contains("--no-verify"), "{text}");

    // Nothing was committed.
    assert_eq!(sandbox.git(&["rev-list", "--all", "--count"]), "0");
}

#[test]
fn a_rejected_commit_leaves_signing_and_hooks_untouched() {
    let sandbox = Sandbox::new();
    let hook = sandbox.write_hook(".git/hooks", "pre-commit", FAILING_HOOK);

    let mut server = sandbox.server();
    assert!(commit_all(&mut server, &sandbox.repo).is_error());

    // Signing is still required, globally and locally.
    assert_eq!(sandbox.git(&["config", "commit.gpgsign"]), "true");
    assert_eq!(
        sandbox.git(&["config", "--global", "commit.gpgsign"]),
        "true"
    );
    // The hook was not disabled, moved, or stripped of its executable bit.
    assert!(hook.exists(), "the hook was removed");
    assert!(is_executable(&hook), "the hook lost its executable bit");
    // core.hooksPath was not redirected away from the hook.
    assert!(
        sandbox
            .git(&["config", "--default", "", "core.hooksPath"])
            .is_empty(),
        "core.hooksPath was rewritten"
    );
}

#[test]
fn a_passing_hook_produces_a_real_signed_commit() {
    let sandbox = Sandbox::new();
    sandbox.write_hook(".git/hooks", "pre-commit", PASSING_HOOK);

    let mut server = sandbox.server();
    let result = commit_all(&mut server, &sandbox.repo);
    let text = result.text();
    assert!(!result.is_error(), "{text}");

    assert_eq!(sandbox.git(&["rev-list", "--all", "--count"]), "1");
    // `git` itself vouches for the signature, not just our report of it.
    assert_eq!(sandbox.git(&["log", "-1", "--format=%G?"]), "G");
    assert_eq!(sandbox.git(&["log", "-1", "--format=%ae"]), EMAIL);
    assert_eq!(result.structured()["signed"], json!(true));
}

#[test]
fn a_hook_under_core_hooks_path_is_honored_too() {
    let sandbox = Sandbox::new();
    sandbox.write_hook("githooks", "pre-commit", FAILING_HOOK);
    sandbox.git(&["config", "core.hooksPath", "githooks"]);

    let mut server = sandbox.server();
    let result = commit_all(&mut server, &sandbox.repo);
    let text = result.text();
    assert!(result.is_error(), "{text}");
    assert!(text.contains("trailing whitespace"), "{text}");
    assert_eq!(sandbox.git(&["rev-list", "--all", "--count"]), "0");
}

#[test]
fn a_non_executable_hook_is_reported_as_drift() {
    let sandbox = Sandbox::new();
    let hook = sandbox.write_hook(".git/hooks", "pre-commit", FAILING_HOOK);
    set_executable(&hook, false);

    let mut server = sandbox.server();
    let result = server.call("git_signing_status", json!({"cwd": sandbox.repo}));
    let text = result.text();

    // git skips a non-executable hook silently, which looks exactly like a passing one.
    assert!(text.contains("not executable"), "{text}");
    let hooks = &result.structured()["hooks"];
    assert_eq!(hooks["pre_commit_present"], json!(true));
    assert_eq!(hooks["pre_commit_executable"], json!(false));
}

#[test]
fn a_hooks_path_pointing_nowhere_is_reported_as_drift() {
    let sandbox = Sandbox::new();
    sandbox.git(&["config", "core.hooksPath", "no-such-dir"]);

    let mut server = sandbox.server();
    let result = server.call("git_signing_status", json!({"cwd": sandbox.repo}));
    let text = result.text();

    assert!(text.contains("does not exist"), "{text}");
    assert_eq!(
        result.structured()["hooks"]["directory_exists"],
        json!(false)
    );
}

#[test]
fn a_clean_sandbox_reports_no_drift() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    let result = server.call("git_signing_status", json!({"cwd": sandbox.repo}));
    let text = result.text();

    assert!(text.contains("Drift: none"), "{text}");
    assert_eq!(result.structured()["signing_available"], json!(true));
}

#[test]
fn missing_ssh_allowed_signers_is_recreated_by_signing_enforce() {
    let sandbox = Sandbox::new();
    let allowed = sandbox.home_path().join("allowed_signers");
    std::fs::remove_file(&allowed).unwrap();

    let mut server = sandbox.server();
    let status = server.call("git_signing_status", json!({"cwd": sandbox.repo}));
    assert!(
        status.text().contains("allowed signers file"),
        "{}",
        status.text()
    );

    let result = server.call("git_signing_enforce", json!({"cwd": sandbox.repo}));
    let text = result.text();
    assert!(!result.is_error(), "{text}");
    assert!(allowed.exists(), "allowed signers file was not recreated");

    let contents = std::fs::read_to_string(&allowed).unwrap();
    assert!(contents.contains(EMAIL), "{contents}");
    assert!(contents.contains("ssh-ed25519"), "{contents}");
}
