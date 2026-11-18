# Plan 05: Stable symbol ids

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

A symbol id is the same on each computer and after each commit. Two different
symbols do not have the same id. A test shows when a change to engraph or to an
indexer changes the ids or the relations of known symbols.

## Condition at this time (2026-10-30)

- The function `scip_loader::normalize_moniker` does not change the symbol id.
- The standalone Go driver (`ScipGo` in `driver.rs`) and the Bazel Go pass
  give a fixed version (`SCIP_GO_VERSION`) to scip-go, and the loader changes
  the version `.` of a scip-go module id to it (2026-11-16, plan 03 step 7).
  The Python driver also gives a fixed version (`SCIP_PYTHON_VERSION`).
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
- The loader gives SCIP local symbols the id
  `local <n>@<project tag>:<relative_path>`. The tag comes
  from the project key, which is a path on the computer. Thus, a snapshot of
  ids must remove the tag of local symbols.

## Steps, part 1: test suite for stable symbol ids

Part 1 is complete (2026-11-17). The fixtures are in
`crates/engraph-codegraph/testdata/fixtures/`, the snapshot files and prebuilt
SCIP files in `testdata/snapshots/` and `testdata/scip/`, and the tests in
`crates/engraph-codegraph/tests/symbol_snapshots.rs`.

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

Note for steps 17 and 18 (2026-11-17): the snapshot files record the indexer
versions of the computer that made them (rust-analyzer 1.100.0, scip-python
0.6.8, scip-go 0.2.7, scip-typescript 0.6.0, scip-java 0.12.3), and the
comparison of step 10 includes the version. Step 18 pins the CI job to these
versions: the toolchain 1.100.0 (rust-analyzer comes with it), scip-python
0.6.8, scip-typescript 0.6.0, and scip-go v0.2.7. A new indexer version needs
a new pin and a new snapshot file together. The snapshot files also record a
hash of the fixture sources, so the test of step 16 shows a fixture that
changed after its files were made, also for Java, which CI does not index.

## Steps, part 2: normalization of symbol ids

Steps 1 and 2 are complete. Step 1: plan 03, step 7. Step 2: the findings
below the steps (2026-11-17).

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

### Findings of step 2 (2026-11-17)

Source: all symbol ids in the prebuilt SCIP files of the part 1 fixtures
(`crates/engraph-codegraph/testdata/scip/`): document symbols, occurrences,
relationships, and external symbols. The computer has Python 3.14, JDK 21,
and Go 1.26.3.

| Indexer | First-party ids | Dependency and standard library ids | Notes |
|---|---|---|---|
| rust-analyzer 1.100.0 | `rust-analyzer cargo <crate name> <Cargo.toml version>` | `rust-analyzer cargo core https://github.com/rust-lang/rust/library/core …`: the version part is a URL, the same on each computer | No absolute paths. A version bump of the crate changes all of its ids. |
| scip-python 0.6.8 | `scip-python python <project name> 0.0.0`: the project name is the **basename of the indexed directory** (`ScipPython::command`, `--project-name`), the version is the pin `SCIP_PYTHON_VERSION` | `scip-python python python-stdlib 3.11 …`: 3.11 is the typeshed version in scip-python, not the Python of the computer; the same on each computer | The ids of one repo differ between two checkouts with different directory names (a worktree `elm` and a clone `engraph`). Two repos with the same directory name and the same module paths have the same ids. |
| scip-go 0.2.7 | `scip-go gomod <module path> 0.0.0`: the version is the pin `SCIP_GO_VERSION` | `scip-go gomod github.com/golang/go/src go1.21 …`: the version is the `go` directive of `go.mod`, not the toolchain | **No absolute paths** in 0.2.7: the doubt in the condition is confirmed, and step 7 has nothing to do. |
| scip-typescript 0.6.0 | `scip-typescript npm <package.json name> <package.json version>` | `scip-typescript npm typescript 6.0.3 …`: the TypeScript in scip-typescript | A version bump of the package changes all of its ids. |
| scip-java 0.12.3 with Maven | `semanticdb maven maven/<groupId>/<artifactId> <pom version>` for classes, methods, and fields | `semanticdb maven jdk 21 java/lang/String#`: 21 is the **JDK of the computer** | **Package symbols have an empty package and version**: `semanticdb maven . . fixture/models/`, and also `. . java/util/`. They enter the database as placeholder rows (`ensure_placeholder_entity`, from occurrences) and are the same for all repos, and the JDK, with the same package name. Method ids with `. .` also occur in `enclosing_symbol` of local symbols, which the loader does not read. The condition's statement about an empty version for all first-party Java code is not correct: classes and methods have the Maven coordinates and version. |

Local symbols (`local <n>`) are 12 to 31 per fixture; the loader gives them a
tag of the project key (see the condition), and the snapshots leave them out.

## Tests

- Two index runs of a Go repo at different commits give the same Go symbol ids.
- Each rule has a unit test with a symbol id before and after the rule.
- Two Java repos with the same package names do not have the same symbol ids.
- The cross-repo test (`tests/workspace_cross_repo.rs`) shows the same result
  as before.
- The test suite of part 1 shows changed ids only for the schemes that have a
  rule.

## Risks

- A new indexer version can change symbol ids correctly. The test then shows an
  error. Make a new snapshot file only after you examine the differences.
- A change to symbol ids disconnects relations between repos that you indexed
  with different rule versions. Thus, after an update of the rules, do a full
  index run of all repos.

## Dependencies

Part 2 must wait for part 1, but step 1 of part 2 can start before part 1.
