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
| File reads | `cat`, `bat`, `less` (whole-file: head + elided middle + tail window when large); `head`, `tail` (user-windowed: dedup + comment-strip only) |

Unrecognized commands route through a generic fallback that strips ANSI escapes, collapses runs of identical lines, and applies extractive ranking. Adding a new filter is a single function in `crates/engraph-compress/src/filters/` plus an arm in `filters::pick`.

File-read filters strip line comments by extension (Python `#`; Rust / Go / JS / TS / JSX / TSX `//`) and collapse runs of blank lines. Whole-file reads over ~500 lines get a head + elided-middle + tail window. If language stripping accidentally empties a non-empty input, the raw text is returned with a marker so Claude never sees a blank file by mistake.

The cargo test wrapper recognizes both libtest (`test foo ... ok` / `---- foo stdout ----`) and cargo-nextest (`PASS [   0.005s] pkg test`) output formats.

The pre-bash parser handles common command shapes that would otherwise misroute. Env prefixes like `FOO=bar cmd args` are peeled for classification and re-emitted ahead of `engraph run` so the assignment lands in the child's environment. Absolute paths (`/usr/bin/git`) normalize to the bare command name. Git's global options (`-C`, `-c`, `--git-dir=…`, `--work-tree=…`) are stripped before subcommand classification so `git -C /tmp status` routes to the `git status` filter. Heredocs (`cat <<'EOF' … EOF`), `sudo`, and `env <vars> cmd` pass through unmodified — rewriting them would corrupt the body or drop privileges.

### Session memory

`engraph ingest` ingests Claude Code's JSONL transcript files into SQLite. Wired as a `SessionEnd` hook, it runs automatically when a session closes. Ingestion is incremental — it tracks file offsets and handles log rotation and truncation correctly.

`engraph recall <query>` retrieves relevant messages and context items using FTS5 full-text search. Results are scoped by working directory so queries in one project don't surface noise from another.

### SessionStart context injection

`engraph hook session-start` injects prior project context as `additionalContext` at the start of each Claude session: stored do-not-repeat rules, open bugs, recent decisions, and budget status (each section capped at the 5 most-recent entries). It also performs a best-effort catch-up ingest of the project's transcripts first, so sessions that closed without a clean `SessionEnd` are still captured (incremental and idempotent via `ingestion_log` offsets). Empty briefs cost zero tokens. The cap is 2KB to keep the per-session opening cost predictable.

### Structure-aware code indexing

`engraph index <repo>` runs a per-language SCIP indexer, decodes the resulting `index.scip`, and populates the `entities` and `relations` tables with symbols, calls, references, imports, and inheritance edges. When a repo's root advertises multiple build manifests (e.g. `pyproject.toml` + `tsconfig.json` for a Python+TypeScript project, or `pyproject.toml` + `pom.xml` for a Python+Java mono-service), every matching driver runs and their SCIP outputs are merged into one index. Per-driver failures are surfaced as warnings; the load proceeds with whatever succeeded. `--lang <name>` or `--scip <path>` pin to a single source as before. `--scip-manifest <file>` ingests externally-produced SCIP files (tab-separated `<repo-relative-root>` + `<scip-file>` lines), rebasing each to repo-root and merging them into one load — for orchestrators that build SCIP per language or module out-of-band.

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

**Cross-repo stitching:** `engraph index --workspace <dir>` indexes every sub-repo with a recognized build manifest in one pass — immediate children only, or add **`--recursive`** to discover projects at any depth (nested and mixed-language modules, e.g. a `go/` submodule inside a Rust crate), pruning build/dep dirs (`node_modules`, `target`, …) and stopping at Bazel roots. Because `entities.id` is the SCIP moniker, cross-repo references collapse onto the same row automatically. `engraph subgraph app_caller` surfaces calls into a dependency library with a `repo:<name>` annotation on the location:

```text
**Calls**: `lib_foo` (repo:lib_a :: src/lib.rs:1)
```

**Bazel monorepos:** `engraph index` on a directory containing `WORKSPACE`, `WORKSPACE.bazel`, or `MODULE.bazel` runs `bazel query --output=streamed_jsonproto 'kind(rule, //...)'` and writes one `bazel_target` entity per rule target plus `BAZEL_DEPENDS_ON` edges. No build runs. Fast and deterministic.

