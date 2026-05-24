use crate::CompressKind;
use regex::Regex;
use std::sync::OnceLock;

pub(crate) fn apply(text: &str, kind: CompressKind) -> String {
    match kind {
        CompressKind::ToolOutput => tool_output(text),
        CompressKind::SessionMessage => session_message(text),
        CompressKind::ProjectNotes => project_notes(text),
        CompressKind::Generic => text.to_string(),
    }
}

fn tool_output(text: &str) -> String {
    let stripped = strip_ansi(text);
    let no_progress = drop_progress_lines(&stripped);
    dedupe_consecutive(&no_progress)
}

fn session_message(text: &str) -> String {
    let no_envelope = strip_tool_envelope_lines(text);
    truncate_blobs(&no_envelope)
}

fn project_notes(text: &str) -> String {
    let no_html_comments = strip_html_comments(text);
    collapse_blank_lines(&no_html_comments)
}

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // ESC [ ... letter — typical SGR / cursor codes
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]").unwrap())
}

fn progress_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Lines that look like progress: contain \r or are mostly digits/percent/equal/hash
    RE.get_or_init(|| Regex::new(r"^[\s#=>\-_.\d%/|]*$").unwrap())
}

fn blob_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // base64 or hex run >= 80 chars
    RE.get_or_init(|| Regex::new(r"(?:[A-Za-z0-9+/=]{80,}|[0-9a-fA-F]{80,})").unwrap())
}

fn html_comment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<!--.*?-->").unwrap())
}

fn strip_ansi(s: &str) -> String {
    ansi_re().replace_all(s, "").to_string()
}

fn drop_progress_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        if line.contains('\r') {
            // carriage-return progress overwrites; keep only last segment
            let last = line.rsplit('\r').next().unwrap_or("");
            if !last.trim().is_empty() && !progress_re().is_match(last) {
                out.push_str(last);
                out.push('\n');
            }
            continue;
        }
        if !line.trim().is_empty() && progress_re().is_match(line) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn dedupe_consecutive(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev: Option<String> = None;
    let mut count = 0u32;
    let flush = |buf: &mut String, prev: &Option<String>, count: u32| {
        if let Some(p) = prev {
            buf.push_str(p);
            if count > 1 {
                buf.push_str(&format!(" (x{count})"));
            }
            buf.push('\n');
        }
    };
    for line in s.lines() {
        match &prev {
            Some(p) if p == line => count += 1,
            _ => {
                flush(&mut out, &prev, count);
                prev = Some(line.to_string());
                count = 1;
            }
        }
    }
    flush(&mut out, &prev, count);
    out
}

fn strip_tool_envelope_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let t = line.trim_start();
        if t.starts_with(r#"{"type":"tool_use""#) || t.starts_with(r#"{"type":"tool_result""#)
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn truncate_blobs(s: &str) -> String {
    blob_re()
        .replace_all(s, |c: &regex::Captures| {
            let full = &c[0];
            let n = full.len();
            let head = &full[..32];
            let tail = &full[n - 32..];
            format!("{head}…[{n}B]…{tail}")
        })
        .to_string()
}

fn strip_html_comments(s: &str) -> String {
    html_comment_re().replace_all(s, "").to_string()
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0u32;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run < 2 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_color() {
        let s = "\x1b[31mred\x1b[0m text";
        assert_eq!(strip_ansi(s), "red text");
    }

    #[test]
    fn dedupe_counts_repeats() {
        let s = "a\na\na\nb\n";
        let out = dedupe_consecutive(s);
        assert!(out.contains("a (x3)"));
        assert!(out.contains("b\n"));
    }

    #[test]
    fn truncate_blob_keeps_head_tail() {
        let hex = "a".repeat(200);
        let out = truncate_blobs(&hex);
        assert!(out.contains("…[200B]…"));
        assert!(out.len() < hex.len());
    }

    #[test]
    fn strip_envelope_drops_tool_lines() {
        let s = "ok\n{\"type\":\"tool_use\",\"name\":\"X\"}\nkeep\n";
        let out = strip_tool_envelope_lines(s);
        assert!(!out.contains("tool_use"));
        assert!(out.contains("keep"));
    }
}
