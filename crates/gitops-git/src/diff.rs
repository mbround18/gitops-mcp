//! What changed, without the whole patch.
//!
//! `git diff` is the operation an agent runs and then regrets: the answer to "what did I
//! change" is usually a dozen paths and a pair of counts, and the shell gives it as
//! thousands of lines of hunks instead. So this module reports the summary first — one
//! line's worth of structure per file, with its status and its added/removed counts — and
//! the hunks only when they are asked for, under a line cap that always says what it cut.
//!
//! It is read-only. There is no flag here that writes, resets or checks anything out, and
//! the pathspec is validated the same way [`crate::restore`] validates one, so nothing a
//! caller passes can be read by git as a revision or an option.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, merge::combined, runner::CommandRunner, trimmed};

/// Patch lines rendered when `patch` is set and no cap is named.
pub const DEFAULT_MAX_LINES: usize = 400;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffRequest {
    /// What to compare. A commit (`HEAD~3`), a branch (`main`), or a range
    /// (`main..topic`, `main...topic`). Omitted, it compares the working tree with
    /// `HEAD` — the uncommitted-changes question.
    #[serde(default)]
    pub rev: Option<String>,
    /// Compare the index with `HEAD` instead of the working tree: what a commit would
    /// contain right now.
    #[serde(default)]
    pub staged: bool,
    /// Limit to these paths. Relative to the repository, no magic pathspecs.
    #[serde(default)]
    pub files: Vec<String>,
    /// Include the hunks. Off by default: the summary answers most questions for a
    /// fraction of the output.
    #[serde(default)]
    pub patch: bool,
    /// Context lines either side of each hunk. Defaults to git's 3.
    #[serde(default)]
    pub context: Option<usize>,
    /// Ceiling on patch lines (default [`DEFAULT_MAX_LINES`]). What is cut is reported,
    /// never dropped silently.
    #[serde(default)]
    pub max_lines: Option<usize>,
    /// Repository to work in. Defaults to the server's working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// One file's share of the diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedFile {
    pub path: String,
    /// Git's own letter, spelled out: `added`, `modified`, `deleted`, `renamed`,
    /// `copied`, `type-changed`, `unmerged`, or the raw letter if git invents another.
    pub status: String,
    /// `None` for a binary file, which git counts in bytes it does not report.
    pub added: Option<usize>,
    pub removed: Option<usize>,
    /// Where a rename or copy came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffOutcome {
    /// What was compared, as git was asked for it.
    pub command: String,
    pub rev: Option<String>,
    pub staged: bool,
    pub files: Vec<ChangedFile>,
    pub added: usize,
    pub removed: usize,
    /// True when nothing differs, which is an answer rather than an error.
    pub unchanged: bool,
    /// The hunks, when `patch` was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// Patch lines cut by `max_lines`, and how to see them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Truncation {
    pub shown: usize,
    pub cut: usize,
    pub hint: String,
}

/// Summarise a diff, and optionally render it.
pub fn diff(runner: &dyn CommandRunner, request: &DiffRequest) -> Result<DiffOutcome> {
    let cwd = request.cwd.as_deref();

    if let Some(rev) = request.rev.as_deref() {
        check_rev(rev)?;
    }
    for file in &request.files {
        check_pathspec(file)?;
    }
    if request.staged && request.rev.is_some() {
        return Err(Error::InvalidRequest(
            "`staged` compares the index with HEAD, so it cannot also take a `rev`; \
             drop one of them"
                .into(),
        ));
    }

    runner
        .run("git", &["rev-parse", "--show-toplevel"], cwd)?
        .value()
        .ok_or(Error::NotARepository)?;

    let numstat = run_diff(runner, request, cwd, &["--numstat", "--find-renames"])?;
    let names = run_diff(runner, request, cwd, &["--name-status", "--find-renames"])?;
    let files = merge_lists(&numstat.stdout, &names.stdout);

    let added = files.iter().filter_map(|f| f.added).sum();
    let removed = files.iter().filter_map(|f| f.removed).sum();

    let (patch, truncated) = if request.patch {
        let context = request.context.map(|n| format!("--unified={n}"));
        let mut flags: Vec<&str> = vec!["--patch", "--find-renames"];
        if let Some(flag) = &context {
            flags.push(flag);
        }
        let out = run_diff(runner, request, cwd, &flags)?;
        let cap = request.max_lines.unwrap_or(DEFAULT_MAX_LINES);
        let (text, cut) = cap_lines(&out.stdout, cap);
        (
            Some(text),
            cut.map(|cut| Truncation {
                shown: cap,
                cut,
                hint: "name `files` to diff one path, or raise `max_lines`".into(),
            }),
        )
    } else {
        (None, None)
    };

    Ok(DiffOutcome {
        command: numstat.command,
        rev: request.rev.clone(),
        staged: request.staged,
        unchanged: files.is_empty(),
        files,
        added,
        removed,
        patch,
        truncated,
    })
}

