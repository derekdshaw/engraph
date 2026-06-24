use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

/// Shared core for the compile-diagnostic commands. Drops `Compiling`/`Finished`
/// progress and counts warnings/errors. When `collapse_warnings` is set, each
/// warning is reduced to its `warning:` header + `-->` location, dropping the
/// `|` code-snippet rendering and `= note`/`= help` boilerplate — that body is
/// reconstructable from `file:line:col`. Errors are *always* kept in full: a
/// compile error is what the caller must act on. A truly blank line ends a
/// diagnostic's rendered body.
fn build_common(
    ctx: &FilterCtx<'_>,
    filter_id: &'static str,
    collapse_warnings: bool,
) -> FilterOutput {
    let mut combined = String::with_capacity(ctx.stdout.len() + ctx.stderr.len());
    combined.push_str(ctx.stdout);
    combined.push_str(ctx.stderr);
    let progress = progress_re();
    let mut out = String::with_capacity(combined.len() / 4);
    let mut warnings = 0_u32;
    let mut errors = 0_u32;

    // Body of the diagnostic we're currently inside.
    enum Body {
        None,
        KeepAll,
        // Collapsing a warning: emit only its first `-->` location, drop the rest.
        CollapseWarn { loc_emitted: bool },
    }
    let mut body = Body::None;

    for line in combined.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            // A blank line terminates a body; keep it only inside a kept block so
            // error formatting stays intact.
            if matches!(body, Body::KeepAll) {
                out.push_str(line);
                out.push('\n');
            }
            body = Body::None;
            continue;
        }
        if progress.is_match(trimmed) {
            continue;
        }
        if trimmed.starts_with("warning:") {
            warnings += 1;
            out.push_str(line);
            out.push('\n');
            body = if collapse_warnings {
                Body::CollapseWarn { loc_emitted: false }
            } else {
                Body::KeepAll
            };
            continue;
        }
        if trimmed.starts_with("error") {
            errors += 1;
            out.push_str(line);
            out.push('\n');
            body = Body::KeepAll;
            continue;
        }
        match &mut body {
            Body::CollapseWarn { loc_emitted } if !*loc_emitted && trimmed.starts_with("-->") => {
                out.push_str(line);
                out.push('\n');
                *loc_emitted = true;
            }
            Body::CollapseWarn { .. } => {} // drop the snippet / note body
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out.push_str(&format!(
        "[engraph: {warnings} warnings, {errors} errors, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id,
    }
}

/// `cargo build` — collapse warnings to header + location; keep errors + result.
pub fn build(ctx: &FilterCtx<'_>) -> FilterOutput {
    build_common(ctx, "cargo_build", true)
}

/// `cargo check` — same compile-diagnostic shape as build.
pub fn check(ctx: &FilterCtx<'_>) -> FilterOutput {
    build_common(ctx, "cargo_check", true)
}

/// `cargo clippy` — keep warnings in FULL. Clippy's lints (and their `help:`
/// rendering) are the whole point of running it, so they are never collapsed.
pub fn clippy(ctx: &FilterCtx<'_>) -> FilterOutput {
    build_common(ctx, "cargo_clippy", false)
}

/// `cargo test` — keep test summary lines (running N tests, test results, FAILED panics),
/// drop passing test name lines ("test foo ... ok"), keep failures and their context.
/// Recognizes both libtest output (`test foo ... ok` / `---- foo stdout ----`) and
/// cargo-nextest output (`PASS [   0.005s] pkg test` / `FAIL [   0.005s] pkg test`).
pub fn test(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut combined = String::with_capacity(ctx.stdout.len() + ctx.stderr.len());
    combined.push_str(ctx.stdout);
    combined.push_str(ctx.stderr);
    let progress = progress_re();
    let pass_re = test_pass_re();
    let nextest_pass_re = nextest_pass_re();
    let nextest_fail_re = nextest_fail_re();
    let mut out = String::with_capacity(combined.len() / 3);
    let mut total = 0_u32;
    let mut passed = 0_u32;
    let mut failed = 0_u32;
    let mut ignored = 0_u32;
    // Inside a `stack backtrace:` block: drop the frames. The panic header and
    // message (already emitted above the backtrace) are what's actionable; the
    // stdlib frames (`rust_begin_unwind`, `at /rustc/.../library/…`) are noise.
    let mut in_backtrace = false;
    for line in combined.lines() {
        let trimmed = line.trim_start();
        if in_backtrace {
            // A blank line or the start of the next block/summary ends the trace.
            if trimmed.is_empty()
                || trimmed.starts_with("----")
                || trimmed.starts_with("test result:")
                || trimmed.starts_with("failures:")
            {
                in_backtrace = false; // fall through and handle this boundary line
            } else {
                continue;
            }
        }
        if trimmed == "stack backtrace:" {
            in_backtrace = true;
            continue;
        }
        // The standalone backtrace hint notes carry no debugging signal.
        if trimmed.starts_with("note: run with `RUST_BACKTRACE")
            || trimmed.starts_with("note: Some details are omitted")
        {
            continue;
        }
        if progress.is_match(trimmed) {
            continue;
        }
        if let Some(c) = pass_re.captures(trimmed) {
            total += 1;
            match &c[1] {
                "ok" => passed += 1,
                "ignored" => ignored += 1,
                _ => {}
            }
            continue;
        }
        // cargo-nextest pass lines: drop them, count them. nextest never emits
        // libtest-style `test foo ... ok`, so the two paths can't double-count.
        if nextest_pass_re.is_match(trimmed) {
            total += 1;
            passed += 1;
            continue;
        }
        // Count failures only on the per-failure block header to avoid double-
        // counting against `... FAILED` lines and the summary `test result: FAILED`.
        if trimmed.starts_with("---- ") && trimmed.ends_with("stdout ----") {
            failed += 1;
        }
        // cargo-nextest failure header: `FAIL [   0.005s] pkg::test_name`.
        // Counted as a total too — nextest doesn't emit a libtest-style pass
        // line for failures, so its FAILs aren't represented anywhere else.
        if nextest_fail_re.is_match(trimmed) {
            total += 1;
            failed += 1;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "[engraph: tests {total} (passed {passed}, failed {failed}, ignored {ignored}), exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "cargo_test",
    }
}

/// `cargo doc` — same compile-diagnostic pattern as `cargo build`.
pub fn doc(ctx: &FilterCtx<'_>) -> FilterOutput {
    build_common(ctx, "cargo_doc", true)
}

/// `cargo bench` — drop progress, keep benchmark result lines (typically
/// `test name ... bench: X ns/iter`).
pub fn bench(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut combined = String::with_capacity(ctx.stdout.len() + ctx.stderr.len());
    combined.push_str(ctx.stdout);
    combined.push_str(ctx.stderr);
    let progress = progress_re();
    let mut out = String::with_capacity(combined.len() / 3);
    let mut benches = 0_u32;
    for line in combined.lines() {
        let trimmed = line.trim_start();
        if progress.is_match(trimmed) {
            continue;
        }
        if trimmed.starts_with("test ") && trimmed.contains("bench:") {
            benches += 1;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "[engraph: cargo bench {benches} results, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "cargo_bench",
    }
}

/// `cargo audit` — drop database-fetch chatter; keep vulnerability blocks.
pub fn audit(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut combined = String::with_capacity(ctx.stdout.len() + ctx.stderr.len());
    combined.push_str(ctx.stdout);
    combined.push_str(ctx.stderr);
    let mut out = String::with_capacity(combined.len() / 2);
    for line in combined.lines() {
        let t = line.trim_start();
        if t.starts_with("Fetching advisory database")
            || t.starts_with("Loaded")
            || t.starts_with("Updating crates.io index")
            || t.starts_with("Scanning")
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("[engraph: cargo audit exit {}]\n", ctx.exit_code));
    FilterOutput {
        text: out,
        filter_id: "cargo_audit",
    }
}

/// `cargo tree` — truncate by depth using the same indent-aware logic as the
/// `tree` filter.
pub fn tree_cmd(ctx: &FilterCtx<'_>) -> FilterOutput {
    use crate::filters::tree;
    let r = tree::tree(ctx);
    FilterOutput {
        text: r.text,
        filter_id: "cargo_tree",
    }
}

fn progress_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(Compiling|Checking|Downloading|Downloaded|Updating|Fresh|Finished|Building|Running|Documenting)\b",
        )
        .unwrap()
    })
}

