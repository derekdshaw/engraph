use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use engraph_compress::{compress, CompressInput, CompressKind};
use engraph_core::{
    budget, db,
    models::EventKind,
    telemetry::{self, EventInput},
};
use std::path::PathBuf;
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
