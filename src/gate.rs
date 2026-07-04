//! The Gate — `sirius gate AMT-n` (PRD §F2, M2).
//!
//! Runs affected-tests over the issue's mapped/claimed entities. Pass advances
//! the issue via `amt issue update --status <target>`; fail files the failure as
//! an issue comment and leaves status untouched.
//!
//! Ground-truth note (CONTRACTS §6): `hayven affected-tests` has no `--gate` /
//! `--gate-tier` flags in 0.0.5. Sirius derives the gate verdict from the
//! command's exit code (0 pass, non-zero fail), which is the PRD §6 fact-4
//! contract. The requested tier is recorded in the ledger/output and applied
//! when a future hayven exposes it, but is not sent to 0.0.5.

use crate::amt::Amt;
use crate::hayven::Hayven;
use crate::ledger::Ledger;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct GateOutcome {
    pub issue: String,
    pub tier: String,
    pub passed: bool,
    pub advanced_to: Option<String>,
    pub tests_selected: usize,
    pub comment_filed: bool,
}

/// Map an issue to the symbols to gate on. Preference order:
///   1. symbols already recorded in the ledger receipts for this issue,
///   2. otherwise map via `hayven query` on the issue title/terms.
pub fn symbols_for_issue(
    amt: &Amt,
    hv: &Hayven,
    ledger: &Ledger,
    issue: &str,
) -> Result<Vec<String>, String> {
    // 1. ledger receipts.
    let mut syms = receipt_symbols(ledger, issue)?;
    if !syms.is_empty() {
        return Ok(syms);
    }
    // 2. map from the issue title.
    let v = amt.issue_show(issue)?;
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(issue)
        .to_string();
    if let Ok(q) = hv.query(&title) {
        syms = crate::gitrange::extract_ids(&q);
    }
    Ok(syms)
}

/// Symbols previously stamped for this issue, read from the ledger.
fn receipt_symbols(ledger: &Ledger, issue: &str) -> Result<Vec<String>, String> {
    let mut stmt = ledger
        .conn
        .prepare("SELECT symbols FROM receipts WHERE ref = ?1 ORDER BY id DESC LIMIT 1")
        .map_err(|e| e.to_string())?;
    let row: Option<String> = stmt.query_row([issue], |r| r.get(0)).ok();
    match row {
        Some(json) => Ok(serde_json::from_str(&json).unwrap_or_default()),
        None => Ok(Vec::new()),
    }
}

/// Run the gate. `target_status` and `tier` come from config unless overridden.
pub fn run_gate(
    amt: &Amt,
    hv: &Hayven,
    ledger: &Ledger,
    issue: &str,
    tier: &str,
    target_status: &str,
) -> Result<GateOutcome, String> {
    let symbols = symbols_for_issue(amt, hv, ledger, issue)?;
    if symbols.is_empty() {
        return Err(format!(
            "cannot gate {issue}: no mapped symbols (link it first or ensure hayven maps its title)"
        ));
    }

    // Gate on the first mapped symbol, passing the rest as --changed context.
    // (0.0.5 affected-tests takes one primary symbol + a --changed set.)
    let primary = &symbols[0];
    let changed: Vec<String> = symbols[1..].to_vec();
    let result = hv.affected_tests(primary, Some(&changed))?;

    if result.passed {
        // Advance status.
        amt.update_status(issue, target_status)?;
        ledger
            .log_policy_event(
                None,
                "gate_tier",
                &serde_json::json!({
                    "issue": issue, "tier": tier, "result": "pass",
                    "tests_selected": result.selected, "advanced_to": target_status
                }),
            )
            .ok();
        Ok(GateOutcome {
            issue: issue.to_string(),
            tier: tier.to_string(),
            passed: true,
            advanced_to: Some(target_status.to_string()),
            tests_selected: result.selected,
            comment_filed: false,
        })
    } else {
        // File the failure as a comment; leave status untouched.
        let body = format!(
            "sirius gate FAILED (tier {tier}): affected-tests did not pass for {}. detail: {}",
            symbols.join(", "),
            result.detail.lines().next().unwrap_or("").trim()
        );
        let comment_filed = amt.comment(issue, &body).is_ok();
        ledger
            .log_policy_event(
                None,
                "gate_tier",
                &serde_json::json!({
                    "issue": issue, "tier": tier, "result": "fail",
                    "tests_selected": result.selected
                }),
            )
            .ok();
        Ok(GateOutcome {
            issue: issue.to_string(),
            tier: tier.to_string(),
            passed: false,
            advanced_to: None,
            tests_selected: result.selected,
            comment_filed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};

    fn ledger_with_receipt(issue: &str, syms: &[&str]) -> Ledger {
        let led = Ledger::open_in_memory().unwrap();
        led.insert_receipt(
            "issue",
            issue,
            &syms.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            true,
            true,
            None,
        )
        .unwrap();
        led
    }

    #[test]
    fn gate_pass_advances_status() {
        let m = MockRunner::new();
        m.expect(&["hayven", "affected-tests"], 0, r#"{"tests":["t1","t2"]}"#);
        m.expect(
            &["amt", "--json", "issue", "update"],
            0,
            r#"{"id":"AMT-7"}"#,
        );
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = ledger_with_receipt("AMT-7", &["nodeA", "nodeB"]);

        let o = run_gate(&amt, &hv, &led, "AMT-7", "safe", "in_review").unwrap();
        assert!(o.passed);
        assert_eq!(o.advanced_to.as_deref(), Some("in_review"));
        assert_eq!(o.tests_selected, 2);
        assert!(!o.comment_filed);
        // status update was called, no comment.
        assert!(m.recorded().iter().any(|c| c.contains("issue update")));
        assert!(!m.recorded().iter().any(|c| c.contains("issue comment")));
    }

    #[test]
    fn gate_fail_files_comment_and_leaves_status() {
        let m = MockRunner::new();
        m.push(MockResponse::new(
            &["hayven", "affected-tests"],
            1,
            "",
            "regression",
        ));
        m.expect(&["amt", "--json", "issue", "comment"], 0, r#"{"ok":true}"#);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = ledger_with_receipt("AMT-7", &["nodeA"]);

        let o = run_gate(&amt, &hv, &led, "AMT-7", "safe", "in_review").unwrap();
        assert!(!o.passed);
        assert!(o.advanced_to.is_none());
        assert!(o.comment_filed);
        assert!(m.recorded().iter().any(|c| c.contains("issue comment")));
        assert!(!m.recorded().iter().any(|c| c.contains("issue update")));
    }

    #[test]
    fn gate_without_symbols_errors() {
        let m = MockRunner::new();
        // issue_show returns a title; query returns no hits.
        m.expect(
            &["amt", "--json", "issue", "show"],
            0,
            r#"{"id":"AMT-9","title":"x"}"#,
        );
        m.expect(&["hayven", "query"], 0, r#"{"hits":[]}"#);
        let amt = Amt::new(&m);
        let hv = Hayven::new(&m);
        let led = Ledger::open_in_memory().unwrap();
        assert!(run_gate(&amt, &hv, &led, "AMT-9", "safe", "in_review").is_err());
    }
}
