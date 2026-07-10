//! The shell boundary.
//!
//! Every call Sirius makes into `amt` or `hayven` goes through a [`Runner`]. In
//! production that is [`RealRunner`], which spawns a subprocess. In tests it is
//! [`MockRunner`], which returns canned output keyed by the argv prefix, so the
//! whole binary is testable offline. Sirius NEVER opens the parent SQLite for
//! writing — this seam is the only write path to the parents (via their CLIs).

use std::path::PathBuf;
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

    /// The exit code, defaulting to 1 when the process was signal-killed.
    pub fn code_or_err(&self) -> i32 {
        self.code.unwrap_or(1)
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
}

/// The result of supervising an agent command (SIRF-7).
#[derive(Debug, Clone)]
pub enum AgentOutcome {
    /// The child exited on its own; carries its captured output.
    Exited(CmdOutput),
    /// The `timeout` elapsed and the child was killed. `output` holds whatever
    /// was captured before the kill.
    TimedOut { output: CmdOutput },
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
            AgentOutcome::TimedOut { output } => output,
        }
    }

    pub fn timed_out(&self) -> bool {
        matches!(self, AgentOutcome::TimedOut { .. })
    }
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
}

/// Spawns real subprocesses.
#[derive(Debug, Default, Clone)]
pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        let out = Command::new(program).args(args).output()?;
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

        let mut child = Command::new(program)
            .args(args)
            .stdout(stdout_cfg)
            .stderr(stderr_cfg)
            .spawn()?;

        // Poll on a tick short enough to stay responsive to the timeout, but
        // never longer than the heartbeat interval.
        let tick = opts
            .heartbeat_interval
            .min(Duration::from_millis(500))
            .max(Duration::from_millis(10));
        let start = Instant::now();
        let mut last_beat = Instant::now();

        loop {
            match child.try_wait()? {
                Some(status) => {
                    append_exit_trailer(opts.log_path.as_deref(), status.code());
                    return Ok(AgentOutcome::Exited(CmdOutput {
                        code: status.code(),
                        stdout: String::new(),
                        stderr: String::new(),
                    }));
                }
                None => {
                    if start.elapsed() >= opts.timeout {
                        // Hung or over-budget: kill, reap, and report a timeout.
                        let _ = child.kill();
                        let code = child.wait().ok().and_then(|s| s.code());
                        append_exit_trailer(opts.log_path.as_deref(), code);
                        return Ok(AgentOutcome::TimedOut {
                            output: CmdOutput {
                                code,
                                stdout: String::new(),
                                stderr: String::new(),
                            },
                        });
                    }
                    if last_beat.elapsed() >= opts.heartbeat_interval {
                        heartbeat();
                        last_beat = Instant::now();
                    }
                    std::thread::sleep(tick);
                }
            }
        }
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
}

#[cfg(test)]
#[derive(Clone)]
struct AgentSim {
    /// How many times to fire the heartbeat callback before returning.
    beats: u32,
    /// True ⇒ report [`AgentOutcome::TimedOut`]; false ⇒ a normal exit.
    timeout: bool,
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
            timeout: true,
        });
        self
    }

    /// SIRF-7: fire the heartbeat `beats` times on a *normal* (non-timeout)
    /// agent return, so tests can assert periodic renewal on the happy path.
    pub fn arm_agent_heartbeats(&self, beats: u32) -> &Self {
        *self.agent_sim.lock().unwrap() = Some(AgentSim {
            beats,
            timeout: false,
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
        _opts: &AgentRunOpts,
        heartbeat: &mut dyn FnMut(),
    ) -> std::io::Result<AgentOutcome> {
        let out = self.run(program, args)?;
        let sim = self.agent_sim.lock().unwrap().take();
        match sim {
            Some(s) => {
                for _ in 0..s.beats {
                    heartbeat();
                }
                if s.timeout {
                    Ok(AgentOutcome::TimedOut { output: out })
                } else {
                    Ok(AgentOutcome::Exited(out))
                }
            }
            None => Ok(AgentOutcome::Exited(out)),
        }
    }
}

#[cfg(test)]
fn prefix_matches(argv: &[String], prefix: &[String]) -> bool {
    if prefix.len() > argv.len() {
        return false;
    }
    prefix.iter().zip(argv.iter()).all(|(p, a)| p == a)
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

    #[test]
    fn real_runner_kills_and_times_out_a_hung_child() {
        // SIRF-7: a real hung command must be killed at the timeout, not waited
        // on forever, and the heartbeat must have fired at least once. Uses a
        // sub-second timeout so the test stays fast.
        let r = RealRunner;
        let opts = AgentRunOpts {
            timeout: Duration::from_millis(200),
            heartbeat_interval: Duration::from_millis(50),
            log_path: None,
        };
        let mut beats = 0;
        let mut hb = || beats += 1;
        let start = Instant::now();
        let outcome = r
            .run_agent("sh", &["-c", "sleep 30"], &opts, &mut hb)
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
        let r = RealRunner;
        let log = std::env::temp_dir().join(format!("sirius-agentlog-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let opts = AgentRunOpts {
            timeout: Duration::from_secs(10),
            heartbeat_interval: Duration::from_secs(1),
            log_path: Some(log.clone()),
        };
        let mut hb = || {};
        let outcome = r
            .run_agent("sh", &["-c", "echo hi; echo boom 1>&2"], &opts, &mut hb)
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
        let r = RealRunner;
        let log = std::env::temp_dir().join(format!("sirius-biglog-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let opts = AgentRunOpts {
            timeout: Duration::from_secs(15),
            heartbeat_interval: Duration::from_secs(100),
            log_path: Some(log.clone()),
        };
        let mut hb = || {};
        // ~200 KB to stdout (>> any pipe buffer).
        let outcome = r
            .run_agent("sh", &["-c", "yes sirius | head -c 200000"], &opts, &mut hb)
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
}
