use super::util::{combine, drop_matching};
use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

/// `go test` — drop `=== RUN`, `--- PASS`, keep `--- FAIL` blocks and `ok pkg X.Xs` summary lines.
pub fn test(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(=== (RUN|PAUSE|CONT)|--- PASS:|PASS$)").unwrap());
    let (filtered, _dropped) = drop_matching(&text, re);
    let mut ok_pkgs = 0_u32;
    let mut fail_pkgs = 0_u32;
    for line in filtered.lines() {
        if line.starts_with("ok  \t") || line.starts_with("ok\t") {
            ok_pkgs += 1;
        }
        if line.starts_with("FAIL\t") {
            fail_pkgs += 1;
        }
    }
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: go test {ok_pkgs} ok, {fail_pkgs} failed pkgs, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "go_test",
    }
}

/// `go build` — passthrough (typically silent on success; errors go to stderr).
pub fn build(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    let mut errors = 0_u32;
    for line in text.lines() {
        if line.contains(": ") && (line.contains("error") || line.contains("undefined:")) {
            errors += 1;
        }
    }
    let mut out = text;
    out.push_str(&format!(
        "[engraph: go build {errors} errors, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "go_build",
    }
}

/// `go vet` — same shape as build.
pub fn vet(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut o = build(ctx);
    o.filter_id = "go_vet";
    o
}

/// `go mod tidy` — drop `go: downloading X` / `go: finding X` chatter.
pub fn mod_tidy(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"^go: (downloading|finding|extracting|upgraded) ").unwrap());
    let (filtered, dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: go mod tidy dropped {dropped} progress lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "go_mod_tidy",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ctx<'a>(stdout: &'a str, stderr: &'a str, exit: i32) -> FilterCtx<'a> {
        FilterCtx {
            cmd: "go",
            args: &[],
            stdout,
            stderr,
            exit_code: exit,
        }
    }

    #[test]
    fn go_test_counts_packages() {
        let stdout = "\
=== RUN   TestA
--- PASS: TestA (0.00s)
=== RUN   TestB
--- PASS: TestB (0.00s)
PASS
ok  \texample.com/m\t0.01s
ok  \texample.com/m/pkg\t0.02s
FAIL\texample.com/m/broken\t0.00s
";
        let out = test(&ctx(stdout, "", 1));
        assert!(!out.text.contains("=== RUN"));
        assert!(!out.text.contains("--- PASS"));
        assert!(out.text.contains("2 ok, 1 failed"));
    }
}
