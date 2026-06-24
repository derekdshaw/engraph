use super::util::{combine, strip_ansi, truncate_lines};
use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

const MAX_LINES: usize = 200;

/// Pure progress/analysis chatter bazel sprays onto stderr. Notably keeps
/// `INFO: From …` (it heads a real compiler diagnostic), `INFO: Build
/// completed`, and `INFO: Elapsed time` (the actual result).
fn noise_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(\[[\d,]+ / [\d,]+\]|Loading:|Analyzing:|INFO: Analyzed |INFO: Found |Starting local Bazel server|Computing main repo mapping|INFO: \d+ processes)",
        )
        .unwrap()
    })
}

/// `bazel build` — drop the `[N / M]` action counters, `Loading:`/`Analyzing:`
/// and target-count chatter; keep `ERROR:`/compiler diagnostics, warnings, and
/// the build-completion summary.
pub fn build(ctx: &FilterCtx<'_>) -> FilterOutput {
    let combined = combine(ctx.stdout, ctx.stderr);
    let text = strip_ansi(&combined);
    let noise = noise_re();
    let mut out = String::with_capacity(text.len() / 2);
    let mut errors = 0_u32;
    for line in text.lines() {
        let t = line.trim_start();
        if noise.is_match(t) {
            continue;
        }
        // Count only bazel's own `ERROR:` markers — the underlying compiler
        // `: error:` detail lines are kept but would double-count the failure.
        if t.starts_with("ERROR:") {
            errors += 1;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "[engraph: bazel build {errors} errors, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "bazel_build",
    }
}

/// `bazel test` — drop per-target `PASSED` lines (count them) and build progress;
/// keep `FAILED`/`FLAKY`/`TIMEOUT` targets, their log paths, and the
/// `Executed N out of M tests` summary.
pub fn test(ctx: &FilterCtx<'_>) -> FilterOutput {
    let combined = combine(ctx.stdout, ctx.stderr);
    let text = strip_ansi(&combined);
    let noise = noise_re();
    let mut out = String::with_capacity(text.len() / 2);
    let mut passed = 0_u32;
    let mut failed = 0_u32;
    for line in text.lines() {
        let t = line.trim_start();
        if noise.is_match(t) {
            continue;
        }
        // Per-target pass line: `//pkg:target   PASSED in 0.5s` / `(cached) PASSED`.
        if t.contains("PASSED") && (t.starts_with("//") || t.contains("(cached)")) {
            passed += 1;
            continue;
        }
        if t.starts_with("//")
            && (t.contains("FAILED") || t.contains("FLAKY") || t.contains("TIMEOUT"))
        {
            failed += 1;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "[engraph: bazel test {passed} passed, {failed} failed targets, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "bazel_test",
    }
}

/// `bazel query` / `cquery` / `aquery` — a target list. Drop any interleaved
/// progress noise and cap the line count.
pub fn query(ctx: &FilterCtx<'_>) -> FilterOutput {
    let combined = combine(ctx.stdout, ctx.stderr);
    let text = strip_ansi(&combined);
    let noise = noise_re();
    let mut filtered = String::with_capacity(text.len());
    for line in text.lines() {
        if noise.is_match(line.trim_start()) {
            continue;
        }
        filtered.push_str(line);
        filtered.push('\n');
    }
    FilterOutput {
        text: truncate_lines(&filtered, MAX_LINES, "targets"),
        filter_id: "bazel_query",
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
    fn build_drops_progress_keeps_errors() {
        let stderr = "\
Starting local Bazel server and connecting to it...
INFO: Analyzed 42 targets (118 packages loaded, 1234 targets configured).
INFO: Found 42 targets...
[1,234 / 5,678] Compiling src/foo.cc; 12s linux-sandbox
[2,000 / 5,678] Compiling src/bar.cc; 5s
ERROR: /w/pkg/BUILD.bazel:12:11: Compiling src/foo.cc failed: (Exit 1)
src/foo.cc:5:10: error: 'x' was not declared
ERROR: Build did NOT complete successfully
";
        let o = build(&ctx("", stderr, 1));
        assert!(!o.text.contains("[1,234 / 5,678]"));
        assert!(!o.text.contains("INFO: Analyzed"));
        assert!(!o.text.contains("Starting local Bazel server"));
        assert!(o.text.contains("error: 'x' was not declared"));
        assert!(o.text.contains("Build did NOT complete"));
        assert!(o.text.contains("2 errors"));
    }

    #[test]
    fn test_drops_passed_keeps_failed() {
        let stdout = "\
//pkg:a                          PASSED in 0.5s
//pkg:b                  (cached) PASSED in 0.0s
//pkg:c                          FAILED in 1.2s
  /home/u/.cache/bazel/.../test.log
Executed 2 out of 3 tests: 2 tests pass and 1 fails locally.
";
        let o = test(&ctx(stdout, "", 3));
        assert!(!o.text.contains("//pkg:a"));
        assert!(!o.text.contains("//pkg:b"));
        assert!(o.text.contains("//pkg:c"));
        assert!(o.text.contains("test.log"));
        assert!(o.text.contains("Executed 2 out of 3"));
        assert!(o.text.contains("2 passed, 1 failed"));
    }

    #[test]
    fn query_caps_targets() {
        let stdout: String = (0..300).map(|i| format!("//pkg:target_{i}\n")).collect();
        let o = query(&ctx(&stdout, "", 0));
        assert!(o.text.contains("//pkg:target_0"));
        assert!(o.text.contains("truncated 100 more targets"));
    }
}
