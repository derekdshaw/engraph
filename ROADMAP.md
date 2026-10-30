# Engraph roadmap

This file gives the features and changes that engraph does not have at this
time. `DETAILS.md` describes the features that engraph has.

This file uses ASD-STE100 Simplified Technical English (issue 9). Code names,
file names, and commands are technical nouns. They show in `code` font.

The items are in the sequence of their expected effect, from the largest to the
smallest. First, the effect on the tokens that engraph saves. Second, the effect
on the part of a codebase that engraph shows correctly. This sequence is not the
sequence of the work. Some items must wait for other items. Refer to
"Dependencies" in each item and to "Sequence of work".

Each item gives its condition at a specified date. Before you start an item,
make sure that the condition is still correct.

## Rules for the work

1. Do the work for each item on a new branch.
2. Make one commit for each step that changes code.
3. For each step that changes code, do these tasks:
   1. Write a test that shows the problem or the new function.
   2. Make sure that the result of the test is incorrect before you change the
      code.
   3. Change the code.
   4. Do the `cargo test --workspace` command.
   5. Do the `cargo clippy --workspace --all-targets` command with and without
      `--features embeddings`.
   6. If the change has an effect on how engraph operates, update `DETAILS.md`,
      `README.md`, and this file.
4. Some steps start with "Find out". For these steps, record the result in the
   pull request. Then do the steps that follow.

## Sequence of work

1. Start steps 1 to 4 of item 4 (measurement of file reads) at the start of the
   work. These steps must collect data for two weeks. You can do other items
   at the same time.
2. Do item 2 (deeper workspace discovery) before item 3 (all Go modules).
   Item 3 uses the member rules of item 2.
3. In item 5 (stable symbol ids), do part 1 (test suite) before part 2
   (normalization). Part 2 changes symbol ids. The test suite shows if a change
   to the ids causes problems. Step 1 of part 2 is a bug fix. You can do it
   before part 1.
4. You can do the other items in any sequence.

---

## 1. Automatic index runs

### Result

When a session starts, engraph updates the code graph of the project if the
code graph is old.

### Condition at this time (2026-10-30)

The `engraph index` command is manual. The SessionStart hook does not start an
index run.

The grep redirect, the PostToolUse(Read) symbol list, and `engraph subgraph` use
the code graph. If the code graph does not exist or is old, these features save
fewer tokens or no tokens. For a file that changed after the index commit, the
grep redirect lets the grep continue, and `engraph subgraph` shows a "Changed
since the index" note. No hook tells Claude or the user to do an index run.

The `index_state` table (schema v10) records the commit of each indexed subtree
of a project.

### Requirements

- The hook must find out if the code graph is old. The commit in `index_state`
  is a possible source for this check.
- A slow index run (for example, a cold scip-java build) must not start in each
  session.

### Dependencies

None.

---

## 2. Deeper workspace discovery

### Result

You can select the projects that `engraph index --recursive` finds. The
discovery obeys `.gitignore` files. Go modules and Python projects below a
project of the same language are in the result.

### Condition at this time (2026-10-30)

`walk_recursive` examines the directory tree to a maximum depth of 16
(`RECURSIVE_MAX_DEPTH`). It does not examine the directories in
`is_pruned_dir`: hidden directories, `node_modules`, `target`, `vendor`,
`__pycache__`, `venv`, and `testdata`. It does not read `.gitignore` files. You
cannot include or exclude specified directories.

If a directory above a project has the same language, the discovery does not
record the project. This rule is correct for Cargo workspaces. It is not correct
for Go: a Go module below a different Go module is not in the result. The rule
is possibly not correct for TypeScript packages and Python projects.

### Steps

1. In `walk_recursive`, use the `ignore` crate to obey `.gitignore` and
   `.ignore` files.
2. Keep the list in `is_pruned_dir` together with the `.gitignore` rules.
3. Add the `--max-depth` flag, with a default value of 16.
4. Add the `--include <glob>` flag. Each glob is relative to the workspace root.
5. Add the `--exclude <glob>` flag. Each glob is relative to the workspace root.
6. In the `--dry-run` output, show each directory that the discovery found.
7. For each directory, show the cause for which the discovery included it or
   did not include it.
