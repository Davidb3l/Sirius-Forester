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
