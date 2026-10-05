//! Discarding local changes to specific paths, with a recoverable safety net.
//!
//! `git checkout -- <path>` destroys uncommitted work and there is no reflog for the
//! working tree, so this module always saves the pre-restore diff as a patch first. The
//! net is not optional and there is no parameter that turns it off: if the patch cannot
//! be written, nothing is restored.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, runner::CommandRunner, status::SigningStatus, trimmed};

/// Paths whose local modifications should be thrown away.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    /// Explicit paths to restore from `HEAD`. Required: there is no "restore everything".
    pub files: Vec<String>,
    /// Repository to work in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreOutcome {
    pub restored: Vec<String>,
    /// Where the discarded diff was saved, when there was anything to discard.
    pub backup: Option<String>,
    /// Paths that had no local changes, so nothing was discarded for them.
    pub unchanged: Vec<String>,
    pub command: String,
}

/// Restore `files` to their committed state, saving whatever is discarded.
pub fn restore(runner: &dyn CommandRunner, request: &RestoreRequest) -> Result<RestoreOutcome> {
    let cwd = request.cwd.as_deref();

    if request.files.is_empty() {
        return Err(Error::InvalidRequest(
            "pass the paths to restore; this tool never restores the whole tree".into(),
        ));
    }
    for file in &request.files {
        check_pathspec(file)?;
    }

    let status = SigningStatus::read(runner, cwd)?;
    let repository = status.require_repository()?.to_owned();

    // What is about to be lost, staged and unstaged alike.
    let mut pathspec: Vec<&str> = vec!["diff", "HEAD", "--"];
    pathspec.extend(request.files.iter().map(String::as_str));
    let diff = runner.run("git", &pathspec, cwd)?;
    if !diff.ok() {
        return Err(Error::RestoreFailed {
            detail: trimmed(&diff.stderr),
        });
    }

    let backup = if diff.stdout.trim().is_empty() {
        None
    } else {
        Some(save_patch(runner, cwd, &repository, &diff.stdout)?)
    };

    let mut args: Vec<&str> = vec!["restore", "--source=HEAD", "--staged", "--worktree", "--"];
    args.extend(request.files.iter().map(String::as_str));
    let command = format!("git {}", args.join(" "));
    let out = runner.run("git", &args, cwd)?;
    if !out.ok() {
        return Err(Error::RestoreFailed {
            detail: trimmed(&out.stderr),
        });
    }

    let changed = changed_paths(&diff.stdout);
    let unchanged = request
        .files
        .iter()
        .filter(|f| !changed.iter().any(|c| c == *f))
        .cloned()
        .collect();

    Ok(RestoreOutcome {
        restored: request.files.clone(),
        backup,
        unchanged,
        command,
    })
}

/// Reject anything that is not plainly a path inside this repository. A pathspec that
/// git could read as a revision, an option, or an escape upwards never reaches git.
fn check_pathspec(file: &str) -> Result<()> {
    let reject = |why: &str| {
        Err(Error::InvalidRequest(format!(
            "`{file}` is not a usable path: {why}"
        )))
    };
    if file.trim().is_empty() {
        return reject("it is empty");
    }
    if file.starts_with('-') {
        return reject("it would be read as an option");
    }
    if file.starts_with(':') {
        return reject("magic pathspecs are not accepted");
    }
    if Path::new(file).is_absolute() {
        return reject("it is absolute; use a path relative to the repository");
    }
    if file.split(['/', '\\']).any(|part| part == "..") {
        return reject("it escapes the repository");
    }
    if matches!(file.trim_end_matches('/'), "" | ".") {
        return reject("it is the whole tree; name the files you mean");
    }
    if file.contains('\0') || file.contains('\n') {
        return reject("it contains a control character");
    }
    Ok(())
}

/// Save the diff under the git directory, where it survives a `clean` of the work tree.
fn save_patch(
    runner: &dyn CommandRunner,
    cwd: Option<&Path>,
    repository: &str,
    diff: &str,
) -> Result<String> {
    let git_dir = runner
        .run("git", &["rev-parse", "--absolute-git-dir"], cwd)?
        .value()
        .unwrap_or_else(|| format!("{}/.git", repository.trim_end_matches('/')));

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let path = PathBuf::from(&git_dir)
        .join("gitops-mcp")
        .join(format!("restore-{stamp}.patch"));

    // No net, no restore.
    runner
        .write_file(&path, diff)
        .map_err(|source| Error::BackupFailed {
            path: path.display().to_string(),
            source,
        })?;
    Ok(path.display().to_string())
}

