//! MCP adapter: exposes `gitops-git` over the Model Context Protocol.

use std::path::PathBuf;

use gitops_git::{
    CommitRequest, MergeRequest, PushRequest, RestoreRequest, SigningStatus, SystemRunner,
    reconcile,
};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_router,
};
use serde::Deserialize;

const INSTRUCTIONS: &str = "\
Git signing governance. The signing key is the source of truth for committer identity.

- `git_signing_status` reports `git config commit.gpgsign` and `git config user.signingkey` \
verbatim, plus every scope and any drift. Call it before touching git config.
- `git_signing_enforce` rewrites global git config to match the signing key. Never edit \
`user.email`, `user.name`, `user.signingkey` or `commit.gpgsign` yourself — call this instead.
- `commit` stages files (or everything with `all`) and creates a signed commit, enforcing \
config first. It never falls back to an unsigned commit.
- `restore` throws away local changes to named paths, saving them first so they can be \
recovered. Use it instead of `git checkout -- <path>`.
- `merge_ff_only` fast-forwards the current branch onto a ref, and refuses if the branches \
have diverged.
- `push` publishes the current branch. It refuses to publish an unsigned commit.

These tools do only the safe form of each operation. When one refuses, the refusal is the \
answer: fix what it reports rather than reaching for a shell to do the same thing without \
the checks.";

/// Paths are resolved against the server's working directory unless `cwd` is given.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RepoParams {
    /// Repository directory to inspect. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EnforceParams {
    /// Repository directory to act in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Report the corrections that would be made without writing any config.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CommitParams {
    /// Commit message. The first line is the subject.
    pub message: String,
    /// Paths to stage. Mutually exclusive with `all`.
    #[serde(default)]
    pub files: Vec<String>,
    /// Stage every change in the working tree, including untracked files.
    #[serde(default)]
    pub all: bool,
    /// Repository directory to commit in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Deliberately minimal: naming paths is the only input, because every other knob
/// `git restore` has either widens the blast radius or skips the recovery patch.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RestoreParams {
    /// Paths whose local changes should be thrown away. Required.
    pub files: Vec<String>,
    /// Repository directory to act in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct MergeParams {
    /// Branch, tag or commit to fast-forward the current branch to.
    pub r#ref: String,
    /// Repository directory to act in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct PushParams {
    /// Remote to push to. Defaults to `origin`.
    #[serde(default)]
    pub remote: Option<String>,
    /// Repository directory to act in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GitOpsServer {
    runner: SystemRunner,
}

#[tool_router]
impl GitOpsServer {
    pub fn new() -> Self {
        Self {
            runner: SystemRunner,
        }
    }

