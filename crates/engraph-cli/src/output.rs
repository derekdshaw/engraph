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

/// An index run's target for display: `project`, or `project/subtree` for a
/// monorepo sub-project run.
/// The recommendation `engraph index` prints when `stale` (from
/// `engraph_codegraph::stale_normalization`) names projects whose symbol ids
/// were written under other normalization rules than the current ones
/// (plan 05, part 2, step 13). `None` when there are none.
pub(crate) fn stale_normalization_note(stale: &[(String, i64)]) -> Option<String> {
    if stale.is_empty() {
        return None;
    }
    let current = engraph_codegraph::NORMALIZATION_VERSION;
    let list = stale
        .iter()
        .map(|(project, version)| format!("  {project} (rules v{version})"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "note: the symbol ids of {} project(s) were written under older normalization \
         rules than the current v{current}; their ids and the ids of newly indexed code do \
         not connect. Run a full `engraph index <repo>` of each:\n{list}",
        stale.len()
    ))
}

pub(crate) fn scope_label(project: &str, subtree: &str) -> String {
    if subtree.is_empty() {
        project.to_string()
    } else {
        format!("{project}/{subtree}")
    }
}

/// Print the per-language outcome of the Bazel symbol-level pass so a run that
/// produced 0 symbols doesn't read as a clean success.
pub(crate) fn print_symbol_langs(langs: &[engraph_codegraph::SymbolLangSummary], indent: &str) {
    print!("{}", format_symbol_langs(langs, indent));
}

/// One `symbol[<lang>]: <status> (<n> SCIP bytes)` line per language. A
/// failure's status is multi-line — the exit code or signal, then a stderr or
/// stdout tail — and the first line alone never says what went wrong
/// (`scip-python exited with signal: 6 (SIGABRT)` hides `JavaScript heap out
/// of memory`), so the rest follows it, indented four deeper, blank lines
/// dropped.
fn format_symbol_langs(langs: &[engraph_codegraph::SymbolLangSummary], indent: &str) -> String {
    let mut out = String::new();
    for l in langs {
        let mut lines = l.status.lines();
        let first = lines.next().unwrap_or("").trim();
        out.push_str(&format!(
            "{indent}symbol[{}]: {first} ({} SCIP bytes)\n",
            l.language, l.scip_bytes
        ));
        for line in lines.map(str::trim).filter(|l| !l.is_empty()) {
            out.push_str(&format!("{indent}    {line}\n"));
        }
    }
    out
}

pub(crate) fn print_repo_plan(repo: &Path, plan: &engraph_codegraph::IndexPlan) {
    println!("DRY RUN — no indexer spawned, no Bazel run, no codegraph writes.");
    println!("repo: {}", repo.display());
    describe_plan(plan, "");
}

pub(crate) fn print_workspace_plan(root: &Path, plan: &engraph_codegraph::WorkspacePlan) {
    println!("DRY RUN — no indexer spawned, no Bazel run, no codegraph writes.");
    println!("workspace: {}", root.display());
    if !plan.dirs.is_empty() {
        println!("examined:");
        let rows: Vec<(String, String)> = plan
            .dirs
            .iter()
            .map(|d| {
                (
                    engraph_codegraph::workspace_rel(root, &d.path),
                    dir_cause(root, d),
                )
            })
            .collect();
        let width = rows
            .iter()
            .map(|(p, _)| p.chars().count())
            .max()
            .unwrap_or(0);
        for (path, cause) in rows {
            println!("  {path:<width$}  {cause}");
        }
    }
    println!("discovered {} repo(s):", plan.repos.len());
    for (repo, repo_plan) in &plan.repos {
        println!("  - {}", repo.display());
        describe_plan(repo_plan, "  ");
    }
}