fn test_pass_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // matches: "test foo::bar ... ok" / "test foo ... ignored"
    RE.get_or_init(|| Regex::new(r"^test [^\s]+ \.\.\. (ok|ignored)\b").unwrap())
}

fn nextest_pass_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // matches: "PASS [   0.005s] pkg test::name"
    RE.get_or_init(|| Regex::new(r"^PASS \[\s*\d+\.\d+s\]").unwrap())
}

fn nextest_fail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // matches: "FAIL [   0.005s] pkg test::name"
    RE.get_or_init(|| Regex::new(r"^FAIL \[\s*\d+\.\d+s\]").unwrap())
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
    fn build_drops_progress_keeps_warnings() {
        let stderr = "\
   Compiling foo v0.1.0
   Compiling bar v0.2.0
warning: unused variable: `x`
   --> src/lib.rs:10:9
    Finished `dev` profile [unoptimized] target(s) in 1.23s
";
        let o = build(&ctx("", stderr, 0));
        assert!(!o.text.contains("Compiling foo"));
        assert!(!o.text.contains("Finished"));
        assert!(o.text.contains("warning: unused variable"));
        assert!(o.text.contains("1 warnings"));
    }

    #[test]
    fn nextest_failures_counted_and_pass_lines_dropped() {
        // cargo-nextest writes one line per test on stderr. We count PASS / FAIL
        // and drop the per-test PASS lines for compression.
        let stderr = "\
        Starting 5 tests across 1 binary
        PASS [   0.005s] my_crate test_a
        PASS [   0.006s] my_crate test_b
        FAIL [   0.010s] my_crate test_c
        PASS [   0.004s] my_crate test_d
        FAIL [   0.011s] my_crate test_e
------------
     Summary [   0.012s] 5 tests run: 3 passed, 2 failed
";
        let o = test(&ctx("", stderr, 1));
        // PASS lines should be dropped from output.
        assert!(
            !o.text.contains("PASS ["),
            "PASS lines should be dropped: {}",
            o.text
        );
        // FAIL lines should be retained.
        assert!(o.text.contains("FAIL ["));
        // Counts are correct.
        assert!(
            o.text.contains("tests 5 (passed 3, failed 2"),
            "expected nextest counts in summary: {}",
            o.text,
        );
    }

    #[test]
    fn nextest_and_libtest_dont_double_count() {
        // Pure libtest input must not gain spurious nextest counts.
        let stdout = "\
running 2 tests
test foo::a ... ok
test foo::b ... ok

test result: ok. 2 passed; 0 failed; 0 ignored
";
        let o = test(&ctx(stdout, "", 0));
        assert!(o.text.contains("tests 2 (passed 2, failed 0"));
    }

    #[test]
    fn test_drops_passing_keeps_summary() {
        let stdout = "\
running 3 tests
test foo::a ... ok
test foo::b ... ok
test foo::c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored
";
        let o = test(&ctx(stdout, "", 0));
        assert!(!o.text.contains("test foo::a"));
        assert!(o.text.contains("running 3 tests"));
        assert!(o.text.contains("test result"));
        assert!(o.text.contains("tests 3 (passed 3, failed 0, ignored 0)"));
    }

    #[test]
    fn test_strips_backtrace_keeps_panic() {
        let stdout = "\
running 1 test
test tests::it_panics ... FAILED

failures:

---- tests::it_panics stdout ----

thread 'tests::it_panics' panicked at src/lib.rs:11:9:
assertion `left == right` failed
  left: 4
 right: 5
stack backtrace:
   0: __rustc::rust_begin_unwind
             at /rustc/abc/library/std/src/panicking.rs:689:5
   1: core::panicking::panic_fmt
             at /rustc/abc/library/core/src/panicking.rs:80:14
   2: cargogap::tests::it_panics
             at ./src/lib.rs:11:9
note: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.

failures:
    tests::it_panics

test result: FAILED. 0 passed; 1 failed; 0 ignored
";
        let o = test(&ctx(stdout, "", 101));
        // Panic header + message survive.
        assert!(o.text.contains("panicked at src/lib.rs:11:9"));
        assert!(o.text.contains("assertion `left == right` failed"));
        assert!(o.text.contains("left: 4"));
        // Backtrace frames and notes are gone.
        assert!(!o.text.contains("stack backtrace:"));
        assert!(!o.text.contains("rust_begin_unwind"));
        assert!(!o.text.contains("/rustc/abc/library"));
        assert!(!o.text.contains("Some details are omitted"));
        // Block boundaries / summary preserved.
        assert!(o.text.contains("---- tests::it_panics stdout ----"));
        assert!(o.text.contains("test result: FAILED"));
        // libtest doesn't count failures toward `total` (only pass/ignored do).
        assert!(o.text.contains("tests 0 (passed 0, failed 1"));
    }

    #[test]
    fn build_collapses_warning_body() {
        let stderr = "\
warning: unused variable: `x`
 --> src/lib.rs:2:19
  |
2 | pub fn f1() { let x = 5; }
  |                   ^ help: prefix it with an underscore: `_x`
  |
  = note: `#[warn(unused_variables)]` on by default

    Finished `dev` profile in 0.1s
";
        let o = build(&ctx("", stderr, 0));
        assert!(o.text.contains("warning: unused variable: `x`"));
        assert!(o.text.contains("--> src/lib.rs:2:19"));
        // The `|` snippet rendering and `= note` boilerplate are dropped.
        assert!(!o.text.contains("pub fn f1()"));
        assert!(!o.text.contains("= note"));
        assert!(!o.text.contains('^'));
        assert!(o.text.contains("1 warnings, 0 errors"));
    }

    #[test]
    fn build_keeps_errors_in_full() {
        let stderr = "\
error[E0308]: mismatched types
 --> src/lib.rs:3:5
  |
3 |     foo()
  |     ^^^^^ expected `i32`, found `()`
  |
  = note: expected type `i32`

error: aborting due to 1 previous error
";
        let o = build(&ctx("", stderr, 101));
        // Error body is preserved verbatim — it's what the caller must fix.
        assert!(o.text.contains("expected `i32`, found `()`"));
        assert!(o.text.contains("= note: expected type `i32`"));
        assert!(o.text.contains("0 warnings, 2 errors"));
    }

    #[test]
    fn clippy_keeps_warning_bodies() {
        let stderr = "\
warning: this `if` has identical blocks
 --> src/lib.rs:5:5
  |
5 |     if x { 1 } else { 1 }
  |     ^^^^^^^^^^^^^^^^^^^^^^
  |
  = help: for further information visit https://rust-lang.github.io/rust-clippy
";
        let o = clippy(&ctx("", stderr, 0));
        // Clippy bodies (the snippet + help link) are retained in full.
        assert!(o.text.contains("if x { 1 } else { 1 }"));
        assert!(o.text.contains("= help: for further information"));
        assert_eq!(o.filter_id, "cargo_clippy");
    }
}
