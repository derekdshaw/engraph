# Plan 06: Bazel symbols for TypeScript, JavaScript, Python, and Rust

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

The symbol-level Bazel pass (`--bazel-symbols`) indexes TypeScript,
JavaScript, Python, and Rust code. When it cannot index a language, it reports
the target count and the command that would index them, in the same way for
each language.

## Condition at this time (2026-11-11)

The symbol-level Bazel pass indexes Go (phase A) and Java (phase C). Refer to
`docs/bazel-symbol-indexing.md` and `DETAILS.md` §14.9. For each language, the
pass first counts the targets, and then reports one of these results:
`SkippedNoTargets`, `SkippedNoIndexer { binary }`,
`SkippedNotConfigured { targets, env }`, `Failed(text)`, or `Indexed`. The
conditions below are from tests on 2026-11-11 with the debug binary of the
branch `plan01-part9-placeholder-upsert`.

### TypeScript and JavaScript (phase B)

The `ts` entry of `LANGS` (`crates/engraph-codegraph/src/bazel_symbols.rs`)
counts the rule kinds `ts_project` and `ts_library`. It does not count the
JavaScript rules of `aspect_rules_js`. The test project `frontend` of
`bazelbuild/examples` (commit `efab5bd`, the source of
`frontend-typescript-bazel` in `engraph-test-repos`) has these rule kinds in
its `BUILD` files:

| Rule kind | Count | Counted by engraph |
|---|---|---|
| `js_library` | 15 | no |
| `ts_project` | 8 | yes |
| `js_test` | 4 | no |
| `js_run_binary` | 4 | no |
| `npm_package` | 2 | no |

The project has 27 `.ts`/`.tsx` files and 14 `.js`/`.jsx` files. Thus:

- A repo with only `js_library` targets gets
  `symbol[ts]: skipped (no targets in this workspace)`. The report is
  incorrect.
- A repo with both gets a count of only the TypeScript targets.

scip-typescript 0.6.0 indexes JavaScript also. Its `--infer-tsconfig` option
makes a `tsconfig.json` when a project has none, and `--pnpm-workspaces` and
`--yarn-workspaces` index all the workspaces of a monorepo.

When targets exist, `run_language` runs `scip-typescript index` at the
workspace root. On the test project this gives
`symbol[ts]: failed: scip-typescript exited with exit status: 1`. The root
has `package.json`, `pnpm-lock.yaml`, `pnpm-workspace.yaml`, and
`MODULE.bazel`, but no `node_modules`: `rules_js` makes them in
`bazel-bin/<pkg>/node_modules` during a build. No command from the repo
(`ENGRAPH_BAZEL_SCIP_TS_CMD`) exists. Go reports `SkippedNotConfigured` before
it runs an indexer when it finds no `go.mod`; TypeScript runs the indexer and
reports `Failed`.

### Python (phase D)

The `python` entry counts `py_library`, `py_binary`, and `py_test`, and runs
`scip-python index --cwd <repo>` at the workspace root. On
`ray-project/ray` (Bazel 7.5.0, `WORKSPACE`, 5,048 `.py` files) the count
is correct:

| Rule kind | Count |
|---|---|
| `py_test` | 2,098 |
| `py_library` | 85 |
| `py_binary` | 43 |

scip-python 0.6.8 on Node v24.22.1 then stops after approximately 4,700 of
5,122 files with
`FATAL ERROR: CALL_AND_RETRY_LAST Allocation failed - JavaScript heap out of memory`
(the default heap of Node, approximately 4 GB, on a computer with 125 GB).
engraph shows `symbol[python]: failed: scip-python exited with signal: 6 (SIGABRT)`.
The reason is in the stderr tail that the status carries, but the printer
shows only the first line (bug 11 of plan 01). The target-level pass and the
Java and Go reports of the same run are correct:
`symbol[java]: skipped (12 targets, set ENGRAPH_BAZEL_SCIP_JAVA_CMD to index them)`.

scip-python also warns that the root `pyproject.toml` has no `[tool.pyright]`
section. The condition from the diagnosis of 2026-06-02 is still expected:
for `rules_python`, scip-python must find the virtual environment and the
`site-packages` of each project.

### Rust (phase E)

`LANGS` has no Rust entry. A `rules_rust` workspace gets four `symbol[…]`
lines, and none is Rust. The user cannot see that engraph did not consider
Rust. On `cameron-martin/bazel-lsp` (Bazel 9.3.2, `MODULE.bazel`,
`Cargo.toml` and `Cargo.lock` at the root):

| Command | Result |
|---|---|
| `engraph index --bazel-symbols .` | 11 `bazel_target` entities, 7 `BAZEL_DEPENDS_ON` relations, no Rust symbol, 11 s |
| `engraph index --lang rust-analyzer .` | 715 `symbol` entities, 997 `CALLS`, 1,126 `REFERENCES`, 47 s |

