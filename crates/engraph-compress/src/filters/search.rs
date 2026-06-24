use super::util::strip_ansi;
use super::{FilterCtx, FilterOutput};
use std::collections::HashMap;
use std::fmt::Write;

/// Cap on emitted match lines (children + inline). Backstop against a search
/// that matches half the tree.
const MAX_MATCH_LINES: usize = 200;
/// Per-match character cap. Defuses a single match landing in a minified /
/// generated line (thousands of columns) without touching normal source lines.
const MAX_LINE_CHARS: usize = 1000;

/// `rg` / `grep` — group matches by file so a path that matched many times is
/// printed once as a heading instead of re-prefixing every line (rg's own
/// `--heading` shape). The repeated path prefix is the dominant redundancy in
/// grep output; single-match files stay on one `path:rest` line so we never
/// *add* overhead to the common one-hit-per-file case.
pub fn rg(ctx: &FilterCtx<'_>) -> FilterOutput {
    // Strip color escapes before reshaping. rg/grep auto-disable color under a
    // pipe, but a forced `--color=always` (or a RIPGREP_CONFIG_PATH / GREP_OPTIONS
    // that forces it) would otherwise wrap every match in SGR codes.
    let clean = strip_ansi(ctx.stdout);
    let text = if groupable(ctx.args, clean.as_ref()) {
        group_by_file(clean.as_ref())
    } else {
        // Context / json / files-only output isn't a clean `path:rest` grid —
        // reshaping it would corrupt structure, so keep the line-capped raw form.
        super::util::truncate_lines(clean.as_ref(), MAX_MATCH_LINES, "matches")
    };
    FilterOutput {
        text,
        filter_id: "rg",
    }
}

/// True when stdout is the standard one-match-per-line `path:rest` grid we can
/// safely regroup. False for formats where a line isn't `path:rest`:
/// `--json`, `-l`/`--files-with-matches`, and any context mode (`-A/-B/-C`,
/// whose context lines use `-` separators and `--` group dividers).
fn groupable(args: &[String], stdout: &str) -> bool {
    if stdout.trim_start().starts_with('{') {
        return false; // JSON output (lines are objects, not path:rest)
    }
    for a in args {
        match a.as_str() {
            "--json"
            | "-l"
            | "--files-with-matches"
            | "-L"
            | "--files-without-match"
            | "--heading"
            | "--context"
            | "--after-context"
            | "--before-context" => return false,
            _ => {}
        }
        if a.starts_with("--context=")
            || a.starts_with("--after-context=")
            || a.starts_with("--before-context=")
        {
            return false;
        }
        // Short flag cluster (single dash) carrying a context flag: `-A2`, `-C`,
        // `-rnB1`. Over-matching here only falls back to the raw path — safe.
        if a.starts_with('-')
            && !a.starts_with("--")
            && a[1..].chars().any(|c| c == 'A' || c == 'B' || c == 'C')
        {
            return false;
        }
    }
    true
}

/// Split a grep line into `(path, rest)` on the first colon. `rest` keeps any
/// `line:` / `line:col:` prefix verbatim, so grouping is lossless for `-n`.
///
/// Only splits when the path candidate is *file-like* (contains `/` or `.`).
/// Single-file `grep`/`rg` omits the filename prefix, so a line like
/// `ERROR: connection failed` has its first colon inside the match text —
/// splitting there would group real content under a fake `ERROR:` heading. A
/// no-extension file in cwd just fails to group (stays inline = identical to
/// input), which is the safe direction.
fn split_match(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(':')?;
    if idx == 0 {
        return None;
    }
    let path = &line[..idx];
    if !path.contains('/') && !path.contains('.') {
        return None;
    }
    Some((path, &line[idx + 1..]))
}

fn push_capped(out: &mut String, s: &str) {
    if s.chars().count() <= MAX_LINE_CHARS {
        out.push_str(s);
    } else {
        out.extend(s.chars().take(MAX_LINE_CHARS));
        out.push('…');
    }
}

