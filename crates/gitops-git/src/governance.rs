//! Reconciling git's config with the signing key's own identity.
//!
//! The signing key is the source of truth. Config is corrected to match it — never the
//! other way around — so a tool that rewrote `user.email` or flipped
//! `commit.gpgsign` off gets silently undone before the next commit.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    Error, Result,
    runner::CommandRunner,
    status::{Drift, SigningStatus, is_true},
};

/// One config change that was applied (or would be, in dry-run mode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Correction {
    /// `git config` arguments, for the record.
    pub command: String,
    pub reason: String,
    pub applied: bool,
    /// Populated when the command failed.
    pub error: Option<String>,
}

/// Result of a reconcile pass.
#[derive(Debug, Clone, Serialize)]
pub struct Reconciliation {
    pub corrections: Vec<Correction>,
    /// Drift that no automatic correction can fix (e.g. no signing key at all).
    pub unresolved: Vec<String>,
    /// Status after the pass.
    pub status: SigningStatus,
}

enum PlannedChange {
    GitConfig {
        args: Vec<String>,
        reason: String,
    },
    WriteFile {
        path: PathBuf,
        contents: String,
        reason: String,
    },
}

impl Reconciliation {
    pub fn changed(&self) -> bool {
        self.corrections.iter().any(|c| c.applied)
    }
}

/// Bring global git config back in line with the signing key.
///
/// With `dry_run`, nothing is written: the corrections are reported as `applied: false`.
pub fn reconcile(
    runner: &dyn CommandRunner,
    cwd: Option<&Path>,
    dry_run: bool,
) -> Result<Reconciliation> {
    let status = SigningStatus::read(runner, cwd)?;
    let mut planned: Vec<PlannedChange> = Vec::new();
    let mut unresolved = Vec::new();

    // 1. Signing stays on, globally.
    if !status.commit_gpgsign.global.as_deref().is_some_and(is_true) {
        planned.push(PlannedChange::GitConfig {
            args: vec!["--global".into(), "commit.gpgsign".into(), "true".into()],
            reason: "commit signing must be enabled in global config".into(),
        });
    }

    // 2. A local `commit.gpgsign` that disables signing is removed outright; there is no
    //    legitimate reason to turn signing off for a repository.
    if let Some(local) = &status.commit_gpgsign.local
        && !is_true(local)
        && status.repository.is_some()
    {
        planned.push(PlannedChange::GitConfig {
            args: vec![
                "--local".into(),
                "--unset-all".into(),
                "commit.gpgsign".into(),
            ],
            reason: format!("local config disabled signing with `{local}`"),
        });
    }

    // 3. The committer identity must match the key that signs for it.
    if let Some(identity) = &status.signing_identity {
        if let Some(key_email) = &identity.email {
            reconcile_user_email(&status, key_email, &mut planned);
        } else if identity.format == "ssh" {
            unresolved.push(format!(
                "SSH signing key `{}` has no email-like public-key comment, so user.email and allowed signers cannot be reconciled automatically",
                identity.key
            ));
        }
        if identity.format == "ssh" {
            reconcile_allowed_signers(
                runner,
                cwd,
                &status,
                identity,
                &mut planned,
                &mut unresolved,
            )?;
        }
        if !identity.secret_key_present {
            unresolved.push(format!(
                "the secret half of signing key `{}` is not available on this machine",
                identity.key
            ));
        }
    }

    for drift in &status.drift {
        if matches!(
            drift,
            Drift::MissingSigningKey | Drift::KeyIdentityUnknown { .. }
        ) {
            unresolved.push(drift.summary());
        }
    }

    let mut corrections = Vec::with_capacity(planned.len());
    for change in planned {
        match change {
            PlannedChange::GitConfig { args, reason } => {
                let command = format!("git config {}", args.join(" "));
                if dry_run {
                    corrections.push(Correction {
                        command,
                        reason,
                        applied: false,
                        error: None,
                    });
                    continue;
                }
                let argv: Vec<&str> = std::iter::once("config")
                    .chain(args.iter().map(String::as_str))
                    .collect();
                let out = runner.run("git", &argv, cwd)?;
                tracing::info!(%command, status = out.status, "applied config correction");
                corrections.push(Correction {
                    command,
                    reason,
                    applied: out.ok(),
                    error: (!out.ok()).then(|| crate::trimmed(&out.stderr)),
                });
            }
            PlannedChange::WriteFile {
                path,
                contents,
                reason,
            } => {
                let command = format!("write {}", path.display());
                if dry_run {
                    corrections.push(Correction {
                        command,
                        reason,
                        applied: false,
                        error: None,
                    });
                    continue;
                }
                match runner.write_file(&path, &contents) {
                    Ok(()) => {
                        tracing::info!(path = %path.display(), bytes = contents.len(), "applied file correction");
                        corrections.push(Correction {
                            command,
                            reason,
                            applied: true,
                            error: None,
                        });
                    }
                    Err(source) => {
                        return Err(Error::FileWriteFailed {
                            path: path.display().to_string(),
                            source,
                        });
                    }
                }
            }
        }
    }

    // Re-read so callers always see post-correction truth.
    let status = if corrections.iter().any(|c| c.applied) {
        SigningStatus::read(runner, cwd)?
    } else {
        status
    };

    Ok(Reconciliation {
        corrections,
        unresolved,
        status,
    })
}

