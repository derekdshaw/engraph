use super::util::{combine, drop_matching, truncate_lines};
use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

/// `pytest` — drop progress dots and PASSED lines; keep failures and summary.
pub fn pytest(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Drop pure-dot progress lines and "test_x PASSED" / "test_x[..] PASSED" lines.
        Regex::new(r"^([.sxF]+\s*\[\s*\d+%\]\s*|.*::\S+ PASSED\b.*)$").unwrap()
    });
    let (filtered, dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: dropped {dropped} pytest progress/passed lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "pytest",
    }
}

/// `pip install` — drop "Collecting X" / "Downloading X" / "Using cached X" spam;
/// keep "Successfully installed", errors, deprecation warnings.
pub fn pip_install(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^(Collecting|Downloading|Using cached|Installing collected|Requirement already satisfied|Obtaining)\b").unwrap()
    });
    let (filtered, dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: dropped {dropped} pip progress lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "pip_install",
    }
}

/// `pip list` — cap at 200 lines; keep header.
pub fn pip_list(ctx: &FilterCtx<'_>) -> FilterOutput {
    FilterOutput {
        text: truncate_lines(ctx.stdout, 200, "packages"),
        filter_id: "pip_list",
    }
}

/// `uv install` / `uv sync` / `uv add` — drop resolution/download/build progress.
pub fn uv(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^\s*(Resolved \d+ packages|Downloaded \d+|Built \d+|Prepared \d+|\+ |Updating https?://)").unwrap()
    });
    let (filtered, dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: dropped {dropped} uv progress lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "uv",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(stdout: &'a str, stderr: &'a str, exit: i32) -> FilterCtx<'a> {
        FilterCtx {
            args: &[],
            stdout,
            stderr,
            exit_code: exit,
        }
    }

    #[test]
    fn pytest_drops_passed_keeps_failed() {
        let stdout = "\
test_a.py::test_one PASSED                                              [ 33%]
test_a.py::test_two FAILED                                              [ 66%]
test_a.py::test_three PASSED                                            [100%]

=================== 2 passed, 1 failed in 0.42s =====================
";
        let out = pytest(&ctx(stdout, "", 1));
        assert!(!out.text.contains("test_one PASSED"));
        assert!(out.text.contains("test_two FAILED"));
        assert!(out.text.contains("2 passed, 1 failed"));
    }

    #[test]
    fn pip_install_drops_collecting() {
        let stdout = "\
Collecting requests
Downloading requests-2.31.0-py3-none-any.whl (62 kB)
Collecting charset_normalizer
Using cached charset_normalizer-3.3.2.tar.gz
Successfully installed requests-2.31.0 charset_normalizer-3.3.2
";
        let out = pip_install(&ctx(stdout, "", 0));
        assert!(!out.text.contains("Collecting"));
        assert!(out.text.contains("Successfully installed"));
    }
}
