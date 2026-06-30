use anyhow::Result;
use clap::Parser;
use engraph_compress::{
    CompressInput, compress,
    filters::{self, FilterCtx},
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

mod cli;
mod hooks;
mod output;
mod redirect;
mod rewrite;
use cli::{BudgetCmd, Cli, Cmd, GainFormat, HookCmd};
use hooks::{
    run_post_read_hook, run_pre_bash_hook, run_pre_grep_hook, run_session_end_hook,
    run_session_start_hook,
};
use output::{
    csv_line, print_filter_gain_table, print_gain_summary, print_gain_table, print_graph,
    print_history, print_hits, print_repo_plan, print_scope_table, print_source_table,
    print_symbol_langs, print_time_table, print_workspace_plan,
};

// `main` is async because the OTLP/gRPC metrics exporter (the `otel` feature)
// needs a tokio runtime, and `run_wrapped_command` awaits tokio::process. With
// `otel` on we use a single worker thread so the worker keeps driving the gRPC
// export while the main task blocks on the meter provider's shutdown — a
// current-thread runtime would deadlock there. Without `otel`, the lean default
// build runs current-thread (no worker threads on the hook hot path).
#[cfg_attr(
    feature = "otel",
    tokio::main(flavor = "multi_thread", worker_threads = 1)
)]
#[cfg_attr(not(feature = "otel"), tokio::main(flavor = "current_thread"))]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ENGRAPH_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Best-effort metrics export; None (no-op) unless built with `--features otel`
    // and `ENGRAPH_OTEL` is set. Held for the whole of `main`: dropped on normal
    // return (flush + shutdown), and shut down explicitly before the process::exit
    // below (which skips Drop).
    let otel_guard = engraph_core::otel::init_from_env();

    let cli = Cli::parse();
    let pool = db::open_default_pool()?;
    let conn = pool.get()?;

    match cli.cmd {
        Cmd::Gain {
            json,
            format,
            by_filter,
            by_project,
            by_session,
            daily,
            weekly,
            monthly,
            all,
            graph,
            history,
        } => {
            let fmt = if json { GainFormat::Json } else { format };
            run_gain(
                &conn,
                fmt,
                GainOpts {
                    by_filter: by_filter || all,
                    by_project: by_project || all,
                    by_session,
                    daily: daily || all,
                    weekly: weekly || all,
                    monthly: monthly || all,
                    graph,
                    history,
                },
            )?;
        }
        Cmd::Run { command, args } => {
            let start = Instant::now();
            let output = run_wrapped_command(&command, &args)
                .await
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
            // process::exit skips Drop, so flush/shutdown the meter provider here.
            if let Some(g) = otel_guard {
                g.shutdown();
            }
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
            HookCmd::SessionStart { client } => {
                if let Err(e) = run_session_start_hook(&conn, client) {
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
            scip_manifest,
            lang,
            project,
            workspace,
            bazel_symbols,
            no_bazel_symbols,
            gc,
            no_gc,
            recursive,
            dry_run,
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
                workspace.is_some() || recursive
            };
            // Pre-index orphan GC: on by default, --no-gc opts out. (clap makes
            // --gc / --no-gc mutually exclusive, so this is just `!no_gc` with
            // an explicit --gc also forcing it on.)
            let effective_gc = gc || !no_gc;
            // `--recursive` indexes a tree of projects rooted at --workspace, or
            // the positional repo if --workspace wasn't given.
            let recursive_root: Option<PathBuf> = if recursive {
                Some(workspace.clone().unwrap_or_else(|| repo.clone()))
            } else {
                workspace.clone()
            };
            if dry_run {
                if let Some(root) = &recursive_root {
                    let plans = engraph_codegraph::plan_workspace(
                        root,
                        effective_bazel_symbols,
                        recursive,
                    )?;
                    print_workspace_plan(root, &plans);
                } else if let Some(m) = &scip_manifest {
                    let text = std::fs::read_to_string(m)?;
                    let entries: Vec<&str> = text
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with('#'))
                        .collect();
                    println!(
                        "dry-run: scip-manifest {} ({} entries)",
                        m.display(),
                        entries.len()
                    );
                    for e in &entries {
                        println!("  {e}");
                    }
                } else {
                    let plan = engraph_codegraph::plan_repo(
                        &repo,
                        scip.as_deref(),
                        lang.as_deref(),
                        effective_bazel_symbols,
                    );
                    print_repo_plan(&repo, &plan);
                }
            } else if let Some(root) = recursive_root {
                let stats = engraph_codegraph::index_workspace(
                    &conn,
                    &root,
                    effective_bazel_symbols,
                    recursive,
                    effective_gc,
                )?;
                let total_bytes: usize = stats
                    .repos
                    .iter()
                    .filter_map(|r| r.outcome.as_ref().ok())
                    .map(|s| s.scip_bytes)
                    .sum();
                for r in &stats.repos {
                    match &r.outcome {
                        Ok(s) => {
                            println!(
                                "  ok  {} ({} entities, {} relations, driver={})",
                                r.project, s.entities_inserted, s.relations_inserted, s.driver_name
                            );
                            print_symbol_langs(&s.symbol_langs, "      ");
                        }
                        Err(e) => println!("  err {} :: {e:#}", r.project),
                    }
                }
                let pruned = stats.pruned_total();
                let pruned_note = if pruned > 0 {
                    format!(", {pruned} pruned")
                } else {
                    String::new()
                };
                println!(
                    "workspace {}: {} repo(s) ok, {} failed; {} entities, {} relations total ({} SCIP bytes, {}ms){}",
                    root.display(),
                    stats.ok_count(),
                    stats.err_count(),
                    stats.entities_total(),
                    stats.relations_total(),
                    total_bytes,
                    start.elapsed().as_millis(),
                    pruned_note
                );
                telemetry::record_event(
                    &conn,
                    EventInput {
                        session_id: session_id.as_deref(),
                        kind: EventKind::Index,
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
            } else if let Some(m) = &scip_manifest {
                // Key by the repo's canonical identity (main worktree) so
                // indexing from any git worktree updates one shared project.
                let project_key = project.unwrap_or_else(|| canonical_repo_key(&repo));
                let stats =
                    engraph_codegraph::index_scip_manifest(&conn, m, &project_key, effective_gc)?;
                telemetry::record_event(
                    &conn,
                    EventInput {
                        session_id: session_id.as_deref(),
                        kind: EventKind::Index,
                        feature: "codegraph_index",
                        filter_id: Some(stats.driver_name),
                        input_tokens: stats.scip_bytes as i64,
                        output_tokens: 0,
                        latency_ms: start.elapsed().as_millis() as i64,
                    },
                )?;
                let pruned_note = if stats.entities_pruned > 0 {
                    format!(", {} pruned", stats.entities_pruned)
                } else {
                    String::new()
                };
                println!(
                    "indexed {} ({} entities, {} relations, {} SCIP bytes, {}ms, driver={}){}",
                    project_key,
                    stats.entities_inserted,
                    stats.relations_inserted,
                    stats.scip_bytes,
                    stats.elapsed_ms,
                    stats.driver_name,
                    pruned_note
                );
            } else {
                // Key by the repo's canonical identity (main worktree) so
                // indexing from any git worktree updates one shared project.
                let project_key = project.unwrap_or_else(|| canonical_repo_key(&repo));
                let stats = engraph_codegraph::index_repo(
                    &conn,
                    &repo,
                    scip.as_deref(),
                    lang.as_deref(),
                    &project_key,
                    effective_bazel_symbols,
                    effective_gc,
                )?;
                telemetry::record_event(
                    &conn,
                    EventInput {
                        session_id: session_id.as_deref(),
                        kind: EventKind::Index,
                        feature: "codegraph_index",
                        filter_id: Some(stats.driver_name),
                        input_tokens: stats.scip_bytes as i64,
                        output_tokens: 0,
                        latency_ms: start.elapsed().as_millis() as i64,
                    },
                )?;
                let pruned_note = if stats.entities_pruned > 0 {
                    format!(", {} pruned", stats.entities_pruned)
                } else {
                    String::new()
                };
                println!(
                    "indexed {} ({} entities, {} relations, {} SCIP bytes, {}ms, driver={}){}",
                    project_key,
                    stats.entities_inserted,
                    stats.relations_inserted,
                    stats.scip_bytes,
                    stats.elapsed_ms,
                    stats.driver_name,
                    pruned_note
                );
                print_symbol_langs(&stats.symbol_langs, "  ");
            }
        }
        Cmd::Subgraph {
            symbol,
            max_nodes,
            json,
        } => {
            let start = Instant::now();
            let neighborhood = engraph_codegraph::subgraph_for(&conn, &symbol, max_nodes)?;
            // Resolve source against the current worktree first, so a symbol
            // indexed from another checkout still reads from where you're working.
            let cur_root = std::env::current_dir()
                .ok()
                .and_then(|c| git_current_toplevel(&c));
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
                    // Baseline = the source the subgraph stands in for (the
                    // symbol's definition file). saved = baseline - body.
                    input_tokens: engraph_codegraph::subgraph::avoided_read_tokens(
                        &neighborhood,
                        cur_root.as_deref(),
                    ) as i64,
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
        Cmd::Remember { rule, project } => {
            let project = resolve_project(project)?;
            let id = engraph_core::memory::add_do_not_repeat(&conn, &project, &rule)?;
            println!("remembered rule {id} for {project}");
        }
        Cmd::Bug {
            summary,
            content,
            project,
            resolve,
        } => {
            if let Some(id) = resolve {
                if engraph_core::memory::resolve_bug(&conn, &id)? == 0 {
                    anyhow::bail!("no bug with id {id}");
                }
                println!("resolved bug {id}");
            } else {
                let summary = summary.expect("clap requires summary unless --resolve");
                let project = resolve_project(project)?;
                let id =
                    engraph_core::memory::log_bug(&conn, &project, &summary, content.as_deref())?;
                println!("logged bug {id} for {project}");
            }
        }
        Cmd::Save {
            decision,
            kind,
            project,
        } => {
            let project = resolve_project(project)?;
            let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
            let id = engraph_core::memory::save_context(
                &conn,
                &project,
                kind.as_str(),
                &decision,
                session_id.as_deref(),
            )?;
            // Recall parity: scope the item to the project so
            // `engraph recall --project <p>` finds it, mirroring how ingest
            // scopes messages.
            let scope_id = engraph_retrieve::scope::ensure_project_scope(&conn, &project)?;
            engraph_retrieve::scope::add_member(&conn, &scope_id, "context_item", &id)?;
            println!("saved {} {id} for {project}", kind.as_str());
        }
    }
    Ok(())
}

/// Which breakdown sections `engraph gain` should emit. `--all` is expanded into
/// the relevant flags by the caller; the default (every field false / None) is a
/// summary plus the per-`(kind, feature)` table.
struct GainOpts {
    by_filter: bool,
    by_project: bool,
    by_session: bool,
    daily: bool,
    weekly: bool,
    monthly: bool,
    graph: bool,
    history: Option<usize>,
}

impl GainOpts {
    /// No breakdown requested → render the default `(kind, feature)` table.
    fn is_default(&self) -> bool {
        !self.by_filter
            && !self.by_project
            && !self.by_session
            && !self.daily
            && !self.weekly
            && !self.monthly
            && !self.graph
            && self.history.is_none()
    }
}

const GRAPH_DAYS: i64 = 30;

fn run_gain(conn: &db::PooledConn, fmt: GainFormat, opts: GainOpts) -> Result<()> {
    use engraph_core::telemetry::{Scope, TimeBucket};

    let summary = telemetry::gain_summary(conn)?;
    // Always computed: the headline answer to "where do the savings come from".
    let by_source = telemetry::gain_by_source(conn)?;
    let kind_feature = opts
        .is_default()
        .then(|| telemetry::gain_report(conn))
        .transpose()?;
    let by_filter = opts
        .by_filter
        .then(|| telemetry::gain_report_by_filter(conn))
        .transpose()?;
    let by_project = opts
        .by_project
        .then(|| telemetry::gain_by_scope(conn, Scope::Project))
        .transpose()?;
    let by_session = opts
        .by_session
        .then(|| telemetry::gain_by_scope(conn, Scope::Session))
        .transpose()?;
    let daily = opts
        .daily
        .then(|| telemetry::gain_by_time(conn, TimeBucket::Daily))
        .transpose()?;
    let weekly = opts
        .weekly
        .then(|| telemetry::gain_by_time(conn, TimeBucket::Weekly))
        .transpose()?;
    let monthly = opts
        .monthly
        .then(|| telemetry::gain_by_time(conn, TimeBucket::Monthly))
        .transpose()?;
    let history = opts
        .history
        .map(|n| telemetry::gain_history(conn, n.max(1)))
        .transpose()?;
    let graph = opts
        .graph
        .then(|| telemetry::gain_daily_series(conn, GRAPH_DAYS))
        .transpose()?;

    match fmt {
        GainFormat::Json => {
            let mut obj = serde_json::Map::new();
            obj.insert("summary".into(), serde_json::to_value(&summary)?);
            obj.insert("by_source".into(), serde_json::to_value(&by_source)?);
            if let Some(r) = &kind_feature {
                obj.insert("by_kind_feature".into(), serde_json::to_value(r)?);
            }
            if let Some(r) = &by_filter {
                obj.insert("by_filter".into(), serde_json::to_value(r)?);
            }
            if let Some(r) = &by_project {
                obj.insert("by_project".into(), serde_json::to_value(r)?);
            }
            if let Some(r) = &by_session {
                obj.insert("by_session".into(), serde_json::to_value(r)?);
            }
            if let Some(r) = &daily {
                obj.insert("daily".into(), serde_json::to_value(r)?);
            }
            if let Some(r) = &weekly {
                obj.insert("weekly".into(), serde_json::to_value(r)?);
            }
            if let Some(r) = &monthly {
                obj.insert("monthly".into(), serde_json::to_value(r)?);
            }
            if let Some(r) = &history {
                obj.insert("history".into(), serde_json::to_value(r)?);
            }
            if let Some(r) = &graph {
                obj.insert("graph".into(), serde_json::to_value(r)?);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::Value::Object(obj))?
            );
        }
        GainFormat::Csv => {
            println!(
                "{}",
                csv_line(&[
                    "section",
                    "key",
                    "count",
                    "input_tk",
                    "output_tk",
                    "saved_tk",
                    "save%"
                ])
            );
            let p = |v: f64| format!("{v:.1}");
            let n = |v: i64| v.to_string();
            println!(
                "{}",
                csv_line(&[
                    "summary",
                    "",
                    &n(summary.commands),
                    &n(summary.input_tokens),
                    &n(summary.output_tokens),
                    &n(summary.saved_tokens),
                    &p(summary.save_pct),
                ])
            );
            for r in &by_source {
                println!(
                    "{}",
                    csv_line(&[
                        "by_source",
                        &r.source,
                        &n(r.count),
                        &n(r.input_tokens),
                        &n(r.output_tokens),
                        &n(r.saved_tokens),
                        &p(r.save_pct),
                    ])
                );
            }
            if let Some(rows) = &by_filter {
                for r in rows {
                    let pct = if r.input_tokens > 0 {
                        r.saved_tokens as f64 / r.input_tokens as f64 * 100.0
                    } else {
                        0.0
                    };
                    println!(
                        "{}",
                        csv_line(&[
                            "by_filter",
                            &r.filter_id,
                            &n(r.count),
                            &n(r.input_tokens),
                            &n(r.output_tokens),
                            &n(r.saved_tokens),
                            &p(pct),
                        ])
                    );
                }
            }
            for (section, rows) in [("by_project", &by_project), ("by_session", &by_session)] {
                if let Some(rows) = rows {
                    for r in rows {
                        println!(
                            "{}",
                            csv_line(&[
                                section,
                                &r.scope,
                                &n(r.count),
                                &n(r.input_tokens),
                                &n(r.output_tokens),
                                &n(r.saved_tokens),
                                &p(r.save_pct),
                            ])
                        );
                    }
                }
            }
            for (section, rows) in [
                ("daily", &daily),
                ("weekly", &weekly),
                ("monthly", &monthly),
            ] {
                if let Some(rows) = rows {
                    for r in rows {
                        println!(
                            "{}",
                            csv_line(&[
                                section,
                                &r.bucket,
                                &n(r.count),
                                &n(r.input_tokens),
                                &n(r.output_tokens),
                                &n(r.saved_tokens),
                                &p(r.save_pct),
                            ])
                        );
                    }
                }
            }
        }
        GainFormat::Text => {
            print_gain_summary(&summary);
            print_source_table(&by_source);
            if let Some(rows) = &kind_feature {
                print_gain_table(rows);
            }
            if let Some(rows) = &by_filter {
                println!("\nby filter");
                print_filter_gain_table(rows);
            }
            if let Some(rows) = &by_project {
                print_scope_table(rows, "project");
            }
            if let Some(rows) = &by_session {
                print_scope_table(rows, "session");
            }
            if let Some(rows) = &daily {
                print_time_table(rows, TimeBucket::Daily.label());
            }
            if let Some(rows) = &weekly {
                print_time_table(rows, TimeBucket::Weekly.label());
            }
            if let Some(rows) = &monthly {
                print_time_table(rows, TimeBucket::Monthly.label());
            }
            if let Some(rows) = &graph {
                print_graph(rows, GRAPH_DAYS);
            }
            if let Some(rows) = &history {
                print_history(rows);
            }
        }
    }
    Ok(())
}

/// Project key for a memory write: explicit `--project` wins; otherwise the
/// canonicalized current working directory. The SessionStart brief keys rows on
/// the session's `cwd` string, so canonicalizing here maximizes the chance a
/// row written mid-session is matched by the next session's brief.
fn resolve_project(over: Option<String>) -> Result<String> {
    if let Some(p) = over {
        return Ok(p);
    }
    let cwd = std::env::current_dir()?;
    Ok(canonical_repo_key(&cwd))
}

/// Stable identity for the repo containing `start`, used as the `project` scope
/// key. For a git repo this is the **main worktree** path — shared across every
/// linked worktree — so all worktrees of a repo map to one logical index instead
/// of each worktree's distinct path spawning a duplicate. Falls back to the
/// canonicalized path when `start` isn't in a git repo (or git is unavailable),
/// which also collapses symlinked checkouts to one key.
pub(crate) fn canonical_repo_key(start: &std::path::Path) -> String {
    if let Some(root) = git_main_worktree(start) {
        return root;
    }
    start
        .canonicalize()
        .unwrap_or_else(|_| start.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The repo's main worktree path via `git worktree list --porcelain`, whose
/// first entry is always the main worktree. `None` when `start` isn't a git
/// working tree or git can't be run.
fn git_main_worktree(start: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let path = stdout.lines().next()?.strip_prefix("worktree ")?.trim();
    if path.is_empty() {
        return None;
    }
    let p = std::path::Path::new(path);
    Some(
        p.canonicalize()
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned(),
    )
}

/// The *current* worktree's root (`git rev-parse --show-toplevel` of `start`) —
/// the checkout you're actually working in, on its branch. Used to resolve a
/// stored relative `file_path` to live source, so subgraph reads the worktree
/// copy, not the (possibly stale or absent) indexed checkout. `None` outside a
/// git working tree.
pub(crate) fn git_current_toplevel(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8(out.stdout).ok()?;
    let top = top.trim();
    if top.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(top))
}

/// Spawn a wrapped command through `tokio::process` and return its combined
/// output. Inherits stdin so interactive commands work (`git log -p` pager,
/// `cargo test -- --interactive`, anything reading from a TTY). Reads stdout
/// and stderr concurrently so neither can deadlock by filling its pipe buffer
/// while we wait on the other. Terminal SIGINT/SIGTERM still reach the child
/// directly via the shared process group; the parent ignores them so it stays
/// alive long enough to drain the child's last output and record telemetry.
async fn run_wrapped_command(command: &str, args: &[String]) -> Result<std::process::Output> {
    // Best-effort: install no-op handlers so the parent doesn't die on Ctrl-C
    // before the child finishes draining. On platforms where signal registration
    // fails (or isn't supported), proceed without it — terminal signals still
    // reach the child either way.
    #[cfg(unix)]
    let _signal_swallower = {
        use tokio::signal::unix::{SignalKind, signal};
        (
            signal(SignalKind::interrupt()).ok(),
            signal(SignalKind::terminate()).ok(),
        )
    };

    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // We buffer the child's stderr to filter it, which hides git's HTTPS
    // credential prompt — a wrapped `git push`/`pull`/`fetch` that needs one
    // would block on input no one can see. The wrapper only ever runs for
    // agent-initiated commands (the hook never rewrites a human's own shell), so
    // no one is there to answer anyway: disable the prompt so git fails fast with
    // a captured error instead of hanging. (SSH passphrase prompts use /dev/tty
    // directly and are unaffected.)
    // Matching the subcommand anywhere is robust to global options
    // (`git -C /repo push`); a false hit on a ref literally named `push` only
    // sets an env var that's a no-op for non-network git commands.
    if command == "git"
        && args
            .iter()
            .any(|a| matches!(a.as_str(), "push" | "pull" | "fetch"))
    {
        cmd.env("GIT_TERMINAL_PROMPT", "0");
    }

    let child = cmd.spawn()?;

    // wait_with_output drains stdout and stderr concurrently while it waits on
    // the child — no pipe-buffer deadlock under large output.
    Ok(child.wait_with_output().await?)
}
