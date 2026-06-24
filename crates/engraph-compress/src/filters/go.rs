use super::util::{combine, drop_matching};
use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

/// `go test` — drop `=== RUN`, `--- PASS` and passing noise; keep `--- FAIL`
/// blocks and `ok pkg X.Xs` / `FAIL pkg` summary lines. A panicking test dumps
/// `goroutine N [running]:` stacks (dozens of stdlib frames) — those are
/// stripped like cargo's backtraces; the `panic:` message above them is kept.
pub fn test(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static DROP: OnceLock<Regex> = OnceLock::new();
    let drop_re =
        DROP.get_or_init(|| Regex::new(r"^(=== (RUN|PAUSE|CONT)|--- PASS:|PASS$)").unwrap());
    static GOROUTINE: OnceLock<Regex> = OnceLock::new();
    let goroutine_re = GOROUTINE.get_or_init(|| Regex::new(r"^goroutine \d+ \[").unwrap());

    let mut out = String::with_capacity(text.len() / 2);
    let mut ok_pkgs = 0_u32;
    let mut fail_pkgs = 0_u32;
    let mut in_dump = false;
    for line in text.lines() {
        let t = line.trim_start();
        if in_dump {
            // A blank line, the package summary, or a test-structure line ends
            // the goroutine dump.
            if t.is_empty() {
                in_dump = false;
                continue;
            }
            if t.starts_with("FAIL\t")
                || t.starts_with("ok\t")
                || t.starts_with("ok  \t")
                || t.starts_with("--- ")
                || t.starts_with("=== ")
                || t.starts_with("exit status")
            {
                in_dump = false; // fall through and emit this boundary line
            } else {
                continue;
            }
        }
        if goroutine_re.is_match(t) {
            in_dump = true;
            continue;
        }
        if drop_re.is_match(t) {
            continue;
        }
        if line.starts_with("ok  \t") || line.starts_with("ok\t") {
            ok_pkgs += 1;
        }
        if line.starts_with("FAIL\t") {
            fail_pkgs += 1;
        }
        out.push_str(line);
        out.push('\n');
    }
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

fn mod_progress_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^go: (downloading|finding|extracting|upgraded) ").unwrap())
}

/// `go mod tidy` — drop `go: downloading X` / `go: finding X` chatter.
pub fn mod_tidy(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    let (filtered, dropped) = drop_matching(&text, mod_progress_re());
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

/// `go mod download` — same download chatter as `tidy`.
pub fn mod_download(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    let (filtered, dropped) = drop_matching(&text, mod_progress_re());
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: go mod download dropped {dropped} progress lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "go_mod_download",
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

    #[test]
    fn go_test_strips_goroutine_dump() {
        let stdout = "\
=== RUN   TestPanics
--- FAIL: TestPanics (0.00s)
panic: boom [recovered]

goroutine 19 [running]:
testing.tRunner.func1.2({0x10a}, {0x20b})
\t/usr/local/go/src/testing/testing.go:1545 +0x3e6
testing.tRunner.func1()
\t/usr/local/go/src/testing/testing.go:1548 +0x35e
example.com/m.TestPanics(0x140001)
\t/home/u/m/x_test.go:10 +0x18
exit status 2
FAIL\texample.com/m\t0.012s
";
        let out = test(&ctx(stdout, "", 1));
        assert!(out.text.contains("panic: boom"));
        assert!(out.text.contains("--- FAIL: TestPanics"));
        assert!(!out.text.contains("goroutine 19"));
        assert!(!out.text.contains("testing.tRunner"));
        assert!(!out.text.contains("/usr/local/go/src/testing"));
        assert!(out.text.contains("FAIL\texample.com/m"));
        assert!(out.text.contains("0 ok, 1 failed"));
    }

    #[test]
    fn go_test_keeps_nonpanic_failures() {
        let stdout = "\
=== RUN   TestThing
    x_test.go:12: got 4, want 5
--- FAIL: TestThing (0.00s)
FAIL
FAIL\texample.com/m\t0.005s
";
        let out = test(&ctx(stdout, "", 1));
        assert!(out.text.contains("got 4, want 5"));
        assert!(out.text.contains("--- FAIL: TestThing"));
    }
}
