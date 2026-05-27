//! Lint-family filters with a shared shape: drop boilerplate, count diagnostics,
//! append a summary trailer. Used by ruff, mypy, eslint, tsc.

use super::util::{combine, drop_matching};
use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

/// Common pipeline: combine stdout+stderr → optionally drop noise lines → count
/// error/warning patterns → append `[engraph: <name>[ N errors[, M warnings]], exit C]`.
fn lint_common(
    ctx: &FilterCtx<'_>,
    filter_id: &'static str,
    drop_re: Option<&Regex>,
    error_pat: Option<&str>,
    warn_pat: Option<&str>,
) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    let filtered = match drop_re {
        Some(re) => drop_matching(&text, re).0,
        None => text,
    };
    let errors = error_pat.map(|p| filtered.lines().filter(|l| l.contains(p)).count());
    let warnings = warn_pat.map(|p| filtered.lines().filter(|l| l.contains(p)).count());

    let mut out = filtered;
    out.push_str("[engraph: ");
    out.push_str(filter_id);
    let mut sep = " ";
    if let Some(e) = errors {
        out.push_str(&format!("{sep}{e} errors"));
        sep = ", ";
    }
    if let Some(w) = warnings {
        out.push_str(&format!("{sep}{w} warnings"));
        sep = ", ";
    }
    out.push_str(&format!("{sep}exit {}]\n", ctx.exit_code));
    FilterOutput {
        text: out,
        filter_id,
    }
}

pub fn ruff(ctx: &FilterCtx<'_>) -> FilterOutput {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^(All checks passed!|Found \d+ errors? \(\d+ fixed\))").unwrap()
    });
    lint_common(ctx, "ruff", Some(re), None, None)
}

pub fn mypy(ctx: &FilterCtx<'_>) -> FilterOutput {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^Success: no issues found").unwrap());
    lint_common(ctx, "mypy", Some(re), Some(": error:"), None)
}

pub fn eslint(ctx: &FilterCtx<'_>) -> FilterOutput {
    lint_common(ctx, "eslint", None, Some(" error"), Some(" warning"))
}

pub fn tsc(ctx: &FilterCtx<'_>) -> FilterOutput {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Drop watch-mode chatter; keep error lines.
        Regex::new(r"^(File change detected\.|Watching for file changes\.|Starting compilation)")
            .unwrap()
    });
    // tsc errors look like "path:line:col - error TSxxxx: message"
    lint_common(ctx, "tsc", Some(re), Some(" error TS"), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ctx<'a>(stdout: &'a str, stderr: &'a str, exit: i32) -> FilterCtx<'a> {
        FilterCtx {
            cmd: "x",
            args: &[],
            stdout,
            stderr,
            exit_code: exit,
        }
    }

    #[test]
    fn mypy_counts_errors() {
        let stdout = "\
foo.py:10: error: incompatible types
foo.py:20: error: missing return
Found 2 errors in 1 file
";
        let out = mypy(&ctx(stdout, "", 1));
        assert!(out.text.contains("mypy 2 errors"));
    }

    #[test]
    fn tsc_counts_ts_errors() {
        let stdout = "\
src/foo.ts:10:5 - error TS2304: Cannot find name 'foo'.
src/bar.ts:1:1 - error TS2345: Argument type mismatch
";
        let out = tsc(&ctx(stdout, "", 1));
        assert!(out.text.contains("tsc 2 errors"));
    }

    #[test]
    fn ruff_summary_has_no_count() {
        let out = ruff(&ctx("All checks passed!\n", "", 0));
        // ruff produces no count, just `[engraph: ruff exit 0]`.
        assert!(out.text.contains("[engraph: ruff exit 0]"));
    }

    #[test]
    fn eslint_counts_errors_and_warnings() {
        let stdout = "\
file.js:1:1  error  Bad
file.js:2:1  warning  Meh
file.js:3:1  error  Worse
";
        let out = eslint(&ctx(stdout, "", 1));
        assert!(out.text.contains("eslint 2 errors, 1 warnings"));
    }
}
