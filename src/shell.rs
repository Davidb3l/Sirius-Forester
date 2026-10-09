//! The shell boundary.
//!
//! Every call Sirius makes into `amt` or `hayven` goes through a [`Runner`]. In
//! production that is [`RealRunner`], which spawns a subprocess. In tests it is
//! [`MockRunner`], which returns canned output keyed by the argv prefix, so the
//! whole binary is testable offline. Sirius NEVER opens the parent SQLite for
//! writing — this seam is the only write path to the parents (via their CLIs).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
#[cfg(test)]
use std::{collections::VecDeque, sync::Mutex};

/// The captured result of one external command invocation.
#[derive(Debug, Clone)]
pub struct CmdOutput {
    /// Process exit code. `None` means the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// How to supervise a long-running agent command (SIRF-7). Passed to
/// [`Runner::run_agent`], which spawns the child, fires the heartbeat callback
/// on `heartbeat_interval`, kills the child if `timeout` elapses, and captures
/// its combined stdout/stderr to `log_path` (if set).
#[derive(Debug, Clone)]
pub struct AgentRunOpts {
    /// Hard wall-clock cap on the agent run. On expiry the child is killed and
    /// the outcome is [`AgentOutcome::TimedOut`].
    pub timeout: Duration,
    /// How often the heartbeat callback fires while the child runs. Derived from
    /// the amt lease TTL (lease/3) so a lease can never lapse mid-run.
    pub heartbeat_interval: Duration,
    /// Where to persist the agent's output so it does not vanish on success.
    /// `None` disables durable capture (still returned in-memory).
    pub log_path: Option<PathBuf>,
    /// Extra environment for the child — the SIRF-22/23 agent contract
    /// (`SIRIUS_ISSUE`, `SIRIUS_WORKER`, `AMT_AGENT`, `SIRIUS_PHASE`, …).
    /// Layered on top of the inherited environment.
    pub env: Vec<(String, String)>,
}

/// Which limit killed an agent (SIRF-41 #3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// No progress — no output, no worktree change — for the idle window.
    Idle,
    /// The wall-clock cap (`AgentRunOpts::timeout`) — the only kind a run
    /// without a [`ProgressWatch`] (reviewer, integration, canary) can hit.
    Hard,
}

impl TimeoutKind {
    /// The NDJSON spelling (`timeout_kind` on work/fix events).
    pub fn as_str(self) -> &'static str {
        match self {
            TimeoutKind::Idle => "idle",
            TimeoutKind::Hard => "hard",
        }
    }

    /// The human sentence for a release comment, naming the limit that fired.
    pub fn describe(self, idle_secs: u64, hard_secs: u64) -> String {
        match self {
            TimeoutKind::Idle => {
                format!("agent idle for {idle_secs}s — no output, no file changes")
            }
            TimeoutKind::Hard => format!("hit the hard cap of {hard_secs}s"),
        }
    }
}

/// The result of supervising an agent command (SIRF-7).
#[derive(Debug, Clone)]
pub enum AgentOutcome {
    /// The child exited on its own; carries its captured output.
    Exited(CmdOutput),
    /// A limit fired and the child was killed. `output` holds whatever was
    /// captured before the kill; `kind` says which limit (SIRF-41).
    TimedOut {
        output: CmdOutput,
        kind: TimeoutKind,
    },
}

impl AgentOutcome {
    /// True only when the child exited cleanly (exit 0). A timeout is a failure.
    pub fn success(&self) -> bool {
        matches!(self, AgentOutcome::Exited(o) if o.success())
    }

    /// The captured output, whichever arm.
    pub fn output(&self) -> &CmdOutput {
        match self {
            AgentOutcome::Exited(o) => o,
            AgentOutcome::TimedOut { output, .. } => output,
        }
    }

    pub fn timed_out(&self) -> bool {
        matches!(self, AgentOutcome::TimedOut { .. })
    }

    /// Which limit killed the child; `None` when it exited on its own.
    pub fn timeout_kind(&self) -> Option<TimeoutKind> {
        match self {
            AgentOutcome::TimedOut { kind, .. } => Some(*kind),
            AgentOutcome::Exited(_) => None,
        }
    }
}

/// A cheap snapshot of the agent's worktree, for progress detection
/// (SIRF-41 #3). Two snapshots that differ mean the agent DID something; a
/// differing `head` means it committed (which also drives the #4 grace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeProbe {
    /// `HEAD`'s sha (empty on an unborn branch).
    pub head: String,
    /// Hash of the porcelain status plus the size+mtime of every listed path,
    /// so an edit to an ALREADY-dirty file still registers.
    pub tree: u64,
}

/// Progress-aware supervision for a WORK/FIX agent (SIRF-41 #3/#4), passed to
/// [`Runner::run_agent_watched`]. The agent is killed only when it has shown
/// no progress for `idle` — progress = its log grew, or `probe` returned a
/// different snapshot — or when `AgentRunOpts::timeout` (the hard cap) runs out.
pub struct ProgressWatch<'a> {
    /// Kill after this long with no output and no worktree change.
    pub idle: Duration,
    /// How often `probe` runs (it shells out to git, so ~30s in production).
    /// One extra probe always runs right before a kill, so a change made since
    /// the last poll is never missed.
    pub probe_every: Duration,
    /// SIRF-41 #4: when a kill is due and `HEAD` moved within this window,
    /// the kill is deferred ONCE by this long — an agent that just committed
    /// is finishing, not stuck.
    pub grace: Duration,
    /// Snapshot the worktree; `None` = could not tell (never counts as progress).
    pub probe: &'a mut dyn FnMut() -> Option<WorktreeProbe>,
}

/// Abstraction over "run this program with these args and give me the output".
pub trait Runner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput>;

    /// Supervise a long-running agent command (SIRF-7): spawn it, fire
    /// `heartbeat` on `opts.heartbeat_interval` while it runs (so both leases
    /// stay renewed), enforce `opts.timeout` by killing the child on expiry,
    /// and capture its output to `opts.log_path`. The `heartbeat` closure is
    /// invoked from the calling thread — it may freely borrow `amt`/`hayven`.
    ///
    /// Default impl ignores supervision and delegates to [`Runner::run`], which
    /// keeps the [`Runner`] trait object-safe for any runner that does not need
    /// agent supervision (only the real/mock agent path overrides it).
    fn run_agent(
        &self,
        program: &str,
        args: &[&str],
        _opts: &AgentRunOpts,
        _heartbeat: &mut dyn FnMut(),
    ) -> std::io::Result<AgentOutcome> {
        self.run(program, args).map(AgentOutcome::Exited)
    }

    /// [`Runner::run_agent`] with PROGRESS-AWARE supervision (SIRF-41 #3/#4):
    /// `opts.timeout` becomes the hard cap, and the child is also killed once
    /// it has made no progress for `watch.idle`. Used for WORK/FIX agents; the
    /// reviewer, integration and canary runs keep the plain wall clock.
    ///
    /// Default impl ignores the watch and delegates to `run_agent`, so a
    /// runner (or a test wrapper) that only overrides `run_agent` still works.
    fn run_agent_watched(
        &self,
        program: &str,
        args: &[&str],
        opts: &AgentRunOpts,
        watch: &mut ProgressWatch<'_>,
        heartbeat: &mut dyn FnMut(),
    ) -> std::io::Result<AgentOutcome> {
        let _ = watch;
        self.run_agent(program, args, opts, heartbeat)
    }
}

/// Build the `Command` for `program args`.
///
/// One exception to plain `args`: a script handed to `cmd.exe /C` on Windows.
/// Rust quotes an argument the MSVC way (`"` inside becomes `\"`), which cmd.exe
/// does not understand — `claude -p "work the claimed issue"` reached claude as
/// the four words `"work`, `the`, `claimed`, `issue"`. So the script goes on the
/// command line RAW, as `/S /C "<script>"`: `/S` makes cmd strip exactly the
/// outer pair of quotes and run the rest verbatim, whatever quotes it contains.
fn command_for(program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    if let [flag, script] = args {
        if flag.eq_ignore_ascii_case("/c") && is_cmd_exe(program) {
            use std::os::windows::process::CommandExt;
            cmd.raw_arg(cmd_exe_raw_args(script));
            return cmd;
        }
    }
    cmd.args(args);
    cmd
}

/// Whether `program` is `cmd.exe` (by file stem, so `%ComSpec%`'s absolute
/// path and a bare `cmd` both count).
fn is_cmd_exe(program: &str) -> bool {
    matches!(program_stem(program).as_str(), "cmd" | "command")
}

/// The raw `cmd.exe` tail that runs `script` verbatim (see [`command_for`]).
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_exe_raw_args(script: &str) -> String {
    format!("/S /C \"{script}\"")
}

/// Spawns real subprocesses.
#[derive(Debug, Default, Clone)]
pub struct RealRunner {
    /// Working directory for every spawned command. `None` inherits the
    /// process cwd. Fleet workers set this to their isolated git worktree so
    /// agents, git, and test runs never touch the shared checkout.
    pub cwd: Option<std::path::PathBuf>,
}

impl Runner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        let mut cmd = command_for(program, args);
        if let Some(d) = &self.cwd {
            cmd.current_dir(d);
        }
        let out = cmd.output()?;
        Ok(CmdOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Real agent supervision (SIRF-7): spawn the child streaming its
    /// stdout+stderr STRAIGHT to the durable log file, then poll `try_wait` on a
    /// short tick. Each `heartbeat_interval` we call back into the heartbeat
    /// closure (renews both leases); once `timeout` elapses we `kill` the child
    /// so a hung agent can never hang the loop forever.
    ///
    /// Output is streamed to the file (not piped into memory and drained after
    /// exit): draining-after-exit **deadlocks** any agent that writes more than
    /// the OS pipe buffer (~64 KB — a verbose test run trivially exceeds it),
    /// because the child blocks on a full pipe, never exits, and `try_wait`
    /// polls forever until the timeout kills it. A file sink has no such buffer.
    fn run_agent(
        &self,
        program: &str,
        args: &[&str],
        opts: &AgentRunOpts,
        heartbeat: &mut dyn FnMut(),
    ) -> std::io::Result<AgentOutcome> {
        self.supervise(program, args, opts, None, heartbeat)
    }

    /// SIRF-41: the same supervision, plus the idle watchdog and the
    /// finish grace — see [`RealRunner::supervise`].
    fn run_agent_watched(
        &self,
        program: &str,
        args: &[&str],
        opts: &AgentRunOpts,
        watch: &mut ProgressWatch<'_>,
        heartbeat: &mut dyn FnMut(),
    ) -> std::io::Result<AgentOutcome> {
        self.supervise(program, args, opts, Some(watch), heartbeat)
    }
}

