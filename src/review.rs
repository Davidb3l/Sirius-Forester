//! The review stage (SIRF-23) — the pure half.
//!
//! In the first real fleet run every ticket passed the gate, yet fresh-eyes
//! reviews found 25 confirmed bugs among them; the workers' own self-review
//! subagents shared the author's assumptions and missed all of them. So after
//! the gate passes, Sirius runs a SEPARATE reviewer process over the diff, and
//! on any confirmed bug runs the worker in fix mode, re-gates, and re-reviews
//! until the review is clean or `review.max_rounds` is spent.
//!
//! This module owns the contracts and the decisions; `run.rs` owns the
//! processes. Everything here is pure and unit-tested:
//!   * the reviewer's findings JSON and the fix-mode worker's responses JSON,
//!   * the blocking rule (`kind ∈ block_on` AND `confidence == "confirmed"`),
//!   * `skip_paths` globs (no glob crate — allowed deps are fixed),
//!   * prompt-template rendering,
//!   * round-to-round bookkeeping (resolved / rebuttal accepted / still open)
//!     and the human-facing comment and receipt text.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The default reviewer prompt, shipped as `.sirius/review-prompt.md`. This is
/// the wording that produced 25 confirmed bugs with very few false positives
/// on the Gramxy run. `$SIRIUS_*` placeholders are rendered per round.
pub const DEFAULT_PROMPT: &str = r#"Fresh-eyes correctness review. You did not write this code.

Review ONLY the diff `git diff $SIRIUS_DIFF_RANGE` (run it in $SIRIUS_REVIEW_DIR) for issue $SIRIUS_ISSUE. Read the spec and its "Done when" with `amt issue show $SIRIUS_ISSUE`.

Rules:
- Do NOT edit, stage, or commit any file in $SIRIUS_REVIEW_DIR or $SIRIUS_WORKTREE. Sirius checks the tree before and after; a review that changes it is discarded.
- If you need to run the code, use your own scratch copy and your own port, and remove them afterwards.

Focus on: correctness against the spec, regressions in callers of anything the diff changed, interaction with recently merged work, escaping/security, and data safety.

This is review round $SIRIUS_ROUND. If $SIRIUS_REVIEW_FINDINGS names a file, it holds the PREVIOUS round's findings with the worker's response to each ("fixed" or "rebutted"). Verify every one: report it in "previous" as "resolved" (the fix works), "accepted" (the rebuttal is right — it was not a bug), or "unresolved" (still broken, or the rebuttal is wrong — then also list it again in "findings" under its ORIGINAL id, e.g. "R1-1", never a new one). Use new ids (R$SIRIUS_ROUND-n) only for NEW findings.

Write $SIRIUS_REVIEW_OUT as exactly this JSON (nothing else in the file):

{ "findings": [
    { "id": "R$SIRIUS_ROUND-1", "kind": "bug|conflict|minor|design", "confidence": "confirmed|uncertain",
      "file": "path/to/file", "line": 123,
      "summary": "one sentence", "scenario": "a concrete repro: inputs/state -> wrong result", "fix": "the suggested fix" } ],
  "previous": [ { "id": "R1-1", "verdict": "resolved|accepted|unresolved", "note": "why" } ],
  "checked": [ "what you verified is OK, free text" ] }

Report a bug as "confirmed" ONLY when you have verified it against the code and can state the concrete failure. Everything you are unsure of goes in as "uncertain" — it is posted as a note and never blocks. An empty "findings" array means the review is clean.
"#;

/// One reviewer finding (`$SIRIUS_REVIEW_OUT` → `findings[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default)]
    pub id: String,
    /// `bug | conflict | minor | design` (free-form; only `block_on` matters).
    pub kind: String,
    /// `confirmed | uncertain`.
    pub confidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Lenient: `250`, `"250"`, or `"250-260"` (the first number) all parse;
    /// anything else is dropped rather than failing the whole review.
    #[serde(
        default,
        deserialize_with = "lenient_line",
        skip_serializing_if = "Option::is_none"
    )]
    pub line: Option<u64>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub scenario: String,
    #[serde(default)]
    pub fix: String,
    /// Filled in by Sirius from the fix-mode worker's response, so the next
    /// round's reviewer sees each finding next to its answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<FixResponse>,
}

