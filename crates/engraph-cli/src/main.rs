use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use engraph_compress::{
    compress,
    filters::{self, FilterCtx},
    CompressInput, CompressKind,
};
use engraph_core::{
    budget, db,
    models::EventKind,
    telemetry::{self, EventInput},
    tokens,
};
use engraph_retrieve::{Query, ScopeFilter, Target};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "engraph", version, about = "Token-saving AI tooling")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show a telemetry report of token savings
    Gain {
        /// Output as JSON
        #[arg(long)]
        json: bool,
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
}

#[derive(Subcommand)]
enum HookCmd {
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
enum CliKind {
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

#[derive(Subcommand)]
enum BudgetCmd {
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ENGRAPH_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let pool = db::open_default_pool()?;
    let conn = pool.get()?;

    match cli.cmd {
        Cmd::Gain { json } => {
            let rows = telemetry::gain_report(&conn)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print_gain_table(&rows);
            }
        }
        Cmd::Run { command, args } => {
            let start = Instant::now();
            let output = run_wrapped_command(&command, &args)
                .map_err(|e| anyhow::anyhow!("exec {command} failed: {e}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            let (filter_fn, filter_id) = filters::pick(&command, &args);
            let result = filter_fn(&FilterCtx {
                args: &args,
                stdout: &stdout,
                stderr: &stderr,
                exit_code,
            });

            let input_tokens = tokens::count(&stdout) as i64 + tokens::count(&stderr) as i64;
            let output_tokens = tokens::count(&result.text) as i64;
            let elapsed = start.elapsed().as_millis() as i64;
            let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
            telemetry::record_event(
                &conn,
                EventInput {
                    session_id: session_id.as_deref(),
                    kind: EventKind::WrappedCmd,
                    feature: "output_filter",
                    filter_id: Some(filter_id),
                    input_tokens,
                    output_tokens,
                    latency_ms: elapsed,
                },
            )?;
            // Charge the budget the post-filter cost — what actually lands in
            // Claude's context. Pre-filter input is recorded for telemetry but
            // never gets sent. No session id (CLI run outside a Claude session)
            // means budget enforcement is opted out for that invocation.
            if let Some(sid) = session_id.as_deref() {
                budget::add_used(&conn, sid, output_tokens)?;
            }

            print!("{}", result.text);
            std::process::exit(exit_code);
        }
        Cmd::Hook { cmd } => match cmd {
            HookCmd::PreBash => {
                if let Err(e) = run_pre_bash_hook(&conn) {
                    tracing::warn!(?e, "pre-bash hook failed; allowing through");
                }
            }
            HookCmd::PreGrep => {
                if let Err(e) = run_pre_grep_hook(&conn) {
                    tracing::warn!(?e, "pre-grep hook failed; allowing through");
                }
            }
            HookCmd::PostRead => {
                if let Err(e) = run_post_read_hook(&conn) {
                    tracing::warn!(?e, "post-read hook failed; emitting empty");
                }
            }
            HookCmd::SessionStart => {
                if let Err(e) = run_session_start_hook(&conn) {
                    tracing::warn!(?e, "session-start hook failed; emitting empty");
                }
            }
            HookCmd::SessionEnd => {
                if let Err(e) = run_session_end_hook(&conn) {
                    tracing::warn!(?e, "session-end hook failed; skipping ingest");
                }
            }
        },
        Cmd::Recall {
            query,
            limit,
            project,
            with_entities,
            with_bugs,
            hybrid,
            json,
        } => {
            let scope = match project {
                Some(p) => ScopeFilter::Project(p),
                None => ScopeFilter::All,
            };
            let mut kinds = vec![Target::Messages, Target::ContextItems];
            if with_entities {
                kinds.push(Target::Entities);
            }
            if with_bugs {
                kinds.push(Target::Bugs);
            }
            let start = Instant::now();
            let q = Query {
                text: &query,
                scope,
                kinds: &kinds,
                limit,
                strategy: Default::default(),
            };
            let hits = if hybrid {
                #[cfg(feature = "embeddings")]
                {
                    let provider = engraph_core::embedding::default_provider()?;
                    engraph_retrieve::hybrid::search_hybrid(&conn, &q, provider.as_ref())?
                }
                #[cfg(not(feature = "embeddings"))]
                {
                    anyhow::bail!(
                        "hybrid retrieval requires the `embeddings` feature; rebuild with `--features embeddings`"
                    );
                }
            } else {
                engraph_retrieve::search(&conn, &q)?
            };
            telemetry::record_event(
                &conn,
                EventInput {
                    session_id: std::env::var("CLAUDE_SESSION_ID").ok().as_deref(),
                    kind: EventKind::Retrieve,
                    feature: "recall",
                    filter_id: Some("fts"),
                    input_tokens: 0,
                    output_tokens: hits.iter().map(|h| tokens::count(&h.preview) as i64).sum(),
                    latency_ms: start.elapsed().as_millis() as i64,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else {
                print_hits(&hits);
            }
        }
        Cmd::Index {
            repo,
            scip,
            lang,
            project,
            workspace,
            bazel_symbols,
            no_bazel_symbols,
        } => {
            let start = Instant::now();
            let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
            // Effective bazel-symbols policy: explicit flags win; otherwise
            // default ON in --workspace mode and OFF for single-repo runs.
            // Workspace runs are infrequent and the symbol pass is the only
            // way to get function-level data inside a Bazel monorepo, so
            // paying the one-time cost matches the typical intent.
            let effective_bazel_symbols = if bazel_symbols {
                true
            } else if no_bazel_symbols {
                false
            } else {
                workspace.is_some()
            };
            if let Some(root) = workspace {
                let stats =
                    engraph_codegraph::index_workspace(&conn, &root, effective_bazel_symbols)?;
                let total_bytes: usize = stats
                    .repos
                    .iter()
                    .filter_map(|r| r.outcome.as_ref().ok())
                    .map(|s| s.scip_bytes)
                    .sum();
                for r in &stats.repos {
                    match &r.outcome {
                        Ok(s) => println!(
                            "  ok  {} ({} entities, {} relations, driver={})",
                            r.project, s.entities_inserted, s.relations_inserted, s.driver_name
                        ),
                        Err(e) => println!("  err {} :: {e:#}", r.project),
                    }
                }
                println!(
                    "workspace {}: {} repo(s) ok, {} failed; {} entities, {} relations total ({} SCIP bytes, {}ms)",
                    root.display(),
                    stats.ok_count(),
                    stats.err_count(),
                    stats.entities_total(),
                    stats.relations_total(),
                    total_bytes,
                    start.elapsed().as_millis()
                );
                telemetry::record_event(
                    &conn,
                    EventInput {
                        session_id: session_id.as_deref(),
                        kind: EventKind::WrappedCmd,
                        feature: "codegraph_index",
                        filter_id: Some("workspace"),
                        input_tokens: total_bytes as i64,
                        output_tokens: 0,
                        latency_ms: start.elapsed().as_millis() as i64,
                    },
                )?;
                if stats.err_count() > 0 && stats.ok_count() == 0 {
                    anyhow::bail!("every repo in the workspace failed to index");
                }
            } else {
                let canonical = repo.canonicalize().unwrap_or_else(|_| repo.clone());
                let project_key =
                    project.unwrap_or_else(|| canonical.to_string_lossy().into_owned());
                let stats = engraph_codegraph::index_repo(
                    &conn,
                    &repo,
                    scip.as_deref(),
                    lang.as_deref(),
                    &project_key,
                    effective_bazel_symbols,
                )?;
                telemetry::record_event(
                    &conn,
                    EventInput {
                        session_id: session_id.as_deref(),
                        kind: EventKind::WrappedCmd,
                        feature: "codegraph_index",
                        filter_id: Some(stats.driver_name),
                        input_tokens: stats.scip_bytes as i64,
                        output_tokens: 0,
                        latency_ms: start.elapsed().as_millis() as i64,
                    },
                )?;
                println!(
                    "indexed {} ({} entities, {} relations, {} SCIP bytes, {}ms, driver={})",
                    project_key,
                    stats.entities_inserted,
                    stats.relations_inserted,
                    stats.scip_bytes,
                    stats.elapsed_ms,
                    stats.driver_name
                );
            }
        }
        Cmd::Subgraph {
            symbol,
            max_nodes,
            json,
        } => {
            let start = Instant::now();
            let neighborhood = engraph_codegraph::subgraph_for(&conn, &symbol, max_nodes)?;
            let body = if json {
                serde_json::to_string_pretty(&neighborhood)?
            } else {
                engraph_codegraph::format_markdown(
                    &neighborhood,
                    engraph_codegraph::subgraph::DEFAULT_BYTE_CAP,
                )
            };
            telemetry::record_event(
                &conn,
                EventInput {
                    session_id: std::env::var("CLAUDE_SESSION_ID").ok().as_deref(),
                    kind: EventKind::Retrieve,
                    feature: "subgraph",
                    filter_id: Some("subgraph"),
                    input_tokens: 0,
                    output_tokens: tokens::count(&body) as i64,
                    latency_ms: start.elapsed().as_millis() as i64,
                },
            )?;
            print!("{body}");
            if !body.ends_with('\n') {
                println!();
            }
        }
        Cmd::Ingest { path } => {
            let stats = engraph_ingest::ingest_file(&conn, &path)?;
            println!(
                "ingested {} messages ({} compressed, {} bytes read, {}ms)",
                stats.messages_inserted,
                stats.messages_compressed,
                stats.bytes_read,
                stats.elapsed_ms
            );
        }
        Cmd::CompressExisting { batch } => {
            let stats = engraph_ingest::compress_existing(&conn, batch)?;
            println!(
                "scanned {} rows, compressed {} ({} -> {} bytes, {}ms)",
                stats.rows_scanned,
                stats.rows_compressed,
                stats.bytes_before,
                stats.bytes_after,
                stats.elapsed_ms
            );
        }
        #[cfg(feature = "embeddings")]
        Cmd::InitEmbeddings => {
            let provider = engraph_core::embedding::default_provider()?;
            println!(
                "initialized embedding model: {} (dim {})",
                provider.model_id(),
                provider.dim()
            );
        }
        #[cfg(feature = "embeddings")]
        Cmd::ReindexEmbeddings { batch } => {
            let provider = engraph_core::embedding::default_provider()?;
            let n = engraph_retrieve::hybrid::reindex_messages(&conn, provider.as_ref(), batch)?;
            println!("embedded {n} messages (model {})", provider.model_id());
        }
        Cmd::Compress {
            path,
            kind,
            target_ratio,
            in_place,
        } => {
            let text = if path.as_os_str() == "-" {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            } else {
                std::fs::read_to_string(&path)?
            };
            let start = Instant::now();
            let result = compress(CompressInput {
                text: &text,
                kind: kind.into(),
                target_ratio,
                brevity: false,
            });
            let elapsed = start.elapsed().as_millis() as i64;
            telemetry::record_event(
                &conn,
                EventInput {
                    session_id: None,
                    kind: EventKind::Compress,
                    feature: "compress",
                    filter_id: Some(result.algorithm_id),
                    input_tokens: result.original_tokens as i64,
                    output_tokens: result.compressed_tokens as i64,
                    latency_ms: elapsed,
                },
            )?;
            if in_place && path.as_os_str() != "-" {
                std::fs::write(&path, &result.text)?;
                eprintln!(
                    "compressed {} → {} tokens (ratio {:.2}) in {}ms",
                    result.original_tokens,
                    result.compressed_tokens,
                    result.ratio(),
                    elapsed
                );
            } else {
                print!("{}", result.text);
            }
        }
        Cmd::Budget { cmd } => match cmd {
            BudgetCmd::Status { session_id } => {
                let g = budget::get_or_init(&conn, &session_id)?;
                let pct = if g.soft > 0 {
                    (g.used as f64 / g.soft as f64) * 100.0
                } else {
                    0.0
                };
                println!(
                    "session={session_id} used={used} soft={soft} hard={hard} pct_of_soft={pct:.1}% level={lvl}",
                    used = g.used,
                    soft = g.soft,
                    hard = g.hard,
                    lvl = g.escalation_level()
                );
            }
            BudgetCmd::Set {
                session_id,
                soft,
                hard,
            } => {
                budget::set_limits(&conn, &session_id, soft, hard)?;
                println!("set session={session_id} soft={soft} hard={hard}");
            }
        },
    }
    Ok(())
}

/// Hard cap for the session-start brief, in bytes. Claude Code injects this
/// into context at session start; keep it small.
const MAX_BRIEF_BYTES: usize = 2048;

/// SessionStart hook: read the JSON from stdin, resolve a project scope from
/// `cwd`, gather a terse markdown brief of prior decisions, do-not-repeat
/// rules, and budget status, and emit it as hookSpecificOutput.additionalContext.
fn run_session_start_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;

    let parsed: Option<serde_json::Value> = if buf.trim().is_empty() {
        None
    } else {
        // Malformed JSON falls back to "no stdin info" rather than failing the hook.
        serde_json::from_str(&buf).ok()
    };
    let cwd = match parsed.as_ref() {
        Some(v) => v.get("cwd").and_then(|c| c.as_str()).map(|s| s.to_string()),
        None => std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
    };
    let session_id = parsed
        .as_ref()
        .and_then(|v| v.get("session_id").and_then(|c| c.as_str()))
        .map(|s| s.to_string());

    let mut signal_sections: Vec<String> = Vec::new();
    if let Some(cwd) = cwd.as_deref() {
        let dnr = recent_do_not_repeat(conn, cwd, 5)?;
        if !dnr.is_empty() {
            signal_sections.push("## do-not-repeat".to_string());
            for r in dnr {
                signal_sections.push(format!("- {r}"));
            }
        }
        let bugs = open_bugs(conn, cwd, 5)?;
        if !bugs.is_empty() {
            signal_sections.push("## open bugs".to_string());
            for b in bugs {
                signal_sections.push(format!("- {b}"));
            }
        }
    }
    if let Some(sid) = session_id.as_deref() {
        let g = budget::get_or_init(conn, sid)?;
        // Surface when usage is non-zero OR limits diverge from defaults.
        let limits_default =
            g.soft == budget::DEFAULT_SOFT_LIMIT && g.hard == budget::DEFAULT_HARD_LIMIT;
        if g.used > 0 || !limits_default {
            signal_sections.push(format!(
                "## budget\nsession={sid} used={used} soft={soft} hard={hard} level={lvl}",
                used = g.used,
                soft = g.soft,
                hard = g.hard,
                lvl = g.escalation_level()
            ));
        }
    }

    // Empty additionalContext on a truly-fresh project: zero injected tokens.
    let body = if signal_sections.is_empty() {
        String::new()
    } else {
        let mut full = String::new();
        if let Some(cwd) = cwd.as_deref() {
            full.push_str(&format!("# engraph brief — {cwd}\n"));
        }
        full.push_str(&signal_sections.join("\n"));
        truncate_to_bytes(&full, MAX_BRIEF_BYTES)
    };

    let start = Instant::now();
    let decision = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": body,
        }
    });
    println!("{decision}");