/// What the supervisor has seen of an agent's progress (SIRF-41 #3/#4).
struct Progress {
    /// The last worktree snapshot (the baseline until the first change).
    snapshot: Option<WorktreeProbe>,
    /// When progress (log growth or a worktree change) was last seen.
    last_progress: Instant,
    /// When a `HEAD` move was last seen — drives the finish grace.
    last_commit: Option<Instant>,
    last_probe: Instant,
    log_len: u64,
}

impl Progress {
    /// Take a fresh worktree snapshot; a difference from the last one is
    /// progress, and a different `HEAD` is a commit.
    fn observe(&mut self, w: &mut ProgressWatch<'_>, now: Instant) {
        self.last_probe = now;
        let Some(snap) = (w.probe)() else { return };
        if let Some(prev) = &self.snapshot {
            if prev.head != snap.head {
                self.last_commit = Some(now);
            }
            if *prev != snap {
                self.last_progress = now;
            }
        }
        self.snapshot = Some(snap);
    }

    /// Output is progress: the streamed log growing is the cheapest signal
    /// there is (one `stat`), so it is checked every tick.
    fn poll_log(&mut self, path: Option<&Path>, now: Instant) {
        let len = path
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len());
        if let Some(len) = len {
            if len != self.log_len {
                self.log_len = len;
                self.last_progress = now;
            }
        }
    }
}

impl RealRunner {
    /// Real agent supervision (SIRF-7, SIRF-41). Without a `watch` this is the
    /// plain wall clock: kill at `opts.timeout`. With one:
    ///
    /// * **idle** — kill once nothing has happened for `watch.idle`: the log
    ///   did not grow (checked every tick) and the worktree snapshot did not
    ///   change (probed every `watch.probe_every`, and once more right before
    ///   any kill, so a change since the last poll is never missed);
    /// * **hard** — `opts.timeout` still caps the run, progress or not;
    /// * **grace** — when either kill is due and `HEAD` moved within
    ///   `watch.grace`, the kill is deferred ONCE by `watch.grace`: an agent
    ///   that committed seconds ago is finishing, not stuck (the MSX-80 case:
    ///   committed at 16:51:46, killed at 16:51:54).
    fn supervise(
        &self,
        program: &str,
        args: &[&str],
        opts: &AgentRunOpts,
        mut watch: Option<&mut ProgressWatch<'_>>,
        heartbeat: &mut dyn FnMut(),
    ) -> std::io::Result<AgentOutcome> {
        use std::process::Stdio;

        // Combined stdout+stderr → the log file (two dup'd handles share one
        // file offset, so writes interleave without clobbering). No log path (or
        // an unopenable file) ⇒ discard, never pipe — a pipe would risk the
        // deadlock described above.
        let (stdout_cfg, stderr_cfg) = match agent_log_file(opts.log_path.as_deref()) {
            Some(file) => {
                let dup = file.try_clone()?;
                (Stdio::from(file), Stdio::from(dup))
            }
            None => (Stdio::null(), Stdio::null()),
        };

        let mut cmd = command_for(program, args);
        // Agents are non-interactive: an inherited stdin makes CLIs like
        // `claude -p` wait for piped input (observed: a 3s stall + warning on
        // every run) or, worse, read the fleet's own terminal.
        cmd.stdin(Stdio::null())
            .stdout(stdout_cfg)
            .stderr(stderr_cfg);
        cmd.envs(opts.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if let Some(d) = &self.cwd {
            cmd.current_dir(d);
        }
        // The baseline snapshot is taken BEFORE the spawn, so even a commit
        // the agent makes in its first second differs from it.
        let baseline = watch.as_mut().and_then(|w| (w.probe)());
        let mut child = cmd.spawn()?;

        // Poll on a tick short enough to stay responsive to the timeout (and
        // the probe cadence), but never longer than the heartbeat interval.
        let mut tick = opts.heartbeat_interval.min(Duration::from_millis(500));
        if let Some(w) = watch.as_ref() {
            tick = tick.min(w.probe_every);
        }
        let tick = tick.max(Duration::from_millis(10));
        let start = Instant::now();
        let mut last_beat = Instant::now();
        let mut seen = Progress {
            snapshot: baseline,
            last_progress: start,
            last_commit: None,
            last_probe: start,
            log_len: 0,
        };
        // SIRF-41 #4: `Some(until)` once the one-time finish grace is granted.
        let mut reprieve: Option<Instant> = None;

        loop {
            if let Some(status) = child.try_wait()? {
                append_exit_trailer(opts.log_path.as_deref(), status.code());
                return Ok(AgentOutcome::Exited(CmdOutput {
                    code: status.code(),
                    stdout: String::new(),
                    stderr: String::new(),
                }));
            }
            let now = Instant::now();
            if let Some(w) = watch.as_mut() {
                seen.poll_log(opts.log_path.as_deref(), now);
                if now.duration_since(seen.last_probe) >= w.probe_every {
                    seen.observe(w, now);
                }
            }
            let hard_due = now.duration_since(start) >= opts.timeout;
            let idle_due = watch
                .as_ref()
                .is_some_and(|w| now.duration_since(seen.last_progress) >= w.idle);
            let reprieved = reprieve.is_some_and(|until| now < until);
            if (hard_due || idle_due) && !reprieved {
                let mut kill = true;
                if let Some(w) = watch.as_mut() {
                    // Look once more before killing: a change since the last
                    // poll is progress, and a fresh commit earns the grace.
                    seen.observe(w, now);
                    let still_idle = now.duration_since(seen.last_progress) >= w.idle;
                    let just_committed = seen
                        .last_commit
                        .is_some_and(|c| now.duration_since(c) <= w.grace);
                    if !hard_due && !still_idle {
                        kill = false;
                    } else if reprieve.is_none() && just_committed && !w.grace.is_zero() {
                        reprieve = Some(now + w.grace);
                        kill = false;
                    }
                }
                if kill {
                    let kind = if hard_due {
                        TimeoutKind::Hard
                    } else {
                        TimeoutKind::Idle
                    };
                    // Hung or over-budget: kill the whole tree (an
                    // integration cmd's servers, an agent's subprocesses
                    // would otherwise outlive it), reap, report a timeout.
                    kill_descendants(child.id());
                    let _ = child.kill();
                    let code = child.wait().ok().and_then(|s| s.code());
                    append_kill_note(opts.log_path.as_deref(), kind);
                    append_exit_trailer(opts.log_path.as_deref(), code);
                    return Ok(AgentOutcome::TimedOut {
                        output: CmdOutput {
                            code,
                            stdout: String::new(),
                            stderr: String::new(),
                        },
                        kind,
                    });
                }
            }
            if last_beat.elapsed() >= opts.heartbeat_interval {
                heartbeat();
                last_beat = Instant::now();
            }
            std::thread::sleep(tick);
        }
    }
}

/// SIRF-41 #3: snapshot the worktree the runner's commands run in — `HEAD`
/// plus a hash of `git status --porcelain -uall` and the size+mtime of every
/// path it lists (so re-editing an already-dirty file still counts). Three
/// cheap git calls; [`RealRunner::supervise`] runs it every ~30s. Ignored
/// paths (`target/`, `node_modules/`) are invisible to it by design — a build
/// writing there is not the agent changing the work.
///
/// `--no-optional-locks`: a background `git status` must never take
/// `.git/index.lock` — the agent's own `git add` would then fail with
/// "index.lock exists". `None` when this is not a git worktree.
pub fn worktree_probe(runner: &dyn Runner) -> Option<WorktreeProbe> {
    use std::hash::{Hash, Hasher};
    let ok = |args: &[&str]| {
        runner
            .run("git", args)
            .ok()
            .filter(CmdOutput::success)
            .map(|o| o.stdout)
    };
    let root = PathBuf::from(ok(&["rev-parse", "--show-toplevel"])?.trim());
    let head = ok(&["rev-parse", "-q", "--verify", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let status = ok(&[
        "--no-optional-locks",
        "-c",
        "status.relativePaths=false",
        "status",
        "--porcelain",
        "-z",
        "-uall",
    ])?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    status.hash(&mut h);
    let mut entries = status.split('\0').filter(|t| !t.is_empty());
    let mut stats = 0usize;
    while let Some(entry) = entries.next() {
        let (Some(xy), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        if xy.contains(['R', 'C']) {
            let _ = entries.next(); // a rename's/copy's ORIGINAL path
        }
        // Bounded: a pathological status (thousands of untracked files)
        // still hashes its listing; only the per-file stats stop.
        if stats >= 5000 {
            continue;
        }
        stats += 1;
        match std::fs::metadata(root.join(path)) {
            Ok(m) => {
                m.len().hash(&mut h);
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .hash(&mut h);
            }
            Err(_) => "missing".hash(&mut h),
        }
    }
    Some(WorktreeProbe {
        head,
        tree: h.finish(),
    })
}

/// SIRF-52: the env var that bounds how long headless `claude -p` waits for
/// its background tasks (helper agents) before killing them. Its built-in
/// default is 600s, which silently killed a worker's helpers mid-ticket.
/// Sirius exports `0` (= wait indefinitely) to every agent and reviewer:
/// Sirius owns the real timeout (idle watchdog + hard cap), so a second,
/// shorter, invisible one only destroys work.
pub const BG_WAIT_CEILING_ENV: &str = "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS";

/// SIRF-52: did the agent's CLI kill its background tasks on the way out?
/// `claude -p` prints "Background tasks still running after 600s;
/// terminating. …" and then exits — often with 0, so the run LOOKS like a
/// success with the main deliverable missing. Judged on the last non-blank
/// lines only (Sirius's own trailer excluded), so an agent that merely
/// quotes the message mid-run does not trip it.
pub fn bg_tasks_killed(log: &str) -> bool {
    log.lines()
        .rev()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("[sirius]"))
        .take(20)
        .any(|l| l.contains("Background tasks still running") && l.contains("terminating"))
}

/// [`bg_tasks_killed`] over the last 64 KB of an agent log file. A missing
/// or unreadable log is "no" — never a reason to fail a run.
pub fn bg_tasks_killed_in(path: Option<&Path>) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Some(mut f) = path.and_then(|p| std::fs::File::open(p).ok()) else {
        return false;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)));
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return false;
    }
    bg_tasks_killed(&String::from_utf8_lossy(&buf))
}

