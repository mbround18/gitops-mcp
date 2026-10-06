mod sandbox;

use sandbox::{Sandbox, Server};
use serde_json::json;

fn commit_all(server: &mut Server, repo: &std::path::Path, message: &str) {
    let result = server.call(
        "commit",
        json!({"message": message, "all": true, "cwd": repo.to_str().unwrap()}),
    );
    assert!(!result.is_error(), "commit failed: {}", result.text());
}

#[test]
fn workspace_scan_reports_sibling_divergence_and_dirty_state() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    let sibling = sandbox.home_path().join("ThunderForgeVTT-levels");
    sandbox.clone_to(&sibling, &sandbox.repo);
    std::fs::write(sibling.join("README.md"), "# sandbox\nsibling edit\n").unwrap();
    sandbox.run_in(&sibling, &["add", "README.md"]);
    sandbox.run_in(&sibling, &["commit", "-q", "-m", "feat: sibling"]);

    let result = server.call(
        "workspace_scan",
        json!({"cwd": sandbox.repo.to_str().unwrap()}),
    );
    assert!(!result.is_error(), "{}", result.text());

    let listed = &result.structured()["workspaces"];
    assert!(listed.is_array());
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let first = &listed[0];
    assert_eq!(
        first["workspace"].as_str().unwrap(),
        "ThunderForgeVTT-levels"
    );
    assert_eq!(first["dirty"].as_bool().unwrap(), false);
    assert_eq!(first["tracked_changes"].as_u64().unwrap(), 0);
    assert_eq!(first["untracked_changes"].as_u64().unwrap(), 0);
    assert_eq!(first["ahead"].as_u64().unwrap(), 1);
}

#[test]
fn workspace_diff_export_and_apply_move_tracked_delta_into_current_repo() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    let sibling = sandbox.home_path().join("ThunderForgeVTT-levels");
    sandbox.clone_to(&sibling, &sandbox.repo);
    std::fs::write(
        sibling.join("README.md"),
        "# sandbox\ncopied from sibling\n",
    )
    .unwrap();

    let patch = sandbox.home_path().join("workspace.patch");
    let export = server.call(
        "workspace_diff_export",
        json!({
            "workspace": sibling.to_str().unwrap(),
            "output": patch.to_str().unwrap(),
            "base_ref": "HEAD",
            "cwd": sandbox.repo.to_str().unwrap()
        }),
    );
    assert!(!export.is_error(), "{}", export.text());
    assert!(patch.exists(), "patch file should be created");

    let check = server.call(
        "workspace_apply",
        json!({
            "patch": patch.to_str().unwrap(),
            "dry_run": true,
            "cwd": sandbox.repo.to_str().unwrap()
        }),
    );
    assert!(!check.is_error(), "{}", check.text());

    let apply = server.call(
        "workspace_apply",
        json!({
            "patch": patch.to_str().unwrap(),
            "cwd": sandbox.repo.to_str().unwrap()
        }),
    );
    assert!(!apply.is_error(), "{}", apply.text());

    let readme = std::fs::read_to_string(sandbox.repo.join("README.md")).unwrap();
    assert!(readme.contains("copied from sibling"), "{readme}");
}

#[test]
fn workspace_cleanup_requires_confirmation_and_can_quarantine() {
    let sandbox = Sandbox::new();
    let mut server = sandbox.server();
    commit_all(&mut server, &sandbox.repo, "chore: seed");

    let sibling = sandbox.home_path().join("ThunderForgeVTT-levels");
    sandbox.clone_to(&sibling, &sandbox.repo);

    let denied = server.call(
        "workspace_cleanup",
        json!({
            "cwd": sandbox.repo.to_str().unwrap(),
            "mode": "quarantine",
            "quarantine_dir": sandbox.home_path().join("quarantine").to_str().unwrap()
        }),
    );
    assert!(denied.is_error(), "cleanup without confirm must be refused");
    assert!(sibling.exists(), "refused cleanup must not remove anything");

    let cleaned = server.call(
        "workspace_cleanup",
        json!({
            "cwd": sandbox.repo.to_str().unwrap(),
            "confirm": true,
            "mode": "quarantine",
            "quarantine_dir": sandbox.home_path().join("quarantine").to_str().unwrap()
        }),
    );
    assert!(!cleaned.is_error(), "{}", cleaned.text());
    assert!(!sibling.exists(), "sibling should be moved away");
    let quarantine = sandbox.home_path().join("quarantine");
    let moved_count = std::fs::read_dir(&quarantine).unwrap().count();
    assert!(
        moved_count >= 1,
        "quarantine should receive moved workspaces"
    );
}
