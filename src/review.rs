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

/// The built-in reviewer prompt (a file at `review.prompt_file` overrides it).
/// Its core is the wording that produced 25 confirmed bugs with very few false
/// positives on the Gramxy run. `$SIRIUS_*` placeholders are rendered per round.
pub const DEFAULT_PROMPT: &str = r#"Fresh-eyes correctness review. You did not write this code.

Review ONLY the diff `git diff $SIRIUS_DIFF_RANGE` (run it in $SIRIUS_REVIEW_DIR) for issue $SIRIUS_ISSUE. Read the spec and its "Done when" with `amt issue show $SIRIUS_ISSUE`.

Rules:
- Do NOT edit, stage, or commit any file in $SIRIUS_REVIEW_DIR or $SIRIUS_WORKTREE. Sirius checks the tree before and after; a review that changes it is discarded.
- You may not be allowed to write files at all. That is expected: your FINAL message is how you deliver the review (see the end).
- If you need to run the code, use your own scratch copy and your own port, and remove them afterwards.

Focus on: correctness against the spec, regressions in callers of anything the diff changed, interaction with recently merged work, escaping/security, and data safety.

Other in-flight changes (other issues' branches awaiting integration; when this review runs against the frontier they are already merged into $SIRIUS_REVIEW_DIR). Review only this issue's diff, but if it breaks once both land — a shared invariant, a contract, producer/consumer parity — report that as a "bug" naming the other issue:
$SIRIUS_SIBLINGS

Known escape patterns in this repo — defects that got past review here before. Check this diff for each one specifically:
$SIRIUS_ESCAPES

This is review round $SIRIUS_ROUND. If $SIRIUS_REVIEW_FINDINGS names a file, it holds the PREVIOUS round's findings with the worker's response to each ("fixed" or "rebutted"). Verify every one: report it in "previous" as "resolved" (the fix works), "accepted" (the rebuttal is right — it was not a bug), or "unresolved" (still broken, or the rebuttal is wrong — then also list it again in "findings" under its ORIGINAL id, e.g. "R1-1", never a new one). Use new ids (R$SIRIUS_ROUND-n) only for NEW findings.

Deliver the review as JSON — your FINAL message must be exactly this JSON object and nothing else (no prose before or after it, no request for permissions). If you are able to write files, also write the same JSON to $SIRIUS_REVIEW_OUT; if not, the final message alone is enough:

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
        // An unfilled template ("bug|conflict|minor|design") is never a real
        // finding — accepting it would read as a clean, non-blocking note.
        if f.kind.contains('|') || f.confidence.contains('|') {
            return Err(format!(
                "review output contains the prompt's template, not findings (kind `{}`)",
                f.kind
            ));
        }
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

/// Find the reviewer's findings JSON in its captured output (the fallback
/// when it could not write `$SIRIUS_REVIEW_OUT`). Strict on purpose: the
/// object must be the output's ENDING — only whitespace, a closing ``` fence,
/// and Sirius's `[sirius] agent exit` trailer may follow it. An object that is
/// merely the LAST one would let an echoed prompt template, a quoted findings
/// file, or a fixture from the diff stand in for the review whenever the
/// reviewer's real answer is missing or malformed — a false clean pass.
pub fn findings_json_in(log: &str) -> Option<String> {
    // The final message sits at the end; bounding the scan to the tail keeps
    // a verbose reviewer's multi-MB log cheap.
    const TAIL: usize = 256 * 1024;
    let mut cut = log.len().saturating_sub(TAIL);
    while !log.is_char_boundary(cut) {
        cut += 1;
    }
    let tail = &log[cut..];
    let is_ending = |rest: &str| {
        rest.lines().all(|l| {
            let l = l.trim();
            l.is_empty() || l.chars().all(|c| c == '`') || l.starts_with("[sirius] agent exit")
        })
    };
    for (i, _) in tail.match_indices('{') {
        let mut it = serde_json::Deserializer::from_str(&tail[i..]).into_iter::<Value>();
        if let Some(Ok(v)) = it.next() {
            let end = i + it.byte_offset();
            if v.get("findings").is_some_and(Value::is_array) && is_ending(&tail[end..]) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The heading under which `$SIRIUS_ESCAPES` is appended to a custom prompt.
pub const ESCAPES_HEADING: &str =
    "Known escape patterns in this repo — defects that got past review here before. Check this diff for each one specifically:";

/// A custom template that predates a section still gets it: append
/// `heading` + `value` when the template has no `$NAME` / `${NAME}` and the
/// value says something.
pub fn append_missing_section(
    rendered: &mut String,
    template: &str,
    name: &str,
    value: &str,
    heading: &str,
) {
    let has = template.contains(&format!("${name}")) || template.contains(&format!("${{{name}}}"));
    if !has && value != "(none)" && !value.is_empty() {
        rendered.push_str(&format!("\n\n{heading}\n{value}\n"));
    }
}

/// The reviewer's findings JSON: `$SIRIUS_REVIEW_OUT` if it wrote one (the
/// file wins; prose or fences around it are tolerated), else — only for a
/// reviewer that EXITED CLEANLY — its final message from the captured log.
/// A failed reviewer's log may hold an echoed template or quoted findings;
/// reading those as its answer would fake a review. Shared by review rounds
/// and canaries, so a canary scores the reviewer exactly as a round reads it.
pub fn read_review_output(
    out_path: &std::path::Path,
    log_path: Option<&std::path::Path>,
    exited_ok: bool,
    round: u32,
) -> Option<String> {
    let from_file = std::fs::read_to_string(out_path).ok().map(|r| {
        if parse_review(&r, round).is_ok() {
            r
        } else {
            findings_json_in(&r).unwrap_or(r)
        }
    });
    from_file.or_else(|| {
        if !exited_ok {
            return None;
        }
        log_path
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|log| findings_json_in(&log))
    })
}

/// FNV-1a 64 — a dependency-free fingerprint for recognizing shipped prompts.
pub fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Built-in prompts that earlier releases WROTE into `.sirius/review-prompt.md`
/// (v0.1.2's `init`/`run` materialized the default). A file with exactly such
/// content is not an operator override — it is a stale copy, so the CURRENT
/// built-in is used instead. Only ever append to this list.
pub const SHIPPED_PROMPT_FNV: &[u64] = &[
    0x5f9f_6624_84de_9fc1, // v0.1.2
];

/// Is this prompt-file content a stale copy of a shipped default?
pub fn is_stale_shipped_prompt(content: &str) -> bool {
    SHIPPED_PROMPT_FNV.contains(&fnv1a(content))
}

/// Findings Sirius computes itself (sibling conflicts, sequence collisions —
/// SIRF-30/31) carry `AUTO-` ids: facts recomputed every round, which no
/// reviewer verdict or worker rebuttal can open or close.
pub fn is_auto(id: &str) -> bool {
    id.starts_with("AUTO-")
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
        // An AUTO fact is never closed BY a rebuttal (it is recomputed, not
        // judged): a vanished one counts as fixed, an open one as no verdict.
        let rebutted = !is_auto(&p.id) && p.response.as_ref().is_some_and(FixResponse::is_rebuttal);
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
            ("SIRIUS_SIBLINGS".into(), "(none)".into()),
            ("SIRIUS_ESCAPES".into(), "(none)".into()),
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
    fn a_vanished_auto_fact_counts_as_fixed_never_as_an_accepted_rebuttal() {
        let mut prev = vec![bug("AUTO-seq-1", "conflict", "confirmed")];
        attach_responses(
            &mut prev,
            &FixReport {
                responses: vec![FixResponse {
                    id: "AUTO-seq-1".into(),
                    status: "rebutted".into(),
                    note: "not a bug".into(),
                }],
            },
        );
        let rec = reconcile(&prev, &ReviewReport::default(), &block());
        assert_eq!((rec.fixed, rec.rebuttals_accepted), (1, 0));
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
    fn an_echoed_template_or_a_trailing_object_is_never_the_review() {
        // The prompt (with its example JSON) echoed into the log, then the
        // reviewer failed: there is NO final answer, so no review.
        let rendered = render_prompt(DEFAULT_PROMPT, &[("SIRIUS_ROUND".into(), "1".into())]);
        let log = format!("{rendered}\nError: rate limited\n[sirius] agent exit: 1\n");
        assert!(findings_json_in(&log).is_none(), "template must not count");
        // A real answer followed by more output is not the ENDING either.
        assert!(findings_json_in("{\"findings\":[]}\nand then I kept talking").is_none());
        // A malformed final answer does not let an earlier object win.
        let log2 = "{\"findings\":[]}\nfinal: {\"findings\":[1,],}\n";
        assert!(findings_json_in(log2).is_none());
        // And the template itself is rejected as malformed if it ever arrives.
        let t = r#"{"findings":[{"kind":"bug|conflict|minor|design","confidence":"confirmed|uncertain"}]}"#;
        assert!(parse_review(t, 1).unwrap_err().contains("template"));
    }

    #[test]
    fn stale_shipped_prompt_copies_are_recognized() {
        assert!(
            !is_stale_shipped_prompt(DEFAULT_PROMPT),
            "the CURRENT default is not stale"
        );
        assert!(!is_stale_shipped_prompt("my own reviewer prompt"));
        // The hash list is non-empty and the hash function is stable.
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert!(!SHIPPED_PROMPT_FNV.is_empty());
    }

    #[test]
    fn findings_json_is_recovered_from_a_final_message() {
        let log = "Reviewing the diff...\nI found one issue.\n\n```json\n{\"findings\":[{\"id\":\"R1-1\",\"kind\":\"bug\",\"confidence\":\"confirmed\",\"summary\":\"x\"}],\"checked\":[\"a\"]}\n```\n\n[sirius] agent exit: 0\n";
        let raw = findings_json_in(log).unwrap();
        let r = parse_review(&raw, 1).unwrap();
        assert_eq!(r.findings.len(), 1);
        // The LAST findings object wins (an earlier draft is superseded).
        let two = format!("{{\"findings\":[]}} then later {}", log);
        assert_eq!(
            parse_review(&findings_json_in(&two).unwrap(), 1)
                .unwrap()
                .findings
                .len(),
            1
        );
        // No findings object at all ⇒ None (a review error, not "clean").
        assert!(findings_json_in("all good! {\"ok\":true}").is_none());
    }

    #[test]
    fn parse_fix_is_lenient() {
        assert!(parse_fix(None).responses.is_empty());
        assert!(parse_fix(Some("garbage")).responses.is_empty());
        let f = parse_fix(Some(r#"{"responses":[{"id":"R1-1","status":"fixed"}]}"#));
        assert_eq!(f.responses.len(), 1);
    }
}
