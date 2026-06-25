use engraph_core::telemetry;
use std::path::{Path, PathBuf};

/// Render one repo's `IndexPlan` for `--dry-run`. `indent` prefixes every line
/// so the workspace view can nest per-repo plans.
fn describe_plan(plan: &engraph_codegraph::IndexPlan, indent: &str) {
    use engraph_codegraph::IndexPlan;
    match plan {
        IndexPlan::Bazel { symbol_langs } => {
            println!("{indent}path: Bazel (MODULE.bazel / WORKSPACE detected)");
            println!(
                "{indent}  target-level: `bazel query //...` — covers the whole tree in one pass (no recursion needed)"
            );
            if symbol_langs.is_empty() {
                println!(
                    "{indent}  symbol-level: OFF — pass --bazel-symbols (default ON only in --workspace mode)"
                );
            } else {
                println!("{indent}  symbol-level: ON — per-language indexers:");
                for l in symbol_langs {
                    let mark = if l.indexer_on_path {
                        "on PATH"
                    } else {
                        "MISSING from PATH — would be skipped"
                    };
                    println!("{indent}    - {} ({}: {})", l.language, l.binary, mark);
                }
            }
        }
        IndexPlan::PrebuiltScip(p) => {
            println!("{indent}path: load prebuilt SCIP file {}", p.display());
        }
        IndexPlan::ForcedDriver(name) => {
            println!("{indent}path: forced driver `{name}` (--lang)");
        }
        IndexPlan::AutoDrivers(names) => {
            println!(
                "{indent}path: auto-detect — would run: {}",
                names.join(", ")
            );
        }
        IndexPlan::NoDriverMatch => {
            println!(
                "{indent}path: NO driver matched — a real run would error (pass --lang or --scip)"
            );
        }
    }
}

/// Print the per-language outcome of the Bazel symbol-level pass so a run that
/// produced 0 symbols doesn't read as a clean success. Multi-line failure
/// messages (which carry a stderr tail) are collapsed to their first line.
pub(crate) fn print_symbol_langs(langs: &[engraph_codegraph::SymbolLangSummary], indent: &str) {
    for l in langs {
        let status = l.status.lines().next().unwrap_or("").trim();
        println!(
            "{indent}symbol[{}]: {} ({} SCIP bytes)",
            l.language, status, l.scip_bytes
        );
    }
}

pub(crate) fn print_repo_plan(repo: &Path, plan: &engraph_codegraph::IndexPlan) {
    println!("DRY RUN — no indexer spawned, no Bazel run, no codegraph writes.");
    println!("repo: {}", repo.display());
    describe_plan(plan, "");
}

pub(crate) fn print_workspace_plan(root: &Path, plans: &[(PathBuf, engraph_codegraph::IndexPlan)]) {
    println!("DRY RUN — no indexer spawned, no Bazel run, no codegraph writes.");
    println!("workspace: {}", root.display());
    println!("discovered {} repo(s):", plans.len());
    for (repo, plan) in plans {
        println!("  - {}", repo.display());
        describe_plan(plan, "  ");
    }
}

