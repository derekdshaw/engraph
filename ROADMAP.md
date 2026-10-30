# Engraph Roadmap

Forward-looking only: features and follow-ups that aren't built yet. Anything
already shipped lives in `DETAILS.md` under its feature section, not here.

Items are ordered by expected impact, most to least: first on tokens saved,
then on how much of a codebase engraph can show correctly. Rank is not build
order. Some items depend on others (see **Depends on** in each item and
[Build sequence](#build-sequence)).

## Working rules

- Do each item on its own branch. Make one commit for each step that changes code.
- For each step that changes code: write a test that shows the problem or the
  new behavior, and confirm it fails; change the code; run
  `cargo test --workspace` and `cargo clippy --workspace --all-targets`, both
  with and without `--features embeddings`. If the change affects how engraph
  behaves, update `DETAILS.md`, `README.md`, and this file.
- Steps that start with "Find out": record the result in the pull request, then
  continue with the steps that follow.

## Build sequence

1. Start Phase 1 of [Pre-read file summary](#1-pre-read-file-summary) now. Its
   two-week measurement window can run alongside everything else.
2. [Stable symbol ids](#6-stable-symbol-ids): the stability suite before
   normalization. Normalization changes ids, and the suite shows whether that
   breaks anything.
3. [Deeper workspace discovery](#3-deeper-workspace-discovery) before
   [Go module coverage](#4-go-module-coverage), which uses its member rules.
4. Everything else is independent.

---

## 1. Pre-read file summary

**What:** before Claude reads a file engraph has indexed, a PreToolUse(Read)
hook injects a short summary: symbols with line ranges and signatures, open bugs
that name the file, and the date of the last commit that changed it. With it,
Claude can read only the part of the file it needs. The hook never blocks the
read: a hook that withholds the file and gives only the summary can make Claude
act on incomplete data.

**Why deferred:** PreToolUse(Read) fires on *every* file read, including small
ones. Injecting 500 tokens before each Read can dwarf the savings on
small files. Worth building only when telemetry shows file re-reads are a top-3
token sink. The codegraph already supplies the anatomy cheaply via the
`entities` table, and the PostToolUse(Read) symbol-augment hook is a shipped,
lower-risk cousin. This is the more aggressive pre-read variant.

**Phase 1: measure (start now)**

1. In the PostToolUse(Read) hook (`run_post_read_hook`), record a telemetry
   event for each read, with the read's token count and a hash of the file path.
2. Add `engraph gain --reads`: tokens of all reads, and tokens of reads of a
   file the same session already read.
3. Collect for two weeks. Over the same window, run the original success
   measurement: ≥ 20% aggregate token savings across real Claude Code use,
   measured via `engraph gain`.
4. If re-reads are not a top-3 token source, stop here.
5. Find out whether Claude Code accepts `additionalContext` from a PreToolUse
   hook. If it does not, stop here.

**Phase 2: build**

1. Add a PreToolUse(Read) hook to `engraph hook`. Claude sends absolute paths;
   map them with `divergence::indexed_location` to the project and the
   repo-relative path.
2. Give a summary only when the codegraph has the file, the file is unchanged
   since the index commit, and the file has more lines than
   `ENGRAPH_PREREAD_MIN_LINES` (default 200).
3. Cap the summary at 300 tokens.
4. When the pre-read hook gave a summary for a read, the PostToolUse(Read) hook
   skips its symbol list for that read.
5. Record a telemetry event for each summary. Compare read tokens for two weeks
   before and two weeks after.

**Tests:** a summary for an indexed file over the line limit; no summary for a
small file; no summary for a file changed since the index commit; summary ≤ 300
tokens; no post-read symbol list after a read that got a summary.

**Risk:** the hook fires on every read. If summaries don't make Claude read
less, they only add tokens. The before/after comparison in Phase 2 is the check.

**Depends on:** nothing. Phase 2 is gated on Phase 1's results.

**Effort:** 1–2 days for Phase 2.

---

## 2. Auto-trigger indexing

**What:** today `engraph index` is manual. Wire it into the SessionStart hook (or
an MCP tool surface) so a fresh session re-indexes the workspace automatically,
with stale-detection so cold builds don't fire every session.

**Why it ranks here:** the grep redirect, the post-read augment, and `subgraph`
all depend on a current graph. A missing or stale graph turns their savings off
without any error, and today keeping it fresh is on the user. This is the most
user-visible codegraph follow-up.

**Depends on:** nothing.

---

## 3. Deeper workspace discovery

**What:** let the user choose which projects `engraph index --recursive` finds,
and make discovery obey `.gitignore`.

**Today:** `walk_recursive` descends to `RECURSIVE_MAX_DEPTH` (16) and skips the
fixed `is_pruned_dir` list (`node_modules`, `target`, `vendor`, …). It reads no
`.gitignore` files, and there's no way to include or exclude directories. A
directory below another directory of the same language is treated as a workspace
member. That is right for Cargo workspaces, and possibly wrong for Go modules,
TypeScript packages, and Python projects.

**Approach:**

1. Walk with the `ignore` crate so `.gitignore` and `.ignore` apply. Keep the
   `is_pruned_dir` list as well.
2. Add `--max-depth` (default 16), and `--include <glob>` / `--exclude <glob>`
   (relative to the workspace root).
3. In `--dry-run` output, list each directory discovery found and why it was
   included or skipped.
4. Find out whether a git repo nested inside another git repo gets its own
   project key. Add a test for it.
5. Replace the same-language member rule with one rule per language:
   - Cargo: a member only if the root `Cargo.toml` has a `[workspace]` section.
   - TypeScript: a member only if the root `package.json` lists it in
     `workspaces`.
   - Go and Python: a directory with its own manifest is never a member.

**Tests:** a directory named in `.gitignore` is skipped; `--exclude` removes a
directory and `--include` limits results to the directories it names;
`--max-depth 2` skips a project at depth 3; a Cargo crate below a root
`[workspace]` is a member, and one below a root crate without `[workspace]` is
not; a Go module below a root Go module is not a member.

**Risk:** the new member rules can increase the number of index runs in a
workspace. Check with `--dry-run` before a full run.

**Depends on:** nothing. [Go module coverage](#4-go-module-coverage) uses its
member rules.

---

## 4. Go module coverage

**What:** index every Go module of a repo, with correct relations between
modules, both natively and inside Bazel.

**Today:**

- **Native:** the Go driver detects a module only when `go.mod` is in the
  indexed directory. A multi-module repo needs `--recursive`, and that may skip
  a module nested under another Go module (see
  [Deeper workspace discovery](#3-deeper-workspace-discovery)). `go.work` files
  are not read.
- **Bazel:** the native pass (`run_go_modules`) reaches only `go.mod`-rooted
  modules. On gazelle-managed `rules_go` monorepos that's the *minority* of the
  code: a large gazelle repo can have thousands of `go_library` targets but only
  a handful of `go.mod` files. `LangStatus::IndexedModules` surfaces the gap
  (`indexed N go.mod modules of M go targets`). The shipped
  `ENGRAPH_BAZEL_SCIP_GO_CMD` delegate lets a repo drive scip-go through its own
  Bazel glue (`docs/examples/scip-go-bazel-index.sh`).
- **Both:** scip-go indexes one build configuration, so platform-gated files
  (`_linux.go`, `//go:build` tags) are missed. `_test.go` files are not indexed
  (the Bazel pass passes `--skip-tests`).

**Approach:**

1. **Nested modules.** Write a test with a root Go module and a second module in
   `tools/`. Check that `engraph index --recursive --dry-run` includes `tools/`.
   If it doesn't, apply the Go member rule from
   [Deeper workspace discovery](#3-deeper-workspace-discovery).
2. **`go.work`.** Find out whether scip-go can index all modules of a `go.work`
   in one run. If it can, give it the `go.work` directory. If not, index each
   module of the `go.work` as a subtree of the repo.
3. **Cross-module resolution.** Write a test where a function in module A calls
   a function in module B. Check that the relation points at module B's symbol
   and doesn't dangle, in the native pass and in the merged per-module Bazel
   SCIP. Check that vendored copies don't double-count.
4. **Build tags.** Find out whether scip-go accepts build tags or `GOOS` /
   `GOARCH`. If it does, add `--go-tags` to `engraph index`. Either way, document
   the one-configuration limit in `DETAILS.md`.
5. **Test files.** Add `--go-tests` to index `_test.go` files (test → impl
   edges), in both the native and the Bazel Go pass.
6. **Synthesize (Bazel; alternative to delegation).** Instead of a repo-supplied
   command, derive an ephemeral `go.mod` per gazelle prefix from `go_library`
   importpaths, run scip-go per synthesized root, and clean up. Fully
   in-engraph, but fiddly. It can't resolve hermetic Bazel deps without extra
   glue, and it must not mutate the target repo.

**Tests:** `engraph index --recursive` indexes the root module and the module in
`tools/`; a relation from module A to module B points at the right symbol; with
`--go-tests` a function in a `_test.go` file is in the graph, and without it, it
is not.

**Risk:** one index run per module can take a long time in a large repo. Scope
it with `--include` from [Deeper workspace discovery](#3-deeper-workspace-discovery).

**Depends on:** [Deeper workspace discovery](#3-deeper-workspace-discovery) (the
Go member rule).

---

## 5. Bazel TypeScript and Python symbols

**What:** symbol-level Bazel indexing is shipped for Go (Phase A) and Java
(Phase C) (`docs/bazel-symbol-indexing.md`, `DETAILS.md` §14.9). Two languages
remain:

- **scip-typescript + rules_ts node_modules (Phase B):** cold runs may fail
  until a prior `bazel build //...` populates `bazel-bin/<pkg>/node_modules`
  symlinks. Likely delegated the same way Java is (an
  `ENGRAPH_BAZEL_SCIP_TS_CMD` hook), since `rules_ts` / pnpm-workspace
  materialization is repo-specific.
- **scip-python in rules_python (Phase D):** per-project venv / site-packages
  resolution. The hardest to make useful; reasonable to leave for last.

**Why deferred:** until a real workload trips them.

**Depends on:** nothing.

---

## 6. Stable symbol ids

**What:** a symbol id is the same on every machine, and two different symbols
never share one. A regression suite shows when a change to engraph or to an
indexer changes the ids or relations of known symbols.

**Why it ranks here:** ids from canonically published deps (Cargo, npm, PyPI, Go
modules) are already machine-stable, and cross-repo stitching works without
rewrites. The known problems are narrow:

- scip-java gives first-party Java code an empty version
  (`semanticdb maven . . …`). Two repos with the same package names can
  therefore produce the same ids.
- scip-go before 0.7 put absolute machine paths in ids.

`scip_loader::normalize_moniker` is the hook for fixes, and it is a no-op today.
Both parts pay back once indexer drift or id collisions hit a real workload.

**Part 1: stability suite (do first)**

1. Make a small fixture repo for each of Rust, Python, Go, TypeScript, and Java,
   with pinned dependency versions.
2. In each fixture, pick 50 symbols: functions, types, and methods, including
   symbols used from a different module.
3. Write a snapshot file per fixture with each symbol's id, name, file path,
   line range, and caller/callee ids, plus the indexer version.
4. Add a test that indexes each fixture and diffs against its snapshot. It
   soft-skips when the indexer is absent (as `tests/drivers_live.rs` does). On a
   mismatch it shows the differences, and if the indexer version differs it
   names both versions. `ENGRAPH_UPDATE_SNAPSHOTS=1` rewrites the snapshots.
5. Commit a prebuilt SCIP file per fixture. Add a second test that loads it and
   diffs against the snapshot. It needs no indexer, so it runs in every CI job
   and catches loader changes.
6. Run the first test in the "Codegraph live drivers" CI job, with pinned
   indexer versions installed.

**Part 2: normalization**

1. Find out which id parts each indexer emits (rust-analyzer, scip-python,
   scip-go, scip-typescript, scip-java): scheme, package manager, package name,
   version.
2. Tabulate problems per scheme: empty versions, absolute paths, versions that
   change on every commit. Leave scip-python's pinned project version as is.
3. Write one rule per problem. Put them in `normalize_moniker`, one `match` arm
   per scheme:
   - First-party Java with an empty version: fill the version from the repo key.
   - scip-go ids with an absolute path: strip the machine-local part.
4. Make sure every function that writes a symbol id to the database goes
   through `normalize_moniker`.
5. Add a `normalization` column to `index_state` recording the rules version.
   When it differs from the binary's rules version, print a message
   recommending a full re-index.

**Tests:** a before/after unit test per rule; two Java repos with the same
package names get different ids; `tests/workspace_cross_repo.rs` is unchanged;
the Part 1 suite shows changed ids only for schemes that have a rule.

**Risks:** a new indexer version can change ids legitimately and fail the suite.
Regenerate snapshots only after reviewing the diff. Changing normalization rules
disconnects cross-repo relations indexed under different rule versions, so after
a rules update, re-index every repo.

**Depends on:** Part 2 depends on Part 1.

---

## 7. MCP server

**What:** expose `engraph recall`, `engraph subgraph`, and related queries as MCP tools. Lets Claude orchestrate retrieval explicitly via tool calls rather than reading CLI output.

**Why deferred:** with ToolSearch enabled (the user's setup), MCP unused-cost is ~50 tk/tool name — low but non-zero. The Bash-CLI path (`engraph recall <q>` → output into transcript) already covers current use cases. MCP wins when there are many tools that benefit from structured args + result types; now that the codegraph is shipped, each graph operation (`find_symbol`, `who_calls`, `subgraph_around`) is a candidate tool, so the 5+-tool threshold is within reach.

**Effort:** 3–4 days. The MCP SDK + a stdio server crate are the bulk of it.

---

## 8. LLM reranking layer

**What:** top-K from RRF hybrid retrieval → small-model rerank for final ordering. Defaults to off because it costs tokens.

**Why deferred:** until hybrid retrieval (shipped) shows a measurable ceiling on a real query benchmark, this is theoretical. Need telemetry showing a p@5 plateau before justifying the per-query model spend.

**Dependencies:** a real query benchmark dataset (nothing close exists yet).

**Effort:** 2–3 days, mostly prompt design + token-cost measurement.

---

## 9. `/resume` and `/save` skills

**What:** the writer (`engraph save`), the SessionStart `## decisions` and `## last session` surfacing, and `engraph resume` (prints the last session's digest) now ship. What remains is the optional skill UX: a `/save` shortcut and a `/resume` skill wrapping `engraph resume`.

**Why deferred:** the SessionStart brief already auto-surfaces saved decisions and the last session's final turn, and `engraph resume` is one Bash call away.

**Effort:** ~0.5 day (skill markdown).

---

## 10. Mnemosyne → engraph data migration

**What:** an operational item, not engineering. Both DBs are SQLite with similar schemas. A one-shot SQL script could copy sessions, messages, do-not-repeat, and bugs over.

**Effort:** a day with the SQL + a sanity-check pass.

---

## 11. Cross-project wikilink / Obsidian surface

**What:** export `context_items` / `bugs` / `decisions` to an Obsidian-compatible vault structure with wikilinks. Useful for human browsability across projects; orthogonal to token savings.

**Why deferred:** not a token-savings feature. Only build if the user wants the vault for personal note-taking.

**Effort:** 2–3 days. Conceptually simple; friction is in matching Obsidian's conventions.

---

## Future research (not on the roadmap, but worth noting)

- **Adaptive compression based on budget escalation level.** When `engraph budget status` shows level 2 or 3, filters could apply stricter caps (e.g. `tree` depth drops from 3 to 2, `docker logs` tail from 200 to 100). Telemetry-driven escalation is the bigger story here that hasn't been fleshed out.
- **Mempalace-style eviction policy.** Memory tools that only accrete become liabilities. Need time-decay scoring with a hard floor, demote-after-N-non-retrievals, size budget per project. The schema supports it (`scopes.archived`); the algorithm doesn't exist yet.
- **Cloud sync.** Local-first is the v1 promise. If you ever want to share memory across machines, the trait boundary in place today (`EmbeddingProvider`) + UUIDv7 IDs + the `ingestion_log` append-only pattern are the foundation. Need to add `MemoryStore` and `GraphStore` traits, then a Turso/LiteFS implementation. The architecture review explicitly argued against adding those traits before there's a concrete cloud need; revisit when there is.
- **A "what did Claude actually do" weekly digest.** An `engraph weekly` subcommand that reads `events` + `messages` and produces a summary of: top compressed commands, top recalled phrases, longest sessions, biggest savings. Useful for both the user and for tuning filter targets.
