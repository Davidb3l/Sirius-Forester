//! Clap CLI definitions (CONTRACTS §2 surface).

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "sirius",
    version,
    about = "Sirius Forester — a local-first fleet foreman for AI coding agents"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create `.sirius/sirius.db` (the ledger) beside an existing `.ametrite/`.
    Init {
        #[arg(long)]
        json: bool,
    },
    /// Check the five §6 contract facts live.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Stamp two-way provenance for an issue or a decision.
    Link {
        /// Issue ref (e.g. AMT-7). Omit when using --decision.
        issue: Option<String>,
        /// Stamp a decision ref instead of an issue (e.g. D-3).
        #[arg(long)]
        decision: Option<String>,
        /// Comma-separated entity ids to stamp.
        #[arg(long, value_delimiter = ',')]
        symbols: Vec<String>,
        /// Resolve symbols from a git range instead of --symbols.
        #[arg(long)]
        changed: bool,
        /// Git range for --changed (default: working tree vs HEAD).
        #[arg(long)]
        range: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Explain a symbol's issues/decisions, or an issue's symbols/decisions.
    Why {
        /// A symbol id or an issue ref (AMT-n).
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Gate an issue: select affected tests, run them (or the full suite on any
    /// doubt), advance on pass, comment on fail.
    Gate {
        issue: String,
        #[arg(long)]
        tier: Option<String>,
        #[arg(long)]
        target_status: Option<String>,
        /// Git range for the changed-file selection (default: working tree vs HEAD).
        #[arg(long)]
        range: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run the loop with N workers.
    Run {
        #[arg(long, default_value_t = 1)]
        workers: u32,
        #[arg(long)]
        agent_cmd: String,
        /// Restrict claimable stages (e.g. todo, backlog).
        #[arg(long)]
        from: Option<String>,
        /// Run at most this many iterations total (0 = until no work).
        #[arg(long, default_value_t = 0)]
        max_iterations: u32,
        #[arg(long)]
        json: bool,
    },
}