fn group_by_file(text: &str) -> String {
    // First-seen path order + the matches under each path. A line that isn't
    // `path:rest` is kept verbatim under its own key with an empty match list.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<&str, Vec<&str>> = HashMap::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        match split_match(line) {
            Some((path, rest)) => {
                if !groups.contains_key(path) {
                    order.push(path.to_string());
                }
                groups.entry(path).or_default().push(rest);
            }
            None => {
                order.push(line.to_string());
                groups.entry(line).or_default();
            }
        }
    }

    let total: usize = order.iter().map(|k| groups[k.as_str()].len().max(1)).sum();

    let mut out = String::with_capacity(text.len());
    let mut emitted = 0usize;
    'outer: for key in &order {
        if emitted >= MAX_MATCH_LINES {
            break;
        }
        let rests = &groups[key.as_str()];
        match rests.len() {
            0 => {
                // Verbatim non-match line (rare once context/json are excluded).
                out.push_str(key);
                out.push('\n');
                emitted += 1;
            }
            1 => {
                out.push_str(key);
                out.push(':');
                push_capped(&mut out, rests[0]);
                out.push('\n');
                emitted += 1;
            }
            _ => {
                out.push_str(key);
                out.push_str(":\n");
                for r in rests {
                    if emitted >= MAX_MATCH_LINES {
                        break 'outer;
                    }
                    out.push_str("  ");
                    push_capped(&mut out, r);
                    out.push('\n');
                    emitted += 1;
                }
            }
        }
    }

    let remaining = total.saturating_sub(emitted);
    if remaining > 0 {
        let _ = writeln!(out, "[engraph: truncated {remaining} more matches]");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(args: &'a [String], stdout: &'a str) -> FilterCtx<'a> {
        FilterCtx {
            args,
            stdout,
            stderr: "",
            exit_code: 0,
        }
    }

    #[test]
    fn rg_caps_long_results() {
        let stdout: String = (0..300)
            .map(|i| format!("src/file{i}.rs:10:match\n"))
            .collect();
        let out = rg(&ctx(&["pattern".to_string()], &stdout));
        assert!(out.text.contains("truncated 100 more matches"));
    }

    #[test]
    fn rg_strips_forced_color() {
        // grep/rg --color=always wraps the match in SGR + erase-line codes.
        let stdout = "util.rs:42:pub \x1b[01;31m\x1b[Kfn\x1b[m\x1b[K truncate_lines\n";
        let out = rg(&ctx(&["fn".to_string()], stdout));
        assert_eq!(out.text, "util.rs:42:pub fn truncate_lines\n");
        assert!(!out.text.contains('\x1b'));
    }

    #[test]
    fn groups_multi_match_files_under_one_header() {
        // Two matches in foo.rs, one in bar.rs: foo's path printed once.
        let stdout = "src/foo.rs:fn a\nsrc/foo.rs:fn b\nsrc/bar.rs:fn c\n";
        let out = rg(&ctx(&["fn".to_string()], stdout));
        assert_eq!(out.text, "src/foo.rs:\n  fn a\n  fn b\nsrc/bar.rs:fn c\n");
        // Path appears once for the multi-match file.
        assert_eq!(out.text.matches("src/foo.rs").count(), 1);
    }

    #[test]
    fn single_match_per_file_is_unchanged() {
        let stdout = "a.rs:hit\nb.rs:hit\n";
        let out = rg(&ctx(&["hit".to_string()], stdout));
        assert_eq!(out.text, "a.rs:hit\nb.rs:hit\n");
    }

    #[test]
    fn preserves_line_numbers_in_grouping() {
        let stdout = "x.rs:10:one\nx.rs:20:two\n";
        let out = rg(&ctx(&["-n".to_string()], stdout));
        assert_eq!(out.text, "x.rs:\n  10:one\n  20:two\n");
    }

    #[test]
    fn context_mode_passes_through_ungrouped() {
        // -A2 emits `-`-separated context lines; grouping must not touch it.
        let stdout = "x.rs:3:match\nx.rs-4-context\n--\nx.rs:9:match2\n";
        let out = rg(&ctx(&["-A2".to_string(), "match".to_string()], stdout));
        assert_eq!(out.text, stdout);
    }

    #[test]
    fn json_passes_through_ungrouped() {
        let stdout = "{\"type\":\"match\",\"data\":{}}\n";
        let out = rg(&ctx(&["--json".to_string(), "x".to_string()], stdout));
        assert_eq!(out.text, stdout);
    }

    #[test]
    fn bare_lines_without_path_prefix_are_not_grouped() {
        // Single-file grep: no `path:` prefix; the colons live in the match text.
        // Must stay verbatim, not collapse under a bogus `ERROR:` heading.
        let stdout = "ERROR: connection failed\nERROR: timeout\n";
        let out = rg(&ctx(&["ERROR".to_string()], stdout));
        assert_eq!(out.text, stdout);
    }

    #[test]
    fn bare_numbered_lines_stay_verbatim() {
        // `grep -n pat singlefile` prefixes line numbers but no path.
        let stdout = "42:ERROR: failed\n43:ERROR: again\n";
        let out = rg(&ctx(&["-n".to_string(), "ERROR".to_string()], stdout));
        assert_eq!(out.text, stdout);
    }

    #[test]
    fn caps_minified_match_line() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let stdout = format!("min.js:{long}\n");
        let out = rg(&ctx(&["x".to_string()], &stdout));
        assert!(out.text.contains('…'));
        assert!(out.text.chars().count() < stdout.chars().count());
    }
}
