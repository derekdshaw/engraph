use anyhow::Result;
use clap::{Parser, Subcommand};
use engraph_core::{budget, db, telemetry};

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
    let mut tot_in = 0_i64;
    let mut tot_out = 0_i64;
    for r in rows {
        println!(
            "{:<12} {:<14} {:>6} {:>10} {:>10} {:>10}",
            r.kind, r.feature, r.count, r.input_tokens, r.output_tokens, r.saved_tokens
        );
        tot_in += r.input_tokens;
        tot_out += r.output_tokens;
    }
    println!(
        "{:<12} {:<14} {:>6} {:>10} {:>10} {:>10}",
        "TOTAL",
        "",
        "",
        tot_in,
        tot_out,
        tot_in - tot_out
    );
}
