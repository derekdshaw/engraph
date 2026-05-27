//! JVM build-tool filters: `mvn` and `gradle`. Each drops its tool-specific
//! download/progress chatter and keeps diagnostics + the final build outcome.

use super::util::{combine, drop_matching};
use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

pub fn mvn(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^\[INFO\] (Downloading from |Downloaded from |Progress )").unwrap()
    });
    let (filtered, dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: mvn dropped {dropped} download lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "mvn",
    }
}

pub fn gradle(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Drop "> Task :compileJava" progress lines and "Downloading" chatter.
        Regex::new(r"^> Task :|^Downloading https?://").unwrap()
    });
    let (filtered, dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: gradle dropped {dropped} progress lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "gradle",
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
    fn mvn_drops_downloads() {
        let stdout = "\
[INFO] Downloading from central: https://repo.maven.apache.org/maven2/...
[INFO] Downloaded from central: ...
[INFO] BUILD SUCCESS
";
        let out = mvn(&ctx(stdout, 0));
        assert!(!out.text.contains("Downloading from"));
        assert!(out.text.contains("BUILD SUCCESS"));
    }

    #[test]
    fn gradle_drops_task_progress() {
        let stdout = "\
> Task :compileJava
> Task :test
BUILD SUCCESSFUL in 12s
";
        let out = gradle(&ctx(stdout, 0));
        assert!(!out.text.contains("> Task :"));
        assert!(out.text.contains("BUILD SUCCESSFUL"));
    }
}
