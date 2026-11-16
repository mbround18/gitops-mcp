//! The outbound port, and its adapters.
//!
//! The domain logic in this crate never touches `std::process` or `std::fs` directly; it
//! talks to [`CommandRunner`], which covers running a process, probing a path, and
//! reading and writing files.
//! Tests substitute a scripted runner, so every behaviour below is deterministic without
//! a real repository, key ring, or filesystem.

use std::{
    collections::HashMap,
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Environment variable that overrides [`DEFAULT_NETWORK_TIMEOUT`], in whole seconds.
pub const NETWORK_TIMEOUT_ENV: &str = "GITOPS_MCP_NETWORK_TIMEOUT_SECS";

/// How long a command that talks to a remote may run before it is killed.
pub const DEFAULT_NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

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

    /// Run a command that talks to a remote (`push`, `ls-remote`). Its transport (ssh,
    /// a proxy command, a credential helper) is outside this server's control and can
    /// wait forever, so an adapter should bound it. A command that overruns is reported
    /// as a failed `Ok` whose stderr says so; it is not left running.
    fn run_network(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> std::io::Result<CommandOutput> {
        self.run(program, args, cwd)
    }

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

impl SystemRunner {
    fn command(program: &str, args: &[&str], cwd: Option<&Path>) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // Keep everything non-interactive: a locked key, an unknown host key or a
        // credential request must fail loudly rather than block the server waiting on a
        // prompt that no MCP client can answer.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        // The MCP client launches this server inside its own terminal session, so every
        // child would otherwise inherit that controlling terminal. ssh (passphrase,
        // host-key confirmation), a ProxyCommand or pinentry then prompt on /dev/tty --
        // a terminal the client owns and nobody is reading for them -- and wait forever,
        // invisibly, while their stderr stays empty. A new session has no controlling
        // terminal, so those prompts fail at once with an error that names the cause. It
        // also makes the child a process-group leader, so a timeout can kill the whole
        // tree (git, ssh, its proxy) instead of orphaning the grandchildren.
        // SAFETY: `setsid` is async-signal-safe and touches no memory of this process.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        tracing::debug!(program, ?args, ?cwd, "running command");
        cmd
    }

    /// Run with both pipes drained concurrently, killing the process group at `limit`.
    fn run_bounded(
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limit: Duration,
    ) -> std::io::Result<CommandOutput> {
        let mut child = Self::command(program, args, cwd).spawn()?;
        let drain = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_end(&mut buf);
                }
                String::from_utf8_lossy(&buf).into_owned()
            })
        };
        let stdout = drain(
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let stderr = drain(
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );

        let deadline = Instant::now() + limit;
        let mut timed_out = false;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                // The child leads its own process group (see `command`); take the
                // grandchildren with it so they release the pipes.
                // SAFETY: plain syscall on a pid this process spawned and has not reaped.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                break child.wait()?;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let stdout = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();
        if !timed_out {
            return Ok(output(status, stdout, stderr));
        }
        let mut stderr = trimmed_end(stderr);
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "`{program} {}` was still running after {}s and was killed. Its transport is \
             waiting on something that never answered (a network stall, or a prompt for a \
             passphrase, host key or login). Run the same command in a terminal to see \
             what it asks for. Set {NETWORK_TIMEOUT_ENV} to allow more time.",
            args.join(" "),
            limit.as_secs()
        ));
        Ok(CommandOutput {
            status: -1,
            stdout,
            stderr,
        })
    }
}

fn trimmed_end(mut text: String) -> String {
    text.truncate(text.trim_end().len());
    text
}

/// The configured bound for network commands.
pub fn network_timeout() -> Duration {
    std::env::var(NETWORK_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_NETWORK_TIMEOUT)
}

fn output(status: std::process::ExitStatus, stdout: String, mut stderr: String) -> CommandOutput {
    use std::os::unix::process::ExitStatusExt;
    // A process killed by a signal has no exit code and usually printed nothing; say so,
    // or the caller is left reporting an empty error.
    if let Some(signal) = status.signal() {
        stderr = trimmed_end(stderr);
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!("terminated by signal {signal}"));
    }
    CommandOutput {
        status: status.code().unwrap_or(-1),
        stdout,
        stderr,
    }
}

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> std::io::Result<CommandOutput> {
        let out = Self::command(program, args, cwd).output()?;
        Ok(output(
            out.status,
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    fn run_network(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> std::io::Result<CommandOutput> {
        Self::run_bounded(program, args, cwd, network_timeout())
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
