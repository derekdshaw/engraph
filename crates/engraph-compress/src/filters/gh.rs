use super::util::truncate_lines;
use super::{FilterCtx, FilterOutput};

pub fn list(ctx: &FilterCtx<'_>) -> FilterOutput {
    FilterOutput {
        text: truncate_lines(ctx.stdout, 50, "rows"),
        filter_id: "gh_list",
    }
}

/// `gh pr view` / `gh issue view` — keep title, state, body preview; drop
/// long comment threads past the first three.
pub fn view(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut out = String::with_capacity(ctx.stdout.len() / 2);
    let mut comments_seen = 0_u32;
    let mut in_comment = false;
    for line in ctx.stdout.lines() {
        // gh view renders comments under "--" separators with "author X (Y days ago)".
        if line.starts_with("--") || line.contains(" days ago)") {
            in_comment = true;
            comments_seen += 1;
        }
        if in_comment && comments_seen > 3 {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if comments_seen > 3 {
        out.push_str(&format!("[engraph: hid {} comments]\n", comments_seen - 3));
    }
    FilterOutput {
        text: out,
        filter_id: "gh_view",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_truncates_long_pr_lists() {
        let stdout: String = (0..100).map(|i| format!("#{i} title\n")).collect();
        let out = list(&FilterCtx {
            cmd: "gh",
            args: &["pr".to_string(), "list".to_string()],
            stdout: &stdout,
            stderr: "",
            exit_code: 0,
        });
        assert!(out.text.contains("truncated 50 more rows"));
    }
}
