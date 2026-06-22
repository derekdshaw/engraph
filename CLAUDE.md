# engraph

A Rust CLI that cuts Claude Code token usage via persistent session memory,
scoped retrieval, deterministic compression of command output, and telemetry
measuring the savings. Local-first: one SQLite file at
`~/.local/share/engraph/engraph.db` (override `ENGRAPH_DB_PATH`). No daemon.

This repo **dogfoods itself** — the `engraph` binary is wired into Claude Code
hooks (`~/.claude/settings.json`: pre-bash, pre-grep, post-read, SessionStart,
SessionEnd). Changes here affect the live session you're in.

## Use engraph for search before broad grep/read

When orienting in this codebase, prefer engraph's own retrieval over a cold
grep/read sweep:

- **Prior context / decisions / conversation history** → `engraph recall "<topic>" --project "$(pwd)"`
- **Code symbol + its callers/callees/siblings** → `engraph subgraph <symbol>`
- Fall back to grep/read when those come up empty or you need exact text.

(The pre-grep / post-read hooks already nudge this when the codegraph is
populated — keep it built with `engraph index .`.)

## Crate map

| Crate | Responsibility |
|---|---|
| `engraph-core` | SQLite pool + schema/migrations (`db`), `budget`, `telemetry`/events, `memory` (do_not_repeat/bugs/context_items), `tokens` (tiktoken), feature-gated `embedding` |
| `engraph-compress` | Deterministic output compression; per-command filters in `filters/` (one fn + a `filters::pick` arm each); `preprocess`/`rank`/`sentinel`/`brevity` |
| `engraph-retrieve` | Recall: FTS5 (`lib`) + `hybrid` embeddings; project `scope`s |
| `engraph-ingest` | JSONL transcript → SQLite. `ingest` (parse + incremental offset/rotation handling), `sweep` (compress existing rows), `common` (sha256 + threshold) |
| `engraph-codegraph` | SCIP indexing (`index`, `scip_loader`, `driver`, `bazel`, `bazel_symbols`), `subgraph` neighborhoods, `relation_kind` |
| `engraph-cli` | The `engraph` binary. `main` (dispatch), `cli` (clap defs), `rewrite` (bash-rewrite parser), `redirect` (grep→subgraph), `hooks` (5 lifecycle hooks + brief + catch-up ingest), `output` (tables/plans) |

## Conventions

- `engraph-cli` and `engraph-ingest` are **multi-module** crates — add new code
  to the right module; do not re-monolith `main.rs` / `lib.rs`.
- `anyhow` in the binary, `thiserror` in libraries. Borrow over `clone`.
- Match existing module style: flat `.rs` files, subdirectory only for a real
  family (e.g. `filters/`).

## Build / test / install

```
cargo build --workspace
cargo test --workspace
cargo fmt --all
# ship the binary the hooks invoke:
cargo build --release -p engraph-cli && install -m 755 target/release/engraph ~/.local/bin/engraph
```

`--features embeddings` gates the hybrid-recall / embedding code paths; build
with it when touching `engraph-core/embedding` or `engraph-retrieve/hybrid`.

`--features otel` compiles the OpenTelemetry metrics exporter. At runtime it's
off unless `ENGRAPH_OTEL=1`; point it at a collector with `ENGRAPH_OTEL_ENDPOINT`
(OTLP/gRPC, default `http://localhost:4317`). `ENGRAPH_OTEL_SESSION=1` additionally
tags metrics with the `session.id` resource attribute (from `CLAUDE_SESSION_ID`)
for per-session correlation — opt-in, since session ids are high-cardinality.
Because the hooks invoke the installed release binary, `ENGRAPH_OTEL=1` is a
**silent no-op** unless that binary was built with the feature — to get metrics
from the live session, ship:

```
cargo build --release -p engraph-cli --features otel && install -m 755 target/release/engraph ~/.local/bin/engraph
```
