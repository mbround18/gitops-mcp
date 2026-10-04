# Contributing

## Layout

```
apps/gitops-mcp/      binary crate: the MCP (rmcp) adapter, stdio transport
crates/gitops-git/    library crate: all signing-governance logic
docs/guides/          user-facing documentation
```

`apps/*` is an execution layer only. Everything worth testing lives in `crates/*`.

## Architecture

Ports and adapters, with exactly one port:

```
         ┌──────────────────────────────┐
  MCP ──▶│ gitops-git (domain)          │──▶ CommandRunner ──▶ git / gpg / ssh-keygen
 stdio   │  status · governance · commit│      (port)            (SystemRunner adapter)
         └──────────────────────────────┘                        (ScriptedRunner in tests)
```

* `runner.rs` — the `CommandRunner` port, a real `SystemRunner`, and a `ScriptedRunner`
  that replays canned output keyed by command line. The domain never touches
  `std::process` directly, so every behaviour is testable without a repository or keyring.
* `status.rs` — reads each signing-related config key at effective/global/local scope and
  classifies the gap between config and key as `Drift`.
* `identity.rs` — resolves what a signing key says about itself (OpenPGP uid, or the
  comment on an SSH public key). Pure parsers, unit-tested against real command output.
* `governance.rs` — turns `Drift` into `git config` writes. Dry-run capable.
* `commit.rs` — reconcile, stage, commit with `-S`, classify failures, report the
  signature.

### Invariants

These are the reason the crate exists. Do not relax them without a very good argument:

1. **The key is the source of truth.** Config is corrected to match the signing key's
   email, never the reverse.
2. **No unsigned fallback.** `--no-gpg-sign` appears nowhere in this codebase. When
   signing is unavailable the commit fails with an actionable error; a caller must pass
   `allow_unsigned: true` explicitly to get an unsigned commit.
3. **Reconcile before committing.** `commit` always runs governance first, and the test
   `config_is_repaired_before_the_commit_is_made` asserts the ordering.
4. **`user.name` is not governed.** A display name is a preference; the email is what the
   signature is checked against. Only `user.email`, `user.signingkey`, `gpg.format` and
   `commit.gpgsign` are policed.
5. **Hook failures are never bypassed.** `--no-verify` is never passed to git, and there
   is no parameter that would add it. A failing `pre-commit` hook comes back as
   `Error::HookRejected` carrying the hook's own output; the commit is not retried and
   nothing about the repository's hooks or config is changed. A hook git will not run
   (missing `core.hooksPath`, no executable bit) is reported as drift and deliberately
   *not* auto-corrected — both have legitimate causes, so the fix is a judgment call.
6. **stdout is the protocol.** All logging goes to stderr (`--log`, or `GITOPS_MCP_LOG`,
   sets the filter). Printing to stdout corrupts the MCP stream. Argument parsing lives in
   `main.rs` and must answer and exit — a flag that fell through to the server would leave
   the binary blocked on stdin, which is what `tests/cli.rs` guards.

## Development

```bash
make test       # unit and end-to-end tests, all hermetic
make lint       # clippy over every target, warnings denied
make fmt        # format
make check      # fmt-check + lint + test, what CI runs
make install    # reinstall the global binary
make help       # list every target
```

The targets are thin wrappers over `cargo`; run `cargo` directly whenever you want a
narrower invocation (`cargo test -p gitops-git drift`, say).

### Testing

Tests are strictly deterministic: no test touches real git config, a real keyring, or the
network. Add behaviour by scripting a `ScriptedRunner`:

```rust
let runner = ScriptedRunner::new()
    .with("git config --global user.email", 0, "llm@nowhere.invalid\n", "")
    .with("git config --global user.email me@example.com", 0, "", "");
```

Unscripted commands return a non-zero exit with an explanatory stderr, so a missing
expectation fails loudly rather than silently passing.

`ScriptedRunner::calls()` records every command line in order — use it to assert on
sequencing and to assert that something was *not* run. Several tests exist purely for that
negative: no `--no-verify`, no `--no-gpg-sign`, no `commit.gpgsign false`, no retry.

The port also covers filesystem probes (`CommandRunner::path_info`), so hook presence and
executability are scriptable too:

```rust
let runner = ScriptedRunner::new()
    .with("git rev-parse --git-path hooks", 0, "/repo/.git/hooks\n", "")
    .with_path("/repo/.git/hooks/pre-commit", true, false);  // present, not executable
```

### End-to-end tests

`apps/gitops-mcp/tests/` drives the built binary over real stdio JSON-RPC against a real
repository, because the hook guards are only worth anything if they hold against real git.

`tests/sandbox/mod.rs` builds a disposable world per test: a temporary `HOME`, a
`GIT_CONFIG_GLOBAL` of its own, `GIT_CONFIG_NOSYSTEM=1`, and a freshly generated
passphrase-free SSH signing key (`gpg.format=ssh`) with a matching `allowedSignersFile`.
That means real signed commits with a real `%G?` verdict of `G`, with no passphrase prompt
and without ever reading or writing the developer's git config or keyring.

```rust
let sandbox = Sandbox::new();
sandbox.write_hook(".git/hooks", "pre-commit", "#!/bin/sh\necho 'lint failed' >&2\nexit 1\n");
let mut server = sandbox.server();
let result = server.call("commit", json!({"message": "m", "all": true, "cwd": sandbox.repo}));
assert!(result.is_error());
assert_eq!(sandbox.git(&["rev-list", "--all", "--count"]), "0");
```

Hook semantics are per [githooks(5)](https://git-scm.com/docs/githooks): `pre-commit` runs
before the message is finalized, and a non-zero exit aborts the commit.

### Adding a tool

1. Put the logic in `crates/gitops-git`, with tests.
2. Add a parameter struct (`Deserialize + schemars::JsonSchema`, doc comments become the
   JSON Schema descriptions) and a `#[tool]` method in `apps/gitops-mcp/src/server.rs`.
3. Return `Ok(CallToolResult::error(...))` for failures the caller should read; reserve
   `Err(ErrorData)` for genuine server faults, which clients render opaquely.
4. Attach structured output with `with_structured` so callers get both prose and JSON.

### Manual protocol check

```bash
cargo build
python3 - <<'PY'
import json, subprocess
p = subprocess.Popen(["./target/debug/gitops-mcp"], stdin=subprocess.PIPE,
                     stdout=subprocess.PIPE, text=True)
def call(o):
    p.stdin.write(json.dumps(o) + "\n"); p.stdin.flush()
    return json.loads(p.stdout.readline())
print(call({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18","capabilities":{},
    "clientInfo":{"name":"smoke","version":"0"}}}))
call({"jsonrpc":"2.0","method":"notifications/initialized"})
print(call({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
PY
```

## Commits

Every commit in this repository is signed — including the ones you make while working on
it. Use the server's own `commit` tool, or `git commit -S`. If signing fails because the
key is locked, unlock it; do not work around it.
