# Using gitops with Claude Code

This guide covers installing the server for Claude Code and — the part that actually
matters — configuring Claude so it reaches for the server instead of running `git` and
`git config` by hand.

## Why bother wiring it into the agent

An agent with a signing requirement and no governance tool has two failure modes, and you
pay for both:

1. **It "fixes" your config.** A commit fails to sign, so it rewrites `user.email`, sets
   `commit.gpgsign=false`, or leaves a local override behind. The commit succeeds, your
   global config is now wrong, and you find out three repos later. The same reflex applies
   to a failing `pre-commit` hook: `--no-verify`, or `chmod -x`, and the guardrail is gone.
2. **It asks you instead.** Every `git config` write becomes a permission prompt you have
   to read and adjudicate. Multiply by a long session and you have spent more time
   arbitrating config changes than steering the work.

Installing the server fixes the first problem — `commit` can't produce an unsigned commit
and reconciles config to your key first. Adding the `CLAUDE.md` rules below fixes the
second, by removing the agent's reason to touch `git config` at all.

## 1. Install the binary

```bash
cargo install --path apps/gitops-mcp
ls ~/.cargo/bin/gitops-mcp
```

## 2. Register it at user scope

User scope makes the server available in every project, which is what you want — signing
governance is not a per-repo concern:

```bash
claude mcp add --scope user gitops ~/.cargo/bin/gitops-mcp
```

Verify:

```bash
claude mcp list
# gitops: /home/you/.cargo/bin/gitops-mcp - ✔ Connected
```

The tools appear as `mcp__gitops__git_signing_status`,
`mcp__gitops__git_signing_enforce`, and `mcp__gitops__commit`.

> A session that was already running when you registered the server will not see it. Tool
> names resolve at startup, so start a new session.

## 3. Tell Claude to use it

Installing the server does not stop Claude from running `git commit` directly. Add rules
to `~/.claude/CLAUDE.md` so it prefers the tools. Under your git practices:

```markdown
- **Strict Commit Signing:** Every git commit, in every repo, must be GPG/SSH-signed—no exceptions.
  - **Commit through the `gitops` MCP server.** When its tools are available, use
    `mcp__gitops__commit` (`{message, files: [...]}` or `{message, all: true}`) instead of
    `git commit`. It reconciles signing config, signs the commit, and reports git's own
    signature verdict. Fall back to `git commit -S` only if the server is unavailable.
  - Before committing, verify signing is active with `mcp__gitops__git_signing_status`.
  - **Never write git config yourself.** Do not run `git config` to set `user.email`,
    `user.name`, `user.signingkey`, `gpg.format`, or `commit.gpgsign`—at any scope, for any
    reason, including to make a failing commit succeed. Repair config with
    `mcp__gitops__git_signing_enforce`, which corrects it to match the signing key. If
    config is wrong in a way that tool does not fix, report it and stop.
  - The signing key is the source of truth for committer identity: `user.email` must match
    the key's uid email. `user.name` is a deliberate preference—leave it alone even when it
    differs from the key's name.
  - If signing fails (e.g. a locked key that cannot be unlocked non-interactively), **stop
    and ask the user to unlock it.** Never fall back to an unsigned commit, never pass
    `allow_unsigned`, and never use `--no-gpg-sign`.
  - **If a hook rejects the commit, fix what the hook reported.** A hook failure is a real
    problem and is unrelated to signing. Never use `--no-verify`, never delete a hook,
    never `chmod -x` one, and never repoint `core.hooksPath`. If the hook's complaint
    cannot be fixed, report it and stop.
```

Each clause is there for a specific failure that happens without it:

| Clause | Prevents |
| --- | --- |
| Use `mcp__gitops__commit` | Raw `git commit` that skips reconciliation entirely |
| Never write git config | The "repair" that rewrites your identity to make a commit pass |
| The key is the source of truth | Config and key drifting apart in the wrong direction |
| `user.name` is a preference | Your handle being renamed to your key's uid name |
| Never `allow_unsigned` | The escape hatch being used as a fallback |
| Fix what the hook reported | `--no-verify`, a deleted hook, or a `chmod -x` that makes a failing hook "pass" |

### Why rules and not just the server

The server already ships instructions describing this policy, and a well-behaved client
surfaces them. `CLAUDE.md` is the stronger signal: project and user instructions override
default behavior, and they apply even when the model has not looked at the server's tool
list yet.

## 4. Optional: fewer prompts, not more

If you allowlist the server's read-only tool, status checks stop prompting. In
`~/.claude/settings.json`:

```jsonc
{
  "permissions": {
    "allow": ["mcp__gitops__git_signing_status"]
  }
}
```

Leave `mcp__gitops__commit` and `mcp__gitops__git_signing_enforce` prompting unless you
want commits and config writes to happen unattended.

## Verifying it works

Ask Claude for your signing status. A healthy answer starts with the verbatim transcript:

```
$ git config commit.gpgsign
true
$ git config user.signingkey
354F34B4DB349BD6
...
Drift: none
```

Then ask it to commit something. The result names the commit, whether it was signed, and
git's `%G?` verdict — `G` is a good signature.

## Troubleshooting

**Claude still runs `git commit`.** The rules are in a `CLAUDE.md` that is not loaded, or
the session predates the server. Check `~/.claude/CLAUDE.md` and start a new session.

**Tools do not appear.** `claude mcp list` should show `✔ Connected`. If the binary moved
(a `cargo install` to a different root, for instance), re-register it.

**A stale binary.** `cargo install --path apps/gitops-mcp` after any change to this repo.
The registered server runs the installed binary, not `target/debug`.

**Server logs.** `GITOPS_MCP_LOG=debug` prints every command the server runs to stderr,
which Claude Code surfaces in the MCP server output.
