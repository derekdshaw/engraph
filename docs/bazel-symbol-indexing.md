# Design: Bazel-resolved symbol indexing

Status: **proposed / not started.** Scoping doc for a later session. Captures the
problem, the per-language approach, integration points in this codebase, risks,
and a phased plan.

Related: `ROADMAP.md` → "Codegraph follow-ups" (the bullet-level precursors);
`crates/engraph-codegraph/src/bazel.rs:8` defers this ("symbol-level half … is
deferred").

---

## Problem & evidence

The target-level Bazel pass (`bazel query //...` → `bazel_target` entities +
`BAZEL_DEPENDS_ON` edges) works. The **symbol-level** pass (`--bazel-symbols`)
produces **zero symbols** on a real Bazel monorepo.

Diagnosed 2026-06-02 against a large Bazel monorepo (`engraph index --workspace`):

- Result: every entity was `kind='bazel_target'` and every relation
  `BAZEL_DEPENDS_ON` — **`0` `symbol` entities, `0` SCIP bytes**.
- Per-language pass outcome (now visible via `symbol_langs`):
  - `java`  → **failed** (`scip-java exited with exit status: 1`)
  - `go`    → **skipped (no targets)**
  - `ts`    → **failed** (`scip-typescript exited with exit status: 1`)
  - `python`→ **skipped (no targets)**
- Rule classes are present in bulk (via `bazel query --output=label_kind`):
  `java_library`, `go_library`, `ts_project` (+ `java_test`, `go_test`, …) all
  appear in quantity. So the probe's rule kinds match — that is **not** the
  problem.

### Root cause

`bazel_symbols.rs` runs the **standalone** SCIP indexers in their native
build-system modes, which do **not** drive Bazel and need root manifests this
monorepo doesn't have:

- `scip-java index` auto-detects only {Maven, Gradle, sbt, mill}
  (`driver.rs:115-118`); no root `pom.xml`/`build.gradle` → exit 1.
- `scip-go --module-root .` requires a root `go.mod`; absent → the guard at
  `bazel_symbols.rs:189` reports `SkippedNoTargets`.
- `scip-typescript index` needs a populated `node_modules` / tsconfig project;
  only a root `package.json` exists → exit 1.
- `scip-python` needs importable modules; no `py_library` targets surfaced.

> **Correct a stale comment while here:** the `bazel_symbols.rs` module doc
> (≈ lines 13-17) claims "scip-java auto-detects Bazel, materializes its own
> aspect". That is **false** (contradicts `driver.rs` and the empirical exit 1).
> Fix it as part of this work.

So the pass silently degrades to target-level. Getting real symbols requires
driving each indexer through **Bazel-resolved sources/classpaths** — a
per-language project, not a tweak.

---

## Goal / success criteria

- `entities.kind='symbol'` rows and non-`BAZEL_DEPENDS_ON` relations
  (`CALLS`/`REFERENCES`/`IMPORTS`) for the monorepo's Java / Go / TS (Python a
  stretch), loaded through the existing `scip_loader::load`.
- Verifiable: `SELECT kind, COUNT(*) FROM entities` shows `symbol`; a known
  function resolves via `engraph subgraph`.
- Bounded blast radius: opt-in, **target-subsettable**, hermetic `--output_base`,
  no churn of the user's `~/.cache/bazel`, no permanent edits to the target repo
  (or clearly documented if unavoidable).

---

## Per-language strategy

### Go — cheapest, do first
- Enumerate Go modules (find `go.mod`, or derive dirs from `go_library` targets —
  the target-level pass already has `BazelTarget.location` → BUILD dir,
  `bazel.rs:80-91`). Run `scip-go --module-root <dir>` per module; merge SCIP.
- Needs a Go toolchain + resolvable module graph (`GOFLAGS`/`GOPATH`, possibly
  `bazel run @rules_go//go -- mod download`).
- Replaces the single-root `go.mod` guard (`bazel_symbols.rs:189`). Closes the
  ROADMAP "scip-go multi-`go.mod`" item.

### TypeScript — medium
- Materialize `node_modules` first, either via the repo's package manager (root
  `package.json` is present — likely a pnpm/yarn workspace under
  `aspect_rules_js`) or a `bazel build` of the ts targets to populate
  `bazel-bin/<pkg>/node_modules` symlinks.
- Then run `scip-typescript` — it natively supports **yarn/pnpm workspaces**
  (`--yarn-workspaces` / `--pnpm-workspaces`), which may be simpler than
  per-`ts_project` invocation. Merge SCIP.
- Closes the ROADMAP "rules_ts node_modules" item.

### Java — the heavy one, do last
- Mechanism (per scip-java docs): compile with the **SemanticDB javac plugin**
  under Bazel, then generate SCIP from the emitted SemanticDB:
  `bazel build //... --@scip_java//semanticdb-javac:enabled=true`, then the
  SemanticDB→SCIP step.
