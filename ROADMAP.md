# Engraph Roadmap

Forward-looking only: features and follow-ups that aren't built yet, ordered
roughly by token-savings impact. Anything already shipped lives in `DETAILS.md`
under its feature section, not here.

---

## Pre-read file-anatomy injection

**What:** before Claude reads a file, the PreToolUse(Read) hook injects a one-paragraph summary if the file is already known — top symbols, last-known signature shape, recent bugs touching it, last-edit timestamp. Claude may skip the re-read entirely.

**Why deferred:** PreToolUse(Read) fires on *every* file read, including small ones. Injecting 500 tokens before each Read can dwarf the savings on small files. Worth building only when telemetry shows file re-reads are a top-3 token sink. The codegraph already supplies the anatomy cheaply via the `entities` table, and the PostToolUse(Read) symbol-augment hook is a shipped, lower-risk cousin — this is the more aggressive pre-read variant.

**Effort:** 1–2 days.

---

## LLM reranking layer

**What:** top-K from RRF hybrid retrieval → small-model rerank for final ordering. Defaults to off because it costs tokens.

**Why deferred:** until hybrid retrieval (shipped) shows a measurable ceiling on a real query benchmark, this is theoretical. Need telemetry showing a p@5 plateau before justifying the per-query model spend.

**Dependencies:** a real query benchmark dataset (nothing close exists yet).

**Effort:** 2–3 days, mostly prompt design + token-cost measurement.

---

## MCP server

**What:** expose `engraph recall`, `engraph subgraph`, and related queries as MCP tools. Lets Claude orchestrate retrieval explicitly via tool calls rather than reading CLI output.

**Why deferred:** with ToolSearch enabled (the user's setup), MCP unused-cost is ~50 tk/tool name — low but non-zero. The Bash-CLI path (`engraph recall <q>` → output into transcript) already covers current use cases. MCP wins when there are many tools that benefit from structured args + result types; now that the codegraph is shipped, each graph operation (`find_symbol`, `who_calls`, `subgraph_around`) is a candidate tool, so the 5+-tool threshold is within reach.

**Effort:** 3–4 days. The MCP SDK + a stdio server crate are the bulk of it.

---

## `/resume` and `/save` skills

**What:** the writer (`engraph save`) and the SessionStart `## decisions` surfacing now ship (alongside `engraph remember` and `engraph bug`). What remains is the optional skill UX: a `/save` shortcut and a `/resume` (thin `engraph resume`) that queries the latest decisions and pretty-prints them mid-session.

**Why deferred:** the SessionStart brief already auto-surfaces saved decisions, covering ~80% of this. `/resume` matters when you want to pull them up explicitly mid-session.

**Effort:** ~0.5 day (skill markdown + a thin `engraph resume` reader).

---

## Cross-project wikilink / Obsidian surface

**What:** export `context_items` / `bugs` / `decisions` to an Obsidian-compatible vault structure with wikilinks. Useful for human browsability across projects; orthogonal to token savings.

**Why deferred:** not a token-savings feature. Only build if the user wants the vault for personal note-taking.

**Effort:** 2–3 days. Conceptually simple; friction is in matching Obsidian's conventions.

---

## Codegraph follow-ups

The codegraph (single-repo, cross-repo, and Bazel polyglot) is shipped. These robustness items were documented at ship time and deferred until a real workload trips them:

- **scip-go multi-`go.mod` monorepos:** the symbol-level path requires a single `go.mod` at the workspace root. Gazelle-managed Bazel-go repos sometimes carry one per package; would need enumeration + merging.
- **scip-typescript + rules_ts node_modules:** cold runs may fail until a prior `bazel build //...` populates `bazel-bin/<pkg>/node_modules` symlinks.
- **scip-java on large monorepos:** 1000+ Java targets can OOM or exceed 30 min. Future `--targets <expr>` pass-through (reserved env var `ENGRAPH_BAZEL_SCIP_JAVA_TARGETS`).
- **Bazel server isolation (Java path):** the target-level pass pins Bazel's `--output_base` into engraph's cache, but scip-java invokes Bazel internally with no startup-option pass-through to plumb, so its build lands in the user's default `~/.cache/bazel`. Verify scip-java exposes a `--bazel-startup-options` flag (or similar) and thread engraph's output_base through it.
- **Per-driver moniker normalization rules:** the rewrite hook in `scip_loader::normalize_moniker` is a no-op today.
- **50-symbol stability test suite:** snapshot known monikers across a polyrepo and regress on every loader change. Pays back once indexer-version drift causes a real failure.

- **Auto-trigger indexing.** Today `engraph index` is manual. Wiring it into the SessionStart hook (or an MCP tool surface) so a fresh session re-indexes the workspace automatically — with stale-detection so cold scip-java doesn't fire every session — is the most user-visible follow-up.
- **Deep workspace discovery.** `discover_workspace_repos` walks immediate children only; nested polyrepo layouts need explicit per-repo invocations. Recursion + a depth-limit / `.gitignore` respect would close this.

---

## Operational items (not engineering)

- **Mnemosyne → engraph data migration script.** Both DBs are SQLite with similar schemas. A one-shot SQL script could copy sessions, messages, do-not-repeat, and bugs over. Effort: a day with the SQL + a sanity-check pass.
- **2-week real-world measurement.** The original success criteria called for ≥ 20% aggregate token savings across two weeks of actual Claude Code use, measured via `engraph gain`. Run it now that `scripts/install.sh` wires the hooks.
- **CI.** No GitHub Actions / equivalent yet. Worth setting up: `cargo test`, `cargo clippy --all-targets -- -D warnings`, and the `--features embeddings` variant of both, on every push.
- **Release binaries.** No `cargo dist` / GitHub Releases yet. Once the hooks have run in production for two weeks and are stable, ship a release with prebuilt binaries.

---

## Future research (not on the roadmap, but worth noting)

- **Adaptive compression based on budget escalation level.** When `engraph budget status` shows level 2 or 3, filters could apply stricter caps (e.g. `tree` depth drops from 3 to 2, `docker logs` tail from 200 to 100). Telemetry-driven escalation is the bigger story here that hasn't been fleshed out.
- **Mempalace-style eviction policy.** Memory tools that only accrete become liabilities. Need time-decay scoring with a hard floor, demote-after-N-non-retrievals, size budget per project. The schema supports it (`scopes.archived`); the algorithm doesn't exist yet.
- **Cloud sync.** Local-first is the v1 promise. If you ever want to share memory across machines, the trait boundary in place today (`EmbeddingProvider`) + UUIDv7 IDs + the `ingestion_log` append-only pattern are the foundation. Need to add `MemoryStore` and `GraphStore` traits, then a Turso/LiteFS implementation. The architecture review explicitly argued against adding those traits before there's a concrete cloud need; revisit when there is.
- **A "what did Claude actually do" weekly digest.** An `engraph weekly` subcommand that reads `events` + `messages` and produces a summary of: top compressed commands, top recalled phrases, longest sessions, biggest savings. Useful for both the user and for tuning filter targets.