8. Find out if a git repo in a subdirectory of a different git repo has a
   different project key.
9. Write a test for this condition.
10. Replace the rule that makes a directory with the same language a member.
    Use a rule for each language:
    - Cargo: a directory is a member only if the root `Cargo.toml` has a
      `[workspace]` section.
    - TypeScript: a directory is a member only if the root `package.json` lists
      it in `workspaces`.
    - Go and Python: a directory with a manifest in it is not a member.

### Tests

- The discovery does not include a directory that `.gitignore` names.
- `--exclude` removes a directory from the result. `--include` limits the
  result to the directories that it names.
- `--max-depth 2` does not include a project at depth 3.
- A Cargo crate below a root `[workspace]` is a member. A Cargo crate below a
  root crate without `[workspace]` is not a member.
- A Go module below a root Go module is not a member.

### Risks

The new member rules can increase the number of index runs in a workspace. Use
`--dry-run` to see the result before you do a full index run.

### Dependencies

None. Item 3 uses the member rules of this item.

---

## 3. All Go modules in the code graph

### Result

engraph indexes all Go modules of a repo, with correct relations between the
modules. This is also true in Bazel repos.

### Condition at this time (2026-10-30)

- The Go driver finds a module only when `go.mod` is in the indexed directory.
  For a repo with more than one Go module, a `--recursive` index run is
  necessary. But that run does not include a Go module below a different Go
  module (item 2).
- The Go driver does not read `go.work` files.
- scip-go indexes only one build configuration. Thus, files with build tags for
  a different platform (for example `_linux.go` or `//go:build`) are not in the
  code graph.
- The standard Go driver indexes `_test.go` files, because scip-go indexes
  tests by default (`Tests: !opts.SkipTests` in scip-go v0.2.7). The Bazel Go
  pass gives `--skip-tests` to scip-go. Thus, the Bazel Go pass does not index
  `_test.go` files.
- In Bazel repos, the native Go pass (`run_go_modules`) indexes only the
  modules that have a `go.mod` file. A large gazelle repo can have thousands of
  `go_library` targets, but only a small number of `go.mod` files.
  `LangStatus::IndexedModules` shows this difference. With the
  `ENGRAPH_BAZEL_SCIP_GO_CMD` command, a repo can use its own Bazel scripts to
  index all targets (`docs/examples/scip-go-bazel-index.sh`).

### Steps

1. Write a test with a root Go module and a second Go module in `tools/`.
2. Make sure that `engraph index --recursive` indexes the two modules. Item 2,
   step 10 changes the discovery for this.
3. Find out if scip-go can index all modules of a `go.work` file in one
   operation.
4. If scip-go can do this, give the `go.work` directory to scip-go.
5. If scip-go cannot do this, index each module of the `go.work` file as a
   subtree of the repo.
6. Write a test in which a function in module A uses a function in module B.
7. Make sure that the relation from module A points to the symbol in module B.
   Do this for the native Go pass and for the merged SCIP data of the Bazel Go
   pass.
8. Make sure that vendored copies of a module do not cause duplicate symbols.
9. Find out if scip-go accepts build tags or the `GOOS` and `GOARCH`
   environment variables.
10. If scip-go accepts them, add the `--go-tags` flag to the `engraph index`
    command.
11. Write the limit of one build configuration in `DETAILS.md`.
12. Add the `--go-tests` flag. With this flag, the Bazel Go pass does not give
    `--skip-tests` to scip-go.

Optional step for Bazel repos: engraph can make a temporary `go.mod` file for
each gazelle prefix from the import paths of the `go_library` targets. Then it
can index each of these roots with scip-go and remove the temporary files. This
method does not need a command from the repo. But it is complex, and it cannot
resolve hermetic Bazel dependencies without more code. It must not change the
target repo.

### Tests

- `engraph index --recursive` indexes the root module and the module in
  `tools/`.