/// Paths named in a unified diff, read off the `+++ b/<path>` lines.
fn changed_paths(diff: &str) -> Vec<String> {
    diff.lines()
        .filter_map(|line| line.strip_prefix("+++ b/"))
        .map(|p| p.trim().to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{commit::tests::compliant_runner, runner::ScriptedRunner};

    const DIFF: &str = "diff --git a/spec.ts b/spec.ts\n--- a/spec.ts\n+++ b/spec.ts\n-old\n+new\n";

    fn runner() -> ScriptedRunner {
        compliant_runner()
            .with("git rev-parse --absolute-git-dir", 0, "/repo/.git\n", "")
            .with("git diff HEAD -- spec.ts", 0, DIFF, "")
            .with(
                "git restore --source=HEAD --staged --worktree -- spec.ts",
                0,
                "",
                "",
            )
    }

    fn request() -> RestoreRequest {
        RestoreRequest {
            files: vec!["spec.ts".into()],
            cwd: None,
        }
    }

    #[test]
    fn discarded_changes_are_saved_before_anything_is_restored() {
        let runner = runner();
        let outcome = restore(&runner, &request()).unwrap();

        let writes = runner.writes();
        assert_eq!(writes.len(), 1, "exactly one recovery patch");
        let (path, contents) = &writes[0];
        assert_eq!(contents, DIFF, "the patch holds the whole diff");
        assert!(
            path.starts_with("/repo/.git/gitops-mcp"),
            "patch belongs under the git dir, which `git clean` does not touch: {path:?}"
        );
        assert_eq!(outcome.backup.as_deref(), Some(path.to_str().unwrap()));

        // Ordering is the guarantee: the patch exists before the work is destroyed.
        let calls = runner.calls();
        let diff = calls
            .iter()
            .position(|c| c.starts_with("git diff HEAD"))
            .unwrap();
        let restored = calls
            .iter()
            .position(|c| c.starts_with("git restore"))
            .unwrap();
        assert!(diff < restored, "{calls:?}");
    }

    #[test]
    fn nothing_is_restored_when_the_patch_cannot_be_written() {
        let runner = runner().failing_writes();
        let err = restore(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::BackupFailed { .. }), "{err}");
        assert!(
            !runner.calls().iter().any(|c| c.starts_with("git restore")),
            "the restore must not run: {:?}",
            runner.calls()
        );
        assert!(err.to_string().contains("still there"), "{err}");
    }

    #[test]
    fn a_clean_path_needs_no_patch() {
        let runner = runner().with("git diff HEAD -- spec.ts", 0, "", "");
        let outcome = restore(&runner, &request()).unwrap();
        assert!(outcome.backup.is_none());
        assert!(runner.writes().is_empty());
        assert_eq!(outcome.unchanged, vec!["spec.ts".to_owned()]);
    }

    #[test]
    fn the_whole_tree_is_never_restorable() {
        for files in [vec![], vec![".".to_owned()], vec!["./".to_owned()]] {
            let request = RestoreRequest { files, cwd: None };
            let err = restore(&runner(), &request).unwrap_err();
            assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        }
    }

    #[test]
    fn a_pathspec_that_is_not_plainly_a_path_is_refused() {
        for bad in [
            "-f",
            "--force",
            ":(exclude)src",
            ":/",
            "/etc/passwd",
            "../../secrets",
            "src/../../etc",
        ] {
            let request = RestoreRequest {
                files: vec![bad.to_owned()],
                cwd: None,
            };
            let runner = runner();
            let err = restore(&runner, &request).unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest(_)),
                "`{bad}` should be refused, got {err}"
            );
            // Refused before git is handed anything at all.
            assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        }
    }

    #[test]
    fn restoring_outside_a_repository_is_refused() {
        let runner = ScriptedRunner::new();
        let err = restore(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::NotARepository), "{err}");
    }
}
