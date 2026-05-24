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
use std::process::Command;
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
    /// SessionStart hook: emit a terse brief of prior context for the current
    /// project as `hookSpecificOutput.additionalContext` (<= MAX_BRIEF_BYTES).
    SessionStart,
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
            let output = Command::new(&command)
                .args(&args)
                .output()
                .map_err(|e| anyhow::anyhow!("exec {command} failed: {e}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            let (filter_fn, filter_id) = filters::pick(&command, &args);
            let result = filter_fn(&FilterCtx {
                cmd: &command,
                args: &args,
                stdout: &stdout,
                stderr: &stderr,
                exit_code,
            });

            let input_tokens =
                tokens::count(&stdout) as i64 + tokens::count(&stderr) as i64;
            let output_tokens = tokens::count(&result.text) as i64;
            let elapsed = start.elapsed().as_millis() as i64;
            telemetry::record_event(
                &conn,
                EventInput {
                    session_id: std::env::var("CLAUDE_SESSION_ID").ok().as_deref(),
                    kind: EventKind::WrappedCmd,
                    feature: "F1",
                    filter_id: Some(filter_id),
                    input_tokens,
                    output_tokens,
                    latency_ms: elapsed,
                },
            )?;

            print!("{}", result.text);
            std::process::exit(exit_code);
        }
        Cmd::Hook { cmd } => match cmd {
            HookCmd::PreBash => {
                if let Err(e) = run_pre_bash_hook() {
                    tracing::warn!(?e, "pre-bash hook failed; allowing through");
                }
            }
            HookCmd::SessionStart => {
                if let Err(e) = run_session_start_hook(&conn) {
                    tracing::warn!(?e, "session-start hook failed; emitting empty");
                }
            }
        },
        Cmd::Recall {
            query,
            limit,
            project,
            with_entities,
            with_bugs,
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
            let hits = engraph_retrieve::search(
                &conn,
                &Query {
                    text: &query,
                    scope,
                    kinds: &kinds,
                    limit,
                    strategy: Default::default(),
                },
            )?;
            telemetry::record_event(
                &conn,
                EventInput {
                    session_id: std::env::var("CLAUDE_SESSION_ID").ok().as_deref(),
                    kind: EventKind::Retrieve,
                    feature: "F3",
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
                    feature: "F6",
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
        None => std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()),
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
        let decisions = recent_decisions(conn, cwd, 5)?;
        if !decisions.is_empty() {
            signal_sections.push("## recent decisions".to_string());
            for d in decisions {
                signal_sections.push(format!("- {d}"));
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
        let limits_default = g.soft == budget::DEFAULT_SOFT_LIMIT
            && g.hard == budget::DEFAULT_HARD_LIMIT;
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
                feature: "F4",
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

fn recent_decisions(conn: &db::PooledConn, project: &str, limit: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT c.content FROM context_items c
         JOIN scope_members sm ON sm.target_kind = 'context_item' AND sm.target_id = c.id
         JOIN scopes s ON s.id = sm.scope_id AND s.kind = 'project' AND s.name = ?1
         WHERE c.kind IN ('decision','note')
         ORDER BY c.ts DESC LIMIT ?2",
    )?;
    let out = stmt
        .query_map(rusqlite::params![project, limit], |r| {
            let content: String = r.get(0)?;
            Ok(content.chars().take(180).collect::<String>())
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

fn recent_do_not_repeat(
    conn: &db::PooledConn,
    project: &str,
    limit: i64,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT rule FROM do_not_repeat WHERE project = ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
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
fn run_pre_bash_hook() -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&buf)?;
    let command = v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if command.is_empty() {
        return Ok(());
    }
    if command.starts_with("engraph ") || command.contains(" engraph run ") {
        return Ok(());
    }
    // Tokenize the first two words (cmd + first arg) for filter lookup.
    let mut parts = command.split_whitespace();
    let cmd_word = parts.next().unwrap_or("");
    let arg_word = parts.next().unwrap_or("");
    let args_vec = vec![arg_word.to_string()];
    let (_fn, filter_id) = filters::pick(cmd_word, &args_vec);
    if filter_id == "generic" {
        return Ok(());
    }
    let reason = format!(
        "engraph has a wrapper for `{cmd_word} {arg_word}` that compresses its output. Re-run as: engraph run {cmd_word} {rest}",
        rest = command.trim_start_matches(cmd_word).trim_start(),
    );
    let decision = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    });
    println!("{decision}");
    Ok(())
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
