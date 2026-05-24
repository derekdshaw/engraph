use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

/// `git log` — collapse to one line per commit: short hash + first subject line.
/// Drops Author/Date/body lines unless the user asked for --oneline (then pass-through).
/// Handles `--graph` by stripping leading graph characters before the regex.
pub fn log(ctx: &FilterCtx<'_>) -> FilterOutput {
    if ctx.args.iter().any(|a| a == "--oneline") {
        return FilterOutput {
            text: ctx.stdout.to_string(),
            filter_id: "git_log",
        };
    }
    let commit_re = commit_re();
    let mut out = String::with_capacity(ctx.stdout.len() / 2);
    let mut current_hash: Option<String> = None;
    let mut subject_emitted_for_current = false;
    let mut total = 0_u32;
    for line in ctx.stdout.lines() {
        let stripped = strip_graph_prefix(line).trim_start();
        if let Some(c) = commit_re.captures(stripped) {
            current_hash = Some(c[1].chars().take(7).collect::<String>());
            subject_emitted_for_current = false;
            total += 1;
            continue;
        }
        if subject_emitted_for_current {
            continue;
        }
        if current_hash.is_some() && is_subject_line(stripped) {
            if let Some(h) = &current_hash {
                out.push_str(&format!("{h} {stripped}\n"));
                subject_emitted_for_current = true;
            }
        }
    }
    out.push_str(&format!("[engraph: {total} commits]\n"));
    FilterOutput {
        text: out,
        filter_id: "git_log",
    }
}

/// Strip leading `git log --graph` decoration characters (`*`, `|`, `/`, `\`, `_`).
/// Does NOT strip spaces — leaves the original whitespace so caller can decide.
fn strip_graph_prefix(line: &str) -> &str {
    let mut idx = 0;
    for (i, ch) in line.char_indices() {
        match ch {
            '*' | '|' | '/' | '\\' | '_' | ' ' if i == idx => idx = i + ch.len_utf8(),
            '*' | '|' | '/' | '\\' | '_' => idx = i + ch.len_utf8(),
            _ => break,
        }
    }
    &line[idx..]
}

/// A subject candidate: non-empty, not a metadata header (`Author:`, `Date:`,
/// `Merge:`, `Commit:`), and not another `commit <hash>` line.
fn is_subject_line(stripped: &str) -> bool {
    if stripped.is_empty() {
        return false;
    }
    for prefix in ["Author:", "AuthorDate:", "Date:", "CommitDate:", "Commit:", "Merge:", "Refs:"] {
        if stripped.starts_with(prefix) {
            return false;
        }
    }
    if commit_re().is_match(stripped) {
        return false;
    }
    true
}

/// `git diff` — keep file headers and hunk headers, summarize each hunk as
/// +X/-Y with a one-line preview of the first non-context line.
/// Stat/shortstat/numstat/name-only forms have no `diff --git`/`@@` markers
/// and would compress to empty; for those, pass through unchanged.
pub fn diff(ctx: &FilterCtx<'_>) -> FilterOutput {
    if ctx.args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--stat" | "--shortstat" | "--numstat" | "--name-only" | "--name-status" | "--summary"
        )
    }) {
        return FilterOutput {
            text: ctx.stdout.to_string(),
            filter_id: "git_diff",
        };
    }
    let mut out = String::with_capacity(ctx.stdout.len() / 4);
    let mut in_hunk = false;
    let mut added = 0_u32;
    let mut removed = 0_u32;
    let mut hunk_preview: Option<String> = None;
    let flush_hunk = |out: &mut String,
                     added: &mut u32,
                     removed: &mut u32,
                     preview: &mut Option<String>| {
        if *added > 0 || *removed > 0 {
            out.push_str(&format!(
                "  +{added} -{removed}{}\n",
                preview
                    .as_ref()
                    .map(|p| format!(" :: {}", truncate(p, 80)))
                    .unwrap_or_default(),
            ));
        }
        *added = 0;
        *removed = 0;
        *preview = None;
    };
    for line in ctx.stdout.lines() {
        if line.starts_with("diff --git") || line.starts_with("--- ") || line.starts_with("+++ ") {
            if in_hunk {
                flush_hunk(&mut out, &mut added, &mut removed, &mut hunk_preview);
                in_hunk = false;
            }
            out.push_str(line);
            out.push('\n');
        } else if line.starts_with("@@") {
            if in_hunk {
                flush_hunk(&mut out, &mut added, &mut removed, &mut hunk_preview);
            }
            in_hunk = true;
            out.push_str(line);
            out.push('\n');
        } else if in_hunk {
            if let Some(rest) = line.strip_prefix('+') {
                if !line.starts_with("+++") {
                    added += 1;
                    if hunk_preview.is_none() {
                        hunk_preview = Some(format!("+{rest}"));
                    }
                }
            } else if let Some(rest) = line.strip_prefix('-') {
                if !line.starts_with("---") {
                    removed += 1;
                    if hunk_preview.is_none() {
                        hunk_preview = Some(format!("-{rest}"));
                    }
                }
            }
        }
    }
    if in_hunk {
        flush_hunk(&mut out, &mut added, &mut removed, &mut hunk_preview);
    }
    FilterOutput {
        text: out,
        filter_id: "git_diff",
    }
}