The rule classes that `bazel query 'kind("rust_.*", //...)'` reports in this
workspace: `rust_binary`, `rust_test`, `rust_library_group`,
`rust_prost_library`, `rust_prost_toolchain`. The repo has no top-level
`rust_library` target: its libraries come from the prost and `library_group`
rules.

Rust has the same shape as Go. `Cargo.toml` is the root that the native
indexer needs, as `go.mod` is for scip-go. rust-analyzer is the standalone
driver of engraph. When the root has no `Cargo.toml`, `rules_rust` can write a
`rust-project.json` (`@rules_rust//tools/rust_analyzer:gen_rust_project`),
which rust-analyzer accepts in place of Cargo. This is not tested.

## Steps

Do the phases in this sequence: E (Rust), B (TypeScript and JavaScript),
D (Python). Phase E is the smallest, because it copies the Go path. Phase D is
the most difficult.

### Phase E: Rust

Status (2026-11-18): steps 1 to 5 and the validation are complete. Validation
results, on `bazel-lsp` commit `c8e653e` (step 1 of the validation):
`symbol[rust]: indexed`, 714 symbols with `.rs` paths (975 symbol rows with
external symbols), `CALLS` 997 and `REFERENCES` 1126 — the same counts as the
standalone run. Step 2: `symbol[rust]: skipped (6 targets, set
ENGRAPH_BAZEL_SCIP_RUST_CMD to index them)`. Step 3:
`symbol[rust]: skipped (no targets in this workspace)`. The symbol load also
deleted 3 of the 11 `bazel_target` entities (bug 23 of plan 01, not a phase E
defect).

1. Write a test for `LANGS`: it has an entry with the language `rust`, the
   binary `rust-analyzer`, and rule kinds that include `rust_library`,
   `rust_binary`, `rust_test`, `rust_proc_macro`, `rust_library_group`, and
   `rust_prost_library`. Do not use the pattern `rust_.*`: it also counts
   toolchain rules.
2. Add the entry.
3. Write a test for `run_rust` with a fake `bazel` (`fake_bazel` in the tests
   of `bazel_symbols.rs`): with targets and no `Cargo.toml` at the root, the
   result is `SkippedNotConfigured { targets, env: "ENGRAPH_BAZEL_SCIP_RUST_CMD" }`.
4. Add `run_rust`, with the structure of `run_go`: if
   `ENGRAPH_BAZEL_SCIP_RUST_CMD` is set, `run_delegated`; if the root has a
   `Cargo.toml`, run the rust-analyzer driver as the standalone path does
   (`crates/engraph-codegraph/src/driver.rs`); if not, the status of step 3.
   Add the `"rust"` arm to the dispatch in `index_bazel_symbols`.
5. Update `plan_symbol_langs` tests, `DETAILS.md` §14.9,
   `docs/bazel-symbol-indexing.md`, and the README section on
   `--bazel-symbols`.

Validation of phase E:

1. Clone `https://github.com/cameron-martin/bazel-lsp` (`--depth 1`). Use a
   new database. Do `engraph index --bazel-symbols .`. Result: the output has
   a line `symbol[rust]: indexed`, and the count of `symbol` entities is
   approximately 715.
2. Remove `Cargo.toml` and `Cargo.lock` from the copy. Do the command again.
   Result: `symbol[rust]: skipped (N targets, set ENGRAPH_BAZEL_SCIP_RUST_CMD to index them)`,
   where N is the count of step 1 of the condition above.
3. Do `engraph index --bazel-symbols .` on `java-tutorial-java-bazel`.
   Result: `symbol[rust]: skipped (no targets in this workspace)`.

### Phase B: TypeScript and JavaScript

