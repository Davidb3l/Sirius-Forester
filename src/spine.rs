//! The suite event spine — SUITE_CONTRACTS §2.
//!
//! A per-repo, append-only, file-based notification log at
//! `<repo-root>/.suite/events/<UTC-date>.jsonl`. Sirius is one of four tools
//! (amt, hayven, sirius, catryna) that append here; the spine is never a source
//! of truth (the ledger is), only a way to tell the others what happened.
//!
//! Two guarantees make it lock-free and multi-writer:
//!   1. one whole line per event, appended with a single `O_APPEND` write;
//!   2. every line < 4 KiB, so the append is atomic (< PIPE_BUF) and concurrent
//!      workers never interleave.
//!
//! Emission is **best-effort**: any failure (no disk, no permission, an
//! oversized payload) is logged to stderr and swallowed. The spine must never
//! fail the operation whose fact it is reporting.

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Max bytes for one line INCLUDING its trailing `\n`. `PIPE_BUF` on Linux and
/// macOS; below it, a single append write is atomic.
const MAX_LINE_BYTES: usize = 4096;

/// An emitter bound to one repo's `.suite/events/` directory.
pub struct Spine {
    dir: PathBuf,
}

impl Spine {
    /// Bind to `<root>/.suite/events`. Creates nothing yet; the directory is
    /// made on the first successful emit ("created by whichever tool writes
    /// first").
    pub fn new(root: &Path) -> Spine {
        Spine {
            dir: root.join(".suite").join("events"),
        }
    }

    /// Emit one `sirius` event. Best-effort — never panics, never propagates.
    pub fn emit(&self, event_type: &str, refs: Vec<String>, data: Value) {
        if let Err(e) = self.try_emit(event_type, refs, data) {
            eprintln!("sirius: spine append failed ({event_type}): {e}");
        }
    }

    fn try_emit(&self, event_type: &str, refs: Vec<String>, data: Value) -> std::io::Result<()> {
        let ts = crate::ledger::now_iso8601(); // "YYYY-MM-DDTHH:MM:SS.mmmZ"
        let date = &ts[..10]; // "YYYY-MM-DD" (ASCII prefix, byte-safe)

        let line = build_line(&ts, event_type, refs, data);

        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{date}.jsonl"));
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        // One append write of a <4 KiB buffer = one atomic write(2).
        f.write_all(line.as_bytes())?;
        Ok(())
    }
}

/// Serialize one envelope line (including the trailing `\n`). If the full line
/// would breach the 4 KiB atomicity bound, shrink it in two stages so we still
/// record that the event happened without risking a torn/interleaved write:
///
///   1. Drop only `data` (bulky detail belongs in our own store anyway) but
///      KEEP `refs` — §2's guidance is that the URIs stay in refs precisely so
///      a truncated `receipt.filed` still says which issue/receipt it was
///      about. Refs are short suite URIs, so this almost always fits.
///   2. If the refs themselves are somehow enormous, drop them too; the bound
///      is a hard atomicity guarantee and must hold unconditionally.
fn build_line(ts: &str, event_type: &str, refs: Vec<String>, data: Value) -> String {
    let envelope = |refs: Value, data: Value| {
        json!({
            "v": 1,
            "id": new_uuid_v4(),
            "ts": ts,
            "source": "sirius",
            "type": event_type,
            "refs": refs,
            "data": data,
        })
    };
    // Conformance (§2.1): the whole line INCLUDING the `\n` must be STRICTLY
    // < 4096 bytes. `line` here excludes the newline, so the total is
    // `line.len() + 1`; truncate once that reaches the bound (`>=`, not `>`),
    // matching validate.ts exactly.
    let refs = json!(refs);
    let mut line = serde_json::to_string(&envelope(refs.clone(), data)).unwrap_or_default();
    if line.len() + 1 >= MAX_LINE_BYTES {
        line = serde_json::to_string(&envelope(refs, json!({ "truncated": true })))
            .unwrap_or_default();
    }
    if line.len() + 1 >= MAX_LINE_BYTES {
        line = serde_json::to_string(&envelope(json!([]), json!({ "truncated": true })))
            .unwrap_or_default();
    }
    line.push('\n');
    line
}

// ── suite-URI ref helpers (SUITE_CONTRACTS §1) ───────────────────────────────

/// `amt:issue/<key>` — the issue key (e.g. `AMT-7`) is a valid id under
/// Ametrite's own scheme grammar; consumers treat foreign URIs opaquely.
pub fn issue_ref(issue: &str) -> String {
    format!("amt:issue/{issue}")
}

/// `sirius:worker/<name>`, e.g. `sirius:worker/sirius/oak`.
pub fn worker_ref(worker: &str) -> String {
    format!("sirius:worker/{worker}")
}

/// `sirius:receipt/<rowid>`.
pub fn receipt_ref(id: i64) -> String {
    format!("sirius:receipt/{id}")
}

// ── id generation ────────────────────────────────────────────────────────────

