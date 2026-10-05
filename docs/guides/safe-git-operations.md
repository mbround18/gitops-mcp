# Safe git operations

Three git operations have a safe form and a destructive one, separated by a single flag:

| Operation | Safe | One flag away |
| --- | --- | --- |
| Discarding a file | restore it and keep a copy of what was discarded | discard it with no way back |
| Landing a branch | fast-forward, or refuse | merge, rebase or reset until it lands |
| Publishing | fast-forward the remote | overwrite what is already there |

An agent that is stuck reaches for the second column, because it ends the error. The
`restore`, `merge_ff_only` and `push` tools exist so the first column is the only one
available.

## The shape of all three

Each tool does one operation in one way, and the inputs are the minimum needed to say
which: paths, a ref, a remote. There is no parameter that relaxes a check and no parameter
that skips a hook — not defaulted to off, but absent, so there is nothing to pass and
nothing to discover. When a tool refuses, the refusal is the answer; the thing to do next
is in the message.

The checks themselves are not something you configure or remember. They run on every call.

---

## `restore` — discard local changes

The safe form of `git checkout -- <path>`.

```jsonc
{ "files": ["apps/web/e2e/canvas-touch.spec.ts"] }
```

Restores each path to its committed state, discarding both staged and unstaged changes to
it.

**It always saves what it discards first.** The diff against `HEAD` is written to
`.git/gitops-mcp/restore-<timestamp>.patch` before anything is touched, and the result
tells you where:

```
Restored 1 path(s) to their committed state.
Discarded changes saved to:
  /your/repo/.git/gitops-mcp/restore-1759573822.patch
Recover them with: git apply /your/repo/.git/gitops-mcp/restore-1759573822.patch
```

There is no working-tree reflog, so this patch is the only way back — which is why it is
not optional. **If the patch cannot be written, nothing is restored** and the changes are
still there. The patch lives under `.git/`, so `git clean` will not take it with everything
else.

What it refuses:

| Input | Why |
| --- | --- |
| no paths | there is no "restore everything"; name the files you mean |
| `.`, `./`, `:/` | the same thing by another spelling |
| `:(exclude)…` and other magic pathspecs | too easy to widen by accident |
| an absolute path, or one containing `..` | it would reach outside the repository |
| anything starting with `-` | git would read it as an option |

A path with no changes is reported as already clean rather than treated as an error. A path
git does not know is an error, so a typo does not look like a successful no-op.

> **Note:** a path that is tracked but absent from `HEAD` — a newly added file — is removed,
> which is what restoring to the committed state means. Its content is in the patch.

## `merge_ff_only` — land a branch

```jsonc
{ "ref": "069-a-board-you-can-touch" }
```

Runs `git merge --ff-only`, which either moves the branch pointer or refuses. It cannot
create a merge commit, and it cannot lose a commit.

Checked before the merge starts:

* **The work tree has to be clean**, counting tracked files only. git aborts a merge that
  would overwrite local changes, and it can abort partway; refusing up front means you
  never land in a half-merged state. Untracked files — build output, scratch files — are
  ignored, since a fast-forward cannot touch them.
* **The ref has to exist.** A typo is a refusal, not a merge of something unintended.
* A range (`main..topic`) is not a merge source and is refused.

When the branches have diverged you get:

```
`topic` cannot be fast-forwarded onto `main`:
fatal: Not possible to fast-forward, aborting.

The branches have diverged, so landing this means choosing between a merge commit, a
rebase, or dropping commits — report the divergence and let the author decide. Do not
reconcile it automatically, and do not reset or force anything.
```

That is the whole behaviour. There is no second attempt by another route, because choosing
between a merge commit, a rebase and dropping commits depends on what the branch is for.

## `push` — publish the current branch

```jsonc
{ "remote": "origin" }   // remote is optional; origin is the default
```

Pushes the branch you are on to the branch of the same name on `remote`, setting up
tracking the first time.

**Every commit it would publish has to be signed.** This is where signing stops being a
local convention: the `commit` tool can only promise that the commits *it* makes are
signed, while this refuses to publish an unsigned commit whoever or whatever made it.

```
refusing to push `main`: 1 of its commits are not signed:
  bbbb2222 N chore: snuck in

Sign them before publishing. An unsigned commit on a shared branch is the exact outcome
this server exists to prevent, so do not work around this.
```

Verdicts come from git itself (`%G?`). `G` and `U` count as signed — `U` is a good
signature from a key this machine does not trust, which is a gap in your trust store rather
than an unsigned commit. Everything else (`N`, `B`, `E`, `X`, `Y`, `R`) does not.

Also checked:

| Situation | What happens |
| --- | --- |
| the remote has commits this branch lacks | refused, naming the divergence; nothing on the remote is touched |
| `HEAD` is detached | refused — there is no branch to publish |
| the remote is not configured | refused; a URL or a refspec in place of a name is refused too |
| a `pre-push` hook exits non-zero | the hook's output comes back as the error, and the push does not happen |
| nothing to push | reported as already up to date |

Published history is never rewritten or removed. When the remote has moved ahead, the
answer is to integrate those commits — what to do about diverged published history is the
author's call, and the tool reports it rather than deciding.

The refspec is written out in full on both sides, so a `push.default` or `remote.*.push`
setting cannot redirect the push somewhere you did not name.

## Recovering a discarded change

```bash
ls .git/gitops-mcp/                       # every restore, newest last
git apply .git/gitops-mcp/restore-<timestamp>.patch
```

These files are never cleaned up automatically — deleting a recovery patch is not something
a tool should decide. Remove them when you are sure:

```bash
rm .git/gitops-mcp/restore-*.patch
```

## Troubleshooting

**A refusal you disagree with.** Every refusal is about an operation that could lose
committed or published work. Doing it anyway is a reasonable thing for *you* to decide:
run the git command yourself. The tools exist so that decision is always yours, not one an
agent makes while trying to clear an error.

**`restore` says a path is unknown.** It is untracked, or the spelling is wrong. Untracked
files are not git's to restore; delete it yourself if that is what you meant.

**A `pre-push` hook that never runs.** `git_signing_status` reports `pre-push hook:` the
same way it reports `pre-commit` — a hook that is present but not executable is listed as
drift, because git skips it silently and a skipped hook looks exactly like a passing one.