pub(crate) fn print_hits(hits: &[engraph_retrieve::Hit]) {
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

fn pct(saved: i64, input: i64) -> f64 {
    if input > 0 {
        saved as f64 / input as f64 * 100.0
    } else {
        0.0
    }
}

pub(crate) fn print_gain_table(rows: &[telemetry::GainRow]) {
    if rows.is_empty() {
        println!("(no events)");
        return;
    }
    println!(
        "{:<12} {:<14} {:>6} {:>10} {:>10} {:>10} {:>6}",
        "kind", "feature", "count", "input_tk", "output_tk", "saved_tk", "save%"
    );
    let mut tot_saved = 0_i64;
    let mut savings_rows = 0_i64;
    for r in rows {
        let (saved_cell, pct_cell) = match r.saved_tokens {
            Some(s) => {
                tot_saved += s;
                savings_rows += 1;
                (s.to_string(), format!("{:.1}", pct(s, r.input_tokens)))
            }
            None => ("-".to_string(), "-".to_string()),
        };
        println!(
            "{:<12} {:<14} {:>6} {:>10} {:>10} {:>10} {:>6}",
            r.kind, r.feature, r.count, r.input_tokens, r.output_tokens, saved_cell, pct_cell
        );
    }
    if savings_rows > 0 {
        println!(
            "{:<12} {:<14} {:>6} {:>10} {:>10} {:>10} {:>6}",
            "TOTAL_SAVED", "", "", "", "", tot_saved, ""
        );
    }
}

pub(crate) fn print_filter_gain_table(rows: &[telemetry::FilterGainRow]) {
    if rows.is_empty() {
        println!("(no output_filter events)");
        return;
    }
    println!(
        "{:<18} {:>6} {:>10} {:>10} {:>10} {:>6} {:>6}",
        "filter_id", "count", "input_tk", "output_tk", "saved_tk", "ratio", "save%"
    );
    let (mut tot_in, mut tot_out) = (0_i64, 0_i64);
    for r in rows {
        tot_in += r.input_tokens;
        tot_out += r.output_tokens;
        let ratio = if r.input_tokens > 0 {
            r.output_tokens as f64 / r.input_tokens as f64
        } else {
            1.0
        };
        println!(
            "{:<18} {:>6} {:>10} {:>10} {:>10} {:>6.2} {:>6.1}",
            r.filter_id,
            r.count,
            r.input_tokens,
            r.output_tokens,
            r.saved_tokens,
            ratio,
            pct(r.saved_tokens, r.input_tokens)
        );
    }
    let tot_ratio = if tot_in > 0 {
        tot_out as f64 / tot_in as f64
    } else {
        1.0
    };
    println!(
        "{:<18} {:>6} {:>10} {:>10} {:>10} {:>6.2} {:>6.1}",
        "TOTAL",
        "",
        tot_in,
        tot_out,
        tot_in - tot_out,
        tot_ratio,
        pct(tot_in - tot_out, tot_in)
    );
}

pub(crate) fn print_gain_summary(s: &telemetry::GainSummary) {
    println!("== engraph gain ==");
    println!("commands : {}", s.commands);
    println!("input_tk : {}", s.input_tokens);
    println!("output_tk: {}", s.output_tokens);
    println!("saved_tk : {}", s.saved_tokens);
    println!("save%    : {:.1}", s.save_pct);
}

pub(crate) fn print_time_table(rows: &[telemetry::TimeRow], label: &str) {
    println!("\n{label} breakdown ({} bucket(s))", rows.len());
    if rows.is_empty() {
        println!("(no events)");
        return;
    }
    println!(
        "{:<12} {:>6} {:>10} {:>10} {:>10} {:>6}",
        "bucket", "count", "input_tk", "output_tk", "saved_tk", "save%"
    );
    for r in rows {
        println!(
            "{:<12} {:>6} {:>10} {:>10} {:>10} {:>6.1}",
            r.bucket, r.count, r.input_tokens, r.output_tokens, r.saved_tokens, r.save_pct
        );
    }
}

pub(crate) fn print_scope_table(rows: &[telemetry::ScopeRow], header: &str) {
    println!("\nby {header}");
    if rows.is_empty() {
        println!("(no events)");
        return;
    }
    println!(
        "{:<40} {:>6} {:>10} {:>10} {:>10} {:>6}",
        header, "count", "input_tk", "output_tk", "saved_tk", "save%"
    );
    for r in rows {
        println!(
            "{:<40} {:>6} {:>10} {:>10} {:>10} {:>6.1}",
            r.scope, r.count, r.input_tokens, r.output_tokens, r.saved_tokens, r.save_pct
        );
    }
}

pub(crate) fn print_history(rows: &[telemetry::HistoryRow]) {
    println!("\nrecent events ({})", rows.len());
    if rows.is_empty() {
        println!("(no events)");
        return;
    }
    println!(
        "{:<20} {:<12} {:<14} {:<18} {:>10} {:>10} {:>10}",
        "ts", "kind", "feature", "filter_id", "input_tk", "output_tk", "saved_tk"
    );
    for r in rows {
        println!(
            "{:<20} {:<12} {:<14} {:<18} {:>10} {:>10} {:>10}",
            r.ts, r.kind, r.feature, r.filter_id, r.input_tokens, r.output_tokens, r.saved_tokens
        );
    }
}

/// ASCII sparkline of saved tokens per active day, oldest→newest. `days` is the
/// queried window (for the heading); days with no events are omitted rather than
/// gap-filled (no date-math dependency). Rendered as a horizontal bar chart —
/// one labeled row per active day, with eighth-block sub-cell precision so even
/// a low-savings day shows a visible sliver rather than nothing.
pub(crate) fn print_graph(series: &[(String, i64)], days: i64) {
    // Partial-cell glyphs (1/8 .. 8/8 of a column) for the bar's fractional tail.
    const EIGHTHS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    const BAR_W: usize = 48;
    println!("\nsaved tokens/day — last {days} days");
    if series.is_empty() {
        println!("(no events)");
        return;
    }
    let max = series.iter().map(|(_, v)| *v).max().unwrap_or(0).max(1);
    let total: i64 = series.iter().map(|(_, v)| *v).sum();
    let val_w = series
        .iter()
        .map(|(_, v)| v.to_string().len())
        .max()
        .unwrap_or(1);
    println!(
        "{} active day(s) · {} total · {} peak/day\n",
        series.len(),
        total,
        max
    );
    for (date, v) in series {
        let eighths = ((*v as f64 / max as f64) * (BAR_W * 8) as f64).round() as usize;
        let full = eighths / 8;
        let rem = eighths % 8;
        let mut bar = "█".repeat(full);
        if rem > 0 && full < BAR_W {
            bar.push(EIGHTHS[rem - 1]);
        }
        // Always show at least a sliver for any non-zero day.
        if bar.is_empty() && *v > 0 {
            bar.push(EIGHTHS[0]);
        }
        println!("{date}  {v:>val_w$} │{bar}");
    }
}

/// Minimal RFC-4180 field quoting: wrap in quotes and double inner quotes when
/// the value contains a comma, quote, or newline. Plenty for filter ids / paths.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub(crate) fn csv_line(fields: &[&str]) -> String {
    fields
        .iter()
        .map(|f| csv_field(f))
        .collect::<Vec<_>>()
        .join(",")
}
