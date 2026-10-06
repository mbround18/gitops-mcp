//! End-to-end: the restore, merge and push tools driven over real stdio JSON-RPC against
//! a real repository, a real signing key and a real (local, bare) remote.
//!
//! These prove the safeguards hold against git itself rather than against a script.

mod sandbox;

use sandbox::{EMAIL, Sandbox, Server};
use serde_json::json;

fn commit_all(server: &mut Server, repo: &std::path::Path, message: &str) {
    let result = server.call(
        "commit",
        json!({"message": message, "all": true, "cwd": repo.to_str().unwrap()}),
    );
    assert!(!result.is_error(), "commit failed: {}", result.text());
}

// ---------------------------------------------------------------- restore

#[test]
fn restore_discards_the_change_and_leaves_a_recoverable_patch() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    sandbox.write("README.md", "# sandbox\nan edit nobody wants\n");

    let result = server.call(
        "restore",
        json!({"files": ["README.md"], "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(!result.is_error(), "{}", result.text());

    // The file really is back.
    assert_eq!(
        std::fs::read_to_string(sandbox.repo.join("README.md")).unwrap(),
        "# sandbox\n"
    );
    assert_eq!(sandbox.git(&["status", "--porcelain"]), "");

    // And the discarded edit really is recoverable.
    let patch = result.structured()["backup"].as_str().unwrap().to_owned();
    let saved = std::fs::read_to_string(&patch).expect("the patch exists on disk");
    assert!(saved.contains("an edit nobody wants"), "{saved}");
    assert!(result.text().contains("git apply"), "{}", result.text());

    sandbox.git(&["apply", &patch]);
    assert!(
        std::fs::read_to_string(sandbox.repo.join("README.md"))
            .unwrap()
            .contains("an edit nobody wants"),
        "the saved patch has to actually apply"
    );
}

#[test]
fn restore_also_discards_a_staged_change() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    sandbox.write("README.md", "# sandbox\nstaged edit\n");
    sandbox.git(&["add", "README.md"]);

    let result = server.call(
        "restore",
        json!({"files": ["README.md"], "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(sandbox.git(&["status", "--porcelain"]), "");

    let patch = result.structured()["backup"].as_str().unwrap();
    assert!(
        std::fs::read_to_string(patch)
            .unwrap()
            .contains("staged edit"),
        "a staged change has to be in the patch too"
    );
}

#[test]
fn restore_refuses_to_wipe_the_tree() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");
    sandbox.write("README.md", "# sandbox\nstill here\n");

    for files in [json!([]), json!(["."]), json!([":/"]), json!(["../escape"])] {
        let result = server.call(
            "restore",
            json!({"files": files, "cwd": sandbox.repo.to_str().unwrap()}),
        );
        assert!(result.is_error(), "{files} should be refused");
    }

    // The edit survived every refusal.
    assert!(
        std::fs::read_to_string(sandbox.repo.join("README.md"))
            .unwrap()
            .contains("still here")
    );
}

#[test]
fn restoring_an_unknown_path_fails_rather_than_doing_nothing() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    let result = server.call(
        "restore",
        json!({"files": ["does/not/exist.txt"], "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(result.is_error(), "{}", result.text());
}

// ---------------------------------------------------------------- merge_ff_only

#[test]
fn a_fast_forward_merge_moves_the_branch() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    sandbox.git(&["checkout", "-q", "-b", "topic"]);
    sandbox.write("feature.txt", "work\n");
    commit_all(&mut server, &sandbox.repo, "feat: work");
    let topic = sandbox.git(&["rev-parse", "HEAD"]);
    sandbox.git(&["checkout", "-q", "main"]);

    let result = server.call(
        "merge_ff_only",
        json!({"ref": "topic", "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(!result.is_error(), "{}", result.text());

    assert_eq!(sandbox.git(&["rev-parse", "HEAD"]), topic);
    // A fast-forward adds no commit of its own.
    assert_eq!(sandbox.git(&["rev-list", "--count", "HEAD"]), "2");
}

#[test]
fn diverged_branches_are_refused_and_left_alone() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    sandbox.git(&["checkout", "-q", "-b", "topic"]);
    sandbox.write("feature.txt", "work\n");
    commit_all(&mut server, &sandbox.repo, "feat: work");

    sandbox.git(&["checkout", "-q", "main"]);
    sandbox.write("other.txt", "divergence\n");
    commit_all(&mut server, &sandbox.repo, "chore: diverge");
    let before = sandbox.git(&["rev-parse", "HEAD"]);

    let result = server.call(
        "merge_ff_only",
        json!({"ref": "topic", "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(result.is_error(), "{}", result.text());
    let text = result.text();
    assert!(text.contains("diverged"), "{text}");

    // Nothing happened: no merge commit, no moved branch, no merge left in progress.
    assert_eq!(sandbox.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(sandbox.git(&["rev-list", "--count", "HEAD"]), "2");
    assert!(!sandbox.repo.join(".git/MERGE_HEAD").exists());
}

#[test]
fn a_dirty_tree_blocks_the_merge_but_untracked_files_do_not() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    sandbox.git(&["checkout", "-q", "-b", "topic"]);
    sandbox.write("feature.txt", "work\n");
    commit_all(&mut server, &sandbox.repo, "feat: work");
    sandbox.git(&["checkout", "-q", "main"]);

    // A tracked modification blocks it.
    sandbox.write("README.md", "# sandbox\nuncommitted\n");
    let result = server.call(
        "merge_ff_only",
        json!({"ref": "topic", "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(result.is_error(), "{}", result.text());
    assert!(
        result.text().contains("Commit or stash"),
        "{}",
        result.text()
    );
    assert!(
        std::fs::read_to_string(sandbox.repo.join("README.md"))
            .unwrap()
            .contains("uncommitted"),
        "the refusal must not discard the change it refused over"
    );

    // An untracked file does not.
    sandbox.write("README.md", "# sandbox\n");
    sandbox.write("scratch.log", "build output\n");
    let result = server.call(
        "merge_ff_only",
        json!({"ref": "topic", "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(!result.is_error(), "{}", result.text());
}

#[test]
fn an_unknown_ref_is_refused() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    let result = server.call(
        "merge_ff_only",
        json!({"ref": "no-such-branch", "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(result.is_error(), "{}", result.text());
    assert!(result.text().contains("not a ref"), "{}", result.text());
}

// ---------------------------------------------------------------- push

#[test]
fn a_signed_branch_reaches_the_remote() {
    let sandbox = Sandbox::new();
    let remote = sandbox.bare_remote("origin");
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    let result = server.call("push", json!({"cwd": sandbox.repo.to_str().unwrap()}));
    assert!(!result.is_error(), "{}", result.text());

    assert_eq!(
        sandbox.remote_git(&remote, &["rev-parse", "refs/heads/main"]),
        sandbox.git(&["rev-parse", "HEAD"]),
        "the remote has the branch"
    );
    // Tracking was set up, so the next push needs no arguments either.
    assert_eq!(
        sandbox.git(&["rev-parse", "--abbrev-ref", "main@{upstream}"]),
        "origin/main"
    );

    let again = server.call("push", json!({"cwd": sandbox.repo.to_str().unwrap()}));
    assert!(!again.is_error(), "{}", again.text());
    assert!(again.text().contains("up to date"), "{}", again.text());
}

#[test]
fn an_unsigned_commit_never_reaches_the_remote() {
    let sandbox = Sandbox::new();
    let remote = sandbox.bare_remote("origin");
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    // Something else makes an unsigned commit, exactly the case the commit tool cannot
    // prevent on its own.
    sandbox.write("sneaky.txt", "unsigned\n");
    sandbox.git(&["add", "sneaky.txt"]);
    sandbox.git(&["commit", "-q", "--no-gpg-sign", "-m", "chore: unsigned"]);
    let head = sandbox.git(&["rev-parse", "HEAD"]);

    let result = server.call("push", json!({"cwd": sandbox.repo.to_str().unwrap()}));
    assert!(result.is_error(), "{}", result.text());
    let text = result.text();
    assert!(text.contains("not signed"), "{text}");
    assert!(
        text.contains(&head[..8]),
        "the offending commit is named: {text}"
    );

    // The remote never heard about it.
    assert_eq!(
        sandbox.remote_git(&remote, &["rev-list", "--all", "--count"]),
        "0"
    );
}

#[test]
fn a_remote_that_moved_ahead_is_reported_not_forced() {
    let sandbox = Sandbox::new();
    let remote = sandbox.bare_remote("origin");
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");
    server.call("push", json!({"cwd": sandbox.repo.to_str().unwrap()}));
    let published = sandbox.git(&["rev-parse", "HEAD"]);

    // Someone else lands a commit, and we rewrite ours on top of the old tip.
    let other = sandbox.home_path().join("other");
    sandbox.clone_to(&other, &remote);
    std::fs::write(other.join("theirs.txt"), "theirs\n").unwrap();
    sandbox.run_in(&other, &["add", "theirs.txt"]);
    sandbox.run_in(&other, &["commit", "-q", "-m", "feat: theirs"]);
    sandbox.run_in(&other, &["push", "-q", "origin", "main"]);

    sandbox.write("mine.txt", "mine\n");
    commit_all(&mut server, &sandbox.repo, "feat: mine");

    let result = server.call("push", json!({"cwd": sandbox.repo.to_str().unwrap()}));
    assert!(result.is_error(), "{}", result.text());
    let text = result.text();
    assert!(text.contains("never force-pushes"), "{text}");

    // Their commit is still the remote tip: nothing was overwritten.
    let tip = sandbox.remote_git(&remote, &["log", "-1", "--format=%s", "refs/heads/main"]);
    assert_eq!(tip, "feat: theirs");
    assert_ne!(tip, "feat: mine");
    assert_ne!(published, sandbox.git(&["rev-parse", "HEAD"]));
}

#[test]
fn a_failing_pre_push_hook_stops_the_push() {
    let sandbox = Sandbox::new();
    let remote = sandbox.bare_remote("origin");
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    sandbox.write_hook(
        ".git/hooks",
        "pre-push",
        "#!/bin/sh\necho 'tests are failing, not publishing' >&2\nexit 1\n",
    );

    let result = server.call("push", json!({"cwd": sandbox.repo.to_str().unwrap()}));
    assert!(result.is_error(), "{}", result.text());
    let text = result.text();
    assert!(text.contains("tests are failing"), "{text}");
    assert!(text.contains("Fix what the hook reports"), "{text}");
    assert!(!text.contains("--no-verify"), "{text}");

    // The hook is untouched and the remote is empty.
    assert_eq!(
        sandbox.remote_git(&remote, &["rev-list", "--all", "--count"]),
        "0"
    );
    let hook = sandbox.repo.join(".git/hooks/pre-push");
    assert!(hook.exists(), "the hook must not be deleted");
    assert!(
        sandbox::is_executable(&hook),
        "the hook must not be made unrunnable"
    );
}

#[test]
fn an_unknown_remote_is_refused() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    let result = server.call(
        "push",
        json!({"remote": "nowhere", "cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(result.is_error(), "{}", result.text());
    assert!(
        result.text().contains("not a configured remote"),
        "{}",
        result.text()
    );
}

#[test]
fn the_signing_identity_is_what_lands() {
    let sandbox = Sandbox::new();
    let remote = sandbox.bare_remote("origin");
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");
    server.call("push", json!({"cwd": sandbox.repo.to_str().unwrap()}));

    assert_eq!(
        sandbox.remote_git(&remote, &["log", "-1", "--format=%ae", "refs/heads/main"]),
        EMAIL
    );
}

// ---------------------------------------------------------------- diff

#[test]
fn diff_summarises_the_change_and_withholds_the_patch_until_asked() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    sandbox.write("README.md", "# sandbox\nan edit\n");
    sandbox.write("new.txt", "fresh\n");
    let repo = sandbox.repo.to_str().unwrap().to_owned();

    let summary = server.call("diff", json!({"cwd": repo}));
    assert!(!summary.is_error(), "{}", summary.text());
    let text = summary.text();
    assert!(text.contains("modified README.md +1 -0"), "{text}");
    assert!(text.contains("+1 -0\n"), "{text}");
    // Untracked files are not a diff, and the patch is not in the summary.
    assert!(!text.contains("new.txt"), "{text}");
    assert!(!text.contains("@@"), "the hunks are opt-in: {text}");
    assert_eq!(summary.structured()["added"], 1);
    assert_eq!(summary.structured()["files"][0]["status"], "modified");

    let patch = server.call("diff", json!({"cwd": repo, "patch": true}));
    assert!(!patch.is_error(), "{}", patch.text());
    assert!(patch.text().contains("@@"), "{}", patch.text());
    assert!(patch.text().contains("+an edit"), "{}", patch.text());

    // And nothing it did touched the working tree.
    assert_eq!(
        std::fs::read_to_string(sandbox.repo.join("README.md")).unwrap(),
        "# sandbox\nan edit\n"
    );
    assert_eq!(sandbox.git(&["status", "--porcelain"]).lines().count(), 2);
}

#[test]
fn diff_reads_the_index_and_a_range_and_reports_an_unknown_revision() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");
    let repo = sandbox.repo.to_str().unwrap().to_owned();

    sandbox.write("README.md", "# sandbox\nstaged\n");
    sandbox.git(&["add", "README.md"]);

    let staged = server.call("diff", json!({"cwd": repo, "staged": true}));
    assert!(!staged.is_error(), "{}", staged.text());
    assert!(
        staged.text().contains("the index against `HEAD`"),
        "{}",
        staged.text()
    );
    assert_eq!(staged.structured()["files"][0]["path"], "README.md");

    commit_all(&mut server, &sandbox.repo, "docs: stage");
    let range = server.call("diff", json!({"cwd": repo, "rev": "HEAD~1..HEAD"}));
    assert!(!range.is_error(), "{}", range.text());
    assert_eq!(range.structured()["files"][0]["status"], "modified");

    let clean = server.call("diff", json!({"cwd": repo}));
    assert!(clean.text().contains("No changes"), "{}", clean.text());
    assert_eq!(clean.structured()["unchanged"], true);

    let unknown = server.call("diff", json!({"cwd": repo, "rev": "no-such-ref"}));
    assert!(unknown.is_error(), "{}", unknown.text());
    assert!(unknown.text().contains("not a ref"), "{}", unknown.text());

    let both = server.call("diff", json!({"cwd": repo, "staged": true, "rev": "HEAD"}));
    assert!(both.is_error(), "{}", both.text());
}