struct Ran {
    command: String,
    stdout: String,
}

fn run_diff(
    runner: &dyn CommandRunner,
    request: &DiffRequest,
    cwd: Option<&Path>,
    flags: &[&str],
) -> Result<Ran> {
    let mut args: Vec<&str> = vec!["diff"];
    args.extend_from_slice(flags);
    if request.staged {
        args.push("--cached");
    }
    if let Some(rev) = request.rev.as_deref() {
        args.push(rev);
    }
    if !request.files.is_empty() {
        args.push("--");
        args.extend(request.files.iter().map(String::as_str));
    }

    let command = format!("git {}", args.join(" "));
    let out = runner.run("git", &args, cwd)?;
    if !out.ok() {
        let detail = combined(&out.stdout, &out.stderr);
        if unknown_revision(&detail) {
            return Err(Error::UnknownRef {
                reference: request.rev.clone().unwrap_or_default(),
            });
        }
        return Err(Error::DiffFailed { detail });
    }
    Ok(Ran {
        command,
        stdout: out.stdout,
    })
}

/// `--numstat` has the counts and `--name-status` has the letter; neither has both, so
/// they are zipped by position — git lists the same files in the same order for both.
fn merge_lists(numstat: &str, names: &str) -> Vec<ChangedFile> {
    let statuses: Vec<(String, Option<String>)> = names
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.split('\t');
            let letter = parts.next().unwrap_or("");
            let first = parts.next().unwrap_or("").to_owned();
            let second = parts.next().map(str::to_owned);
            // A rename reads `R096\told\tnew`: the old path is where it came from.
            match second {
                Some(_) => (spell(letter), Some(first)),
                None => (spell(letter), None),
            }
        })
        .collect();

    numstat
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(index, line)| {
            let mut parts = line.split('\t');
            let added = parts.next().and_then(count);
            let removed = parts.next().and_then(count);
            let path = parts.next().unwrap_or("").to_owned();
            let (status, from) = statuses
                .get(index)
                .cloned()
                .unwrap_or_else(|| ("modified".to_owned(), None));
            ChangedFile {
                path,
                status,
                added,
                removed,
                from,
            }
        })
        .collect()
}

/// `-` is git's way of saying "binary, no line counts".
fn count(field: &str) -> Option<usize> {
    field.trim().parse().ok()
}

fn spell(letter: &str) -> String {
    match letter.chars().next() {
        Some('A') => "added",
        Some('M') => "modified",
        Some('D') => "deleted",
        Some('R') => "renamed",
        Some('C') => "copied",
        Some('T') => "type-changed",
        Some('U') => "unmerged",
        _ => letter,
    }
    .to_owned()
}

/// Keep the first `cap` lines and report the rest, rather than handing back a patch that
/// trails off mid-hunk with no sign that it did.
fn cap_lines(patch: &str, cap: usize) -> (String, Option<usize>) {
    let total = patch.lines().count();
    if total <= cap {
        return (trimmed(patch), None);
    }
    let kept: Vec<&str> = patch.lines().take(cap).collect();
    (kept.join("\n"), Some(total - cap))
}

/// Reject anything git could read as an option. Ranges are the point of a diff, so `..`
/// is allowed here where [`crate::merge`] refuses it.
fn check_rev(rev: &str) -> Result<()> {
    let reject = |why: &str| {
        Err(Error::InvalidRequest(format!(
            "`{rev}` is not a usable revision: {why}"
        )))
    };
    if rev.trim().is_empty() {
        return reject("it is empty");
    }
    if rev.starts_with('-') {
        return reject("it would be read as an option");
    }
    if rev.contains(char::is_whitespace) {
        return reject("it contains whitespace");
    }
    Ok(())
}