fn reconcile_user_email(status: &SigningStatus, key_email: &str, planned: &mut Vec<PlannedChange>) {
    if status.uses_repo_local_signing() {
        if status.user_email.local.as_deref() != Some(key_email) {
            planned.push(PlannedChange::GitConfig {
                args: vec!["--local".into(), "user.email".into(), key_email.to_owned()],
                reason: format!(
                    "this repository signs with a repo-local key, so local user.email must match `{key_email}`"
                ),
            });
        }
        return;
    }

    if status.user_email.global.as_deref() != Some(key_email) {
        planned.push(PlannedChange::GitConfig {
            args: vec!["--global".into(), "user.email".into(), key_email.to_owned()],
            reason: format!("global user.email must match the signing key's email `{key_email}`"),
        });
    }
    if let Some(local) = &status.user_email.local
        && local != key_email
    {
        planned.push(PlannedChange::GitConfig {
            args: vec!["--local".into(), "--unset-all".into(), "user.email".into()],
            reason: format!("local user.email `{local}` shadows the signing key's email"),
        });
    }
}

fn reconcile_allowed_signers(
    runner: &dyn CommandRunner,
    cwd: Option<&Path>,
    status: &SigningStatus,
    identity: &crate::status::SigningIdentity,
    planned: &mut Vec<PlannedChange>,
    unresolved: &mut Vec<String>,
) -> Result<()> {
    let Some(principal) = identity.email.as_deref() else {
        return Ok(());
    };

    let target = allowed_signers_target(runner, cwd, status)?;
    let public_key = ssh_public_key(runner, cwd, &identity.key)?;
    let contents = ensure_allowed_signer(
        target.existing.as_deref().unwrap_or(""),
        principal,
        &public_key,
    );
    if target.existing.as_deref() != Some(contents.as_str()) {
        planned.push(PlannedChange::WriteFile {
            path: target.path.clone(),
            contents,
            reason: format!(
                "SSH signing must authorize `{principal}` in {}",
                target.path.display()
            ),
        });
    }
    if target.configure_local {
        planned.push(PlannedChange::GitConfig {
            args: vec![
                "--local".into(),
                "gpg.ssh.allowedSignersFile".into(),
                target.path.display().to_string(),
            ],
            reason: "SSH signing needs an allowed signers file in this repository".into(),
        });
    }

    if status.ssh_allowed_signers_file.effective.is_none() && !target.configure_local {
        unresolved.push("SSH signing has no usable allowed signers file".into());
    }
    Ok(())
}