/// SIGKILL every descendant of `pid` (children first found, whole tree).
/// Not a new process group: that would keep the terminal's Ctrl-C from
/// reaching agents. Best-effort; a no-op where `ps` is unavailable (Windows),
/// and a daemon that double-forked away is out of reach by design.
fn kill_descendants(pid: u32) {
    if cfg!(windows) {
        return;
    }
    // Freeze the root BEFORE the snapshot, so it cannot fork its next
    // command between the snapshot and the kill (`a; b`: b would escape).
    let _ = Command::new("kill")
        .args(["-STOP", &pid.to_string()])
        .output();
    let Ok(out) = Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
        .output()
    else {
        return; // the caller's child.kill() still ends the (stopped) root
    };
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace().map(|n| n.parse::<u32>().ok());
            Some((it.next()??, it.next()??))
        })
        .collect();
    let mut tree = vec![pid];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        tree.extend(
            pairs
                .iter()
                .filter(|(_, pp)| *pp == parent)
                .map(|(p, _)| *p),
        );
        i += 1;
    }
    // The root too: a stopped root would otherwise wait for its CONT.
    let victims: Vec<String> = tree.iter().map(u32::to_string).collect();
    if !victims.is_empty() {
        let _ = Command::new("kill").arg("-KILL").args(&victims).output();
    }
}

/// Open (create+truncate) the agent log file for streaming, creating parent
/// dirs. Returns None when no path is configured or the file can't be opened —
/// the caller then discards output rather than risk the pipe-buffer deadlock.
/// Best-effort: a log failure must never fail the iteration. (SIRF-7)
fn agent_log_file(path: Option<&std::path::Path>) -> Option<std::fs::File> {
    let path = path?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::File::create(path).ok()
}

/// Append the agent's exit status to the streamed log, once it has exited.
/// Best-effort. (SIRF-7)
fn append_exit_trailer(path: Option<&std::path::Path>, code: Option<i32>) {
    let Some(path) = path else { return };
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = writeln!(
            f,
            "\n[sirius] agent exit: {}",
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "killed".into())
        );
    }
}

/// Say in the log WHY the agent was killed (SIRF-41), so a reader of the log
/// alone can tell a hang from a long-but-busy run. Best-effort.
fn append_kill_note(path: Option<&std::path::Path>, kind: TimeoutKind) {
    let Some(path) = path else { return };
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
        let why = match kind {
            TimeoutKind::Idle => "idle timeout: no output and no worktree change",
            TimeoutKind::Hard => "hard cap reached",
        };
        let _ = writeln!(f, "\n[sirius] agent killed ({why})");
    }
}

// ── Shell resolution (SF-15) ────────────────────────────────────────────────
//
// `gate.test_cmd` is a SCRIPT, not an argv: `cargo test --workspace && bun run
// check` needs something to interpret `&&`. Sirius used to spawn the literal
// program `sh`, which meant the gate silently depended on *whoever launched
// sirius*: from Git Bash `sh` is on PATH and the compound command passed; from
// PowerShell (same repo, same config, same commit) `sh` is not on PATH, the
// spawn failed with a bare "program not found", and the gate FAILED. Sirius is
// also started by things with no shell at all (a service, a scheduler), so
// inheriting is not an option. Resolution is therefore explicit and identical
// no matter who launched us — see [`resolve_shell_from`] for the rule.

/// Environment override for the shell Sirius runs `gate.test_cmd` through.
/// Set it to a full program path (e.g. `C:\Program Files\Git\bin\bash.exe`)
/// when the automatic rule picks the wrong interpreter.
pub const SHELL_OVERRIDE_ENV: &str = "SIRIUS_SHELL";

/// A resolved shell invocation: the program to spawn plus the flag that means
/// "run this entire string as a script".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCmd {
    /// Absolute path where we resolved one; otherwise a bare program name.
    pub program: String,
    /// `-c` for POSIX shells, `/C` for `cmd.exe`.
    pub flag: String,
    /// True when the resolved shell is POSIX, i.e. `&&`, `|`, and single-quote
    /// quoting behave the way a `test_cmd` author wrote them.
    pub posix: bool,
}

impl ShellCmd {
    /// The canonical POSIX shell. Also the deterministic value tests use so a
    /// gate test never depends on which shell the test machine happens to have.
    pub fn posix_sh() -> ShellCmd {
        ShellCmd {
            program: "/bin/sh".into(),
            flag: "-c".into(),
            posix: true,
        }
    }

    /// Build an invocation for an explicitly-named program, inferring its
    /// "run this string" flag from the program name (`cmd.exe` wants `/C`).
    fn for_program(program: &str) -> ShellCmd {
        let cmd_like = is_cmd_exe(program);
        ShellCmd {
            program: program.to_string(),
            flag: if cmd_like { "/C".into() } else { "-c".into() },
            posix: !cmd_like,
        }
    }

    /// `<program> <flag>` — for messages that must name the shell.
    pub fn describe(&self) -> String {
        format!("{} {}", self.program, self.flag)
    }
}

/// Lowercased file stem of a program path (`C:\…\sh.exe` → `sh`).
fn program_stem(program: &str) -> String {
    let base = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_string()
}

/// The shell for this process, resolved from the real environment.
pub fn resolve_shell() -> ShellCmd {
    resolve_shell_from(
        std::env::var(SHELL_OVERRIDE_ENV).ok().as_deref(),
        cfg!(windows),
        std::env::var("PATH").ok().as_deref(),
        std::env::var("ComSpec").ok().as_deref(),
        &|p: &Path| p.is_file(),
    )
}

