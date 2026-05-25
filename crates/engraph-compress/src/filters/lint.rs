//! Lint-family filters with a shared shape: drop boilerplate, keep diagnostics
//! and the final summary. Used by ruff, mypy, eslint, tsc.

use super::util::{combine, drop_matching};
use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

pub fn ruff(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^(All checks passed!|Found \d+ errors? \(\d+ fixed\))").unwrap()
    });
    let (filtered, _dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!("[engraph: ruff exit {}]\n", ctx.exit_code));
    FilterOutput {
        text: out,
        filter_id: "ruff",
    }
}

pub fn mypy(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^Success: no issues found").unwrap());
    let (filtered, _dropped) = drop_matching(&text, re);
    let mut errors = 0_u32;
    for line in filtered.lines() {
        if line.contains(": error:") {
            errors += 1;
        }
    }
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: mypy {errors} errors, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "mypy",
    }
}

pub fn eslint(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    // Drop files with no issues (just absolute file paths followed by clean).
    let mut out = String::with_capacity(text.len() / 2);
    let mut errors = 0_u32;
    let mut warnings = 0_u32;
    for line in text.lines() {
        if line.contains(" error") {
            errors += 1;
        }
        if line.contains(" warning") {
            warnings += 1;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "[engraph: eslint {errors} errors, {warnings} warnings, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "eslint",
    }
}

pub fn tsc(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Drop watch-mode chatter; keep error lines.
        Regex::new(r"^(File change detected\.|Watching for file changes\.|Starting compilation)")
            .unwrap()
    });
    let (filtered, _dropped) = drop_matching(&text, re);
    let mut errors = 0_u32;
    for line in filtered.lines() {
        // tsc errors look like "path:line:col - error TSxxxx: message"
        if line.contains(" error TS") {
            errors += 1;
        }
    }
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: tsc {errors} errors, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "tsc",
    }
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
}