Status (2026-11-18): steps 1 to 7 and the validation are complete. Validation
results, on a sparse clone of `bazelbuild/examples` at `efab5bd` (the tip of
the repository on 2026-11-18). Step 2: `symbol[ts]: skipped (248 targets, set
ENGRAPH_BAZEL_SCIP_TS_CMD to index them)`, and a marker shows that
scip-typescript did not start. The count includes the first-party `js_library`
targets, but 187 of the 248 are `js_library` targets that
`npm_link_all_packages` makes for third-party npm packages
(`//:.aspect_rules_js/node_modules/<pkg>`, …); only 61 are first-party. The
count now leaves out each label with `node_modules` in it
(`target_count_query`), so the same run gives
`symbol[ts]: skipped (61 targets, …)`; the Rust (6) and Java (2) counts of
the phase E validation do not change. Step 3
passes on a clean clone with pnpm 8 on `PATH`. The lockfile is version 6.0,
which pnpm 12 does not read: with pnpm 12 on `PATH` the driver stops before
it installs and names the two fixes (`pnpm install --lockfile-only`, which
pnpm 12 can do — it rewrites the lockfile as version 9 — or pnpm 8 on
`PATH`), and changes nothing in the repo. The lockfile of `efab5bd` is not up to date with
`react/package.json` (vite), so the driver installs with
`--no-frozen-lockfile` and restores the lockfile after. Result:
`symbol[ts]: indexed (100330 SCIP bytes)`, 283 symbols (74 `.tsx`, 58 `.js`,
55 `.ts`, 5 `.jsx`, 5 `.mjs`, 4 `.mts`), with symbols from `.js` and from
`.ts` files as step 3 asks. The JavaScript symbols come from the driver's
second pass: the workspace pass reads each project through its tsconfig,
and a tsconfig without `allowJs` skips `.js`/`.jsx` files, so the driver
indexes, in one more scip-typescript run, exactly the JS files no project
covers (12 here; `astro.config.mjs` is covered — astro turns `allowJs` on
through `extends` — and stays single). `--infer-tsconfig` does not close
this gap: its file walk enters `node_modules` and the first `.d.ts` makes
it write `{}`, the TypeScript-only config (the same in scip-typescript
0.6.0 and 0.6.2), and it leaves the written `tsconfig.json` files in the
repo — the driver and the native pass now remove the files it wrote
(`remove_inferred_tsconfigs`), and the repo is clean after a run. The
native path, with the installed `node_modules`, no command, and a pnpm on
`PATH` that reads the lockfile version (scip-typescript runs `pnpm ls` for
`--pnpm-workspaces`), gives the TypeScript-only index (78679 SCIP bytes);
with a pnpm that cannot read it, `symbol[ts]: failed:` names the same two
fixes as the driver (`pnpm_broken_lockfile_help`). The pass also deleted 32 `bazel_target` entities
with no edges (bug 23 of plan 01).
Step 1 decision: the language id stays `ts`. The id is only printed, never
stored; `ts/js` breaks the output path `index-{language}.scip`; and `js`
gives an incorrect name to the TypeScript targets. Step 5 uses the command of
the standalone driver (`driver::typescript_command`): a workspace flag only
when the package list can be read and `pnpm` or `yarn` is on `PATH`, and the
root as an argument.

1. Find out if the language id `ts` must change to `js`, or to a label such
   as `ts/js`. The id is in `LangSpec.language`, in the `symbol[…]` output,
   and in `SymbolLangSummary`. Record the decision in the pull request.
2. Write a test: the `ts` entry of `LANGS` counts `js_library`, `js_binary`,
   and `js_test` also.
3. Add the rule kinds.
4. Write a test with a fake `bazel`: with targets and no `node_modules` at the
   workspace root, the result is
   `SkippedNotConfigured { targets, env: "ENGRAPH_BAZEL_SCIP_TS_CMD" }`, and
   scip-typescript does not start.
5. Add `run_ts`, with the structure of `run_go`: if `ENGRAPH_BAZEL_SCIP_TS_CMD`
   is set, `run_delegated`; if the root has `node_modules`, run
   `scip-typescript index --infer-tsconfig`, with `--pnpm-workspaces` when
   `pnpm-workspace.yaml` exists and `--yarn-workspaces` when the root
   `package.json` has a `workspaces` field; if not, the status of step 4.
6. Write an example driver `docs/examples/scip-typescript-bazel-index.sh`. It
   installs the packages with the package manager of the lock file
   (`pnpm install --frozen-lockfile`, or `yarn install --immutable`, or
   `npm ci`), and then runs the command of step 5. It must write
   repo-root-relative document paths.
7. Update `DETAILS.md`, `docs/bazel-symbol-indexing.md`, and the README.

Validation of phase B:

1. Make a sparse clone of `https://github.com/bazelbuild/examples` at
   `efab5bd4660ebfe720bd52efaeeccefaf00ee4ad` with only the `frontend`
   directory, or use `frontend-typescript-bazel` from `engraph-test-repos`.
2. Do `engraph index --bazel-symbols .`. Result:
   `symbol[ts]: skipped (N targets, set ENGRAPH_BAZEL_SCIP_TS_CMD to index them)`,
   where N includes the 15 `js_library` targets. No scip-typescript process
   starts.
3. Set `ENGRAPH_BAZEL_SCIP_TS_CMD` to the example driver. Do the command
   again. Result: `symbol[ts]: indexed`, and the graph has symbols from `.js`
   files and from `.ts` files.

### Phase D: Python

Status (2026-11-18): steps 1 to 3 and the validation are complete; step 4 is
not started, because its condition is not true on ray. Step 2 results, on
ray commit `a24e301` (5,115 `.py` files; `.bazelversion` is now 8.8.1),
scip-python 0.6.8, Node v24.22.1, `--project-name ray --project-version
0.0.0` as the Bazel pass gives them:

