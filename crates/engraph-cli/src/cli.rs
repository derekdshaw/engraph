use clap::{Parser, Subcommand, ValueEnum};
use engraph_compress::CompressKind;
use engraph_core::budget;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "engraph", version, about = "Token-saving AI tooling")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Show a telemetry report of token savings
    Gain {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Break the `wrapped_cmd`/`output_filter` savings down per filter,
        /// ordered by token volume — the per-command view (like `rtk gain`).
        #[arg(long)]
        by_filter: bool,
    },
    /// Manage per-session token budget
    Budget {
        #[command(subcommand)]
        cmd: BudgetCmd,
    },
    /// Run a wrapped command and compress its output before printing
    Run {
        /// The command to execute (e.g. `git`)
        command: String,
        /// Arguments to the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Hook handlers for Claude Code lifecycle events
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Search prior sessions and context for a phrase
    Recall {
        /// FTS query (words are AND-ed)
        query: String,
        /// Limit on returned hits
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Restrict to a project (matches scopes.name where kind='project')
        #[arg(long)]
        project: Option<String>,
        /// Include entities in results
        #[arg(long)]
        with_entities: bool,
        /// Include bugs in results
        #[arg(long)]
        with_bugs: bool,
        /// Use hybrid (FTS+embeddings+recency) retrieval. Only available
        /// when built with `--features embeddings`.
        #[arg(long)]
        hybrid: bool,
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Ingest a Claude Code JSONL transcript into the SQLite store
    Ingest {
        /// JSONL file to ingest. Use `-` for stdin (reads session_id from stdin JSON).
        path: PathBuf,
    },
    /// Sweep messages and context_items, compressing any rows above the
    /// token threshold that haven't been compressed yet. Idempotent.
    CompressExisting {
        /// Cap the number of rows examined per table per run.
        #[arg(long, default_value_t = 1000)]
        batch: usize,
    },
    /// Initialize the embedding model on disk (downloads if absent).
    /// Available only when built with `--features embeddings`.
    #[cfg(feature = "embeddings")]
    InitEmbeddings,
    /// Embed messages that don't yet have a vector for the current model.
    /// Available only when built with `--features embeddings`.
    #[cfg(feature = "embeddings")]
    ReindexEmbeddings {
        /// Cap the number of rows embedded per run.
        #[arg(long, default_value_t = 200)]
        batch: usize,
    },
    /// Build / refresh the codegraph for a repo by running a SCIP indexer
    /// (or loading a prebuilt index.scip).
    Index {
        /// Path to repo root
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Use a prebuilt SCIP file instead of running a driver
        #[arg(long)]
        scip: Option<PathBuf>,
        /// Load externally-produced SCIP files from a manifest (TSV:
        /// `<repo-relative-root>\t<scip-file>` per line; `#` comments allowed).
        /// engraph rebases each to repo-root, merges, and loads once. Mutually
        /// exclusive with --scip / --lang / --workspace / --bazel-symbols.
        #[arg(long, conflicts_with_all = ["scip", "lang", "workspace", "bazel_symbols"])]
        scip_manifest: Option<PathBuf>,
        /// Force a driver: rust-analyzer, scip-python, scip-go, scip-typescript, scip-java
        #[arg(long)]
        lang: Option<String>,
        /// Project key for the indexed entities (default: canonical repo path)
        #[arg(long)]
        project: Option<String>,
        /// Index every sub-repo under this directory (Phase 2.2 cross-repo).
        /// Mutually exclusive with --scip and --lang.
        #[arg(long, conflicts_with_all = ["scip", "lang"])]
        workspace: Option<PathBuf>,
        /// Also drive scip-java / scip-go / scip-typescript via Bazel-resolved
        /// sources after the target-level pass (Phase 2.3 #2). Heavy: implies
        /// toolchain downloads and full builds via Bazel. Defaults: ON for
        /// `--workspace` (one-time hit, full coverage), OFF for single-repo
        /// runs. Use `--no-bazel-symbols` to override the workspace default.
        #[arg(long, conflicts_with = "no_bazel_symbols")]
        bazel_symbols: bool,
        /// Disable symbol-level Bazel indexing in `--workspace` mode where it
        /// is on by default. No effect outside `--workspace`.
        #[arg(long, conflicts_with = "bazel_symbols")]
        no_bazel_symbols: bool,
        /// Prune orphan entities (rows referenced by no relation, in or out)
        /// for the project(s) being indexed, before the load runs. On by
        /// default; use --no-gc to disable. Note: after a partial load (a
        /// --scip-manifest covering only some of a project's languages), GC
        /// will delete the other languages' now-orphaned rows — re-index all
        /// languages to restore them.
        #[arg(long, conflicts_with = "no_gc")]
        gc: bool,
        /// Disable the pre-index orphan GC pass (see --gc).
        #[arg(long, conflicts_with = "gc")]
        no_gc: bool,
        /// Recurse into subdirectories to discover every project (not just the
        /// immediate children of --workspace): prunes build/dep dirs
        /// (node_modules, target, vendor, …), stops at Bazel roots, and
        /// suppresses same-language workspace members. Implies workspace-style
        /// indexing rooted at --workspace (or the positional repo).
        #[arg(long, conflicts_with_all = ["scip", "scip_manifest", "lang"])]
        recursive: bool,
        /// Preview what would be indexed (chosen path, drivers, symbol-level
        /// languages, discovered repos) without spawning any indexer, running
        /// Bazel, or writing the codegraph. Composes with all other flags.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show a 2-hop markdown neighborhood for a symbol from the codegraph
    Subgraph {
        /// Symbol name or full SCIP moniker
        symbol: String,
        /// Soft cap on outgoing + incoming + sibling rows shown
        #[arg(long, default_value_t = 30)]
        max_nodes: usize,
        /// Emit the structured Neighborhood as JSON instead of markdown
        #[arg(long)]
        json: bool,
    },
    /// One-shot deterministic compression of a file
    Compress {
        /// File to compress (use `-` for stdin)
        path: PathBuf,
        /// Compression kind
        #[arg(long, value_enum, default_value_t = CliKind::Generic)]
        kind: CliKind,
        /// Target compressed/original token ratio
        #[arg(long, default_value_t = 0.5)]
        target_ratio: f32,
        /// Write back to the file in place (otherwise print to stdout)
        #[arg(long)]
        in_place: bool,
    },
    /// Record a do-not-repeat rule for the current project
    Remember {
        /// The rule text (e.g. "never force-push main")
        rule: String,
        /// Project key override (default: the current working directory)
        #[arg(long)]
        project: Option<String>,
    },
    /// Log a bug for the current project (open by default), or close an
    /// existing one with `--resolve <id>`
    Bug {
        /// One-line bug summary (required unless --resolve is given)
        #[arg(required_unless_present = "resolve")]
        summary: Option<String>,
        /// Optional long-form detail (root cause, repro, fix notes)
        #[arg(long)]
        content: Option<String>,
        /// Project key override (default: the current working directory)
        #[arg(long)]
        project: Option<String>,
        /// Mark an existing bug resolved by id instead of creating one
        #[arg(long, value_name = "ID")]
        resolve: Option<String>,
    },
    /// Save a curated decision/note for the current project
    Save {
        /// The decision / note text
        decision: String,
        /// Category of the saved item
        #[arg(long, value_enum, default_value_t = SaveKind::Decision)]
        kind: SaveKind,
        /// Project key override (default: the current working directory)
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum HookCmd {
    /// PreToolUse(Bash) backstop: deny commands with available wrappers,
    /// suggesting `engraph run` as the replacement.
    PreBash,
    /// PreToolUse(Grep) redirect: when Claude greps a bareword that resolves
    /// to 1-3 entities in the codegraph, deny and point at
    /// `engraph subgraph <symbol>` for a 2-hop neighborhood instead.
    PreGrep,
    /// PostToolUse(Read) augment: when Claude reads a file we've indexed,
    /// append a listing of symbols in the file (name, line range, signature)
    /// as additionalContext so a follow-up subgraph call is often unneeded.
    PostRead,
    /// SessionStart hook: emit a terse brief of prior context for the current
    /// project as `hookSpecificOutput.additionalContext` (<= MAX_BRIEF_BYTES).
    SessionStart,
    /// SessionEnd hook: ingest the JSONL transcript that Claude Code emits
    /// at session shutdown. Reads the hook payload from stdin, extracts
    /// `transcript_path`, calls `ingest_file`. Errors are logged and swallowed
    /// so a broken ingest never blocks Claude from exiting cleanly.
    SessionEnd,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum CliKind {
    ProjectNotes,
    SessionMessage,
    ToolOutput,
    Generic,
}

impl From<CliKind> for CompressKind {
    fn from(k: CliKind) -> Self {
        match k {
            CliKind::ProjectNotes => CompressKind::ProjectNotes,
            CliKind::SessionMessage => CompressKind::SessionMessage,
            CliKind::ToolOutput => CompressKind::ToolOutput,
            CliKind::Generic => CompressKind::Generic,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum SaveKind {
    Decision,
    Architecture,
    Convention,
    Performance,
}

impl SaveKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SaveKind::Decision => "decision",
            SaveKind::Architecture => "architecture",
            SaveKind::Convention => "convention",
            SaveKind::Performance => "performance",
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum BudgetCmd {
    /// Show current budget status for a session
    Status {
        #[arg(long)]
        session_id: String,
    },
    /// Set soft/hard limits for a session
    Set {
        #[arg(long)]
        session_id: String,
        #[arg(long, default_value_t = budget::DEFAULT_SOFT_LIMIT)]
        soft: i64,
        #[arg(long, default_value_t = budget::DEFAULT_HARD_LIMIT)]
        hard: i64,
    },
}
