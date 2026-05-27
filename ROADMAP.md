# Engraph Roadmap

What's not yet shipped, ordered roughly by token-savings impact. F2 (the structure-aware code graph) is the largest remaining lever and is documented in depth; everything else is sized for context. This file is forward-looking — anything described as "current" or "shipped" refers to the state of the codebase at commit `a4e765b` (v2: auto-rewrite + 24 new filters).

---

## F2 — Structure-aware code retrieval (codegraph)

**The single biggest remaining token lever for code-heavy sessions.** Today, when Claude needs to understand "what calls `processOrder`" or "what does the `User` struct touch," it reads files and greps. A typical exploration loop burns 5,000–30,000 tokens on file Reads that a 30-line markdown subgraph could answer. F2 builds a deterministic graph of symbols, calls, and dependencies once, then serves 2-hop neighborhoods on demand.

### Why F2 belongs in engraph

Engraph already has the right scaffolding:
- SQLite WAL store, schema migrations, telemetry plumbing.
- Provenance/confidence pattern (`relations.provenance IN ('extracted','inferred','ambiguous','generated')` in schema v2).
- Knowledge graph tables (`entities`, `relations` with validity windows).
- The retrieval surface (`engraph recall`) and the SessionStart auto-inject hook are already wired to consume KG entities.

F2 is "fill in the entity/relation tables from real code, then add a subgraph exporter on top of `engraph recall`."

### Goal

Given a symbol name or file path, return a markdown 2-hop neighborhood that fits in ~8KB of tokens:

```
## Symbol `processOrder` (defined in src/orders/process.rs:42)
**Calls**: `validate()` (src/orders/validate.rs:10), `charge()` (src/payments.rs:88)
**Called by**: `handleCheckout()` (src/api/checkout.rs:55), `retry_order()` (src/orders/retry.rs:20)
**Sibling symbols** (same file): `cancelOrder()`, `refundOrder()`
**Imports**: `crate::payments::Stripe`, `crate::orders::OrderStatus`
**Cross-repo refs**: 2 callers in `payment-service`, 1 in `admin-dashboard`
```

That's typically 100× compression vs a Read+grep loop, applied to the highest-frequency operation in code work.

### Architecture

```
                    ┌─────────────────────────────────┐
                    │  per-repo indexer drivers       │
                    │  (Rust, parallel, per-language) │
                    └────────────────┬────────────────┘
                                     │ SCIP protobuf indexes
                                     ▼
                    ┌─────────────────────────────────┐
                    │  intermediate Parquet layer     │
                    │  (lets us swap stores later)    │
                    └────────────────┬────────────────┘
                                     │
                                     ▼
                    ┌─────────────────────────────────┐
                    │  engraph.db (SQLite)            │
                    │  symbols, edges, files, repos   │
                    └────────────────┬────────────────┘
                                     │
                          ┌──────────┴──────────┐
                          ▼                     ▼
                  ┌──────────────┐      ┌──────────────────┐
                  │ engraph recall│      │ engraph subgraph │
                  │   --kind sym  │      │   <symbol|file>  │
                  └──────────────┘      └──────────────────┘
```

**Storage decision (from prior architecture review):** start SQLite-only. The 2-hop neighborhood workload is a relational join, not a deep graph traversal. With `(src_entity, dst_entity)` and `(dst_entity, src_entity)` indexes on `relations`, the join finishes in milliseconds for graphs up to ~1M edges. The earlier plan also considered Kuzu — defer until telemetry shows ≥5-hop pathfinding is a real workload, which it almost certainly isn't.

**Parquet intermediate layer is the hedge:** SCIP indexers write protobuf; we transform to Parquet and load into SQLite. If we ever need to swap stores (DuckDB+PGQ, Kuzu, an in-memory engine), we re-ingest from Parquet without re-running the indexers.

### Implementation phasing

Three independently shippable phases. Phase 2.1 alone delivers single-repo navigation and is worth shipping by itself.

#### Phase 2.1 — Single-repo indexer + symbol/call graph