    if !body.is_empty() {
        telemetry::record_event(
            conn,
            EventInput {
                session_id: session_id.as_deref(),
                kind: EventKind::Hook,
                feature: "session_brief",
                filter_id: Some("session_start"),
                input_tokens: 0,
                output_tokens: tokens::count(&body) as i64,
                latency_ms: start.elapsed().as_millis() as i64,
            },
        )?;
    }
    Ok(())
}

const TRUNCATE_MARKER: &str = "\n…[truncated]";

fn truncate_to_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let marker_len = TRUNCATE_MARKER.len();
    if max <= marker_len {
        // No room for content; emit marker alone, clipped to max.
        return TRUNCATE_MARKER.chars().take(max).collect();
    }
    let mut cut = max - marker_len;
    while !s.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push_str(TRUNCATE_MARKER);
    out
}

fn recent_do_not_repeat(conn: &db::PooledConn, project: &str, limit: i64) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT rule FROM do_not_repeat WHERE project = ?1 ORDER BY ts DESC LIMIT ?2")?;
    let out = stmt
        .query_map(rusqlite::params![project, limit], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

fn open_bugs(conn: &db::PooledConn, project: &str, limit: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT summary FROM bugs WHERE project = ?1 AND resolved = 0 ORDER BY ts DESC LIMIT ?2",
    )?;
    let out = stmt
        .query_map(rusqlite::params![project, limit], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

/// PreToolUse(Bash) hook: read tool_input.command, and if we have a wrapper
/// for it (and it isn't already wrapped via `engraph run`), emit a deny+suggest
/// JSON. Otherwise stay silent and exit 0.
/// Decision returned by the pre-bash analysis. Phase A of v2: prefer Rewrite
/// (silent allow + updatedInput); fall back to DenySuggest only when the
/// command can't be safely re-wrapped.
#[derive(Debug, PartialEq, Eq)]
enum RewriteOutcome {
    Rewrite {
        new_command: String,
        filter_id: &'static str,
    },
    DenySuggest {
        reason: String,
        filter_id: &'static str,
    },
    Passthrough,
}

fn try_auto_rewrite(command: &str) -> RewriteOutcome {
    let command = command.trim();
    if command.is_empty() {
        return RewriteOutcome::Passthrough;
    }
    // Recursion guard: never wrap something that already routes through engraph.
    if command.starts_with("engraph ") || command.contains(" engraph run ") {
        return RewriteOutcome::Passthrough;
    }
    // Heredocs (`<<EOF`/`<<-EOF`/`<<'EOF'`): rewriting would corrupt the body.
    // Passthrough so the original command runs untouched.
    if has_heredoc(command) {
        return RewriteOutcome::Passthrough;
    }

    let mut argv = match shell_words::split(command) {
        Ok(v) if !v.is_empty() => v,
        _ => return RewriteOutcome::Passthrough,
    };

    // Peel sudo/env/`FOO=bar` prefix. `None` means the prefix shape is one we
    // can't safely re-emit (sudo would run engraph as root with a different
    // $HOME; `env` arg-parsing is non-trivial; whitespace-containing values
    // would need fragile re-quoting).
    let prefix = match strip_command_prefix(&mut argv) {
        Some(p) => p,
        None => return RewriteOutcome::Passthrough,
    };
    if argv.is_empty() {
        return RewriteOutcome::Passthrough;
    }
    normalize_argv0(&mut argv);
    // Classify on a copy with git's global options (`-C <path>`, `-c k=v`,
    // …) stripped so the subcommand is visible to `filters::pick`. The
    // original `argv` keeps those options intact and is what we re-emit —
    // dropping `-C <path>` here would silently run the wrapped command
    // against the wrong repo (cwd instead of the `-C` target).
    let mut classify = argv.clone();
    strip_git_global_opts(&mut classify);
    if classify.is_empty() {
        return RewriteOutcome::Passthrough;
    }

    // Compound / pipeline detection — checked on the raw string with quote
    // awareness so things like `git log --grep='foo && bar'` don't trip.
    // For compound commands we scan the parsed argv for ANY wrappable token,
    // so `cd /tmp && git log` and `git log | head` both surface a suggestion.
    if has_unquoted_shell_meta(command) {
        for (i, tok) in classify.iter().enumerate() {
            let next = classify.get(i + 1).map(String::as_str).unwrap_or("");
            let (_fn, fid) = filters::pick(tok, &[next.to_string()]);
            if fid != "generic" {
                let reason = format!(
                    "engraph has a wrapper for `{tok} {next}` but the command contains shell operators we can't auto-rewrap. Re-run the wrappable part as: engraph run {tok} {next}"
                );
                return RewriteOutcome::DenySuggest {
                    reason,
                    filter_id: fid,
                };
            }
        }
        return RewriteOutcome::Passthrough;
    }

    let cmd_word = classify[0].as_str();
    let arg_word = classify.get(1).map(String::as_str).unwrap_or("");
    let (_filter_fn, filter_id) = filters::pick(cmd_word, &[arg_word.to_string()]);
    if filter_id == "generic" {
        return RewriteOutcome::Passthrough;
    }

    // Build the wrapped command. shell_words::quote preserves any whitespace
    // or special-char arg in argv. Env prefix is re-emitted verbatim — quoting
    // `KEY=value` would turn it into a literal command name. strip_command_prefix
    // already validated the prefix tokens are shape-safe (identifier=novalue-ws).
    let mut parts: Vec<String> = Vec::with_capacity(prefix.len() + argv.len() + 2);
    parts.extend(prefix);
    parts.push("engraph".to_string());
    parts.push("run".to_string());
    for a in &argv {
        parts.push(shell_words::quote(a).into_owned());
    }
    let new_command = parts.join(" ");
    RewriteOutcome::Rewrite {
        new_command,
        filter_id,
    }
}

/// Detects `<<TAG`/`<<-TAG` (heredoc) outside of single/double quotes.
/// Matches the same quote-tracking logic as `has_unquoted_shell_meta`.
fn has_heredoc(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i + 1 < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !in_single {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double && b == b'<' && bytes[i + 1] == b'<' {
            return true;
        }
        i += 1;
    }
    false
}

/// Peels `sudo`/`env`/`FOO=bar` prefix tokens off argv. Returns the peeled
/// tokens (to be re-emitted ahead of `engraph run`), or `None` if the
/// command shape can't be safely rewritten (sudo, `env`, or an env value
/// containing whitespace that would need fragile re-quoting).
fn strip_command_prefix(argv: &mut Vec<String>) -> Option<Vec<String>> {
    if argv.is_empty() {
        return Some(Vec::new());
    }
    // sudo / env would run engraph in a different environment ($HOME, $USER)
    // and we'd lose the SQLite path. Bail and let the original run.
    if argv[0] == "sudo" || argv[0] == "env" {
        return None;
    }
    let mut prefix = Vec::new();
    while !argv.is_empty() && is_env_assignment(&argv[0]) {
        let tok = &argv[0];
        let eq = tok.find('=').expect("is_env_assignment guarantees '='");
        let value = &tok[eq + 1..];
        // Whitespace in the value means shell_words::split has already merged
        // tokens (`MSG='hello world'` becomes one element `MSG=hello world`).
        // Re-quoting that for the rewritten command is fragile; passthrough.
        if value.chars().any(char::is_whitespace) {
            return None;
        }
        prefix.push(argv.remove(0));
    }
    Some(prefix)
}

/// `/usr/bin/grep` → `grep`. Pure normalization so absolute-path invocations
/// reach the same filter as the bare name.
fn normalize_argv0(argv: &mut [String]) {
    let Some(first) = argv.first_mut() else {
        return;
    };
    if !(first.starts_with('/') || first.starts_with("./")) {
        return;
    }
    if let Some(base) = std::path::Path::new(first.as_str())
        .file_name()
        .and_then(|s| s.to_str())
    {
        *first = base.to_string();
    }
}

/// Drop git's global options (`-C path`, `-c k=v`, `--git-dir=...`,
/// `--work-tree=...`) so `git -C /tmp status` classifies as `git status`.
/// Stops at the first non-flag token (the subcommand).
fn strip_git_global_opts(argv: &mut Vec<String>) {
    if argv.first().map(String::as_str) != Some("git") {
        return;
    }
    // The global-option set itself lives in engraph-compress so this rewrite
    // path and `filters::pick` agree on where the subcommand starts.
    let n = filters::git_global_opt_len(&argv[1..]);
    argv.drain(1..1 + n);
}

fn is_env_assignment(tok: &str) -> bool {
    // Identifier=anything → env-var prefix. Match shell rule for variable names.
    let mut chars = tok.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    let mut saw_eq = false;
    for c in chars {
        if c == '=' {
            saw_eq = true;
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    saw_eq
}

/// Scan for shell operators outside of single/double quotes. Tracks backslash
/// escapes and `$(...)` / backtick command substitutions. False positives are
/// fine — they fall back to deny+suggest, which is safe. False negatives are
/// the concern; we err conservative.
fn has_unquoted_shell_meta(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !in_single {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            match b {
                b'|' | b';' | b'&' | b'<' | b'>' | b'`' => return true,
                b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'(' => return true,
                _ => {}
            }
        }
        i += 1;
    }
    false
}

fn run_pre_bash_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let command = v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if command.is_empty() {
        return Ok(());
    }

    // Subgraph redirect wins over the compression rewrite: `rg <symbol>` or
    // `grep <symbol>` on an indexed bareword (1-3 matches) gets a deny+suggest
    // pointing at `engraph subgraph <symbol>`. That's an order of magnitude
    // smaller than even the compressed grep output, and gives structured edges.
    if let Some(reason) = try_subgraph_redirect_for_bash(&command, conn) {
        emit_subgraph_deny(conn, &reason, "grep_redirect");
        return Ok(());
    }

    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
    match try_auto_rewrite(&command) {
        RewriteOutcome::Rewrite {
            new_command,
            filter_id,
        } => {
            let decision = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "updatedInput": { "command": new_command }
                }
            });
            println!("{decision}");
            telemetry::record_event(
                conn,
                EventInput {
                    session_id: session_id.as_deref(),
                    kind: EventKind::Hook,
                    feature: "cmd_rewrite",
                    filter_id: Some(filter_id),
                    input_tokens: 0,
                    output_tokens: 0,
                    latency_ms: 0,
                },
            )
            .ok();
        }
        RewriteOutcome::DenySuggest { reason, filter_id } => {
            let decision = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason
                }
            });
            println!("{decision}");
            telemetry::record_event(
                conn,
                EventInput {
                    session_id: session_id.as_deref(),
                    kind: EventKind::Hook,
                    feature: "cmd_deny",
                    filter_id: Some(filter_id),
                    input_tokens: 0,
                    output_tokens: 0,
                    latency_ms: 0,
                },
            )
            .ok();
        }
        RewriteOutcome::Passthrough => {}
    }
    Ok(())
}

