# Engraph

A Rust toolchain that cuts Claude Code token usage by combining persistent session memory, scoped retrieval, deterministic compression of stored content and per-command output, and telemetry that proves the savings.

Engraph is local-first. Storage is one SQLite file under `~/.local/share/engraph/engraph.db` (override with `ENGRAPH_DB_PATH`). The cloud path is reserved through a thin `EmbeddingProvider` trait and append-only ingest log; no service is required.

For a deeper walkthrough of the architecture and the algorithm behind each feature, see [`DETAILS.md`](DETAILS.md).

## Status

Six phases of the implementation plan are shipped:

| Feature | Crate / module | Subcommand |
|---|---|---|
| Telemetry + savings dashboard | `engraph-core::telemetry` | `engraph gain` |
| Session token budget with escalation levels | `engraph-core::budget` | `engraph budget {status, set}` |
| Deterministic compressor (F6) | `engraph-compress` | `engraph compress` |
| Per-command Bash wrappers (F1) | `engraph-compress::filters` | `engraph run` |
| PreToolUse(Bash) auto-rewrite + deny fallback | `engraph-cli` | `engraph hook pre-bash` |
| JSONL ingest with rotation guard, sidechain filtering, per-file transactional commit | `engraph-ingest` | `engraph ingest` |
| FTS5 + scoped retrieval + KG (F3) | `engraph-retrieve` | `engraph recall` |
| SessionStart auto-context inject (F4) | `engraph-cli` | `engraph hook session-start` |
| Compress-existing sweep | `engraph-ingest` | `engraph compress-existing` |
| Local embeddings + hybrid retrieval | `engraph-core::embedding`, `engraph-retrieve::hybrid` | `engraph init-embeddings`, `engraph reindex-embeddings`, `engraph recall --hybrid` |
| Structure-aware code retrieval (F2 Phases 2.1 + 2.2 + 2.3) | `engraph-codegraph` | `engraph index`, `engraph index --workspace`, `engraph index --bazel-symbols`, `engraph subgraph` |

### Supported wrapper commands (v2)

The `engraph run` registry (and the auto-rewrite hook) recognizes:

| Bucket | Commands |
|---|---|
| git | `log`, `diff`, `status`, `show` (all forms, incl. `--graph`, `--stat`, `--oneline`) |
| cargo | `test`, `build`, `check`, `clippy`, `doc`, `bench`, `audit`, `tree` |
| npm | `install` (+ `i`, `ci`), `test` (+ `t`) |
| Python | `pytest`, `pip install`, `pip list`, `uv install`/`sync`/`add` |
| Lint | `ruff`, `mypy`, `eslint`, `tsc` |
| Go | `go test`, `go build`, `go vet`, `go mod tidy` |
| JS/TS extras | `yarn install`/`add`, `pnpm install`/`add`/`i`, `jest`/`vitest`/`mocha` |
| Containers | `docker ps`/`images`, `docker logs`, `docker compose ps`/`logs`, `kubectl get`/`logs`/`describe` |
| GitHub CLI | `gh pr`/`issue`/`repo` with `list` or `view` |
| Build systems | `make`, `mvn`, `gradle` (and `./gradlew`) |
| Search | `rg`, `grep` |
| Listings | `tree`, `fd`, `ls` |

Unrecognized commands route through a generic fallback that strips ANSI, dedupes consecutive lines, and applies extractive ranking. Adding a new filter is a single function in `crates/engraph-compress/src/filters/` plus an arm in `filters::pick`.

The cargo test wrapper recognizes both libtest (`test foo ... ok` / `---- foo stdout ----`) and cargo-nextest (`PASS [   0.005s] pkg test` / `FAIL [   0.005s] pkg test`) output, so passing test lines are dropped and failure counts are accurate against either runner.

## Install

Engraph builds and runs on Linux, macOS, and Windows. Two paths:

### From a release archive (recommended)

Pre-built archives are attached to each GitHub Release. Each archive ships
`engraph` (or `engraph.exe`), the README, and platform-specific install
scripts (`install.sh` / `install.ps1`).

**Targets published:** `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`.

```bash
# macOS / Linux
tar -xzf engraph-<version>-<target>.tar.gz
cd engraph-<version>-<target>
./install.sh
```

