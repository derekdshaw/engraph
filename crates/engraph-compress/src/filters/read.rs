//! Read-bucket filter for `cat`/`bat`/`less` (whole-file reads) and
//! `head`/`tail` (user-already-windowed reads). Strips line comments by
//! extension, collapses blank-line runs, and for whole-file reads applies a
//! head + elided-middle + tail window when the file exceeds the cap.
//! Falls back to raw text if language stripping accidentally emptied input.

use super::{FilterCtx, FilterOutput, util};

const CAT_HEAD_LINES: usize = 400;
const CAT_TAIL_LINES: usize = 100;

pub fn cat(ctx: &FilterCtx<'_>) -> FilterOutput {
    let raw = util::combine(ctx.stdout, ctx.stderr);
    let path = extract_file_path(ctx.args);
    let text = apply_language_strip(&raw, path.as_deref());
    let text = util::dedup_consecutive(&text);
    let text = window(&text, CAT_HEAD_LINES, CAT_TAIL_LINES);
    let text = fallback_if_emptied(&raw, text);
    FilterOutput {
        text,
        filter_id: "read_cat",
    }
}

pub fn head_tail(ctx: &FilterCtx<'_>) -> FilterOutput {
    // The user already chose a window via `head -n` / `tail -n`. Don't
    // re-window; just strip comments and dedup blanks.
    let raw = util::combine(ctx.stdout, ctx.stderr);
    let path = extract_file_path(ctx.args);
    let text = apply_language_strip(&raw, path.as_deref());
    let text = util::dedup_consecutive(&text);
    let text = fallback_if_emptied(&raw, text);
    FilterOutput {
        text,
        filter_id: "read_head_tail",
    }
}

fn extract_file_path(args: &[String]) -> Option<String> {
    // Last non-flag, non-numeric arg. Numeric skip handles `-n 100` where
    // shell_words leaves "100" as its own token.
    args.iter()
        .rev()
        .find(|a| !a.starts_with('-') && !a.chars().all(|c| c.is_ascii_digit()))
        .cloned()
}

fn apply_language_strip(text: &str, path: Option<&str>) -> String {
    let Some(p) = path else {
        return text.to_string();
    };
    let ext = std::path::Path::new(p)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let Some(markers) = comment_markers_for(ext) else {
        return text.to_string();
    };
    strip_line_comments(text, markers)
}

fn comment_markers_for(ext: &str) -> Option<&'static [&'static str]> {
    match ext {
        "py" => Some(&["#"]),
        "rs" | "go" | "js" | "ts" | "jsx" | "tsx" => Some(&["//"]),
        _ => None,
    }
}

fn strip_line_comments(text: &str, markers: &[&str]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_blank = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if markers.iter().any(|m| trimmed.starts_with(m)) {
            continue;
        }
        if trimmed.is_empty() {
            if prev_blank {
                continue;
            }
            prev_blank = true;
        } else {
            prev_blank = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn window(text: &str, head_n: usize, tail_n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= head_n + tail_n {
        return text.to_string();
    }
    let head = &lines[..head_n];
    let tail = &lines[lines.len() - tail_n..];
    let elided = lines.len() - head_n - tail_n;
    format!(
        "{}\n[engraph: omitted {} middle lines]\n{}\n",
        head.join("\n"),
        elided,
        tail.join("\n")
    )
}

fn fallback_if_emptied(raw: &str, filtered: String) -> String {
    if filtered.trim().is_empty() && !raw.trim().is_empty() {
        format!("[engraph: filter emptied input; raw follows]\n{raw}")
    } else {
        filtered
    }
}