/// `git status` — pass through (already terse). Truncate if untracked list is huge.
pub fn status(ctx: &FilterCtx<'_>) -> FilterOutput {
    const MAX_LINES: usize = 200;
    let lines: Vec<&str> = ctx.stdout.lines().collect();
    let text = if lines.len() > MAX_LINES {
        let kept = &lines[..MAX_LINES];
        format!(
            "{}\n[engraph: truncated {} more lines]\n",
            kept.join("\n"),
            lines.len() - MAX_LINES
        )
    } else {
        ctx.stdout.to_string()
    };
    FilterOutput {
        text,
        filter_id: "git_status",
    }
}

/// `git show` — treat like a single commit + diff. Use diff() for the body.
pub fn show(ctx: &FilterCtx<'_>) -> FilterOutput {
    let r = diff(ctx);
    FilterOutput {
        text: r.text,
        filter_id: "git_show",
    }
}

fn commit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^commit ([0-9a-f]{7,40})").unwrap())
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim_end();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(cmd: &'a str, args: &'a [String], stdout: &'a str) -> FilterCtx<'a> {
        FilterCtx {
            cmd,
            args,
            stdout,
            stderr: "",
            exit_code: 0,
        }
    }

    #[test]
    fn log_collapses_to_one_line_per_commit() {
        let input = "\
commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Author: Dev <d@x>
Date: Mon Jan 1 12:00:00 2026 -0700

    Fix the parser

commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
Author: Dev <d@x>
Date: Mon Jan 1 12:00:00 2026 -0700

    Add tests
    extra body line that should be dropped
";
        let args = vec!["log".to_string()];
        let o = log(&ctx("git", &args, input));
        assert!(o.text.contains("aaaaaaa Fix the parser"));
        assert!(o.text.contains("bbbbbbb Add tests"));
        assert!(!o.text.contains("extra body line"));
        assert!(o.text.contains("2 commits"));
    }

    #[test]
    fn log_oneline_passes_through() {
        let input = "aaaaaa Fix\nbbbbbb Add\n";
        let args = vec!["log".to_string(), "--oneline".to_string()];
        let o = log(&ctx("git", &args, input));
        assert_eq!(o.text, input);
    }

    #[test]
    fn diff_summarizes_hunks() {
        let input = "\
diff --git a/foo.rs b/foo.rs
--- a/foo.rs
+++ b/foo.rs
@@ -1,3 +1,3 @@
-old line one
+new line one
 unchanged
";
        let args = vec!["diff".to_string()];
        let o = diff(&ctx("git", &args, input));
        assert!(o.text.contains("diff --git a/foo.rs"));
        assert!(o.text.contains("+1 -1"));
        assert!(o.text.contains(":: -old line one") || o.text.contains(":: +new line one"));
    }
}