```powershell
# Windows
Expand-Archive engraph-<version>-x86_64-pc-windows-msvc.zip
cd engraph-<version>-x86_64-pc-windows-msvc
.\install.ps1
```

The script assumes the `engraph` binary lives in the same directory it does
(the release-archive layout), installs it under a per-user prefix, and merges
the SessionStart + PreToolUse(Bash) hooks into `~/.claude/settings.json`.
Re-running replaces engraph's entries in-place rather than duplicating them.

### From source

```bash
git clone <repo>
cd engraph
cargo build --release                       # default build, no embeddings
cargo build --release --features embeddings # opt-in semantic retrieval (~150MB ONNX runtime + model)

# Either install the binary by hand:
install -m 0755 target/release/engraph ~/.local/bin/engraph

# …or run the bundled installer (auto-falls-back to ./target/release):
./scripts/install.sh
```

## Wiring into Claude Code

Add to `~/.claude/settings.json`:

```jsonc
{
  "hooks": {
    "SessionStart": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "engraph hook session-start" }] }
    ],
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "engraph hook pre-bash" }] }
    ],
    "SessionEnd": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "engraph ingest --from-stdin" }] }
    ]
  }
}
```

### How `pre-bash` rewrites commands

The PreToolUse hook on Bash uses Claude Code's `hookSpecificOutput.updatedInput` to **silently rewrite eligible commands** through `engraph run` before they execute. When Claude tries `git log -n 5`, the hook returns:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "allow",
    "updatedInput": { "command": "engraph run git log -n 5" }
  }
}
```

Claude Code substitutes the new command before running it. The rewrite is invisible to Claude's reasoning loop — it sees the wrapped command in the transcript with the compressed output. PostToolUse hooks cannot replace tool results (they can only append), so this PreToolUse-rewrite pattern is the only path to transparent compression inside Claude Code.

Compound commands (`cd /tmp && git log`, `git log | head`, env-prefixed forms) fall back to a deny+suggest decision pointing Claude at the wrappable subcommand. Quoted args with spaces or shell metacharacters are preserved through `shell-words` quoting.

## Storage

One SQLite DB, WAL mode, schema migrations under `engraph-core/src/schema.rs`. Current schema version is `6`. v5 drops the `AFTER UPDATE` triggers on `messages` and `context_items`, so in-place compression via `engraph compress-existing` no longer overwrites the FTS index. v6 adds `file_path`, `line_range`, and `signature` columns to `entities` (plus an index on `file_path`) for the F2 codegraph; `relations.kind` is validated in Rust (`RelationKind` enum) rather than via a DB-level CHECK constraint.

| Table | Purpose |
|---|---|
| `sessions`, `messages`, `tool_calls` | Session-state snapshot from JSONL ingestion |
| `scopes`, `scope_members` | Hierarchical scoping (mempalace wings/rooms/drawers, generalized) |
| `context_items`, `bugs`, `do_not_repeat` | Curated decisions and rules |
| `entities`, `relations` | Knowledge graph with validity windows and provenance flags; F2 codegraph stores symbols here with `file_path` / `line_range` / `signature` (v6) |
| `messages_fts`, `context_items_fts` | FTS5 virtual tables auto-synced via triggers |
| `embeddings` | Vector store (populated only with `--features embeddings`) |
| `events`, `session_budget` | Telemetry + per-session token budget |
| `ingestion_log` | JSONL ingest offsets + rotation fingerprint (inode + size) |

## Token-savings telemetry

Every compression, retrieval, and wrapped-command invocation writes a row to `events`. `engraph gain` prints a per-feature table:

```
kind         feature         count   input_tk  output_tk   saved_tk
wrapped_cmd  F1                  1       1024         50        974
compress     F6                  3      12440       4730       7710
retrieve     F3                  8          0        842          -
TOTAL_SAVED                                                    8684
```

`saved_tk` is only meaningful for kinds where input represents the pre-compression size (`compress`, `wrapped_cmd`); other kinds show `-`.

When `CLAUDE_SESSION_ID` is set in the environment of an `engraph run` invocation, the post-filter output token count is also charged to that session's `session_budget` row — so `engraph budget status --session-id <sid>` reflects real consumption from wrapped commands. Running outside a Claude session (no env var) skips the budget charge.

## Hybrid retrieval (semantic + lexical)

When built with `--features embeddings`, `engraph recall --hybrid <query>` combines lexical search (BM25 from SQLite FTS5) and semantic search (cosine similarity over locally-computed embeddings). This section explains the algorithm, why it exists, and when to use it.

### Why hybrid

Lexical-only retrieval misses synonyms: a query for "auth" won't find a message about "login". Semantic-only retrieval misses identifier exact-matches: a query for `processOrder` against an embedding model trained on prose loses the compositional cue. Hybrid trades a small per-query embedding cost for higher recall on real-world queries that mix concept and identifier vocabulary.

### Why not a weighted sum of scores

The natural-looking formula is

```
score(d) = α·BM25(d) + β·cosine(d)
```

This is mathematically broken. BM25 scores are unbounded positive (typically 0–20+, occasionally higher), while cosine sits in [−1, 1]. The larger-scale source dominates the sum regardless of weights — cosine ends up acting as a tiebreaker for BM25 rather than as a co-equal signal. Min-max normalization within the candidate set could fix this but is sensitive to outliers and varies per query.

### The algorithm: Reciprocal Rank Fusion

Engraph uses **Reciprocal Rank Fusion** (RRF; Cormack, Clarke, Büttcher, SIGIR 2009):

```
rrf_score(d) = Σ over sources i:  w_i / (k + rank_i(d))
```

For each candidate document `d` and each source list `i` (lexical, semantic), look up `d`'s 1-based rank in that source and contribute `w_i / (k + rank_i(d))`. Documents missing from a source contribute zero for that term.

Constants used by Engraph:

| Constant | Value | Meaning |
|---|---|---|
| `K_RRF` | `60.0` | Smoothing — the standard value from the RRF paper. Larger `k` flattens the bonus for top-of-list documents. |
| `W_LEXICAL` | `1.0` | Weight on the BM25 ranking. |
| `W_SEMANTIC` | `1.0` | Weight on the embedding-cosine ranking. |
| `W_RECENCY` | `0.5` | Weight on the recency ranking (newest `ts` first). Half the content weight: freshness is a tiebreaker, not a primary signal. |
| `CANDIDATE_MULT` | `4` | The FTS stage pulls `q.limit * 4` candidates so the reranker has headroom. |

Why this works:
- **Scale-free.** Only ranks are combined; raw score scales don't matter.
- **Robust to missing data.** A document with no embedding still gets its full lexical contribution; nothing collapses to zero.
- **Composable.** Adding a third source (e.g., recency) is just another term in the sum.

The maximum achievable score is `(W_LEXICAL + W_SEMANTIC + W_RECENCY) / (K_RRF + 1)` ≈ `0.0410` with the defaults — a document ranked first in all three sources.

### Pipeline

1. Sanitize the query (strip FTS5 meta-characters; quote and AND each remaining word).
2. Run the lexical search with the widened limit (`q.limit * CANDIDATE_MULT`), recording each candidate's BM25 rank.
3. Embed the query text once via the configured `EmbeddingProvider`.
4. For every candidate, fetch its stored vector under the current `model_id` (rows from a stale model are ignored). Score by cosine; sort the candidate set; record each candidate's cosine rank.
5. Compute RRF per candidate by combining its lexical and semantic ranks. Candidates without a stored embedding get zero on the semantic term.
6. Sort by fused score, stable secondary by `target_id`, truncate to `q.limit`.

### When to use it

- **Use hybrid** when you query a corpus with synonyms or paraphrase — sessions where users describe the same concept in different words, code-adjacent prose mixing identifiers and English, or long-running projects where vocabulary drifts.
- **Stick with `Strategy::Fts` (the default)** for short-lived projects, tight identifier matching, or any environment that can't tolerate the embeddings dependency (~150MB on disk; the model also requires the ONNX runtime). The FTS path is deterministic, faster cold, and has zero per-query model load.

### Operationally

```bash
# Build with embeddings enabled.
cargo build --release --features embeddings