/// Why discovery listed `d`, for the dry-run report of a workspace under `root`.
fn dir_cause(root: &Path, d: &engraph_codegraph::DiscoveredDir) -> String {
    use engraph_codegraph::DirOutcome;
    let members = |covered: &[(&str, PathBuf)]| {
        covered
            .iter()
            .map(|(lang, p)| format!("{} ({lang})", engraph_codegraph::workspace_rel(root, p)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut cause = match &d.outcome {
        DirOutcome::Project { langs, covered } if covered.is_empty() => {
            format!("project ({})", langs.join(", "))
        }
        DirOutcome::Project { langs, covered } => {
            format!(
                "project ({}); member of {}",
                langs.join(", "),
                members(covered)
            )
        }
        DirOutcome::BazelRoot => "Bazel root; subtree not examined".to_string(),
        DirOutcome::Member { covered } => format!("member of {}", members(covered)),
        DirOutcome::Excluded => "dropped by --exclude".to_string(),
        DirOutcome::NotIncluded => "not matched by --include".to_string(),
        DirOutcome::NoManifest => "no manifest".to_string(),
        DirOutcome::Ignored => "ignored (.gitignore / .ignore / exclude rules)".to_string(),
        DirOutcome::BeyondMaxDepth { max_depth } => format!("beyond --max-depth {max_depth}"),
    };
    if d.own_repo {
        cause.push_str("; ignored, but its own git repo");
    }
    cause
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
        println!("(no savings events)");
        return;
    }
    println!(
        "{:<18} {:>6} {:>10} {:>10} {:>10} {:>6} {:>6}",
        "item", "count", "input_tk", "output_tk", "saved_tk", "ratio", "save%"
    );
    let (mut tot_in, mut tot_out, mut tot_saved) = (0_i64, 0_i64, 0_i64);
    for r in rows {
        tot_in += r.input_tokens;
        tot_out += r.output_tokens;
        tot_saved += r.saved_tokens;
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
        tot_saved,
        tot_ratio,
        pct(tot_saved, tot_in)
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

/// The three-bucket savings breakdown (command / codegraph / memory) plus each
/// bucket's share of total saved tokens. Surfaces non-command savings that the
/// per-item table would otherwise bury.
pub(crate) fn print_source_table(rows: &[telemetry::SourceRow]) {
    if rows.is_empty() {
        return;
    }
    let total: i64 = rows.iter().map(|r| r.saved_tokens).sum::<i64>().max(1);
    println!("\nby source");
    println!(
        "{:<12} {:>6} {:>10} {:>10} {:>10} {:>7} {:>6}",
        "source", "count", "input_tk", "output_tk", "saved_tk", "share", "save%"
    );
    for r in rows {
        let share = r.saved_tokens as f64 / total as f64 * 100.0;
        println!(
            "{:<12} {:>6} {:>10} {:>10} {:>10} {:>6.1}% {:>6.1}",
            r.source, r.count, r.input_tokens, r.output_tokens, r.saved_tokens, share, r.save_pct
        );
    }
}

/// File reads (`--reads`): all of them, the repeats (a range of a file that
/// the session read before), and the new ranges (another range of such a
/// file), with each row's share of the read tokens.
pub(crate) fn print_reads(r: &telemetry::ReadsReport) {
    println!("\nfile reads");
    if r.reads == 0 {
        println!("(no file reads)");
        return;
    }
    println!(
        "{:<10} {:>6} {:>10} {:>7}",
        "reads", "count", "tokens", "share"
    );
    // Reads of only images or PDFs have 0 tokens: every share is 0, also
    // the share of all reads.
    let share = |tokens: i64| {
        if r.read_tokens > 0 {
            tokens as f64 / r.read_tokens as f64 * 100.0
        } else {
            0.0
        }
    };
    for (key, count, tokens) in [
        ("all", r.reads, r.read_tokens),
        ("repeat", r.repeat_reads, r.repeat_tokens),
        ("new_range", r.new_range_reads, r.new_range_tokens),
    ] {
        println!(
            "{:<10} {:>6} {:>10} {:>6.1}%",
            key,
            count,
            tokens,
            share(tokens)
        );
    }
}

/// Index runs per `filter_id` (`--index`): the runs that produced SCIP, and
/// the no-op runs. Not savings.
pub(crate) fn print_index_table(rows: &[telemetry::IndexRow]) {
    println!("\nindex runs");
    if rows.is_empty() {
        println!("(no index runs)");
        return;
    }
    println!(
        "{:<16} {:>6} {:>10} {:>9} {:>12} {:>6}",
        "type", "runs", "total_ms", "mean_ms", "scip_bytes", "no_op"
    );
    for r in rows {
        println!(
            "{:<16} {:>6} {:>10} {:>9.0} {:>12} {:>6}",
            r.filter_id, r.runs, r.total_ms, r.mean_ms, r.scip_bytes, r.no_op
        );
    }
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

/// Upper bound on the scope column so a single long project path can't blow out
/// the table on a narrow terminal.
const MAX_SCOPE_W: usize = 50;

/// Fit `s` into `w` chars, keeping the **tail** when it's too long (a project
/// path's identifying part — the repo name — is at the end) and marking the cut
/// with a leading `…`.
fn fit_left(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - w.saturating_sub(1)).collect();
    format!("…{tail}")
}

pub(crate) fn print_scope_table(rows: &[telemetry::ScopeRow], header: &str) {
    println!("\nby {header}");
    if rows.is_empty() {
        println!("(no events)");
        return;
    }
    // Size the scope column to the widest value, but never wider than
    // MAX_SCOPE_W or narrower than the header.
    let w = rows
        .iter()
        .map(|r| r.scope.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(header.len(), MAX_SCOPE_W);
    println!(
        "{:<w$} {:>6} {:>10} {:>10} {:>10} {:>6}",
        header, "count", "input_tk", "output_tk", "saved_tk", "save%"
    );
    for r in rows {
        println!(
            "{:<w$} {:>6} {:>10} {:>10} {:>10} {:>6.1}",
            fit_left(&r.scope, w),
            r.count,
            r.input_tokens,
            r.output_tokens,
            r.saved_tokens,
            r.save_pct
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

#[cfg(test)]
mod tests {
    #[test]
    fn stale_normalization_note_names_the_projects_and_the_versions() {
        assert_eq!(super::stale_normalization_note(&[]), None);
        let note = super::stale_normalization_note(&[
            ("/home/me/app".to_string(), 0),
            ("/home/me/lib".to_string(), 0),
        ])
        .unwrap();
        assert!(note.contains("2 project(s)"), "{note}");
        assert!(
            note.contains("/home/me/app") && note.contains("/home/me/lib"),
            "{note}"
        );
        assert!(
            note.contains(&format!("v{}", engraph_codegraph::NORMALIZATION_VERSION)),
            "{note}"
        );
        assert!(note.contains("engraph index"), "{note}");
    }

    use super::*;

    fn lang(status: &str) -> engraph_codegraph::SymbolLangSummary {
        engraph_codegraph::SymbolLangSummary {
            language: "python",
            status: status.to_string(),
            scip_bytes: 0,
        }
    }

    #[test]
    fn symbol_lang_failure_tail_follows_the_summary_line_indented() {
        let status = "failed: scip-python exited with signal: 6 (SIGABRT)\n\
                      stderr (tail):\n\
                      \n\
                      FATAL ERROR: CALL_AND_RETRY_LAST Allocation failed - JavaScript heap out of memory";
        assert_eq!(
            format_symbol_langs(&[lang(status)], "  "),
            "  symbol[python]: failed: scip-python exited with signal: 6 (SIGABRT) (0 SCIP bytes)\n\
             \x20     stderr (tail):\n\
             \x20     FATAL ERROR: CALL_AND_RETRY_LAST Allocation failed - JavaScript heap out of memory\n"
        );
    }

    #[test]
    fn symbol_lang_one_line_status_prints_as_before() {
        assert_eq!(
            format_symbol_langs(&[lang("indexed")], "  "),
            "  symbol[python]: indexed (0 SCIP bytes)\n"
        );
    }

    #[test]
    fn fit_left_keeps_short_unchanged() {
        assert_eq!(fit_left("/home/me/proj", 50), "/home/me/proj");
    }

    #[test]
    fn fit_left_truncates_long_paths_keeping_tail() {
        let long = "/home/me/very/deeply/nested/workspace/that/exceeds/the/cap/myrepo";
        let out = fit_left(long, MAX_SCOPE_W);
        // Width-bounded, ellipsis-prefixed, and the repo name survives.
        assert_eq!(out.chars().count(), MAX_SCOPE_W);
        assert!(out.starts_with('…'));
        assert!(out.ends_with("myrepo"));
    }
}
