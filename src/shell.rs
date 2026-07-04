//! The shell boundary.
//!
//! Every call Sirius makes into `amt` or `hayven` goes through a [`Runner`]. In
//! production that is [`RealRunner`], which spawns a subprocess. In tests it is
//! [`MockRunner`], which returns canned output keyed by the argv prefix, so the
//! whole binary is testable offline. Sirius NEVER opens the parent SQLite for
//! writing — this seam is the only write path to the parents (via their CLIs).

use std::process::Command;
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

/// Abstraction over "run this program with these args and give me the output".
pub trait Runner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput>;
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
}