impl Finding {
    /// `file:line` (or just the file), for comments.
    pub fn location(&self) -> String {
        match (&self.file, self.line) {
            (Some(f), Some(l)) => format!("{f}:{l}"),
            (Some(f), None) => f.clone(),
            _ => "(no location)".into(),
        }
    }
}

fn lenient_line<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => {
            let digits: String = s
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            digits.parse().ok()
        }
        _ => None,
    })
}

/// The reviewer's verdict on one PREVIOUS finding, given the worker's response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub id: String,
    /// `resolved` (the fix works) | `accepted` (the rebuttal is right) |
    /// `unresolved` (still broken / rebuttal rejected). Missing ⇒ "" (treated
    /// as no verdict), never a malformed review.
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub note: String,
}

impl Verdict {
    /// True when the reviewer says the finding is closed.
    pub fn closes(&self) -> bool {
        matches!(
            self.verdict.trim().to_ascii_lowercase().as_str(),
            "resolved" | "accepted" | "fixed" | "closed"
        )
    }
}

/// The reviewer's whole output.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReviewReport {
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub previous: Vec<Verdict>,
    #[serde(default)]
    pub checked: Vec<String>,
}

/// One fix-mode response (`$SIRIUS_FIX_OUT` → `responses[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixResponse {
    pub id: String,
    /// `fixed | rebutted`.
    pub status: String,
    #[serde(default)]
    pub note: String,
}

impl FixResponse {
    pub fn is_rebuttal(&self) -> bool {
        self.status.trim().eq_ignore_ascii_case("rebutted")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FixReport {
    #[serde(default)]
    pub responses: Vec<FixResponse>,
}

/// Parse the reviewer's JSON. A missing `findings` array, a non-object, or a
/// finding without `kind`/`confidence` is MALFORMED — a review error, never a
/// silently clean review. Findings without an id get `R<round>-<n>`.
pub fn parse_review(raw: &str, round: u32) -> Result<ReviewReport, String> {
    let v: Value =
        serde_json::from_str(raw.trim()).map_err(|e| format!("review output is not JSON: {e}"))?;
    if !v.get("findings").is_some_and(Value::is_array) {
        return Err("review output has no \"findings\" array".into());
    }
    let mut report: ReviewReport =
        serde_json::from_value(v).map_err(|e| format!("review output is malformed: {e}"))?;
    for (i, f) in report.findings.iter_mut().enumerate() {
        if f.id.trim().is_empty() {
            f.id = format!("R{round}-{}", i + 1);
        }
        f.response = None; // only Sirius attaches responses
    }
    Ok(report)
}

/// Parse the fix-mode worker's responses. Lenient: a worker that fixed things
/// but wrote no (or broken) response file still had its fixes re-reviewed, so
/// a missing file just means "no responses", never an error.
pub fn parse_fix(raw: Option<&str>) -> FixReport {
    raw.and_then(|r| serde_json::from_str(r.trim()).ok())
        .unwrap_or_default()
}

/// The blocking rule: a confirmed finding of a blocking kind.
pub fn is_blocking(f: &Finding, block_on: &[String]) -> bool {
    f.confidence.trim().eq_ignore_ascii_case("confirmed")
        && block_on
            .iter()
            .any(|k| k.trim().eq_ignore_ascii_case(f.kind.trim()))
}

/// Translate a path glob to an anchored regex: `**/` matches zero or more
/// directories, `**` anything, `*` anything but `/`, `?` one non-`/` char.
fn glob_regex(glob: &str) -> Option<Regex> {
    let mut re = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
            }
            '*' => {
                re.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                re.push_str("[^/]");
                i += 1;
            }
            c => {
                re.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    re.push('$');
    Regex::new(&re).ok()
}

/// True when the diff is non-empty and EVERY file matches a skip glob — a
/// doc-only change, say, which is not worth a reviewer's 100k+ tokens.
pub fn all_skippable(files: &[String], skip_paths: &[String]) -> bool {
    if files.is_empty() || skip_paths.is_empty() {
        return false;
    }
    let globs: Vec<Regex> = skip_paths.iter().filter_map(|g| glob_regex(g)).collect();
    files
        .iter()
        .all(|f| globs.iter().any(|g| g.is_match(f.trim_start_matches("./"))))
}

/// Render `$NAME` / `${NAME}` placeholders in a prompt template. Longest names
/// first, so `$SIRIUS_REVIEW_FINDINGS` is never half-eaten by a shorter name.
pub fn render_prompt(template: &str, vars: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = vars.iter().collect();
    sorted.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    let mut out = template.to_string();
    for (k, v) in sorted {
        out = out.replace(&format!("${{{k}}}"), v);
        out = out.replace(&format!("${k}"), v);
    }
    out
}

/// How one review round ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundResult {
    /// No blocking findings.
    Clean,
    /// At least one blocking finding.
    Blocking,
    /// Missing/malformed output, non-zero exit, or a timeout.
    Error,
    /// The reviewer changed the tree; its changes were discarded.
    Tampered,
}

impl RoundResult {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoundResult::Clean => "clean",
            RoundResult::Blocking => "blocking",
            RoundResult::Error => "error",
            RoundResult::Tampered => "tampered",
        }
    }
}