- Implications:
  - **Repo-side Bazel config**: `scip_java` must be a Bazel dep
    (`MODULE.bazel`/`WORKSPACE` + flag wiring). engraph can't assume the monorepo
    has it. Options — (a) require the repo to add it (document); (b) engraph
    injects an overlay (`.bazelrc` + a generated repo); (c) an `--aspects`-based
    variant bundled by engraph that needs no `WORKSPACE` edit. **Biggest open
    question — verify what the installed scip-java version supports.**
  - **Cost**: a full compile of every `java_library` target — minutes to tens
    of minutes, GBs of `bazel-out`. **Must** be subsettable
    (`ENGRAPH_BAZEL_SCIP_JAVA_TARGETS` is already reserved, ROADMAP line 67) and
    incremental.
  - **output_base isolation**: if **engraph drives `bazel build` directly** (vs.
    delegating to `scip-java index`), we control `--output_base` and avoid the
    "scip-java touches `~/.cache/bazel`" problem (ROADMAP line 68). Argues for
    engraph orchestrating the build itself.

### Python — stretch, maybe defer
- `scip-python` only emits for **importable modules** and needs a configured
  environment (verified 2026-06-02; also why we pin `--project-version`, see
  `driver::SCIP_PYTHON_VERSION`). For `rules_python`, resolve a per-target/per-
  project venv or site-packages, then index per project. Likely the hardest to
  make useful; reasonable to leave out of v1.

---

## Architecture (this codebase)

- Refactor `bazel_symbols.rs`: replace the single `build_indexer_command` +
  standalone-mode invocation with per-language **Bazel strategies** (consider a
  `bazel_scip/` module, one file per language).
- Reuse:
  - `bazel.rs`: `bazel_binary()`, `output_base_for()` (isolation), `tail_lines()`,
    and the parsed `BazelTarget` list (rule_class + location) so the symbol pass
    can enumerate targets/dirs **without a second `bazel query`** — consider
    handing the target-level pass's parsed targets to the symbol pass.
  - `scip_loader::load` + `bazel_symbols::merge_scip_bytes` — unchanged; they
    already merge N language SCIP streams and load once.
- New: an engraph-driven `bazel build` helper with `--output_base` threaded
  (for Java, and TS materialization).
- Reporting: `IndexStats.symbol_langs` already surfaces per-language status
  (shipped). Extend `LangStatus` for the new phases (e.g. a "building" note,
  richer failure reasons). Keep per-language failure isolation — a failed Java
  build must not sink Go/TS.

---

## Risks & open questions

1. **Java build cost** dominates everything. Without target subsetting + caching
   this is unusable on a 20 GB monorepo. Design subsetting in from the start.
2. **scip-java repo-side config** — can we drive it without editing the target
   repo (overlay or `--aspects`)? If not, this becomes "document a setup the
   user runs once," not "engraph does it automatically." **Resolve before
   committing to the Java phase.**
3. **Toolchain downloads** (JDK/Go/Node) on first run — minutes, GBs.
4. **Hermeticity** — keep `--output_base` isolated; never churn `~/.cache/bazel`.
5. **Moniker / entity-ID stability** across runs (monikers embed versions;
   already pinned for Python — confirm Java/Go/TS are stable).
6. **Verify upstream incantations** against the installed indexer versions
   (`scip-java` ~0.x, `scip-go`, `scip-typescript` 0.6.x) — the flags above are
   the documented direction, not version-locked.

---

## Phasing

- **Phase A — Go.** No compile; per-module enumeration + merge. Validates the
  multi-strategy refactor and the per-language status plumbing on real data.
- **Phase B — TypeScript.** node_modules materialization + workspace-mode index.
- **Phase C — Java.** Bazel SemanticDB build + SCIP gen; target subsetting;
  output_base threading; resolve the config-injection question first.
- **Phase D — Python (stretch).** Per-project env + scip-python.

Each phase: stays behind `--bazel-symbols`, reports per-language status, and is
verified on a real Bazel monorepo (start with a small `//some/subtree/...`
subset) by checking `entities.kind='symbol'` and `CALLS`/`REFERENCES` rows.

---

## Verification (per phase)

- Subset run: `engraph index --bazel-symbols <repo>` limited to a small target
  expression; confirm `SELECT COUNT(*) FROM entities WHERE kind='symbol'` > 0 and
  a known function appears in `engraph subgraph`.
- `scip_bytes` > 0 in the run summary; the relevant `symbol[<lang>]` line reads
  `indexed`.
- `~/Library/Caches/engraph/bazel-out/<hash>` grows; the user's `~/.cache/bazel`
  is untouched.
- Where feasible, a tiny hermetic Bazel fixture as an integration test (Java
  likely manual-only given the plugin wiring).

---

## References

- Code: `crates/engraph-codegraph/src/bazel_symbols.rs`,
  `…/bazel.rs`, `…/scip_loader.rs`, `…/driver.rs`.
- Upstream (verify against installed versions):
  [scip-java getting started](https://sourcegraph.github.io/scip-java/docs/getting-started.html)
  (SemanticDB plugin + `bazel build //... --@scip_java//semanticdb-javac:enabled=true`),
  [scip-typescript](https://github.com/sourcegraph/scip-typescript) (yarn/pnpm
  workspaces).
- Session evidence: 2026-06-02 diagnosis (this doc, "Problem & evidence").
