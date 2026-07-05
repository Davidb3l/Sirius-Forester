//! The Bridge — `sirius link` and `sirius why` (PRD §F1, M1).
//!
//! Two-way provenance:
//!   * Forward: `amt issue comment` names the entity ids touched.
//!   * Reverse: `hayven remember --kind decision --node <id> --scope <ids>` on
//!     each touched node, naming the AMT-n / D-n reference.
//!
//! `sirius why <symbol>` recalls fleet memory for the node and resolves the
//! AMT-n / D-n references. `sirius why AMT-n` reads the issue activity.

use crate::amt::Amt;
use crate::hayven::Hayven;
use crate::ledger::Ledger;
use regex::Regex;
use serde_json::Value;

/// What kind of thing a link stamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Issue,
    Decision,
}

impl LinkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkKind::Issue => "issue",
            LinkKind::Decision => "decision",
        }
    }
}

/// The result of a `link` operation, mirrors the CONTRACTS §2 JSON shape.
#[derive(Debug, Clone)]
pub struct LinkReceipt {
    pub receipt_id: i64,
    pub kind: LinkKind,
    pub r#ref: String,
    pub symbols: Vec<String>,
    pub forward_ok: bool,
    pub reverse_ok: bool,
}

/// Compose the forward comment body naming the stamped entities.
pub fn forward_comment_body(r#ref: &str, symbols: &[String]) -> String {
    format!(
        "sirius: {} touches {} entit{} — {}",
        r#ref,
        symbols.len(),
        if symbols.len() == 1 { "y" } else { "ies" },
        symbols.join(", ")
    )
}