- A relation from module A to module B points to the correct symbol.
- In the Bazel Go pass with `--go-tests`, a function in a `_test.go` file is in
  the code graph. Without `--go-tests`, it is not in the code graph.

### Risks

An index run for each Go module can use much time in a large repo. Use the
`--include` flag of item 2 to select modules.

### Dependencies

Item 2 (the member rule for Go).

---

## 4. Measurement of file reads

### Result

Telemetry shows the tokens that file reads use. It also shows the tokens of
reads of a file that the session read before. With these data, you can decide
if a change to file reads is worth the work.

### Condition at this time (2026-10-30)

The PostToolUse(Read) hook adds a list of symbols after Claude reads a file. Its
telemetry event (`read_augment`) records only the tokens of this list. It does
not record the tokens of the read.

An earlier plan added a PreToolUse(Read) hook that gives a file summary before
the read. The purpose of the summary was that Claude reads only the necessary
part of the file. This plan does not operate. The Claude Code hooks documentation
(`code.claude.com/docs/en/hooks`) gives this description of `additionalContext`
for PreToolUse: "String added to Claude's context alongside the tool result."
Thus, Claude receives the summary at the same time as the full file. The summary
cannot make the read smaller. It gives the same data as the PostToolUse(Read)
symbol list.

A hook that stops the read and gives only a summary can make Claude use
incorrect data.

### Steps

1. Find out if the input of the PostToolUse(Read) hook includes the content of
   the read in `tool_response`. If it does not, count the tokens of the lines
   that the read selected. Use the file path, `offset`, and `limit` in
   `tool_input`.
2. In the PostToolUse(Read) hook, record a telemetry event for each read. In
   this event, record the token count of the read and a hash of the file path.
3. Add the `--reads` flag to the `engraph gain` command. With `--reads`, show
   the tokens of all reads. Also show the tokens of reads of a file that the
   session read before.
4. Collect this data for two weeks. In the same two weeks, measure the total
   savings with `engraph gain`. The initial success criterion of engraph is
   that it saves 20% or more of all tokens.
5. If the reads of the same file again are not one of the three largest token
   sources, stop this item.
6. Find out if a different method can make reads of the same file smaller. A
   possible method: for a read of an unchanged file that the session read
   before, the PreToolUse(Read) hook stops the read and refers Claude to the
   earlier result.
7. Before you use the method of step 6, find out what occurs after Claude Code
   compacts the conversation. After compaction, the earlier result is possibly
   not in the context.

### Tests

- The PostToolUse(Read) hook records one telemetry event for each read, with
  the token count of the read.
- `engraph gain --reads` shows the tokens of reads of the same file again
  separately from the tokens of all reads.

### Risks

The hook operates for each read. The telemetry must be small and fast.

### Dependencies

None.

---

## 5. Stable symbol ids

### Result

A symbol id is the same on each computer and after each commit. Two different
symbols do not have the same id. A test shows when a change to engraph or to an
indexer changes the ids or the relations of known symbols.

### Condition at this time (2026-10-30)

- The function `scip_loader::normalize_moniker` does not change the symbol id.
- The standalone Go driver (`ScipGo` in `driver.rs`) does not give
  `--module-version` to scip-go. Thus, scip-go uses a version tag on `HEAD`, or
  the first 12 characters of the `HEAD` commit hash, in each Go symbol id
  (`InferModuleVersion` in scip-go v0.2.7). All Go symbol ids change at each
  commit that has no tag. The Bazel Go pass
  gives a fixed version (`SCIP_GO_VERSION`). The Python driver also gives a
  fixed version (`SCIP_PYTHON_VERSION`).
- Earlier documents say that scip-java gives an empty version to first-party
  Java code (`semanticdb maven . . …`). Thus, two repos with the same package
  names can have the same symbol ids. This is not verified. The live database
  has no Java symbols, and no test or fixture in this repo shows this id.
- Earlier documents say that scip-go versions before 0.7 put absolute paths of
  the computer in symbol ids. This is possibly not correct. The installed
  scip-go is v0.2.7, and its source does not show absolute paths in symbol ids.
  Step 2 of part 2 finds out.
