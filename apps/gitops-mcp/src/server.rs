//! MCP adapter: exposes `gitops-git` over the Model Context Protocol.

use std::path::PathBuf;

use gitops_git::{CommitRequest, SigningStatus, SystemRunner, reconcile};
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
config first. It never falls back to an unsigned commit.";

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
    /// Permit an unsigned commit when no usable signing key exists. Leave this off:
    /// an unsigned commit is a governance failure, not a fallback.
    #[serde(default)]
    pub allow_unsigned: bool,
    /// Repository directory to commit in. Defaults to the server's working directory.
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
        let result =
            reconcile(&self.runner, cwd.as_deref(), params.dry_run).map_err(internal)?;

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
            allow_unsigned: params.allow_unsigned,
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
                    text.push_str(&format!(
                        "Signature: {sig} ({})\n",
                        describe_signature(sig)
                    ));
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
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(
                err.to_string(),
            )])),
        }
    }
}

#[rmcp::tool_handler]
impl ServerHandler for GitOpsServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
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
        describe_pre_commit(&status.hooks)
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
fn describe_pre_commit(hooks: &gitops_git::HooksStatus) -> String {
    match (hooks.pre_commit_present, hooks.pre_commit_executable) {
        (true, true) => format!(
            "active ({})",
            hooks.directory.as_deref().unwrap_or("unknown hooks dir")
        ),
        (true, false) => "present but NOT executable — git will skip it".to_owned(),
        (false, _) => "none".to_owned(),
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
