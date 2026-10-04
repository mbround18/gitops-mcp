# gitops

An MCP server that keeps git commit signing honest.

Agents are good at rewriting `user.email` and flipping `commit.gpgsign` off when a commit
fails. `gitops` makes that pointless: your signing key is the source of truth, and every
commit made through this server reconciles git config back to the key first.

## Tools

| Tool | What it does |
| --- | --- |
| `git_signing_status` | Reports `git config commit.gpgsign` and `git config user.signingkey` verbatim, plus every scope, the signing key's own identity, and any drift. |
| `git_signing_enforce` | Rewrites global git config to match the signing key. `dry_run: true` shows the plan without writing. |
| `commit` | Stages `files` (or everything with `all: true`), reconciles config, and creates a **signed** commit. Never falls back to an unsigned commit. |

## Install

```bash
cargo install --path apps/gitops-mcp
claude mcp add --scope user gitops ~/.cargo/bin/gitops-mcp
```

## Use

```jsonc
// commit specific paths
{ "message": "fix: handle empty staging area", "files": ["src/commit.rs"] }

// commit everything
{ "message": "chore: sweep", "all": true }
```

If signing is unavailable — no key, or a locked one — the call fails and tells you to
unlock the key. That is the point.

See [docs/guides/git-signing-governance.md](docs/guides/git-signing-governance.md) for the
governance rules, and [CONTRIBUTING.md](CONTRIBUTING.md) to work on the code.

## License

BSD 3-Clause. See [LICENSE](LICENSE).
