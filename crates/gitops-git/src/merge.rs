//! Fast-forward-only merges.
//!
//! The safe merge is the one that cannot lose a commit or invent a resolution. This
//! module only ever runs `git merge --ff-only`. When the branches have diverged there is
//! no fallback: no real merge, no rebase, no reset. There is no parameter that selects
//! one, because deciding how diverged history should be reconciled is not an automatic
//! call.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Error, Result, runner::CommandRunner, status::SigningStatus, trimmed};

/// The ref to fast-forward onto the current branch.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeRequest {
    /// Branch, tag or commit to fast-forward to.
    pub r#ref: String,
    /// Repository to work in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeOutcome {
    pub branch: Option<String>,
    pub merged: String,
    pub before: Option<String>,
    pub after: Option<String>,
    /// True when `ref` was already an ancestor of HEAD ("Already up to date"), so
    /// nothing moved.
    pub already_current: bool,
    pub command: String,
    pub output: String,
}

/// Fast-forward the current branch to `ref`, or refuse.
pub fn merge_ff_only(runner: &dyn CommandRunner, request: &MergeRequest) -> Result<MergeOutcome> {
    let cwd = request.cwd.as_deref();
    let target = request.r#ref.trim();

    check_ref(target)?;

    let status = SigningStatus::read(runner, cwd)?;
    status.require_repository()?;

    // git itself aborts a merge when the index or tracked files differ from HEAD; doing
    // the check up front turns a half-applied merge into a clear refusal. Untracked files
    // never block a fast-forward, so they are not counted as dirty.
    let dirty = runner.run(
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
        cwd,
    )?;
    if dirty.ok() && !dirty.stdout.trim().is_empty() {
        return Err(Error::WorkTreeDirty {
            action: "merge".into(),
            detail: trimmed(&dirty.stdout),
        });
    }

    let resolved = runner.run(
        "git",
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{target}^{{commit}}"),
        ],
        cwd,
    )?;
    if !resolved.ok() {
        return Err(Error::UnknownRef {
            reference: target.to_owned(),
        });
    }

    let branch = runner
        .run("git", &["rev-parse", "--abbrev-ref", "HEAD"], cwd)?
        .value();
    let before = runner.run("git", &["rev-parse", "HEAD"], cwd)?.value();

    let args = ["merge", "--ff-only", target];
    let command = format!("git {}", args.join(" "));
    let out = runner.run("git", &args, cwd)?;
    if !out.ok() {
        let detail = combined(&out.stdout, &out.stderr);
        if refused_fast_forward(&detail) {
            return Err(Error::FastForwardRefused {
                reference: target.to_owned(),
                branch: branch.clone().unwrap_or_else(|| "HEAD".into()),
                detail,
            });
        }
        return Err(Error::MergeFailed { detail });
    }

    let after = runner.run("git", &["rev-parse", "HEAD"], cwd)?.value();
    let output = trimmed(&out.stdout);
    Ok(MergeOutcome {
        branch,
        merged: target.to_owned(),
        // git says so itself; the commit ids only agree by coincidence on an empty merge.
        already_current: output.to_ascii_lowercase().contains("already up to date"),
        before,
        after,
        command,
        output,
    })
}

fn check_ref(target: &str) -> Result<()> {
    let reject = |why: &str| {
        Err(Error::InvalidRequest(format!(
            "`{target}` is not a usable ref: {why}"
        )))
    };
    if target.is_empty() {
        return reject("it is empty");
    }
    if target.starts_with('-') {
        return reject("it would be read as an option");
    }
    if target.contains(char::is_whitespace) {
        return reject("it contains whitespace");
    }
    if target.contains("..") {
        return reject("a range is not a merge source");
    }
    Ok(())
}

