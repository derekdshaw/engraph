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
use cli::{BudgetCmd, Cli, Cmd, HookCmd};
use hooks::{
    run_post_read_hook, run_pre_bash_hook, run_pre_grep_hook, run_session_end_hook,
    run_session_start_hook,
};
use output::{
    print_filter_gain_table, print_gain_table, print_hits, print_repo_plan, print_symbol_langs,
    print_workspace_plan,
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
        Cmd::Gain { json, by_filter } => {
            if by_filter {
                let rows = telemetry::gain_report_by_filter(&conn)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    print_filter_gain_table(&rows);
                }
            } else {
                let rows = telemetry::gain_report(&conn)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    print_gain_table(&rows);
                }
            }
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
                let canonical = repo.canonicalize().unwrap_or_else(|_| repo.clone());
                let project_key =
                    project.unwrap_or_else(|| canonical.to_string_lossy().into_owned());
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
                    input_tokens: engraph_codegraph::subgraph::avoided_read_tokens(&neighborhood)
                        as i64,
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

/// Project key for a memory write: explicit `--project` wins; otherwise the
/// canonicalized current working directory. The SessionStart brief keys rows on
/// the session's `cwd` string, so canonicalizing here maximizes the chance a
/// row written mid-session is matched by the next session's brief.
fn resolve_project(over: Option<String>) -> Result<String> {
    if let Some(p) = over {
        return Ok(p);
    }
    let cwd = std::env::current_dir()?;
    Ok(cwd
        .canonicalize()
        .unwrap_or(cwd)
        .to_string_lossy()
        .into_owned())
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