/// The resolution RULE, pure over its inputs so both platforms are unit-tested
/// on either platform:
///
/// 1. `SIRIUS_SHELL`, if set, wins verbatim — the documented escape hatch.
/// 2. On unix: `/bin/sh -c`. Absolute, not `sh` off PATH: a daemon/service can
///    be started with an empty PATH, and the shell must not depend on it.
/// 3. On Windows: the first real POSIX `sh` on PATH (`sh.exe`, then `sh`), used
///    with `-c`, because `test_cmd` is written POSIX-style — Git for Windows
///    puts one there. We resolve it to an ABSOLUTE path so a later PATH change
///    cannot swap the interpreter out from under a queued gate.
/// 4. Otherwise on Windows: `%ComSpec%` (`cmd.exe`) with `/C`, which at least
///    understands `&&` and `|`, so most compound commands still work.
///
/// `bash` is deliberately NEVER probed by name on Windows:
/// `C:\Windows\System32\bash.exe` is the WSL launcher, so picking it up would
/// run the test suite inside a Linux VM against a translated path — a different
/// machine from the checkout, failing in ways nobody could diagnose from the
/// gate output. Only `sh` is searched; anything else must be named explicitly
/// via `SIRIUS_SHELL`.
pub fn resolve_shell_from(
    override_cmd: Option<&str>,
    windows: bool,
    path_var: Option<&str>,
    comspec: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> ShellCmd {
    if let Some(o) = override_cmd.map(str::trim).filter(|s| !s.is_empty()) {
        return ShellCmd::for_program(o);
    }
    if !windows {
        return ShellCmd::posix_sh();
    }
    if let Some(sh) = find_posix_sh(path_var, exists) {
        return ShellCmd {
            program: sh,
            flag: "-c".into(),
            posix: true,
        };
    }
    let comspec = comspec
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("cmd.exe");
    ShellCmd {
        program: comspec.to_string(),
        flag: "/C".into(),
        posix: false,
    }
}

/// Scan a Windows `PATH` for a POSIX `sh`, returning its absolute path.
fn find_posix_sh(path_var: Option<&str>, exists: &dyn Fn(&Path) -> bool) -> Option<String> {
    let path_var = path_var?;
    for dir in path_var.split(';') {
        let dir = dir.trim().trim_matches('"');
        if dir.is_empty() {
            continue;
        }
        for name in ["sh.exe", "sh"] {
            // Textual `\` join, NOT `Path::join`: this is the Windows branch,
            // and `Path::join` uses the HOST's separator — on a Linux or macOS
            // CI runner it built `C:\…\bin/sh.exe`, so the Windows resolution
            // tests could never pass there.
            let candidate = format!("{}\\{name}", dir.trim_end_matches(['\\', '/']));
            if exists(Path::new(&candidate)) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Shell builtins and keywords: always "runnable" because the shell itself
/// provides them, so `agent_program` must never report them as missing.
const SHELL_BUILTINS: &[&str] = &[
    ".", ":", "[", "cd", "echo", "eval", "exit", "export", "false", "for", "if", "printf", "read",
    "set", "source", "test", "true", "case", "while", "until",
];

/// SF-14: the program an agent command would execute — the first word a shell
/// would run, after any leading `NAME=value` assignments and the transparent
/// wrappers `exec` / `command` / `nohup`. Used to preflight
/// `sirius run --agent-cmd` before a fleet spawns workers that cannot start.
///
/// Deliberately conservative: anything a static read cannot pin down — a
/// subshell, `$(…)` or backticks, a variable, a `{model}`-style template, a
/// wrapper with its own flags, a shell builtin — returns `None`, and the caller
/// then SKIPS the preflight. A false "not found" would refuse a perfectly good
/// fleet, which is worse than the bug this guards against.
pub fn agent_program(cmd: &str) -> Option<String> {
    let mut rest = cmd;
    loop {
        let (tok, after) = first_word(rest)?;
        rest = after;
        let is_assignment = tok.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if is_assignment || matches!(tok.as_str(), "exec" | "command" | "nohup") {
            continue;
        }
        let dynamic = tok.is_empty()
            || tok.starts_with(['(', '-', '!'])
            || tok.contains(['$', '`', '{', '}', '*', '?', '<', '>', '|', '&', ';']);
        if dynamic || SHELL_BUILTINS.contains(&tok.as_str()) {
            return None;
        }
        // A PATH that is not absolute cannot be checked from here: `~` is the
        // shell's to expand, and a relative path resolves against the worker's
        // worktree (the repo root), not wherever sirius was launched. On Windows
        // an MSYS-style `/c/Users/...` is not absolute either (no drive) — the
        // POSIX sh that runs it maps it, `Path::is_file` cannot.
        let is_path = tok.contains('/') || (cfg!(windows) && tok.contains('\\'));
        if tok.starts_with('~') || (is_path && !Path::new(&tok).is_absolute()) {
            return None;
        }
        return Some(tok);
    }
}

/// Split off one shell word: a bare run up to whitespace, or a single- or
/// double-quoted run with the quotes stripped. `None` when nothing is left or a
/// quote is unterminated (too irregular to read statically).
fn first_word(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let quote = s.chars().next()?;
    if quote == '\'' || quote == '"' {
        let inner = &s[1..];
        let end = inner.find(quote)?;
        return Some((inner[..end].to_string(), &inner[end + 1..]));
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    Some((s[..end].to_string(), &s[end..]))
}

/// SF-14: where `program` resolves the way a shell would find it — a path is
/// checked as written, a bare name is searched on `PATH`. Pure over its inputs
/// (like [`resolve_shell_from`]) so both platforms are testable on either.
///
/// On Windows a bare name also tries every `PATHEXT` extension, and the
/// fallback list includes `.CMD` / `.BAT`: npm installs `claude` as
/// `claude.cmd`, so an `.exe`-only probe would report a working install as
/// missing and refuse the fleet.
pub fn find_program_from(
    program: &str,
    windows: bool,
    path_var: Option<&str>,
    pathext: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let exts: Vec<&str> = if windows {
        pathext
            .unwrap_or(".COM;.EXE;.BAT;.CMD")
            .split(';')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    // The name as written first, then each PATHEXT spelling of it.
    let probe = |base: &str| -> Option<PathBuf> {
        std::iter::once(base.to_string())
            .chain(exts.iter().map(|ext| format!("{base}{ext}")))
            .map(PathBuf::from)
            .find(|candidate| exists(candidate.as_path()))
    };
    if program.contains('/') || (windows && program.contains('\\')) {
        return probe(program);
    }
    // Joined textually with the TARGET platform's separator, not `Path::join`:
    // that uses the HOST's, so a Windows lookup tested on a Linux CI runner
    // would build `C:\dir/claude` and never match.
    let (list_sep, dir_sep) = if windows { (';', '\\') } else { (':', '/') };
    for dir in path_var?.split(list_sep) {
        let dir = dir.trim().trim_matches('"');
        if dir.is_empty() {
            continue;
        }
        let base = format!("{}{dir_sep}{program}", dir.trim_end_matches(['\\', '/']));
        if let Some(found) = probe(&base) {
            return Some(found);
        }
    }
    None
}

/// [`find_program_from`] against the real environment.
pub fn find_program(program: &str) -> Option<PathBuf> {
    find_program_from(
        program,
        cfg!(windows),
        std::env::var("PATH").ok().as_deref(),
        std::env::var("PATHEXT").ok().as_deref(),
        &|p: &Path| p.is_file(),
    )
}

/// Run `script` through `shell`. A spawn failure NAMES the shell: the old bare
/// "program not found" read as if the *test binary* were missing and sent the
/// SF-15 reporter looking in entirely the wrong place.
pub fn run_in_shell(
    runner: &dyn Runner,
    shell: &ShellCmd,
    script: &str,
) -> Result<CmdOutput, String> {
    runner
        .run(&shell.program, &[shell.flag.as_str(), script])
        .map_err(|e| {
            format!(
                "cannot start the SHELL `{}` that sirius runs gate.test_cmd through: {e} \
                 (this is the shell, not your test binary — the command `{}` was never \
                 reached). Install a POSIX sh, or set {SHELL_OVERRIDE_ENV} to a shell \
                 that exists.",
                shell.program,
                first_line_of(script),
            )
        })
}

fn first_line_of(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// A single programmed response for the mock runner. Test-only.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct MockResponse {
    /// Argv prefix that must match (program + leading args), e.g.
    /// `["amt", "claim"]`. An empty vec matches anything.
    pub match_prefix: Vec<String>,
    pub output: CmdOutput,
}

#[cfg(test)]
impl MockResponse {
    pub fn new(prefix: &[&str], code: i32, stdout: &str, stderr: &str) -> Self {
        MockResponse {
            match_prefix: prefix.iter().map(|s| s.to_string()).collect(),
            output: CmdOutput {
                code: Some(code),
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            },
        }
    }
}

/// Records calls and returns queued responses. Responses are matched by the
/// most-specific queued prefix; matching responses are consumed in FIFO order.
/// Test-only: the production binary uses [`RealRunner`].
#[cfg(test)]
#[derive(Default)]
pub struct MockRunner {
    responses: Mutex<Vec<MockResponse>>,
    calls: Mutex<VecDeque<Vec<String>>>,
    /// SIRF-7: controls how the next `run_agent` call behaves. When set, it
    /// fires `heartbeat` `beats` times first (so tests can assert lease renewal)
    /// then either returns normally or reports a timeout kill.
    agent_sim: Mutex<Option<AgentSim>>,
    /// SIRF-23: the environment of every `run_agent` call, in order, so tests
    /// can assert the agent contract.
    agent_envs: Mutex<Vec<Vec<(String, String)>>>,
    /// SIRF-23: scripted agent side effects keyed by `SIRIUS_PHASE`. Each
    /// entry is (env var naming the output path, file contents); a `run_agent`
    /// call in that phase pops one and writes it — how a test plays the
    /// reviewer (`SIRIUS_REVIEW_OUT`) or a fix-mode worker (`SIRIUS_FIX_OUT`).
    phase_writes: Mutex<std::collections::HashMap<String, VecDeque<(String, String)>>>,
    /// SIRF-23: phases whose NEXT `run_agent` call simulates a timeout kill.
    phase_timeouts: Mutex<Vec<String>>,
    /// Scripted agent STDOUT per phase, written to `opts.log_path` (what the
    /// real runner streams there) — how a test plays a reviewer that prints
    /// its findings instead of writing `$SIRIUS_REVIEW_OUT`.
    phase_stdout: Mutex<std::collections::HashMap<String, VecDeque<String>>>,
    /// SIRF-41: (hard cap, idle window, grace) of every `run_agent_watched`
    /// call, in order — how a test asserts the per-label limits arrived.
    watches: Mutex<Vec<(Duration, Duration, Duration)>>,
    /// SIRF-41: how many times the next `run_agent_watched` call invokes the
    /// caller's progress probe (0 = never, the default: a probe shells out
    /// to git, which would perturb tests that count recorded calls).
    probe_calls: Mutex<u32>,
    /// Everything the probe returned when it was invoked.
    probes: Mutex<Vec<Option<WorktreeProbe>>>,
}

#[cfg(test)]
#[derive(Clone)]
struct AgentSim {
    /// How many times to fire the heartbeat callback before returning.
    beats: u32,
    /// `Some(kind)` ⇒ report [`AgentOutcome::TimedOut`] with that kind;
    /// `None` ⇒ a normal exit.
    timeout: Option<TimeoutKind>,
}

#[cfg(test)]
impl MockRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a response for a given argv prefix.
    pub fn push(&self, resp: MockResponse) {
        self.responses.lock().unwrap().push(resp);
    }

    /// Convenience: queue a JSON stdout success/failure for an argv prefix.
    pub fn expect(&self, prefix: &[&str], code: i32, stdout: &str) -> &Self {
        self.push(MockResponse::new(prefix, code, stdout, ""));
        self
    }

    /// Every recorded call, as a flat `program arg arg ...` string, in order.
    pub fn recorded(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.join(" "))
            .collect()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// SIRF-7: arm the next `run_agent` call to simulate a timeout. It fires the
    /// heartbeat callback `beats` times (so lease-renewal is observable in
    /// tests) and then returns [`AgentOutcome::TimedOut`] without any real sleep.
    pub fn arm_agent_timeout(&self, beats: u32) -> &Self {
        *self.agent_sim.lock().unwrap() = Some(AgentSim {
            beats,
            timeout: Some(TimeoutKind::Hard),
        });
        self
    }

    /// SIRF-41: like [`MockRunner::arm_agent_timeout`], but the kill is the
    /// IDLE watchdog's (no output, no worktree change).
    pub fn arm_agent_idle_timeout(&self, beats: u32) -> &Self {
        *self.agent_sim.lock().unwrap() = Some(AgentSim {
            beats,
            timeout: Some(TimeoutKind::Idle),
        });
        self
    }

    /// SIRF-41: make the next `run_agent_watched` call invoke the caller's
    /// progress probe `n` times (results via [`MockRunner::probes`]).
    pub fn arm_probe_calls(&self, n: u32) -> &Self {
        *self.probe_calls.lock().unwrap() = n;
        self
    }

    /// SIRF-41: (hard cap, idle, grace) of every watched agent run so far.
    pub fn watches(&self) -> Vec<(Duration, Duration, Duration)> {
        self.watches.lock().unwrap().clone()
    }

    /// SIRF-41: every snapshot the progress probe returned.
    pub fn probes(&self) -> Vec<Option<WorktreeProbe>> {
        self.probes.lock().unwrap().clone()
    }

    /// SIRF-23: when the next `run_agent` call in `phase` happens, write
    /// `contents` to the path held by its env var `out_var`. Queued per phase,
    /// consumed FIFO.
    pub fn on_phase_write(&self, phase: &str, out_var: &str, contents: &str) -> &Self {
        self.phase_writes
            .lock()
            .unwrap()
            .entry(phase.to_string())
            .or_default()
            .push_back((out_var.to_string(), contents.to_string()));
        self
    }

    /// Script the next `run_agent` call in `phase` to print `text` (written
    /// to its log file, as the real runner captures stdout).
    pub fn on_phase_stdout(&self, phase: &str, text: &str) -> &Self {
        self.phase_stdout
            .lock()
            .unwrap()
            .entry(phase.to_string())
            .or_default()
            .push_back(text.to_string());
        self
    }

    /// SIRF-23: make the next `run_agent` call in `phase` time out (killed).
    pub fn arm_phase_timeout(&self, phase: &str) -> &Self {
        self.phase_timeouts.lock().unwrap().push(phase.to_string());
        self
    }

    /// SIRF-23: the env of every `run_agent` call so far, in order.
    pub fn agent_envs(&self) -> Vec<Vec<(String, String)>> {
        self.agent_envs.lock().unwrap().clone()
    }

    /// SIRF-7: fire the heartbeat `beats` times on a *normal* (non-timeout)
    /// agent return, so tests can assert periodic renewal on the happy path.
    pub fn arm_agent_heartbeats(&self, beats: u32) -> &Self {
        *self.agent_sim.lock().unwrap() = Some(AgentSim {
            beats,
            timeout: None,
        });
        self
    }
}

#[cfg(test)]
impl Runner for MockRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(program.to_string());
        argv.extend(args.iter().map(|s| s.to_string()));
        self.calls.lock().unwrap().push_back(argv.clone());

        let mut responses = self.responses.lock().unwrap();
        // Find the queued response whose prefix matches argv, preferring the
        // longest (most specific) prefix, then FIFO.
        let mut best: Option<usize> = None;
        let mut best_len = 0usize;
        for (i, r) in responses.iter().enumerate() {
            let matches = prefix_matches(&argv, &r.match_prefix);
            let more_specific = r.match_prefix.len() > best_len || best.is_none();
            if matches && r.match_prefix.len() >= best_len && more_specific {
                best = Some(i);
                best_len = r.match_prefix.len();
            }
        }
        if let Some(i) = best {
            return Ok(responses.remove(i).output);
        }
        // Unmatched calls default to a benign empty success so tests that only
        // assert on specific calls don't have to program every incidental one.
        Ok(CmdOutput {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    /// SIRF-7: simulate agent supervision. The underlying command is recorded
    /// via `run` (so argv assertions still work). If a sim was armed we fire the
    /// heartbeat callback the requested number of times and, when `timeout` is
    /// set, report a kill; otherwise we return the normal `run` output. No real
    /// clocks or sleeps are involved, so tests stay deterministic and fast.
    fn run_agent(
        &self,
        program: &str,
        args: &[&str],
        opts: &AgentRunOpts,
        heartbeat: &mut dyn FnMut(),
    ) -> std::io::Result<AgentOutcome> {
        self.agent_envs.lock().unwrap().push(opts.env.clone());
        // Play any scripted side effect for this phase (SIRF-23).
        let phase = opts
            .env
            .iter()
            .find(|(k, _)| k == "SIRIUS_PHASE")
            .map(|(_, v)| v.clone());
        if let Some(phase) = &phase {
            let mut timeouts = self.phase_timeouts.lock().unwrap();
            if let Some(i) = timeouts.iter().position(|p| p == phase) {
                timeouts.remove(i);
                drop(timeouts);
                let out = self.run(program, args)?;
                return Ok(AgentOutcome::TimedOut {
                    output: out,
                    kind: TimeoutKind::Hard,
                });
            }
        }
        if let Some(phase) = &phase {
            let printed = self
                .phase_stdout
                .lock()
                .unwrap()
                .get_mut(phase)
                .and_then(VecDeque::pop_front);
            if let (Some(text), Some(path)) = (printed, opts.log_path.as_ref()) {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, text)?;
            }
        }
        if let Some(phase) = phase {
            let next = self
                .phase_writes
                .lock()
                .unwrap()
                .get_mut(&phase)
                .and_then(VecDeque::pop_front);
            if let Some((var, contents)) = next {
                if let Some((_, path)) = opts.env.iter().find(|(k, _)| *k == var) {
                    std::fs::write(path, contents)?;
                }
            }
        }
        let out = self.run(program, args)?;
        let sim = self.agent_sim.lock().unwrap().take();
        match sim {
            Some(s) => {
                for _ in 0..s.beats {
                    heartbeat();
                }
                match s.timeout {
                    Some(kind) => Ok(AgentOutcome::TimedOut { output: out, kind }),
                    None => Ok(AgentOutcome::Exited(out)),
                }
            }
            None => Ok(AgentOutcome::Exited(out)),
        }
    }

    /// SIRF-41: record the limits, play any armed probe calls, then behave
    /// exactly like `run_agent` (no clocks, no sleeps).
    fn run_agent_watched(
        &self,
        program: &str,
        args: &[&str],
        opts: &AgentRunOpts,
        watch: &mut ProgressWatch<'_>,
        heartbeat: &mut dyn FnMut(),
    ) -> std::io::Result<AgentOutcome> {
        self.watches
            .lock()
            .unwrap()
            .push((opts.timeout, watch.idle, watch.grace));
        let n = std::mem::take(&mut *self.probe_calls.lock().unwrap());
        for _ in 0..n {
            let snap = (watch.probe)();
            self.probes.lock().unwrap().push(snap);
        }
        self.run_agent(program, args, opts, heartbeat)
    }
}

/// Prefix match with SHELL INVOCATIONS CANONICALIZED (SF-15). A test that
/// programs `["sh", "-c"]` is expressing "the shell, running a script" — not
/// "the literal seven-byte program `sh`". Since `resolve_shell` legitimately
/// yields `/bin/sh`, `C:\Program Files\Git\usr\bin\sh.exe`, or `cmd.exe /C`
/// depending on the machine, matching raw argv would make every mock-based
/// gate test pass or fail according to which shell the developer's box has —
/// exactly the machine-state dependence this crate's test suite forbids.
#[cfg(test)]
fn prefix_matches(argv: &[String], prefix: &[String]) -> bool {
    if prefix.len() > argv.len() {
        return false;
    }
    let a = canon_shell(argv);
    let p = canon_shell(prefix);
    p.iter().zip(a.iter()).all(|(p, a)| p == a)
}

/// If argv[0] is any known shell, rewrite it to `sh` and its script flag to
/// `-c`. Everything else is left byte-identical.
#[cfg(test)]
fn canon_shell(argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();
    let Some(first) = out.first() else {
        return out;
    };
    if !matches!(
        program_stem(first).as_str(),
        "sh" | "bash" | "dash" | "cmd" | "command"
    ) {
        return out;
    }
    out[0] = "sh".to_string();
    if out.len() > 1 && out[1].eq_ignore_ascii_case("/c") {
        out[1] = "-c".to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_matches_by_longest_prefix() {
        let m = MockRunner::new();
        m.expect(&["amt"], 0, "{\"generic\":true}");
        m.expect(&["amt", "claim"], 0, "{\"claimed\":false}");

        let out = m.run("amt", &["claim", "--json"]).unwrap();
        assert_eq!(out.stdout, "{\"claimed\":false}");
        // The generic one is still queued for a different amt call.
        let out2 = m.run("amt", &["issue", "show", "AMT-1"]).unwrap();
        assert_eq!(out2.stdout, "{\"generic\":true}");
    }

    // ---- shell resolution (SF-15) ------------------------------------------

    /// A fake filesystem: only the listed paths exist.
    fn only<'a>(paths: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        move |p: &Path| {
            let s = p.to_string_lossy().to_ascii_lowercase();
            paths.iter().any(|q| q.to_ascii_lowercase() == s)
        }
    }

    #[test]
    fn unix_shell_is_absolute_bin_sh() {
        // Absolute, never `sh` off PATH: a service can be started PATH-less.
        let s = resolve_shell_from(None, false, Some("/usr/bin:/bin"), None, &only(&[]));
        assert_eq!(s, ShellCmd::posix_sh());
        assert_eq!(s.describe(), "/bin/sh -c");
    }

    // SF-15: from Git Bash `sh` is on PATH, so the POSIX-written test_cmd runs
    // in a POSIX shell and `&&` means what the config author meant.
    #[test]
    fn windows_prefers_a_real_posix_sh_on_path() {
        let sh = r"C:\Program Files\Git\usr\bin\sh.exe";
        let s = resolve_shell_from(
            None,
            true,
            Some(r"C:\WINDOWS\system32;C:\Program Files\Git\usr\bin"),
            Some("cmd.exe"),
            &only(&[sh]),
        );
        assert!(s.posix, "a POSIX sh must be preferred: {s:?}");
        assert_eq!(s.program, sh, "resolved to an absolute path");
        assert_eq!(s.flag, "-c");
    }

    // The PowerShell half of the SF-15 report: no `sh` anywhere on PATH. The
    // old code spawned `sh` regardless and died; now we fall back to a shell
    // that exists and still understands `&&`.
    #[test]
    fn windows_without_sh_falls_back_to_comspec() {
        let s = resolve_shell_from(
            None,
            true,
            Some(r"C:\WINDOWS\system32"),
            Some(r"C:\WINDOWS\system32\cmd.exe"),
            &only(&[]),
        );
        assert!(!s.posix);
        assert_eq!(s.program, r"C:\WINDOWS\system32\cmd.exe");
        assert_eq!(s.flag, "/C");
    }

    #[test]
    fn windows_without_sh_or_comspec_still_names_cmd() {
        let s = resolve_shell_from(None, true, None, None, &only(&[]));
        assert_eq!(s.program, "cmd.exe");
        assert_eq!(s.flag, "/C");
    }

    // `C:\Windows\System32\bash.exe` is the WSL launcher. Picking it up would
    // run the suite inside a Linux VM against a translated path — a different
    // machine from the checkout. Only `sh` is ever probed by name.
    #[test]
    fn windows_never_auto_selects_the_wsl_bash_launcher() {
        let wsl = r"C:\WINDOWS\system32\bash.exe";
        let s = resolve_shell_from(
            None,
            true,
            Some(r"C:\WINDOWS\system32"),
            Some("cmd.exe"),
            &only(&[wsl]),
        );
        assert!(!s.program.to_ascii_lowercase().contains("bash"), "{s:?}");
    }

    #[test]
    fn override_wins_and_infers_its_flag() {
        let bash = resolve_shell_from(
            Some(r"C:\Program Files\Git\bin\bash.exe"),
            true,
            None,
            None,
            &only(&[]),
        );
        assert_eq!(bash.flag, "-c");
        assert!(bash.posix);
        // cmd.exe named explicitly still gets /C, not -c.
        let cmd = resolve_shell_from(Some("cmd.exe"), false, None, None, &only(&[]));
        assert_eq!(cmd.flag, "/C");
        assert!(!cmd.posix);
        // Blank/whitespace override is ignored, not spawned.
        let blank = resolve_shell_from(Some("   "), false, None, None, &only(&[]));
        assert_eq!(blank, ShellCmd::posix_sh());
    }

    // SF-15(b): the spawn failure must NAME THE SHELL. The original message
    // read like the test binary was missing and sent the reporter hunting in
    // the wrong place entirely.
    #[test]
    fn shell_spawn_failure_names_the_shell_not_the_test_binary() {
        struct Boom;
        impl Runner for Boom {
            fn run(&self, _p: &str, _a: &[&str]) -> std::io::Result<CmdOutput> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "program not found",
                ))
            }
        }
        let shell = ShellCmd {
            program: "sh".into(),
            flag: "-c".into(),
            posix: true,
        };
        let err =
            run_in_shell(&Boom, &shell, "cargo test --workspace && bun run check").unwrap_err();
        assert!(err.contains("SHELL `sh`"), "{err}");
        assert!(err.contains("not your test binary"), "{err}");
        assert!(err.contains(SHELL_OVERRIDE_ENV), "{err}");
    }

    #[test]
    fn run_in_shell_passes_the_script_through_the_flag() {
        let m = MockRunner::new();
        m.expect(&["sh", "-c"], 0, "ok");
        let shell = ShellCmd {
            program: r"C:\WINDOWS\system32\cmd.exe".into(),
            flag: "/C".into(),
            posix: false,
        };
        let out = run_in_shell(&m, &shell, "cargo test").unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, "ok");
        // Recorded verbatim — canonicalization is for MATCHING only.
        assert_eq!(
            m.recorded()[0],
            r"C:\WINDOWS\system32\cmd.exe /C cargo test"
        );
    }

    // The mock must key on "a shell running a script", never on which shell
    // this developer's machine happens to have.
    #[test]
    fn mock_matches_any_resolved_shell_against_an_sh_prefix() {
        for shell in [
            ShellCmd::posix_sh(),
            ShellCmd::for_program(r"C:\Program Files\Git\usr\bin\sh.exe"),
            ShellCmd::for_program("cmd.exe"),
        ] {
            let m = MockRunner::new();
            m.expect(&["sh", "-c"], 7, "suite output");
            let out = run_in_shell(&m, &shell, "cargo test").unwrap();
            assert_eq!(out.code, Some(7), "did not match for {shell:?}");
        }
    }

    #[test]
    fn mock_records_calls() {
        let m = MockRunner::new();
        let _ = m.run("hayven", &["query", "add", "--json"]).unwrap();
        assert_eq!(m.recorded(), vec!["hayven query add --json".to_string()]);
        assert_eq!(m.call_count(), 1);
    }

    #[test]
    fn unmatched_call_is_benign_success() {
        let m = MockRunner::new();
        let out = m.run("amt", &["whatever"]).unwrap();
        assert!(out.success());
    }

    #[test]
    fn mock_run_agent_simulates_normal_return_with_heartbeats() {
        // SIRF-7: an armed non-timeout sim fires the heartbeat N times and
        // returns the underlying command's output as an Exited outcome.
        let m = MockRunner::new();
        m.expect(&["sh", "-c"], 0, "done");
        m.arm_agent_heartbeats(3);
        let opts = AgentRunOpts {
            timeout: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(1),
            log_path: None,
            env: vec![],
        };
        let mut beats = 0;
        let mut hb = || beats += 1;
        let outcome = m.run_agent("sh", &["-c", "x"], &opts, &mut hb).unwrap();
        assert_eq!(beats, 3);
        assert!(outcome.success());
        assert!(!outcome.timed_out());
        assert_eq!(outcome.output().stdout, "done");
    }

    #[test]
    fn mock_run_agent_simulates_timeout() {
        // SIRF-7: an armed timeout fires beats then reports TimedOut (a failure).
        let m = MockRunner::new();
        m.expect(&["sh", "-c"], 0, "");
        m.arm_agent_timeout(2);
        let opts = AgentRunOpts {
            timeout: Duration::from_secs(1),
            heartbeat_interval: Duration::from_secs(1),
            log_path: None,
            env: vec![],
        };
        let mut beats = 0;
        let mut hb = || beats += 1;
        let outcome = m
            .run_agent("sh", &["-c", "sleep 999"], &opts, &mut hb)
            .unwrap();
        assert_eq!(beats, 2);
        assert!(outcome.timed_out());
        assert!(!outcome.success());
    }

    /// Pick the script a REAL spawn should use for the shell we actually
    /// resolved. These three tests spawn a live child, and they used to hardcode
    /// `sh` with POSIX scripts — which is the SF-15 bug reproduced inside our
    /// own suite: they passed from Git Bash and failed from PowerShell on the
    /// same commit. There is no one script both families understand, so the
    /// equivalent is written out per family.
    fn script(shell: &ShellCmd, posix: &str, cmd: &str) -> String {
        if shell.posix { posix } else { cmd }.to_string()
    }

    #[test]
    #[cfg(unix)]
    fn a_timeout_kills_the_childs_whole_tree() {
        // SIRF-32 review F9: a server an integration cmd started must not
        // outlive the timeout (it would hold its port for the next run).
        let dir = std::env::temp_dir().join(format!("sirius-tree-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let pidfile = dir.join("bg.pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let r = RealRunner::default();
        let opts = AgentRunOpts {
            timeout: Duration::from_millis(1500),
            heartbeat_interval: Duration::from_millis(100),
            log_path: None,
            env: vec![],
        };
        let out = r
            .run_agent("sh", &["-c", &script], &opts, &mut || {})
            .unwrap();
        assert!(out.timed_out());
        let bg = std::fs::read_to_string(&pidfile).unwrap();
        // A SIGKILLed child can linger as a zombie until reaped: poll, and
        // count a zombie as dead.
        let mut alive = true;
        for _ in 0..40 {
            alive = Command::new("ps")
                .args(["-o", "stat=", "-p", bg.trim()])
                .output()
                .map(|o| {
                    let st = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    o.status.success() && !st.is_empty() && !st.starts_with('Z')
                })
                .unwrap_or(false);
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive, "the background child {bg} survived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_runner_kills_and_times_out_a_hung_child() {
        // SIRF-7: a real hung command must be killed at the timeout, not waited
        // on forever, and the heartbeat must have fired at least once. Uses a
        // sub-second timeout so the test stays fast.
        let r = RealRunner::default();
        let opts = AgentRunOpts {
            timeout: Duration::from_millis(200),
            heartbeat_interval: Duration::from_millis(50),
            log_path: None,
            env: vec![],
        };
        let mut beats = 0;
        let mut hb = || beats += 1;
        let start = Instant::now();
        let sh = resolve_shell();
        // `ping -n 31` is cmd.exe's sleep: 30 one-second waits.
        let hang = script(&sh, "sleep 30", "ping -n 31 127.0.0.1 >nul");
        let outcome = r
            .run_agent(&sh.program, &[sh.flag.as_str(), &hang], &opts, &mut hb)
            .unwrap();
        // Killed well before the 30s sleep would end.
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(outcome.timed_out());
        assert!(!outcome.success());
        assert!(beats >= 1, "heartbeat should fire while the child runs");
    }

    #[test]
    fn real_runner_captures_output_and_writes_log() {
        // SIRF-7: a fast command exits normally and both streams land in the
        // durable log (previously the output vanished on the success path).
        // Output now streams straight to the file, so the AgentOutcome carries
        // only the exit code — the log is the source of truth.
        let r = RealRunner::default();
        let log = std::env::temp_dir().join(format!("sirius-agentlog-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let opts = AgentRunOpts {
            timeout: Duration::from_secs(10),
            heartbeat_interval: Duration::from_secs(1),
            log_path: Some(log.clone()),
            env: vec![],
        };
        let mut hb = || {};
        let sh = resolve_shell();
        let two_streams = script(&sh, "echo hi; echo boom 1>&2", "echo hi& echo boom 1>&2");
        let outcome = r
            .run_agent(
                &sh.program,
                &[sh.flag.as_str(), &two_streams],
                &opts,
                &mut hb,
            )
            .unwrap();
        assert!(outcome.success());
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(written.contains("hi")); // stdout
        assert!(written.contains("boom")); // stderr
        assert!(written.contains("agent exit: 0")); // exit trailer
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn real_runner_streams_large_output_without_deadlock() {
        // Regression guard: an agent that emits far more than the OS pipe buffer
        // (~64 KB) must still exit cleanly. The old pipe+drain-after-exit design
        // deadlocked here — the child blocked on a full pipe, never exited, and
        // the poll loop spun until the timeout. The generous 15s timeout means a
        // regression surfaces as a TimedOut assertion failure, not a hung suite.
        let r = RealRunner::default();
        let log = std::env::temp_dir().join(format!("sirius-biglog-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let opts = AgentRunOpts {
            timeout: Duration::from_secs(15),
            heartbeat_interval: Duration::from_secs(100),
            log_path: Some(log.clone()),
            env: vec![],
        };
        let mut hb = || {};
        let sh = resolve_shell();
        // ~200 KB to stdout (>> any pipe buffer). The cmd.exe form emits 3200
        // lines of 62 chars + CRLF = 204800 bytes.
        let flood = script(
            &sh,
            "yes sirius | head -c 200000",
            "for /L %i in (1,1,3200) do @echo              sirius-sirius-sirius-sirius-sirius-sirius-sirius-sirius-sirius",
        );
        let outcome = r
            .run_agent(&sh.program, &[sh.flag.as_str(), &flood], &opts, &mut hb)
            .unwrap();
        assert!(
            !outcome.timed_out(),
            "large output must not deadlock/timeout"
        );
        assert!(outcome.success());
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(
            written.len() >= 200_000,
            "streamed log should hold the output"
        );
        let _ = std::fs::remove_file(&log);
    }

    // ---- progress-aware supervision (SIRF-41) -------------------------------

    fn opts_for(hard_ms: u64, log: Option<PathBuf>) -> AgentRunOpts {
        AgentRunOpts {
            timeout: Duration::from_millis(hard_ms),
            heartbeat_interval: Duration::from_millis(50),
            log_path: log,
            env: vec![],
        }
    }

    /// A probe that never sees a change — isolates the log-growth signal.
    fn still() -> Option<WorktreeProbe> {
        Some(WorktreeProbe {
            head: "h".into(),
            tree: 0,
        })
    }

    /// A throwaway git repo with one commit; commits made in it never sign or
    /// run hooks, whatever the developer's global git config says.
    #[cfg(unix)]
    fn temp_repo(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sirius-watch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .current_dir(&d)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--no-verify",
            "--allow-empty",
            "-m",
            "init",
        ]);
        d
    }

    /// The commit a stub agent makes, as a shell fragment.
    #[cfg(unix)]
    const COMMIT: &str = "git -c user.name=t -c user.email=t@t -c commit.gpgsign=false \
                          commit -q --no-verify --allow-empty -m done";

    // The ticket's first "done when": an agent that keeps working past the
    // old wall clock is NOT killed — until the hard cap, which still holds.
    #[test]
    #[cfg(unix)]
    fn a_child_that_keeps_printing_survives_idle_and_dies_only_at_the_hard_cap() {
        let log = std::env::temp_dir().join(format!("sirius-chatty-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let r = RealRunner::default();
        let mut probe = still;
        let mut watch = ProgressWatch {
            idle: Duration::from_millis(400),
            probe_every: Duration::from_millis(100),
            grace: Duration::from_millis(500),
            probe: &mut probe,
        };
        let start = Instant::now();
        let out = r
            .run_agent_watched(
                "sh",
                &["-c", "while :; do echo tick; sleep 0.05; done"],
                &opts_for(1500, Some(log.clone())),
                &mut watch,
                &mut || {},
            )
            .unwrap();
        let took = start.elapsed();
        assert_eq!(out.timeout_kind(), Some(TimeoutKind::Hard), "{out:?}");
        assert!(
            took >= Duration::from_millis(1450),
            "killed early: {took:?}"
        );
        assert!(took < Duration::from_secs(10), "{took:?}");
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(written.contains("hard cap reached"), "{written}");
        let _ = std::fs::remove_file(&log);
    }

    // The other half: a silent agent that changes nothing is killed at the
    // idle window, long before the hard cap.
    #[test]
    #[cfg(unix)]
    fn a_silent_child_is_killed_at_the_idle_window() {
        let log = std::env::temp_dir().join(format!("sirius-silent-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let r = RealRunner::default();
        let mut probe = still;
        let mut watch = ProgressWatch {
            idle: Duration::from_millis(300),
            probe_every: Duration::from_millis(100),
            grace: Duration::from_millis(500),
            probe: &mut probe,
        };
        let start = Instant::now();
        let out = r
            .run_agent_watched(
                "sh",
                &["-c", "sleep 30"],
                &opts_for(20_000, Some(log.clone())),
                &mut watch,
                &mut || {},
            )
            .unwrap();
        let took = start.elapsed();
        assert_eq!(out.timeout_kind(), Some(TimeoutKind::Idle), "{out:?}");
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(
            took < Duration::from_secs(5),
            "not killed at idle: {took:?}"
        );
        assert!(std::fs::read_to_string(&log)
            .unwrap()
            .contains("idle timeout"));
        let _ = std::fs::remove_file(&log);
    }

    // Progress without a word of output: the worktree keeps changing, so the
    // REAL probe (git status + stat) must keep the idle watchdog off.
    #[test]
    #[cfg(unix)]
    fn a_silent_child_that_keeps_editing_files_is_not_idle() {
        let repo = temp_repo("edits");
        let r = RealRunner {
            cwd: Some(repo.clone()),
        };
        let probe_runner = r.clone();
        let mut probe = || worktree_probe(&probe_runner);
        let mut watch = ProgressWatch {
            idle: Duration::from_millis(600),
            probe_every: Duration::from_millis(100),
            grace: Duration::from_millis(100),
            probe: &mut probe,
        };
        let start = Instant::now();
        let out = r
            .run_agent_watched(
                "sh",
                &[
                    "-c",
                    "i=0; while :; do i=$((i+1)); echo $i > scratch.txt; sleep 0.1; done",
                ],
                &opts_for(2000, None),
                &mut watch,
                &mut || {},
            )
            .unwrap();
        let took = start.elapsed();
        assert_eq!(out.timeout_kind(), Some(TimeoutKind::Hard), "{out:?}");
        assert!(
            took >= Duration::from_millis(1950),
            "killed early: {took:?}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    // SIRF-41 #4, the MSX-80 shape: the agent commits shortly before the cap
    // and is finishing up. It gets the grace and exits on its own.
    #[test]
    #[cfg(unix)]
    fn a_child_that_commits_near_the_deadline_gets_the_grace() {
        let repo = temp_repo("grace");
        let r = RealRunner {
            cwd: Some(repo.clone()),
        };
        let probe_runner = r.clone();
        let mut probe = || worktree_probe(&probe_runner);
        let mut watch = ProgressWatch {
            idle: Duration::from_secs(20),
            probe_every: Duration::from_millis(100),
            grace: Duration::from_millis(2000),
            probe: &mut probe,
        };
        let script = format!("sleep 0.3; {COMMIT}; sleep 1.5; exit 0");
        let start = Instant::now();
        let out = r
            .run_agent_watched(
                "sh",
                &["-c", &script],
                &opts_for(1500, None),
                &mut watch,
                &mut || {},
            )
            .unwrap();
        let took = start.elapsed();
        assert!(out.success(), "the grace was not granted: {out:?}");
        assert!(took >= Duration::from_millis(1500), "{took:?}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    // The grace is granted ONCE: an agent that commits and then hangs is
    // still killed — one grace after the cap, as a hard-cap kill.
    #[test]
    #[cfg(unix)]
    fn the_finish_grace_is_granted_only_once() {
        let repo = temp_repo("grace-once");
        let r = RealRunner {
            cwd: Some(repo.clone()),
        };
        let probe_runner = r.clone();
        let mut probe = || worktree_probe(&probe_runner);
        let mut watch = ProgressWatch {
            idle: Duration::from_secs(20),
            probe_every: Duration::from_millis(100),
            grace: Duration::from_millis(800),
            probe: &mut probe,
        };
        let script = format!("sleep 0.3; {COMMIT}; sleep 30");
        let start = Instant::now();
        let out = r
            .run_agent_watched(
                "sh",
                &["-c", &script],
                &opts_for(1000, None),
                &mut watch,
                &mut || {},
            )
            .unwrap();
        let took = start.elapsed();
        assert_eq!(out.timeout_kind(), Some(TimeoutKind::Hard), "{out:?}");
        assert!(took >= Duration::from_millis(1750), "no grace: {took:?}");
        assert!(took < Duration::from_secs(10), "{took:?}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    // A commit long before the deadline is not "finishing": no grace.
    #[test]
    #[cfg(unix)]
    fn an_old_commit_earns_no_grace() {
        let repo = temp_repo("grace-old");
        let r = RealRunner {
            cwd: Some(repo.clone()),
        };
        let probe_runner = r.clone();
        let mut probe = || worktree_probe(&probe_runner);
        let mut watch = ProgressWatch {
            idle: Duration::from_secs(20),
            probe_every: Duration::from_millis(100),
            grace: Duration::from_millis(1000),
            probe: &mut probe,
        };
        let script = format!("{COMMIT}; sleep 30");
        let start = Instant::now();
        let out = r
            .run_agent_watched(
                "sh",
                &["-c", &script],
                &opts_for(2500, None),
                &mut watch,
                &mut || {},
            )
            .unwrap();
        let took = start.elapsed();
        assert_eq!(out.timeout_kind(), Some(TimeoutKind::Hard), "{out:?}");
        assert!(
            took < Duration::from_millis(2500 + 900),
            "an old commit got the grace: {took:?}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    // The probe must see an edit to an ALREADY-dirty file (porcelain alone
    // says ` M a.txt` both times) and a commit (HEAD moves).
    #[test]
    #[cfg(unix)]
    fn worktree_probe_sees_re_edits_and_commits() {
        let repo = temp_repo("probe");
        let r = RealRunner {
            cwd: Some(repo.clone()),
        };
        std::fs::write(repo.join("a.txt"), "1\n").unwrap();
        let p1 = worktree_probe(&r).unwrap();
        std::fs::write(repo.join("a.txt"), "22\n").unwrap();
        let p2 = worktree_probe(&r).unwrap();
        assert_ne!(p1.tree, p2.tree, "a re-edit must register");
        assert_eq!(p1.head, p2.head);
        assert_eq!(worktree_probe(&r).unwrap(), p2, "stable when idle");
        let ok = Command::new("sh")
            .current_dir(&repo)
            .args(["-c", COMMIT])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert_ne!(worktree_probe(&r).unwrap().head, p2.head);
        // Not a repo: no snapshot (and never counts as progress).
        let bare = std::env::temp_dir().join(format!("sirius-norepo-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&bare);
        let none = RealRunner {
            cwd: Some(bare.clone()),
        };
        assert_eq!(worktree_probe(&none), None);
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&repo);
    }

    // Reviewer/integration/canary runs pass no watch: their wall clock is
    // reported as a HARD kill.
    #[test]
    #[cfg(unix)]
    fn an_unwatched_timeout_is_a_hard_kill() {
        let r = RealRunner::default();
        let out = r
            .run_agent("sh", &["-c", "sleep 30"], &opts_for(200, None), &mut || {})
            .unwrap();
        assert_eq!(out.timeout_kind(), Some(TimeoutKind::Hard));
    }

    #[test]
    fn mock_watched_run_records_limits_and_plays_the_probe() {
        let m = MockRunner::new();
        m.arm_probe_calls(2).arm_agent_idle_timeout(1);
        let mut calls = 0;
        let mut probe = || {
            calls += 1;
            still()
        };
        let mut watch = ProgressWatch {
            idle: Duration::from_secs(900),
            probe_every: Duration::from_secs(30),
            grace: Duration::from_secs(60),
            probe: &mut probe,
        };
        let out = m
            .run_agent_watched(
                "sh",
                &["-c", "x"],
                &opts_for(5000, None),
                &mut watch,
                &mut || {},
            )
            .unwrap();
        assert_eq!(out.timeout_kind(), Some(TimeoutKind::Idle));
        assert_eq!(calls, 2);
        assert_eq!(m.probes().len(), 2);
        assert_eq!(
            m.watches(),
            vec![(
                Duration::from_millis(5000),
                Duration::from_secs(900),
                Duration::from_secs(60)
            )]
        );
        // Unarmed: the probe is never called (no stray git calls in tests).
        let mut probe = || -> Option<WorktreeProbe> { panic!("probed") };
        let mut watch = ProgressWatch {
            idle: Duration::from_secs(1),
            probe_every: Duration::from_secs(1),
            grace: Duration::from_secs(1),
            probe: &mut probe,
        };
        let out = m
            .run_agent_watched(
                "sh",
                &["-c", "x"],
                &opts_for(5000, None),
                &mut watch,
                &mut || {},
            )
            .unwrap();
        assert!(out.success());
    }

    #[test]
    fn timeout_kinds_describe_themselves() {
        assert_eq!(TimeoutKind::Idle.as_str(), "idle");
        assert_eq!(TimeoutKind::Hard.as_str(), "hard");
        assert_eq!(
            TimeoutKind::Idle.describe(1800, 10800),
            "agent idle for 1800s — no output, no file changes"
        );
        assert_eq!(
            TimeoutKind::Hard.describe(1800, 10800),
            "hit the hard cap of 10800s"
        );
    }

    // ---- SIRF-52: helper agents killed by the CLI's background-wait ceiling --

    #[test]
    fn bg_task_kill_is_detected_at_the_end_of_the_log_only() {
        let killed = "did the work\nresult: ok\nBackground tasks still running after 600s; terminating. Set CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0 to wait indefinitely.\n\n[sirius] agent exit: 0\n";
        assert!(bg_tasks_killed(killed));
        // Quoted mid-run, then lots of real work after: not the CLI's exit.
        let mut quoted = String::from(
            "reading SIRF-52: Background tasks still running after 600s; terminating.\n",
        );
        for i in 0..50 {
            quoted.push_str(&format!("step {i}\n"));
        }
        assert!(!bg_tasks_killed(&quoted));
        assert!(!bg_tasks_killed("all done\n[sirius] agent exit: 0\n"));
        // From a file, via its tail; a missing file is simply "no".
        let p = std::env::temp_dir().join(format!("sirius-bg-{}.log", std::process::id()));
        std::fs::write(&p, format!("{}{killed}", "x".repeat(100_000))).unwrap();
        assert!(bg_tasks_killed_in(Some(&p)));
        let _ = std::fs::remove_file(&p);
        assert!(!bg_tasks_killed_in(Some(&p)));
        assert!(!bg_tasks_killed_in(None));
    }

    // ---- agent-command preflight (SF-14) -----------------------------------

    /// The documented launch, and the shapes it is commonly varied into.
    #[test]
    fn agent_program_reads_the_program_a_shell_would_run() {
        let p = |c: &str| agent_program(c);
        assert_eq!(
            p(r#"claude -p "work the claimed issue""#).as_deref(),
            Some("claude")
        );
        assert_eq!(p("  claude").as_deref(), Some("claude"));
        // Leading assignments and transparent wrappers are not the program.
        assert_eq!(p("FOO=1 BAR=x claude -p go").as_deref(), Some("claude"));
        assert_eq!(p("exec nohup claude -p go").as_deref(), Some("claude"));
        // A quoted path with spaces is one word, quotes stripped.
        assert_eq!(
            p(r#""C:\Program Files\Claude\claude.exe" -p go"#).as_deref(),
            Some(r"C:\Program Files\Claude\claude.exe")
        );
    }

    /// Anything a static read cannot pin down must SKIP the preflight (None),
    /// never be reported missing: refusing a good fleet is worse than the bug.
    #[test]
    fn agent_program_declines_what_it_cannot_read_statically() {
        for cmd in [
            "$AGENT -p go",
            "$(which claude) -p go",
            "`which claude` -p go",
            "{model}-runner go",
            "(cd sub && claude -p go)",
            "true",
            "echo hi && claude",
            "",
            "   ",
            r#""unterminated -p go"#,
            // Paths only the shell (or the worktree cwd) can resolve.
            "~/bin/claude -p go",
            "./scripts/agent.sh go",
            "scripts/agent.sh go",
        ] {
            assert_eq!(
                agent_program(cmd),
                None,
                "should skip the preflight: {cmd:?}"
            );
        }
    }

    /// An absolute path is still checked (it means the same thing everywhere).
    #[test]
    fn agent_program_checks_an_absolute_path() {
        let abs = if cfg!(windows) {
            r"C:\bin\claude.exe -p go"
        } else {
            "/usr/local/bin/claude -p go"
        };
        let want = abs.split(' ').next().unwrap();
        assert_eq!(agent_program(abs).as_deref(), Some(want));
    }

    /// cmd.exe gets the script raw inside one outer quote pair that `/S`
    /// strips — so the script's own quotes reach the program intact.
    #[test]
    fn cmd_exe_script_is_wrapped_for_slash_s() {
        assert_eq!(
            cmd_exe_raw_args(r#"claude -p "work the claimed issue""#),
            r#"/S /C "claude -p "work the claimed issue"""#
        );
        assert!(is_cmd_exe(r"C:\Windows\system32\cmd.exe"));
        assert!(is_cmd_exe("CMD"));
        assert!(!is_cmd_exe("/usr/bin/sh"));
    }

    /// The SF-14 field case: npm installs `claude` on Windows as `claude.cmd`,
    /// so the lookup must honour PATHEXT's .CMD — an .exe-only probe would call
    /// a working install missing and refuse the fleet.
    #[test]
    fn find_program_honours_pathext_cmd_shims_on_windows() {
        let shim = r"C:\Users\u\AppData\Roaming\npm\claude.CMD";
        let path = r"C:\WINDOWS\system32;C:\Users\u\AppData\Roaming\npm";
        let found = find_program_from("claude", true, Some(path), None, &only(&[shim]));
        assert_eq!(found, Some(PathBuf::from(shim)));
        // An explicit PATHEXT is respected verbatim.
        let found = find_program_from("claude", true, Some(path), Some(".EXE"), &only(&[shim]));
        assert_eq!(
            found, None,
            "PATHEXT without .CMD must not find a .CMD shim"
        );
    }

    #[test]
    fn find_program_searches_path_in_order_on_unix() {
        let found = find_program_from(
            "claude",
            false,
            Some("/usr/bin:/opt/agents/bin"),
            None,
            &only(&["/opt/agents/bin/claude"]),
        );
        assert_eq!(found, Some(PathBuf::from("/opt/agents/bin/claude")));
        // No PATHEXT games off Windows: `claude.exe` is not `claude`.
        let found = find_program_from(
            "claude",
            false,
            Some("/bin"),
            None,
            &only(&["/bin/claude.exe"]),
        );
        assert_eq!(found, None);
    }

    /// The Claude Code desktop/web case this ticket is about: nothing on PATH.
    #[test]
    fn find_program_reports_a_missing_program() {
        let path = r"C:\WINDOWS\system32;C:\Program Files\Git\cmd";
        assert_eq!(
            find_program_from("claude", true, Some(path), None, &only(&[])),
            None
        );
        assert_eq!(
            find_program_from("claude", true, None, None, &only(&[])),
            None
        );
    }

    /// A path is checked as written (plus PATHEXT on Windows), never searched.
    #[test]
    fn find_program_checks_an_explicit_path_directly() {
        // PATHEXT's own casing: the probe returns the spelling it built, and
        // PathBuf equality is case-sensitive even on Windows.
        let exe = r"C:\tools\claude.EXE";
        let found = find_program_from(
            r"C:\tools\claude",
            true,
            Some(r"C:\other"),
            None,
            &only(&[exe]),
        );
        assert_eq!(found, Some(PathBuf::from(exe)));
        let found = find_program_from(
            "./bin/agent",
            false,
            Some("/usr/bin"),
            None,
            &only(&["./bin/agent"]),
        );
        assert_eq!(found, Some(PathBuf::from("./bin/agent")));
    }
}