    #[tool(
        name = "git_signing_status",
        description = "Report git commit-signing configuration: the verbatim output of `git config commit.gpgsign` and `git config user.signingkey`, the value at every scope, the signing key's own identity, and any drift that needs correcting."
    )]
    fn git_signing_status(
        &self,
        Parameters(params): Parameters<RepoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cwd = params.cwd.map(PathBuf::from);
        let status = SigningStatus::read(&self.runner, cwd.as_deref()).map_err(internal)?;

        let mut text = status.raw.clone();
        text.push_str("\n\n");
        text.push_str(&summarize(&status));
        Ok(with_structured(text, &status))
    }

    #[tool(
        name = "git_signing_enforce",
        description = "Reconcile global git config with the signing key's own identity: re-enable `commit.gpgsign`, restore `user.email`/`user.name` to the key's uid, and drop local overrides that shadow them. Use this instead of editing git config directly."
    )]
    fn git_signing_enforce(
        &self,
        Parameters(params): Parameters<EnforceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cwd = params.cwd.map(PathBuf::from);
        let result = reconcile(&self.runner, cwd.as_deref(), params.dry_run).map_err(internal)?;

        let mut text = if result.corrections.is_empty() {
            "Git signing config already matches the signing key; nothing to correct.".to_owned()
        } else {
            let verb = if params.dry_run {
                "Would apply"
            } else {
                "Applied"
            };
            let mut lines = format!("{verb} {} correction(s):\n", result.corrections.len());
            for c in &result.corrections {
                lines.push_str(&format!("- `{}` — {}\n", c.command, c.reason));
                if let Some(err) = &c.error {
                    lines.push_str(&format!("  failed: {err}\n"));
                }
            }
            lines
        };
        if !result.unresolved.is_empty() {
            text.push_str("\nNeeds a human:\n");
            for item in &result.unresolved {
                text.push_str(&format!("- {item}\n"));
            }
        }
        text.push('\n');
        text.push_str(&summarize(&result.status));
        Ok(with_structured(text, &result))
    }

    #[tool(
        name = "commit",
        description = "Create a signed git commit. Pass `files` to stage specific paths or `all: true` to stage everything, with a `message`. Signing config is reconciled with the signing key first, and the commit is signed; if signing is unavailable the call fails rather than producing an unsigned commit."
    )]
    fn commit(
        &self,
        Parameters(params): Parameters<CommitParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = CommitRequest {
            message: params.message,
            files: params.files,
            all: params.all,
            // Not a parameter: a caller cannot ask this server for an unsigned commit.
            allow_unsigned: false,
            cwd: params.cwd.map(PathBuf::from),
        };

        match gitops_git::commit::commit(&self.runner, &request) {
            Ok(outcome) => {
                let mut text = format!(
                    "{} commit {} — {}\n",
                    if outcome.signed { "Signed" } else { "UNSIGNED" },
                    outcome.commit.as_deref().unwrap_or("(unknown)"),
                    outcome.subject
                );
                if let Some(sig) = &outcome.signature_status {
                    text.push_str(&format!("Signature: {sig} ({})\n", describe_signature(sig)));
                }
                text.push_str(&format!("Staged {} path(s)\n", outcome.staged.len()));
                if outcome.governance.changed() {
                    text.push_str("Config corrected before committing:\n");
                    for c in outcome.governance.corrections.iter().filter(|c| c.applied) {
                        text.push_str(&format!("- `{}` — {}\n", c.command, c.reason));
                    }
                }
                Ok(with_structured(text, &outcome))
            }
            // A failed commit is the caller's problem to fix, so it comes back as a
            // tool-level error whose message the caller actually sees.
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "restore",
        description = "Throw away local changes to specific paths, restoring them to their committed state — the safe form of `git checkout -- <path>`. Requires explicit paths; it cannot restore the whole tree. Whatever is discarded is saved as a recovery patch first, and if that patch cannot be written nothing is restored."
    )]
    fn restore(
        &self,
        Parameters(params): Parameters<RestoreParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = RestoreRequest {
            files: params.files,
            cwd: params.cwd.map(PathBuf::from),
        };

        match gitops_git::restore::restore(&self.runner, &request) {
            Ok(outcome) => {
                let mut text = format!(
                    "Restored {} path(s) to their committed state.\n",
                    outcome.restored.len()
                );
                match &outcome.backup {
                    Some(path) => text.push_str(&format!(
                        "Discarded changes saved to:\n  {path}\nRecover them with: git apply {path}\n"
                    )),
                    None => text.push_str("Nothing had changed, so nothing was discarded.\n"),
                }
                if !outcome.unchanged.is_empty() {
                    text.push_str(&format!(
                        "Already clean: {}\n",
                        outcome.unchanged.join(", ")
                    ));
                }
                Ok(with_structured(text, &outcome))
            }
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "merge_ff_only",
        description = "Fast-forward the current branch onto a ref (`git merge --ff-only`). Refuses if the work tree has uncommitted changes, if the ref is unknown, or if the branches have diverged. Because it only ever fast-forwards, no commit can be lost or rewritten and no merge commit is created."
    )]
    fn merge_ff_only(
        &self,
        Parameters(params): Parameters<MergeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = MergeRequest {
            r#ref: params.r#ref,
            cwd: params.cwd.map(PathBuf::from),
        };

        match gitops_git::merge::merge_ff_only(&self.runner, &request) {
            Ok(outcome) => {
                let branch = outcome.branch.as_deref().unwrap_or("HEAD");
                let mut text = if outcome.already_current {
                    format!("`{branch}` already contains `{}`.\n", outcome.merged)
                } else {
                    format!(
                        "Fast-forwarded `{branch}` to `{}`.\n{} → {}\n",
                        outcome.merged,
                        short(outcome.before.as_deref()),
                        short(outcome.after.as_deref()),
                    )
                };
                if !outcome.output.is_empty() {
                    text.push_str(&outcome.output);
                    text.push('\n');
                }
                Ok(with_structured(text, &outcome))
            }
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "push",
        description = "Publish the current branch to a remote (default `origin`). It pushes only that branch, only when every commit it would publish is signed, and only when the remote can fast-forward to it; otherwise it refuses and explains why. Nothing already on the remote is ever rewritten or removed."
    )]
    fn push(
        &self,
        Parameters(params): Parameters<PushParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = PushRequest {
            remote: params.remote,
            cwd: params.cwd.map(PathBuf::from),
        };

        match gitops_git::push::push(&self.runner, &request) {
            Ok(outcome) => {
                let mut text = if outcome.up_to_date {
                    format!(
                        "`{}` is already up to date on `{}`; nothing to push.\n",
                        outcome.branch, outcome.remote
                    )
                } else {
                    format!(
                        "Pushed `{}` to `{}` — {} commit(s), all signed.\n",
                        outcome.branch,
                        outcome.remote,
                        outcome.published.len()
                    )
                };
                if outcome.created_remote_branch {
                    text.push_str("Created the branch on the remote and set up tracking.\n");
                }
                for commit in &outcome.published {
                    text.push_str(&format!(
                        "  {} {} {}\n",
                        &commit.commit[..commit.commit.len().min(8)],
                        commit.verdict,
                        commit.subject
                    ));
                }
                if !outcome.output.is_empty() {
                    text.push_str(&outcome.output);
                    text.push('\n');
                }
                Ok(with_structured(text, &outcome))
            }
            Err(err) => Ok(failed(err)),
        }
    }
}

