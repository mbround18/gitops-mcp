//! Signed, annotated tags that never move once made, and are published only when they verify.
//!
//! A tag is how a release is named, and other people pin to it: a tag that is replaced
//! after someone fetched it silently changes what they build. So [`tag`] only ever creates
//! a new tag, always annotated and always signed, after reconciling signing config the way
//! [`crate::commit`] does. There is no parameter that replaces an existing tag, makes a
//! lightweight one, or skips the signature.
//!
//! [`push_tag`] publishes one tag by its full ref. It refuses a tag that is lightweight or
//! does not verify, a tag the remote already has pointing somewhere else, and — the same
//! rule as [`crate::push`] — any unsigned commit the tag would carry to the remote with it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    Error, Result,
    commit::looks_like_signing_failure,
    governance::{Reconciliation, reconcile},
    merge::{check_ref, combined},
    push::{CommitSignature, check_remote, parse_log},
    runner::CommandRunner,
    status::SigningStatus,
    trimmed,
};

/// A new tag to create.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagRequest {
    /// Tag name, e.g. `v1.2.0`. It must not exist yet.
    pub name: String,
    /// Annotation message. Required: it is part of what the signature covers.
    pub message: String,
    /// Commit to tag. Defaults to `HEAD`.
    #[serde(default)]
    pub target: Option<String>,
    /// Repository to work in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagOutcome {
    /// Config corrections applied before signing.
    pub governance: Reconciliation,
    pub name: String,
    /// The tag object's id.
    pub tag: String,
    /// The commit the tag points at.
    pub commit: String,
    /// Whether `git verify-tag` accepted the signature.
    pub verified: bool,
    /// What the verification said.
    pub verification: String,
    pub command: String,
}

/// The tag to publish.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushTagRequest {
    /// Name of an existing local tag.
    pub name: String,
    /// Remote to push to. Defaults to `origin`.
    #[serde(default)]
    pub remote: Option<String>,
    /// Repository to work in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PushTagOutcome {
    pub remote: String,
    pub name: String,
    pub tag: String,
    pub commit: String,
    /// Commits the remote did not have that this push carried along, newest first.
    pub published: Vec<CommitSignature>,
    /// True when the remote already had this exact tag.
    pub up_to_date: bool,
    pub command: String,
    pub output: String,
}

/// Create a signed, annotated tag named `name` on `target`, or change nothing.
pub fn tag(runner: &dyn CommandRunner, request: &TagRequest) -> Result<TagOutcome> {
    let cwd = request.cwd.as_deref();
    let name = request.name.trim();
    let message = request.message.trim();
    let target = request.target.as_deref().map(str::trim).unwrap_or("HEAD");

    check_name(name)?;
    if message.is_empty() {
        return Err(Error::InvalidRequest(
            "a tag needs a message: an annotated tag's message is part of what is signed".into(),
        ));
    }
    check_ref(target, "tag")?;

    // Governance first: the tag is signed with the same identity a commit would be.
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

    let full = format!("refs/tags/{name}");
    if !runner.run("git", &["check-ref-format", &full], cwd)?.ok() {
        return Err(Error::InvalidRequest(format!(
            "`{name}` is not a valid tag name"
        )));
    }
    if let Some(existing) = resolve(runner, cwd, &full)? {
        return Err(Error::TagExists {
            name: name.to_owned(),
            existing,
        });
    }
    let commit = resolve(runner, cwd, &format!("{target}^{{commit}}"))?.ok_or_else(|| {
        Error::UnknownRef {
            reference: target.to_owned(),
        }
    })?;

    let args = ["tag", "-s", "-m", message, name, commit.as_str()];
    let command = format!("git tag -s -m <message> {name} {commit}");
    let out = runner.run("git", &args, cwd)?;
    if !out.ok() {
        let detail = combined(&out.stdout, &out.stderr);
        if looks_like_signing_failure(&detail) {
            return Err(Error::SigningFailed { detail });
        }
        return Err(Error::TagFailed { detail });
    }

    // `^{tag}` resolves only an annotated tag, so this also proves what was made.
    let tag =
        resolve(runner, cwd, &format!("{full}^{{tag}}"))?.ok_or_else(|| Error::TagFailed {
            detail: format!("git reported success but `{full}` is not an annotated tag"),
        })?;
    let check = runner.run("git", &["verify-tag", &full], cwd)?;

    Ok(TagOutcome {
        governance,
        name: name.to_owned(),
        tag,
        commit,
        verified: check.ok(),
        verification: combined(&check.stdout, &check.stderr),
        command,
    })
}

