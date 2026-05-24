use super::{FilterCtx, FilterOutput};
use crate::{compress, CompressInput, CompressKind};

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
    let r = compress(CompressInput::new(&combined, CompressKind::ToolOutput));
    FilterOutput {
        text: r.text,
        filter_id: "generic",
    }
}
