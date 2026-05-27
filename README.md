# Engraph

A Rust CLI that cuts Claude Code token usage by combining persistent session memory, scoped retrieval, deterministic compression of command output, and telemetry that measures the savings.

Engraph is local-first. Storage is one SQLite file at `~/.local/share/engraph/engraph.db` (override with `ENGRAPH_DB_PATH`). No daemon, no cloud service required.

For a deep walkthrough of the algorithms and internal architecture, see [`DETAILS.md`](DETAILS.md).

---

## Features

### Command output compression

`engraph run <cmd> [args...]` wraps a command, runs a per-command filter on its output, and emits the compressed result. The PreToolUse hook on Bash silently rewrites eligible commands through `engraph run` before they execute — Claude never sees the uncompressed output.

Recognized commands:

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

The cargo test wrapper recognizes both libtest (`test foo ... ok` / `---- foo stdout ----`) and cargo-nextest (`PASS [   0.005s] pkg test`) output formats.

### Session memory

`engraph ingest` ingests Claude Code's JSONL transcript files into SQLite. Wired as a `SessionEnd` hook, it runs automatically when a session closes. Ingestion is incremental — it tracks file offsets and handles log rotation and truncation correctly.

`engraph recall <query>` retrieves relevant messages and context items using FTS5 full-text search. Results are scoped by working directory so queries in one project don't surface noise from another.

### SessionStart context injection

`engraph hook session-start` reads the project's stored do-not-repeat rules and open bugs from the database and injects them as `additionalContext` at the start of each Claude session. Empty briefs cost zero tokens. The cap is 2KB to keep the per-session opening cost predictable.

### Structure-aware code indexing

`engraph index <repo>` runs a per-language SCIP indexer, decodes the resulting `index.scip`, and populates the `entities` and `relations` tables with symbols, calls, references, imports, and inheritance edges.

`engraph subgraph <symbol>` returns a 2-hop markdown neighborhood — typically orders of magnitude smaller than the file-read-and-grep loop Claude would otherwise run to answer "what calls `processOrder`."

**Supported languages:**

| Driver | Detected by | Upstream indexer |
|---|---|---|
| `rust-analyzer` | `Cargo.toml` | `rust-analyzer` (rustup component) |
| `scip-python` | `pyproject.toml` / `setup.py` | `@sourcegraph/scip-python` (npm) |
| `scip-go` | `go.mod` | `github.com/scip-code/scip-go/cmd/scip-go` (go install) |
| `scip-typescript` | `package.json` + `tsconfig.json` | `@sourcegraph/scip-typescript` (npm) |
| `scip-java` | `pom.xml` / `build.gradle*` / `build.sbt` / `build.sc` | `scip-java` via Coursier (`cs install --contrib scip-java`) |

`scripts/install-scip-indexers.sh` is an idempotent installer for all five upstream indexers with per-toolchain prerequisite checks.

**Cross-repo stitching:** `engraph index --workspace <dir>` indexes every sub-repo with a recognized build manifest in one pass. Because `entities.id` is the SCIP moniker, cross-repo references collapse onto the same row automatically. `engraph subgraph app_caller` surfaces calls into a dependency library with a `repo:<name>` annotation on the location:

```text
**Calls**: `lib_foo` (repo:lib_a :: src/lib.rs:1)
```

**Bazel monorepos:** `engraph index` on a directory containing `WORKSPACE`, `WORKSPACE.bazel`, or `MODULE.bazel` runs `bazel query --output=streamed_jsonproto 'kind(rule, //...)'` and writes one `bazel_target` entity per rule target plus `BAZEL_DEPENDS_ON` edges. No build runs. Fast and deterministic.

`engraph index --bazel-symbols` adds symbol-level indexing on top by driving `scip-java` / `scip-go` / `scip-typescript` against the same Bazel workspace. Off by default — toolchain downloads and full Bazel builds make it heavy.

### Hybrid retrieval

When built with `--features embeddings`, `engraph recall --hybrid <query>` combines lexical search (BM25 via FTS5) and semantic search (cosine similarity over locally-computed embeddings) using Reciprocal Rank Fusion.