- The symbol ids of published dependencies (Cargo, npm, PyPI, Go modules) are
  the same on each computer.
- No test compares the symbol ids of a repo between two engraph versions or two
  indexer versions.

### Steps, part 1: test suite for stable symbol ids

1. Make a small fixture repo for each language: Rust, Python, Go, TypeScript,
   and Java.
2. Use specified versions of all dependencies in each fixture repo.
3. In each fixture repo, select 50 symbols.
4. Include functions, types, and methods in the 50 symbols.
5. Include symbols that code in a different module uses.
6. For each fixture repo, make a snapshot file.
7. In the snapshot file, record the id, the name, the file path, and the line
   range of each symbol.
8. In the snapshot file, also record the ids of the callers and callees of each
   symbol.
9. In the snapshot file, record the version of the indexer.
10. Add a test that indexes each fixture repo and compares the result with the
    snapshot file.
11. If the indexer is not installed, stop the test without an error, as
    `tests/drivers_live.rs` does.
12. If the result is not the same as the snapshot file, show the differences.
13. If the indexer version is not the version in the snapshot file, show the
    two versions in the error message.
14. Add the `ENGRAPH_UPDATE_SNAPSHOTS=1` environment variable to write new
    snapshot files.
15. Keep a prebuilt SCIP file for each fixture repo in the repo.
16. Add a second test that loads the prebuilt SCIP file and compares the result
    with the snapshot file. This test does not use an indexer. Thus, it
    operates in all CI jobs and shows changes to the loader.
17. Add the first test to the "Codegraph live drivers" CI job.
18. In the CI job, install specified versions of the indexers.

### Steps, part 2: normalization of symbol ids

1. In the standalone Go driver, give `--module-version` with the value of
   `SCIP_GO_VERSION` to scip-go. This change replaces all Go symbol ids one
   time.
2. Find out which parts of the symbol id each indexer gives. Do this for
   rust-analyzer, scip-python, scip-go, scip-typescript, and scip-java.
3. For each indexer, record the scheme, the package manager, the package name,
   and the version.
4. Make a table of the problems for each scheme. Look for empty versions,
   absolute paths, and versions that change at each commit. Do not change the
   specified version that the scip-python driver gives.
5. Write one rule for each problem in the table.
6. For first-party Java code with an empty version, put a value from the
   project key in the version part.
7. If scip-go symbol ids have an absolute path, remove the part of the path
   that is on the computer.
8. Put the rules in `normalize_moniker`, with one `match` arm for each scheme.
9. Find each function that writes a symbol id into the database.
10. Make sure that each of these functions uses `normalize_moniker`.
11. Add the `normalization` column to the `index_state` table, with the version
    of the rules.
12. Compare the version in `index_state` with the version of the rules.
13. If the two versions are different, show a message that recommends a full
    index run.

### Tests

- Two index runs of a Go repo at different commits give the same Go symbol ids.
- Each rule has a unit test with a symbol id before and after the rule.
- Two Java repos with the same package names do not have the same symbol ids.
- The cross-repo test (`tests/workspace_cross_repo.rs`) shows the same result
  as before.
- The test suite of part 1 shows changed ids only for the schemes that have a
  rule.

### Risks

- A new indexer version can change symbol ids correctly. The test then shows an
  error. Make a new snapshot file only after you examine the differences.
- A change to symbol ids disconnects relations between repos that you indexed
  with different rule versions. Thus, after an update of the rules, do a full
  index run of all repos.

### Dependencies

Part 2 must wait for part 1, but step 1 of part 2 can start before part 1.

---

## 6. Bazel symbols for TypeScript and Python

### Result

The symbol-level Bazel pass (`--bazel-symbols`) indexes TypeScript and Python
code.

### Condition at this time (2026-10-30)

The symbol-level Bazel pass indexes Go (phase A) and Java (phase C). Refer to
`docs/bazel-symbol-indexing.md` and `DETAILS.md` §14.9. For TypeScript and
Python, `run_language` runs the standard indexer at the workspace root. No
command from the repo (`ENGRAPH_BAZEL_SCIP_TS_CMD`) exists. The failures below
are from the diagnosis of 2026-06-02. They are not verified again. Two phases
remain:

- **Phase B, TypeScript:** scip-typescript possibly fails on a cold run. A
  `bazel build //...` command must first make the `node_modules` links in
  `bazel-bin/<pkg>/node_modules`. The setup of `rules_ts` and pnpm workspaces
  is different in each repo. Thus, a command from the repo
  (`ENGRAPH_BAZEL_SCIP_TS_CMD`) is a possible solution, as for Java.
- **Phase D, Python:** scip-python must find the virtual environment and the
  `site-packages` of each project in `rules_python`. This phase is the most
  difficult. Do it last.

### Reason to wait

Do this work when a real repo shows the problem.

### Dependencies

None.

---

## 7. LLM reranking

### Result

A small model puts the top results of hybrid retrieval (RRF) in a better
sequence. This function is off by default, because it uses tokens.

### Reason to wait

Hybrid retrieval operates at this time. No data shows that its results are not
sufficiently good. Before this work, a benchmark must show that the precision
of the first 5 results (p@5) does not increase more.

### Dependencies

A benchmark data set of real queries. No such data set exists at this time.

### Effort

2 to 3 days. Most of the work is the prompt design and the measurement of the
token cost.

---

## 8. `/resume` and `/save` skills

### Result

A `/save` skill calls `engraph save`. A `/resume` skill calls `engraph resume`.

### Condition at this time (2026-10-30)

`engraph save`, `engraph resume`, and the `## decisions` and `## last session`
sections of the SessionStart brief operate.

### Reason to wait

The SessionStart brief shows the saved decisions and the last session. Claude
can do `engraph resume` with one Bash command. Thus, the skills add only a
small improvement.

### Effort

Approximately half a day (skill Markdown).

---

## 9. Obsidian export

### Result

engraph writes `context_items`, `bugs`, and decisions to an Obsidian vault, with
wikilinks between projects.

### Reason to wait

This function does not save tokens. It is useful only for notes that a person
reads. Do it only if the user wants the vault.

### Effort

2 to 3 days. The function is simple. Most of the work is to match the Obsidian
conventions.

---

## Possible future work (not on the roadmap)

- **MCP server.** An MCP server can give `engraph recall` and
  `engraph subgraph` as MCP tools with typed arguments. For Claude Code and
  Codex, this adds almost nothing. These clients can do the `engraph` commands
  with their shell tool. The hooks can also change and stop tool calls, and an
  MCP server cannot do this. Each MCP tool also adds tokens to each session. Do
  this work only if a client that has no shell access must use engraph.
- **Compression that changes with the budget level.** When
  `engraph budget status` shows level 2 or 3, the filters can use smaller
  limits. For example, the `tree` depth can change from 3 to 2, and the
  `docker logs` tail can change from 200 lines to 100 lines. At this time, only
  the SessionStart brief and `engraph budget status` show the level. No filter
  uses it. A design for this
  escalation does not exist at this time.
- **Removal of old memory.** A memory store that only increases becomes a
  problem. A possible design: a score that decreases with time and has a
  minimum, a lower rank for an item that retrieval does not find after N
  queries, and a size limit for each project. The schema has the necessary
  column (`scopes.archived`). The algorithm does not exist.
- **Cloud sync.** engraph keeps all data on the local computer. To share memory
  between computers, these parts can be the base: the `EmbeddingProvider`
  trait, the UUIDv7 ids, and the append-only `ingestion_log` table. The work
  must add `MemoryStore` and `GraphStore` traits, and then an implementation
  for Turso or LiteFS. The architecture review recommended that engraph does
  not add these traits before a real need for cloud sync occurs.
- **Weekly summary.** `engraph gain --weekly` shows token totals for each week.
  It does not show the data that follows. An `engraph weekly` command reads
  `events` and `messages`.
  It shows the most compressed commands, the most recalled phrases, the longest
  sessions, and the largest savings. These data help the user and help to
  adjust the filters.
