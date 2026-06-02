use super::{FilterCtx, FilterOutput};
use crate::{CompressInput, CompressKind, compress};

pub fn filter(ctx: &FilterCtx<'_>) -> FilterOutput {
    let mut combined = String::with_capacity(ctx.stdout.len() + ctx.stderr.len() + 32);
    if !ctx.stdout.is_empty() {
        combined.push_str(ctx.stdout);
    }
    if !ctx.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str("--- stderr ---\n");
        combined.push_str(ctx.stderr);
    }
    // ToolOutput is exactly the case the brevity flag exists to serve —
    // noisy, non-prose, grammar not preserved by extractive ranking anyway.
    let r = compress(CompressInput {
        text: &combined,
        kind: CompressKind::ToolOutput,
        target_ratio: 0.5,
        brevity: true,
    });
    FilterOutput {
        text: r.text,
        filter_id: "generic",
    }
}
