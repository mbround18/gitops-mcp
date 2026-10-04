# Git signing governance

This guide explains what `gitops` enforces, when it rewrites your git config, and what it
deliberately leaves alone.

## The rule

Your signing key is the source of truth for your committer identity. If
`git config user.email` disagrees with the email on your signing key, the config is wrong
— not the key. `gitops` corrects the config.

This matters because a mismatched `user.email` produces commits that are signed but not
*attributed*: hosts like GitHub show them as unverified or attach them to the wrong
account. Tools that "fix" a failing commit by editing `user.email`, or by setting
`commit.gpgsign=false`, leave that damage behind in your global config for every
repository you touch afterwards.

## What it checks

`git_signing_status` reports, for `commit.gpgsign`, `user.signingkey`, `gpg.format`,
`user.email` and `user.name`:

* **effective** — what git actually uses (`git config <key>`)
* **global** — `git config --global <key>`
* **local** — `git config --local <key>`, which shadows the global value

It also reads the signing key's own identity:

| `gpg.format` | Identity source |
| --- | --- |
| `openpgp` (default) | the first `uid` of the key: name and email |
| `ssh` | the comment on the public key, if it looks like an email address |

The tool's text output starts with the verbatim transcript of:

```
$ git config commit.gpgsign
$ git config user.signingkey
```

so the same two values come back the same way every time, no matter how the question was
phrased. The full structured report is attached as JSON.

### Drift

| Drift | Meaning |
| --- | --- |
| `signing_disabled` | `commit.gpgsign` is unset or falsey at some scope |
| `missing_signing_key` | `user.signingkey` is not configured anywhere |
| `email_mismatch` | `user.email` does not match the signing key's email |
| `key_identity_unknown` | a key is configured but could not be read |
| `local_override` | a local value shadows the global one for an identity key |

## What it corrects

`git_signing_enforce` applies only these writes:

1. `git config --global commit.gpgsign true` — when signing is not enabled globally.
2. `git config --local --unset-all commit.gpgsign` — when a repository turned signing off.
   There is no legitimate reason for a repository to disable signing.
3. `git config --global user.email <key email>` — when the global email does not match
   the signing key.
4. `git config --local --unset-all user.email` — when a local email shadows the key's
   email, so the corrected global value takes effect.

Pass `dry_run: true` to see the plan without writing anything.

### What it will not touch

* **`user.name`** — a display name is a preference. `MBRound18` and
  `Michael Bruno` are both fine; the signature is checked against the email.
* **A local `user.email` that matches the key.** Only shadowing values are removed.
* **`gpg.format`** — switching signing formats is a decision, not a repair.
* **Your keys.** `gitops` never creates, imports, or unlocks a key.

## Committing

```jsonc
{ "message": "fix: stop clobbering user.email", "files": ["crates/gitops-git/src/governance.rs"] }
{ "message": "chore: sweep", "all": true }
```

`files` and `all` are mutually exclusive, and one of them is required — there is no
implicit "commit whatever happens to be staged".

The sequence is always: enforce config → stage → verify something is staged → `git commit
-S` → report the commit id and git's own signature verdict (`%G?`, where `G` is a good
signature).

### When signing is not available

If no usable signing key exists, the call fails with an explanation instead of producing
an unsigned commit:

```
refusing to commit: signing is unavailable (the secret half of signing key `354F34B4DB349BD6`
is not available on this machine). Unlock or install the signing key, then retry — do not
create an unsigned commit.
```

If the key exists but git cannot use it — usually a locked key with no cached passphrase —
you get:

```
git could not sign the commit: error: gpg failed to sign the data. The key is most likely
locked; unlock it and retry.
```

Unlock the key (signing anything once caches the passphrase in `gpg-agent`) and retry.
`allow_unsigned: true` exists as a deliberate escape hatch; using it is a decision you are
making on purpose.

## Hooks

A failing [`pre-commit` hook](https://git-scm.com/docs/githooks) is the other place where
signing gets sacrificed for convenience. The hook fails, and the next move is
`--no-verify`, or `chmod -x`, or `core.hooksPath=/dev/null` — after which the commit
succeeds, the guardrail is gone, and the original problem is still there.

`gitops` treats a hook rejection as a result, not an obstacle:

```
a git hook rejected the commit:
lint: trailing whitespace in README.md

Fix what the hook reports, then retry. Do not bypass the hook with `--no-verify`, do not
disable or delete the hook, and do not turn off commit signing — the hook failure is the
real problem and it is unrelated to signing.
```

Everything the hook printed comes back, because that is the part you can act on. The
commit is not retried, and nothing about your repository changes: the hook keeps its
executable bit, `core.hooksPath` is untouched, and `commit.gpgsign` stays `true`.

There is no `no_verify` parameter, and the server never passes `--no-verify` to git, so
neither you nor an agent can ask it to skip a hook — bypassing one is a decision you make
deliberately with raw `git`.

Note that hook failures and signing failures are different problems. A hook that rejects
your commit has nothing to do with your key, and disabling signing will not make it pass.

### Hooks that cannot run

`git_signing_status` also reports whether git will actually run your `pre-commit` hook,
because a skipped hook is indistinguishable from a passing one:

```
pre-commit hook: active (.git/hooks)
pre-commit hook: present but NOT executable — git will skip it
pre-commit hook: none
```

Two cases are reported as drift:

| Drift | Meaning |
| --- | --- |
| `hooks_directory_missing` | `core.hooksPath` points at a directory that does not exist, so no hook can run |
| `pre_commit_not_executable` | the hook file is there but has no executable bit, so git skips it silently |

Neither is auto-corrected. Both can be legitimate — a `core.hooksPath` of `.husky` before
`pnpm install` has run, for instance — and deciding between "install the tooling",
"`chmod +x` the hook" and "remove the setting" is a judgment call, not a repair. They are
reported so the decision is yours, and so a disabled hook cannot quietly stay disabled.

## Troubleshooting

**`unsupported gpg.format`** — only `openpgp` and `ssh` are understood. `x509`/gpgsm is
not supported.

**`could not read the identity of signing key`** — the configured key is not in your
keyring, or the path in `user.signingkey` does not exist. Check
`gpg --list-keys <keyid>`.

**An SSH key reports no email** — SSH keys carry only a free-form comment. Give the public
key a comment that is your email address (`ssh-keygen -C you@example.com`) and the email
check starts working; until then, only the `commit.gpgsign` rules apply.

**A hook keeps rejecting the commit** — read what the hook printed; it is included in the
error. Fix that. If the hook itself is broken, fix or remove the hook deliberately — do
not route around it to get one commit through.

**Server logs** — set `GITOPS_MCP_LOG=debug` to see every command the server runs, on
stderr.