/// A UUIDv4-shaped id. Not cryptographically random — its only job (§2) is
/// per-event uniqueness so a consumer can dedup on replay. We seed splitmix64
/// from wall-clock nanos, the process id, and a per-process atomic counter, so
/// two events never collide within a process (counter) or across concurrent
/// processes on one repo (pid + nanos).
fn new_uuid_v4() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seed = nanos
        ^ (std::process::id() as u64).rotate_left(32)
        ^ CTR.fetch_add(1, Ordering::Relaxed).rotate_left(17);
    let hi = splitmix64(seed);
    let lo = splitmix64(seed ^ 0x9E37_79B9_7F4A_7C15);

    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&hi.to_be_bytes());
    b[8..].copy_from_slice(&lo.to_be_bytes());
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    static UUID_V4: &str = r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$";

    fn read_only_line(dir: &Path) -> Value {
        let mut files: Vec<_> = fs::read_dir(dir.join(".suite").join("events"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        let body = fs::read_to_string(&files[0]).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1, "expected exactly one line");
        assert!(body.ends_with('\n'), "line must end in newline");
        serde_json::from_str(lines[0]).unwrap()
    }

    #[test]
    fn envelope_has_the_contract_shape() {
        let dir = std::env::temp_dir().join(format!("spine-shape-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let spine = Spine::new(&dir);
        spine.emit(
            "receipt.filed",
            vec![receipt_ref(42), issue_ref("AMT-7")],
            json!({"issue": "AMT-7", "symbols": ["auth::verify", "auth::mint"]}),
        );
        let ev = read_only_line(&dir);
        assert_eq!(ev["v"], 1);
        assert_eq!(ev["source"], "sirius");
        assert_eq!(ev["type"], "receipt.filed");
        assert_eq!(ev["data"]["issue"], "AMT-7");
        assert_eq!(ev["refs"][0], "sirius:receipt/42");
        assert!(ev["ts"].as_str().unwrap().ends_with('Z'));
        let re = regex::Regex::new(UUID_V4).unwrap();
        assert!(
            re.is_match(ev["id"].as_str().unwrap()),
            "id must be a UUIDv4"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_are_unique_across_many_emits() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..5000 {
            assert!(seen.insert(new_uuid_v4()), "uuid collision");
        }
    }

    #[test]
    fn appends_do_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("spine-append-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let spine = Spine::new(&dir);
        spine.emit(
            "job.dispatched",
            vec![],
            json!({"issue": "A-1", "worker": "sirius/oak"}),
        );
        spine.emit(
            "job.completed",
            vec![],
            json!({"issue": "A-1", "worker": "sirius/oak"}),
        );
        let body = fs::read_to_string(
            fs::read_dir(dir.join(".suite").join("events"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert_eq!(body.lines().count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_payload_is_truncated_below_the_atomicity_bound() {
        let dir = std::env::temp_dir().join(format!("spine-big-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let spine = Spine::new(&dir);
        let huge: Vec<String> = (0..2000).map(|i| format!("symbol::number_{i}")).collect();
        spine.emit(
            "receipt.filed",
            vec!["amt:issue/A-1".into(), "sirius:receipt/9".into()],
            json!({"issue": "A-1", "symbols": huge}),
        );
        let ev = read_only_line(&dir);
        assert_eq!(ev["data"]["truncated"], true);
        // Stage-1 truncation drops only `data` — the refs SURVIVE, so a
        // truncated receipt.filed still says which issue/receipt it was about
        // (§2: bulky detail goes in your own store; the URIs stay in refs).
        assert_eq!(ev["refs"], json!(["amt:issue/A-1", "sirius:receipt/9"]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pathologically_large_refs_are_dropped_as_the_last_resort() {
        // Stage 2: the 4 KiB bound is a hard atomicity guarantee. If the refs
        // themselves would breach it, they too are dropped rather than ever
        // emitting a line that could tear.
        let huge_refs: Vec<String> = (0..300).map(|i| format!("hayven:node/{i:0>32}")).collect();
        let line = build_line(
            "2026-07-11T00:00:00.000Z",
            "receipt.filed",
            huge_refs,
            json!({"issue": "A-1"}),
        );
        assert!(line.len() < MAX_LINE_BYTES);
        let ev: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(ev["data"]["truncated"], true);
        assert_eq!(ev["refs"], json!([]));
    }

    #[test]
    fn every_line_stays_strictly_under_the_atomicity_bound() {
        // §2.1: the line INCLUDING its newline must be < 4096, not <=. Sweep a
        // range of payload widths so we hit the exact boundary the guard
        // protects (a subset just below the cutoff, where an off-by-one would
        // let a 4096-byte line escape truncation).
        for n in 60..200usize {
            let syms: Vec<String> = (0..n).map(|i| format!("symbol::number_{i:05}")).collect();
            let line = build_line(
                "2026-07-11T00:00:00.000Z",
                "receipt.filed",
                vec![],
                json!({ "symbols": syms }),
            );
            assert!(
                line.len() < MAX_LINE_BYTES,
                "n={n}: line is {} bytes, must be < {}",
                line.len(),
                MAX_LINE_BYTES
            );
            // Whatever survives untruncated must itself be valid JSON + newline.
            assert!(line.ends_with('\n'));
            let _: Value = serde_json::from_str(line.trim_end()).unwrap();
        }
    }
}