/// "Looks like a single-symbol lookup": bareword identifier of length >= 3,
/// no regex metachars. Common short tokens (`id`, `if`) and any regex
/// (`.+*?()[]{}|\^$`) pass through silently.
fn is_symbol_lookup(pattern: &str) -> bool {
    if pattern.len() < 3 {
        return false;
    }
    let mut chars = pattern.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Shared subgraph-redirect message generator. Returns Some(reason) when the
/// pattern is a bareword resolving to 1-3 entities in the codegraph; None
/// otherwise (not a symbol shape, 0 matches, or 4+ matches — ambiguous).
/// The LIMIT 4 inner scan caps work and aligns with the 8-cap in
/// `resolve_matches` (subgraph.rs:84).
fn try_subgraph_redirect(pattern: &str, conn: &db::PooledConn) -> Option<String> {
    if !is_symbol_lookup(pattern) {
        return None;
    }
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM entities WHERE name = ?1 OR id = ?1 LIMIT 4)",
            rusqlite::params![pattern],
            |r| r.get(0),
        )
        .ok()?;
    if !(1..=3).contains(&count) {
        return None;
    }
    let plural = if count == 1 { "" } else { "es" };
    Some(format!(
        "`{pattern}` is indexed in the engraph code graph ({count} match{plural}). \
         Run `engraph subgraph {pattern}` for a 2-hop neighborhood (calls, callers, \
         siblings) instead of grepping. If you still need a raw search, add a regex \
         metachar (e.g. `{pattern}\\b`) to bypass this redirect."
    ))
}

