//! Git signing governance, free of any transport or framework.
//!
//! The crate answers three questions, each through the [`runner::CommandRunner`] port:
//!
//! * [`status::SigningStatus::read`] — what is git's signing config right now, at every scope?
//! * [`governance::reconcile`] — put that config back in line with the signing key's identity.
//! * [`commit::commit`] — stage and create a signed commit, reconciling first.
//!
//! The signing key is always the source of truth. Config bends to the key.
//!
//! Three more operations are here for the same reason: they are the ones an agent reaches
//! for a shell and a dangerous flag to perform. Each does only its safe form —
//! [`restore::restore`] keeps a recoverable patch, [`merge::merge_ff_only`] only
//! fast-forwards, [`push::push`] only publishes signed commits on the current branch —
//! and none of them exposes a parameter that relaxes the rule.
//!
//! [`diff::diff`] is here for the opposite reason: it changes nothing, and exists so that
//! "what did I change" costs a summary rather than a patch nobody budgeted for.

pub mod commit;
pub mod diff;
pub mod governance;
pub mod identity;
pub mod merge;
pub mod push;
pub mod restore;
pub mod runner;
pub mod status;
pub mod workspace;

pub use commit::{CommitOutcome, CommitRequest};
pub use diff::{ChangedFile, DiffOutcome, DiffRequest, Truncation, diff};
pub use governance::{Correction, Reconciliation, reconcile};
pub use merge::{MergeOutcome, MergeRequest, merge_ff_only};
pub use push::{CommitSignature, PushOutcome, PushRequest, push};
pub use restore::{RestoreOutcome, RestoreRequest, restore};
pub use runner::{CommandRunner, SystemRunner};
pub use status::{Drift, HooksStatus, ScopedValue, SigningIdentity, SigningStatus};
pub use workspace::{
    CleanupMode, WorkspaceApplyOutcome, WorkspaceApplyRequest, WorkspaceCleanupAction,
    WorkspaceCleanupOutcome, WorkspaceCleanupRequest, WorkspaceDiffExportOutcome,
    WorkspaceDiffExportRequest, WorkspaceScanOutcome, WorkspaceScanRequest, WorkspaceSummary,
    WorkspaceValidateOutcome, WorkspaceValidateRequest, WorkspaceValidateStep, workspace_apply,
    workspace_cleanup, workspace_diff_export, workspace_scan, workspace_validate,
};

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
        "a git hook rejected the {action}:\n{detail}\n\n\
         Fix what the hook reports, then retry. Do not bypass the hook, do not disable, \
         delete or un-execute it, do not repoint `core.hooksPath`, and do not turn off \
         commit signing — the hook failure is the real problem and it is unrelated to \
         signing."
    )]
    HookRejected { action: String, detail: String },
    #[error("commit failed: {detail}")]
    CommitFailed { detail: String },
    #[error("nothing staged to commit")]
    NothingToCommit,
    #[error(
        "refusing to discard changes: the recovery patch could not be written to `{path}` ({source}). \
         Nothing was restored, so the changes are still there."
    )]
    BackupFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("restore failed: {detail}")]
    RestoreFailed { detail: String },
    #[error(
        "refusing to {action}: the working tree has changes that are not committed:\n{detail}\n\n\
         Commit or stash them first. Discarding them to get the {action} through would \
         throw away work nobody asked to lose."
    )]
    WorkTreeDirty { action: String, detail: String },
    #[error("`{reference}` is not a ref this repository knows")]
    UnknownRef { reference: String },
    #[error(
        "`{reference}` cannot be fast-forwarded onto `{branch}`:\n{detail}\n\n\
         The branches have diverged, so landing this means choosing between a merge \
         commit, a rebase, or dropping commits — report the divergence and let the author \
         decide. Do not reconcile it automatically, and do not reset or force anything."
    )]
    FastForwardRefused {
        reference: String,
        branch: String,
        detail: String,
    },
    #[error("merge failed: {detail}")]
    MergeFailed { detail: String },
    #[error("HEAD is detached, so there is no branch to push; check out a branch first")]
    DetachedHead,
    #[error("`{remote}` is not a configured remote")]
    UnknownRemote { remote: String },
    #[error(
        "refusing to push `{branch}`: {} of its commits are not signed:\n{}\n\n\
         Sign them before publishing. An unsigned commit on a shared branch is the exact \
         outcome this server exists to prevent, so do not work around this.",
        commits.len(),
        commits.iter()
            .map(|c| format!("  {} {} {}", &c.commit[..c.commit.len().min(8)], c.verdict, c.subject))
            .collect::<Vec<_>>()
            .join("\n")
    )]
    UnsignedCommits {
        branch: String,
        commits: Vec<crate::push::CommitSignature>,
    },
    #[error(
        "the remote rejected the push of `{branch}` to `{remote}`:\n{detail}\n\n\
         `{remote}/{branch}` has commits this branch does not. Integrate them first — \
         this server never force-pushes, because overwriting published history is the \
         author's call, not an automatic one."
    )]
    PushRejected {
        remote: String,
        branch: String,
        detail: String,
    },
    #[error("push failed: {detail}")]
    PushFailed { detail: String },
    #[error("diff failed: {detail}")]
    DiffFailed { detail: String },
    #[error("workspace operation failed: {detail}")]
    WorkspaceOperationFailed { detail: String },
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