# Materialize the local model (downloads bge-small-en-v1.5, ~130MB).
engraph init-embeddings

# Embed all existing messages under the current model.
engraph reindex-embeddings --batch 500

# Hybrid recall (semantic + lexical).
engraph recall --hybrid "auth flow"
```

`engraph reindex-embeddings` only embeds rows that don't yet have a vector under the current `model_id`, so it's safe to re-run; a model upgrade (new `model_id`) requires a fresh full reindex by design — old embeddings are not silently mixed with new ones.

### Future signals

The RRF combinator is intentionally open-ended. Recency is wired in today as a
third source list (`W_RECENCY = 0.5`), ranking candidates by `ts` descending
(RFC3339 strings sort chronologically). Candidates without a `ts` are treated
as missing from the recency list. Natural future signals — engagement, scope
proximity, author — would each enter as one more term in the same
`Σ w_i / (k + rank_i(d))` formula.

## Structure-aware code retrieval (F2 Phase 2.1)

`engraph index <repo>` runs a per-language SCIP indexer (engraph shells
out to the upstream binary), decodes the resulting `index.scip`, and
populates the `entities` + `relations` tables with symbols, calls,
references, imports, and inheritance edges. `engraph subgraph <symbol>`
then returns a 2-hop markdown neighborhood — typically ~100× smaller
than the Read+grep loop Claude would otherwise run to answer "what
calls processOrder."

### Supported drivers

| Driver | `detect()` trigger | Upstream indexer |
|---|---|---|
| `rust-analyzer` | `Cargo.toml` | `rust-analyzer` (rustup component) |
| `scip-python`   | `pyproject.toml` / `setup.py` | `@sourcegraph/scip-python` (npm) |
| `scip-go`       | `go.mod` | `github.com/scip-code/scip-go/cmd/scip-go` (go install) |
| `scip-typescript` | `package.json` + `tsconfig.json` | `@sourcegraph/scip-typescript` (npm) |
| `scip-java`     | `pom.xml` / `build.gradle*` / `build.sbt` / `build.sc` | `scip-java` via Coursier (`cs install --contrib scip-java`); also needs `mvn`/`gradle`/`sbt`/`mill` on PATH |

`scripts/install-scip-indexers.sh` is an idempotent installer for all
five upstream indexers with per-toolchain prerequisite checks. Phase 2.2
(cross-repo moniker stitching, see "Cross-repo stitching" below) and
Phase 2.3 (polyglot Bazel: target-level via `bazel query`, symbol-level
via the per-language indexers, see "Bazel monorepos" below) are shipped.
Remaining roadmap items live in `ROADMAP.md`.

### Example

```bash
engraph index .
# indexed /home/me/project (2416 entities, 1236 relations, 1.1MB SCIP, 14s, driver=rust-analyzer)

