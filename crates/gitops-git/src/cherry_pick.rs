//! Signed cherry-picks that either land whole or leave the branch where it was.
//!
//! A cherry-pick creates commits, so it is held to the same rules as [`crate::commit`]:
//! governance runs first and every new commit is signed. Each pick records where it came
//! from (`-x`) and keeps the original author, so taking a contributor's work onto an
//! integration branch preserves attribution.
//!
//! When a pick cannot be applied — a conflict, a change that is already there, a failed
//! signature — the whole cherry-pick is aborted and the branch goes back to the commit it
//! started on. There is no parameter that resolves a conflict by picking a side, skips the
//! commit that failed, or replays a merge commit against a chosen parent: each of those
//! decides what the code should be, which is the author's call.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    Error, Result,
    commit::looks_like_signing_failure,
    governance::{Reconciliation, reconcile},
    merge::{check_ref, combined},
    push::{CommitSignature, parse_log},
    runner::CommandRunner,
    trimmed,
};

/// The commits to replay onto the current branch, oldest first.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CherryPickRequest {
    /// Commits to pick, applied in the order given. Each names one commit; ranges are
    /// refused so the set being picked is always spelled out.
    pub commits: Vec<String>,
    /// Repository to work in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CherryPickOutcome {
    /// Config corrections applied before picking.
    pub governance: Reconciliation,
    pub branch: Option<String>,
    /// The commits that were picked, resolved to full ids.
    pub sources: Vec<String>,
    pub before: String,
    pub after: Option<String>,
    /// The new commits on the branch, oldest first, with git's verdict on each signature.
    pub picked: Vec<CommitSignature>,
    pub command: String,
    pub output: String,
}

/// State files git leaves in the git directory while an operation is unfinished.
const IN_PROGRESS: [(&str, &str); 6] = [
    ("sequencer", "cherry-pick or revert"),
    ("CHERRY_PICK_HEAD", "cherry-pick"),
    ("REVERT_HEAD", "revert"),
    ("MERGE_HEAD", "merge"),
    ("rebase-merge", "rebase"),
    ("rebase-apply", "rebase or am"),
];