struct AllowedSignersTarget {
    path: PathBuf,
    existing: Option<String>,
    configure_local: bool,
}

fn allowed_signers_target(
    runner: &dyn CommandRunner,
    cwd: Option<&Path>,
    status: &SigningStatus,
) -> Result<AllowedSignersTarget> {
    if let Some(path) = status.ssh_allowed_signers_file.effective.as_deref() {
        let path = PathBuf::from(path);
        return Ok(AllowedSignersTarget {
            existing: runner.read_file(&path).ok(),
            path,
            configure_local: false,
        });
    }

    let git_dir = runner
        .run("git", &["rev-parse", "--absolute-git-dir"], cwd)?
        .value()
        .ok_or(Error::NotARepository)?;
    let path = PathBuf::from(git_dir)
        .join("gitops-mcp")
        .join("allowed_signers");
    Ok(AllowedSignersTarget {
        existing: runner.read_file(&path).ok(),
        path,
        configure_local: true,
    })
}

fn ssh_public_key(runner: &dyn CommandRunner, cwd: Option<&Path>, key: &str) -> Result<String> {
    if let Some(public) = normalize_public_key(key) {
        return Ok(public);
    }

    let path = Path::new(key.trim_start_matches("key::"));
    if let Ok(contents) = runner.read_file(path)
        && let Some(public) = normalize_public_key(&contents)
    {
        return Ok(public);
    }

    let path_text = path.display().to_string();
    let out = runner.run("ssh-keygen", &["-y", "-f", &path_text], cwd)?;
    if !out.ok() {
        return Err(Error::KeyNotFound {
            key: key.to_owned(),
            detail: crate::trimmed(&out.stderr),
        });
    }
    normalize_public_key(&out.stdout).ok_or(Error::KeyNotFound {
        key: key.to_owned(),
        detail: "could not derive a public key for allowed signers".into(),
    })
}

fn normalize_public_key(text: &str) -> Option<String> {
    let mut fields = text.split_whitespace();
    let kind = fields.next()?;
    if !matches!(
        kind,
        "ssh-ed25519"
            | "ssh-rsa"
            | "ssh-dss"
            | "ecdsa-sha2-nistp256"
            | "ecdsa-sha2-nistp384"
            | "ecdsa-sha2-nistp521"
    ) && !kind.starts_with("sk-ssh-")
    {
        return None;
    }
    let key = fields.next()?;
    Some(format!("{kind} {key}"))
}