/// `rg`/`grep <bareword>` on an indexed symbol takes precedence over the
/// compression rewrite. Reuses the same parser as `try_auto_rewrite` so
/// `/usr/bin/rg foo` and `FOO=bar rg foo` get caught too. Skips compounds
/// and heredocs — the existing rewrite path handles those.
fn try_subgraph_redirect_for_bash(command: &str, conn: &db::PooledConn) -> Option<String> {
    if has_heredoc(command) || has_unquoted_shell_meta(command) {
        return None;
    }
    let mut argv = shell_words::split(command).ok()?;
    let _prefix = strip_command_prefix(&mut argv)?;
    if argv.is_empty() {
        return None;
    }
    normalize_argv0(&mut argv);
    if argv[0] != "rg" && argv[0] != "grep" {
        return None;
    }
    // First non-flag token is the pattern. Handles `rg -i foo`, `grep -r foo .`.
    let pattern = argv.iter().skip(1).find(|a| !a.starts_with('-'))?;
    try_subgraph_redirect(pattern, conn)
}

fn emit_subgraph_deny(conn: &db::PooledConn, reason: &str, feature: &'static str) {
    let decision = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    });
    println!("{decision}");
    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
    telemetry::record_event(
        conn,
        EventInput {
            session_id: session_id.as_deref(),
            kind: EventKind::Hook,
            feature,
            filter_id: Some("subgraph-redirect"),
            input_tokens: 0,
            output_tokens: 0,
            latency_ms: 0,
        },
    )
    .ok();
}

