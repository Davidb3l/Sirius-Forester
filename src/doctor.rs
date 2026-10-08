//! `sirius doctor` — checks the five PRD §6 contract facts live (M0), plus a
//! sixth workspace fact and two ADVISORY checks on the Claude Code plugin half
//! of the install.
//!
//! 1. amt present + schema (read the ametrite `meta.schema_version` read-only;
//!    pragmatic ≥ 3, NOT a version-string compare — `amt 0.1.0` ships schema 3).
//! 2. hayven daemon on :7777 — the HTTP probe AND an affirmatively-running
//!    `hayven daemon status` AND a verified repo-local `.hayven/` must all
//!    agree; port-answers-but-status-stopped is flagged as an orphan daemon
//!    (with the actual listener pids via lsof, best-effort).
//! 3. claim exit-code semantics (amt claim JSON shape is parseable; hayven claim
//!    surface present).
//! 4. gate exit codes (hayven affected-tests present).
//! 5. fleet-memory write path (hayven remember/recall present).
//! 6. gate configuration (SF-11/SF-15): can THIS workspace run tests at all?
//!    Fact 4 is about the affected-tests BINARY; it says nothing about whether
//!    `gate.test_cmd` is set here, so a workspace whose gate is inert used to
//!    get "all contract facts hold" — and a fleet started against it burned
//!    workers doing real edits while every gate refused to advance.
//! 7. plugin handoff (ADVISORY, never gates `ok`): is the Sothis bundle
//!    marketplace added and are the sirius/hayvenhurst/catryna plugins
//!    installed in Claude Code? This exists because the CLI half and the
//!    plugin half install separately, and a real audit found machines with
//!    every BINARY present but the `sirius` plugin never installed — the
//!    printed `/plugin` handoff is a silent drop-off unless something checks.
//! 8. plugin/CLI version skew (ADVISORY, SF-10): report the plugin manifest
//!    version beside the CLI's, so a plugin fix that shipped without a version
//!    bump is visible instead of silently cached forever.

use crate::amt::Amt;
use crate::config::{test_cmd_suggestion, Config};
use crate::hayven::Hayven;
use crate::shell::Runner;
use crate::workspace::Workspace;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub pass: bool,
    pub detail: String,
    /// A gating check flips the report's `ok` when it fails. An advisory
    /// (non-gating) check reports and recommends but never fails doctor —
    /// sirius is fully functional without the Claude Code plugin layer
    /// (CI boxes, plain-terminal users), so incompleteness there is a WARN.
    pub gating: bool,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            pass: true,
            detail: detail.into(),
            gating: true,
        }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            pass: false,
            detail: detail.into(),
            gating: true,
        }
    }
    fn advisory(name: &str, pass: bool, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            pass,
            detail: detail.into(),
            gating: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<Check>,
}

/// Minimum ametrite schema version Sirius depends on (PRD "schema >= v3").
pub const MIN_AMETRITE_SCHEMA: i64 = 3;

/// Read the ametrite schema version from its `meta` table, read-only. Sirius
/// never writes the parent DB; here it only reads (§2.2 allows read-only).
pub fn ametrite_schema_version(ws: &Workspace) -> Result<i64, String> {
    let db = ws
        .ametrite_db
        .as_ref()
        .ok_or_else(|| "no .ametrite/ametrite.db found (run `amt init`)".to_string())?;
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("cannot open ametrite db read-only: {e}"))?;
    let v: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("ametrite meta.schema_version unreadable: {e}"))?;
    v.trim()
        .parse::<i64>()
        .map_err(|e| format!("ametrite schema_version not an integer: {e}"))
}

/// The pids actually LISTENING on :7777, via `lsof` (best-effort: empty on
/// any failure — lsof missing, no permission). Used only to enrich the
/// orphan-daemon failure detail; never gates on its own. `-t` prints one pid
/// per line; distinct pids preserved in order.
fn listener_pids(runner: &dyn Runner) -> Vec<String> {
    match runner.run("lsof", &["-nP", "-iTCP:7777", "-sTCP:LISTEN", "-t"]) {
        Ok(o) => {
            let mut pids: Vec<String> = Vec::new();
            for line in o.stdout.lines() {
                let p = line.trim();
                if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
                    let p = p.to_string();
                    if !pids.contains(&p) {
                        pids.push(p);
                    }
                }
            }
            pids
        }
        Err(_) => Vec::new(),
    }
}

/// Probe the hayven daemon health on :7777 via a plain HTTP GET (no deps —
/// we shell `curl`, present on macOS/Linux; a failure is reported, not fatal).
fn daemon_http_ok(runner: &dyn Runner) -> bool {
    match runner.run(
        "curl",
        &[
            "-s",
            "-o",
            "/dev/null",
            "-m",
            "3",
            "-w",
            "%{http_code}",
            "http://localhost:7777/",
        ],
    ) {
        Ok(o) => o.stdout.trim() == "200",
        Err(_) => false,
    }
}

/// Ask the RUNNING daemon what it is, via its own `/api/health` (verified live
/// against hayven 0.0.7: `{"ok":true,"version":"0.0.7","pid":…,
/// "native_version":"present","root":…,"projects":[…]}`).
///
/// SF-13: `hayven --version` reports the CLI on disk; `hayven daemon status`
/// reports that *a* process is alive. Neither asks the daemon what BUILD it is,
/// and a daemon left running across an upgrade answers /health perfectly while
/// failing the one operation the whole loop depends on ("hayven-native
/// serialize encode failed (undefined)" on every entity claim, 2026-08-26).
/// This is the field that closes that hole.
fn daemon_health(runner: &dyn Runner) -> Option<serde_json::Value> {
    // Same `curl` seam as `daemon_http_ok` — if curl could fetch `/` for the
    // liveness probe it can fetch this. 127.0.0.1, not `localhost`: a machine
    // whose `localhost` resolves to ::1 first would otherwise probe a
    // different socket than the daemon bound.
    let out = runner
        .run(
            "curl",
            &["-s", "-m", "3", "http://127.0.0.1:7777/api/health"],
        )
        .ok()?;
    serde_json::from_str(&out.stdout).ok()
}

/// Compare two hayven version strings tolerantly: `hayven 0.0.7`, `v0.0.7`, and
/// `0.0.7` are the same build. Only the CLI's spelling has ever varied, but a
/// cosmetic reword upstream must not manufacture a false skew alarm.
fn same_build(a: &str, b: &str) -> bool {
    normalize_version(a) == normalize_version(b)
}

fn normalize_version(v: &str) -> String {
    v.trim()
        .trim_start_matches("hayven")
        .trim()
        .trim_start_matches('v')
        .trim()
        .to_string()
}