fn ensure_allowed_signer(existing: &str, principal: &str, public_key: &str) -> String {
    let expected = format!("{principal} {public_key}");
    let mut kept = Vec::new();
    let mut inserted = false;

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            kept.push(line.to_owned());
            continue;
        }

        let Some(first) = trimmed.split_whitespace().next() else {
            kept.push(line.to_owned());
            continue;
        };
        if first == principal {
            if !inserted {
                kept.push(expected.clone());
                inserted = true;
            }
            continue;
        }

        kept.push(line.to_owned());
    }

    if !inserted {
        kept.push(expected);
    }

    let mut rendered = kept.join("\n");
    if !rendered.is_empty() && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    fn base_runner() -> ScriptedRunner {
        ScriptedRunner::new()
            .with("git rev-parse --show-toplevel", 0, "/repo\n", "")
            .with("git config commit.gpgsign", 0, "true\n", "")
            .with("git config --global commit.gpgsign", 0, "true\n", "")
            .with("git config --local commit.gpgsign", 1, "", "")
            .with("git config user.signingkey", 0, "354F34B4DB349BD6\n", "")
            .with(
                "git config --global user.signingkey",
                0,
                "354F34B4DB349BD6\n",
                "",
            )
            .with("git config --local user.signingkey", 1, "", "")
            .with("git config gpg.format", 0, "openpgp\n", "")
            .with("git config --global gpg.format", 0, "openpgp\n", "")
            .with("git config --local gpg.format", 1, "", "")
            .with("git config user.name", 0, "Michael Bruno\n", "")
            .with("git config --global user.name", 0, "Michael Bruno\n", "")
            .with("git config --local user.name", 1, "", "")
            .with(
                "gpg --list-keys --with-colons 354F34B4DB349BD6",
                0,
                "uid:u::::1711036800::ABC::Michael Bruno <me@example.com>::::::::::0:\n",
                "",
            )
            .with(
                "gpg --list-secret-keys --with-colons 354F34B4DB349BD6",
                0,
                "sec:u:4096:1:354F34B4DB349BD6:\n",
                "",
            )
    }

    fn with_email(runner: ScriptedRunner, effective: &str, global: &str) -> ScriptedRunner {
        runner
            .with("git config user.email", 0, effective, "")
            .with("git config --global user.email", 0, global, "")
            .with("git config --local user.email", 1, "", "")
    }

    fn ssh_runner() -> ScriptedRunner {
        ScriptedRunner::new()
            .with("git rev-parse --show-toplevel", 0, "/repo\n", "")
            .with("git config commit.gpgsign", 0, "true\n", "")
            .with("git config --global commit.gpgsign", 0, "true\n", "")
            .with("git config --local commit.gpgsign", 1, "", "")
            .with("git config user.signingkey", 0, "/keys/work.pub\n", "")
            .with(
                "git config --global user.signingkey",
                0,
                "354F34B4DB349BD6\n",
                "",
            )
            .with(
                "git config --local user.signingkey",
                0,
                "/keys/work.pub\n",
                "",
            )
            .with("git config gpg.format", 0, "ssh\n", "")
            .with("git config --global gpg.format", 0, "openpgp\n", "")
            .with("git config --local gpg.format", 0, "ssh\n", "")
            .with("git config --path gpg.ssh.allowedSignersFile", 1, "", "")
            .with(
                "git config --path --global gpg.ssh.allowedSignersFile",
                1,
                "",
                "",
            )
            .with(
                "git config --path --local gpg.ssh.allowedSignersFile",
                1,
                "",
                "",
            )
            .with("git config user.name", 0, "Work User\n", "")
            .with("git config --global user.name", 0, "Personal User\n", "")
            .with("git config --local user.name", 0, "Work User\n", "")
            .with(
                "ssh-keygen -l -f /keys/work.pub",
                0,
                "256 SHA256:abc work@example.com (ED25519)\n",
                "",
            )
            .with_file(
                "/keys/work.pub",
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIworkkey work@example.com\n",
            )
    }

    #[test]
    fn compliant_config_needs_no_corrections() {
        let runner = with_email(base_runner(), "me@example.com\n", "me@example.com\n");
        let result = reconcile(&runner, None, false).unwrap();
        assert!(result.corrections.is_empty(), "{:?}", result.corrections);
        assert!(result.unresolved.is_empty());
        assert!(result.status.compliant, "{:?}", result.status.drift);
        assert!(result.status.signing_available);
    }

    #[test]
    fn rewritten_email_is_restored_from_the_key() {
        let runner = with_email(
            base_runner(),
            "llm@nowhere.invalid\n",
            "llm@nowhere.invalid\n",
        )
        .with("git config --global user.email me@example.com", 0, "", "");
        let result = reconcile(&runner, None, false).unwrap();
        assert_eq!(result.corrections.len(), 1);
        assert_eq!(
            result.corrections[0].command,
            "git config --global user.email me@example.com"
        );
        assert!(result.corrections[0].applied);
    }

    #[test]
    fn disabled_signing_is_re_enabled_at_both_scopes() {
        let runner = with_email(
            base_runner()
                .with("git config commit.gpgsign", 0, "false\n", "")
                .with("git config --global commit.gpgsign", 0, "false\n", "")
                .with("git config --local commit.gpgsign", 0, "false\n", ""),
            "me@example.com\n",
            "me@example.com\n",
        )
        .with("git config --global commit.gpgsign true", 0, "", "")
        .with("git config --local --unset-all commit.gpgsign", 0, "", "");
        let result = reconcile(&runner, None, false).unwrap();
        let commands: Vec<_> = result
            .corrections
            .iter()
            .map(|c| c.command.as_str())
            .collect();
        assert_eq!(
            commands,
            vec![
                "git config --global commit.gpgsign true",
                "git config --local --unset-all commit.gpgsign",
            ]
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let runner = with_email(
            base_runner(),
            "llm@nowhere.invalid\n",
            "llm@nowhere.invalid\n",
        );
        let result = reconcile(&runner, None, true).unwrap();
        assert_eq!(result.corrections.len(), 1);
        assert!(!result.corrections[0].applied);
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| c.contains("--global user.email me@example.com"))
        );
    }

    #[test]
    fn missing_signing_key_is_reported_as_unresolved() {
        let runner = with_email(
            base_runner()
                .with("git config user.signingkey", 1, "", "")
                .with("git config --global user.signingkey", 1, "", ""),
            "me@example.com\n",
            "me@example.com\n",
        );
        let result = reconcile(&runner, None, true).unwrap();
        assert!(
            result
                .unresolved
                .iter()
                .any(|u| u.contains("user.signingkey is not configured")),
            "{:?}",
            result.unresolved
        );
    }

    #[test]
    fn repo_local_signing_repairs_local_email_instead_of_global_email() {
        let runner = ssh_runner()
            .with("git config user.email", 0, "personal@example.com\n", "")
            .with("git config --global user.email", 0, "personal@example.com\n", "")
            .with("git config --local user.email", 1, "", "")
            .with("git rev-parse --absolute-git-dir", 0, "/repo/.git\n", "")
            .with("git config --local user.email work@example.com", 0, "", "")
            .with(
                "git config --local gpg.ssh.allowedSignersFile /repo/.git/gitops-mcp/allowed_signers",
                0,
                "",
                "",
            );

        let result = reconcile(&runner, None, false).unwrap();
        let commands: Vec<_> = result
            .corrections
            .iter()
            .map(|c| c.command.as_str())
            .collect();

        assert!(commands.contains(&"git config --local user.email work@example.com"));
        assert!(
            !commands
                .iter()
                .any(|c| c == &"git config --global user.email work@example.com"),
            "{commands:?}"
        );
        assert!(commands.contains(
            &"git config --local gpg.ssh.allowedSignersFile /repo/.git/gitops-mcp/allowed_signers"
        ));

        let writes = runner.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert_eq!(
            writes[0].0,
            PathBuf::from("/repo/.git/gitops-mcp/allowed_signers")
        );
        assert_eq!(
            writes[0].1,
            "work@example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIworkkey\n"
        );
    }

    #[test]
    fn existing_allowed_signers_file_is_updated_in_place() {
        let runner = ssh_runner()
            .with("git config user.email", 0, "work@example.com\n", "")
            .with("git config --global user.email", 0, "personal@example.com\n", "")
            .with("git config --local user.email", 0, "work@example.com\n", "")
            .with(
                "git config --path gpg.ssh.allowedSignersFile",
                0,
                "/tmp/allowed_signers\n",
                "",
            )
            .with(
                "git config --path --local gpg.ssh.allowedSignersFile",
                0,
                "/tmp/allowed_signers\n",
                "",
            )
            .with_file(
                "/tmp/allowed_signers",
                "# keep me\nwork@example.com ssh-ed25519 AAAAOLD old-comment\nother@example.com ssh-ed25519 AAAAOTHER\n",
            );

        let result = reconcile(&runner, None, false).unwrap();
        assert!(
            result
                .corrections
                .iter()
                .all(|c| !c.command.starts_with("git config --global user.email")),
            "{:?}",
            result.corrections
        );

        let writes = runner.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert_eq!(writes[0].0, PathBuf::from("/tmp/allowed_signers"));
        assert_eq!(
            writes[0].1,
            "# keep me\nwork@example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIworkkey\nother@example.com ssh-ed25519 AAAAOTHER\n"
        );
    }
}