/// Publish the signed tag `name` to `remote`, refusing anything that is not.
pub fn push_tag(runner: &dyn CommandRunner, request: &PushTagRequest) -> Result<PushTagOutcome> {
    let cwd = request.cwd.as_deref();
    let name = request.name.trim();
    let remote = request.remote.as_deref().unwrap_or("origin").trim();

    check_name(name)?;
    check_remote(remote)?;

    let status = SigningStatus::read(runner, cwd)?;
    status.require_repository()?;

    if !runner.run("git", &["remote", "get-url", remote], cwd)?.ok() {
        return Err(Error::UnknownRemote {
            remote: remote.to_owned(),
        });
    }

    let full = format!("refs/tags/{name}");
    let tag = resolve(runner, cwd, &full)?.ok_or_else(|| Error::UnknownRef {
        reference: full.clone(),
    })?;

    // Only a signed, annotated tag that verifies is published.
    let kind = runner.run("git", &["cat-file", "-t", &tag], cwd)?.value();
    if kind.as_deref() != Some("tag") {
        return Err(Error::UnsignedTag {
            name: name.to_owned(),
            detail: "it is a lightweight tag, so it carries no signature".into(),
        });
    }
    let check = runner.run("git", &["verify-tag", &full], cwd)?;
    if !check.ok() {
        return Err(Error::UnsignedTag {
            name: name.to_owned(),
            detail: format!(
                "its signature does not verify:\n{}",
                combined(&check.stdout, &check.stderr)
            ),
        });
    }

    let commit =
        resolve(runner, cwd, &format!("{full}^{{commit}}"))?.ok_or_else(|| Error::TagFailed {
            detail: format!("`{name}` does not point at a commit"),
        })?;

    // What the remote already has under this name decides whether there is anything to do.
    let listed = runner.run("git", &["ls-remote", remote, &full], cwd)?;
    if !listed.ok() {
        return Err(Error::PushFailed {
            detail: combined(&listed.stdout, &listed.stderr),
        });
    }
    let theirs = listed
        .stdout
        .lines()
        .find_map(|line| line.split_whitespace().next())
        .map(str::to_owned);
    match theirs {
        Some(theirs) if theirs == tag => {
            return Ok(PushTagOutcome {
                remote: remote.to_owned(),
                name: name.to_owned(),
                tag,
                commit,
                published: Vec::new(),
                up_to_date: true,
                command: String::new(),
                output: "Everything up-to-date".into(),
            });
        }
        Some(theirs) => {
            return Err(Error::TagOnRemote {
                name: name.to_owned(),
                remote: remote.to_owned(),
                detail: format!("theirs is `{theirs}`, ours is `{tag}`"),
            });
        }
        None => {}
    }

    // A tag carries its commit's history with it: the same signing rule as `push`.
    let remotes = format!("--remotes={remote}");
    let log = runner.run(
        "git",
        &["log", "--format=%H %G? %s", &commit, "--not", &remotes],
        cwd,
    )?;
    if !log.ok() {
        return Err(Error::PushFailed {
            detail: trimmed(&log.stderr),
        });
    }
    let published = parse_log(&log.stdout);
    let unsigned: Vec<CommitSignature> =
        published.iter().filter(|c| !c.signed()).cloned().collect();
    if !unsigned.is_empty() {
        return Err(Error::UnsignedCommits {
            branch: name.to_owned(),
            commits: unsigned,
        });
    }

    // Fully qualified, no `+`: the remote refuses to replace a tag it already has.
    let refspec = format!("{full}:{full}");
    let args = ["push", remote, refspec.as_str()];
    let command = format!("git {}", args.join(" "));
    let out = runner.run("git", &args, cwd)?;
    if !out.ok() {
        let detail = combined(&out.stdout, &out.stderr);
        let lower = detail.to_ascii_lowercase();
        if lower.contains("already exists") || lower.contains("[rejected]") {
            return Err(Error::TagOnRemote {
                name: name.to_owned(),
                remote: remote.to_owned(),
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

    Ok(PushTagOutcome {
        remote: remote.to_owned(),
        name: name.to_owned(),
        tag,
        commit,
        published,
        up_to_date: false,
        command,
        output: combined(&out.stdout, &out.stderr),
    })
}

fn check_name(name: &str) -> Result<()> {
    let reject = |why: &str| {
        Err(Error::InvalidRequest(format!(
            "`{name}` is not a usable tag name: {why}"
        )))
    };
    if name.is_empty() {
        return reject("it is empty");
    }
    if name.starts_with('-') {
        return reject("it would be read as an option");
    }
    if name.contains(char::is_whitespace) || name.contains(':') || name.contains('+') {
        return reject("name the tag itself, not a refspec");
    }
    if name.starts_with("refs/") {
        return reject("give the name without `refs/tags/`");
    }
    Ok(())
}

fn resolve(
    runner: &dyn CommandRunner,
    cwd: Option<&std::path::Path>,
    rev: &str,
) -> Result<Option<String>> {
    Ok(runner
        .run("git", &["rev-parse", "--verify", "--quiet", rev], cwd)?
        .value())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{commit::tests::compliant_runner, runner::ScriptedRunner};

    const TAG: &str = "git tag -s -m release one v1.0.0 cccc3333";
    const PUSH: &str = "git push origin refs/tags/v1.0.0:refs/tags/v1.0.0";

    fn tag_runner() -> ScriptedRunner {
        compliant_runner()
            .with("git check-ref-format refs/tags/v1.0.0", 0, "", "")
            .with(
                "git rev-parse --verify --quiet HEAD^{commit}",
                0,
                "cccc3333\n",
                "",
            )
            .with(TAG, 0, "", "")
            .with(
                "git rev-parse --verify --quiet refs/tags/v1.0.0^{tag}",
                0,
                "tttt0000\n",
                "",
            )
            .with("git verify-tag refs/tags/v1.0.0", 0, "", "Good signature\n")
    }

    /// The tag already exists locally.
    fn created(runner: ScriptedRunner) -> ScriptedRunner {
        runner.with(
            "git rev-parse --verify --quiet refs/tags/v1.0.0",
            0,
            "tttt0000\n",
            "",
        )
    }

    fn tag_request() -> TagRequest {
        TagRequest {
            name: "v1.0.0".into(),
            message: "release one".into(),
            target: None,
            cwd: None,
        }
    }

    fn push_runner() -> ScriptedRunner {
        created(compliant_runner())
            .with(
                "git remote get-url origin",
                0,
                "git@github.com:me/repo.git\n",
                "",
            )
            .with("git cat-file -t tttt0000", 0, "tag\n", "")
            .with("git verify-tag refs/tags/v1.0.0", 0, "", "Good signature\n")
            .with(
                "git rev-parse --verify --quiet refs/tags/v1.0.0^{commit}",
                0,
                "cccc3333\n",
                "",
            )
            .with("git ls-remote origin refs/tags/v1.0.0", 0, "", "")
            .with(
                "git log --format=%H %G? %s cccc3333 --not --remotes=origin",
                0,
                "cccc3333 G feat: the release\n",
                "",
            )
            .with(PUSH, 0, "", " * [new tag]         v1.0.0 -> v1.0.0\n")
    }

    fn push_request() -> PushTagRequest {
        PushTagRequest {
            name: "v1.0.0".into(),
            remote: None,
            cwd: None,
        }
    }

    #[test]
    fn a_new_tag_is_signed_annotated_and_never_forced() {
        let runner = tag_runner();
        let outcome = tag(&runner, &tag_request()).unwrap();
        assert_eq!(outcome.tag, "tttt0000");
        assert_eq!(outcome.commit, "cccc3333");
        assert!(outcome.verified, "{}", outcome.verification);

        let calls = runner.calls();
        let tags: Vec<_> = calls.iter().filter(|c| c.starts_with("git tag")).collect();
        assert_eq!(tags, vec![TAG], "{calls:?}");
        for forbidden in ["--force", " -f ", "--no-sign", "--no-gpg-sign", " -d "] {
            assert!(
                !calls.iter().any(|c| c.contains(forbidden)),
                "`{forbidden}` must never be run: {calls:?}"
            );
        }
    }

    #[test]
    fn governance_runs_before_the_tag_is_signed() {
        let runner = tag_runner();
        let _ = tag(&runner, &tag_request());
        let calls = runner.calls();
        let tag_at = calls.iter().position(|c| c == TAG).unwrap();
        let email_at = calls
            .iter()
            .position(|c| c == "git config user.email")
            .unwrap();
        assert!(email_at < tag_at, "{calls:?}");
    }

    #[test]
    fn an_existing_tag_is_never_replaced() {
        let runner = created(tag_runner());
        let err = tag(&runner, &tag_request()).unwrap_err();
        assert!(matches!(err, Error::TagExists { .. }), "{err}");
        assert!(
            !runner.calls().iter().any(|c| c.starts_with("git tag")),
            "{:?}",
            runner.calls()
        );
        // The refusal states the rule; it does not name a flag that would get around it.
        let text = err.to_string();
        assert!(!text.contains("-f") && !text.contains("force"), "{text}");
    }

    #[test]
    fn a_tag_that_does_not_verify_is_reported_not_hidden() {
        let runner = tag_runner().with(
            "git verify-tag refs/tags/v1.0.0",
            1,
            "",
            "error: no allowed signers\n",
        );
        let outcome = tag(&runner, &tag_request()).unwrap();
        assert!(!outcome.verified);
        assert!(outcome.verification.contains("allowed signers"));
    }

    #[test]
    fn a_tag_needs_a_message() {
        let runner = tag_runner();
        let request = TagRequest {
            message: "  ".into(),
            ..tag_request()
        };
        assert!(matches!(
            tag(&runner, &request),
            Err(Error::InvalidRequest(_))
        ));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn names_that_are_options_or_refspecs_are_refused() {
        for name in ["", "-f", "v1 x", "+v1", "a:b", "refs/tags/v1"] {
            let request = TagRequest {
                name: name.into(),
                ..tag_request()
            };
            assert!(
                matches!(tag(&tag_runner(), &request), Err(Error::InvalidRequest(_))),
                "`{name}` should be refused"
            );
            let request = PushTagRequest {
                name: name.into(),
                ..push_request()
            };
            assert!(
                matches!(
                    push_tag(&push_runner(), &request),
                    Err(Error::InvalidRequest(_))
                ),
                "`{name}` should be refused"
            );
        }
    }

    #[test]
    fn an_invalid_ref_name_is_refused_by_git_s_own_rules() {
        let runner = tag_runner().with("git check-ref-format refs/tags/v1.0.0", 1, "", "");
        let err = tag(&runner, &tag_request()).unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[test]
    fn an_unknown_target_is_refused() {
        let runner = tag_runner().with("git rev-parse --verify --quiet nope^{commit}", 1, "", "");
        let request = TagRequest {
            target: Some("nope".into()),
            ..tag_request()
        };
        assert!(matches!(
            tag(&runner, &request),
            Err(Error::UnknownRef { .. })
        ));
    }

    #[test]
    fn a_locked_key_is_a_signing_failure_with_no_unsigned_retry() {
        let runner = tag_runner().with(TAG, 128, "", "error: gpg failed to sign the data\n");
        let err = tag(&runner, &tag_request()).unwrap_err();
        assert!(matches!(err, Error::SigningFailed { .. }), "{err}");
        let tags = runner
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("git tag"))
            .count();
        assert_eq!(tags, 1, "never retried");
    }

    #[test]
    fn a_signed_tag_is_pushed_by_its_full_ref() {
        let runner = push_runner();
        let outcome = push_tag(&runner, &push_request()).unwrap();
        assert_eq!(outcome.command, PUSH);
        assert_eq!(outcome.published.len(), 1);
        assert!(!outcome.up_to_date);

        let calls = runner.calls();
        for forbidden in ["--force", "--delete", "--mirror", "--tags", "--no-verify"] {
            assert!(
                !calls.iter().any(|c| c.contains(forbidden)),
                "`{forbidden}` must never be run: {calls:?}"
            );
        }
        assert!(
            !calls.iter().any(|c| c.contains("push") && c.contains('+')),
            "{calls:?}"
        );
    }

    #[test]
    fn a_lightweight_tag_is_never_published() {
        let runner = push_runner().with("git cat-file -t tttt0000", 0, "commit\n", "");
        let err = push_tag(&runner, &push_request()).unwrap_err();
        assert!(matches!(err, Error::UnsignedTag { .. }), "{err}");
        assert!(!runner.calls().iter().any(|c| c.starts_with("git push")));
    }

    #[test]
    fn a_tag_that_does_not_verify_is_never_published() {
        let runner = push_runner().with(
            "git verify-tag refs/tags/v1.0.0",
            1,
            "",
            "error: no signature found\n",
        );
        let err = push_tag(&runner, &push_request()).unwrap_err();
        assert!(matches!(err, Error::UnsignedTag { .. }), "{err}");
        assert!(!runner.calls().iter().any(|c| c.starts_with("git push")));
    }

    #[test]
    fn unsigned_history_does_not_ride_along_with_a_tag() {
        let runner = push_runner().with(
            "git log --format=%H %G? %s cccc3333 --not --remotes=origin",
            0,
            "cccc3333 N chore: unsigned\n",
            "",
        );
        let err = push_tag(&runner, &push_request()).unwrap_err();
        assert!(matches!(err, Error::UnsignedCommits { .. }), "{err}");
        assert!(!runner.calls().iter().any(|c| c.starts_with("git push")));
    }

    #[test]
    fn the_same_tag_on_the_remote_is_up_to_date() {
        let runner = push_runner().with(
            "git ls-remote origin refs/tags/v1.0.0",
            0,
            "tttt0000\trefs/tags/v1.0.0\n",
            "",
        );
        let outcome = push_tag(&runner, &push_request()).unwrap();
        assert!(outcome.up_to_date);
        assert!(!runner.calls().iter().any(|c| c.starts_with("git push")));
    }

    #[test]
    fn a_different_tag_on_the_remote_is_never_overwritten() {
        let runner = push_runner().with(
            "git ls-remote origin refs/tags/v1.0.0",
            0,
            "ffff9999\trefs/tags/v1.0.0\n",
            "",
        );
        let err = push_tag(&runner, &push_request()).unwrap_err();
        assert!(matches!(err, Error::TagOnRemote { .. }), "{err}");
        assert!(!runner.calls().iter().any(|c| c.starts_with("git push")));
    }

    #[test]
    fn a_rejected_tag_push_is_never_retried() {
        let runner = push_runner().with(
            PUSH,
            1,
            "",
            " ! [rejected]        v1.0.0 -> v1.0.0 (already exists)\n",
        );
        let err = push_tag(&runner, &push_request()).unwrap_err();
        assert!(matches!(err, Error::TagOnRemote { .. }), "{err}");
        let pushes = runner
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("git push"))
            .count();
        assert_eq!(pushes, 1);
    }

    #[test]
    fn an_unknown_tag_or_remote_is_refused() {
        let runner =
            push_runner().with("git rev-parse --verify --quiet refs/tags/v1.0.0", 1, "", "");
        assert!(matches!(
            push_tag(&runner, &push_request()),
            Err(Error::UnknownRef { .. })
        ));
        let runner = push_runner().with("git remote get-url origin", 2, "", "");
        assert!(matches!(
            push_tag(&runner, &push_request()),
            Err(Error::UnknownRemote { .. })
        ));
    }
}
