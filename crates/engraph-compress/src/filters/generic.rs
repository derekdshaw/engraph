use super::{util, FilterCtx, FilterOutput};
use crate::{compress, CompressInput, CompressKind};

pub fn filter(ctx: &FilterCtx<'_>) -> FilterOutput {
    // Cheap, deterministic passes first: strip ANSI escapes and collapse
    // runs of identical lines (typical sources: progress bars, repeated
    // stack frames, retried-request logs). Run before the extractive
    // compress step so the ranker doesn't waste budget on noise.
    let combined = util::combine(ctx.stdout, ctx.stderr);
    let combined = util::strip_ansi(&combined);
    let combined = util::dedup_consecutive(&combined);
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
