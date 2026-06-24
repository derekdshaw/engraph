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
        if current_hash.is_some()
            && is_subject_line(stripped)
            && let Some(h) = &current_hash
        {
            out.push_str(&format!("{h} {stripped}\n"));
            subject_emitted_for_current = true;
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
    for prefix in [
        "Author:",
        "AuthorDate:",
        "Date:",
        "CommitDate:",
        "Commit:",
        "Merge:",
        "Refs:",
    ] {
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
    let flush_hunk =
        |out: &mut String, added: &mut u32, removed: &mut u32, preview: &mut Option<String>| {
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
            } else if let Some(rest) = line.strip_prefix('-')
                && !line.starts_with("---")
            {
                removed += 1;
                if hunk_preview.is_none() {
                    hunk_preview = Some(format!("-{rest}"));
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

/// `git status` (long form) — drop the boilerplate the model never acts on: the
/// `(use "git ...")` hint lines, the `no changes added to commit` trailer, the
/// `Your branch is up to date` line, and blank separators. Keep the branch line,
/// the section headers, and the actual file entries. `--porcelain`/`--short`
/// output has none of this and passes through (only ANSI strip + cap).
pub fn status(ctx: &FilterCtx<'_>) -> FilterOutput {
    const MAX_LINES: usize = 200;
    let clean = super::util::strip_ansi(ctx.stdout);
    let mut out = String::with_capacity(clean.len());
    for line in clean.lines() {
        let t = line.trim_start();
        if t.is_empty()
            || t.starts_with("(use \"")
            || t.starts_with("no changes added to commit")
            || t.starts_with("Your branch is up to date")
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    // Safety: a porcelain/empty-after-strip case falls back to the raw output.
    if out.trim().is_empty() && !clean.trim().is_empty() {
        out = clean.into_owned();
    }
    let text = super::util::truncate_lines(&out, MAX_LINES, "lines");
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

/// Object-transfer progress chatter that `push`/`pull`/`fetch` spray onto stderr.
/// Matched after stripping a leading `remote:` so the server-side copies go too.
const TRANSFER_DROP: &[&str] = &[
    "Enumerating objects:",
    "Counting objects:",
    "Compressing objects:",
    "Writing objects:",
    "Receiving objects:",
    "Resolving deltas:",
    "Unpacking objects:",
    "Delta compression using",
    "Total ",
];

fn drop_transfer_progress(text: &str, out: &mut String) {
    for line in text.lines() {
        let body = line.trim_start();
        let is_remote = body.starts_with("remote:");
        let body = body.strip_prefix("remote:").unwrap_or(body).trim_start();
        // Bare `remote:` separators and the progress counters are pure noise.
        if (is_remote && body.is_empty()) || TRANSFER_DROP.iter().any(|p| body.starts_with(p)) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
}

/// `git push` / `pull` / `fetch` — drop the object-transfer progress, keep the
/// `To`/`From` line, the ref updates, and (for pull) the merge result + diffstat.
/// Most of this lands on stderr; the merge summary is on stdout, emitted after.
fn transfer(ctx: &FilterCtx<'_>, filter_id: &'static str) -> FilterOutput {
    let stderr = super::util::strip_ansi(ctx.stderr);
    let stdout = super::util::strip_ansi(ctx.stdout);
    let mut out = String::with_capacity((stderr.len() + stdout.len()) / 2);
    drop_transfer_progress(&stderr, &mut out);
    drop_transfer_progress(&stdout, &mut out);
    FilterOutput {
        text: out,
        filter_id,
    }
}

pub fn push(ctx: &FilterCtx<'_>) -> FilterOutput {
    transfer(ctx, "git_push")
}

pub fn pull(ctx: &FilterCtx<'_>) -> FilterOutput {
    transfer(ctx, "git_pull")
}

pub fn fetch(ctx: &FilterCtx<'_>) -> FilterOutput {
    transfer(ctx, "git_fetch")
}

/// `git commit` — keep the `[branch hash] subject` header and the
/// `N files changed …` summary; drop the per-file `create/delete mode`, `rename`
/// and `mode change` lines that bloat a large commit's output.
pub fn commit(ctx: &FilterCtx<'_>) -> FilterOutput {
    let clean = super::util::strip_ansi(ctx.stdout);
    let mut out = String::with_capacity(clean.len());
    let mut dropped = 0_u32;
    for line in clean.lines() {
        let t = line.trim_start();
        if t.starts_with("create mode ")
            || t.starts_with("delete mode ")
            || t.starts_with("rename ")
            || t.starts_with("mode change ")
            || t.starts_with("copy ")
        {
            dropped += 1;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if dropped > 0 {
        out.push_str(&format!("[engraph: {dropped} file-mode lines hidden]\n"));
    }
    FilterOutput {
        text: out,
        filter_id: "git_commit",
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

    fn ctx<'a>(args: &'a [String], stdout: &'a str) -> FilterCtx<'a> {
        FilterCtx {
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
        let o = log(&ctx(&args, input));
        assert!(o.text.contains("aaaaaaa Fix the parser"));
        assert!(o.text.contains("bbbbbbb Add tests"));
        assert!(!o.text.contains("extra body line"));
        assert!(o.text.contains("2 commits"));
    }

    #[test]
    fn status_strips_hint_boilerplate() {
        let input = "\
On branch main
Your branch is up to date with 'origin/main'.

Changes not staged for commit:
  (use \"git add <file>...\" to update what will be committed)
  (use \"git restore <file>...\" to discard changes in working directory)
	modified:   a.txt

Untracked files:
  (use \"git add <file>...\" to include in what will be committed)
	b.txt

no changes added to commit (use \"git add\" and/or \"git commit -a\")
";
        let args = vec!["status".to_string()];
        let o = status(&ctx(&args, input));
        assert!(o.text.contains("On branch main"));
        assert!(o.text.contains("Changes not staged for commit:"));
        assert!(o.text.contains("modified:   a.txt"));
        assert!(o.text.contains("b.txt"));
        assert!(!o.text.contains("(use \""));
        assert!(!o.text.contains("no changes added"));
        assert!(!o.text.contains("Your branch is up to date"));
    }

    #[test]
    fn status_porcelain_passes_through() {
        let input = "## main...origin/main\n M a.txt\n?? b.txt\n";
        let args = vec![
            "status".to_string(),
            "--porcelain".to_string(),
            "-b".to_string(),
        ];
        let o = status(&ctx(&args, input));
        assert!(o.text.contains("## main...origin/main"));
        assert!(o.text.contains(" M a.txt"));
        assert!(o.text.contains("?? b.txt"));
    }

    #[test]
    fn log_oneline_passes_through() {
        let input = "aaaaaa Fix\nbbbbbb Add\n";
        let args = vec!["log".to_string(), "--oneline".to_string()];
        let o = log(&ctx(&args, input));
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
        let o = diff(&ctx(&args, input));
        assert!(o.text.contains("diff --git a/foo.rs"));
        assert!(o.text.contains("+1 -1"));
        assert!(o.text.contains(":: -old line one") || o.text.contains(":: +new line one"));
    }
}