Lexical-only retrieval misses synonyms; semantic-only retrieval misses exact identifier matches. RRF combines ranks rather than scores, so the two signals are scale-independent and composable:

```
rrf_score(d) = w_lex / (k + lex_rank(d))
             + w_sem / (k + sem_rank(d))
             + w_rec / (k + rec_rank(d))   # recency tiebreaker
```

Constants: `K_RRF = 60.0` (standard RRF paper value), `W_LEXICAL = W_SEMANTIC = 1.0`, `W_RECENCY = 0.5` (freshness as tiebreaker, not primary signal).

### Token savings telemetry

Every compression, retrieval, and wrapped-command invocation writes a row to `events`. `engraph gain` prints a per-feature table:

```
kind         feature          count   input_tk  output_tk   saved_tk
wrapped_cmd  git                  5       8200        980       7220
compress     session-memory       3      12440       4730       7710
retrieve     recall               8          0        842          -
TOTAL_SAVED                                                   14930
```

---

## Install

Engraph builds and runs on Linux, macOS, and Windows.

### From a release archive (recommended)

Pre-built archives are attached to each GitHub Release. Each archive contains `engraph` (or `engraph.exe`), the README, and platform-specific install scripts.

**Targets:** `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`.

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

The install script places the binary under a per-user prefix and merges the SessionStart and PreToolUse(Bash) hooks into `~/.claude/settings.json`. Re-running replaces existing entries in-place rather than duplicating them.

### From source

```bash
git clone <repo>
cd engraph
cargo build --release                       # default: lexical retrieval only
cargo build --release --features embeddings # opt-in: semantic retrieval (~150MB ONNX runtime + model)

# Install the binary:
install -m 0755 target/release/engraph ~/.local/bin/engraph

# Or use the bundled installer (falls back to ./target/release/):
./scripts/install.sh
```

---

## Wire into Claude Code

Add to `~/.claude/settings.json`:

```jsonc
{
  "hooks": {
    "SessionStart": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "engraph hook session-start" }] }
    ],
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "engraph hook pre-bash" }] },
      { "matcher": "Grep", "hooks": [{ "type": "command", "command": "engraph hook pre-grep" }] }
    ],
    "PostToolUse": [
      { "matcher": "Read", "hooks": [{ "type": "command", "command": "engraph hook post-read" }] }
    ],
    "SessionEnd": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "engraph ingest --from-stdin" }] }
    ]
  }
}
```

The install script does this automatically.

### How `pre-bash` rewrites commands

The PreToolUse hook uses Claude Code's `hookSpecificOutput.updatedInput` to silently rewrite eligible commands through `engraph run` before they execute. When Claude tries `git log -n 5`, the hook returns:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "allow",
    "updatedInput": { "command": "engraph run git log -n 5" }
  }
}
```

Claude Code substitutes the rewritten command before running it. The rewrite is invisible to Claude's reasoning loop — Claude sees the wrapped command in the transcript with compressed output. PostToolUse hooks cannot replace tool results (only append), so this PreToolUse-rewrite pattern is the only path to transparent compression inside Claude Code.

Compound commands (`cd /tmp && git log`, `git log | head`, env-prefixed forms) fall back to a deny+suggest response pointing Claude at the wrappable subcommand. Quoted args with spaces or shell metacharacters are preserved correctly through `shell-words` quoting.

### How `pre-grep` redirects symbol lookups

After `engraph index .` populates the codegraph, the PreToolUse hook on Grep watches for bareword patterns (`^[A-Za-z_][A-Za-z0-9_]*$`, length ≥ 3) that resolve to **1–3 entities** by `name` or moniker. On a hit, it returns `permissionDecision: "deny"` with a message pointing Claude at `engraph subgraph <symbol>` — typically 100× smaller than the file-read-and-grep loop Claude would otherwise run.

The same redirect fires for `rg <symbol>` and `grep <symbol>` invoked via the Bash tool — checked inside `pre-bash` before the compression rewrite, so subgraph wins over `engraph run rg <symbol>` when both apply.

### How `post-read` enriches Read results

PostToolUse(Read) appends a brief listing of indexed symbols in the file as `hookSpecificOutput.additionalContext` — name, line range, and signature for up to 30 entities, capped at `MAX_BRIEF_BYTES`. Often answers "what's in this file" without Claude needing a follow-up grep or subgraph call. Silent passthrough for files not in the graph. Telemetry feature `F3_post_read`.

The gate is deliberately narrow:

| Pattern | Resolves to | Decision |
|---|---|---|
| `processOrder` | 1 entity | deny + suggest `engraph subgraph processOrder` |
| `parse` | 12 entities | passthrough (ambiguous; subgraph would emit a disambiguation block) |
| `unindexed_helper` | 0 entities | passthrough (not in graph) |
| `process.*` | — | passthrough (regex metachar) |
| `id` | — | passthrough (too short) |

When Claude wants the raw grep anyway (e.g. to search for a literal occurrence in comments), adding a regex metachar like `\b` bypasses the redirect.

---

## Usage

```bash
# Index a repo (auto-detects language)
engraph index .
# indexed /home/me/project (2416 entities, 1236 relations, 14s, driver=rust-analyzer)

