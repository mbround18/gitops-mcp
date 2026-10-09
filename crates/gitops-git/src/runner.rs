//! The outbound port, and its adapters.
//!
//! The domain logic in this crate never touches `std::process` or `std::fs` directly; it
//! talks to [`CommandRunner`], which covers running a process, probing a path, and
//! reading and writing files.
//! Tests substitute a scripted runner, so every behaviour below is deterministic without
//! a real repository, key ring, or filesystem.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// Captured result of a single process invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    /// Trimmed stdout, or `None` when the command failed or produced nothing.
    pub fn value(&self) -> Option<String> {
        if !self.ok() {
            return None;
        }
        let trimmed = self.stdout.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }
}

/// What the domain knows about a path: enough to tell a missing hook from a hook that
/// cannot run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct PathInfo {
    pub exists: bool,
    pub executable: bool,
}

/// The single outbound port of this crate.
pub trait CommandRunner {
    /// Run `program` with `args`, optionally inside `cwd`. Returning `Err` means the
    /// process could not be spawned at all; a non-zero exit is a successful `Ok`.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> std::io::Result<CommandOutput>;

    /// Probe a path. A path that cannot be inspected reads as absent.
    fn path_info(&self, path: &Path) -> PathInfo;

    /// Read a UTF-8 file.
    fn read_file(&self, path: &Path) -> std::io::Result<String>;

    /// Write `contents` to `path`, creating parent directories. Used for the safety net
    /// kept before a destructive operation, so a failure here must abort that operation.
    fn write_file(&self, path: &Path, contents: &str) -> std::io::Result<()>;
}

/// Adapter that shells out to the real binaries on `PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> std::io::Result<CommandOutput> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // Keep signing non-interactive: a locked key must fail loudly rather than block
        // the server waiting on a TTY prompt that no MCP client can answer.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        tracing::debug!(program, ?args, ?cwd, "running command");
        let out = cmd.output()?;
        Ok(CommandOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn path_info(&self, path: &Path) -> PathInfo {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => PathInfo {
                exists: true,
                executable: meta.permissions().mode() & 0o111 != 0,
            },
            Err(_) => PathInfo::default(),
        }
    }

    fn read_file(&self, path: &Path) -> std::io::Result<String> {
        tracing::debug!(path = %path.display(), "reading file");
        std::fs::read_to_string(path)
    }

    fn write_file(&self, path: &Path, contents: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tracing::debug!(path = %path.display(), bytes = contents.len(), "writing file");
        std::fs::write(path, contents)
    }
}

/// Test adapter: replays canned output keyed by `"program arg arg ..."`.
#[derive(Debug, Default)]
pub struct ScriptedRunner {
    responses: HashMap<String, CommandOutput>,
    paths: HashMap<PathBuf, PathInfo>,
    files: HashMap<PathBuf, String>,
    calls: std::cell::RefCell<Vec<String>>,
    writes: std::cell::RefCell<Vec<(PathBuf, String)>>,
    write_fails: bool,
}

impl ScriptedRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, command: &str, status: i32, stdout: &str, stderr: &str) -> Self {
        self.responses.insert(
            command.to_owned(),
            CommandOutput {
                status,
                stdout: stdout.to_owned(),
                stderr: stderr.to_owned(),
            },
        );
        self
    }

    /// Declare what a path looks like. Undeclared paths read as absent.
    pub fn with_path(mut self, path: &str, exists: bool, executable: bool) -> Self {
        self.paths
            .insert(PathBuf::from(path), PathInfo { exists, executable });
        self
    }

    /// Seed a readable file. Paths not declared here fail to read.
    pub fn with_file(mut self, path: &str, contents: &str) -> Self {
        self.files.insert(PathBuf::from(path), contents.to_owned());
        self
    }

    /// Make every [`CommandRunner::write_file`] fail, to prove a destructive operation
    /// aborts when its safety net cannot be written.
    pub fn failing_writes(mut self) -> Self {
        self.write_fails = true;
        self
    }

    /// Every command line this runner was asked to run, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    /// Every file this runner was asked to write, in order.
    pub fn writes(&self) -> Vec<(PathBuf, String)> {
        self.writes.borrow().clone()
    }

    fn key(program: &str, args: &[&str]) -> String {
        let mut key = String::from(program);
        for arg in args {
            key.push(' ');
            key.push_str(arg);
        }
        key
    }
}

impl CommandRunner for ScriptedRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        _cwd: Option<&Path>,
    ) -> std::io::Result<CommandOutput> {
        let key = Self::key(program, args);
        self.calls.borrow_mut().push(key.clone());
        Ok(self.responses.get(&key).cloned().unwrap_or(CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: format!("scripted runner has no response for `{key}`"),
        }))
    }

    fn path_info(&self, path: &Path) -> PathInfo {
        self.paths.get(path).copied().unwrap_or_default()
    }

    fn read_file(&self, path: &Path) -> std::io::Result<String> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "scripted file"))
    }

    fn write_file(&self, path: &Path, contents: &str) -> std::io::Result<()> {
        if self.write_fails {
            return Err(std::io::Error::other("scripted write failure"));
        }
        self.writes
            .borrow_mut()
            .push((path.to_path_buf(), contents.to_owned()));
        Ok(())
    }
}

/// Where a command should run. `None` means "inherit the server's working directory".
pub type WorkDir = Option<PathBuf>;