/// Check #6 — can THIS workspace actually run tests? (SF-11)
///
/// Pure over the workspace root and the loaded gate config, so it is testable
/// against a temp fixture. GATING on purpose: an unset `test_cmd` means every
/// gate in this repo fails closed forever, which is precisely the "doctor says
/// healthy while nothing can advance" state the ticket is about. The fix
/// direction is to report the truth — never to relax the gate.
/// `shell` is passed in rather than resolved here so the fact stays pure — and
/// so doctor can TELL the operator which interpreter their `test_cmd` will
/// actually reach. That line is not decoration: SF-15 was a gate that passed
/// from Git Bash and failed from PowerShell on the same commit, and nothing in
/// the tool ever said which shell it had picked.
pub fn gate_configured_check(
    root: &Path,
    cfg: &Result<Config, String>,
    shell: &crate::shell::ShellCmd,
) -> Check {
    const NAME: &str = "gate_configured";
    let cfg = match cfg {
        Ok(c) => c,
        // A config we cannot read is not a configured gate. Fail-closed and say
        // exactly which file is the problem.
        Err(e) => return Check::fail(NAME, format!("cannot read gate config: {e}")),
    };
    match cfg.gate.test_cmd.as_deref().map(str::trim) {
        Some(cmd) if !cmd.is_empty() => Check::ok(
            NAME,
            format!(
                "gate.test_cmd = `{cmd}` (fallback: {:?}; runs via `{}`)",
                cfg.gate.fallback,
                shell.describe()
            ),
        ),
        // A whitespace-only command is worse than none: it reaches the shell,
        // runs nothing, and exits 0 — a gate that passes everything.
        Some(_) => Check::fail(
            NAME,
            format!(
                "gate.test_cmd is set to an empty/whitespace string — it would run nothing \
                 and pass everything; {}",
                test_cmd_suggestion(root)
            ),
        ),
        None => Check::fail(
            NAME,
            format!(
                "gate.test_cmd is not set — EVERY gate in this workspace fails closed \
                 (exit 3, plan `unconfigured`, 0 tests), so no issue can advance and a \
                 fleet started here would burn a full agent run per issue for nothing; {}",
                test_cmd_suggestion(root)
            ),
        ),
    }
}

/// Check #8 — plugin/CLI version skew (ADVISORY, SF-10). Pure over the plugin
/// root so tests drive it with a fixture instead of mutating the environment.
///
/// ADVISORY and always a PASS: `sirius` is a CLI first, and most invocations
/// (CI, a plain terminal, a service) have no plugin around them at all. Failing
/// doctor because a plugin manifest is not present would break exactly the
/// environments that never wanted one. The job here is visibility: a plugin fix
/// that shipped WITHOUT a version bump leaves installed users on a stale cached
/// copy forever, and nothing in the system says so out loud.
pub fn plugin_version_check(plugin_root: Option<&Path>, cli_version: &str) -> Check {
    const NAME: &str = "plugin_version";
    let Some(root) = plugin_root else {
        return Check::advisory(
            NAME,
            true,
            format!(
                "sirius CLI {cli_version}; not running inside a plugin (CLAUDE_PLUGIN_ROOT unset)"
            ),
        );
    };
    let manifest = root.join(".claude-plugin").join("plugin.json");
    match read_json(&manifest).and_then(|v| {
        v.get("version")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
    }) {
        Some(pv) => Check::advisory(
            NAME,
            true,
            format!(
                "sirius CLI {cli_version}, plugin {pv} (from {}) — these versions move \
                 independently; if a plugin fix is missing here, its manifest version was \
                 not bumped and Claude Code is serving a cached copy",
                manifest.display()
            ),
        ),
        None => Check::advisory(
            NAME,
            true,
            format!(
                "sirius CLI {cli_version}; CLAUDE_PLUGIN_ROOT is set to {} but no readable \
                 version in .claude-plugin/plugin.json — plugin skew cannot be seen from here",
                root.display()
            ),
        ),
    }
}

/// The Claude Code plugin directory for this user, if resolvable.
fn default_plugins_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude").join("plugins"))
}

/// Read a JSON file into a Value; None if absent or unparseable.
fn read_json(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Check #6 — the plugin handoff (ADVISORY). Pure over `plugins_dir` so tests
/// drive it with fixture directories, no env mutation.
///
/// Detection rules (the gotcha that produces false negatives if ignored):
/// the same plugin can be installed from DIFFERENT marketplaces — on a real
/// machine Hayvenhurst is `hayvenhurst@hayvenhurst`, via the bundle it would be
/// `hayvenhurst@sirius-forester`. So plugins match on the `<name>@` KEY PREFIX,
/// never one full key. Only the bundle MARKETPLACE is matched by its exact
/// name, because that is the thing only this repo provides.
pub fn plugin_handoff_check(plugins_dir: Option<&Path>) -> Check {
    const NAME: &str = "plugin_handoff";
    let dir = match plugins_dir {
        // No resolvable plugin dir at all: not a Claude Code environment —
        // nothing to hand off to. Advisory pass, clearly labeled as skipped.
        Some(d) if d.is_dir() => d,
        _ => {
            return Check::advisory(
                NAME,
                true,
                "no Claude Code plugin dir (~/.claude/plugins) — not a Claude Code \
                 environment, check skipped",
            );
        }
    };

    // Marketplace: known_marketplaces.json is an object keyed by marketplace
    // name. Absent/unparseable counts as "not added" — that IS the cold state.
    let marketplace_ok = read_json(&dir.join("known_marketplaces.json"))
        .and_then(|v| v.as_object().map(|o| o.contains_key("sirius-forester")))
        .unwrap_or(false);

    // Plugins: installed_plugins.json v2 is {version, plugins: {"name@mkt": …}}.
    // PREFIX match per the rule above; any marketplace satisfies a plugin.
    let plugin_keys: Vec<String> = read_json(&dir.join("installed_plugins.json"))
        .and_then(|v| {
            v.get("plugins")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect())
        })
        .unwrap_or_default();
    let has_plugin = |name: &str| {
        let prefix = format!("{name}@");
        plugin_keys.iter().any(|k| k.starts_with(&prefix))
    };

    // The fix commands are the NON-interactive `claude plugin` CLI (v2.1.195+),
    // which runs from any plain shell — unlike the interactive `/plugin`
    // dialog, which desktop-app users don't have at all. App users can also
    // use the plugin browser (the + next to the prompt box → Plugins).
    let mut fixes: Vec<String> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    if !marketplace_ok {
        missing.push("sirius-forester marketplace");
        fixes.push("claude plugin marketplace add Davidb3l/Sirius-Forester".into());
    }
    for name in ["sirius", "hayvenhurst", "catryna"] {
        if !has_plugin(name) {
            missing.push(match name {
                "sirius" => "sirius plugin",
                "hayvenhurst" => "hayvenhurst plugin",
                _ => "catryna plugin",
            });
            fixes.push(format!("claude plugin install {name}@sirius-forester"));
        }
    }

    if missing.is_empty() {
        Check::advisory(
            NAME,
            true,
            "bundle marketplace added; sirius/hayvenhurst/catryna plugins installed",
        )
    } else {
        Check::advisory(
            NAME,
            false,
            format!(
                "CLIs alone are half the install — missing: {}. Fix from any shell: {} \
                 (or in the Claude desktop app: + → Plugins → Add plugin)",
                missing.join(", "),
                fixes.join("  then  ")
            ),
        )
    }
}

