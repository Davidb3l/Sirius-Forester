//! Resolving a git range to changed files, then to Hayvenhurst symbols.
//!
//! Used by `sirius link ... --changed` and the gate. We shell `git diff` for
//! the file list, then map the changed files to entities by PATH via
//! `hayven affected-tests --changed` (its `roots`) — never by basename.

use crate::hayven::Hayven;
use crate::shell::Runner;
use serde_json::Value;

/// Run one git command, mapping every failure mode (spawn error, non-zero
/// exit) to a single `Err(detail)`. THE one home for the run-git-or-explain
/// idiom — hand-rolled copies of this drift on error formatting.
pub fn run_git(runner: &dyn Runner, args: &[&str]) -> Result<crate::shell::CmdOutput, String> {
    let out = runner.run("git", args).map_err(|e| e.to_string())?;
    if !out.success() {
        let stderr = out.stderr.trim();
        return Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr.to_string()
        });
    }
    Ok(out)
}

/// Split command stdout into trimmed, non-empty lines.
fn stdout_lines(out: &crate::shell::CmdOutput) -> Vec<String> {
    out.stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// List files changed in a git range (default: working tree vs HEAD).
pub fn changed_files(runner: &dyn Runner, range: Option<&str>) -> Result<Vec<String>, String> {
    // `git diff --name-only <range>`; with no range, diff HEAD (staged+unstaged).
    let out = run_git(runner, &["diff", "--name-only", range.unwrap_or("HEAD")])?;
    Ok(stdout_lines(&out))
}

/// The current HEAD commit id — captured BEFORE an agent runs so the gate can
/// diff against a fixed baseline instead of whatever HEAD means afterwards.
pub fn head_rev(runner: &dyn Runner) -> Result<String, String> {
    let out = run_git(runner, &["rev-parse", "HEAD"])?;
    let rev = out.stdout.trim().to_string();
    if rev.is_empty() {
        return Err("git rev-parse HEAD produced no output".into());
    }
    Ok(rev)
}

/// Untracked (never-`git add`ed) files. `git diff` cannot see these, but a new
/// module or test file is as real a change as an edit.
pub fn untracked_files(runner: &dyn Runner) -> Result<Vec<String>, String> {
    let out = run_git(runner, &["ls-files", "--others", "--exclude-standard"])?;
    Ok(stdout_lines(&out))
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
    // HashSets: on repos with thousands of un-ignored untracked files, the
    // naive Vec::contains scans are O(U x (P + C)) per gate evaluation.
    let pre: std::collections::HashSet<&str> = pre_untracked.iter().map(String::as_str).collect();
    let seen: std::collections::HashSet<String> = files.iter().cloned().collect();
    for f in untracked_files(runner)? {
        if !pre.contains(f.as_str()) && !seen.contains(&f) {
            files.push(f);
        }
    }
    Ok(files)
}

/// What a git range resolved to: the changed files and the entities in them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangedSymbols {
    pub files: Vec<String>,
    pub symbols: Vec<String>,
}

/// Resolve a git range's changed files to Hayvenhurst entity ids, by PATH.
///
/// SIRF-20: this used to `hayven query <file stem>` per file — a full-text
/// search on the BASENAME, so a commit touching one package's `types.ts`
/// stamped every `types`/`index` module in every package of a monorepo
/// (83 symbols from 5 files, observed), writing false provenance into the
/// ledger. `hayven affected-tests --changed <files>` reports `roots`: the
/// daemon's own path-exact file → entity resolution (the same mapping the
/// gate trusts). Unindexed files (lockfiles, configs) contribute nothing.
/// A daemon that cannot answer is an ERROR, never a silently empty stamp.
pub fn changed_symbols(
    runner: &dyn Runner,
    hv: &Hayven,
    range: Option<&str>,
) -> Result<ChangedSymbols, String> {
    let files = changed_files(runner, range)?;
    if files.is_empty() {
        return Ok(ChangedSymbols::default());
    }
    let (ok, parsed, detail) = hv.affected_tests_changed(&files);
    let v = match parsed {
        Some(v) if ok => v,
        _ => {
            return Err(format!(
                "hayven could not map the changed files to entities: {}",
                detail.lines().next().unwrap_or("no output")
            ))
        }
    };
    let roots = v
        .get("roots")
        .and_then(Value::as_array)
        .ok_or_else(|| "hayven affected-tests returned no \"roots\" array".to_string())?;
    let mut symbols: Vec<String> = Vec::new();
    for r in roots {
        let id = r
            .as_str()
            .or_else(|| r.get("id").and_then(Value::as_str))
            .map(String::from);
        if let Some(id) = id {
            if !symbols.contains(&id) {
                symbols.push(id);
            }
        }
    }
    Ok(ChangedSymbols { files, symbols })
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
    fn changed_symbols_resolves_by_path_not_basename() {
        // SIRF-20 regression: two same-named modules in different packages.
        // Only the changed one may be stamped — never b/types via basename.
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "a/types.ts\nbun.lock\n");
        m.expect(
            &["hayven", "affected-tests"],
            0,
            r#"{"changed":["a/types.ts","bun.lock"],"roots":["a/types","a/types/Foo","a/types"],"tests":[]}"#,
        );
        // If anything still searched by basename, this would leak b/types in.
        m.expect(
            &["hayven", "query"],
            0,
            r#"{"hits":[{"id":"a/types"},{"id":"b/types"}]}"#,
        );
        let hv = Hayven::new(&m);
        let got = changed_symbols(&m, &hv, Some("x~1..x")).unwrap();
        assert_eq!(got.files, vec!["a/types.ts", "bun.lock"]);
        assert_eq!(
            got.symbols,
            vec!["a/types", "a/types/Foo"],
            "deduped, path-exact"
        );
        let calls = m.recorded();
        assert!(calls
            .iter()
            .any(|c| c == "hayven affected-tests --changed a/types.ts,bun.lock --json"));
        assert!(
            !calls.iter().any(|c| c.starts_with("hayven query")),
            "{calls:?}"
        );
    }

    #[test]
    fn changed_symbols_errors_when_hayven_cannot_answer() {
        // A daemon failure must not become a silently EMPTY (or partial) stamp.
        let m = MockRunner::new();
        m.expect(&["git", "diff"], 0, "src/a.rs\n");
        m.push(MockResponse::new(
            &["hayven", "affected-tests"],
            1,
            "",
            "daemon serves a DIFFERENT project",
        ));
        let hv = Hayven::new(&m);
        let e = changed_symbols(&m, &hv, None).unwrap_err();
        assert!(e.contains("DIFFERENT project"), "{e}");
        // An empty range is just empty — not an error, and no hayven call.
        let m2 = MockRunner::new();
        m2.expect(&["git", "diff"], 0, "");
        let hv2 = Hayven::new(&m2);
        assert_eq!(
            changed_symbols(&m2, &hv2, None).unwrap(),
            ChangedSymbols::default()
        );
        assert!(!m2.recorded().iter().any(|c| c.starts_with("hayven")));
    }
}