/// Compose the reverse fleet-memory note naming the reference.
pub fn reverse_note_body(r#ref: &str) -> String {
    format!("Governed by {} (recorded by sirius link)", r#ref)
}

/// Reverse-stamp retry policy (SIRF-4). The `:7777` daemon can return a
/// transient non-zero (e.g. mid-startup / reindex) right when `sirius link`
/// runs; without a retry that leaves a permanent half-receipt (forward_ok but
/// reverse_ok false). So each node's fleet-memory note is retried with
/// exponential backoff before we give up on it.
const REVERSE_ATTEMPTS: u32 = 3;
#[cfg(not(test))]
const REVERSE_BASE_MS: u64 = 250;
#[cfg(test)]
const REVERSE_BASE_MS: u64 = 0; // no real sleeps under test

/// Stamp one node's reverse note, retrying a transient failure with backoff.
/// Returns true once the note lands, false if every attempt failed.
fn remember_with_retry(hv: &Hayven, note: &str, node: &str, scope: &[String]) -> bool {
    for attempt in 0..REVERSE_ATTEMPTS {
        if hv.remember(note, Some(node), "decision", scope).is_ok() {
            return true;
        }
        if attempt + 1 < REVERSE_ATTEMPTS {
            let backoff = REVERSE_BASE_MS.saturating_mul(1u64 << attempt);
            std::thread::sleep(std::time::Duration::from_millis(backoff));
        }
    }
    false
}

/// Stamp both directions and write a receipt row. `worker_id` is optional
/// (the bridge is usable outside the loop).
pub fn link(
    amt: &Amt,
    hv: &Hayven,
    ledger: &Ledger,
    kind: LinkKind,
    r#ref: &str,
    symbols: &[String],
    worker_id: Option<&str>,
) -> Result<LinkReceipt, String> {
    if symbols.is_empty() {
        return Err("no symbols to link (provide --symbols or --changed)".into());
    }

    // Forward stamp: for an issue, comment on the issue. For a decision, comment
    // on the issue the decision resolves — resolve it via `amt decision show`.
    let issue_for_comment = match kind {
        LinkKind::Issue => Some(r#ref.to_string()),
        LinkKind::Decision => amt
            .decision_show(r#ref)
            .ok()
            .and_then(|v| v.get("resolves").and_then(Value::as_str).map(String::from)),
    };
    let forward_ok = if let Some(issue) = issue_for_comment.as_deref() {
        amt.comment(issue, &forward_comment_body(r#ref, symbols))
            .is_ok()
    } else {
        false
    };

    // Reverse stamp: a decision note on each node, scoped to the whole set.
    // Each node is retried on a transient daemon failure (SIRF-4) so one hiccup
    // doesn't leave a permanent half-receipt.
    let note = reverse_note_body(r#ref);
    let mut reverse_all = true;
    for node in symbols {
        if !remember_with_retry(hv, &note, node, symbols) {
            reverse_all = false;
        }
    }
    // If there were symbols but none stamped, reverse is not ok.
    let reverse_ok = reverse_all;

    let receipt_id = ledger
        .insert_receipt(
            kind.as_str(),
            r#ref,
            symbols,
            forward_ok,
            reverse_ok,
            worker_id,
        )
        .map_err(|e| format!("ledger write failed: {e}"))?;

    Ok(LinkReceipt {
        receipt_id,
        kind,
        r#ref: r#ref.to_string(),
        symbols: symbols.to_vec(),
        forward_ok,
        reverse_ok,
    })
}

/// `sirius why <symbol>`: recall fleet memory for the node, extract AMT-n / D-n
/// refs, and resolve their titles/summaries from Ametrite.
#[derive(Debug, Clone)]
pub struct WhySymbol {
    pub symbol: String,
    pub issues: Vec<(String, String)>,    // (ref, title)
    pub decisions: Vec<(String, String)>, // (ref, summary)
}

pub fn why_symbol(amt: &Amt, hv: &Hayven, symbol: &str) -> Result<WhySymbol, String> {
    let recall = hv.recall_node(symbol).unwrap_or(Value::Null);
    let text = collect_note_text(&recall);
    let (issue_refs, decision_refs) = extract_refs(&text);

    let mut issues = Vec::new();
    for r in issue_refs {
        // Resolve against Ametrite; skip candidates that don't exist (the
        // generic prefix regex may over-match tokens like "UTF-8").
        if let Ok(v) = amt.issue_show(&r) {
            let title = v
                .get("title")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_default();
            issues.push((r, title));
        }
    }
    let mut decisions = Vec::new();
    for r in decision_refs {
        let summary = amt
            .decision_show(&r)
            .ok()
            .and_then(|v| v.get("title").and_then(Value::as_str).map(String::from))
            .unwrap_or_default();
        decisions.push((r, summary));
    }
    Ok(WhySymbol {
        symbol: symbol.to_string(),
        issues,
        decisions,
    })
}

/// `sirius why AMT-n`: read the issue activity and list the symbols + decisions
/// referenced there.
#[derive(Debug, Clone)]
pub struct WhyIssue {
    pub r#ref: String,
    pub symbols: Vec<String>,
    pub decisions: Vec<String>,
}

pub fn why_issue(amt: &Amt, issue: &str) -> Result<WhyIssue, String> {
    let v = amt.issue_show(issue)?;
    let mut activity_text = String::new();
    if let Some(acts) = v.get("activity").and_then(Value::as_array) {
        for a in acts {
            if let Some(b) = a.get("body").and_then(Value::as_str) {
                activity_text.push_str(b);
                activity_text.push('\n');
            }
        }
    }
    // Symbols appear in sirius forward comments as "entities — a, b, c".
    let symbols = extract_symbols_from_comment(&activity_text);
    let (_issues, decisions) = extract_refs(&activity_text);
    Ok(WhyIssue {
        r#ref: issue.to_string(),
        symbols,
        decisions,
    })
}

/// Concatenate all note bodies from a `hayven recall` payload.
fn collect_note_text(v: &Value) -> String {
    let mut s = String::new();
    if let Some(notes) = v.get("notes").and_then(Value::as_array) {
        for n in notes {
            if let Some(t) = n.get("note").and_then(Value::as_str) {
                s.push_str(t);
                s.push('\n');
            }
        }
    }
    s
}

/// Extract issue (`<PREFIX>-n`) and decision (`D-n`) references from free text.
///
/// The issue prefix is workspace-configurable in Ametrite (`AMT`, `SIRF`, …),
/// so we match any `[A-Z][A-Z0-9]+-\d+` token rather than a fixed prefix. That
/// requires ≥2 leading alphanumerics, which keeps it from colliding with the
/// single-letter `D-n` decision namespace. Callers resolve each candidate
/// against Ametrite and drop anything that doesn't exist, so an over-match is
/// harmless.
pub fn extract_refs(text: &str) -> (Vec<String>, Vec<String>) {
    // Static regexes, compiled once.
    let issue_re = Regex::new(r"\b[A-Z][A-Z0-9]+-\d+\b").unwrap();
    let dec_re = Regex::new(r"\bD-\d+\b").unwrap();
    let mut issues: Vec<String> = Vec::new();
    for m in issue_re.find_iter(text) {
        let s = m.as_str().to_string();
        if !issues.contains(&s) {
            issues.push(s);
        }
    }
    let mut decisions: Vec<String> = Vec::new();
    for m in dec_re.find_iter(text) {
        let s = m.as_str().to_string();
        if !decisions.contains(&s) {
            decisions.push(s);
        }
    }
    (issues, decisions)
}

/// Parse the entity list from a sirius forward comment ("... — a, b, c").
fn extract_symbols_from_comment(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(idx) = line.find(" — ") {
            let (head, tail) = line.split_at(idx);
            if head.contains("sirius:") && head.contains("entit") {
                for sym in tail.trim_start_matches(" — ").split(',') {
                    let s = sym.trim().to_string();
                    if !s.is_empty() && !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::MockRunner;

    #[test]
    fn forward_body_names_entities() {
        let b = forward_comment_body("AMT-7", &["a".into(), "b".into()]);
        assert!(b.contains("AMT-7"));
        assert!(b.contains("2 entities"));
        assert!(b.contains("a, b"));
    }

    #[test]
    fn extract_refs_dedups_and_separates() {
        let text = "Governed by AMT-7. See D-3 and AMT-7 again, plus D-3.";
        let (issues, decisions) = extract_refs(text);
        assert_eq!(issues, vec!["AMT-7"]);
        assert_eq!(decisions, vec!["D-3"]);
    }

    #[test]
    fn link_issue_stamps_both_directions_and_writes_receipt() {
        let m = MockRunner::new();
        // forward comment ok
        m.expect(&["amt", "--json", "issue", "comment"], 0, r#"{"ok":true}"#);
        // reverse remember ok for each node
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem_1"}"#);
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem_2"}"#);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();

        let r = link(
            &amt,
            &hv,
            &led,
            LinkKind::Issue,
            "AMT-7",
            &["nodeA".into(), "nodeB".into()],
            Some("sirius/oak"),
        )
        .unwrap();
        assert!(r.forward_ok);
        assert!(r.reverse_ok);
        assert_eq!(r.receipt_id, 1);

        // The comment happened before the remembers (forward before reverse).
        let calls = m.recorded();
        let comment_idx = calls
            .iter()
            .position(|c| c.contains("issue comment"))
            .unwrap();
        let remember_idx = calls.iter().position(|c| c.contains("remember")).unwrap();
        assert!(comment_idx < remember_idx);
    }

    #[test]
    fn link_empty_symbols_is_error() {
        let m = MockRunner::new();
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        assert!(link(&amt, &hv, &led, LinkKind::Issue, "AMT-7", &[], None).is_err());
    }

    #[test]
    fn link_reverse_failure_marks_reverse_not_ok() {
        let m = MockRunner::new();
        m.expect(&["amt", "--json", "issue", "comment"], 0, r#"{"ok":true}"#);
        // Every attempt fails → reverse stays not ok (SIRF-4 retries exhausted).
        for _ in 0..3 {
            m.push(crate::shell::MockResponse::new(
                &["hayven", "remember"],
                1,
                "",
                "daemon down",
            ));
        }
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let r = link(
            &amt,
            &hv,
            &led,
            LinkKind::Issue,
            "AMT-7",
            &["n".into()],
            None,
        )
        .unwrap();
        assert!(r.forward_ok);
        assert!(!r.reverse_ok);
        // It really did retry the bounded number of times before giving up.
        let attempts = m
            .recorded()
            .iter()
            .filter(|c| c.contains("remember"))
            .count();
        assert_eq!(attempts, 3);
    }

    #[test]
    fn link_reverse_retry_recovers_transient_failure() {
        // SIRF-4: two transient failures then a success → reverse_ok true,
        // no permanent half-receipt.
        let m = MockRunner::new();
        m.expect(&["amt", "--json", "issue", "comment"], 0, r#"{"ok":true}"#);
        m.push(crate::shell::MockResponse::new(
            &["hayven", "remember"],
            1,
            "",
            "daemon not ready",
        ));
        m.push(crate::shell::MockResponse::new(
            &["hayven", "remember"],
            1,
            "",
            "daemon not ready",
        ));
        m.expect(&["hayven", "remember"], 0, r#"{"id":"mem_1"}"#);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        let r = link(
            &amt,
            &hv,
            &led,
            LinkKind::Issue,
            "AMT-7",
            &["n".into()],
            None,
        )
        .unwrap();
        assert!(r.forward_ok);
        assert!(r.reverse_ok);
        let attempts = m
            .recorded()
            .iter()
            .filter(|c| c.contains("remember"))
            .count();
        assert_eq!(attempts, 3);
    }

    #[test]
    fn why_symbol_resolves_refs() {
        let m = MockRunner::new();
        m.expect(
            &["hayven", "recall"],
            0,
            r#"{"count":1,"notes":[{"note":"Governed by AMT-7 (recorded by sirius link)"}]}"#,
        );
        m.expect(
            &["amt", "--json", "issue", "show"],
            0,
            r#"{"id":"AMT-7","title":"Fix the widget"}"#,
        );
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let w = why_symbol(&amt, &hv, "nodeA").unwrap();
        assert_eq!(w.issues.len(), 1);
        assert_eq!(w.issues[0].0, "AMT-7");
        assert_eq!(w.issues[0].1, "Fix the widget");
    }

    #[test]
    fn why_issue_lifts_symbols_from_comment() {
        let m = MockRunner::new();
        m.expect(
            &["amt", "--json", "issue", "show"],
            0,
            r#"{"id":"AMT-7","activity":[
                {"body":"sirius: AMT-7 touches 2 entities — nodeA, nodeB"},
                {"body":"decided D-3"}
            ]}"#,
        );
        let amt = Amt::new(&m);
        let w = why_issue(&amt, "AMT-7").unwrap();
        assert_eq!(w.symbols, vec!["nodeA", "nodeB"]);
        assert_eq!(w.decisions, vec!["D-3"]);
    }
}