/// Replay `commits` onto the current branch as signed commits, or change nothing.
pub fn cherry_pick(
    runner: &dyn CommandRunner,
    request: &CherryPickRequest,
) -> Result<CherryPickOutcome> {
    let cwd = request.cwd.as_deref();
    let targets: Vec<&str> = request.commits.iter().map(|c| c.trim()).collect();

    if targets.is_empty() {
        return Err(Error::InvalidRequest(
            "name the commits to cherry-pick".into(),
        ));
    }
    for target in &targets {
        check_ref(target, "cherry-pick")?;
    }

    // Governance first: the picks are new commits, and must not be made under config
    // some other tool bent.
    let governance = reconcile(runner, cwd, false)?;
    governance.status.require_repository()?;
    if !governance.status.signing_available {
        return Err(Error::SigningUnavailable {
            detail: if governance.unresolved.is_empty() {
                "no usable signing key".to_owned()
            } else {
                governance.unresolved.join("; ")
            },
        });
    }

    let git_dir = runner
        .run("git", &["rev-parse", "--absolute-git-dir"], cwd)?
        .value()
        .map(PathBuf::from)
        .ok_or(Error::NotARepository)?;
    // Someone else's unfinished operation is theirs to finish. Refusing here is also what
    // makes the abort below safe: any cherry-pick state found after a failure is ours.
    if let Some(operation) = in_progress(runner, &git_dir) {
        return Err(Error::OperationInProgress {
            operation: operation.into(),
        });
    }

    let dirty = runner.run(
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
        cwd,
    )?;
    if dirty.ok() && !dirty.stdout.trim().is_empty() {
        return Err(Error::WorkTreeDirty {
            action: "cherry-pick".into(),
            detail: trimmed(&dirty.stdout),
        });
    }

    let mut sources = Vec::with_capacity(targets.len());
    for target in &targets {
        let resolved = runner
            .run(
                "git",
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{target}^{{commit}}"),
                ],
                cwd,
            )?
            .value()
            .ok_or_else(|| Error::UnknownRef {
                reference: (*target).to_owned(),
            })?;
        let parents = runner
            .run("git", &["rev-list", "--parents", "-n", "1", &resolved], cwd)?
            .stdout;
        if parents.split_whitespace().count() > 2 {
            return Err(Error::InvalidRequest(format!(
                "`{target}` is a merge commit. Picking one means choosing which parent to \
                 replay it against, which is the author's decision — name the individual \
                 commits instead, or leave this one to the author."
            )));
        }
        sources.push(resolved);
    }

    let branch = runner
        .run("git", &["rev-parse", "--abbrev-ref", "HEAD"], cwd)?
        .value();
    let before = runner
        .run("git", &["rev-parse", "HEAD"], cwd)?
        .value()
        .ok_or_else(|| {
            Error::InvalidRequest("the current branch has no commits to pick onto yet".into())
        })?;

    let mut args: Vec<&str> = vec!["cherry-pick", "-x", "-S"];
    args.extend(sources.iter().map(String::as_str));
    let command = format!("git {}", args.join(" "));
    let out = runner.run("git", &args, cwd)?;
    if !out.ok() {
        return Err(abort(
            runner,
            cwd,
            &git_dir,
            &before,
            &out.stdout,
            &out.stderr,
        ));
    }

    let after = runner.run("git", &["rev-parse", "HEAD"], cwd)?.value();
    let range = format!("{before}..HEAD");
    let picked = parse_log(
        &runner
            .run(
                "git",
                &["log", "--reverse", "--format=%H %G? %s", &range],
                cwd,
            )?
            .stdout,
    );

    Ok(CherryPickOutcome {
        governance,
        branch,
        sources,
        before,
        after,
        picked,
        command,
        output: trimmed(&out.stdout),
    })
}

/// Classify a failed cherry-pick, after putting the branch back where it started.
fn abort(
    runner: &dyn CommandRunner,
    cwd: Option<&Path>,
    git_dir: &Path,
    before: &str,
    stdout: &str,
    stderr: &str,
) -> Error {
    let detail = combined(stdout, stderr);
    let run = |args: &[&str]| runner.run("git", args, cwd).ok();

    let stopped_at =
        run(&["rev-parse", "--verify", "--quiet", "CHERRY_PICK_HEAD"]).and_then(|out| out.value());
    let conflicts: Vec<String> = run(&["diff", "--name-only", "--diff-filter=U"])
        .map(|out| out.stdout.lines().map(str::to_owned).collect())
        .unwrap_or_default();

    let started = stopped_at.is_some() || runner.path_info(&git_dir.join("sequencer")).exists;
    if started {
        let aborted = run(&["cherry-pick", "--abort"]);
        let head = run(&["rev-parse", "HEAD"]).and_then(|out| out.value());
        if !aborted.as_ref().is_some_and(|out| out.ok()) || head.as_deref() != Some(before) {
            let why = aborted
                .map(|out| combined(&out.stdout, &out.stderr))
                .unwrap_or_default();
            return Error::CherryPickStuck {
                detail: format!("{detail}\n\nthe abort said: {why}"),
                before: before.to_owned(),
            };
        }
    }

    if !conflicts.is_empty() {
        return Error::CherryPickConflict {
            commit: stopped_at.unwrap_or_else(|| "(unknown)".into()),
            files: conflicts,
            detail,
        };
    }
    if looks_like_signing_failure(&detail) {
        return Error::SigningFailed { detail };
    }
    Error::CherryPickFailed { detail }
}