engraph subgraph run_migrations
## Symbol `run_migrations` (defined in crates/engraph-core/src/schema.rs:244)
```
pub fn run_migrations(conn: &mut Connection) -> Result<()>
```
**Calls**: `current_version` (crates/engraph-core/src/schema.rs:225)
**References**: `Result#` (crates/engraph-core/src/error.rs:21)
**Called by**: `open_pool` (crates/engraph-core/src/db.rs:30), `migrations_apply_idempotently` (…), `tables_exist_after_migration` (…)
**Sibling symbols** (same file): `current_version`, `check_drift`, `tests`, `MIGRATIONS`
```

`engraph subgraph <sym> --json` emits the structured `Neighborhood`
record for programmatic consumers. Ambiguous names yield a
disambiguation block listing each match by location, not a best-guess
pick.

### Cross-repo stitching (Phase 2.2)

`engraph index --workspace <dir>` indexes every sub-repo with a
recognized build manifest under `<dir>` in one pass. Each repo's
canonical path becomes its project key, and because
`entities.id` is the SCIP moniker, cross-repo references collapse onto
the same row automatically — index `lib_a` and `app_b` (where `app_b`
depends on `lib_a`), and `engraph subgraph app_caller` surfaces calls
into `lib_a` with a `repo:lib_a` annotation on the location:

```text
**Calls**: `lib_foo` (repo:lib_a :: src/lib.rs:1)
```

If `<dir>` itself carries a build manifest (a cargo workspace root, a
single-crate root, etc.) only `<dir>` is indexed; otherwise each
immediate child whose `Driver::detect()` matches gets indexed
separately. Per-repo failures are reported but do not abort the run.

### Bazel monorepos (Phase 2.3, target-level)

`engraph index` on a directory containing `WORKSPACE`,
`WORKSPACE.bazel`, or `MODULE.bazel` runs `bazel query
--output=streamed_jsonproto 'kind(rule, //...)'` once and writes one
`bazel_target` entity per rule target plus `BAZEL_DEPENDS_ON` edges
between them. No build runs; no per-language SCIP indexer is invoked
on this path. Coarse-grained but deterministic and fast.

```bash
engraph index path/to/bazel/monorepo
# indexed /path/to/bazel/monorepo (482 entities, 1731 relations, 0 SCIP bytes, 4200ms, driver=bazel-query)

