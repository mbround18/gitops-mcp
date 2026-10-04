//! Reconciling git's config with the signing key's own identity.
//!
//! The signing key is the source of truth. Config is corrected to match it — never the
//! other way around — so a tool that rewrote `user.email` or flipped
//! `commit.gpgsign` off gets silently undone before the next commit.

use std::path::Path;

use serde::Serialize;

use crate::{
    Result,
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
    let mut planned: Vec<(Vec<String>, String)> = Vec::new();
    let mut unresolved = Vec::new();

    // 1. Signing stays on, globally.
    if !status.commit_gpgsign.global.as_deref().is_some_and(is_true) {
        planned.push((
            vec!["--global".into(), "commit.gpgsign".into(), "true".into()],
            "commit signing must be enabled in global config".into(),
        ));
    }

    // 2. A local `commit.gpgsign` that disables signing is removed outright; there is no
    //    legitimate reason to turn signing off for a repository.
    if let Some(local) = &status.commit_gpgsign.local
        && !is_true(local)
        && status.repository.is_some()
    {
        planned.push((
            vec![
                "--local".into(),
                "--unset-all".into(),
                "commit.gpgsign".into(),
            ],
            format!("local config disabled signing with `{local}`"),
        ));
    }

    // 3. The committer identity must match the key that signs for it.
    if let Some(identity) = &status.signing_identity {
        if let Some(key_email) = &identity.email {
            if status.user_email.global.as_deref() != Some(key_email.as_str()) {
                planned.push((
                    vec!["--global".into(), "user.email".into(), key_email.clone()],
                    format!("global user.email must match the signing key's email `{key_email}`"),
                ));
            }
            // A local override would shadow the corrected global value.
            if let Some(local) = &status.user_email.local
                && local != key_email
            {
                planned.push((
                    vec!["--local".into(), "--unset-all".into(), "user.email".into()],
                    format!("local user.email `{local}` shadows the signing key's email"),
                ));
            }
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
    for (args, reason) in planned {
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
}