fn in_progress(runner: &dyn CommandRunner, git_dir: &Path) -> Option<&'static str> {
    IN_PROGRESS
        .iter()
        .find(|(name, _)| runner.path_info(&git_dir.join(name)).exists)
        .map(|(_, operation)| *operation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{commit::tests::compliant_runner, runner::ScriptedRunner};

    const PICK: &str = "git cherry-pick -x -S aaaa1111";

    fn runner() -> ScriptedRunner {
        compliant_runner()
            .with("git rev-parse --absolute-git-dir", 0, "/repo/.git\n", "")
            .with("git status --porcelain --untracked-files=no", 0, "", "")
            .with(
                "git rev-parse --verify --quiet topic~1^{commit}",
                0,
                "aaaa1111\n",
                "",
            )
            .with(
                "git rev-list --parents -n 1 aaaa1111",
                0,
                "aaaa1111 pppp0000\n",
                "",
            )
            .with("git rev-parse --abbrev-ref HEAD", 0, "integration\n", "")
            .with(PICK, 0, "[integration bbbb2222] feat: from a fork\n", "")
            .with(
                "git log --reverse --format=%H %G? %s abc1234..HEAD",
                0,
                "bbbb2222 G feat: from a fork\n",
                "",
            )
    }

    fn request() -> CherryPickRequest {
        CherryPickRequest {
            commits: vec!["topic~1".into()],
            cwd: None,
        }
    }

    fn conflicted(runner: ScriptedRunner) -> ScriptedRunner {
        runner
            .with(
                PICK,
                1,
                "",
                "error: could not apply aaaa1111... feat: from a fork\n",
            )
            .with(
                "git rev-parse --verify --quiet CHERRY_PICK_HEAD",
                0,
                "aaaa1111\n",
                "",
            )
            .with(
                "git diff --name-only --diff-filter=U",
                0,
                "src/lib.rs\n",
                "",
            )
            .with("git cherry-pick --abort", 0, "", "")
    }

    #[test]
    fn a_signed_pick_that_records_its_origin_is_the_only_form_run() {
        let runner = runner();
        let outcome = cherry_pick(&runner, &request()).unwrap();
        assert_eq!(outcome.command, PICK);
        assert_eq!(outcome.sources, vec!["aaaa1111".to_owned()]);
        assert_eq!(outcome.picked.len(), 1);
        assert_eq!(outcome.picked[0].verdict, "G");

        let calls = runner.calls();
        let picks: Vec<_> = calls.iter().filter(|c| c.contains("cherry-pick")).collect();
        assert_eq!(picks, vec![PICK], "{calls:?}");
        for forbidden in [
            "--no-gpg-sign",
            "--no-verify",
            "--strategy",
            "-X",
            "--skip",
            "--continue",
            "--quit",
            "--mainline",
            " -m ",
            "--allow-empty",
            "--keep-redundant-commits",
            "--empty",
            "reset",
            "rebase",
        ] {
            assert!(
                !calls.iter().any(|c| c.contains(forbidden)),
                "`{forbidden}` must never be run: {calls:?}"
            );
        }
    }

    #[test]
    fn config_is_reconciled_before_anything_is_picked() {
        let runner = runner();
        cherry_pick(&runner, &request()).unwrap();
        let calls = runner.calls();
        let config = calls
            .iter()
            .position(|c| c.starts_with("git config"))
            .unwrap();
        let pick = calls.iter().position(|c| c == PICK).unwrap();
        assert!(config < pick, "{calls:?}");
    }

    #[test]
    fn without_a_signing_key_nothing_is_picked() {
        let runner = runner().with(
            "gpg --list-secret-keys --with-colons KEYID",
            2,
            "",
            "no secret key\n",
        );
        let err = cherry_pick(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::SigningUnavailable { .. }), "{err}");
        assert!(!runner.calls().iter().any(|c| c.contains("cherry-pick")));
    }

    #[test]
    fn a_conflict_is_aborted_and_reported_never_resolved() {
        let runner = conflicted(runner());
        let err = cherry_pick(&runner, &request()).unwrap_err();
        let Error::CherryPickConflict { commit, files, .. } = &err else {
            panic!("expected a conflict, got {err}");
        };
        assert_eq!(commit, "aaaa1111");
        assert_eq!(files, &vec!["src/lib.rs".to_owned()]);
        let message = err.to_string();
        assert!(message.contains("back where it started"), "{message}");
        assert!(message.contains("let the author decide"), "{message}");

        let calls = runner.calls();
        let picks: Vec<_> = calls
            .iter()
            .filter(|c| c.contains("cherry-pick "))
            .collect();
        assert_eq!(picks, vec![PICK, "git cherry-pick --abort"], "{calls:?}");
    }

    #[test]
    fn an_abort_that_does_not_restore_the_branch_is_reported_as_stuck() {
        let runner = conflicted(runner()).with("git cherry-pick --abort", 128, "", "boom\n");
        let err = cherry_pick(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::CherryPickStuck { .. }), "{err}");
        assert!(err.to_string().contains("abc1234"), "{err}");
    }

    #[test]
    fn a_locked_key_is_a_signing_failure_and_is_aborted() {
        let runner = runner()
            .with(PICK, 1, "", "error: gpg failed to sign the data\n")
            .with(
                "git rev-parse --verify --quiet CHERRY_PICK_HEAD",
                0,
                "aaaa1111\n",
                "",
            )
            .with("git diff --name-only --diff-filter=U", 0, "", "")
            .with("git cherry-pick --abort", 0, "", "");
        let err = cherry_pick(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::SigningFailed { .. }), "{err}");
        assert!(
            runner
                .calls()
                .contains(&"git cherry-pick --abort".to_owned())
        );
    }

    #[test]
    fn someone_elses_unfinished_operation_is_left_alone() {
        for (state, operation) in IN_PROGRESS {
            let runner = runner().with_path(&format!("/repo/.git/{state}"), true, false);
            let err = cherry_pick(&runner, &request()).unwrap_err();
            assert!(
                matches!(&err, Error::OperationInProgress { operation: o } if o == operation),
                "{state}: {err}"
            );
            assert!(
                !runner.calls().iter().any(|c| c.contains("cherry-pick")),
                "{state}: {:?}",
                runner.calls()
            );
        }
    }

    #[test]
    fn a_dirty_work_tree_stops_the_pick_before_it_starts() {
        let runner = runner().with(
            "git status --porcelain --untracked-files=no",
            0,
            " M src/lib.rs\n",
            "",
        );
        let err = cherry_pick(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::WorkTreeDirty { .. }), "{err}");
        assert!(!runner.calls().iter().any(|c| c.contains("cherry-pick")));
    }

    #[test]
    fn a_merge_commit_is_refused() {
        let runner = runner().with(
            "git rev-list --parents -n 1 aaaa1111",
            0,
            "aaaa1111 pppp0000 qqqq0000\n",
            "",
        );
        let err = cherry_pick(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        assert!(err.to_string().contains("merge commit"), "{err}");
        assert!(!runner.calls().iter().any(|c| c.contains("cherry-pick")));
    }

    #[test]
    fn an_unknown_commit_is_refused_without_picking() {
        let runner = runner().with("git rev-parse --verify --quiet topic~1^{commit}", 1, "", "");
        let err = cherry_pick(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::UnknownRef { .. }), "{err}");
        assert!(!runner.calls().iter().any(|c| c.contains("cherry-pick")));
    }

    #[test]
    fn anything_that_is_not_plainly_a_commit_is_refused() {
        for bad in [
            vec![],
            vec![""],
            vec!["--strategy=ours"],
            vec!["-Xtheirs"],
            vec!["main..topic"],
            vec!["a b"],
            vec!["topic", "--continue"],
        ] {
            let request = CherryPickRequest {
                commits: bad.iter().map(|s| (*s).to_owned()).collect(),
                cwd: None,
            };
            let runner = runner();
            let err = cherry_pick(&runner, &request).unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest(_)),
                "{bad:?} should be refused, got {err}"
            );
            assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        }
    }

    #[test]
    fn picking_outside_a_repository_is_refused() {
        let err = cherry_pick(&ScriptedRunner::new(), &request()).unwrap_err();
        assert!(matches!(err, Error::NotARepository), "{err}");
    }
}
