use super::util::{strip_ansi, truncate_lines};
use super::{FilterCtx, FilterOutput};

pub fn rg(ctx: &FilterCtx<'_>) -> FilterOutput {
    // Strip color escapes before capping. rg/grep auto-disable color under a
    // pipe, but a forced `--color=always` (or a RIPGREP_CONFIG_PATH / GREP_OPTIONS
    // that forces it) would otherwise wrap every match in SGR codes — matching
    // every other noisy filter, which strips ANSI. No-op when there's no color.
    let clean = strip_ansi(ctx.stdout);
    FilterOutput {
        text: truncate_lines(&clean, 200, "matches"),
        filter_id: "rg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rg_caps_long_results() {
        let stdout: String = (0..300)
            .map(|i| format!("src/file{i}.rs:10:match\n"))
            .collect();
        let out = rg(&FilterCtx {
            args: &["pattern".to_string()],
            stdout: &stdout,
            stderr: "",
            exit_code: 0,
        });
        assert!(out.text.contains("truncated 100 more matches"));
    }

    #[test]
    fn rg_strips_forced_color() {
        // grep/rg --color=always wraps the match in SGR + erase-line codes.
        let stdout = "util.rs:42:pub \x1b[01;31m\x1b[Kfn\x1b[m\x1b[K truncate_lines\n";
        let out = rg(&FilterCtx {
            args: &["fn".to_string()],
            stdout,
            stderr: "",
            exit_code: 0,
        });
        assert_eq!(out.text, "util.rs:42:pub fn truncate_lines\n");
        assert!(!out.text.contains('\x1b'));
    }
}
