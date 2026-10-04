//! Git signing governance, free of any transport or framework.
//!
//! The crate answers three questions, each through the [`runner::CommandRunner`] port:
//!
//! * [`status::SigningStatus::read`] — what is git's signing config right now, at every scope?
//! * [`governance::reconcile`] — put that config back in line with the signing key's identity.
//! * [`commit::commit`] — stage and create a signed commit, reconciling first.
//!
//! The signing key is always the source of truth. Config bends to the key.

pub mod commit;
pub mod governance;
pub mod identity;
pub mod runner;
pub mod status;

pub use commit::{CommitOutcome, CommitRequest};
pub use governance::{Correction, Reconciliation, reconcile};
pub use runner::{CommandRunner, SystemRunner};
pub use status::{Drift, HooksStatus, ScopedValue, SigningIdentity, SigningStatus};

/// Errors this crate can produce.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not run `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("not inside a git repository")]
    NotARepository,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("unsupported gpg.format `{0}`; expected `openpgp` or `ssh`")]
    UnsupportedSigningFormat(String),
    #[error("signing key `{key}` could not be read: {detail}")]
    KeyNotFound { key: String, detail: String },
    #[error(
        "refusing to commit: signing is unavailable ({detail}). \
         Unlock or install the signing key, then retry — do not create an unsigned commit."
    )]
    SigningUnavailable { detail: String },
    #[error(
        "git could not sign the commit: {detail}. \
         The key is most likely locked; unlock it (e.g. `gpg --sign` once to cache the passphrase) and retry."
    )]
    SigningFailed { detail: String },
    #[error("staging failed: {detail}")]
    StageFailed { detail: String },
    #[error(
        "a git hook rejected the commit:\n{detail}\n\n\
         Fix what the hook reports, then retry. Do not bypass the hook with `--no-verify`, \
         do not disable or delete the hook, and do not turn off commit signing — the hook \
         failure is the real problem and it is unrelated to signing."
    )]
    HookRejected { detail: String },
    #[error("commit failed: {detail}")]
    CommitFailed { detail: String },
    #[error("nothing staged to commit")]
    NothingToCommit,
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Self::Spawn {
            program: "git".into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn trimmed(text: &str) -> String {
    text.trim().to_owned()
}
