//! Shared helpers used by multiple filter modules.

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::OnceLock;

/// A leading timestamp: an optional ISO date, `H:MM:SS` with an optional
/// fraction (`.`, `:` or `,` separated) and zone, optionally in brackets or
/// parentheses, and the space after it.
fn timestamp_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*[\[(]?(?:\d{4}-\d{2}-\d{2}[T ])?\d{1,2}:\d{2}:\d{2}(?:[.:,]\d{1,9})?(?:Z|[+-]\d{2}:?\d{2})?[\])]?\s*",
        )
        .unwrap()
    })
}

/// The text of a log line after its leading timestamp; `None` when the line
/// has no timestamp, or nothing after it.
fn log_key<'a>(re: &Regex, line: &'a str) -> Option<&'a str> {
    let m = re.find(line)?;
    let rest = line[m.end()..].trim_end();
    (!rest.is_empty()).then_some(rest)
}

/// Collapse log lines that repeat with only their timestamp changed —
/// application code logging in every test writes the same line again and
/// again. The first line of each text stays, the later ones go, and
/// `[engraph: + N more lines like the one above]` follows the first; the
/// repeats need not be consecutive. Kept lines are unchanged, so a filter's
/// prefix test sees them as before. Lines with no leading timestamp are
/// never collapsed. Returns `Cow::Borrowed(text)` when nothing repeats.
pub fn collapse_repeated_lines(text: &str) -> std::borrow::Cow<'_, str> {
    let re = timestamp_prefix_re();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in text.lines() {
        if let Some(k) = log_key(re, line) {
            *counts.entry(k).or_insert(0) += 1;
        }
    }
    if !counts.values().any(|&n| n > 1) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut seen: HashSet<&str> = HashSet::new();
    for line in text.lines() {
        match log_key(re, line) {
            Some(k) if counts[k] > 1 => {
                if seen.insert(k) {
                    out.push_str(line);
                    out.push('\n');
                    out.push_str(&format!(
                        "[engraph: + {} more lines like the one above]\n",
                        counts[k] - 1
                    ));
                }
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if !text.ends_with('\n') {
        out.pop();
    }
    std::borrow::Cow::Owned(out)
}

/// Strip ANSI CSI escape sequences (SGR colors, cursor moves, DEC private modes
/// like `\x1b[?25l`). Matches `ESC[<digits/semicolons/?><letter>`. Returns
/// `Cow::Borrowed(s)` when no escapes match, avoiding a clone for the common
/// case of plain-text input.
pub fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]").unwrap());
    re.replace_all(s, "")
}

/// Collapse runs of consecutive identical lines into the line + a marker
/// describing how many were dropped. O(n), no regex. Lines are compared
/// byte-for-byte (no trim) — useful for noisy stack traces where leading
/// whitespace differs and we want it to.
pub fn dedup_consecutive(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut iter = s.lines().peekable();
    while let Some(line) = iter.next() {
        let mut count = 1usize;
        while iter.peek() == Some(&line) {
            iter.next();
            count += 1;
        }
        out.push_str(line);
        out.push('\n');
        if count > 1 {
            let _ = writeln!(out, "[engraph: + {} more identical lines]", count - 1);
        }
    }
    out
}

/// Cap the output at `max_lines`; if anything was dropped, append a marker
/// describing what was hidden. `unit` is a short noun for the line item
/// (e.g. "rows", "paths", "matches").
pub fn truncate_lines(text: &str, max_lines: usize, unit: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_string();
    }
    let kept = &lines[..max_lines];
    format!(
        "{}\n[engraph: truncated {} more {unit}]\n",
        kept.join("\n"),
        lines.len() - max_lines
    )
}

/// Keep only the last `n` lines.
pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        return text.to_string();
    }
    let dropped = lines.len() - n;
    let kept = &lines[dropped..];
    format!(
        "[engraph: hid {dropped} earlier lines]\n{}\n",
        kept.join("\n")
    )
}

