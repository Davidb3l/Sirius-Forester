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
        /// Git range for --changed (default: working tree vs HEAD). With other
        /// agents working in the same checkout, pass the issue's own commits
        /// (e.g. `base..branch`) — the default sweeps in everyone's edits.
        #[arg(long)]
        range: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Explain a symbol's issues/decisions, or an issue's symbols/decisions.
    Why {
        /// A symbol id (as `hayven query` prints it) or an issue key (PREFIX-n).
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
    /// Record a defect that got past review (SIRF-35), retire a kind that a
    /// real check now catches (`--kind K --automated-by PATH`), or `--list`.
    Escape {
        /// The issue whose work introduced the defect.
        issue: Option<String>,
        /// A slug for the defect class, e.g. `migration-fork`.
        #[arg(long)]
        kind: Option<String>,
        /// What escaped.
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
        /// Who caught it: main-session | integration | human | e2e | …
        #[arg(long)]
        found_by: Option<String>,
        /// The commit that fixed it — makes it a review canary.
        #[arg(long)]
        fix: Option<String>,
        /// Retire `--kind` from the review prompt: this check now catches it.
        #[arg(long)]
        automated_by: Option<String>,
        #[arg(long)]
        list: bool,
        #[arg(long)]
        json: bool,
    },
    /// Measure reviewer recall: replay recorded escapes (their fixes
    /// reverted) and `.sirius/canaries/*.patch` through review.cmd (SIRF-35).
    ReviewCanary {
        /// At most this many canaries.
        #[arg(long, default_value_t = 10)]
        n: usize,
        #[arg(long)]
        json: bool,
    },
    /// Build the integration frontier (base tip + every in-flight sibling)
    /// and run `integration.cmd` on it before anything merges (SIRF-32).
    Integrate {
        /// Clear a red state BY HAND (no green run) and say so on its issue.
        #[arg(long)]
        clear_red: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run the loop with N workers.
    Run {
        /// How many parallel workers. Wins over `worker_concurrency` in
        /// .sirius/config.json, which is the count when this flag is absent
        /// (SIRF-50: the config used to silently CAP the flag).
        #[arg(long)]
        workers: Option<u32>,
        /// Shell command each worker runs per claimed issue (e.g. `claude -p
        /// "…"`). Its program must be on PATH — `run` refuses to start (exit 2)
        /// when it is not. A Claude Code desktop or web session usually has no
        /// agent CLI on PATH: drive iterations by hand instead (sirius skill,
        /// solo mode).
        #[arg(long)]
        agent_cmd: String,
        /// Restrict claimable stages (e.g. todo, backlog).
        #[arg(long)]
        from: Option<String>,
        /// Run at most this many iterations total (0 = until no work).
        #[arg(long, default_value_t = 0)]
        max_iterations: u32,
        /// Fresh-eyes reviewer command for the review stage (SIRF-23),
        /// overriding `review.cmd`; "" disables a configured review.
        #[arg(long)]
        review_cmd: Option<String>,
        /// The workers' model, exported as ANTHROPIC_MODEL / SIRIUS_MODEL and
        /// `{model}` (SIRF-26). Pass YOUR OWN exact model id when a Claude
        /// session launches the fleet; `inherit` reads $SIRIUS_PARENT_MODEL.
        /// Overrides `models.default`; label routes still apply per ticket.
        #[arg(long)]
        model: Option<String>,
        /// The reviewer's model (overrides `models.review`; default: the
        /// workers' model). A different model avoids shared blind spots.
        #[arg(long)]
        review_model: Option<String>,
        /// Launch even when no worker model resolves (agents then use their
        /// CLI's own default — e.g. ~/.claude/settings.json `model`).
        #[arg(long)]
        allow_default_model: bool,
        /// Accepted for CLI-contract compatibility (CONTRACTS §2 documents it);
        /// `run` ALWAYS streams NDJSON to stdout, so this changes nothing.
        #[arg(long)]
        json: bool,
    },
}
