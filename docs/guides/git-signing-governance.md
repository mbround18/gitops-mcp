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
`gpg.ssh.allowedSignersFile`, `user.email` and `user.name`:

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
| `missing_allowed_signers_file` | SSH signing is active but `gpg.ssh.allowedSignersFile` is unset |
| `allowed_signers_file_missing` | SSH signing points at an allowed signers file that does not exist |
| `local_override` | a local `user.email` still shadows the effective signing identity with the wrong email |

## What it corrects

`git_signing_enforce` applies only these writes:

1. `git config --global commit.gpgsign true` — when signing is not enabled globally.
2. `git config --local --unset-all commit.gpgsign` — when a repository turned signing off.
   There is no legitimate reason for a repository to disable signing.
3. `git config --global user.email <key email>` — when the repository uses your global
   signing identity and the global email does not match the signing key.
4. `git config --local user.email <key email>` — when the repository intentionally uses a
   repo-local signer (for example global OpenPGP, local SSH) and its effective email must
   follow that repo-local key instead of your global identity.
5. `git config --local --unset-all user.email` — when a local email shadows the global
   signer with the wrong value.
6. For SSH signing, create or update an allowed signers file containing the current
   signing principal and public key. If none is configured, `gitops` creates one under the
   repository's git dir and points `gpg.ssh.allowedSignersFile` at it locally.

Pass `dry_run: true` to see the plan without writing anything.

### What it will not touch

* **`user.name`** — a display name is a preference. `MBRound18` and
  `Michael Bruno` are both fine; the signature is checked against the email.
* **A local `user.email` that already matches the effective key.**
* **Intentional repo-local `user.signingkey` / `gpg.format` overrides.** That is how you
  can use GPG in one repo and SSH in another without rewriting global config every time.
* **`gpg.format` itself** — switching signing formats is a decision, not a repair.
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

That flow is the same for OpenPGP and SSH. `gitops` does not choose between separate
commit paths; it always runs `git commit -S`, and git signs with whatever the effective
`gpg.format` / `user.signingkey` is for that repository.

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

A failing [hook](https://git-scm.com/docs/githooks) is the other place where signing gets
sacrificed for convenience. The `pre-commit` hook fails, the guardrail gets switched off so
the commit lands, and the original problem is still there.

`gitops` treats a hook rejection as a result, not an obstacle:

```
a git hook rejected the commit:
lint: trailing whitespace in README.md

Fix what the hook reports, then retry. Do not bypass the hook, do not disable, delete or
un-execute it, do not repoint `core.hooksPath`, and do not turn off commit signing — the
hook failure is the real problem and it is unrelated to signing.
```

Everything the hook printed comes back, because that is the part you can act on. The
operation is not retried, and nothing about your repository changes: the hook keeps its
executable bit, `core.hooksPath` is untouched, and `commit.gpgsign` stays `true`.

The same applies to `pre-push`. When a `pre-push` hook rejects a
[`push`](safe-git-operations.md#push), the refusal says `rejected the push` instead, and
nothing reaches the remote.

There is no parameter for skipping a hook on any tool, so neither you nor an agent can ask
this server to do it — running a hook-free git command is a decision you make deliberately
yourself, outside this server.

Note that hook failures and signing failures are different problems. A hook that rejects
your commit has nothing to do with your key, and disabling signing will not make it pass.

### Hooks that cannot run

`git_signing_status` also reports whether git will actually run your hooks, because a
skipped hook is indistinguishable from a passing one. Both governed hooks get a line:

```
pre-commit hook: active (.git/hooks)
pre-push hook: present but NOT executable — git will skip it
```

The three possible values are `active (<hooks dir>)`, `present but NOT executable — git
will skip it`, and `none`.

Two cases are reported as drift:

| Drift | Meaning |
| --- | --- |
| `hooks_directory_missing` | `core.hooksPath` points at a directory that does not exist, so no hook can run |
| `hook_not_executable` | a hook file is there but has no executable bit, so git skips it silently; the entry names which hook (`pre-commit` or `pre-push`) and its path |

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
check starts working; until then, `gitops` cannot reconcile `user.email` or build an
allowed signers entry for that key automatically.

**A hook keeps rejecting the commit** — read what the hook printed; it is included in the
error. Fix that. If the hook itself is broken, fix or remove the hook deliberately — do
not route around it to get one commit through.

**Server logs** — set `GITOPS_MCP_LOG=debug`, or pass `--log debug`, to see every command the server runs, on
stderr.