| `--max-old-space-size` | Result | Time | Maximum RSS | SCIP size |
|---|---|---|---|---|
| Node default (about 4 GB) | out of memory (2026-11-11) | — | — | — |
| 8192 | completes | 7m54s | 8.3 GiB (8,713,088 KB) | 210,778,316 bytes |
| 16384 | completes | 5m21s | 9.85 GiB (10,332,304 KB) | 210,778,316 bytes |

With 8 GB the run is GC-bound: the RSS stays at the limit and the time is
48% longer, for the same SCIP. The default is 12288 (about 20% more than ray
uses); `ENGRAPH_NODE_HEAP_MB` overrides it. Validation:
`symbol[python]: indexed (210778316 SCIP bytes)`, 425,006 symbol entities
(423,039 in `.py` files, 171 in `.pyi`), 223,993 `CALLS`, 179,313
`REFERENCES`, 10,936 `IMPLEMENTS`, 6m25s with the load (release build,
maximum RSS 9.66 GiB). The graph has many relations, and they resolve across
packages: 6,445 of the 6,908 textual `ray.get(` call sites in 727 files are
`CALLS` to its definition, from 671 files. Thus step 4 (`rules_python`) waits
for a repo that shows the problem. Two notes from the validation: the symbol
load deleted 383 of the 3,129 `bazel_target` entities (bug 23 of plan 01),
and scip-python names modules by their path from `--cwd`
(`python.ray._private.worker`, not `ray._private.worker`), so a second repo
that imports `ray` does not link to these ids.

1. Write a test: `build_indexer_command` for `scip-python` sets
   `NODE_OPTIONS=--max-old-space-size=<N>` in the environment of the command,
   where N is from `ENGRAPH_NODE_HEAP_MB` when set, or a default that this
   plan gives after step 2. Do the same for `scip-typescript`: both are Node
   programs.
2. Find out the heap that ray needs: run scip-python on ray with
   `NODE_OPTIONS=--max-old-space-size=8192`. Record the result, the time, and
   the SCIP size in the pull request. Select the default from the result. A
   test on 2026-11-11 did not complete: at approximately 3,700 of 5,126 files
   the process had 7.5 GB resident, near the 8 GB limit. Thus 8 GB is possibly
   not sufficient, and the first measurement must also record the maximum
   resident memory.
3. Add the environment variable to the command.
4. If scip-python completes on ray but the graph has few relations, do the
   `rules_python` work: resolve a virtual environment or `site-packages` for
   each project, and index each project. This step is the most difficult.
   Start it only when a real repo shows the problem.

Validation of phase D:

1. Clone `https://github.com/ray-project/ray` (`--depth 1 --filter=blob:none`).
   Do `engraph index --bazel-symbols .`. Result: `symbol[python]: indexed`
   with a SCIP size that is more than 0 bytes, or a `failed:` line that shows
   the reason (after bug 11 of plan 01). Record the SCIP size and the count of
   `symbol` entities in the pull request: this plan has no correct value for
   them, because no test of the step 2 command is complete.

## Tests

- `LANGS` has a `rust` entry with the rule kinds of phase E, step 1.
- `run_rust` without `Cargo.toml` reports `SkippedNotConfigured` and does not
  start rust-analyzer.
- The `ts` entry counts `js_library`, `js_binary`, and `js_test`.
- `run_ts` without root `node_modules` reports `SkippedNotConfigured` and does
  not start scip-typescript.
- The scip-python and scip-typescript commands have `NODE_OPTIONS` with the
  heap size.

## Risks

- **Phase E, duplicate symbols.** A repo with `Cargo.toml` that is also a
  Cargo workspace is indexed two times when the user does both
  `engraph index .` (standalone) and `engraph index --bazel-symbols .`. The
  loader is idempotent for one project key, so the second load replaces the
  first.
- **Phase B, the language id.** A change of `ts` to `js` changes the output
  that users and scripts read. Step 1 of phase B decides.
- **Phase D, memory.** A large heap default uses memory that the computer may
  not have. Read the available memory, or use a moderate default (for example
  8 GB) with `ENGRAPH_NODE_HEAP_MB` as the override.

## Reason to wait

Phases B and D wait for a real repo that shows the problem, as before. Phase E
is ready: `bazel-lsp` shows the problem, and the Go path gives the structure.

## Dependencies

None. The generic `SkippedNotConfigured { targets, env }` status (2026-11-11)
is the status that each phase uses; no phase adds a status.

## Effort

Phase E: half a day. Phase B: one day, with the example driver. Phase D: half
a day for the heap; the `rules_python` work is not estimated.
