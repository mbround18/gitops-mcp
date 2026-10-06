//! Safe sibling-workspace operations for multi-checkout workflows.
//!
//! These helpers cover the repetitive loop of discovering sibling workspaces, exporting
//! tracked deltas from one workspace, applying those deltas in another, and cleaning up
//! temporary sibling checkouts behind explicit confirmation.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, merge::combined, runner::CommandRunner};

pub const DEFAULT_PREFIX: &str = "ThunderForgeVTT";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceScanRequest {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub base_dir: Option<PathBuf>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceSummary {
    pub workspace: String,
    pub path: String,
    pub branch: String,
    pub head: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
    pub dirty: bool,
    pub tracked_changes: usize,
    pub untracked_changes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceScanOutcome {
    pub current_repo: String,
    pub current_head: String,
    pub base_dir: String,
    pub prefix: String,
    pub workspaces: Vec<WorkspaceSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDiffExportRequest {
    pub workspace: String,
    pub output: String,
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceDiffExportOutcome {
    pub workspace: String,
    pub output: String,
    pub base_ref: String,
    pub bytes: usize,
    pub untracked_files: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceApplyRequest {
    pub patch: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub three_way: bool,
    #[serde(default)]
    pub index: bool,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceApplyOutcome {
    pub patch: String,
    pub dry_run: bool,
    pub three_way: bool,
    pub index: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CleanupMode {
    #[default]
    Delete,
    Quarantine,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCleanupRequest {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub base_dir: Option<PathBuf>,
    #[serde(default)]
    pub confirm: bool,
    #[serde(default)]
    pub mode: CleanupMode,
    #[serde(default)]
    pub quarantine_dir: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceCleanupAction {
    pub source: String,
    pub action: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceCleanupOutcome {
    pub mode: CleanupMode,
    pub actions: Vec<WorkspaceCleanupAction>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceValidateRequest {
    pub commands: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceValidateStep {
    pub command: String,
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceValidateOutcome {
    pub steps: Vec<WorkspaceValidateStep>,
}

pub fn workspace_scan(
    runner: &dyn CommandRunner,
    request: &WorkspaceScanRequest,
) -> Result<WorkspaceScanOutcome> {
    let cwd = request.cwd.as_deref();
    let current_repo = repo_root(runner, cwd)?;
    let current_head = git_value(runner, &["rev-parse", "HEAD"], Some(current_repo.as_path()))?;
    let prefix = normalized_prefix(request.prefix.as_deref());
    let base_dir = resolve_base_dir(&current_repo, request.base_dir.as_deref())?;
    let current_name = current_repo
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::WorkspaceOperationFailed {
            detail: "current repository has no usable directory name".into(),
        })?
        .to_owned();
    let inv =
        workspaceops::inventory(&current_repo, &prefix, Some(&base_dir), &[]).map_err(|e| {
            Error::WorkspaceOperationFailed {
                detail: format!("workspace inventory failed: {e}"),
            }
        })?;
    let mut dirs: Vec<PathBuf> = inv
        .siblings
        .into_iter()
        .filter(|s| s.has_git_dir && s.name != current_name)
        .map(|s| PathBuf::from(s.path))
        .collect();
    dirs.sort();

    let mut workspaces = Vec::new();
    for dir in dirs {
        let workspace = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        let branch = git_value(runner, &["rev-parse", "--abbrev-ref", "HEAD"], Some(&dir))
            .unwrap_or_else(|_| "detached".into());
        let head = git_value(runner, &["rev-parse", "HEAD"], Some(&dir))
            .unwrap_or_else(|_| "unknown".into());

        let status_out = runner
            .run("git", &["status", "--porcelain"], Some(&dir))
            .map_err(Error::from)?;
        let status_text = if status_out.ok() {
            status_out.stdout
        } else {
            String::new()
        };
        let (tracked_changes, untracked_changes) = parse_porcelain_counts(&status_text);

        let mut ahead = None;
        let mut behind = None;
        let has_current = runner
            .run(
                "git",
                &["cat-file", "-e", &format!("{current_head}^{{commit}}")],
                Some(&dir),
            )
            .map_err(Error::from)?
            .ok();
        if has_current {
            let rev = runner
                .run(
                    "git",
                    &[
                        "rev-list",
                        "--left-right",
                        "--count",
                        &format!("{current_head}...HEAD"),
                    ],
                    Some(&dir),
                )
                .map_err(Error::from)?;
            if rev.ok() {
                let nums: Vec<&str> = rev.stdout.split_whitespace().collect();
                if nums.len() == 2 {
                    behind = nums[0].parse::<usize>().ok();
                    ahead = nums[1].parse::<usize>().ok();
                }
            }
        }

        workspaces.push(WorkspaceSummary {
            workspace,
            path: dir.display().to_string(),
            branch,
            head,
            ahead,
            behind,
            dirty: tracked_changes > 0 || untracked_changes > 0,
            tracked_changes,
            untracked_changes,
        });
    }

    Ok(WorkspaceScanOutcome {
        current_repo: current_repo.display().to_string(),
        current_head,
        base_dir: base_dir.display().to_string(),
        prefix,
        workspaces,
    })
}

pub fn workspace_diff_export(
    runner: &dyn CommandRunner,
    request: &WorkspaceDiffExportRequest,
) -> Result<WorkspaceDiffExportOutcome> {
    let cwd = request.cwd.as_deref();
    let workspace = resolve_path(cwd, &request.workspace)?;
    if !workspace.join(".git").exists() {
        return Err(Error::WorkspaceOperationFailed {
            detail: format!(
                "workspace is not a git working tree: {}",
                workspace.display()
            ),
        });
    }
    let base_ref = request.base_ref.as_deref().unwrap_or("HEAD").to_owned();
    let check = runner
        .run(
            "git",
            &["rev-parse", "--verify", &base_ref],
            Some(&workspace),
        )
        .map_err(Error::from)?;
    if !check.ok() {
        return Err(Error::WorkspaceOperationFailed {
            detail: format!(
                "base ref `{base_ref}` is not valid in `{}`",
                workspace.display()
            ),
        });
    }

    let diff = runner
        .run(
            "git",
            &["diff", "--binary", "--full-index", "--patch", &base_ref],
            Some(&workspace),
        )
        .map_err(Error::from)?;
    if !diff.ok() {
        return Err(Error::WorkspaceOperationFailed {
            detail: format!(
                "diff export failed: {}",
                combined(&diff.stdout, &diff.stderr)
            ),
        });
    }

    let output = resolve_path(cwd, &request.output)?;
    runner
        .write_file(&output, &diff.stdout)
        .map_err(Error::from)?;

    let untracked_out = runner
        .run(
            "git",
            &["ls-files", "--others", "--exclude-standard"],
            Some(&workspace),
        )
        .map_err(Error::from)?;
    let untracked_files = if untracked_out.ok() {
        untracked_out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else {
        Vec::new()
    };

    Ok(WorkspaceDiffExportOutcome {
        workspace: workspace.display().to_string(),
        output: output.display().to_string(),
        base_ref,
        bytes: diff.stdout.len(),
        untracked_files,
    })
}

pub fn workspace_apply(
    runner: &dyn CommandRunner,
    request: &WorkspaceApplyRequest,
) -> Result<WorkspaceApplyOutcome> {
    let cwd = request.cwd.as_deref();
    let patch = resolve_path(cwd, &request.patch)?;
    if !patch.exists() {
        return Err(Error::WorkspaceOperationFailed {
            detail: format!("patch file does not exist: {}", patch.display()),
        });
    }
    let patch_str = patch.display().to_string();
    let mut args = vec!["apply"];
    if request.three_way {
        args.push("--3way");
    }
    if request.index {
        args.push("--index");
    }
    if request.dry_run {
        args.push("--check");
    }
    args.push(&patch_str);
    let out = runner.run("git", &args, cwd).map_err(Error::from)?;
    if !out.ok() {
        return Err(Error::WorkspaceOperationFailed {
            detail: format!("patch apply failed: {}", combined(&out.stdout, &out.stderr)),
        });
    }

    Ok(WorkspaceApplyOutcome {
        patch: patch_str,
        dry_run: request.dry_run,
        three_way: request.three_way,
        index: request.index,
    })
}

pub fn workspace_cleanup(request: &WorkspaceCleanupRequest) -> Result<WorkspaceCleanupOutcome> {
    if !request.confirm {
        return Err(Error::InvalidRequest(
            "cleanup refused: pass `confirm: true` to allow deleting or quarantining workspaces"
                .into(),
        ));
    }

    let cwd = request.cwd.as_deref();
    let current_repo = request
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| Error::WorkspaceOperationFailed {
            detail: "could not determine current directory for cleanup".into(),
        })?;
    let prefix = normalized_prefix(request.prefix.as_deref());
    let base_dir = match request.base_dir.clone() {
        Some(path) => path,
        None => current_repo
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| Error::WorkspaceOperationFailed {
                detail: "could not resolve parent directory for cleanup".into(),
            })?,
    };
    let base_canon = fs::canonicalize(&base_dir).map_err(|e| Error::WorkspaceOperationFailed {
        detail: format!(
            "could not resolve cleanup base directory `{}`: {e}",
            base_dir.display()
        ),
    })?;
    let current_canon =
        fs::canonicalize(&current_repo).map_err(|e| Error::WorkspaceOperationFailed {
            detail: format!(
                "could not resolve current repo path `{}`: {e}",
                current_repo.display()
            ),
        })?;

    let mut victims: Vec<PathBuf> = fs::read_dir(&base_dir)
        .map_err(|e| Error::WorkspaceOperationFailed {
            detail: format!(
                "could not read cleanup base directory `{}`: {e}",
                base_dir.display()
            ),
        })?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|name| name.starts_with(&format!("{prefix}-")))
                .unwrap_or(false)
        })
        .filter(|p| p.join(".git").exists())
        .collect();
    victims.sort();

    let quarantine_root = if request.mode == CleanupMode::Quarantine {
        let q = request.quarantine_dir.as_deref().ok_or_else(|| {
            Error::InvalidRequest("quarantine mode requires `quarantine_dir`".into())
        })?;
        let path = resolve_path(cwd, q)?;
        fs::create_dir_all(&path).map_err(|e| Error::WorkspaceOperationFailed {
            detail: format!(
                "could not create quarantine directory `{}`: {e}",
                path.display()
            ),
        })?;
        Some(path)
    } else {
        None
    };

    let mut actions = Vec::new();
    for victim in victims {
        let victim_canon =
            fs::canonicalize(&victim).map_err(|e| Error::WorkspaceOperationFailed {
                detail: format!("could not resolve victim path `{}`: {e}", victim.display()),
            })?;
        if !victim_canon.starts_with(&base_canon) {
            return Err(Error::WorkspaceOperationFailed {
                detail: format!(
                    "refusing to clean `{}` because it does not live under `{}`",
                    victim.display(),
                    base_canon.display()
                ),
            });
        }
        if victim_canon == current_canon {
            continue;
        }

        match request.mode {
            CleanupMode::Delete => {
                fs::remove_dir_all(&victim).map_err(|e| Error::WorkspaceOperationFailed {
                    detail: format!("failed deleting `{}`: {e}", victim.display()),
                })?;
                actions.push(WorkspaceCleanupAction {
                    source: victim.display().to_string(),
                    action: "deleted".into(),
                    target: None,
                });
            }
            CleanupMode::Quarantine => {
                let root = quarantine_root.as_ref().expect("quarantine root set");
                let stamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let target = root.join(format!(
                    "{}.{stamp}",
                    victim
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("workspace")
                ));
                fs::rename(&victim, &target).map_err(|e| Error::WorkspaceOperationFailed {
                    detail: format!(
                        "failed moving `{}` to quarantine `{}`: {e}",
                        victim.display(),
                        target.display()
                    ),
                })?;
                actions.push(WorkspaceCleanupAction {
                    source: victim.display().to_string(),
                    action: "quarantined".into(),
                    target: Some(target.display().to_string()),
                });
            }
        }
    }

    Ok(WorkspaceCleanupOutcome {
        mode: request.mode,
        actions,
    })
}

pub fn workspace_validate(
    runner: &dyn CommandRunner,
    request: &WorkspaceValidateRequest,
) -> Result<WorkspaceValidateOutcome> {
    if request.commands.is_empty() {
        return Err(Error::InvalidRequest(
            "validation requires at least one command".into(),
        ));
    }
    let cwd = request.cwd.as_deref();
    let mut steps = Vec::new();
    for command in &request.commands {
        let out = runner
            .run("bash", &["-lc", command], cwd)
            .map_err(Error::from)?;
        let step = WorkspaceValidateStep {
            command: command.clone(),
            status: out.status,
            stdout: out.stdout.clone(),
            stderr: out.stderr.clone(),
        };
        if !out.ok() {
            return Err(Error::WorkspaceOperationFailed {
                detail: format!(
                    "validation command failed (`{}`): {}",
                    command,
                    combined(&out.stdout, &out.stderr)
                ),
            });
        }
        steps.push(step);
    }

    Ok(WorkspaceValidateOutcome { steps })
}

fn repo_root(runner: &dyn CommandRunner, cwd: Option<&Path>) -> Result<PathBuf> {
    let out = runner.run("git", &["rev-parse", "--show-toplevel"], cwd)?;
    let root = out.value().ok_or(Error::NotARepository)?;
    Ok(PathBuf::from(root))
}

fn git_value(runner: &dyn CommandRunner, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let out = runner.run("git", args, cwd)?;
    if out.ok() {
        return Ok(out.stdout.trim().to_owned());
    }
    Err(Error::WorkspaceOperationFailed {
        detail: format!(
            "`git {}` failed: {}",
            args.join(" "),
            combined(&out.stdout, &out.stderr)
        ),
    })
}

fn parse_porcelain_counts(status: &str) -> (usize, usize) {
    let mut tracked = 0usize;
    let mut untracked = 0usize;
    for line in status.lines() {
        if line.starts_with("??") {
            untracked += 1;
        } else if !line.trim().is_empty() {
            tracked += 1;
        }
    }
    (tracked, untracked)
}

fn resolve_base_dir(current_repo: &Path, provided: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = provided {
        return Ok(dir.to_path_buf());
    }
    current_repo
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::WorkspaceOperationFailed {
            detail: "could not determine base directory (repository has no parent)".into(),
        })
}

fn normalized_prefix(prefix: Option<&str>) -> String {
    let value = prefix.unwrap_or(DEFAULT_PREFIX).trim();
    if value.is_empty() {
        DEFAULT_PREFIX.to_owned()
    } else {
        value.to_owned()
    }
}

fn resolve_path(cwd: Option<&Path>, path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return Ok(path);
    }
    if let Some(cwd) = cwd {
        return Ok(cwd.join(path));
    }
    let here = std::env::current_dir().map_err(|e| Error::WorkspaceOperationFailed {
        detail: format!("could not determine current directory: {e}"),
    })?;
    Ok(here.join(path))
}