/// Drop lines whose trimmed start matches `drop_re`. Returns the surviving
/// text plus the count of dropped lines.
pub fn drop_matching(text: &str, drop_re: &Regex) -> (String, u32) {
    let mut out = String::with_capacity(text.len());
    let mut dropped = 0_u32;
    for line in text.lines() {
        if drop_re.is_match(line.trim_start()) {
            dropped += 1;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, dropped)
}

/// Cap on stderr lines appended by `with_stderr_on_failure`.
const FAILURE_STDERR_LINES: usize = 50;

/// For filters that shape stdout only: append the command's stderr when it
/// failed. `engraph run` prints nothing but the filtered text, so without this
/// the error (`fatal: not a git repository`, `No such file`) is lost. A
/// successful run's stderr stays dropped, so success output is unchanged.
pub fn with_stderr_on_failure(ctx: &super::FilterCtx<'_>, text: String) -> String {
    if ctx.exit_code == 0 || ctx.stderr.trim().is_empty() {
        return text;
    }
    let stderr = truncate_lines(
        &strip_ansi(ctx.stderr),
        FAILURE_STDERR_LINES,
        "stderr lines",
    );
    combine(&text, &stderr)
}

/// Concatenate stdout + stderr with a separator if both are non-empty.
pub fn combine(stdout: &str, stderr: &str) -> String {
    if stdout.is_empty() {
        return stderr.to_string();
    }
    if stderr.is_empty() {
        return stdout.to_string();
    }
    let mut out = String::with_capacity(stdout.len() + stderr.len() + 16);
    out.push_str(stdout);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("--- stderr ---\n");
    out.push_str(stderr);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    #[test]
    fn truncate_keeps_first_n() {
        let s = (0..10).map(|i| format!("line {i}\n")).collect::<String>();
        let out = truncate_lines(&s, 3, "lines");
        assert!(out.contains("line 0"));
        assert!(out.contains("line 2"));
        assert!(!out.contains("line 3"));
        assert!(out.contains("truncated 7 more lines"));
    }

    #[test]
    fn tail_keeps_last_n() {
        let s = (0..10).map(|i| format!("line {i}\n")).collect::<String>();
        let out = tail_lines(&s, 3);
        assert!(out.contains("line 9"));
        assert!(out.contains("line 7"));
        assert!(!out.contains("line 5"));
        assert!(out.contains("hid 7 earlier lines"));
    }

    #[test]
    fn repeated_timestamped_lines_collapse_to_the_first_and_a_count() {
        let text = "10:23:21:255 [xhr-interceptor] constructing the interceptor...\n\
                    10:23:21:278 [xhr-interceptor] constructing the interceptor...\n\
                    10:23:22:001 [xhr-interceptor] constructing the interceptor...\n\
                    10:23:22:002 [xhr-interceptor] request intercepted\n";
        assert_eq!(
            collapse_repeated_lines(text),
            "10:23:21:255 [xhr-interceptor] constructing the interceptor...\n\
             [engraph: + 2 more lines like the one above]\n\
             10:23:22:002 [xhr-interceptor] request intercepted\n"
        );
    }

    #[test]
    fn repeated_lines_are_counted_across_the_whole_text() {
        // The repeats need not be consecutive: a log line per test file
        // interleaves with the other output.
        let text = "10:00:00 a\nother\n10:00:01 a\n10:00:02 b\n10:00:03 a\n";
        assert_eq!(
            collapse_repeated_lines(text),
            "10:00:00 a\n[engraph: + 2 more lines like the one above]\nother\n10:00:02 b\n"
        );
    }

    #[test]
    fn common_timestamp_shapes_count() {
        for (a, b) in [
            ("10:23:21:255 x", "10:23:21:256 x"),
            ("10:23:21.255 x", "10:23:21.256 x"),
            ("2026-11-14 10:23:21 x", "2026-11-14 10:23:22 x"),
            ("2026-11-14T10:23:21.255Z x", "2026-11-14T10:23:21.300Z x"),
            ("[10:23:21] x", "[10:23:22] x"),
        ] {
            let text = format!("{a}\n{b}\n");
            assert_eq!(
                collapse_repeated_lines(&text),
                format!("{a}\n[engraph: + 1 more lines like the one above]\n"),
                "{a}"
            );
        }
    }

    #[test]
    fn lines_without_a_timestamp_or_with_different_text_are_unchanged() {
        let text = "same\nsame\n10:00:00 a\n10:00:01 b\n";
        let out = collapse_repeated_lines(text);
        assert_eq!(out, text);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        let s = "\x1b[31mred\x1b[0m text \x1b[1;32mgreen\x1b[0m";
        assert_eq!(strip_ansi(s), "red text green");
    }

    #[test]
    fn strip_ansi_preserves_plain_text() {
        assert_eq!(strip_ansi("no escapes here"), "no escapes here");
    }

    #[test]
    fn dedup_consecutive_collapses_runs() {
        let s = "foo\nfoo\nfoo\nbar\nbaz\nbaz\n";
        let out = dedup_consecutive(s);
        assert!(
            out.contains("foo\n[engraph: + 2 more identical lines]\n"),
            "missing foo marker: {out}"
        );
        assert!(out.contains("bar\n"));
        assert!(
            out.contains("baz\n[engraph: + 1 more identical lines]\n"),
            "missing baz marker: {out}"
        );
    }

    #[test]
    fn dedup_consecutive_leaves_unique_lines_alone() {
        let s = "a\nb\nc\n";
        assert_eq!(dedup_consecutive(s), "a\nb\nc\n");
    }

    #[test]
    fn dedup_consecutive_does_not_cross_non_identical_lines() {
        // a/a/b/a/a — two separate runs, not one collapsed run of 4.
        let s = "a\na\nb\na\na\n";
        let out = dedup_consecutive(s);
        assert_eq!(
            out,
            "a\n[engraph: + 1 more identical lines]\nb\na\n[engraph: + 1 more identical lines]\n"
        );
    }

    #[test]
    fn dedup_consecutive_handles_empty() {
        assert_eq!(dedup_consecutive(""), "");
    }

    #[test]
    fn drop_matching_filters_progress() {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r"^progress:").unwrap());
        let s = "progress: 1/10\nreal log\nprogress: 2/10\nanother log\n";
        let (out, n) = drop_matching(s, re);
        assert_eq!(n, 2);
        assert!(out.contains("real log"));
        assert!(!out.contains("progress:"));
    }
}