/// PreToolUse(Grep) hook: redirect bareword Grep on an indexed symbol to
/// `engraph subgraph`. See `try_subgraph_redirect` for the gate.
fn run_pre_grep_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let pattern = v
        .pointer("/tool_input/pattern")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if let Some(reason) = try_subgraph_redirect(&pattern, conn) {
        emit_subgraph_deny(conn, &reason, "grep_redirect");
    }
    Ok(())
}

/// PostToolUse(Read) hook: when Claude reads a file that engraph has indexed,
/// append a listing of symbols defined in that file (name, line range,
/// signature) as `additionalContext`. Often answers "what's in this file"
/// without a follow-up subgraph or grep.
fn run_post_read_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let file_path = v
        .pointer("/tool_input/file_path")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim();
    if file_path.is_empty() {
        return Ok(());
    }
    let entities = engraph_codegraph::subgraph::entities_in_file(conn, file_path, 30)?;
    if entities.is_empty() {
        return Ok(());
    }
    let context = truncate_to_bytes(&build_read_context(file_path, &entities), MAX_BRIEF_BYTES);
    let decision = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": context
        }
    });
    println!("{decision}");
    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
    telemetry::record_event(
        conn,
        EventInput {
            session_id: session_id.as_deref(),
            kind: EventKind::Hook,
            feature: "read_augment",
            filter_id: Some("read-augment"),
            input_tokens: 0,
            output_tokens: tokens::count(&context) as i64,
            latency_ms: 0,
        },
    )
    .ok();
    Ok(())
}