engraph subgraph bar
## Symbol `bar` (defined in bar/BUILD.bazel:1)
**Bazel deps**: `foo` (foo/BUILD.bazel:1)
```

`bazel` (or `bazelisk` symlinked as `bazel`) must be on PATH; the
companion installer covers `bazelisk` via `go install
github.com/bazelbuild/bazelisk@latest`. Bazel's analysis cache lands
under `~/.cache/engraph/bazel-out/<sha-of-workspace-path>` (override
with `ENGRAPH_BAZEL_OUTPUT_BASE`) to keep it isolated from the user's
own `~/.cache/bazel`.

**Symbol-level Bazel indexing** (Phase 2.3 #2) layers on top via
`engraph index --bazel-symbols`. After the target-level pass, this drives
`scip-java` / `scip-go` / `scip-typescript` from the Bazel workspace —
scip-java's bundled aspect orchestrates Bazel internally; Go and TS run
at the workspace root. Each language probes for matching rule kinds
(`java_library`, `go_library`, `ts_project`, …) via `bazel query` and
the corresponding indexer's presence on `PATH`; missing toolchains
soft-skip per-language rather than aborting. Off by default —
toolchain downloads and full Bazel builds make it heavy; the
target-level pass remains the fast deterministic default. Known
limitations and follow-ups (multi-`go.mod`, `rules_ts` node_modules,
large Java monorepos) are documented in `ROADMAP.md`.

## Reading the events table

```bash
engraph gain --json
```

Useful queries directly against the DB:

```sql
-- Largest single compressions in the last day:
SELECT kind, feature, filter_id, input_tokens, output_tokens, ts
FROM events
WHERE kind = 'compress' AND ts > datetime('now', '-1 day')
ORDER BY (input_tokens - output_tokens) DESC LIMIT 20;

-- Per-filter ratio:
SELECT filter_id, AVG(1.0 * output_tokens / NULLIF(input_tokens, 0)) AS avg_ratio,
       COUNT(*) AS samples
FROM events
WHERE kind IN ('compress','wrapped_cmd') AND input_tokens > 0
GROUP BY filter_id ORDER BY avg_ratio ASC;
```

## Crate layout

```
crates/
├── engraph-core/          # schema, db pool, telemetry, budget, tokens, embedding trait
├── engraph-compress/      # F6 compressor + per-command filters (git, cargo, npm, tree, fd, ls)
├── engraph-retrieve/      # FTS+scoping+KG, hybrid (feature-gated)
├── engraph-ingest/        # JSONL → SQLite with rotation guard and in-place compression sweep
├── engraph-codegraph/     # F2 codegraph: SCIP indexer drivers, loader, subgraph queries
└── engraph-cli/           # the `engraph` binary
```

## Feature flags

| Feature | Effect |
|---|---|
| (default) | Lexical retrieval only; no ONNX/fastembed dependency. |
| `embeddings` | Pulls in fastembed-rs (≈150MB transitive), enables `engraph init-embeddings`, `engraph reindex-embeddings`, and `engraph recall --hybrid`. |

## Verification

```bash
# Default build:
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check

