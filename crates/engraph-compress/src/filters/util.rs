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

/// The trimmed text of a line that can start a run of identical lines:
/// non-blank and without a leading timestamp (stamped lines are keyed by
/// `log_key` instead).
fn run_key<'a>(re: &Regex, line: &'a str) -> Option<&'a str> {
    let t = line.trim();
    (!t.is_empty() && log_key(re, line).is_none()).then_some(t)
}

/// Collapse repeated log lines. A line that repeats with only its leading
/// timestamp changed collapses at 2 occurrences anywhere in the text: the
/// first stays, the later ones go, and `[engraph: + N more lines like the
/// one above]` follows the first. A line with no timestamp collapses only
/// in a run of 3 or more identical lines next to each other (the threshold
/// keeps a coincidental repeat of real output): the first line of the run
/// stays, every later line of the run goes, whatever its length, and the
/// marker counts them. Scanning resumes after the run, so a later run of
/// the same line collapses on its own with its own marker. A line that
/// recurs a block apart stays: it belongs to a different diagnostic — the
/// `|` gutter of a rustc warning, the `left:`/`right:` of a failed
/// assertion — and removing it would leave that diagnostic incomplete. A
/// blank line never collapses.
/// Kept lines are unchanged, so a filter's prefix test sees them as before.
/// The caller gates on `OutputKind`: this runs only for an event stream,
/// never for content. Returns `Cow::Borrowed(text)` when nothing collapses.
pub fn collapse_repeated_lines(text: &str) -> std::borrow::Cow<'_, str> {
    let re = timestamp_prefix_re();
    let lines: Vec<&str> = text.lines().collect();
    let mut stamped: HashMap<&str, usize> = HashMap::new();
    for line in &lines {
        if let Some(k) = log_key(re, line) {
            *stamped.entry(k).or_insert(0) += 1;
        }
    }
    let any_run = lines
        .windows(3)
        .any(|w| run_key(re, w[0]).is_some_and(|t| w[1].trim() == t && w[2].trim() == t));
    if !any_run && !stamped.values().any(|&n| n >= 2) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut seen: HashSet<&str> = HashSet::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let (keep, more, step) = if let Some(k) = log_key(re, line) {
            let n = stamped[k];
            match n {
                1 => (true, 0, 1),
                _ => (seen.insert(k), n - 1, 1),
            }
        } else {
            let run = run_key(re, line).map_or(1, |t| {
                lines[i..].iter().take_while(|l| l.trim() == t).count()
            });
            match run {
                3.. => (true, run - 1, run),
                _ => (true, 0, 1),
            }
        };
        if keep {
            out.push_str(line);
            out.push('\n');
            if more > 0 {
                let _ = writeln!(out, "[engraph: + {more} more lines like the one above]");
            }
        }
        i += step;
    }
    if !text.ends_with('\n') {
        out.pop();
    }
    std::borrow::Cow::Owned(out)
}

/// Strip ANSI escape sequences: CSI (SGR colors, cursor moves, DEC private
/// modes like `\x1b[?25l`) and OSC (`ESC]…BEL` or `ESC]…ESC\\` — terminal
/// hyperlinks, as on cargo's `Finished` line with color forced, and titles).
/// Returns `Cow::Borrowed(s)` when no escapes match, avoiding a clone for
/// the common case of plain-text input.
pub fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\x1b(?:\[[0-9;?]*[a-zA-Z]|\][^\x07\x1b]*(?:\x07|\x1b\\))").unwrap()
    });
    // Overwritten text goes first, while the sequences that prove the
    // overwrite are still present; the regex then removes what remains
    // (SGR colors, DEC private modes).
    match collapse_overwrites(s) {
        std::borrow::Cow::Borrowed(b) => re.replace_all(b, ""),
        std::borrow::Cow::Owned(o) => std::borrow::Cow::Owned(re.replace_all(&o, "").into_owned()),
    }
}

