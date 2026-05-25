//! Shared helpers used by multiple filter modules.

use regex::Regex;

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
