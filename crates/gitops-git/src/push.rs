//! Publishing the current branch.
//!
//! Pushing is the only operation here that other people can see, and the only one this
//! server cannot undo. So it does exactly one thing: fast-forward the current branch's
//! own ref on the named remote. Everything that can rewrite or remove published history
//! -- `--force`, `--force-with-lease`, a `+refspec`, `--delete`, `--mirror`, `--prune` --
//! is absent from the request type, not merely defaulted off, so there is no knob to
//! reach for. The refspec is always written out in full for the same reason: it stops a
//! `remote.<name>.push` or `push.default` setting from redirecting the push somewhere the
//! caller did not name.
//!
//! It is also where signing is enforced for real. A commit tool can only promise that the
//! commits it creates are signed; this refuses to publish a branch containing an unsigned
//! commit whoever made it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    Error, Result, merge::combined, runner::CommandRunner, status::SigningStatus, trimmed,
};

/// Where to publish the current branch.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushRequest {
    /// Remote to push to. Defaults to `origin`.
    #[serde(default)]
    pub remote: Option<String>,
    /// Repository to work in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// One commit on the branch, with git's verdict on its signature.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CommitSignature {
    pub commit: String,
    /// `%G?`: `G` good, `U` good but untrusted, `N` none, `B` bad, `E`/`X`/`Y`/`R` other.
    pub verdict: String,
    pub subject: String,
}

impl CommitSignature {
    /// Only a verifiable signature counts. `U` is a good signature from a key this
    /// machine does not trust, which is a trust-store gap rather than an unsigned commit.
    fn signed(&self) -> bool {
        matches!(self.verdict.as_str(), "G" | "U")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PushOutcome {
    pub remote: String,
    pub branch: String,
    /// Commits this push published, newest first.
    pub published: Vec<CommitSignature>,
    /// True when the branch did not exist on the remote and tracking was set up.
    pub created_remote_branch: bool,
    pub up_to_date: bool,
    pub command: String,
    pub output: String,
}

/// Push the current branch to `remote`, refusing anything unsigned.
pub fn push(runner: &dyn CommandRunner, request: &PushRequest) -> Result<PushOutcome> {
    let cwd = request.cwd.as_deref();
    let remote = request.remote.as_deref().unwrap_or("origin").trim();

    check_remote(remote)?;

    let status = SigningStatus::read(runner, cwd)?;
    status.require_repository()?;

    let branch = runner
        .run("git", &["rev-parse", "--abbrev-ref", "HEAD"], cwd)?
        .value()
        .ok_or(Error::NotARepository)?;
    if branch == "HEAD" {
        return Err(Error::DetachedHead);
    }

    if !runner.run("git", &["remote", "get-url", remote], cwd)?.ok() {
        return Err(Error::UnknownRemote {
            remote: remote.to_owned(),
        });
    }

    // What this push would publish: everything on the branch the remote does not have.
    let tracking = format!("{remote}/{branch}");
    let remote_has_branch = runner
        .run(
            "git",
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{tracking}^{{commit}}"),
            ],
            cwd,
        )?
        .ok();
    let range = if remote_has_branch {
        vec![format!("{tracking}..HEAD")]
    } else {
        vec![
            "HEAD".to_owned(),
            "--not".to_owned(),
            format!("--remotes={remote}"),
        ]
    };
    let mut log_args = vec!["log", "--format=%H %G? %s"];
    log_args.extend(range.iter().map(String::as_str));
    let log = runner.run("git", &log_args, cwd)?;
    if !log.ok() {
        return Err(Error::PushFailed {
            detail: trimmed(&log.stderr),
        });
    }
    let published = parse_log(&log.stdout);

    // The governance that matters: an unsigned commit must not become someone else's
    // problem. Refusing here is the whole reason this tool exists.
    let unsigned: Vec<CommitSignature> =
        published.iter().filter(|c| !c.signed()).cloned().collect();
    if !unsigned.is_empty() {
        return Err(Error::UnsignedCommits {
            branch: branch.clone(),
            commits: unsigned,
        });
    }

    if published.is_empty() && remote_has_branch {
        return Ok(PushOutcome {
            remote: remote.to_owned(),
            branch,
            published,
            created_remote_branch: false,
            up_to_date: true,
            command: String::new(),
            output: "Everything up-to-date".into(),
        });
    }

    // Fully qualified on both sides, so no config can redirect it and no `+` can sneak in.
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let mut args: Vec<&str> = vec!["push"];
    if !remote_has_branch {
        args.push("--set-upstream");
    }
    args.extend_from_slice(&[remote, refspec.as_str()]);
    let command = format!("git {}", args.join(" "));

    let out = runner.run("git", &args, cwd)?;
    if !out.ok() {
        let detail = combined(&out.stdout, &out.stderr);
        if rejected_non_fast_forward(&detail) {
            return Err(Error::PushRejected {
                remote: remote.to_owned(),
                branch: branch.clone(),
                detail,
            });
        }
        if status.hooks.pre_push_active() {
            return Err(Error::HookRejected {
                action: "push".into(),
                detail,
            });
        }
        return Err(Error::PushFailed { detail });
    }

    Ok(PushOutcome {
        remote: remote.to_owned(),
        branch,
        published,
        created_remote_branch: !remote_has_branch,
        up_to_date: false,
        command,
        output: combined(&out.stdout, &out.stderr),
    })
}

fn check_remote(remote: &str) -> Result<()> {
    let reject = |why: &str| {
        Err(Error::InvalidRequest(format!(
            "`{remote}` is not a usable remote name: {why}"
        )))
    };
    if remote.is_empty() {
        return reject("it is empty");
    }
    if remote.starts_with('-') {
        return reject("it would be read as an option");
    }
    // A name, never a URL or a refspec: those are how a push reaches an unintended place.
    if remote.contains(char::is_whitespace)
        || remote.contains(':')
        || remote.contains('/')
        || remote.contains('+')
    {
        return reject("name a configured remote, not a URL or refspec");
    }
    Ok(())
}

fn parse_log(stdout: &str) -> Vec<CommitSignature> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.splitn(3, ' ');
            CommitSignature {
                commit: parts.next().unwrap_or_default().to_owned(),
                verdict: parts.next().unwrap_or_default().to_owned(),
                subject: parts.next().unwrap_or_default().to_owned(),
            }
        })
        .collect()
}