`engraph index --bazel-symbols` adds symbol-level indexing on top of the target-level pass. **Defaults: on for `--workspace` runs** (a workspace index is already a one-time commitment, and the symbol pass is the only path to function-level data inside Bazel), **off for single-repo `engraph index <repo>`** (the target-level Bazel pass is fast and deterministic by itself). Use `--no-bazel-symbols` to disable inside a workspace run. Each language is driven the way that actually works on real Bazel monorepos:

- **Go** — *delegated when configured.* If **`ENGRAPH_BAZEL_SCIP_GO_CMD`** is set, engraph runs it as `<cmd> <repo> <out.scip>` and merges the SCIP it writes (the same contract as Java) — the only way to reach gazelle-managed `go_library` targets that have no `go.mod` (scip-go needs a module root *and* Bazel-resolved deps, both repo-specific). A ready-made best-effort driver ships as **`docs/examples/scip-go-bazel-index.sh`** (scope module roots with `ENGRAPH_BAZEL_SCIP_GO_ROOTS`). Unset → the native multi-module pass: enumerate every `go.mod` under the workspace, run `scip-go` per module, rebase paths to repo-root, and merge — its `symbol[go]: indexed N go.mod modules of M go targets` line makes that pass's coverage gap visible (`go_library` targets without a `go.mod` are unreachable by the native pass).
- **Java** — *delegated*. A Java SCIP build is too repo-specific to bake in (a Bazel SemanticDB aspect, Maven, Gradle, and custom annotation-processor toolchains all differ), so engraph runs the command named by **`ENGRAPH_BAZEL_SCIP_JAVA_CMD`** as `<cmd> <repo> <out.scip>` and merges the SCIP it writes; unset reports `skipped (… not set)`. A ready-made Bazel driver — the scip-java SemanticDB aspect patched for Bazel 8 + custom annotation-processor toolchains — ships as **`docs/examples/scip-java-bazel-index.sh`**. Point the env var at it (optionally scope with `ENGRAPH_BAZEL_SCIP_JAVA_TARGETS`, default first-party Java roots):

  ```sh
  export ENGRAPH_BAZEL_SCIP_JAVA_CMD="$PWD/docs/examples/scip-java-bazel-index.sh"
  export ENGRAPH_BAZEL_SCIP_JAVA_TARGETS='//src/java/...'   # optional; scope to your repo's first-party Java roots
  engraph index --bazel-symbols ~/src/monorepo
  ```

  Any other Java build system plugs in by pointing the same env var at its own SCIP-producing command — engraph stays build-system-agnostic.
- **TypeScript / Python** — run their standalone indexers against the workspace root (best-effort; `rules_ts` `node_modules` and `rules_python` venv resolution are follow-ups).

### Hybrid retrieval

When built with `--features embeddings`, `engraph recall --hybrid <query>` combines lexical search (BM25 via FTS5) and semantic search (cosine similarity over locally-computed embeddings) using Reciprocal Rank Fusion.

Lexical-only retrieval misses synonyms; semantic-only retrieval misses exact identifier matches. RRF combines ranks rather than scores, so the two signals are scale-independent and composable:

```
rrf_score(d) = w_lex / (k + lex_rank(d))
             + w_sem / (k + sem_rank(d))
             + w_rec / (k + rec_rank(d))   # recency tiebreaker
```

Constants: `K_RRF = 60.0` (standard RRF paper value), `W_LEXICAL = W_SEMANTIC = 1.0`, `W_RECENCY = 0.5` (freshness as tiebreaker, not primary signal).

