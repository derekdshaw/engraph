# engraph

A Rust CLI that cuts Claude Code token usage via persistent session memory,
scoped retrieval, deterministic compression of command output, and telemetry
measuring the savings. Local-first: one SQLite file at
`~/.local/share/engraph/engraph.db` (override `ENGRAPH_DB_PATH`). No daemon.

This repo **dogfoods itself** — the `engraph` binary is wired into Claude Code
hooks (`~/.claude/settings.json`: pre-bash, pre-grep, post-read, SessionStart,
SessionEnd). Changes here affect the live session you're in.

## Use engraph for search before broad grep/read

When you need context on a code symbol — what it is, what calls it, what it
calls, what lives beside it — **`engraph subgraph <symbol>` is the default, not
grep/read.** It returns a 2-hop neighborhood in one call; grepping a symbol then
reading each hit is the slow path. Reach for it whenever you catch yourself about
to grep for a function/type/method name or open a file just to see what's around
a definition.

- **Code symbol + its callers/callees/siblings** → `engraph subgraph <symbol>` (the first move for code context)
- **Prior context / decisions / conversation history** → `engraph recall "<topic>" --project "$(pwd)"`
- Fall back to grep/read only when subgraph comes up empty, the target isn't a
  symbol, or you need exact text/line content.

The pre-grep / pre-bash hooks enforce this: a `grep`/`rg` for an indexed symbol
(including `fn x`, `x(`, `Foo::bar` shapes) is denied and redirected to
`engraph subgraph`. Add a regex metachar (`x\b`) to bypass when you truly need
raw text. Keep the graph fresh with `engraph index .`.

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
