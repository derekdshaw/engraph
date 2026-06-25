---
name: engraph-refresh
description: Refresh engraph's local indexes after a work session — re-embed conversation messages (engraph reindex-embeddings) and, only when asked, rebuild the code graph (engraph index .). Use when the user wants to update or refresh engraph embeddings, semantic recall, or the SCIP/codegraph index.
---

# engraph-refresh

Bring engraph's local indexes up to date so semantic recall and the code graph
reflect recent work. Run the steps from the **repo root**.

## Step 1 — Re-embed conversation messages (always)

```
engraph reindex-embeddings
```

Incremental and idempotent: it only embeds messages added since the last run, so
it's cheap when nothing is new. Report the count it prints (`embedded N
messages`).

If it fails with `embeddings feature not enabled`, the `engraph` on PATH is a
lean build — not a skill failure. Tell the user to install an embeddings build
and stop:

```
cargo build --release -p engraph-cli --features embeddings && \
  install -m 755 target/release/engraph ~/.local/bin/engraph
```

## Step 2 — Rebuild the code graph (opt-in)

Only run this when the skill was invoked with an argument matching `index`,
`code`, `scip`, or `all` (case-insensitive). Otherwise skip it entirely.

```
engraph index .
```

Report the per-language SCIP summary it prints (symbols / SCIP bytes, or any
`MISSING from PATH` indexer warnings).

## Report

Summarize in a line or two: messages embedded, and — if step 2 ran — the index
outcome. Don't dump full command output.