/// The open findings after a round, given the previous round's blocking
/// findings (with responses attached) and the new report. Also counts what
/// closed. A previous finding stays OPEN when the reviewer explicitly says
/// `unresolved`, even if it forgot to list it again — the explicit verdict
/// wins. A previous finding with no verdict is closed iff not re-reported
/// under the same id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reconciled {
    pub blocking: Vec<Finding>,
    pub notes: Vec<Finding>,
    /// Previous findings now closed whose response was `fixed` (or none).
    pub fixed: usize,
    /// Previous findings now closed whose response was `rebutted`.
    pub rebuttals_accepted: usize,
    /// Previous rebuttals the reviewer did NOT accept.
    pub rebuttals_rejected: usize,
}

pub fn reconcile(previous: &[Finding], report: &ReviewReport, block_on: &[String]) -> Reconciled {
    let mut out = Reconciled::default();
    for f in &report.findings {
        if is_blocking(f, block_on) {
            out.blocking.push(f.clone());
        } else {
            out.notes.push(f.clone());
        }
    }
    for p in previous {
        let verdict = report.previous.iter().find(|v| v.id == p.id);
        let re_reported = out.blocking.iter().any(|f| f.id == p.id);
        // A finding re-reported under its own id is OPEN, whatever the
        // verdict says (contradictory output must never count as "fixed").
        let closed = !re_reported && verdict.map_or(true, Verdict::closes);
        let rebutted = p.response.as_ref().is_some_and(FixResponse::is_rebuttal);
        if closed {
            if rebutted {
                out.rebuttals_accepted += 1;
            } else {
                out.fixed += 1;
            }
        } else {
            if rebutted {
                out.rebuttals_rejected += 1;
            }
            if !re_reported {
                // Explicitly unresolved but not re-listed: keep it open.
                let mut still = p.clone();
                still.response = None;
                out.blocking.push(still);
            }
        }
    }
    out
}

/// Attach each fix response to its finding (by id) for the next round.
pub fn attach_responses(findings: &mut [Finding], fix: &FixReport) {
    for f in findings.iter_mut() {
        f.response = fix.responses.iter().find(|r| r.id == f.id).cloned();
    }
}

fn bullet(f: &Finding) -> String {
    let mut s = format!("- [{}] {} — {}", f.id, f.location(), f.summary);
    if !f.scenario.is_empty() {
        s.push_str(&format!("\n  scenario: {}", f.scenario));
    }
    if !f.fix.is_empty() {
        s.push_str(&format!("\n  fix: {}", f.fix));
    }
    s
}

/// The per-round issue comment: "Review round N: K confirmed bugs (list), M notes."
pub fn round_comment(round: u32, rec: &Reconciled, had_previous: bool) -> String {
    let mut s = format!(
        "Review round {round}: {} confirmed blocking finding{}, {} note{}.",
        rec.blocking.len(),
        if rec.blocking.len() == 1 { "" } else { "s" },
        rec.notes.len(),
        if rec.notes.len() == 1 { "" } else { "s" },
    );
    if had_previous {
        s.push_str(&format!(
            "\nPrevious round: {} fixed, {} rebuttal(s) accepted, {} rebuttal(s) not accepted.",
            rec.fixed, rec.rebuttals_accepted, rec.rebuttals_rejected
        ));
    }
    for f in &rec.blocking {
        s.push('\n');
        s.push_str(&bullet(f));
    }
    if !rec.notes.is_empty() {
        s.push_str("\nNotes (non-blocking):");
        for f in &rec.notes {
            s.push('\n');
            s.push_str(&bullet(f));
        }
    }
    s
}