1. **Driver crate `engraph-codegraph`** with one driver per (build system, language). Initial set:
   - `cargo` projects → `rust-analyzer --scip`
   - Generic `pyproject.toml` → `scip-python`
   - `go.mod` → `scip-go`
   - `package.json` + `tsconfig.json` → `scip-typescript`
   - `pom.xml` / `build.gradle` → `scip-java`
2. **SCIP loader** (`scip` crate from Sourcegraph) reads protobuf, normalizes monikers, writes to the existing `entities`/`relations` tables. Each symbol gets `kind = 'symbol'`, each reference becomes a `REFERENCES` or `CALLS` relation tagged `provenance = 'extracted'`.
3. **Schema additions** (v5 migration):
   - `entities` already has `kind`, `name`, `project` — add `file_path`, `line_range`, `signature` columns.
   - `relations` already has provenance/confidence — add `relation_kind` constraint covering `DEFINES`, `REFERENCES`, `CALLS`, `IMPLEMENTS`, `EXTENDS`, `IMPORTS`.
4. **`engraph index <repo>` CLI subcommand**: discovers build manifests, runs the right driver, loads SCIP into the DB.
5. **`engraph subgraph <symbol>` CLI subcommand**: SQL query for the symbol's 2-hop neighborhood, formats as markdown per the example above, caps at ~30 nodes / ~8KB.

**Single-repo verification:** index a known repo (e.g. engraph itself), run `engraph subgraph engraph::compress`, eyeball the output. Re-index — symbol IDs (the SCIP monikers) must be stable across runs.

#### Phase 2.2 — Cross-repo stitching

SCIP monikers (`scheme manager package descriptor`) are stable cross-repo identifiers when normalized. The work is:

1. **Repo manifest** (`engraph_manifest.toml` per repo, checked in): declares language(s), build system, package roots, and any indexer-specific moniker rewrite rules. Auto-generated on first index; manually editable.
2. **Moniker normalization rules** in the loader. Pre-0.7 `scip-go` embeds absolute paths; some indexers leak build flags into descriptors. Each language driver strips known noise so monikers compare equal across machines.
3. **Symbol-stability test suite** (`tests/symbol_stability/`): pick 50 known symbols across a polyrepo, snapshot their normalized monikers, re-run on every loader change. Catches indexer-version drift — the #1 silent failure mode for cross-repo graphs.
4. **`engraph index --workspace <dir>`**: discovers all repos under a workspace root, indexes them, stitches edges via monikers.

