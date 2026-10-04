# gitops

An MCP server that keeps git commit signing honest, so you can stop babysitting it.

## Why this exists

Required commit signing and a coding agent are a bad combination. The agent hits a
signing error, decides the config is the problem, and "fixes" it — `user.email` rewritten,
`commit.gpgsign` flipped to `false`, a local override left behind that quietly breaks
every later commit in that repo. Or it asks for permission to run `git config` for the
fifth time in an hour, and you spend the session arbitrating config changes instead of
steering the actual work.

Either way the cost is the same: more time spent policing `git config` than reviewing
code.

`gitops` takes the decision away from the agent. Your signing key is the source of truth,
config is reconciled to the key before every commit, and there is no unsigned fallback to
reach for. The agent gets one tool that always works and no reason to touch `git config`
at all — so "may I change your git config?" stops being a question anyone has to answer.

## What it does

| Tool | Behavior |
| --- | --- |
| `git_signing_status` | Reports `git config commit.gpgsign` and `git config user.signingkey` verbatim, every time, plus the value at each scope, the signing key's own identity, and any drift. |
| `git_signing_enforce` | Rewrites global git config to match the signing key. `dry_run: true` shows the plan without writing. |
| `commit` | Stages `files` (or everything with `all: true`), reconciles config, and creates a **signed** commit. Never falls back to an unsigned commit. |

The rules it enforces:

* `commit.gpgsign` stays `true` globally, and a repo that turned it off gets fixed.
* `user.email` matches the signing key's own email — the key wins, not the config.
* `user.name` is left alone. A display name is a preference; the email is what the
  signature is attributed by.
* No signing key, or a locked one? The commit **fails** with an actionable error.
  `--no-gpg-sign` appears nowhere in this codebase.

## Getting started

### Requirements

* Rust (stable) and `cargo`
* A working signing key: GPG (`gpg.format=openpgp`, the default) or SSH (`gpg.format=ssh`)
* `git config user.signingkey` set to that key

Check where you stand — if `user.signingkey` is empty, set it first
([GitHub's guide](https://docs.github.com/en/authentication/managing-commit-signature-verification)):

```bash
git config --global user.signingkey   # e.g. 354F34B4DB349BD6, or a path to an SSH key
git config --global gpg.format        # openpgp (default) or ssh
```

### Install

```bash
git clone git@github.com:mbround18/gitops-mcp.git
cd gitops-mcp
cargo install --path apps/gitops-mcp      # → ~/.cargo/bin/gitops-mcp
```

### Register with your client

Claude Code, available in every project:

```bash
claude mcp add --scope user gitops ~/.cargo/bin/gitops-mcp
claude mcp list        # gitops: ... - ✔ Connected
```

Any other MCP client — it is a plain stdio server:

```jsonc
{
  "mcpServers": {
    "gitops": { "command": "/home/you/.cargo/bin/gitops-mcp" }
  }
}
```

To get the full benefit, also tell the agent to *use* it instead of raw `git`:
[docs/guides/claude-code.md](docs/guides/claude-code.md) has the `CLAUDE.md` rules to
paste in.

### First run

Ask for signing status. You should see your key, your email, and `Drift: none`:

```
$ git config commit.gpgsign
true
$ git config user.signingkey
354F34B4DB349BD6

Signing available: true
Compliant: true
Signing key: 354F34B4DB349BD6 (openpgp)
Key identity: Your Name <you@example.com>
git user: your-handle <you@example.com>
Drift: none
```

If it reports drift, run `git_signing_enforce` with `dry_run: true` to see what it would
change, then again without it.

### Committing

```jsonc
// specific paths
{ "message": "fix: handle empty staging area", "files": ["crates/gitops-git/src/commit.rs"] }

// everything in the working tree, including untracked files
{ "message": "chore: sweep", "all": true }
```

`files` and `all` are mutually exclusive, and one is required — there is no implicit
"commit whatever happens to be staged". You get back the commit id and git's own
signature verdict.

If signing is unavailable, the call fails and tells you to unlock the key. That is the
feature, not a bug.

## Documentation

* [Git signing governance](docs/guides/git-signing-governance.md) — every rule, what it
  will and will not touch, and troubleshooting.
* [Using it with Claude Code](docs/guides/claude-code.md) — installation and the
  `CLAUDE.md` rules that make the agent reach for it.
* [CONTRIBUTING.md](CONTRIBUTING.md) — architecture and development.

## License

BSD 3-Clause. See [LICENSE](LICENSE).
