//! Creating signed commits, with signing governance applied first.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    Error, Result,
    governance::{Reconciliation, reconcile},
    runner::CommandRunner,
    trimmed,
};

/// What to commit, and how.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CommitRequest {
    /// Commit message. The first line is the subject.
    pub message: String,
    /// Explicit paths to stage. Mutually exclusive with `all`.
    #[serde(default)]
    pub files: Vec<String>,
    /// Stage every change in the working tree, including untracked files.
    #[serde(default)]
    pub all: bool,
    /// Allow an unsigned commit when no usable signing key exists. Off by default:
    /// an unsigned commit is a governance failure, not a fallback.
    #[serde(default)]
    pub allow_unsigned: bool,
    /// Repository to work in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// Outcome of a commit attempt.
#[derive(Debug, Clone, Serialize)]
pub struct CommitOutcome {
    /// Config corrections applied before committing.
    pub governance: Reconciliation,
    pub staged: Vec<String>,
    pub signed: bool,
    pub commit: Option<String>,
    pub subject: String,
    /// `%G?` from git: `G` good, `U` good but untrusted, `N` none, `B` bad.
    pub signature_status: Option<String>,
    pub command: String,
    pub output: String,
}

/// Stage the requested paths and create a signed commit.
pub fn commit(runner: &dyn CommandRunner, request: &CommitRequest) -> Result<CommitOutcome> {
    let cwd = request.cwd.as_deref();

    if request.message.trim().is_empty() {
        return Err(Error::InvalidRequest("commit message is empty".into()));
    }
    if request.all && !request.files.is_empty() {
        return Err(Error::InvalidRequest(
            "pass either `files` or `all: true`, not both".into(),
        ));
    }
    if !request.all && request.files.is_empty() {
        return Err(Error::InvalidRequest(
            "pass `files` to stage specific paths, or `all: true` to stage everything".into(),
        ));
    }

    // Governance first: a commit must never be created under config some other tool bent.
    let governance = reconcile(runner, cwd, false)?;
    governance.status.require_repository()?;

    let signing_available = governance.status.signing_available;
    if !signing_available && !request.allow_unsigned {
        return Err(Error::SigningUnavailable {
            detail: if governance.unresolved.is_empty() {
                "no usable signing key".to_owned()
            } else {
                governance.unresolved.join("; ")
            },
        });
    }

    let staged = stage(runner, cwd, request)?;

    // `git diff --cached --quiet` exits 1 when something is staged.
    if runner
        .run("git", &["diff", "--cached", "--quiet"], cwd)?
        .ok()
    {
        return Err(Error::NothingToCommit);
    }

    let mut args: Vec<&str> = vec!["commit"];
    if signing_available {
        args.push("-S");
    }
    args.extend_from_slice(&["-m", request.message.as_str()]);
    let command = format!("git {}", args.join(" "));

    let out = runner.run("git", &args, cwd)?;
    if !out.ok() {
        let stderr = trimmed(&out.stderr);
        if looks_like_signing_failure(&stderr) {
            return Err(Error::SigningFailed { detail: stderr });
        }
        // Everything the commit printed, because a hook's complaint is the actionable
        // part and git puts it on either stream.
        let combined = [trimmed(&out.stdout), stderr.clone()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        // A live pre-commit hook is by far the likeliest reason a staged, signable commit
        // is refused, and misreading some other failure as a hook failure still points at
        // the right output — whereas missing a hook failure invites a `--no-verify` retry.
        if governance.status.hooks.pre_commit_active() {
            return Err(Error::HookRejected {
                action: "commit".into(),
                detail: combined,
            });
        }
        return Err(Error::CommitFailed { detail: combined });
    }

    let commit_id = runner.run("git", &["rev-parse", "HEAD"], cwd)?.value();
    let signature_status = runner
        .run("git", &["log", "-1", "--format=%G?"], cwd)?
        .value();

    Ok(CommitOutcome {
        governance,
        staged,
        signed: signing_available,
        commit: commit_id,
        subject: request.message.lines().next().unwrap_or("").to_owned(),
        signature_status,
        command,
        output: trimmed(&out.stdout),
    })
}

fn stage(
    runner: &dyn CommandRunner,
    cwd: Option<&Path>,
    request: &CommitRequest,
) -> Result<Vec<String>> {
    if request.all {
        let out = runner.run("git", &["add", "-A"], cwd)?;
        if !out.ok() {
            return Err(Error::StageFailed {
                detail: trimmed(&out.stderr),
            });
        }
        let listed = runner
            .run("git", &["diff", "--cached", "--name-only"], cwd)?
            .stdout;
        return Ok(listed.lines().map(str::to_owned).collect());
    }

    let mut args: Vec<&str> = vec!["add", "--"];
    args.extend(request.files.iter().map(String::as_str));
    let out = runner.run("git", &args, cwd)?;
    if !out.ok() {
        return Err(Error::StageFailed {
            detail: trimmed(&out.stderr),
        });
    }
    Ok(request.files.clone())
}

pub(crate) fn looks_like_signing_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "gpg failed to sign",
        "secret key not available",
        "no secret key",
        "signing failed",
        "inappropriate ioctl",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    pub(crate) fn compliant_runner() -> ScriptedRunner {
        ScriptedRunner::new()
            .with("git rev-parse --show-toplevel", 0, "/repo\n", "")
            .with("git config commit.gpgsign", 0, "true\n", "")
            .with("git config --global commit.gpgsign", 0, "true\n", "")
            .with("git config --local commit.gpgsign", 1, "", "")
            .with("git config user.signingkey", 0, "KEYID\n", "")
            .with("git config --global user.signingkey", 0, "KEYID\n", "")
            .with("git config --local user.signingkey", 1, "", "")
            .with("git config gpg.format", 0, "openpgp\n", "")
            .with("git config --global gpg.format", 0, "openpgp\n", "")
            .with("git config --local gpg.format", 1, "", "")
            .with("git config user.email", 0, "me@example.com\n", "")
            .with("git config --global user.email", 0, "me@example.com\n", "")
            .with("git config --local user.email", 1, "", "")
            .with("git config user.name", 0, "Michael Bruno\n", "")
            .with("git config --global user.name", 0, "Michael Bruno\n", "")
            .with("git config --local user.name", 1, "", "")
            .with(
                "gpg --list-keys --with-colons KEYID",
                0,
                "uid:u::::0::ABC::Michael Bruno <me@example.com>::::::::::0:\n",
                "",
            )
            .with(
                "gpg --list-secret-keys --with-colons KEYID",
                0,
                "sec:u:\n",
                "",
            )
            .with("git diff --cached --quiet", 1, "", "")
            .with("git rev-parse HEAD", 0, "abc1234\n", "")
            .with("git log -1 --format=%G?", 0, "G\n", "")
            .with("git config core.hooksPath", 1, "", "")
            .with("git config --global core.hooksPath", 1, "", "")
            .with("git config --local core.hooksPath", 1, "", "")
            .with(
                "git rev-parse --git-path hooks",
                0,
                "/repo/.git/hooks\n",
                "",
            )
            .with_path("/repo/.git/hooks", true, true)
    }

    /// A repository whose `pre-commit` hook is present and executable.
    pub(crate) fn with_live_pre_commit(runner: ScriptedRunner) -> ScriptedRunner {
        runner.with_path("/repo/.git/hooks/pre-commit", true, true)
    }

    #[test]
    fn signs_a_file_list_commit() {
        let runner = compliant_runner()
            .with("git add -- src/lib.rs", 0, "", "")
            .with(
                "git commit -S -m feat: thing",
                0,
                "[main abc1234] feat: thing\n",
                "",
            );
        let outcome = commit(
            &runner,
            &CommitRequest {
                message: "feat: thing".into(),
                files: vec!["src/lib.rs".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(outcome.signed);
        assert_eq!(outcome.staged, vec!["src/lib.rs".to_owned()]);
        assert_eq!(outcome.signature_status.as_deref(), Some("G"));
        assert_eq!(outcome.command, "git commit -S -m feat: thing");
    }

    #[test]
    fn all_flag_stages_everything() {
        let runner = compliant_runner()
            .with("git add -A", 0, "", "")
            .with("git diff --cached --name-only", 0, "a.rs\nb.rs\n", "")
            .with("git commit -S -m chore: sweep", 0, "", "");
        let outcome = commit(
            &runner,
            &CommitRequest {
                message: "chore: sweep".into(),
                all: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(outcome.staged, vec!["a.rs".to_owned(), "b.rs".to_owned()]);
    }

    #[test]
    fn rejects_files_and_all_together() {
        let err = commit(
            &compliant_runner(),
            &CommitRequest {
                message: "m".into(),
                files: vec!["a".into()],
                all: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[test]
    fn rejects_neither_files_nor_all() {
        let err = commit(
            &compliant_runner(),
            &CommitRequest {
                message: "m".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[test]
    fn refuses_to_commit_unsigned_without_opt_in() {
        let runner = compliant_runner().with(
            "gpg --list-secret-keys --with-colons KEYID",
            2,
            "",
            "no secret key\n",
        );
        let err = commit(
            &runner,
            &CommitRequest {
                message: "m".into(),
                all: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::SigningUnavailable { .. }), "{err}");
        // Nothing was staged and no commit was attempted.
        assert!(!runner.calls().iter().any(|c| c.starts_with("git commit")));
    }

    #[test]
    fn a_locked_key_is_a_signing_failure_not_an_unsigned_commit() {
        let runner = compliant_runner()
            .with("git add -A", 0, "", "")
            .with("git diff --cached --name-only", 0, "a.rs\n", "")
            .with(
                "git commit -S -m m",
                128,
                "",
                "error: gpg failed to sign the data\nfatal: failed to write commit object\n",
            );
        let err = commit(
            &runner,
            &CommitRequest {
                message: "m".into(),
                all: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::SigningFailed { .. }), "{err}");
        assert!(!runner.calls().iter().any(|c| c.contains("--no-gpg-sign")));
    }

    #[test]
    fn nothing_staged_is_an_error() {
        let runner = compliant_runner().with("git add -- a.rs", 0, "", "").with(
            "git diff --cached --quiet",
            0,
            "",
            "",
        );
        let err = commit(
            &runner,
            &CommitRequest {
                message: "m".into(),
                files: vec!["a.rs".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::NothingToCommit), "{err}");
    }

    #[test]
    fn config_is_repaired_before_the_commit_is_made() {
        let runner = compliant_runner()
            .with("git config user.email", 0, "llm@nowhere.invalid\n", "")
            .with(
                "git config --global user.email",
                0,
                "llm@nowhere.invalid\n",
                "",
            )
            .with("git config --global user.email me@example.com", 0, "", "")
            .with("git add -A", 0, "", "")
            .with("git diff --cached --name-only", 0, "a.rs\n", "")
            .with("git commit -S -m m", 0, "", "");
        let outcome = commit(
            &runner,
            &CommitRequest {
                message: "m".into(),
                all: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(outcome.governance.changed());
        let calls = runner.calls();
        let fix = calls
            .iter()
            .position(|c| c == "git config --global user.email me@example.com")
            .expect("email was repaired");
        let made = calls
            .iter()
            .position(|c| c.starts_with("git commit"))
            .expect("commit was made");
        assert!(fix < made, "config must be repaired before committing");
    }
}

#[cfg(test)]
mod hook_tests {
    use super::{tests::*, *};

    const HOOK_OUTPUT: &str = "clippy: unused variable `x`\npre-commit hook failed\n";

    fn request() -> CommitRequest {
        CommitRequest {
            message: "m".into(),
            all: true,
            ..Default::default()
        }
    }

    fn staged(runner: crate::runner::ScriptedRunner) -> crate::runner::ScriptedRunner {
        runner
            .with("git add -A", 0, "", "")
            .with("git diff --cached --name-only", 0, "a.rs\n", "")
    }

    #[test]
    fn a_failing_pre_commit_hook_is_reported_as_a_hook_rejection() {
        let runner = with_live_pre_commit(staged(compliant_runner())).with(
            "git commit -S -m m",
            1,
            "",
            HOOK_OUTPUT,
        );
        let err = commit(&runner, &request()).unwrap_err();
        match &err {
            Error::HookRejected { detail, .. } => {
                // The hook's own complaint has to survive, or there is nothing to fix.
                assert!(detail.contains("unused variable"), "{detail}");
            }
            other => panic!("expected a hook rejection, got {other}"),
        }
        let message = err.to_string();
        assert!(message.contains("Fix what the hook reports"), "{message}");
        assert!(message.contains("unrelated to signing"), "{message}");
        // The refusal states the rule without naming the flag that breaks it: spelling
        // out the bypass is how a caller learns the bypass exists.
        assert!(!message.contains("--no-verify"), "{message}");
    }

    #[test]
    fn a_hook_rejection_never_retries_or_bypasses_anything() {
        let runner = with_live_pre_commit(staged(compliant_runner())).with(
            "git commit -S -m m",
            1,
            "",
            HOOK_OUTPUT,
        );
        commit(&runner, &request()).unwrap_err();
        let calls = runner.calls();
        assert!(
            !calls.iter().any(|c| c.contains("--no-verify")),
            "{calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("--no-gpg-sign")),
            "{calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("commit.gpgsign false")),
            "{calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("core.hooksPath")
                && (c.contains("--unset") || c.contains("/dev/null"))),
            "{calls:?}"
        );
        // Exactly one commit attempt: a rejection is not retried.
        assert_eq!(
            calls.iter().filter(|c| c.starts_with("git commit")).count(),
            1,
            "{calls:?}"
        );
    }

    #[test]
    fn without_a_live_hook_a_failure_stays_a_plain_commit_failure() {
        let runner = staged(compliant_runner()).with(
            "git commit -S -m m",
            1,
            "",
            "fatal: cannot lock ref HEAD\n",
        );
        let err = commit(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::CommitFailed { .. }), "{err}");
    }

    #[test]
    fn a_signing_failure_outranks_the_hook_explanation() {
        let runner = with_live_pre_commit(staged(compliant_runner())).with(
            "git commit -S -m m",
            128,
            "",
            "error: gpg failed to sign the data\n",
        );
        let err = commit(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::SigningFailed { .. }), "{err}");
    }

    #[test]
    fn a_non_executable_pre_commit_hook_is_drift() {
        let runner = compliant_runner().with_path("/repo/.git/hooks/pre-commit", true, false);
        let status = crate::SigningStatus::read(&runner, None).unwrap();
        assert!(!status.hooks.pre_commit_active());
        assert!(
            status
                .drift
                .iter()
                .any(|d| matches!(d, crate::Drift::HookNotExecutable { .. })),
            "{:?}",
            status.drift
        );
    }

    #[test]
    fn a_hooks_path_pointing_nowhere_is_drift() {
        let runner = compliant_runner()
            .with("git config core.hooksPath", 0, "/repo/.nope\n", "")
            .with("git config --global core.hooksPath", 1, "", "")
            .with("git config --local core.hooksPath", 0, "/repo/.nope\n", "")
            .with("git rev-parse --git-path hooks", 0, "/repo/.nope\n", "");
        let status = crate::SigningStatus::read(&runner, None).unwrap();
        assert!(!status.hooks.directory_exists);
        assert!(
            status
                .drift
                .iter()
                .any(|d| matches!(d, crate::Drift::HooksDirectoryMissing { .. })),
            "{:?}",
            status.drift
        );
    }
}
