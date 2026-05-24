use super::{FilterCtx, FilterOutput};

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

/// `fd` / `find` results — usually one path per line. Just cap total lines.
pub fn fd(ctx: &FilterCtx<'_>) -> FilterOutput {
    let lines: Vec<&str> = ctx.stdout.lines().collect();
    let text = if lines.len() > MAX_LINES {
        let kept = &lines[..MAX_LINES];
        format!(
            "{}\n[engraph: truncated {} more paths]\n",
            kept.join("\n"),
            lines.len() - MAX_LINES
        )
    } else {
        ctx.stdout.to_string()
    };
    FilterOutput {
        text,
        filter_id: "fd",
    }
}

/// `ls` — same shape as fd.
pub fn ls(ctx: &FilterCtx<'_>) -> FilterOutput {
    let r = fd(ctx);
    FilterOutput {
        text: r.text,
        filter_id: "ls",
    }
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
            cmd: "tree",
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