/// SessionEnd hook: Claude Code emits a JSON envelope on stdin that carries
/// `transcript_path` — the path to the JSONL transcript file for the session
/// that just ended. Ingest it into the codegraph store so subsequent sessions'
/// `engraph recall` queries can surface this session's messages. Empty stdin
/// or a missing `transcript_path` is treated as a no-op (not every SessionEnd
/// reason carries a transcript).
fn run_session_end_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let transcript_path = match v.pointer("/transcript_path").and_then(|s| s.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(()),
    };
    let start = std::time::Instant::now();
    let stats = engraph_ingest::ingest_file(conn, std::path::Path::new(transcript_path))?;
    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
    telemetry::record_event(
        conn,
        EventInput {
            session_id: session_id.as_deref(),
            kind: EventKind::Hook,
            feature: "session_ingest",
            filter_id: Some("ingest"),
            input_tokens: stats.bytes_read as i64,
            output_tokens: stats.messages_inserted as i64,
            latency_ms: start.elapsed().as_millis() as i64,
        },
    )
    .ok();
    Ok(())
}

fn build_read_context(
    file_path: &str,
    entities: &[engraph_codegraph::subgraph::EntityRow],
) -> String {
    let mut out = format!("Indexed symbols in {file_path}:\n");
    for e in entities {
        let line = e.line_range.as_deref().unwrap_or("?");
        match e.signature.as_deref() {
            Some(sig) if !sig.is_empty() => {
                out.push_str(&format!("- `{}` @ {line} — `{sig}`\n", e.name));
            }
            _ => {
                out.push_str(&format!("- `{}` @ {line}\n", e.name));
            }
        }
    }
    out.push_str("\n(Use `engraph subgraph <name>` for calls/callers.)\n");
    out
}

