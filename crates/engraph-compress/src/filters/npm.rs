use super::{FilterCtx, FilterOutput};

/// `npm install` — keep the summary (added N, removed M), warnings, and vulnerability
/// section. Drop the per-package "added xxx" spam.
pub fn install(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut combined = String::with_capacity(ctx.stdout.len() + ctx.stderr.len());
    combined.push_str(ctx.stdout);
    combined.push_str(ctx.stderr);
    let mut out = String::with_capacity(combined.len() / 5);
    for line in combined.lines() {
        let t = line.trim_start();
        // Drop "added X packages, removed Y..." per-line spam; keep the final summary
        if t.starts_with("added ")
            && t.contains("packages")
            && !t.contains("found")
            && !t.contains("audited")
        {
            // The final summary line typically reads "added N packages in Xs"
            if t.contains(" in ") {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if t.is_empty() {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("[engraph: exit {}]\n", ctx.exit_code));
    FilterOutput {
        text: out,
        filter_id: "npm_install",
    }
}

/// `npm test` — let the underlying test runner handle compression where we can,
/// fall back to keeping summary lines and dropping pass spam.
pub fn test(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut combined = String::with_capacity(ctx.stdout.len() + ctx.stderr.len());
    combined.push_str(ctx.stdout);
    combined.push_str(ctx.stderr);
    let mut out = String::with_capacity(combined.len() / 3);
    for line in combined.lines() {
        let t = line.trim_start();
        // Drop jest/mocha-style green check passes — keep failures
        if t.starts_with("\u{2713}") || t.starts_with("✓") || t.starts_with("PASS") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("[engraph: exit {}]\n", ctx.exit_code));
    FilterOutput {
        text: out,
        filter_id: "npm_test",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(stdout: &'a str, stderr: &'a str) -> FilterCtx<'a> {
        FilterCtx {
            cmd: "npm",
            args: &[],
            stdout,
            stderr,
            exit_code: 0,
        }
    }

    #[test]
    fn install_keeps_summary_drops_progress() {
        let stdout = "\
added 142 packages in 8s

12 packages are looking for funding

found 3 high severity vulnerabilities
";
        let o = install(&ctx(stdout, ""));
        assert!(o.text.contains("added 142 packages in 8s"));
        assert!(o.text.contains("vulnerabilities"));
    }
}
