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

pub(crate) fn print_gain_table(rows: &[telemetry::GainRow]) {
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

pub(crate) fn print_filter_gain_table(rows: &[telemetry::FilterGainRow]) {
    if rows.is_empty() {
        println!("(no output_filter events)");
        return;
    }
    println!(
        "{:<18} {:>6} {:>10} {:>10} {:>10} {:>6}",
        "filter_id", "count", "input_tk", "output_tk", "saved_tk", "ratio"
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
            "{:<18} {:>6} {:>10} {:>10} {:>10} {:>6.2}",
            r.filter_id, r.count, r.input_tokens, r.output_tokens, r.saved_tokens, ratio
        );
    }
    let tot_ratio = if tot_in > 0 {
        tot_out as f64 / tot_in as f64
    } else {
        1.0
    };
    println!(
        "{:<18} {:>6} {:>10} {:>10} {:>10} {:>6.2}",
        "TOTAL",
        "",
        tot_in,
        tot_out,
        tot_in - tot_out,
        tot_ratio
    );
}