fn refused_fast_forward(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    [
        "not possible to fast-forward",
        "not a fast-forward",
        "diverge",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(crate) fn combined(stdout: &str, stderr: &str) -> String {
    [trimmed(stdout), trimmed(stderr)]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{commit::tests::compliant_runner, runner::ScriptedRunner};

    fn runner() -> ScriptedRunner {
        compliant_runner()
            .with("git status --porcelain --untracked-files=no", 0, "", "")
            .with(
                "git rev-parse --verify --quiet topic^{commit}",
                0,
                "dead\n",
                "",
            )
            .with("git rev-parse --abbrev-ref HEAD", 0, "main\n", "")
            .with("git rev-parse HEAD", 0, "beef\n", "")
            .with("git merge --ff-only topic", 0, "Fast-forward\n", "")
    }

    fn request() -> MergeRequest {
        MergeRequest {
            r#ref: "topic".into(),
            cwd: None,
        }
    }

    #[test]
    fn a_fast_forward_is_the_only_merge_performed() {
        let runner = runner();
        let outcome = merge_ff_only(&runner, &request()).unwrap();
        assert_eq!(outcome.command, "git merge --ff-only topic");
        assert_eq!(outcome.branch.as_deref(), Some("main"));

        let calls = runner.calls();
        let merges: Vec<_> = calls.iter().filter(|c| c.contains("merge")).collect();
        assert_eq!(merges, vec!["git merge --ff-only topic"], "{calls:?}");
        for forbidden in ["--no-ff", "rebase", "reset", "--squash", "--strategy"] {
            assert!(
                !calls.iter().any(|c| c.contains(forbidden)),
                "`{forbidden}` must never be run: {calls:?}"
            );
        }
    }

    #[test]
    fn divergence_is_reported_and_never_reconciled() {
        let runner = runner().with(
            "git merge --ff-only topic",
            128,
            "",
            "fatal: Not possible to fast-forward, aborting.\n",
        );
        let err = merge_ff_only(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::FastForwardRefused { .. }), "{err}");
        let message = err.to_string();
        assert!(message.contains("diverged"), "{message}");
        assert!(message.contains("let the author"), "{message}");

        // The refusal is the end of it: no second attempt by another route.
        let calls = runner.calls();
        assert_eq!(
            calls.iter().filter(|c| c.contains("merge")).count(),
            1,
            "{calls:?}"
        );
        for forbidden in ["--no-ff", "rebase", "reset", "--force"] {
            assert!(!calls.iter().any(|c| c.contains(forbidden)), "{calls:?}");
        }
    }

    #[test]
    fn a_dirty_work_tree_stops_the_merge_before_it_starts() {
        let runner = runner().with(
            "git status --porcelain --untracked-files=no",
            0,
            " M src/lib.rs\n",
            "",
        );
        let err = merge_ff_only(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::WorkTreeDirty { .. }), "{err}");
        assert!(err.to_string().contains("Commit or stash"), "{err}");
        assert!(
            !runner.calls().iter().any(|c| c.contains("merge")),
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn untracked_files_do_not_block_a_fast_forward() {
        // The dirty check asks git to ignore untracked files, so build output or a stray
        // scratch file never stands in the way of a merge that cannot touch it.
        let runner = runner();
        let outcome = merge_ff_only(&runner, &request()).unwrap();
        assert!(!outcome.already_current);
        assert_eq!(outcome.output, "Fast-forward");
        assert!(
            runner
                .calls()
                .contains(&"git status --porcelain --untracked-files=no".to_owned()),
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn an_unknown_ref_is_refused_without_merging() {
        let runner = runner().with("git rev-parse --verify --quiet topic^{commit}", 1, "", "");
        let err = merge_ff_only(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::UnknownRef { .. }), "{err}");
        assert!(!runner.calls().iter().any(|c| c.contains("merge")));
    }

    #[test]
    fn a_ref_that_is_not_plainly_a_ref_is_refused() {
        for bad in ["", "-f", "--no-ff", "main..topic", "a b"] {
            let request = MergeRequest {
                r#ref: bad.into(),
                cwd: None,
            };
            let runner = runner();
            let err = merge_ff_only(&runner, &request).unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest(_)),
                "`{bad}` should be refused, got {err}"
            );
            assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        }
    }

    #[test]
    fn an_already_current_branch_is_not_an_error() {
        let runner = runner().with("git merge --ff-only topic", 0, "Already up to date.\n", "");
        let outcome = merge_ff_only(&runner, &request()).unwrap();
        assert!(outcome.already_current);
    }

    #[test]
    fn merging_outside_a_repository_is_refused() {
        let err = merge_ff_only(&ScriptedRunner::new(), &request()).unwrap_err();
        assert!(matches!(err, Error::NotARepository), "{err}");
    }
}