/// Remove the text a terminal would have overwritten. A progress bar is
/// written, then repositioned over: a cursor movement (`ESC[…A`–`H`), a
/// whole-line erase (`ESC[1K`, `ESC[2K`, `ESC[…J`), or a `\r` with more text
/// after it marks the segment before it as overwritten. A segment ended by
/// `\n` is a line and stays (`ESC[K`/`ESC[0K` before the `\n` erases to the
/// right of real text, so the text stays); a newline whose whole segment was
/// overwritten away is dropped with it. Other sequences pass through for
/// `strip_ansi`'s regex. Borrowed and unchanged when the text has no escape
/// and no `\r`.
fn collapse_overwrites(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\x1b') && !s.contains('\r') {
        return std::borrow::Cow::Borrowed(s);
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut line = String::new();
    let mut cleared = false; // the segment lost text to an overwrite
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b if bytes.get(i + 1) == Some(&b'[') => {
                let mut j = i + 2;
                while j < bytes.len() && matches!(bytes[j], b'0'..=b'9' | b';' | b'?') {
                    j += 1;
                }
                let params = &s[i + 2..j];
                match bytes.get(j) {
                    // Cursor movement: the segment was repositioned over.
                    Some(b'A'..=b'H') => {
                        if !line.is_empty() {
                            line.clear();
                            cleared = true;
                        }
                    }
                    // Erase: `K`/`0K` erases right of the text (keep); the
                    // other erases remove what was written before them.
                    Some(b'K') if !(params.is_empty() || params == "0") => {
                        if !line.is_empty() {
                            line.clear();
                            cleared = true;
                        }
                    }
                    Some(b'J') => {
                        if !line.is_empty() {
                            line.clear();
                            cleared = true;
                        }
                    }
                    // SGR and the rest: keep for the regex pass.
                    Some(_) => line.push_str(&s[i..=j]),
                    None => {}
                }
                i = j + 1;
            }
            b'\r' => {
                if bytes.get(i + 1) == Some(&b'\n') {
                    out.push_str(&line);
                    out.push('\n');
                    line.clear();
                    cleared = false;
                    i += 2;
                } else {
                    if !line.is_empty() {
                        line.clear();
                        cleared = true;
                    }
                    i += 1;
                }
            }
            b'\n' => {
                if !(cleared && line.is_empty()) {
                    out.push_str(&line);
                    out.push('\n');
                }
                line.clear();
                cleared = false;
                i += 1;
            }
            _ => {
                let start = i;
                i += 1;
                while i < bytes.len() && !matches!(bytes[i], 0x1b | b'\r' | b'\n') {
                    i += 1;
                }
                line.push_str(&s[start..i]);
            }
        }
    }
    out.push_str(&line);
    std::borrow::Cow::Owned(out)
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
    fn a_run_of_three_or_more_identical_untimestamped_lines_collapses() {
        let text = "start\nError: coverage write failed\nError: coverage write failed\nError: coverage write failed\nend\n";
        assert_eq!(
            collapse_repeated_lines(text),
            "start\nError: coverage write failed\n[engraph: + 2 more lines like the one above]\nend\n"
        );
    }

    #[test]
    fn untimestamped_lines_repeated_a_block_apart_stay() {
        // Each failure of a test run carries the same assertion lines; each
        // copy belongs to its own failure, so none may go.
        let text = "---- a stdout ----\nassertion `left == right` failed\n  left: 1\n right: 2\n\n\
                    ---- b stdout ----\nassertion `left == right` failed\n  left: 1\n right: 2\n\n\
                    ---- c stdout ----\nassertion `left == right` failed\n  left: 1\n right: 2\n";
        let out = collapse_repeated_lines(text);
        assert_eq!(out, text);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn two_identical_untimestamped_lines_stay() {
        // A coincidental repeat of real output; the threshold is 3.
        let text = "same\nmiddle\nsame\n";
        let out = collapse_repeated_lines(text);
        assert_eq!(out, text);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn blank_lines_never_collapse() {
        let text = "a\n\n\n\n\nb\n";
        assert_eq!(collapse_repeated_lines(text), text);
    }

    #[test]
    fn lines_without_a_timestamp_or_with_different_text_are_unchanged() {
        let text = "same\nsame\n10:00:00 a\n10:00:01 b\n";
        let out = collapse_repeated_lines(text);
        assert_eq!(out, text);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn strip_ansi_drops_text_a_rich_console_overwrote() {
        // The gradle --console=rich line of plan 01 bug 18: progress bar
        // fragments positioned over each other with cursor movement, then
        // the real lines, each ended by an erase-to-end and a newline.
        let input = "\n\n\x1b[2A\x1b[1m<\x1b[0;1m-------------> 0% CONFIGURING [109ms]\x1b[m\x1b[38D\x1b[1B\x1b[1m> root project\x1b[m\x1b[14D\x1b[1B\x1b[2A\x1b[1m<\x1b[0;1m-------------> 50% EXECUTING\x1b[m\x1b[26D\x1b[1B\x1b[1m> :test\x1b[m\x1b[7D\x1b[1B\x1b[2ADeprecated Gradle features were used in this build.\x1b[0K\nBUILD SUCCESSFUL in 1s\x1b[0K\n9 actionable tasks: 9 up-to-date\x1b[0K\n";
        assert_eq!(
            strip_ansi(input),
            "\n\nDeprecated Gradle features were used in this build.\nBUILD SUCCESSFUL in 1s\n9 actionable tasks: 9 up-to-date\n"
        );
    }

    #[test]
    fn strip_ansi_overwrite_primitives() {
        // \r with more text after it overwrites the segment before it.
        assert_eq!(strip_ansi("a\rb\n"), "b\n");
        // Erase to the end of the line keeps the text before it.
        assert_eq!(strip_ansi("line\x1b[0K\n"), "line\n");
        assert_eq!(strip_ansi("line\x1b[K\n"), "line\n");
        // A whole-line erase removes the text before it.
        assert_eq!(strip_ansi("x\x1b[2Ky\n"), "y\n");
        // A cursor movement means the segment before it was overwritten.
        assert_eq!(strip_ansi("one\x1b[1Atwo\n"), "two\n");
        // \r\n is a line end, not an overwrite.
        assert_eq!(strip_ansi("keep\r\nnext\n"), "keep\nnext\n");
    }

    #[test]
    fn strip_ansi_keeps_plain_text_borrowed() {
        let plain = "no escapes here\njust text\n";
        assert!(matches!(strip_ansi(plain), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn strip_ansi_removes_osc_sequences() {
        // cargo's Finished line carries an OSC 8 hyperlink when color is
        // forced: ESC ] 8 ; ; url ESC \ around the text.
        let s = "\x1b]8;;https://doc.rust-lang.org/cargo\x1b\\Finished\x1b]8;;\x1b\\ `dev` profile";
        assert_eq!(strip_ansi(s), "Finished `dev` profile");
        // A BEL-terminated OSC (a terminal title) goes too.
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
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
