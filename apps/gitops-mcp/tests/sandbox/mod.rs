//! A disposable git world: its own HOME, its own global config, its own signing key.
//!
//! Every git and server invocation here runs with `HOME`, `GIT_CONFIG_GLOBAL` and
//! `GIT_CONFIG_NOSYSTEM` pointed at the sandbox, so these tests never read or write the
//! developer's real git config or keyring.
//!
//! Signing uses a freshly generated, passphrase-free SSH key (`gpg.format=ssh`), so real
//! signed commits happen without a passphrase prompt.
//!
//! Each test binary compiles this module separately and uses only part of it, so unused
//! helpers here are expected rather than dead.
#![allow(dead_code)]

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

pub const EMAIL: &str = "hooks-test@example.com";

pub struct Sandbox {
    home: tempfile::TempDir,
    pub repo: PathBuf,
}

impl Sandbox {
    /// Create a HOME with a signing key and global config, plus an initialized repo
    /// holding one unstaged file.
    pub fn new() -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let home_path = home.path().to_path_buf();
        let key = home_path.join("id_ed25519");

        run(
            &home_path,
            &home_path,
            "ssh-keygen",
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                EMAIL,
                "-f",
                key.to_str().unwrap(),
            ],
        );

        let public = std::fs::read_to_string(home_path.join("id_ed25519.pub")).unwrap();
        let allowed = home_path.join("allowed_signers");
        std::fs::write(&allowed, format!("{EMAIL} {}", public.trim())).unwrap();

        std::fs::write(
            home_path.join("gitconfig"),
            format!(
                "[user]\n\tname = Hooks Test\n\temail = {EMAIL}\n\tsigningkey = {key}.pub\n\
                 [gpg]\n\tformat = ssh\n\
                 [gpg \"ssh\"]\n\tallowedSignersFile = {allowed}\n\
                 [commit]\n\tgpgsign = true\n\
                 [init]\n\tdefaultBranch = main\n",
                key = key.display(),
                allowed = allowed.display(),
            ),
        )
        .unwrap();

        let repo = home_path.join("repo");
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &home_path, "git", &["init", "-q"]);

        let sandbox = Self { home, repo };
        sandbox.write("README.md", "# sandbox\n");
        sandbox
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    /// The sandbox HOME, for building paths outside the repository.
    pub fn home_path(&self) -> &Path {
        self.home.path()
    }

    /// Write a file inside the repository.
    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.repo.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// Install an executable hook. `dir` is relative to the repo, e.g. `.git/hooks`.
    pub fn write_hook(&self, dir: &str, name: &str, script: &str) -> PathBuf {
        let path = self.repo.join(dir).join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, script).unwrap();
        set_executable(&path, true);
        path
    }

    /// Create a bare repository inside the sandbox and add it as a remote, so push tests
    /// are real pushes with no network involved.
    pub fn bare_remote(&self, name: &str) -> PathBuf {
        let path = self.home().join(format!("{name}.git"));
        std::fs::create_dir_all(&path).unwrap();
        run(&path, self.home(), "git", &["init", "-q", "--bare"]);
        self.git(&["remote", "add", name, path.to_str().unwrap()]);
        path
    }

    /// Run a git command in the bare remote at `path` and return trimmed stdout.
    pub fn remote_git(&self, path: &Path, args: &[&str]) -> String {
        self.run_in(path, args)
    }

    /// Run a git command in any directory inside the sandbox.
    pub fn run_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = command(dir, self.home(), "git", args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} in {dir:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Clone `source` to `target`, standing in for a second developer.
    pub fn clone_to(&self, target: &Path, source: &Path) {
        std::fs::create_dir_all(target).unwrap();
        self.run_in(
            self.home(),
            &[
                "clone",
                "-q",
                source.to_str().unwrap(),
                target.to_str().unwrap(),
            ],
        );
    }

    /// Run a git command in the sandbox and return trimmed stdout.
    pub fn git(&self, args: &[&str]) -> String {
        let out = command(&self.repo, self.home(), "git", args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Start the MCP server against this sandbox and complete the handshake.
    pub fn server(&self) -> Server {
        let mut child = command(
            &self.repo,
            self.home(),
            env!("CARGO_BIN_EXE_gitops-mcp"),
            &[],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gitops-mcp");

        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut server = Server {
            child,
            stdout,
            next_id: 1,
        };
        server.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "hooks-test", "version": "0"}
            }),
        );
        server.notify("notifications/initialized");
        server
    }
}

pub struct Server {
    child: Child,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Server {
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        let response: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad response `{line}`: {e}"));
        response["result"].clone()
    }

    fn notify(&mut self, method: &str) {
        self.send(serde_json::json!({"jsonrpc": "2.0", "method": method}));
    }

    fn send(&mut self, value: serde_json::Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    /// Call a tool and return its result.
    pub fn call(&mut self, tool: &str, arguments: serde_json::Value) -> ToolResult {
        ToolResult(self.request(
            "tools/call",
            serde_json::json!({"name": tool, "arguments": arguments}),
        ))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct ToolResult(serde_json::Value);

impl ToolResult {
    pub fn is_error(&self) -> bool {
        self.0["isError"].as_bool().unwrap_or(false)
    }

    pub fn text(&self) -> String {
        self.0["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    pub fn structured(&self) -> &serde_json::Value {
        &self.0["structuredContent"]
    }
}

pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn set_executable(path: &Path, executable: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn command(cwd: &Path, home: &Path, program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", home.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GNUPGHOME");
    cmd
}

fn run(cwd: &Path, home: &Path, program: &str, args: &[&str]) {
    let out = command(cwd, home, program, args).output().expect(program);
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