/// Spawn a wrapped command through `tokio::process` and return its combined
/// output. Inherits stdin so interactive commands work (`git log -p` pager,
/// `cargo test -- --interactive`, anything reading from a TTY). Reads stdout
/// and stderr concurrently so neither can deadlock by filling its pipe buffer
/// while we wait on the other. Terminal SIGINT/SIGTERM still reach the child
/// directly via the shared process group; the parent ignores them so it stays
/// alive long enough to drain the child's last output and record telemetry.
fn run_wrapped_command(command: &str, args: &[String]) -> Result<std::process::Output> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // Best-effort: install no-op handlers so the parent doesn't die on
        // Ctrl-C before the child finishes draining. On platforms where signal
        // registration fails (or isn't supported), proceed without it — terminal
        // signals still reach the child either way.
        #[cfg(unix)]
        let _signal_swallower = {
            use tokio::signal::unix::{signal, SignalKind};
            (
                signal(SignalKind::interrupt()).ok(),
                signal(SignalKind::terminate()).ok(),
            )
        };

        let child = tokio::process::Command::new(command)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // wait_with_output drains stdout and stderr concurrently while it
        // waits on the child — no pipe-buffer deadlock under large output.
        let output = child.wait_with_output().await?;
        Ok::<_, std::io::Error>(output)
    })
    .map_err(|e: std::io::Error| anyhow::anyhow!(e))
}