**Cross-repo verification:** two repos sharing a dependency (e.g. one consumes another's public API). After `engraph index --workspace`, `engraph subgraph <consumer symbol>` should include callees defined in the other repo with `repo:` annotations.

#### Phase 2.3 — Bazel polyglot monorepo

`scip-bazel` exists but is thin and lags on Kotlin/Swift. The production approach is dual-source:

1. **`bazel query 'kind("(java|kt_jvm|go|ts)_library", //...)'`** enumerates targets per language. **(shipped — commit `5ee0546`)**
2. **Per-language `scip-*` driven by Bazel-resolved classpaths/srcs.** For Java: scip-java's bundled aspect (no aspect reimplementation needed). For Go: `scip-go` against the workspace's `go.mod`. For TypeScript: `scip-typescript` at the workspace root. Opt-in via `engraph index --bazel-symbols`. **(shipped — Phase 2.3 #2)**
3. **`BAZEL_DEPENDS_ON` edges** extracted directly from `bazel query 'deps(...)'`. Package-level edges, deterministic, independent of SCIP. Covers architecture-viz use case even where language indexers miss things. **(shipped — commit `5ee0546`)**

Symbol-level (from SCIP) + target-level (from `bazel query`) is the combination that works in production for polyglot Bazel monorepos.

Known follow-ups for Phase 2.3 #2 (documented at ship; deferred until a real workload trips them):
- **scip-go multi-`go.mod` monorepos**: today the symbol-level path requires a single `go.mod` at the workspace root. Gazelle-managed Bazel-go repos sometimes carry one per package; would need enumeration + merging.
- **scip-typescript + rules_ts node_modules**: cold runs may fail until a prior `bazel build //...` populates `bazel-bin/<pkg>/node_modules` symlinks.
- **scip-java on large monorepos**: 1000+ Java targets can OOM or exceed 30 min. Future `--targets <expr>` pass-through (reserved env var `ENGRAPH_BAZEL_SCIP_JAVA_TARGETS`).
- **Bazel server isolation (Java path)**: the target-level pass pins Bazel's `--output_base` into engraph's cache; scip-java invokes Bazel internally with no startup-option pass-through we can plumb. With `--bazel-symbols`, scip-java's Bazel build lands in the user's default `~/.cache/bazel`. Follow-up: verify scip-java exposes a `--bazel-startup-options` flag (or similar) and thread the engraph output_base through it.

### Known constraints (carry over from the original plan)

- **Generated code can 10× node count.** Protobuf/gRPC generated Java/Go especially. Tag with `provenance = 'generated'` and exclude from default queries.
- **Kotlin coverage will be partial.** `scip-kotlin` lags `scip-java`. For JVM-only Kotlin lean on `scip-java`'s Kotlin support. For Compose-heavy / multiplatform, accept partial coverage and mark symbols `provenance = 'ambiguous'` rather than failing the run.
- **Swift is best-effort.** SourceKit-LSP dumps work; flag in `engraph_manifest.toml`. Don't gate releases on Swift indexing.
- **SCIP indexers do whole-package re-indexing.** 30s–5min per run on large Gradle modules. Concurrent edits during indexing will corrupt queries if you write live — use a staging-tables-then-atomic-swap pattern. Mnemosyne's `ingestion_log` pattern (already in engraph schema) is the same shape.

### How F2 surfaces to Claude

Three integration points; all reuse existing engraph hooks:

1. **`engraph subgraph <sym>` invoked as a Bash command.** Same wrapper pattern as `engraph run`. Markdown subgraph goes straight into the transcript.
2. **SessionStart inject** (`engraph hook session-start`) adds a top-symbols line per project: "files touched most often in this project's history" + their direct call neighborhoods. Today the hook only emits decisions/bugs/budget; F2 lets it also emit code-structure context.
3. **MCP tool surface (optional, if/when MCP server is built):** `find_symbol`, `who_calls`, `subgraph_around` — structured args, deferred load via ToolSearch.

### Effort estimate

- Phase 2.1 (single-repo): 1–2 weeks. Most of the work is the indexer drivers and the markdown formatter.
- Phase 2.2 (cross-repo): 1 week on top of 2.1. Moniker normalization is the hardest part.
- Phase 2.3 (Bazel): 2–3 weeks. Largely diagnostic work — running Bazel queries, parsing aspect output, debugging language-specific quirks.

Phase 2.1 is the high-ROI cut. If 2.2 turns out painful, 2.1 still delivers the in-repo subgraph win.

---

## F5 — Pre-read file-anatomy injection

**What:** before Claude reads a file, the PreToolUse(Read) hook injects a one-paragraph summary if the file is already known — top symbols, last-known signature shape, recent bugs touching it, last-edit timestamp. Claude may skip the re-read entirely.

**Why deferred:** PreToolUse Read fires on *every* file read, including small ones. Injecting 500 tokens before each Read can dwarf the savings on small files. Worth building only when F7 telemetry shows file re-reads are a top-3 token sink, AND when F2 is wired (the graph supplies anatomy cheaply via the `entities` table).

**Dependencies:** F2 Phase 2.1 (so the anatomy data is already in the DB).

**Effort:** 1–2 days once F2 ships.

---

## F8 — `/resume` and `/save` skills

**What:** user-facing slash commands in `~/.claude/skills/` for explicit session bookmarking. `/save <summary>` writes a `context_items` row with `kind = 'decision'` scoped to the current project. `/resume` queries the latest decisions and pretty-prints them.

**Why deferred:** F4 (SessionStart auto-inject) covers ~80% of this. The manual variants matter when auto-inject is wrong or when the user wants to bookmark mid-session. Build after measuring how often F4's auto-brief misses.

**Dependencies:** None — the schema (`context_items.kind = 'decision'`) already exists.

**Effort:** 1 day (mostly skill markdown + a thin `engraph save` / `engraph resume` CLI).

---

## F9 — LLM reranking layer

**What:** top-K from RRF hybrid retrieval (F3 advanced) → small model rerank for final ordering. Defaults to off because it costs tokens.

**Why deferred:** until hybrid retrieval (already shipped) shows a measurable ceiling on a real query benchmark, this is theoretical. Need F7 telemetry showing p@5 plateau before justifying the per-query model spend.

**Dependencies:** a real query benchmark dataset (currently nothing close to that exists).

**Effort:** 2–3 days, mostly prompt design + token-cost measurement.

---

## F10 — Cross-project wikilink / Obsidian surface

**What:** export `context_items` / `bugs` / `decisions` to an Obsidian-compatible vault structure with wikilinks. Useful for human browsability across projects; orthogonal to token savings.

**Why deferred:** not a token-savings feature. Only build if the user wants the vault for personal note-taking.

**Effort:** 2–3 days. Conceptually simple, friction is in matching Obsidian's conventions.

---

## MCP server

**What:** expose `engraph recall`, `engraph subgraph`, and related queries as MCP tools. Lets Claude orchestrate retrieval explicitly via tool calls rather than reading CLI output.

**Why deferred:** with ToolSearch enabled (the user's setup), MCP unused-cost is ~50 tk/tool name, low but non-zero. The Bash-CLI path (`engraph recall <q>` → output goes into transcript) already works for the use cases we have. MCP wins when there are many tools that benefit from structured args + result types — that becomes true once F2 ships (each graph operation is its own tool).

**Build trigger:** after F2 Phase 2.1, when there are 5+ tools to expose.

**Effort:** 3–4 days. The MCP SDK + a stdio server crate are the bulk of it.

---

## Outstanding review findings (bugs and improvements)

Items flagged by code review during v1 development. Closed items have been
folded into "Shipped in the v2.1 polish pass" below.

### Shipped in the v2.1 polish pass

Each of these landed with a positive and (where applicable) negative test.
See README "Verification" section for the test inventory.

- **Phase 3 M2 — partial trailing line on mid-write read.** `read_line` returning a partial line at EOF would commit the offset past unparsed content. Now the loop breaks without advancing on any line that doesn't end with `\n`. Regression test: `ingest_holds_offset_when_trailing_line_is_partial` in `crates/engraph-ingest/src/lib.rs`.
- **Phase 3 M3 — cargo-nextest format.** `cargo::test` now matches both libtest (`test foo ... ok` / `---- foo stdout ----`) and cargo-nextest (`PASS [   X.Xs] pkg test` / `FAIL [   X.Xs] pkg test`). Tests: `nextest_failures_counted_and_pass_lines_dropped`, `nextest_and_libtest_dont_double_count`, plus the byte-exact `cargo_test_nextest` golden fixture.
- **Phase 4 SHOULD-FIX — `recent_decisions` SQL is dead.** Deleted both the query and its call site in `crates/engraph-cli/src/main.rs`. F8 `/save`/`/resume` remains deferred (see below).
- **N+1 implicit transactions per ingest message.** `engraph ingest` now wraps an entire JSONL file's writes in a single transaction guarded by a `TxGuard` RAII helper that rolls back on error so a pooled connection never returns with an open txn.
- **`compress_existing` UPDATE re-fires the FTS trigger.** Resolved by v5 migration dropping `messages_au` / `context_items_au`. Pinned by `compress_existing_keeps_fts_pointed_at_original` (asserts FTS recall against the original distinctive phrase survives a compression sweep).
- **Sidechain JSONL entries pollute regular ingest.** `RawEvent::is_sidechain` (`isSidechain`) is filtered at parse time. Test: `ingest_skips_sidechain_events`.
- **`limit` bound as TEXT not INTEGER** in retrieve. Both `search_messages` and `search_context_items` now build `Vec<rusqlite::types::Value>` with `Value::Integer(limit as i64)`.
- **`cargo check` filter_id mislabel.** Added `cargo::check` wrapper that stamps `cargo_check` so picker id and `FilterOutput.filter_id` agree. Regression test: `picker_and_filter_output_agree_on_filter_id` covers every cargo + git arm.
- **`run_pre_bash_hook` "two `.pointer()` calls"** — verified stale: only one pointer call exists today (`main.rs:724`), preceded by a single `serde_json::from_str`. No change needed.
- **Golden snapshot fixtures.** `crates/engraph-compress/tests/fixtures/` now holds three pairs (`git_log_basic`, `cargo_check_basic`, `cargo_test_nextest`) wired up by `tests/golden_fixtures.rs` with a byte-exact assertion. Negative test: requesting a missing fixture panics rather than silently passing.
- **`Cmd::Run` does not call `budget::add_used`.** Fixed: when `CLAUDE_SESSION_ID` is set, the post-filter output token count is charged to `session_budget`. Positive + negative integration tests in `crates/engraph-cli/tests/run_budget.rs`.
- **Phase 6 RRF recency weight `0.0` placeholder.** Now wired as a third RRF source with `W_RECENCY = 0.5`, ranking candidates by `ts` descending (RFC3339 lexicographic). Candidates without a `ts` are absent from the recency list. Test: `hybrid_recency_tiebreaks_toward_newer`. README "Future signals" section rewritten to reflect this.
- **`tokio::process` migration.** `Cmd::Run` now spawns the wrapped command through `tokio::process::Command` on a single-threaded `tokio` runtime in `run_wrapped_command`. Stdin is inherited (interactive wrappers work), `wait_with_output` drains stdout and stderr concurrently (no pipe-buffer deadlock), and `tokio::signal::unix` handlers swallow SIGINT/SIGTERM in the parent so the child can handle terminal signals and exit cleanly without the parent dying first. Tests: `wrapped_run_inherits_stdin` (positive), `wrapped_run_drains_large_concurrent_output_without_deadlock` (negative — ~200KB on each stream exceeds the typical 64KB pipe buffer).

### Still outstanding

_(none under this heading after the v2.1 + tokio::process pass)_

---

## Operational items (not engineering)

- **Wire hooks into `~/.claude/settings.json`.** The README documents the snippet; nothing is installed yet on this machine.
- **Mnemosyne → engraph data migration script.** Both DBs are SQLite with similar schemas. A one-shot SQL script could copy sessions, messages, do-not-repeat, and bugs over. Effort: a day with the SQL + a sanity-check pass.
- **2-week real-world measurement.** The original success criteria called for ≥ 20% aggregate token savings across two weeks of actual Claude Code use, measured via `engraph gain`. Hasn't been run yet because hooks aren't installed.
- **CI.** No GitHub Actions / equivalent yet. Worth setting up: `cargo test`, `cargo clippy --all-targets -- -D warnings`, and the `--features embeddings` variant of both, on every push.
- **Release binaries.** No `cargo dist` / GitHub Releases yet. Once the hooks are wired in production for two weeks and stable, ship a release with prebuilt binaries.

---

## Future research (not on the roadmap, but worth noting)

- **Adaptive compression based on F11 escalation level.** When `engraph budget status` shows level 2 or 3, filters could apply stricter caps (e.g. `tree` depth drops from 3 to 2, `docker logs` tail from 200 to 100). Telemetry-driven escalation is the bigger story behind F11 that hasn't been fleshed out.
- **Mempalace-style eviction policy.** Memory tools that only accrete become liabilities. Need time-decay scoring with a hard floor, demote-after-N-non-retrievals, size budget per project. The schema supports it (`scopes.archived`); the algorithm doesn't exist yet.
- **Cloud sync.** Local-first is the v1 promise. If you ever want to share memory across machines, the trait boundary in place today (`EmbeddingProvider`) + UUIDv7 IDs + the `ingestion_log` append-only pattern are the foundation. Need to add `MemoryStore` and `GraphStore` traits, then a Turso/LiteFS implementation. The architecture review explicitly argued against adding those traits before there's a concrete cloud need; revisit when there is.
- **A "what did Claude actually do" weekly digest.** A `engraph weekly` subcommand that reads `events` + `messages` and produces a summary of: top compressed commands, top recalled phrases, longest sessions, biggest savings. Useful for both the user and for tuning filter targets.