#[rmcp::tool_handler]
impl ServerHandler for GitOpsServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

fn summarize(status: &SigningStatus) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Signing available: {}\nCompliant: {}\n",
        status.signing_available, status.compliant
    ));
    if let Some(identity) = &status.signing_identity {
        out.push_str(&format!(
            "Signing key: {} ({})\nKey identity: {} <{}>\n",
            identity.key,
            identity.format,
            identity.name.as_deref().unwrap_or("(no name)"),
            identity.email.as_deref().unwrap_or("(no email)"),
        ));
    }
    out.push_str(&format!(
        "pre-commit hook: {}\n",
        describe_hook(
            &status.hooks,
            status.hooks.pre_commit_present,
            status.hooks.pre_commit_executable
        )
    ));
    out.push_str(&format!(
        "pre-push hook: {}\n",
        describe_hook(
            &status.hooks,
            status.hooks.pre_push_present,
            status.hooks.pre_push_executable
        )
    ));
    out.push_str(&format!(
        "git user: {} <{}>\n",
        status.user_name.effective.as_deref().unwrap_or("(unset)"),
        status.user_email.effective.as_deref().unwrap_or("(unset)"),
    ));
    if status.drift.is_empty() {
        out.push_str("Drift: none\n");
    } else {
        out.push_str("Drift:\n");
        for drift in &status.drift {
            out.push_str(&format!("- {}\n", drift.summary()));
        }
        out.push_str("Run `git_signing_enforce` to correct this.\n");
    }
    out
}

/// A hook git will not run is worth naming explicitly: it is indistinguishable from a
/// hook that passes.
fn describe_hook(hooks: &gitops_git::HooksStatus, present: bool, executable: bool) -> String {
    match (present, executable) {
        (true, true) => format!(
            "active ({})",
            hooks.directory.as_deref().unwrap_or("unknown hooks dir")
        ),
        (true, false) => "present but NOT executable — git will skip it".to_owned(),
        (false, _) => "none".to_owned(),
    }
}

/// A refused operation comes back as a tool error rather than a protocol error, so the
/// caller reads the reason and what to do about it instead of an opaque failure.
fn failed(err: gitops_git::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.to_string())])
}

fn short(id: Option<&str>) -> String {
    match id {
        Some(id) => id[..id.len().min(8)].to_owned(),
        None => "(unknown)".to_owned(),
    }
}

fn with_structured<T: serde::Serialize>(text: String, value: &T) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = serde_json::to_value(value).ok();
    result
}

fn describe_signature(code: &str) -> &'static str {
    match code {
        "G" => "good signature",
        "U" => "good signature, untrusted",
        "X" => "good signature, expired",
        "Y" => "good signature from an expired key",
        "R" => "good signature from a revoked key",
        "B" => "bad signature",
        "E" => "signature could not be checked",
        "N" => "no signature",
        _ => "unknown",
    }
}

fn internal(err: gitops_git::Error) -> ErrorData {
    ErrorData::internal_error(err.to_string(), None)
}