To make Claude actually reach for `--hybrid`, import the embeddings variant of the memory-guidance file (`docs/engraph-embeddings.md`) instead of `docs/engraph.md` — see [Install](#from-a-release-archive-recommended). Keep embeddings fresh with `engraph reindex-embeddings` (new transcripts are ingested at `SessionEnd` but not auto-embedded); the bundled [`engraph-refresh` skill](#claude-code-skill-engraph-refresh) wraps this.

### Token savings telemetry

Every compression, retrieval, and wrapped-command invocation writes a row to `events`. `engraph gain` prints a savings summary, a three-bucket "by source" breakdown, and a per-feature table (all with a Save% column):

```
== engraph gain ==
commands : 93
input_tk : 37323
output_tk: 22570
saved_tk : 14753
save%    : 39.5

by source
source        count   input_tk  output_tk   saved_tk   share  save%
command          91      31444      22073       9371   63.5%   29.8
codegraph         2       5879        497       5382   36.5%   91.5
kind         feature         count   input_tk  output_tk   saved_tk  save%
retrieve     subgraph            2       5879        497       5382   91.5
wrapped_cmd  output_filter      90      31090      22073       9017   29.0
TOTAL_SAVED                                                   14753
```

Savings come from three sources, partitioned by the `by source` table: **command** output compression (`wrapped_cmd`), **codegraph** retrieval (`subgraph` replacing a file-read+grep loop), and **memory** (message compression at ingest/sweep). Save% is computed only over these savings-bearing rows; rows where input/output carry no savings semantic — `recall`, `hook`, and `index` (which records millions of input tokens against 0 output) — show `-` and never inflate the totals. Flags add detail without changing the underlying numbers:

| Flag | Report |
|---|---|
| `--by-filter` | itemized breakdown across **all** sources — commands by name (`rg`, `git_log`), plus `subgraph` and `compress_*` rows; its TOTAL matches the summary |
| `--by-project` / `--by-session` | savings scoped via the `sessions` join |
| `--daily` / `--weekly` / `--monthly` / `--all` | time-bucketed breakdowns |
| `--graph` | horizontal bar chart of saved tokens/day over the last 30 days |
| `--history [N]` | the most recent N savings events |
| `--format text\|json\|csv` (or `--json`) | machine-readable export |

The same telemetry can be exported to an OpenTelemetry collector (OTLP/gRPC) for
dashboards — off by default, behind the `otel` build feature and `ENGRAPH_OTEL`.
See [docs/opentelemetry.md](docs/opentelemetry.md), which covers enabling it and
the required collector `deltatocumulative` config.

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

The install script places the binary under a per-user prefix and merges the `SessionStart`, `PreToolUse(Bash, Grep)`, `PostToolUse(Read)`, and `SessionEnd` hooks into `~/.claude/settings.json`. It also installs the memory-capture guidance (`docs/engraph.md` → `~/.claude/engraph.md`, imported via `@engraph.md` in your `CLAUDE.md`), offers to install the [`engraph-refresh` skill](#claude-code-skill-engraph-refresh), and offers to run the SCIP-indexer installer. Re-running replaces existing entries in-place rather than duplicating them.

If you build with `--features embeddings`, swap the memory-guidance import to the embeddings variant — `docs/engraph-embeddings.md`, a superset of `docs/engraph.md` that adds a *Semantic recall* section steering Claude to prefer `engraph recall --hybrid` for conversation memory. Copy it to `~/.claude/engraph-embeddings.md` and import `@engraph-embeddings.md` (instead of `@engraph.md`) in your `CLAUDE.md`. Use exactly one of the two.

### From source

```bash
git clone <repo>
cd engraph
cargo build --release                       # default: lexical retrieval only
cargo build --release --features embeddings # opt-in: semantic retrieval (~150MB ONNX runtime + model)
cargo build --release --features otel       # opt-in: OTLP/gRPC metrics export (see docs/opentelemetry.md)

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
      { "matcher": "", "hooks": [{ "type": "command", "command": "engraph hook session-end" }] }
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

Display-sink pipelines (`git log | head`, `git diff | less`) are rewritten by wrapping the producer and keeping the pipe intact, so the window command still sees compressed output. Env-prefixed forms (`FOO=bar git log`) are peeled and re-emitted ahead of `engraph run`. Other compound commands (`cd /tmp && git log`, `;`-sequences, and byte-consuming pipes like `git log | grep x`) pass through unmodified. Quoted args with spaces or shell metacharacters are preserved correctly through `shell-words` quoting.

### How `pre-grep` redirects symbol lookups

After `engraph index .` populates the codegraph, the PreToolUse hook on Grep watches for bareword patterns (`^[A-Za-z_][A-Za-z0-9_]*$`, length ≥ 3) that resolve to **1–3 entities** by `name` or moniker. On a hit, it returns `permissionDecision: "deny"` with a message pointing Claude at `engraph subgraph <symbol>` — typically orders of magnitude smaller than the file-read-and-grep loop Claude would otherwise run.

The gate is deliberately narrow:

| Pattern | Resolves to | Decision |
|---|---|---|
| `processOrder` | 1 entity | deny + suggest `engraph subgraph processOrder` |
| `parse` | 12 entities | passthrough (ambiguous; subgraph would emit a disambiguation block) |
| `unindexed_helper` | 0 entities | passthrough (not in graph) |
| `process.*` | — | passthrough (regex metachar) |
| `id` | — | passthrough (too short) |

The same redirect fires for `rg <symbol>` and `grep <symbol>` invoked via the Bash tool — checked inside `pre-bash` before the compression rewrite, so subgraph wins over `engraph run rg <symbol>` when both apply.

When Claude wants the raw grep anyway (e.g. to search for a literal occurrence in comments), adding a regex metachar like `\b` bypasses the redirect.

### How `post-read` enriches Read results

PostToolUse(Read) appends a brief listing of indexed symbols in the just-read file as `hookSpecificOutput.additionalContext` — name, line range, and signature for up to 30 entities. Often answers "what's in this file" without a follow-up grep or subgraph call. Silent passthrough for files that aren't in the graph; the augment never displaces or rewrites the actual Read output.

---

## Claude Code skill: `engraph-refresh`

A bundled skill that brings engraph's local indexes up to date after a work session. The source lives at [`skills/engraph-refresh/SKILL.md`](skills/engraph-refresh/SKILL.md); the installer offers (opt-in, like the SCIP installer) to copy it to `~/.claude/skills/engraph-refresh/`. Invoke it from Claude Code:

| Invocation | What it does |
|---|---|
| `/engraph-refresh` | Re-embeds new conversation messages (`engraph reindex-embeddings`; incremental + idempotent). This is how semantic recall stays current — transcripts are ingested at `SessionEnd` but not auto-embedded. |
| `/engraph-refresh index` (or `all` / `code` / `scip`) | Additionally rebuilds the code graph for the current repo (`engraph index .`). |

The code-graph step is opt-in: a bare invocation only touches embeddings. The reindex step needs an embeddings build (see [Feature flags](#feature-flags)); on a lean binary the skill reports that and stops rather than failing silently.

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

# Ingest a transcript file (one path, or - for stdin)
engraph ingest ~/.claude/projects/<project>/<session>.jsonl

# Compress existing stored messages
engraph compress-existing

# Token savings report
engraph gain                       # summary + per-feature table
engraph gain --by-filter           # per-command breakdown
engraph gain --daily               # (or --weekly / --monthly / --all)
engraph gain --graph               # ASCII saved-tokens/day chart
engraph gain --all --format json   # machine-readable export (--json still works)

# Per-session token budget (--session-id is required)
engraph budget status --session-id <id>
engraph budget set --session-id <id> --soft 80000 --hard 120000

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

# With OTel export
cargo build --release --features otel
cargo test --features otel
cargo clippy --all-targets --features otel -- -D warnings
```

GitHub Actions (`.github/workflows/ci.yml`) runs `cargo build`, `cargo test`, `cargo clippy`, and `cargo fmt --check` on `ubuntu-latest`, `macos-latest`, and `windows-latest` for every push and pull request. On pushes to `main`, a separate job detects a workspace version bump, pushes the tag, fans out a build matrix across the five release targets, and publishes the archives to a GitHub Release.

### Test coverage

Key test locations:

| What's tested | Location |
|---|---|
| Per-filter compression ratios | `engraph-compress/tests/filter_ratios.rs` |
| Golden snapshot output (git log, cargo check, cargo test) | `engraph-compress/tests/golden_fixtures.rs` |
| Compress idempotency (fixed-point property) | `engraph-compress/src/lib.rs` |
| Pre-bash hook branches (rewrite, deny, passthrough, parser shapes) | `engraph-cli/tests/pre_bash_hook.rs` |
| Pre-grep subgraph redirect gate | `engraph-cli/tests/pre_grep_hook.rs` |
| Post-read augment shape and passthrough on unindexed files | `engraph-cli/tests/post_read_hook.rs` |
| Read-bucket filter (cat/head/tail) per-language strip + windowing | `engraph-compress/tests/read_filter.rs` |
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
| Bazel symbol pass runs with Java delegated (gated behind env var) | `engraph-codegraph/tests/bazel_symbols_live.rs` |

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
| `sessions`, `messages` | Session-state snapshot from JSONL ingestion |
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
| `otel` | Adds OpenTelemetry OTLP/gRPC export of the `events` telemetry. Off at runtime unless `ENGRAPH_OTEL=1`; see [docs/opentelemetry.md](docs/opentelemetry.md). Required when building the binary the Claude Code hooks invoke. |

---

## License

MIT
