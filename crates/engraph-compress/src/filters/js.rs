use super::util::{combine, drop_matching};
use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

pub fn yarn_install(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| Regex::new(r"^\[\d+/\d+\] (Resolving|Fetching|Linking|Building)").unwrap());
    let (filtered, dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: yarn dropped {dropped} progress lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "yarn_install",
    }
}

pub fn pnpm_install(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    // Drop progress dots and per-package resolve/fetch lines.
    let re = RE.get_or_init(|| {
        Regex::new(r"^(Progress: |Packages: |\.+\s*$|Resolving: |Fetching: )").unwrap()
    });
    let (filtered, dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: pnpm dropped {dropped} progress lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "pnpm_install",
    }
}

/// jest / vitest / mocha — drop pass-lines (✓, ok), keep failures + summary.
pub fn js_test(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    let mut out = String::with_capacity(text.len() / 2);
    let mut passed = 0_u32;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('\u{2713}')
            || t.starts_with('✓')
            || t.starts_with("PASS ")
            || t.starts_with("ok ")
        {
            passed += 1;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "[engraph: js_test dropped {passed} pass lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "js_test",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ctx<'a>(stdout: &'a str, exit: i32) -> FilterCtx<'a> {
        FilterCtx {
            args: &[],
            stdout,
            stderr: "",
            exit_code: exit,
        }
    }

    #[test]
    fn yarn_drops_progress() {
        let stdout = "\
[1/4] Resolving packages...
[2/4] Fetching packages...
[3/4] Linking dependencies...
[4/4] Building fresh packages...
warning Workspaces can only be enabled in private projects.
Done in 3.42s.
";
        let out = yarn_install(&ctx(stdout, 0));
        assert!(!out.text.contains("Resolving packages"));
        assert!(out.text.contains("Done in 3.42s"));
    }

    #[test]
    fn js_test_drops_pass_lines() {
        let stdout = "\
PASS src/a.test.ts
  ✓ adds (5 ms)
  ✓ subtracts (1 ms)
  ✗ fails (3 ms)
Tests: 1 failed, 2 passed
";
        let out = js_test(&ctx(stdout, 1));
        assert!(!out.text.contains("✓ adds"));
        assert!(out.text.contains("✗ fails"));
        assert!(out.text.contains("Tests: 1 failed"));
    }
}