fn print_hits(hits: &[engraph_retrieve::Hit]) {
    if hits.is_empty() {
        println!("(no hits)");
        return;
    }
    for h in hits {
        println!(
            "[{kind} score={score:.3} session={session:?} ts={ts:?}]",
            kind = h.target_kind,
            score = h.score,
            session = h.session_id.as_deref().unwrap_or("-"),
            ts = h.ts.as_deref().unwrap_or("-")
        );
        println!("  {}", h.preview);
    }
}

fn print_gain_table(rows: &[telemetry::GainRow]) {
    if rows.is_empty() {
        println!("(no events)");
        return;
    }
    println!(
        "{:<12} {:<14} {:>6} {:>10} {:>10} {:>10}",
        "kind", "feature", "count", "input_tk", "output_tk", "saved_tk"
    );
    let mut tot_saved = 0_i64;
    let mut savings_rows = 0_i64;
    for r in rows {
        let saved_cell = match r.saved_tokens {
            Some(s) => {
                tot_saved += s;
                savings_rows += 1;
                s.to_string()
            }
            None => "-".to_string(),
        };
        println!(
            "{:<12} {:<14} {:>6} {:>10} {:>10} {:>10}",
            r.kind, r.feature, r.count, r.input_tokens, r.output_tokens, saved_cell
        );
    }
    if savings_rows > 0 {
        println!(
            "{:<12} {:<14} {:>6} {:>10} {:>10} {:>10}",
            "TOTAL_SAVED", "", "", "", "", tot_saved
        );
    }
}