# Index a workspace (multiple repos)
engraph index --workspace /path/to/monorepo

# Query the code graph
engraph subgraph run_migrations
## Symbol `run_migrations` (defined in crates/engraph-core/src/schema.rs:244)
```
pub fn run_migrations(conn: &mut Connection) -> Result<()>
```
**Calls**: `current_version` (crates/engraph-core/src/schema.rs:225)
**Called by**: `open_pool` (crates/engraph-core/src/db.rs:30)
**Sibling symbols** (same file): `current_version`, `check_drift`, `MIGRATIONS`

# JSON output for programmatic use
engraph subgraph run_migrations --json

# Recall from session memory
engraph recall "auth flow"
engraph recall --hybrid "auth flow"    # requires --features embeddings

# Ingest transcript files
engraph ingest ~/.claude/projects/<project>/*.jsonl

# Compress existing stored messages
engraph compress-existing

# Token savings report
engraph gain
engraph gain --json

# Per-session token budget
engraph budget status
engraph budget set --soft 80000 --hard 120000

# Run a command through a filter directly
engraph run git log -n 20
engraph run cargo test

# Embeddings (requires --features embeddings)
engraph init-embeddings          # download model (~130MB)
engraph reindex-embeddings       # embed all stored messages
```

### Subgraph disambiguation

Ambiguous symbol names return a disambiguation block listing each match by location rather than a best-guess pick:

```
Ambiguous: `foo` matches 3 symbols:
  1. src/lib.rs:12  — fn foo()
  2. src/util.rs:8  — fn foo(x: i32)
  3. tests/mod.rs:3 — fn foo()
```

---

## Build and test

```bash
# Default build
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check

# With embeddings
cargo build --release --features embeddings
cargo test --features embeddings
cargo clippy --all-targets --features embeddings -- -D warnings
```

GitHub Actions (`.github/workflows/ci.yml`) runs `cargo build`, `cargo test`, `cargo clippy`, and `cargo fmt --check` on `ubuntu-latest`, `macos-latest`, and `windows-latest` for every push and pull request. On pushes to `main`, a separate job detects a workspace version bump, pushes the tag, fans out a build matrix across the five release targets, and publishes the archives to a GitHub Release.

### Test coverage

Key test locations:

| What's tested | Location |
|---|---|
| Per-filter compression ratios | `engraph-compress/tests/filter_ratios.rs` |
| Golden snapshot output (git log, cargo check, cargo test) | `engraph-compress/tests/golden_fixtures.rs` |
| Compress idempotency (fixed-point property) | `engraph-compress/src/lib.rs` |
| Pre-bash hook branches (rewrite, deny, passthrough) | `engraph-cli/tests/pre_bash_hook.rs` |
| SessionStart brief content and size cap | `engraph-cli/tests/session_start_hook.rs` |
| `engraph run` budget tracking and stdin inheritance | `engraph-cli/tests/run_budget.rs` |
| Ingest: rotation/truncation replay, partial-line handling, sidechain skip | `engraph-ingest/src/lib.rs` |
| FTS retention through compress-existing sweep | `engraph-ingest/src/lib.rs` |
| Recall, scope restriction, KG entity search | `engraph-retrieve/tests/end_to_end.rs` |
| Hybrid RRF reordering, recency tiebreak, missing embeddings | `engraph-retrieve/tests/hybrid_path.rs` |
| SCIP loader: entities, edges, idempotency, project scoping | `engraph-codegraph/tests/loader_unit.rs` |
| Subgraph markdown shape, disambiguation, byte-cap truncation | `engraph-codegraph/tests/subgraph_format.rs` |
| Driver file-probe detection (one per build system) | `engraph-codegraph/tests/drivers_detect.rs` |
| Driver live end-to-end (soft-skips when indexer absent) | `engraph-codegraph/tests/drivers_live.rs` |
| Cross-repo workspace discovery and CALLS edge annotation | `engraph-codegraph/tests/workspace_cross_repo.rs` |
| Bazel target-level index and re-index idempotency | `engraph-codegraph/tests/bazel_live.rs` |
| Bazel symbol-level Java end-to-end (gated behind env var) | `engraph-codegraph/tests/bazel_symbols_live.rs` |

---

## Crate layout

```
crates/
├── engraph-core/          # schema, db pool, telemetry, budget, tokens, embedding trait
├── engraph-compress/      # compressor + per-command output filters
├── engraph-retrieve/      # FTS, scoping, knowledge graph, hybrid retrieval (feature-gated)
├── engraph-ingest/        # JSONL → SQLite with rotation guard and compress sweep
├── engraph-codegraph/     # SCIP indexer drivers, loader, subgraph queries
└── engraph-cli/           # the `engraph` binary
```

The dependency graph is strictly layered: `engraph-cli` consumes everything; `engraph-core` has no dependencies on the other engraph crates.

---

## Storage

One SQLite database, WAL mode. Schema migrations are versioned and applied automatically on open; a binary built against an older schema version refuses to run against a newer database.

| Table | Purpose |
|---|---|
| `sessions`, `messages`, `tool_calls` | Session-state snapshot from JSONL ingestion |
| `scopes`, `scope_members` | Hierarchical scoping (project / topic / time-window) |
| `context_items`, `bugs`, `do_not_repeat` | Curated decisions and rules |
| `entities`, `relations` | Knowledge graph; codegraph stores symbols here with `file_path`, `line_range`, `signature` |
| `messages_fts`, `context_items_fts` | FTS5 virtual tables auto-synced via triggers |
| `embeddings` | Vector store (populated only with `--features embeddings`) |
| `events`, `session_budget` | Telemetry and per-session token budget |
| `ingestion_log` | JSONL ingest offsets and rotation fingerprint |

Useful queries:

```bash
engraph gain --json
```

```sql
-- Largest single compressions in the last day:
SELECT kind, feature, filter_id, input_tokens, output_tokens, ts
FROM events
WHERE kind = 'compress' AND ts > datetime('now', '-1 day')
ORDER BY (input_tokens - output_tokens) DESC LIMIT 20;

-- Per-filter compression ratio:
SELECT filter_id, AVG(1.0 * output_tokens / NULLIF(input_tokens, 0)) AS avg_ratio,
       COUNT(*) AS samples
FROM events
WHERE kind IN ('compress','wrapped_cmd') AND input_tokens > 0
GROUP BY filter_id ORDER BY avg_ratio ASC;
```

---

## Feature flags

| Feature | Effect |
|---|---|
| (default) | Lexical retrieval only; no ONNX dependency |
| `embeddings` | Adds fastembed-rs (~150MB transitive), enables `engraph init-embeddings`, `engraph reindex-embeddings`, and `engraph recall --hybrid` |

---

## License

MIT
