//! `make` output filter. Drops directory-tracking lines and compiler-command
//! echoes; keeps diagnostics.

use super::util::{combine, drop_matching};
use super::{FilterCtx, FilterOutput};
use regex::Regex;
use std::sync::OnceLock;

pub fn make(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Drop "Entering/Leaving directory" + recipe-line echoes (lines starting
        // with a compiler invocation we recognize).
        Regex::new(
            r"^(make\[\d+\]: (Entering|Leaving) directory|cc -c |g\+\+ -c |gcc -c |clang -c )",
        )
        .unwrap()
    });
    let (filtered, dropped) = drop_matching(&text, re);
    let mut out = filtered;
    out.push_str(&format!(
        "[engraph: make dropped {dropped} echo/dir lines, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "make",
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
    fn make_drops_dir_changes() {
        let stdout = "\
make[1]: Entering directory '/build/sub'
cc -c -o foo.o foo.c
make[1]: Leaving directory '/build/sub'
foo.c:10:5: error: 'x' undeclared
";
        let out = make(&ctx(stdout, 1));
        assert!(!out.text.contains("Entering directory"));
        assert!(!out.text.contains("cc -c"));
        assert!(out.text.contains("'x' undeclared"));
    }
}
