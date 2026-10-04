//! Reading git's signing configuration, scope by scope.

use std::path::Path;

use serde::Serialize;

use crate::{Error, Result, runner::CommandRunner};

/// One config key, resolved at every scope that matters for governance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScopedValue {
    /// What git actually uses here (`git config <key>`).
    pub effective: Option<String>,
    /// `git config --global <key>` — the value LLMs and tools like to rewrite.
    pub global: Option<String>,
    /// `git config --local <key>` — a local override shadows the global one.
    pub local: Option<String>,
}

impl ScopedValue {
    fn read(runner: &dyn CommandRunner, cwd: Option<&Path>, key: &str) -> Result<Self> {
        Ok(Self {
            effective: read_config(runner, cwd, &[], key)?,
            global: read_config(runner, cwd, &["--global"], key)?,
            local: read_config(runner, cwd, &["--local"], key)?,
        })
    }
}

fn read_config(
    runner: &dyn CommandRunner,
    cwd: Option<&Path>,
    scope: &[&str],
    key: &str,
) -> Result<Option<String>> {
    let mut args = vec!["config"];
    args.extend_from_slice(scope);
    args.push(key);
    // Exit code 1 just means "unset", which is a value, not a failure.
    Ok(runner.run("git", &args, cwd)?.value())
}

/// A signing key's own identity, as the key itself declares it.
///
/// This is the governance source of truth: `user.email` must match it, not the
/// other way around.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SigningIdentity {
    /// `openpgp` or `ssh`.
    pub format: String,
    /// The key id / path as configured in `user.signingkey`.
    pub key: String,
    /// Email carried by the key (OpenPGP uid, or the comment on an SSH public key).
    pub email: Option<String>,
    /// Display name carried by the key, when it has one.
    pub name: Option<String>,
    /// Whether the secret half of the key is actually present locally.
    pub secret_key_present: bool,
}

/// A governance violation: something that must be corrected before committing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Drift {
    /// Signing is turned off at some scope.
    SigningDisabled { scope: String, value: String },
    /// `user.signingkey` is not set anywhere.
    MissingSigningKey,
    /// `user.email` does not match the signing key's own email.
    EmailMismatch { git: Option<String>, key: String },
    /// The key is configured but its identity could not be read.
    KeyIdentityUnknown { key: String, reason: String },
    /// A local config value shadows the global one for a signing key.
    LocalOverride { key: String, value: String },
}

impl Drift {
    pub fn summary(&self) -> String {
        match self {
            Self::SigningDisabled { scope, value } => {
                format!("commit.gpgsign is `{value}` in {scope} config")
            }
            Self::MissingSigningKey => "user.signingkey is not configured".into(),
            Self::EmailMismatch { git, key } => format!(
                "user.email is {} but the signing key's email is `{key}`",
                git.as_deref()
                    .map(|g| format!("`{g}`"))
                    .unwrap_or_else(|| "unset".into())
            ),
            Self::KeyIdentityUnknown { key, reason } => {
                format!("could not read the identity of signing key `{key}`: {reason}")
            }
            Self::LocalOverride { key, value } => {
                format!("local config overrides {key} with `{value}`")
            }
        }
    }
}

/// Everything the server knows about signing in one place.
#[derive(Debug, Clone, Serialize)]
pub struct SigningStatus {
    /// Repository root, when the working directory is inside one.
    pub repository: Option<String>,
    pub commit_gpgsign: ScopedValue,
    pub user_signingkey: ScopedValue,
    pub gpg_format: ScopedValue,
    pub user_email: ScopedValue,
    pub user_name: ScopedValue,
    pub signing_identity: Option<SigningIdentity>,
    /// True when a key is configured, signing is enabled, and the secret key is present.
    pub signing_available: bool,
    /// True when nothing needs correcting.
    pub compliant: bool,
    pub drift: Vec<Drift>,
    /// Literal transcript of `git config commit.gpgsign; git config user.signingkey`,
    /// so the answer is the same every time regardless of how it is asked for.
    pub raw: String,
}

impl SigningStatus {
    pub fn read(runner: &dyn CommandRunner, cwd: Option<&Path>) -> Result<Self> {
        let repository = runner
            .run("git", &["rev-parse", "--show-toplevel"], cwd)?
            .value();

        let commit_gpgsign = ScopedValue::read(runner, cwd, "commit.gpgsign")?;
        let user_signingkey = ScopedValue::read(runner, cwd, "user.signingkey")?;
        let gpg_format = ScopedValue::read(runner, cwd, "gpg.format")?;
        let user_email = ScopedValue::read(runner, cwd, "user.email")?;
        let user_name = ScopedValue::read(runner, cwd, "user.name")?;

        let raw = format!(
            "$ git config commit.gpgsign\n{}\n$ git config user.signingkey\n{}",
            commit_gpgsign.effective.as_deref().unwrap_or(""),
            user_signingkey.effective.as_deref().unwrap_or("")
        );

        let format = gpg_format
            .effective
            .clone()
            .unwrap_or_else(|| "openpgp".to_owned());

        let mut drift = Vec::new();

        let signing_identity = match user_signingkey.effective.as_deref() {
            None => {
                drift.push(Drift::MissingSigningKey);
                None
            }
            Some(key) => match crate::identity::resolve(runner, cwd, &format, key) {
                Ok(identity) => Some(identity),
                Err(err) => {
                    drift.push(Drift::KeyIdentityUnknown {
                        key: key.to_owned(),
                        reason: err.to_string(),
                    });
                    None
                }
            },
        };

        // Signing off at any scope is drift: the effective value is what bites, but a
        // `false` left behind in global config will bite the next repository.
        for (scope, value) in [
            ("effective", &commit_gpgsign.effective),
            ("global", &commit_gpgsign.global),
            ("local", &commit_gpgsign.local),
        ] {
            match value.as_deref() {
                Some(v) if !is_true(v) => drift.push(Drift::SigningDisabled {
                    scope: scope.to_owned(),
                    value: v.to_owned(),
                }),
                None if scope != "local" => drift.push(Drift::SigningDisabled {
                    scope: scope.to_owned(),
                    value: "unset".to_owned(),
                }),
                _ => {}
            }
        }

        // `user.name` is deliberately not governed: a display name is a preference, while
        // the email is what the signature is checked against.
        if let Some(identity) = &signing_identity
            && let Some(key_email) = &identity.email
            && user_email.effective.as_deref() != Some(key_email.as_str())
        {
            drift.push(Drift::EmailMismatch {
                git: user_email.effective.clone(),
                key: key_email.clone(),
            });
        }

        // A local override of an identity key is how signing silently breaks per-repo.
        for (key, scoped) in [
            ("user.email", &user_email),
            ("user.signingkey", &user_signingkey),
            ("gpg.format", &gpg_format),
        ] {
            if let Some(local) = &scoped.local
                && scoped.global.as_deref() != Some(local.as_str())
            {
                drift.push(Drift::LocalOverride {
                    key: key.to_owned(),
                    value: local.clone(),
                });
            }
        }

        let signing_available = signing_identity
            .as_ref()
            .is_some_and(|id| id.secret_key_present);

        Ok(Self {
            repository,
            commit_gpgsign,
            user_signingkey,
            gpg_format,
            user_email,
            user_name,
            signing_identity,
            signing_available,
            compliant: drift.is_empty(),
            drift,
            raw,
        })
    }

    pub(crate) fn require_repository(&self) -> Result<&str> {
        self.repository
            .as_deref()
            .ok_or(Error::NotARepository)
    }
}

pub(crate) fn is_true(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}
