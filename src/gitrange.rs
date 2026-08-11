//! Resolving a git range to changed files, then to Hayvenhurst symbols.
//!
//! Used by `sirius link ... --changed` and the gate. We shell `git diff` for
//! the file list, then map each changed file to symbols via `hayven query`.

use crate::hayven::Hayven;
use crate::shell::Runner;
use serde_json::Value;

/// List files changed in a git range (default: working tree vs HEAD).
pub fn changed_files(runner: &dyn Runner, range: Option<&str>) -> Result<Vec<String>, String> {
    // `git diff --name-only <range>`; with no range, diff HEAD (staged+unstaged).
    let mut args: Vec<&str> = vec!["diff", "--name-only"];
    match range {
        Some(r) => args.push(r),
        None => args.push("HEAD"),
    }
    let out = runner.run("git", &args).map_err(|e| e.to_string())?;
    if !out.success() {
        return Err(if out.stderr.trim().is_empty() {
            "git diff failed".into()
        } else {
            out.stderr.trim().to_string()
        });
    }
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// The current HEAD commit id — captured BEFORE an agent runs so the gate can
/// diff against a fixed baseline instead of whatever HEAD means afterwards.
pub fn head_rev(runner: &dyn Runner) -> Result<String, String> {
    let out = runner
        .run("git", &["rev-parse", "HEAD"])
        .map_err(|e| e.to_string())?;
    let rev = out.stdout.trim().to_string();
    if !out.success() || rev.is_empty() {
        return Err(if out.stderr.trim().is_empty() {
            "git rev-parse HEAD failed".into()
        } else {
            out.stderr.trim().to_string()
        });
    }
    Ok(rev)
}

/// Untracked (never-`git add`ed) files. `git diff` cannot see these, but a new
/// module or test file is as real a change as an edit.
pub fn untracked_files(runner: &dyn Runner) -> Result<Vec<String>, String> {
    let out = runner
        .run("git", &["ls-files", "--others", "--exclude-standard"])
        .map_err(|e| e.to_string())?;
    if !out.success() {
        return Err(if out.stderr.trim().is_empty() {
            "git ls-files failed".into()
        } else {
            out.stderr.trim().to_string()
        });
    }
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// Everything changed since `base` (a commit id captured before the work):
/// committed + staged + unstaged (`git diff <base>`) plus NEW untracked files.
/// SIRF-11: `git diff HEAD` alone is blind to work an agent COMMITTED (agents
/// routinely commit) and to new untracked files — both used to read as
/// "nothing changed", which skipped the gate entirely. With no `base` this
/// degrades to the old worktree-vs-HEAD diff, still plus new untracked files.
///
/// `pre_untracked` is the untracked snapshot taken BEFORE the work: untracked
/// files that already existed (developer scratch files, un-ignored artifacts)
/// are not the agent's doing, and counting them made every iteration look like
/// it changed something — which could gate (and even advance) an issue whose
/// agent did nothing at all.
pub fn changed_since(
    runner: &dyn Runner,
    base: Option<&str>,
    pre_untracked: &[String],
) -> Result<Vec<String>, String> {
    let mut files = changed_files(runner, base)?;
    for f in untracked_files(runner)? {
        if !pre_untracked.contains(&f) && !files.contains(&f) {
            files.push(f);
        }
    }
    Ok(files)
}

/// Resolve changed files to Hayvenhurst symbol ids by querying the index for
/// each file's basename and collecting entity ids. Best-effort and dedup'd.
pub fn changed_symbols(
    runner: &dyn Runner,
    hv: &Hayven,
    range: Option<&str>,
) -> Result<Vec<String>, String> {
    let files = changed_files(runner, range)?;
    let mut symbols: Vec<String> = Vec::new();
    for f in &files {
        // Query by the file path stem; hayven FTS matches on it.
        let stem = std::path::Path::new(f)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(f);
        if let Ok(v) = hv.query(stem) {
            for id in extract_ids(&v) {
                if !symbols.contains(&id) {
                    symbols.push(id);
                }
            }
        }
    }
    Ok(symbols)
}

/// Pull entity ids out of a `hayven query` result (`{"hits":[{"id":..}]}`).
pub fn extract_ids(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let arr = v
        .get("hits")
        .or_else(|| v.get("results"))
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    for item in arr {
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            out.push(id.to_string());
        } else if let Some(id) = item.as_str() {
            out.push(id.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};

    #[test]
    fn changed_files_splits_lines() {
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "src/a.rs\nsrc/b.rs\n\n");
        let files = changed_files(&m, None).unwrap();
        assert_eq!(files, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(m.recorded()[0], "git diff --name-only HEAD");
    }

    #[test]
    fn changed_files_uses_range() {
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "x.rs\n");
        changed_files(&m, Some("main..HEAD")).unwrap();
        assert_eq!(m.recorded()[0], "git diff --name-only main..HEAD");
    }

    // SIRF-11: the gate's changed-file view must include committed work and
    // untracked files — `git diff HEAD` alone reads both as "nothing changed".
    #[test]
    fn changed_since_sees_committed_and_untracked_work() {
        let m = MockRunner::new();
        // Diff vs the pre-work baseline picks up the agent's COMMITTED change…
        m.expect(&["git", "diff", "--name-only", "base123"], 0, "src/a.rs\n");
        // …and ls-files adds the never-`git add`ed NEW file (dup deduped;
        // TODO.txt existed before the work, so it is NOT the agent's change).
        m.expect(
            &["git", "ls-files"],
            0,
            "src/new_test.rs\nsrc/a.rs\nTODO.txt\n",
        );
        let pre = vec!["TODO.txt".to_string()];
        let files = changed_since(&m, Some("base123"), &pre).unwrap();
        assert_eq!(files, vec!["src/a.rs", "src/new_test.rs"]);
        assert_eq!(m.recorded()[0], "git diff --name-only base123");
        assert_eq!(m.recorded()[1], "git ls-files --others --exclude-standard");
    }

    #[test]
    fn changed_since_propagates_git_errors() {
        // A git failure must be an ERROR the caller can fail closed on — not
        // an empty list (the old fold that skipped the gate).
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["git", "diff"],
            128,
            "",
            "fatal: not a git repository",
        ));
        assert!(changed_since(&m, None, &[]).unwrap_err().contains("fatal"));
    }

    #[test]
    fn head_rev_requires_real_output() {
        // Empty stdout (e.g. a mock's benign default, or a repo with no HEAD)
        // must be an error, never an empty baseline string.
        let m = MockRunner::new();
        m.expect(&["git", "rev-parse"], 0, "\n");
        assert!(head_rev(&m).is_err());
        m.expect(&["git", "rev-parse"], 0, "base123\n");
        assert_eq!(head_rev(&m).unwrap(), "base123");
    }

    #[test]
    fn extract_ids_from_hits() {
        let v = serde_json::json!({"hits":[{"id":"a::f"},{"id":"b::g"}]});
        assert_eq!(extract_ids(&v), vec!["a::f", "b::g"]);
    }

    #[test]
    fn changed_symbols_dedups() {
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["git", "diff"],
            0,
            "src/math.rs\nsrc/math_test.rs\n",
            "",
        ));
        // Both queries return the same id → deduped.
        m.push(MockResponse::new(
            &["hayven", "query"],
            0,
            r#"{"hits":[{"id":"src/math::add"}]}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "query"],
            0,
            r#"{"hits":[{"id":"src/math::add"}]}"#,
            "",
        ));
        let hv = Hayven::new(&m);
        let syms = changed_symbols(&m, &hv, None).unwrap();
        assert_eq!(syms, vec!["src/math::add"]);
    }
}
