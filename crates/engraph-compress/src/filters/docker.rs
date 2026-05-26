use super::util::{combine, tail_lines, truncate_lines};
use super::{FilterCtx, FilterOutput};

pub fn ps(ctx: &FilterCtx<'_>) -> FilterOutput {
    FilterOutput {
        text: truncate_lines(ctx.stdout, 100, "rows"),
        filter_id: "docker_ps",
    }
}

pub fn logs(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    FilterOutput {
        text: tail_lines(&text, 200),
        filter_id: "docker_logs",
    }
}

pub fn compose(ctx: &FilterCtx<'_>) -> FilterOutput {
    // docker compose ps → table truncate; docker compose logs → tail.
    let is_logs = ctx.args.get(1).map(|a| a == "logs").unwrap_or(false);
    let text = combine(ctx.stdout, ctx.stderr);
    let body = if is_logs {
        tail_lines(&text, 200)
    } else {
        truncate_lines(&text, 100, "rows")
    };
    FilterOutput {
        text: body,
        filter_id: "docker_compose",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_truncates_at_cap() {
        let stdout: String = (0..150).map(|i| format!("container{i}\n")).collect();
        let out = ps(&FilterCtx {
            cmd: "docker",
            args: &["ps".to_string()],
            stdout: &stdout,
            stderr: "",
            exit_code: 0,
        });
        assert!(out.text.contains("truncated 50 more rows"));
    }
}