fn rejected_non_fast_forward(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    [
        "non-fast-forward",
        "[rejected]",
        "fetch first",
        "behind its remote counterpart",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{commit::tests::compliant_runner, runner::ScriptedRunner};

    const SIGNED_LOG: &str = "aaaa1111 G feat: a thing\nbbbb2222 G fix: another\n";

    fn runner() -> ScriptedRunner {
        compliant_runner()
            .with("git rev-parse --abbrev-ref HEAD", 0, "main\n", "")
            .with(
                "git remote get-url origin",
                0,
                "git@github.com:me/repo.git\n",
                "",
            )
            .with(
                "git rev-parse --verify --quiet origin/main^{commit}",
                0,
                "cafe\n",
                "",
            )
            .with(
                "git log --format=%H %G? %s origin/main..HEAD",
                0,
                SIGNED_LOG,
                "",
            )
            .with(
                "git push origin refs/heads/main:refs/heads/main",
                0,
                "",
                "To github.com:me/repo.git\n   cafe..aaaa  main -> main\n",
            )
    }

    fn request() -> PushRequest {
        PushRequest::default()
    }

    #[test]
    fn a_signed_branch_is_pushed_with_a_fully_qualified_refspec() {
        let runner = runner();
        let outcome = push(&runner, &request()).unwrap();
        assert_eq!(outcome.branch, "main");
        assert_eq!(outcome.remote, "origin");
        assert_eq!(outcome.published.len(), 2);
        // Spelling out both sides stops push.default or remote.origin.push redirecting it.
        assert_eq!(
            outcome.command,
            "git push origin refs/heads/main:refs/heads/main"
        );
    }

    #[test]
    fn an_unsigned_commit_is_never_published() {
        let runner = runner().with(
            "git log --format=%H %G? %s origin/main..HEAD",
            0,
            "aaaa1111 G feat: a thing\nbbbb2222 N chore: snuck in\n",
            "",
        );
        let err = push(&runner, &request()).unwrap_err();
        match &err {
            Error::UnsignedCommits { commits, branch } => {
                assert_eq!(branch, "main");
                assert_eq!(commits.len(), 1);
                assert_eq!(commits[0].commit, "bbbb2222");
            }
            other => panic!("expected unsigned commits, got {other}"),
        }
        // Named precisely enough to go and sign them.
        let message = err.to_string();
        assert!(message.contains("bbbb2222"), "{message}");
        assert!(message.contains("chore: snuck in"), "{message}");
        assert!(
            !runner.calls().iter().any(|c| c.starts_with("git push")),
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn a_bad_signature_is_unsigned_too() {
        for verdict in ["N", "B", "E", "X", "Y", "R"] {
            let runner = runner().with(
                "git log --format=%H %G? %s origin/main..HEAD",
                0,
                &format!("aaaa1111 {verdict} feat: a thing\n"),
                "",
            );
            let err = push(&runner, &request()).unwrap_err();
            assert!(
                matches!(err, Error::UnsignedCommits { .. }),
                "`{verdict}` must not pass as signed, got {err}"
            );
        }
    }

    #[test]
    fn an_untrusted_but_good_signature_still_counts_as_signed() {
        // `U` is a trust-store gap on this machine, not an unsigned commit.
        let runner = runner().with(
            "git log --format=%H %G? %s origin/main..HEAD",
            0,
            "aaaa1111 U feat: a thing\n",
            "",
        );
        assert!(push(&runner, &request()).is_ok());
    }

    #[test]
    fn nothing_this_server_runs_can_overwrite_published_history() {
        let runner = runner();
        push(&runner, &request()).unwrap();
        let calls = runner.calls();
        for forbidden in [
            "--force",
            "--force-with-lease",
            "--force-if-includes",
            "--delete",
            "--mirror",
            "--prune",
            "--no-verify",
            ":refs/heads/main +",
        ] {
            assert!(
                !calls.iter().any(|c| c.contains(forbidden)),
                "`{forbidden}` must never be run: {calls:?}"
            );
        }
        // No `+` prefix on either side of the refspec, which would force that one ref.
        assert!(
            !calls.iter().any(|c| c.contains("push") && c.contains('+')),
            "{calls:?}"
        );
    }

    #[test]
    fn a_rejected_push_is_never_retried_with_force() {
        let runner = runner().with(
            "git push origin refs/heads/main:refs/heads/main",
            1,
            "",
            " ! [rejected]        main -> main (non-fast-forward)\nerror: failed to push some refs\n",
        );
        let err = push(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::PushRejected { .. }), "{err}");
        assert!(err.to_string().contains("never force-pushes"), "{err}");
        assert_eq!(
            runner
                .calls()
                .iter()
                .filter(|c| c.starts_with("git push"))
                .count(),
            1,
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn a_pre_push_hook_rejection_is_the_hooks_problem_to_fix() {
        let runner = runner()
            .with_path("/repo/.git/hooks/pre-push", true, true)
            .with(
                "git push origin refs/heads/main:refs/heads/main",
                1,
                "",
                "tests failed, not pushing\n",
            );
        let err = push(&runner, &request()).unwrap_err();
        match &err {
            Error::HookRejected { action, detail } => {
                assert_eq!(action, "push");
                assert!(detail.contains("tests failed"), "{detail}");
            }
            other => panic!("expected a hook rejection, got {other}"),
        }
        assert!(!err.to_string().contains("--no-verify"), "{err}");
    }

    #[test]
    fn a_new_branch_sets_up_tracking() {
        let runner = runner()
            .with(
                "git rev-parse --verify --quiet origin/main^{commit}",
                1,
                "",
                "",
            )
            .with(
                "git log --format=%H %G? %s HEAD --not --remotes=origin",
                0,
                SIGNED_LOG,
                "",
            )
            .with(
                "git push --set-upstream origin refs/heads/main:refs/heads/main",
                0,
                "",
                "branch 'main' set up to track 'origin/main'\n",
            );
        let outcome = push(&runner, &request()).unwrap();
        assert!(outcome.created_remote_branch);
    }

    #[test]
    fn an_up_to_date_branch_does_not_push() {
        let runner = runner().with("git log --format=%H %G? %s origin/main..HEAD", 0, "", "");
        let outcome = push(&runner, &request()).unwrap();
        assert!(outcome.up_to_date);
        assert!(!runner.calls().iter().any(|c| c.starts_with("git push")));
    }

    #[test]
    fn a_detached_head_has_no_branch_to_push() {
        let runner = runner().with("git rev-parse --abbrev-ref HEAD", 0, "HEAD\n", "");
        let err = push(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::DetachedHead), "{err}");
        assert!(!runner.calls().iter().any(|c| c.starts_with("git push")));
    }

    #[test]
    fn an_unknown_remote_is_refused() {
        let runner = runner().with(
            "git remote get-url origin",
            1,
            "",
            "error: No such remote\n",
        );
        let err = push(&runner, &request()).unwrap_err();
        assert!(matches!(err, Error::UnknownRemote { .. }), "{err}");
    }

    #[test]
    fn a_remote_that_is_not_plainly_a_name_is_refused() {
        for bad in [
            "",
            "-f",
            "git@github.com:me/repo.git",
            "origin/main",
            "a b",
            "+origin",
        ] {
            let request = PushRequest {
                remote: Some(bad.to_owned()),
                cwd: None,
            };
            let runner = runner();
            let err = push(&runner, &request).unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest(_)),
                "`{bad}` should be refused, got {err}"
            );
            assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        }
    }

    #[test]
    fn pushing_outside_a_repository_is_refused() {
        let err = push(&ScriptedRunner::new(), &request()).unwrap_err();
        assert!(matches!(err, Error::NotARepository), "{err}");
    }
}