/// The fix-round issue comment: "Fixed: … / Rebutted: …".
pub fn fix_comment(round: u32, fix: &FixReport) -> String {
    let fixed: Vec<&FixResponse> = fix.responses.iter().filter(|r| !r.is_rebuttal()).collect();
    let rebutted: Vec<&FixResponse> = fix.responses.iter().filter(|r| r.is_rebuttal()).collect();
    let mut s = format!("Fix round {round}:");
    if fix.responses.is_empty() {
        s.push_str(" the worker wrote no responses (its changes are re-reviewed regardless).");
    }
    if !fixed.is_empty() {
        s.push_str("\nFixed:");
        for r in fixed {
            s.push_str(&format!("\n- [{}] {}", r.id, r.note));
        }
    }
    if !rebutted.is_empty() {
        s.push_str("\nRebutted (the next review decides):");
        for r in rebutted {
            s.push_str(&format!("\n- [{}] {}", r.id, r.note));
        }
    }
    s
}

/// The final escalation comment listing every unresolved confirmed finding.
pub fn unresolved_comment(why: &str, open: &[Finding]) -> String {
    let mut s = format!("sirius review: {why}. Unresolved confirmed findings:");
    if open.is_empty() {
        s.push_str(" none recorded (the review itself could not complete).");
    }
    for f in open {
        s.push('\n');
        s.push_str(&bullet(f));
    }
    s
}

/// The review summary for the receipt: "review: 2 rounds, 4 bugs fixed, 1
/// rebuttal accepted".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReviewSummary {
    pub rounds: u32,
    pub fixed: usize,
    pub rebuttals_accepted: usize,
    pub open: usize,
    /// `clean | skipped | flagged | released`.
    pub outcome: String,
}