/// Run all checks. `runner` is the shell seam so this is testable offline.
pub fn run(ws: &Workspace, runner: &dyn Runner) -> DoctorReport {
    run_with_plugins_dir(ws, runner, default_plugins_dir())
}

/// Like `run`, with the Claude Code plugins dir injected (test seam).
pub fn run_with_plugins_dir(
    ws: &Workspace,
    runner: &dyn Runner,
    plugins_dir: Option<PathBuf>,
) -> DoctorReport {
    let amt = Amt::new(runner);
    let hv = Hayven::new(runner);
    let mut checks = Vec::new();

    // 1. amt present + schema.
    match amt.version() {
        Ok(ver) => match ametrite_schema_version(ws) {
            Ok(v) if v >= MIN_AMETRITE_SCHEMA => checks.push(Check::ok(
                "amt_present_and_schema",
                format!("{ver}, ametrite schema v{v} (>= v{MIN_AMETRITE_SCHEMA})"),
            )),
            Ok(v) => checks.push(Check::fail(
                "amt_present_and_schema",
                format!("{ver} but ametrite schema v{v} < v{MIN_AMETRITE_SCHEMA}"),
            )),
            Err(e) => checks.push(Check::fail("amt_present_and_schema", format!("{ver}; {e}"))),
        },
        Err(e) => checks.push(Check::fail(
            "amt_present_and_schema",
            format!("amt not runnable: {e}"),
        )),
    }

    // 2. hayven daemon on :7777.
    let http = daemon_http_ok(runner);
    let status = hv.daemon_status().unwrap_or_default();
    let hv_ver = hv.version().unwrap_or_else(|_| "unknown".into());
    // VERIFY the directory exists right now — never infer presence from
    // discovery alone (2026-08-05 defect: ".hayven/ present" was reported for
    // a directory this repo did not have).
    let hv_dir_ok = ws.hayven_dir.as_deref().is_some_and(Path::is_dir);
    let hv_ws = if hv_dir_ok {
        " .hayven/ present"
    } else {
        " .hayven/ not found (run `hayven init`)"
    };
    if http {
        let status_line = first_line(&status);
        // AFFIRMATIVE health only: the status line must actually say the
        // tracked daemon is running. "Not an error" is not health — a 200
        // paired with `status: stopped` means SOMETHING answers the port that
        // the pidfile does not know about (an orphan daemon), and the old
        // check blessed exactly that broken state as "healthy (status:
        // stopped)" (observed 2026-08-05, two daemons, one orphaned).
        // starts_with, not contains: hayven's status vocabulary today is
        // "running (pid N)" / "stale pidfile (…)" / "stopped" (verified in
        // daemon.ts statusDaemon), but a future upstream reword to
        // "not running" would turn a contains() check back into the original
        // healthy-while-stopped bug. Prefix-matching is wording-proof.
        let running = status_line.to_ascii_lowercase().starts_with("running");
        // Branch ORDER matters: a missing repo-local .hayven/ is diagnosed
        // FIRST — in that state `hayven daemon status` errors too, and the
        // orphan branch below would misread it as "kill the (legitimate)
        // listener". The local, actionable cause wins over the exotic one.
        if !hv_dir_ok {
            // SIRF-10: a 200 on :7777 means *a* daemon is up, not that it
            // serves THIS workspace. This is the one silent-degradation state
            // CONTRACTS documents: forward stamps (amt) still land but reverse
            // stamps (hayven remember) quietly go one-way (reverse_ok:false).
            // So a daemon that cannot be serving this repo must FAIL, not pass.
            checks.push(Check::fail(
                "hayven_daemon_7777",
                format!(
                    "hayven {hv_ver}, daemon up on :7777 but not serving this workspace (status: {status_line});{hv_ws} — run `hayven init` then `hayven daemon start` in this repo"
                ),
            ));
        } else if running {
            // SF-13: LIVENESS IS NOT HEALTH. Everything above this point proves
            // a process is up and answering; none of it proves the process is
            // running the build the CLI just installed. On 2026-08-26 doctor
            // printed "daemon healthy on :7777 (status: running (pid 25808))"
            // and every single entity claim immediately failed with
            // "hayven-native serialize encode failed (undefined)" — the daemon
            // predated the hayven 0.0.7 CLI beside it, and a stop/start fixed
            // it instantly. So: ASK THE DAEMON what it is, and refuse to bless
            // a build we cannot read. Fail-closed, because the failure mode is
            // silent and total (nothing in the loop works, everything looks OK).
            const REMEDY: &str = "run `hayven daemon stop && hayven daemon start`";
            let health = daemon_health(runner);
            let daemon_ver = health
                .as_ref()
                .and_then(|v| v.get("version"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // Reported verbatim: today hayven answers `"present"` here, not a
            // version (see the module-level gap note in this check's ticket) —
            // showing it keeps the limitation in front of whoever reads doctor.
            let native = health
                .as_ref()
                .and_then(|v| v.get("native_version"))
                .and_then(|v| v.as_str())
                .unwrap_or("unreported");
            match daemon_ver {
                _ if hv_ver == "unknown" => checks.push(Check::fail(
                    "hayven_daemon_7777",
                    format!(
                        "a daemon is running on :7777 but `hayven --version` is unreadable, so its build cannot be verified against the CLI (status: {status_line});{hv_ws} — reinstall the hayven CLI, then {REMEDY}"
                    ),
                )),
                Some(dv) if same_build(&dv, &hv_ver) => checks.push(Check::ok(
                    "hayven_daemon_7777",
                    format!(
                        "hayven {hv_ver}, daemon healthy on :7777 running the SAME build {dv} (native: {native}, status: {status_line});{hv_ws}"
                    ),
                )),
                Some(dv) => checks.push(Check::fail(
                    "hayven_daemon_7777",
                    format!(
                        "STALE DAEMON: the hayven CLI is {hv_ver} but the daemon on :7777 is build {dv}. It is live and answers /health, and it will fail the ONE operation the loop depends on — every entity claim returns 500 'hayven-native serialize encode failed'. Fix: {REMEDY} (status: {status_line});{hv_ws}"
                    ),
                )),
                None => checks.push(Check::fail(
                    "hayven_daemon_7777",
                    format!(
                        "hayven {hv_ver}: a daemon answers :7777 but would not report its build at http://127.0.0.1:7777/api/health, so it cannot be shown to match the installed CLI — an upgrade-straddling daemon looks exactly like this. Fix: {REMEDY} (status: {status_line});{hv_ws}"
                    ),
                )),
            }
        } else {
            // Workspace is set up, port answers, THIS repo's pidfile disagrees.
            // Two states look identical from here: an orphan daemon nothing
            // tracks, or a healthy daemon started from ANOTHER repo (0.0.6+ is
            // multi-project; this repo just never claimed the pidfile). The
            // remedy must try the non-destructive one first: `hayven daemon
            // start` attaches to a healthy daemon and claims the pidfile —
            // advising a kill first would take down a daemon serving other
            // repos. Name the listener(s) so the last-resort kill is precise.
            let pids = listener_pids(runner);
            let who = match pids.len() {
                0 => String::new(),
                1 => format!(" (listener pid: {})", pids[0]),
                _ => format!(" (MULTIPLE listeners: pids {})", pids.join(", ")),
            };
            checks.push(Check::fail(
                "hayven_daemon_7777",
                format!(
                    "a daemon ANSWERS on :7777 but `hayven daemon status` reports '{status_line}' — either an orphan daemon or one started from another repo{who};{hv_ws} — run `hayven daemon start` in this repo first (it attaches and fixes the pidfile); only if status STILL disagrees, kill the listener pid"
                ),
            ));
        }
    } else {
        checks.push(Check::fail(
            "hayven_daemon_7777",
            format!(
                "no 200 from http://localhost:7777 (status: {});{hv_ws}",
                first_line(&status)
            ),
        ));
    }

    // 3. claim exit-code semantics — verify amt claim --peek returns parseable
    //    JSON (does not take a lease) and the hayven claim surface exists.
    match runner.run(
        "amt",
        &["--json", "claim", "--peek", "--agent", "sirius/doctor"],
    ) {
        Ok(o) if serde_json::from_str::<serde_json::Value>(&o.stdout).is_ok() => {
            checks.push(Check::ok(
                "claim_exit_codes",
                "amt claim JSON shape parseable; hayven claim: 0/1/3",
            ))
        }
        Ok(o) => checks.push(Check::fail(
            "claim_exit_codes",
            format!("amt claim --peek non-JSON: {}", first_line(&o.stdout)),
        )),
        Err(e) => checks.push(Check::fail(
            "claim_exit_codes",
            format!("amt claim --peek failed: {e}"),
        )),
    }

    // Fetch hayven's command surface once and reuse for checks 4 and 5.
    let hayven_help = runner.run("hayven", &["--help"]);

    // 4. gate exit codes — hayven affected-tests present (its --help mentions it).
    match &hayven_help {
        Ok(o) if o.stdout.contains("affected-tests") => checks.push(Check::ok(
            "gate_exit_codes",
            "hayven affected-tests present (exit 0 pass / non-0 fail)",
        )),
        Ok(_) => checks.push(Check::fail(
            "gate_exit_codes",
            "hayven affected-tests not found in help",
        )),
        Err(e) => checks.push(Check::fail(
            "gate_exit_codes",
            format!("hayven not runnable: {e}"),
        )),
    }

    // 5. fleet-memory write path — hayven remember/recall present.
    match &hayven_help {
        Ok(o) if o.stdout.contains("remember") && o.stdout.contains("recall") => checks.push(
            Check::ok("fleet_memory_write_path", "hayven remember/recall present"),
        ),
        Ok(_) => checks.push(Check::fail(
            "fleet_memory_write_path",
            "remember/recall not found in help",
        )),
        Err(e) => checks.push(Check::fail(
            "fleet_memory_write_path",
            format!("hayven not runnable: {e}"),
        )),
    }

    // 6. gate configuration — GATING (SF-11). Check 4 above proves the
    //     affected-tests SELECTOR exists; it says nothing about whether this
    //     workspace can RUN anything. Loaded here rather than passed in so the
    //     fact needs no new plumbing through the caller.
    checks.push(gate_configured_check(
        &ws.root,
        &Config::load(&ws.config_path()),
        &crate::shell::resolve_shell(),
    ));

    // 7. plugin handoff — advisory; reports and recommends, never gates.
    checks.push(plugin_handoff_check(plugins_dir.as_deref()));

    // 8. fleet models (SIRF-26) — advisory: what the fleet would run on.
    checks.push(models_check(ws));

    // 9. integration (SIRF-32) — advisory: is the line stopped, and is any
    //    held work parked where a human must look?
    checks.push(integration_check(ws));

    // 10. plugin/CLI version skew — advisory (SF-10). A CLI run outside a plugin
    //    context must never fail doctor, so this is informational either way.
    checks.push(plugin_version_check(
        std::env::var_os("CLAUDE_PLUGIN_ROOT")
            .map(PathBuf::from)
            .as_deref(),
        env!("CARGO_PKG_VERSION"),
    ));

    // Only GATING checks decide overall health; an advisory failure is a WARN.
    // `gate_configured` is gating, so an inert gate now flips `ok` — which is
    // what stops main.rs printing "all contract facts hold" and what makes the
    // human-mode exit code non-zero. No new aggregation mechanism: the fact
    // simply joins the existing all-gating-checks-pass fold.
    let ok = checks.iter().all(|c| !c.gating || c.pass);
    DoctorReport { ok, checks }
}

/// The integration red state and parked held work (SIRF-32): with
/// `on_fail: block` a red frontier is why `sirius run` refuses to launch, so
/// doctor must say so instead of reporting all green.
pub fn integration_check(ws: &Workspace) -> Check {
    const NAME: &str = "integration";
    let red = crate::ledger::Ledger::open(&ws.ledger_path())
        .ok()
        .and_then(|l| crate::integrate::red_state(&l));
    let parked = std::process::Command::new("git")
        .current_dir(&ws.root)
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/sirius/held-conflicted/",
            "refs/sirius/held-superseded/",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0);
    let parked_note = if parked > 0 {
        format!("; {parked} parked held-work ref(s) under refs/sirius/held-conflicted|held-superseded need a human look")
    } else {
        String::new()
    };
    match red {
        Some(r) => Check::advisory(
            NAME,
            false,
            format!(
                "RED at {}{} — with integration.on_fail \"block\" `sirius run` refuses to launch; fix and `sirius integrate`, or `sirius integrate --clear-red`{parked_note}",
                r.frontier,
                r.issue.map(|i| format!(" ({i})")).unwrap_or_default()
            ),
        ),
        None => Check::advisory(NAME, parked == 0, format!("not red{parked_note}")),
    }
}

/// Check #7 (ADVISORY, SIRF-26): which models the fleet would run on. A
/// config with no `models.default` is a WARN naming the model every Claude
/// worker would silently inherit — `sirius run` refuses it without
/// `--model` / `--allow-default-model`.
pub fn models_check(ws: &Workspace) -> Check {
    const NAME: &str = "fleet_models";
    // An invalid config is reported as such — never masked by defaults
    // (every other command refuses it).
    let mut cfg = match crate::config::Config::load(&ws.config_path()) {
        Ok(c) => c,
        Err(e) => return Check::advisory(NAME, false, e),
    };
    // Normalize exactly as `sirius run` does (no flags), then validate — a
    // config `run` would refuse must not pass here.
    if let Err(e) = crate::models::resolve(&mut cfg.models, None, None, false, None)
        .map(|_| ())
        .and_then(|()| crate::models::validate(&cfg.models))
    {
        return Check::advisory(NAME, false, format!("invalid models config: {e}"));
    }
    let m = &cfg.models;
    let routes: Vec<String> = m
        .routes
        .iter()
        .map(|r| format!("[{}]→{}", r.labels.join("|"), r.model))
        .collect();
    let describe = format!(
        "default {}, review {}, fix floor {}, routes {}",
        m.default.as_deref().unwrap_or("(none)"),
        crate::models::review_model(m)
            .as_deref()
            .unwrap_or("(none)"),
        m.fix_floor.as_deref().unwrap_or("(none)"),
        if routes.is_empty() {
            "(none)".to_string()
        } else {
            routes.join(", ")
        }
    );
    let describe = match m.fallback.as_deref() {
        Some(fb) => format!(
            "{describe}; FALLBACK on a fleet stop (usage limit / unsupported model): default {}, review {}",
            fb.default.as_deref().unwrap_or("(none)"),
            crate::models::review_model(fb)
                .as_deref()
                .unwrap_or("(none)")
        ),
        None => describe,
    };
    if m.default.as_deref().is_some_and(|d| !d.trim().is_empty()) {
        return Check::advisory(NAME, true, describe);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let would = crate::models::claude_default_model(&ws.root, home.as_deref())
        .map(|(model, from)| format!("`{model}` (from {from})"))
        .unwrap_or_else(|| "the CLI's built-in default".into());
    Check::advisory(
        NAME,
        m.allow_default,
        format!(
            "{describe} — no models.default: `sirius run` needs --model <id> (or --allow-default-model), else workers would run on {would}"
        ),
    )
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MockResponse, MockRunner};
    use std::path::PathBuf;

    fn ws_no_ametrite() -> Workspace {
        Workspace {
            root: PathBuf::from("/nonexistent"),
            ametrite_db: None,
            hayven_dir: None,
        }
    }

    /// Queue BOTH daemon probes: the `/` liveness GET (`curl -s -o /dev/null …`)
    /// and the SF-13 build probe (`curl -s -m 3 …/api/health`). They are keyed
    /// on their distinct third argv element, so the mock's longest-prefix rule
    /// hands each call the right canned answer.
    fn expect_daemon(m: &MockRunner, http_code: &str, health_json: &str) {
        m.push(MockResponse::new(&["curl", "-s", "-o"], 0, http_code, ""));
        m.push(MockResponse::new(&["curl", "-s", "-m"], 0, health_json, ""));
    }

    /// Give a fixture workspace a CONFIGURED gate, so `gate_configured` is not
    /// the thing under test in checks that are about something else.
    fn write_gate_cfg(root: &Path, test_cmd: &str) {
        std::fs::create_dir_all(root.join(".sirius")).unwrap();
        std::fs::write(
            root.join(".sirius/config.json"),
            format!(r#"{{"gate":{{"test_cmd":"{test_cmd}","fallback":"full-suite"}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn a_red_integration_shows_in_doctor() {
        // Branch review X11: doctor said all green while `run` refused.
        let dir = std::env::temp_dir().join(format!("sirius-doctor-red-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let led = crate::ledger::Ledger::create(&dir.join(".sirius/sirius.db"), "t").unwrap();
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: None,
            hayven_dir: None,
        };
        assert!(integration_check(&ws).pass, "not red");
        led.set_meta(
            crate::integrate::RED_KEY,
            Some(r#"{"at":"t","frontier":"f1","issue":"AMT-90"}"#),
        )
        .unwrap();
        let c = integration_check(&ws);
        assert!(!c.pass && !c.gating, "advisory, never gates `ok`");
        assert!(c.detail.contains("RED at f1 (AMT-90)") && c.detail.contains("--clear-red"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_config_is_reported_not_masked() {
        let dir = std::env::temp_dir().join(format!("sirius-doctor-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".sirius")).unwrap();
        std::fs::write(
            dir.join(".sirius/config.json"),
            r#"{"review":{"sequences":[{"dir":"m","key":"^\\d+"}]}}"#,
        )
        .unwrap();
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: None,
            hayven_dir: None,
        };
        let c = models_check(&ws);
        assert!(
            !c.pass && c.detail.contains("capture group"),
            "{}",
            c.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_green_when_everything_healthy() {
        // Build a workspace with a real read-only ametrite-like db in a temp dir.
        let dir = std::env::temp_dir().join(format!("sirius-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        std::fs::create_dir_all(dir.join(".hayven")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        // SIRF-10: a genuinely-healthy workspace must have a .hayven/ so the daemon
        // on :7777 is serving THIS repo (serving == true).
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: Some(dir.join(".hayven")),
        };

        write_gate_cfg(&dir, "cargo test");

        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        // SF-13: the daemon reports the SAME build as the CLI beside it.
        expect_daemon(
            &m,
            "200",
            r#"{"ok":true,"version":"0.0.5","native_version":"present"}"#,
        );
        m.expect(&["hayven", "--version"], 0, "0.0.5");
        m.expect(&["hayven", "daemon", "status"], 0, "running");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "commands: affected-tests remember recall claim",
            "",
        ));

        let report = run_with_plugins_dir(&ws, &m, None);
        assert!(report.ok, "checks: {:?}", report.checks);
        assert_eq!(report.checks.len(), 10);
        // With no plugins dir the handoff check is an advisory PASS (skipped),
        // clearly labeled — a CI box is not an incomplete install.
        let ph = report
            .checks
            .iter()
            .find(|c| c.name == "plugin_handoff")
            .unwrap();
        assert!(ph.pass && !ph.gating);
        assert!(ph.detail.contains("skipped"), "detail: {}", ph.detail);
    }

    // The central invariant of the advisory design, pinned END-TO-END: a
    // failing plugin_handoff must NOT flip report.ok (and therefore never
    // changes doctor's exit code) even when every gating check passes.
    #[test]
    fn advisory_failure_never_flips_overall_ok() {
        let dir = std::env::temp_dir().join(format!("sirius-doctor-adv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        std::fs::create_dir_all(dir.join(".hayven")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: Some(dir.join(".hayven")),
        };
        write_gate_cfg(&dir, "cargo test");
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        expect_daemon(&m, "200", r#"{"ok":true,"version":"0.0.5"}"#);
        m.expect(&["hayven", "--version"], 0, "0.0.5");
        m.expect(&["hayven", "daemon", "status"], 0, "running");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        // An EXISTING but empty plugins dir = the true cold state → advisory FAILS.
        let plugins = dir.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let report = run_with_plugins_dir(&ws, &m, Some(plugins));
        let ph = report
            .checks
            .iter()
            .find(|c| c.name == "plugin_handoff")
            .unwrap();
        assert!(!ph.pass && !ph.gating, "advisory must fail here");
        assert!(report.ok, "advisory failure flipped ok — the design broke");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- plugin handoff (check #6) -----------------------------------------

    fn plugins_fixture(name: &str, marketplaces: &str, plugins: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sirius-ph-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("known_marketplaces.json"), marketplaces).unwrap();
        std::fs::write(dir.join("installed_plugins.json"), plugins).unwrap();
        dir
    }

    // The exact state found on the audited machine (2026-08-05): every CLI
    // installed, hayvenhurst + catryna plugins from their OWN marketplaces,
    // and sirius — the only tool without a standalone marketplace — missing.
    #[test]
    fn plugin_handoff_flags_the_audited_dropoff_state() {
        let dir = plugins_fixture(
            "audit",
            r#"{"claude-plugins-official":{},"rlm-claude-code":{},"hayvenhurst":{},"catryna-wikinelli":{}}"#,
            r#"{"version":2,"plugins":{"rlm-claude-code@rlm-claude-code":{},"frontend-design@claude-plugins-official":{},"hayvenhurst@hayvenhurst":{},"catryna@catryna-wikinelli":{}}}"#,
        );
        let c = plugin_handoff_check(Some(&dir));
        assert!(!c.pass && !c.gating);
        // Hayvenhurst and catryna are present via their standalone marketplaces
        // (prefix rule) — ONLY the marketplace and the sirius plugin may be named.
        assert!(c.detail.contains("sirius-forester marketplace"));
        assert!(c.detail.contains("sirius plugin"));
        assert!(!c.detail.contains("hayvenhurst plugin"), "{}", c.detail);
        assert!(!c.detail.contains("catryna plugin"), "{}", c.detail);
        // The fix commands are exact, shell-runnable, and in cold-start order.
        assert!(c
            .detail
            .contains("claude plugin marketplace add Davidb3l/Sirius-Forester"));
        assert!(c
            .detail
            .contains("claude plugin install sirius@sirius-forester"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugin_handoff_passes_when_bundle_complete() {
        let dir = plugins_fixture(
            "full",
            r#"{"sirius-forester":{},"hayvenhurst":{}}"#,
            r#"{"version":2,"plugins":{"sirius@sirius-forester":{},"hayvenhurst@sirius-forester":{},"catryna@sirius-forester":{}}}"#,
        );
        let c = plugin_handoff_check(Some(&dir));
        assert!(c.pass && !c.gating, "detail: {}", c.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Mixed sources must satisfy the check: bundle marketplace added, sirius
    // from the bundle, hayvenhurst/catryna from their standalone marketplaces.
    #[test]
    fn plugin_handoff_accepts_any_marketplace_per_plugin() {
        let dir = plugins_fixture(
            "mixed",
            r#"{"sirius-forester":{},"hayvenhurst":{},"catryna-wikinelli":{}}"#,
            r#"{"version":2,"plugins":{"sirius@sirius-forester":{},"hayvenhurst@hayvenhurst":{},"catryna@catryna-wikinelli":{}}}"#,
        );
        let c = plugin_handoff_check(Some(&dir));
        assert!(c.pass, "mixed sources must pass, detail: {}", c.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Absent files inside an existing plugins dir = the true cold state.
    #[test]
    fn plugin_handoff_cold_state_names_everything() {
        let dir = std::env::temp_dir().join(format!("sirius-ph-cold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = plugin_handoff_check(Some(&dir));
        assert!(!c.pass && !c.gating);
        for needle in [
            "sirius-forester marketplace",
            "sirius plugin",
            "hayvenhurst plugin",
            "catryna plugin",
        ] {
            assert!(c.detail.contains(needle), "missing {needle}: {}", c.detail);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A prefix must not match a plugin whose name merely STARTS with another
    // tool's name — "sirius-console@x" is not the sirius plugin.
    #[test]
    fn plugin_handoff_prefix_requires_the_at_sign() {
        let dir = plugins_fixture(
            "prefix",
            r#"{"sirius-forester":{}}"#,
            r#"{"version":2,"plugins":{"sirius-console@somewhere":{},"hayvenhurst@hayvenhurst":{},"catryna@catryna-wikinelli":{}}}"#,
        );
        let c = plugin_handoff_check(Some(&dir));
        assert!(!c.pass);
        assert!(c.detail.contains("sirius plugin"), "{}", c.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fails_without_ametrite_schema() {
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.expect(&["curl"], 0, "200");
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"ok":true}"#,
            "",
        ));
        let report = run_with_plugins_dir(&ws_no_ametrite(), &m, None);
        assert!(!report.ok);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "amt_present_and_schema")
            .unwrap();
        assert!(!c.pass);
    }

    #[test]
    fn fails_when_daemon_down() {
        let dir = std::env::temp_dir().join(format!("sirius-doctor2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: None,
        };
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.push(MockResponse::new(&["curl"], 0, "000", "")); // no 200
        m.expect(&["hayven", "--version"], 0, "0.0.5");
        m.expect(&["hayven", "daemon", "status"], 0, "stopped");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        let report = run_with_plugins_dir(&ws, &m, None);
        assert!(!report.ok);
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.name == "hayven_daemon_7777")
                .unwrap()
                .pass
        );
    }

    // The 2026-08-05 defect: doctor said "daemon healthy on :7777 (status:
    // stopped)" — an orphan answered the port while the pidfile said stopped.
    // A 200 without an affirmatively-running status must FAIL and name the
    // real listener(s).
    #[test]
    fn port_answering_while_status_stopped_is_an_orphan_not_healthy() {
        let dir = std::env::temp_dir().join(format!("sirius-doctor-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        std::fs::create_dir_all(dir.join(".hayven")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: Some(dir.join(".hayven")),
        };
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.push(MockResponse::new(&["curl"], 0, "200", "")); // port ANSWERS
        m.expect(&["hayven", "--version"], 0, "0.0.7");
        m.expect(&["hayven", "daemon", "status"], 0, "stopped"); // pidfile disagrees
        m.push(MockResponse::new(&["lsof"], 0, "61445\n72001\n61445\n", ""));
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        let report = run_with_plugins_dir(&ws, &m, None);
        assert!(!report.ok, "healthy-while-stopped must be a FAIL");
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "hayven_daemon_7777")
            .unwrap();
        assert!(!c.pass);
        assert!(c.detail.contains("orphan"), "detail: {}", c.detail);
        // Distinct listeners named, deduped, flagged as MULTIPLE.
        assert!(
            c.detail.contains("MULTIPLE listeners: pids 61445, 72001"),
            "detail: {}",
            c.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Presence must be VERIFIED at check time, not inferred from discovery: a
    // hayven_dir pointing at a path that does not exist is "not found".
    #[test]
    fn claimed_hayven_dir_must_actually_exist() {
        let dir = std::env::temp_dir().join(format!("sirius-doctor-ghost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: Some(dir.join(".hayven")), // never created on disk
        };
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.push(MockResponse::new(&["curl"], 0, "200", ""));
        m.expect(&["hayven", "--version"], 0, "0.0.7");
        m.expect(&["hayven", "daemon", "status"], 0, "running (pid 999)");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        let report = run_with_plugins_dir(&ws, &m, None);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "hayven_daemon_7777")
            .unwrap();
        assert!(!c.pass, "ghost .hayven/ must not pass: {}", c.detail);
        assert!(c.detail.contains(".hayven/ not found"), "{}", c.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // SIRF-10: daemon up (http 200) but serving a DIFFERENT project must FAIL,
    // because reverse stamps (hayven remember) silently go one-way in that state.
    #[test]
    fn fails_when_daemon_serves_different_workspace() {
        let dir = std::env::temp_dir().join(format!("sirius-doctor3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        // This workspace has NO .hayven/, so a running daemon on :7777 is serving
        // some other project — `serving` is false and the check must fail.
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: None,
        };
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        m.push(MockResponse::new(&["curl"], 0, "200", "")); // daemon IS up
        m.expect(&["hayven", "--version"], 0, "0.0.5");
        m.expect(&["hayven", "daemon", "status"], 0, "running (other-repo)");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        let report = run_with_plugins_dir(&ws, &m, None);
        assert!(!report.ok, "checks: {:?}", report.checks);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "hayven_daemon_7777")
            .unwrap();
        assert!(!c.pass, "expected mismatch to fail, got: {}", c.detail);
        assert!(
            c.detail.contains("not serving this workspace"),
            "detail: {}",
            c.detail
        );
        assert!(
            c.detail.contains("hayven daemon start"),
            "expected fix-it hint, detail: {}",
            c.detail
        );
    }

    // ---- gate configuration (check #6, SF-11) ------------------------------

    /// A workspace fixture rooted in a fresh temp dir, with a healthy ametrite
    /// db and a real `.hayven/`. Everything else is per-test.
    fn healthy_ws(tag: &str) -> (PathBuf, Workspace) {
        let dir = std::env::temp_dir().join(format!("sirius-doc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ametrite")).unwrap();
        std::fs::create_dir_all(dir.join(".hayven")).unwrap();
        let dbp = dir.join(".ametrite/ametrite.db");
        {
            let c = Connection::open(&dbp).unwrap();
            c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO meta VALUES ('schema_version','3')", [])
                .unwrap();
        }
        let ws = Workspace {
            root: dir.clone(),
            ametrite_db: Some(dbp),
            hayven_dir: Some(dir.join(".hayven")),
        };
        (dir, ws)
    }

    /// Every non-gate call a healthy run needs, so a test can be about ONE fact.
    fn healthy_mock(cli_ver: &str, daemon_health_json: &str) -> MockRunner {
        let m = MockRunner::new();
        m.expect(&["amt", "--version"], 0, "amt 0.1.0");
        expect_daemon(&m, "200", daemon_health_json);
        m.expect(&["hayven", "--version"], 0, cli_ver);
        m.expect(&["hayven", "daemon", "status"], 0, "running (pid 4242)");
        m.push(MockResponse::new(
            &["amt", "--json", "claim", "--peek"],
            0,
            r#"{"claimed":false}"#,
            "",
        ));
        m.push(MockResponse::new(
            &["hayven", "--help"],
            0,
            "affected-tests remember recall",
            "",
        ));
        m
    }

    fn cfg_of(json: &str) -> Result<Config, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    #[test]
    fn gate_configured_passes_with_a_test_cmd() {
        let c = gate_configured_check(
            Path::new("/nowhere"),
            &cfg_of(r#"{"gate":{"test_cmd":"cargo test --workspace"}}"#),
            &crate::shell::ShellCmd::posix_sh(),
        );
        assert!(c.pass && c.gating, "detail: {}", c.detail);
        assert!(c.detail.contains("cargo test --workspace"), "{}", c.detail);
    }

    // The SF-11 state: `sirius init` wrote `"test_cmd": null`, so the FIRST gate
    // on this workspace fails closed and the issue strands in in_progress. The
    // message must name a repo-appropriate command AND the file it came from.
    #[test]
    fn gate_configured_fails_on_null_test_cmd_and_suggests_per_ecosystem() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "Cargo.toml",
                "[package]\nname=\"x\"\n",
                "cargo test --workspace",
            ),
            ("go.mod", "module x\n", "go test ./..."),
            ("pyproject.toml", "[project]\nname=\"x\"\n", "pytest"),
        ];
        for (marker, body, want) in cases {
            let dir = std::env::temp_dir().join(format!(
                "sirius-gc-{}-{}",
                marker.replace('.', "_"),
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(marker), body).unwrap();

            let c = gate_configured_check(
                &dir,
                &cfg_of(r#"{"gate":{"test_cmd":null}}"#),
                &crate::shell::ShellCmd::posix_sh(),
            );
            assert!(!c.pass, "must fail for {marker}");
            assert!(
                c.gating,
                "an inert gate is a CONTRACT failure, not a warning"
            );
            assert!(c.detail.contains(want), "{marker}: {}", c.detail);
            assert!(
                c.detail.contains(marker),
                "must name the file: {}",
                c.detail
            );
            // It must explain the consequence, not just the fact.
            assert!(c.detail.contains("fails closed"), "{}", c.detail);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // A whitespace test_cmd is worse than none: it reaches the shell, runs
    // nothing, and exits 0 — a gate that passes literally everything.
    #[test]
    fn gate_configured_rejects_a_blank_test_cmd() {
        let c = gate_configured_check(
            Path::new("/nowhere"),
            &cfg_of(r#"{"gate":{"test_cmd":"   "}}"#),
            &crate::shell::ShellCmd::posix_sh(),
        );
        assert!(!c.pass);
        assert!(c.detail.contains("pass everything"), "{}", c.detail);
    }

    #[test]
    fn gate_configured_fails_when_the_config_cannot_be_read() {
        let c = gate_configured_check(
            Path::new("/nowhere"),
            &Err("invalid config.json".into()),
            &crate::shell::ShellCmd::posix_sh(),
        );
        assert!(!c.pass && c.gating);
        assert!(c.detail.contains("invalid config.json"), "{}", c.detail);
    }

    // The headline of SF-11/SF-15: an inert gate must flip `report.ok`, because
    // that bit is what stops main.rs printing "all contract facts hold" and
    // what makes doctor's human-mode exit code non-zero. Pinned END-TO-END,
    // with every OTHER fact healthy, so nothing else can be crediting the flip.
    #[test]
    fn unconfigured_gate_flips_overall_ok_even_when_all_else_is_healthy() {
        let (dir, ws) = healthy_ws("gate-inert");
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        // Exactly what `sirius init` used to write.
        std::fs::create_dir_all(dir.join(".sirius")).unwrap();
        std::fs::write(
            dir.join(".sirius/config.json"),
            r#"{"gate":{"test_cmd":null,"fallback":"full-suite"}}"#,
        )
        .unwrap();

        let m = healthy_mock("0.0.7", r#"{"ok":true,"version":"0.0.7"}"#);
        let report = run_with_plugins_dir(&ws, &m, None);

        let gc = report
            .checks
            .iter()
            .find(|c| c.name == "gate_configured")
            .expect("gate_configured is a fact");
        assert!(!gc.pass, "an unset test_cmd must FAIL: {}", gc.detail);
        assert!(!report.ok, "an inert gate must flip overall ok");
        // ...and it is the ONLY gating failure, i.e. nothing else was disturbed.
        let failing: Vec<&str> = report
            .checks
            .iter()
            .filter(|c| c.gating && !c.pass)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            failing,
            vec!["gate_configured"],
            "checks: {:?}",
            report.checks
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- daemon build skew (check #2, SF-13) --------------------------------

    // The 2026-08-26 incident: doctor said "daemon healthy on :7777 (status:
    // running (pid 25808))" while the daemon was a build predating the 0.0.7
    // CLI, and every entity claim failed with "hayven-native serialize encode
    // failed". Liveness is not health -- a build mismatch must FAIL and carry
    // the exact remedy.
    #[test]
    fn stale_daemon_build_fails_with_the_stop_start_remedy() {
        let (dir, ws) = healthy_ws("stale-daemon");
        write_gate_cfg(&dir, "cargo test");
        let m = healthy_mock(
            "0.0.7",
            r#"{"ok":true,"version":"0.0.5","native_version":"present"}"#,
        );
        let report = run_with_plugins_dir(&ws, &m, None);

        let c = report
            .checks
            .iter()
            .find(|c| c.name == "hayven_daemon_7777")
            .unwrap();
        assert!(
            !c.pass,
            "a stale daemon must not read as healthy: {}",
            c.detail
        );
        assert!(!report.ok);
        assert!(c.detail.contains("STALE DAEMON"), "{}", c.detail);
        assert!(
            c.detail.contains("0.0.7") && c.detail.contains("0.0.5"),
            "{}",
            c.detail
        );
        assert!(
            c.detail
                .contains("hayven daemon stop && hayven daemon start"),
            "the exact remedy must be in the message: {}",
            c.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_daemon_build_passes_and_names_both_versions() {
        let (dir, ws) = healthy_ws("fresh-daemon");
        write_gate_cfg(&dir, "cargo test");
        let m = healthy_mock(
            "0.0.7",
            r#"{"ok":true,"version":"0.0.7","native_version":"present"}"#,
        );
        let report = run_with_plugins_dir(&ws, &m, None);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "hayven_daemon_7777")
            .unwrap();
        assert!(c.pass, "detail: {}", c.detail);
        assert!(c.detail.contains("SAME build 0.0.7"), "{}", c.detail);
        // native_version is surfaced verbatim -- hayven reports "present", not a
        // version, which is the residual gap this check cannot close.
        assert!(c.detail.contains("native: present"), "{}", c.detail);
        assert!(report.ok, "checks: {:?}", report.checks);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Upstream spells the CLI version several ways over time ("0.0.7",
    // "hayven 0.0.7", "v0.0.7"). A cosmetic reword must not manufacture a
    // false stale-daemon alarm -- that would train operators to ignore it.
    #[test]
    fn version_spelling_differences_are_not_skew() {
        assert!(same_build("hayven 0.0.7", "0.0.7"));
        assert!(same_build("v0.0.7", " 0.0.7 "));
        assert!(!same_build("0.0.7", "0.0.6"));
    }

    // A daemon that will not say what it is cannot be shown to match the CLI.
    // Fail-closed: an upgrade-straddling daemon looks EXACTLY like this, and
    // the cost of guessing wrong is a whole fleet run producing nothing.
    #[test]
    fn daemon_that_reports_no_build_is_not_blessed() {
        let (dir, ws) = healthy_ws("mute-daemon");
        write_gate_cfg(&dir, "cargo test");
        // /api/health answers, but without a `version` field.
        let m = healthy_mock("0.0.7", r#"{"ok":true,"pid":1234}"#);
        let report = run_with_plugins_dir(&ws, &m, None);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "hayven_daemon_7777")
            .unwrap();
        assert!(!c.pass, "unverifiable build must not pass: {}", c.detail);
        assert!(
            c.detail.contains("would not report its build"),
            "{}",
            c.detail
        );
        assert!(
            c.detail
                .contains("hayven daemon stop && hayven daemon start"),
            "{}",
            c.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- plugin/CLI version skew (check #8, SF-10) --------------------------

    // Informational by construction: a plain CLI run has no plugin around it
    // and must never fail doctor for that.
    #[test]
    fn plugin_version_outside_a_plugin_context_is_advisory_pass() {
        let c = plugin_version_check(None, "0.1.1");
        assert!(c.pass && !c.gating);
        assert!(c.detail.contains("0.1.1"), "{}", c.detail);
        assert!(c.detail.contains("CLAUDE_PLUGIN_ROOT"), "{}", c.detail);
    }

    // SF-10: a plugin fix that shipped WITHOUT a manifest bump leaves users on
    // a stale cached copy forever. Doctor cannot fix that -- it makes it visible
    // by printing both versions side by side.
    #[test]
    fn plugin_version_reports_the_manifest_version_beside_the_cli() {
        let root = std::env::temp_dir().join(format!("sirius-pv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/plugin.json"),
            r#"{"name":"sirius","version":"0.2.0"}"#,
        )
        .unwrap();
        let c = plugin_version_check(Some(&root), "0.1.1");
        assert!(c.pass && !c.gating, "must never gate: {}", c.detail);
        assert!(c.detail.contains("plugin 0.2.0"), "{}", c.detail);
        assert!(c.detail.contains("0.1.1"), "{}", c.detail);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plugin_version_with_an_unreadable_manifest_still_never_gates() {
        let root = std::env::temp_dir().join(format!("sirius-pv-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let c = plugin_version_check(Some(&root), "0.1.1");
        assert!(c.pass && !c.gating);
        assert!(c.detail.contains("cannot be seen"), "{}", c.detail);
        let _ = std::fs::remove_dir_all(&root);
    }
}
