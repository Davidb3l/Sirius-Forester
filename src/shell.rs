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
pub struct RealRunner {
    /// Working directory for every spawned command. `None` inherits the
    /// process cwd. Fleet workers set this to their isolated git worktree so
    /// agents, git, and test runs never touch the shared checkout.
    pub cwd: Option<std::path::PathBuf>,
}

impl Runner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        let mut cmd = Command::new(program);
        cmd.args(args);
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

        let mut cmd = Command::new(program);
        // Agents are non-interactive: an inherited stdin makes CLIs like
        // `claude -p` wait for piped input (observed: a 3s stall + warning on
        // every run) or, worse, read the fleet's own terminal.
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(stdout_cfg)
            .stderr(stderr_cfg);
        cmd.envs(opts.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if let Some(d) = &self.cwd {
            cmd.current_dir(d);
        }
        let mut child = cmd.spawn()?;

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
                        // Hung or over-budget: kill the whole tree (an
                        // integration cmd's servers, an agent's subprocesses
                        // would otherwise outlive it), reap, report a timeout.
                        kill_descendants(child.id());
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
                return Ok(AgentOutcome::TimedOut { output: out });
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