impl ReviewSummary {
    pub fn line(&self) -> String {
        if self.outcome == "skipped" {
            return "review: skipped (nothing reviewable changed)".into();
        }
        if self.open == 0 && (self.outcome == "flagged" || self.outcome == "released") {
            // Escalated with nothing confirmed open = the review itself never
            // completed. Saying "0 bugs fixed" would read exactly like a clean
            // review — a receipt must never claim a review that did not happen.
            return format!(
                "review: did not complete after {} round{} ({})",
                self.rounds,
                if self.rounds == 1 { "" } else { "s" },
                self.outcome
            );
        }
        let mut s = format!(
            "review: {} round{}, {} bug{} fixed, {} rebuttal{} accepted",
            self.rounds,
            if self.rounds == 1 { "" } else { "s" },
            self.fixed,
            if self.fixed == 1 { "" } else { "s" },
            self.rebuttals_accepted,
            if self.rebuttals_accepted == 1 {
                ""
            } else {
                "s"
            },
        );
        if self.open > 0 {
            s.push_str(&format!(", {} still open ({})", self.open, self.outcome));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bug(id: &str, kind: &str, conf: &str) -> Finding {
        Finding {
            id: id.into(),
            kind: kind.into(),
            confidence: conf.into(),
            file: Some("a.rs".into()),
            line: Some(3),
            summary: "s".into(),
            scenario: String::new(),
            fix: String::new(),
            response: None,
        }
    }

    fn block() -> Vec<String> {
        vec!["bug".into(), "conflict".into()]
    }

    #[test]
    fn parse_review_requires_a_findings_array() {
        assert!(parse_review("", 1).is_err());
        assert!(parse_review("not json", 1).is_err());
        assert!(parse_review(r#"{"checked":[]}"#, 1).is_err());
        assert!(parse_review(r#"{"findings":{}}"#, 1).is_err());
        // A finding without kind/confidence is malformed, not silently dropped.
        assert!(parse_review(r#"{"findings":[{"summary":"x"}]}"#, 1).is_err());
        let r = parse_review(r#"{"findings":[]}"#, 1).unwrap();
        assert!(r.findings.is_empty());
    }

    #[test]
    fn parse_review_assigns_ids_and_strips_forged_responses() {
        let r = parse_review(
            r#"{"findings":[{"kind":"bug","confidence":"confirmed",
                 "response":{"id":"x","status":"fixed"}},
                {"id":"mine","kind":"minor","confidence":"uncertain"}]}"#,
            2,
        )
        .unwrap();
        assert_eq!(r.findings[0].id, "R2-1");
        assert_eq!(r.findings[0].response, None);
        assert_eq!(r.findings[1].id, "mine");
    }

    #[test]
    fn blocking_rule_needs_confirmed_and_a_blocking_kind() {
        assert!(is_blocking(&bug("1", "bug", "confirmed"), &block()));
        assert!(is_blocking(&bug("1", "Conflict", "CONFIRMED"), &block()));
        assert!(!is_blocking(&bug("1", "bug", "uncertain"), &block()));
        assert!(!is_blocking(&bug("1", "minor", "confirmed"), &block()));
        assert!(!is_blocking(&bug("1", "design", "confirmed"), &block()));
    }

    #[test]
    fn skip_globs_match_only_when_every_file_is_skippable() {
        let skip = vec!["**/*.md".to_string(), "docs/**".to_string()];
        assert!(all_skippable(&["README.md".into()], &skip));
        assert!(all_skippable(
            &["a/b/c.md".into(), "docs/x/y.png".into()],
            &skip
        ));
        assert!(!all_skippable(
            &["README.md".into(), "src/a.rs".into()],
            &skip
        ));
        assert!(!all_skippable(&["src/md.rs".into()], &skip));
        // `*` does not cross directories.
        assert!(!all_skippable(&["src/a.rs".into()], &["*.rs".into()]));
        assert!(all_skippable(&["a.rs".into()], &["*.rs".into()]));
        // An empty diff is never "skippable" (there is nothing to judge).
        assert!(!all_skippable(&[], &skip));
        // Regex metacharacters in globs are literal.
        assert!(all_skippable(&["a+b.txt".into()], &["a+b.txt".into()]));
        assert!(!all_skippable(&["aab.txt".into()], &["a+b.txt".into()]));
    }

    #[test]
    fn render_prompt_replaces_longest_names_first() {
        let vars = vec![
            ("SIRIUS_REVIEW".to_string(), "WRONG".to_string()),
            ("SIRIUS_REVIEW_OUT".to_string(), "/tmp/out.json".to_string()),
            ("SIRIUS_ROUND".to_string(), "2".to_string()),
        ];
        let out = render_prompt("write $SIRIUS_REVIEW_OUT (round ${SIRIUS_ROUND})", &vars);
        assert_eq!(out, "write /tmp/out.json (round 2)");
        // The shipped default renders without leaving known placeholders.
        let all = vec![
            ("SIRIUS_DIFF_RANGE".into(), "b..HEAD".into()),
            ("SIRIUS_REVIEW_DIR".into(), "/w".into()),
            ("SIRIUS_ISSUE".into(), "AMT-1".into()),
            ("SIRIUS_ROUND".into(), "1".into()),
            ("SIRIUS_REVIEW_FINDINGS".into(), "(none)".into()),
            ("SIRIUS_REVIEW_OUT".into(), "/o.json".into()),
            ("SIRIUS_WORKTREE".into(), "/w".into()),
        ];
        let r = render_prompt(DEFAULT_PROMPT, &all);
        assert!(!r.contains("$SIRIUS_"), "unrendered placeholder in:\n{r}");
        assert!(r.contains("git diff b..HEAD"));
    }

    #[test]
    fn reconcile_closes_fixed_and_accepted_and_keeps_explicit_unresolved() {
        let mut prev = vec![
            bug("R1-1", "bug", "confirmed"),
            bug("R1-2", "bug", "confirmed"),
            bug("R1-3", "bug", "confirmed"),
        ];
        attach_responses(
            &mut prev,
            &FixReport {
                responses: vec![
                    FixResponse {
                        id: "R1-1".into(),
                        status: "fixed".into(),
                        note: String::new(),
                    },
                    FixResponse {
                        id: "R1-2".into(),
                        status: "rebutted".into(),
                        note: "not a bug".into(),
                    },
                    FixResponse {
                        id: "R1-3".into(),
                        status: "rebutted".into(),
                        note: "nope".into(),
                    },
                ],
            },
        );
        // Round 2: R1-1 resolved implicitly, R1-2 rebuttal accepted, R1-3
        // explicitly unresolved but NOT re-listed, plus one fresh bug.
        let report = ReviewReport {
            findings: vec![
                bug("R2-1", "bug", "confirmed"),
                bug("R2-2", "minor", "confirmed"),
            ],
            previous: vec![
                Verdict {
                    id: "R1-2".into(),
                    verdict: "accepted".into(),
                    note: String::new(),
                },
                Verdict {
                    id: "R1-3".into(),
                    verdict: "unresolved".into(),
                    note: String::new(),
                },
            ],
            checked: vec![],
        };
        let rec = reconcile(&prev, &report, &block());
        assert_eq!(rec.fixed, 1);
        assert_eq!(rec.rebuttals_accepted, 1);
        assert_eq!(rec.rebuttals_rejected, 1);
        let ids: Vec<&str> = rec.blocking.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["R2-1", "R1-3"]);
        assert_eq!(rec.notes.len(), 1);
    }

    #[test]
    fn reconcile_rereported_without_verdict_stays_open_once() {
        let prev = vec![bug("R1-1", "bug", "confirmed")];
        let report = ReviewReport {
            findings: vec![bug("R1-1", "bug", "confirmed")],
            ..Default::default()
        };
        let rec = reconcile(&prev, &report, &block());
        assert_eq!(rec.blocking.len(), 1, "no duplicate when re-listed");
        assert_eq!(rec.fixed, 0);
    }

    #[test]
    fn summary_line_reads_like_the_spec() {
        let s = ReviewSummary {
            rounds: 2,
            fixed: 4,
            rebuttals_accepted: 1,
            open: 0,
            outcome: "clean".into(),
        };
        assert_eq!(
            s.line(),
            "review: 2 rounds, 4 bugs fixed, 1 rebuttal accepted"
        );
        let flagged = ReviewSummary {
            rounds: 3,
            fixed: 0,
            rebuttals_accepted: 0,
            open: 2,
            outcome: "flagged".into(),
        };
        assert!(flagged.line().ends_with("2 still open (flagged)"));
    }

    #[test]
    fn incomplete_review_never_reads_as_clean() {
        let s = ReviewSummary {
            rounds: 1,
            outcome: "flagged".into(),
            ..Default::default()
        };
        assert_eq!(s.line(), "review: did not complete after 1 round (flagged)");
        assert!(!s.line().contains("0 bugs fixed"));
    }

    #[test]
    fn reported_line_and_missing_verdict_are_lenient() {
        let r = parse_review(
            r#"{"findings":[{"kind":"bug","confidence":"confirmed","line":"250-260"},
                            {"kind":"bug","confidence":"confirmed","line":"n/a"}],
                "previous":[{"id":"R1-1"}]}"#,
            2,
        )
        .unwrap();
        assert_eq!(r.findings[0].line, Some(250));
        assert_eq!(r.findings[1].line, None);
        assert_eq!(r.previous[0].verdict, "");
    }

    #[test]
    fn rereported_finding_stays_open_even_with_a_resolved_verdict() {
        let prev = vec![bug("R1-1", "bug", "confirmed")];
        let report = ReviewReport {
            findings: vec![bug("R1-1", "bug", "confirmed")],
            previous: vec![Verdict {
                id: "R1-1".into(),
                verdict: "resolved".into(),
                note: String::new(),
            }],
            checked: vec![],
        };
        let rec = reconcile(&prev, &report, &block());
        assert_eq!(rec.blocking.len(), 1);
        assert_eq!(rec.fixed, 0, "contradictory output must not count as fixed");
    }

    #[test]
    fn parse_fix_is_lenient() {
        assert!(parse_fix(None).responses.is_empty());
        assert!(parse_fix(Some("garbage")).responses.is_empty());
        let f = parse_fix(Some(r#"{"responses":[{"id":"R1-1","status":"fixed"}]}"#));
        assert_eq!(f.responses.len(), 1);
    }
}
