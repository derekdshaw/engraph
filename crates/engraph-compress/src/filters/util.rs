//! Shared helpers used by multiple filter modules.

use regex::Regex;
use std::fmt::Write;
use std::sync::OnceLock;

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
