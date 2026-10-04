//! Port + adapter for running external processes.
//!
//! The domain logic in this crate never touches `std::process` directly; it talks to
//! [`CommandRunner`]. Tests substitute a scripted runner, so every behaviour below is
//! deterministic without a real repository, key ring, or network.

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
}

/// Test adapter: replays canned output keyed by `"program arg arg ..."`.
#[derive(Debug, Default)]
pub struct ScriptedRunner {
    responses: HashMap<String, CommandOutput>,
    calls: std::cell::RefCell<Vec<String>>,
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

    /// Every command line this runner was asked to run, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
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
}

/// Where a command should run. `None` means "inherit the server's working directory".
pub type WorkDir = Option<PathBuf>;
