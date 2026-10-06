# Working in this repository

`gitops-mcp` exists to stop agents from bypassing commit signing and git hooks. Holding
that line while working on the tool itself is the bar.

## Commit through this server

This repo registers itself as a project-scope MCP server (`.mcp.json`), so after
`make install` the tools are available here:

* `mcp__gitops__commit` — `{message, files: [...]}` or `{message, all: true}`. Use it
  instead of `git commit`.
* `mcp__gitops__restore` — `{files: [...]}` instead of `git checkout -- <path>`.
* `mcp__gitops__merge_ff_only` — `{ref}` instead of `git merge`.
* `mcp__gitops__push` — `{}` instead of `git push`.
* `mcp__gitops__diff` — `{}`, or `{patch: true}` for the hunks, instead of `git diff`.
* `mcp__gitops__git_signing_status` — before touching anything signing-related.
* `mcp__gitops__git_signing_enforce` — the only way config gets repaired.

Never run `git config` to set `user.email`, `user.name`, `user.signingkey`, `gpg.format`
or `commit.gpgsign`. Never bypass a hook or sign-off check by hand. If a hook rejects a
commit, fix what the hook reported; if signing fails, ask for the key to be unlocked.
Routing around either is the bug this repo was written to prevent.

When one of these tools refuses, the refusal is the answer. Report what it said and let the
author decide — do not reach for a shell to do the same operation without the checks, and
do not add a parameter to the tool to make the refusal go away.

## Invariants

The nine rules in [CONTRIBUTING.md](CONTRIBUTING.md#invariants) are the product, not style
preferences. Before changing behaviour in `crates/gitops-git`, read them. In short: the key
is the source of truth, there is no unsigned fallback, reconcile precedes commit,
`user.name` is not governed, hook failures are never bypassed, a safeguard is never a
parameter, a destructive operation keeps a way back, published history is not the server's
to rewrite, and stdout belongs to the protocol.

## Layout

* `crates/gitops-git` — all logic, behind the `CommandRunner` port. New behaviour goes
  here, with tests.
* `apps/gitops-mcp` — rmcp adapter only. Keep it thin.

## Checks

```bash
make check     # fmt-check + clippy (warnings denied) + the full test suite
make test      # tests only
make help      # every target
```

A new safeguard is not finished until a test asserts the command line it prevents, the way
`nothing_this_server_runs_can_overwrite_published_history` does.

Unit tests script a `ScriptedRunner`; end-to-end tests in `apps/gitops-mcp/tests/` drive
the built binary against a real repo in a disposable `HOME`. Both must stay deterministic
and must never read or write the developer's real git config or keyring.

After changing the code, reinstall — the registered server runs the installed binary, not
`target/debug`:

```bash
make install
```

## Docs

User-facing behaviour goes in `docs/guides/*`, developer detail in `CONTRIBUTING.md`, and
`README.md` stays short. A behaviour change that users can observe is not finished until
the matching guide says so.