# With embeddings:
cargo test --features embeddings
cargo clippy --all-targets --features embeddings -- -D warnings
```

GitHub Actions (`.github/workflows/ci.yml`) runs `cargo build`, `cargo test`,
`cargo clippy -- -D warnings`, and `cargo fmt --check` on `ubuntu-latest`,
`macos-latest`, and `windows-latest` for every push and pull request. On
pushes to `main`, a separate `tag-and-release` job detects a workspace
version bump, pushes the tag, fans out a build matrix across the five
release targets, and publishes the archives to a GitHub Release.

The plan's verification gates are wired as tests:
- `engraph-compress/tests/git_log_ratio.rs` — F6 ratio < 0.5 on a 2k-line git log
- `engraph-compress/tests/filter_ratios.rs` — per-filter token-reduction gates, plus a picker/`FilterOutput.filter_id` agreement check that pins every wrapped command
- `engraph-compress/tests/golden_fixtures.rs` — byte-exact snapshot tests for high-signal filters (`git log`, `cargo check`, `cargo test` against nextest output) to catch format drift
- `engraph-retrieve/tests/end_to_end.rs` — ingest + recall, scope restriction, KG entity search
- `engraph-retrieve/tests/hybrid_path.rs` — RRF reordering vs FTS, recency tiebreak toward newer `ts`, unembedded-candidate fallback, idempotent upsert
- `engraph-ingest/src/lib.rs` (unit) — incremental re-ingest, rotation/truncation replay, mid-write partial-line hold, sidechain event filtering, sweep idempotency and recoverability, FTS-retention through `compress_existing`
- `engraph-cli/tests/session_start_hook.rs` — empty brief on unknown project, populated brief includes rules/bugs, size cap respected
- `engraph-cli/tests/run_budget.rs` — `engraph run` charges `session_budget` when `CLAUDE_SESSION_ID` is set, no-ops cleanly when not, inherits stdin (cat round-trip), and drains ~200KB of concurrent stdout+stderr without pipe-buffer deadlock (validates the `tokio::process` migration)
- `engraph-codegraph/tests/loader_unit.rs` — SCIP loader two-pass emits a known `CALLS` edge from a synthesized in-memory `Index`, is idempotent on re-load, scopes its DELETE to the requested `project`, and preserves co-resident `BAZEL_DEPENDS_ON` edges (Phase 2.3 #2 regression)
- `engraph-codegraph/tests/subgraph_format.rs` — embedded unit tests for the markdown formatter: section shape, ambiguity disambiguation, byte-cap truncation
- `engraph-codegraph/tests/drivers_detect.rs` — pure file-probe tests, one per build system (Cargo, pyproject, go.mod, package.json+tsconfig, pom.xml/build.gradle*/build.sbt/build.sc); also pins that a Bazel-only workspace does **not** pick scip-java (Phase 2.3 territory)
- `engraph-codegraph/tests/drivers_live.rs` — per-language end-to-end runs against tiny fixtures; soft-skip when the upstream indexer (or, for scip-java, the JVM build tool) is absent
- `engraph-codegraph/tests/workspace_cross_repo.rs` — Phase 2.2 cross-repo workspace fixture (two crates with a path dep). Asserts both repos land in `entities` with their canonical projects, a `CALLS` edge spans them, and the rendered markdown carries the `repo:<name>` annotation. Soft-skips when rust-analyzer is absent.
- `engraph-codegraph/tests/bazel_live.rs` — Phase 2.3 target-level Bazel: two-genrule fixture (no external rules), asserts both targets land as `bazel_target` entities with the right `BAZEL_DEPENDS_ON` edge and repo-relative `file_path`. Separate test pins re-index idempotency. Soft-skips when neither `bazel` nor `bazelisk` is on PATH.
- `engraph-codegraph/tests/bazel_symbols_live.rs` — Phase 2.3 #2 symbol-level Bazel: minimal `java_library` fixture, drives `scip-java` (which orchestrates Bazel via its bundled aspect), asserts symbol entities land alongside the target-level `bazel_target` row, and the surviving target row pins the loader's BAZEL_DEPENDS_ON preservation. Triple-gated (bazel + scip-java + `ENGRAPH_LIVE_BAZEL_SYMBOLS=1`) so default `cargo test` doesn't pay the 2-5 min cold cost.

## License

MIT
