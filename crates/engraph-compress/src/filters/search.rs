use super::util::truncate_lines;
use super::{FilterCtx, FilterOutput};

pub fn rg(ctx: &FilterCtx<'_>) -> FilterOutput {
    FilterOutput {
        text: truncate_lines(ctx.stdout, 200, "matches"),
        filter_id: "rg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rg_caps_long_results() {
        let stdout: String = (0..300)
            .map(|i| format!("src/file{i}.rs:10:match\n"))
            .collect();
        let out = rg(&FilterCtx {
            cmd: "rg",
            args: &["pattern".to_string()],
            stdout: &stdout,
            stderr: "",
            exit_code: 0,
        });
        assert!(out.text.contains("truncated 100 more matches"));
    }
}
