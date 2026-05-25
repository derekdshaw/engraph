use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

/// `cargo build` / `cargo check` — drop "Compiling X", "Checking X", "Downloading X",
/// "Finished" progress; keep warnings, errors, notes.
pub fn build(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut combined = String::with_capacity(ctx.stdout.len() + ctx.stderr.len());
    combined.push_str(ctx.stdout);
    combined.push_str(ctx.stderr);
    let progress = progress_re();
    let mut out = String::with_capacity(combined.len() / 4);
    let mut warnings = 0_u32;
    let mut errors = 0_u32;
    for line in combined.lines() {
        let trimmed = line.trim_start();
        if progress.is_match(trimmed) {
            continue;
        }
        if trimmed.starts_with("warning:") {
            warnings += 1;
        }
        if trimmed.starts_with("error") {
            errors += 1;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "[engraph: {warnings} warnings, {errors} errors, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "cargo_build",
    }
}

/// `cargo clippy` — same shape as build but tagged differently.
pub fn clippy(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut o = build(ctx);
    o.filter_id = "cargo_clippy";
    o
}

/// `cargo test` — keep test summary lines (running N tests, test results, FAILED panics),
/// drop passing test name lines ("test foo ... ok"), keep failures and their context.
pub fn test(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut combined = String::with_capacity(ctx.stdout.len() + ctx.stderr.len());
    combined.push_str(ctx.stdout);
    combined.push_str(ctx.stderr);
    let progress = progress_re();
    let pass_re = test_pass_re();
    let mut out = String::with_capacity(combined.len() / 3);
    let mut total = 0_u32;
    let mut passed = 0_u32;
    let mut failed = 0_u32;
    let mut ignored = 0_u32;
    for line in combined.lines() {
        let trimmed = line.trim_start();
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
        // Count failures only on the per-failure block header to avoid double-
        // counting against `... FAILED` lines and the summary `test result: FAILED`.
        if trimmed.starts_with("---- ") && trimmed.ends_with("stdout ----") {
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

/// `cargo doc` — same compile-noise pattern as `cargo build`.
pub fn doc(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut o = build(ctx);
    o.filter_id = "cargo_doc";
    o
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(stdout: &'a str, stderr: &'a str, exit: i32) -> FilterCtx<'a> {
        FilterCtx {
            cmd: "cargo",
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
}