/// The same rule [`crate::restore`] applies: a path, not a revision, an option, or an
/// escape upwards.
fn check_pathspec(file: &str) -> Result<()> {
    let reject = |why: &str| {
        Err(Error::InvalidRequest(format!(
            "`{file}` is not a usable path: {why}"
        )))
    };
    if file.trim().is_empty() {
        return reject("it is empty");
    }
    if file.starts_with('-') {
        return reject("it would be read as an option");
    }
    if file.starts_with(':') {
        return reject("magic pathspecs are not accepted");
    }
    if Path::new(file).is_absolute() {
        return reject("it is absolute; use a path relative to the repository");
    }
    if file.split(['/', '\\']).any(|part| part == "..") {
        return reject("it escapes the repository");
    }
    Ok(())
}

fn unknown_revision(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    ["unknown revision", "bad revision", "ambiguous argument"]
        .iter()
        .any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    fn runner() -> ScriptedRunner {
        ScriptedRunner::new()
            .with("git rev-parse --show-toplevel", 0, "/repo\n", "")
            .with(
                "git diff --numstat --find-renames",
                0,
                "12\t3\tsrc/lib.rs\n0\t40\tdocs/old.md\n-\t-\tassets/logo.png\n",
                "",
            )
            .with(
                "git diff --name-status --find-renames",
                0,
                "M\tsrc/lib.rs\nD\tdocs/old.md\nM\tassets/logo.png\n",
                "",
            )
    }

    #[test]
    fn the_summary_is_the_answer_and_the_patch_is_opt_in() {
        let runner = runner();
        let outcome = diff(&runner, &DiffRequest::default()).unwrap();

        assert_eq!(outcome.files.len(), 3);
        assert_eq!(outcome.added, 12);
        assert_eq!(outcome.removed, 43);
        assert_eq!(outcome.command, "git diff --numstat --find-renames");
        assert!(outcome.patch.is_none(), "no hunks unless asked for");
        assert!(
            !runner.calls().iter().any(|call| call.contains("--patch")),
            "{:?}",
            runner.calls()
        );
        assert_eq!(
            outcome.files[0],
            ChangedFile {
                path: "src/lib.rs".into(),
                status: "modified".into(),
                added: Some(12),
                removed: Some(3),
                from: None,
            }
        );
        assert_eq!(outcome.files[1].status, "deleted");
    }

    #[test]
    fn a_binary_file_is_listed_without_invented_counts() {
        let outcome = diff(&runner(), &DiffRequest::default()).unwrap();
        let binary = &outcome.files[2];
        assert_eq!(binary.path, "assets/logo.png");
        assert_eq!(binary.added, None);
        assert_eq!(binary.removed, None);
    }

    #[test]
    fn a_rename_keeps_the_path_it_came_from() {
        let runner = runner()
            .with(
                "git diff --numstat --find-renames",
                0,
                "0\t0\tsrc/new.rs\n",
                "",
            )
            .with(
                "git diff --name-status --find-renames",
                0,
                "R096\tsrc/old.rs\tsrc/new.rs\n",
                "",
            );
        let outcome = diff(&runner, &DiffRequest::default()).unwrap();
        assert_eq!(outcome.files[0].status, "renamed");
        assert_eq!(outcome.files[0].from.as_deref(), Some("src/old.rs"));
    }

    #[test]
    fn the_patch_is_capped_and_says_what_it_cut() {
        let hunks: String = (0..50).map(|n| format!("+line {n}\n")).collect();
        let runner = runner().with("git diff --patch --find-renames", 0, &hunks, "");
        let outcome = diff(
            &runner,
            &DiffRequest {
                patch: true,
                max_lines: Some(10),
                ..DiffRequest::default()
            },
        )
        .unwrap();

        let patch = outcome.patch.unwrap();
        assert_eq!(patch.lines().count(), 10);
        assert_eq!(
            outcome.truncated,
            Some(Truncation {
                shown: 10,
                cut: 40,
                hint: "name `files` to diff one path, or raise `max_lines`".into(),
            })
        );
    }

    #[test]
    fn a_patch_that_fits_is_not_reported_as_truncated() {
        let runner = runner().with("git diff --patch --find-renames", 0, "+one\n+two\n", "");
        let outcome = diff(
            &runner,
            &DiffRequest {
                patch: true,
                ..DiffRequest::default()
            },
        )
        .unwrap();
        assert_eq!(outcome.patch.as_deref(), Some("+one\n+two"));
        assert!(outcome.truncated.is_none());
    }

    #[test]
    fn staged_compares_the_index_and_a_rev_narrows_to_a_range() {
        let cached = runner()
            .with("git diff --numstat --find-renames --cached", 0, "", "")
            .with("git diff --name-status --find-renames --cached", 0, "", "");
        let outcome = diff(
            &cached,
            &DiffRequest {
                staged: true,
                ..DiffRequest::default()
            },
        )
        .unwrap();
        assert!(outcome.unchanged);
        assert_eq!(
            outcome.command,
            "git diff --numstat --find-renames --cached"
        );

        let ranged = runner()
            .with(
                "git diff --numstat --find-renames main..topic -- src",
                0,
                "1\t1\tsrc/lib.rs\n",
                "",
            )
            .with(
                "git diff --name-status --find-renames main..topic -- src",
                0,
                "M\tsrc/lib.rs\n",
                "",
            );
        let outcome = diff(
            &ranged,
            &DiffRequest {
                rev: Some("main..topic".into()),
                files: vec!["src".into()],
                ..DiffRequest::default()
            },
        )
        .unwrap();
        assert_eq!(outcome.rev.as_deref(), Some("main..topic"));
        assert_eq!(outcome.files.len(), 1);
    }

    #[test]
    fn nothing_this_tool_runs_can_change_the_repository() {
        let hunks = "+one\n";
        let runner = runner().with("git diff --patch --find-renames", 0, hunks, "");
        diff(
            &runner,
            &DiffRequest {
                patch: true,
                ..DiffRequest::default()
            },
        )
        .unwrap();

        for call in runner.calls() {
            assert!(
                call.starts_with("git diff") || call.starts_with("git rev-parse"),
                "diff may only read: {call}"
            );
        }
        for forbidden in [
            "restore", "checkout", "reset", "apply", "commit", "clean", "stash", "--force",
        ] {
            assert!(
                !runner.calls().iter().any(|call| call.contains(forbidden)),
                "`{forbidden}` must never be run: {:?}",
                runner.calls()
            );
        }
        assert!(runner.writes().is_empty(), "diff writes nothing");
    }

    #[test]
    fn a_rev_or_path_git_could_read_as_an_option_is_refused() {
        for bad_rev in ["", "-f", "--exit-code", "a b"] {
            let runner = runner();
            let err = diff(
                &runner,
                &DiffRequest {
                    rev: Some(bad_rev.into()),
                    ..DiffRequest::default()
                },
            )
            .unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest(_)),
                "`{bad_rev}` should be refused, got {err}"
            );
            assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        }

        for bad_path in ["-f", ":(exclude)src", "/etc/passwd", "../outside"] {
            let runner = runner();
            let err = diff(
                &runner,
                &DiffRequest {
                    files: vec![bad_path.into()],
                    ..DiffRequest::default()
                },
            )
            .unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest(_)),
                "`{bad_path}` should be refused, got {err}"
            );
            assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        }
    }

    #[test]
    fn staged_and_a_rev_together_are_refused_rather_than_guessed() {
        let err = diff(
            &runner(),
            &DiffRequest {
                staged: true,
                rev: Some("main".into()),
                ..DiffRequest::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[test]
    fn a_revision_git_does_not_know_is_reported_as_such() {
        let runner = runner().with(
            "git diff --numstat --find-renames nope",
            128,
            "",
            "fatal: ambiguous argument 'nope': unknown revision or path not in the working tree.\n",
        );
        let err = diff(
            &runner,
            &DiffRequest {
                rev: Some("nope".into()),
                ..DiffRequest::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::UnknownRef { .. }), "{err}");
    }

    #[test]
    fn diffing_outside_a_repository_is_refused() {
        let err = diff(&ScriptedRunner::new(), &DiffRequest::default()).unwrap_err();
        assert!(matches!(err, Error::NotARepository), "{err}");
    }
}
