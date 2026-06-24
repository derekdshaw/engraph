use super::{FilterCtx, FilterOutput};
use std::collections::HashMap;
use std::fmt::Write;

const MAX_DEPTH: usize = 3;
const MAX_LINES: usize = 200;

/// `tree` — limit to first MAX_DEPTH levels of indentation; truncate at MAX_LINES.
pub fn tree(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut out = String::with_capacity(ctx.stdout.len());
    let mut kept = 0_usize;
    let mut deeper_truncated = 0_usize;
    for line in ctx.stdout.lines() {
        let depth = indent_depth(line);
        if depth > MAX_DEPTH {
            deeper_truncated += 1;
            continue;
        }
        if kept >= MAX_LINES {
            break;
        }
        out.push_str(line);
        out.push('\n');
        kept += 1;
    }
    if deeper_truncated > 0 {
        out.push_str(&format!(
            "[engraph: hid {deeper_truncated} entries below depth {MAX_DEPTH}]\n"
        ));
    }
    FilterOutput {
        text: out,
        filter_id: "tree",
    }
}

/// `fd` results — a flat path list. Group files under a shared parent directory
/// so a deep prefix is printed once instead of on every sibling.
pub fn fd(ctx: &FilterCtx<'_>) -> FilterOutput {
    FilterOutput {
        text: group_paths_by_dir(ctx.stdout, MAX_LINES),
        filter_id: "fd",
    }
}

/// `find` — same flat path list as `fd` (often `./`-prefixed). Group by directory.
pub fn find_cmd(ctx: &FilterCtx<'_>) -> FilterOutput {
    FilterOutput {
        text: group_paths_by_dir(ctx.stdout, MAX_LINES),
        filter_id: "find",
    }
}

/// `ls` — a single directory's entries; no parent paths to factor out, so just
/// cap the line count (grouping would collapse everything under one `./`).
pub fn ls(ctx: &FilterCtx<'_>) -> FilterOutput {
    FilterOutput {
        text: cap_lines(ctx.stdout, MAX_LINES, "paths"),
        filter_id: "ls",
    }
}

fn cap_lines(text: &str, max: usize, unit: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.to_string();
    }
    format!(
        "{}\n[engraph: truncated {} more {unit}]\n",
        lines[..max].join("\n"),
        lines.len() - max
    )
}

/// Group `find`/`fd` paths by parent directory: each directory is printed once
/// as `dir/ (N): a, b, c`. Files with no `/` and directories holding a single
/// entry stay on their original line, so a list of all-unique paths is byte-for-
/// byte unchanged (no regression) and only repeated prefixes are factored out.
fn group_paths_by_dir(text: &str, max: usize) -> String {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<&str, Vec<&str>> = HashMap::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        match line.rfind('/') {
            Some(i) => {
                let dir = &line[..=i]; // keep the trailing slash
                let file = &line[i + 1..];
                if !groups.contains_key(dir) {
                    order.push(dir.to_string());
                }
                groups.entry(dir).or_default().push(file);
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
    for key in &order {
        if emitted >= max {
            break;
        }
        let files = &groups[key.as_str()];
        match files.len() {
            0 => {
                // Rootless path (e.g. `Cargo.toml`) — keep verbatim.
                out.push_str(key);
                out.push('\n');
                emitted += 1;
            }
            1 => {
                // Reconstruct the original `dir/file` line; no dedup to gain.
                out.push_str(key);
                out.push_str(files[0]);
                out.push('\n');
                emitted += 1;
            }
            _ => {
                let take = (max - emitted).min(files.len());
                let _ = writeln!(out, "{key} ({}): {}", files.len(), files[..take].join(", "));
                emitted += take;
            }
        }
    }

    let remaining = total.saturating_sub(emitted);
    if remaining > 0 {
        let _ = writeln!(out, "[engraph: truncated {remaining} more paths]");
    }
    out
}

fn indent_depth(line: &str) -> usize {
    // `tree` uses 4-char indent per level: "│   ", "    ", "├── ", "└── "
    let mut count = 0_usize;
    let mut chars = line.chars();
    loop {
        let chunk: String = (&mut chars).take(4).collect();
        if chunk.is_empty() {
            break;
        }
        match chunk.as_str() {
            "│   " | "    " => count += 1,
            "├── " | "└── " => {
                count += 1;
                break;
            }
            _ => break,
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_depth_basic() {
        assert_eq!(indent_depth(".git"), 0);
        assert_eq!(indent_depth("├── Cargo.toml"), 1);
        assert_eq!(indent_depth("│   ├── lib.rs"), 2);
        assert_eq!(indent_depth("│   │   └── deep.rs"), 3);
    }

    fn ctx<'a>(stdout: &'a str) -> FilterCtx<'a> {
        FilterCtx {
            args: &[],
            stdout,
            stderr: "",
            exit_code: 0,
        }
    }

    #[test]
    fn fd_groups_files_under_shared_dir() {
        let stdout = "src/filters/a.rs\nsrc/filters/b.rs\nsrc/filters/c.rs\nsrc/lib.rs\n";
        let o = fd(&ctx(stdout));
        assert_eq!(o.text, "src/filters/ (3): a.rs, b.rs, c.rs\nsrc/lib.rs\n");
        assert_eq!(o.filter_id, "fd");
    }

    #[test]
    fn fd_all_unique_paths_unchanged() {
        // One file per directory: nothing to factor out, output == input.
        let stdout = "a/x.rs\nb/y.rs\ntop.txt\n";
        let o = fd(&ctx(stdout));
        assert_eq!(o.text, stdout);
    }

    #[test]
    fn find_groups_dotslash_paths() {
        let stdout = "./src/a.rs\n./src/b.rs\n";
        let o = find_cmd(&ctx(stdout));
        assert_eq!(o.text, "./src/ (2): a.rs, b.rs\n");
        assert_eq!(o.filter_id, "find");
    }

    #[test]
    fn ls_is_capped_not_grouped() {
        let stdout = "a.rs\nb.rs\nc.rs\n";
        let o = ls(&ctx(stdout));
        assert_eq!(o.text, stdout);
    }

    #[test]
    fn tree_caps_depth() {
        let stdout = "\
.
├── Cargo.toml
├── src
│   ├── lib.rs
│   │   ├── deeper.rs
│   │   │   └── way_deep.rs
│   └── main.rs
└── README.md
";
        let o = tree(&FilterCtx {
            args: &[],
            stdout,
            stderr: "",
            exit_code: 0,
        });
        assert!(o.text.contains("Cargo.toml"));
        assert!(!o.text.contains("way_deep.rs"));
        assert!(o.text.contains("hid 1 entries"));
    }
}
