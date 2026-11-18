# Plan 05: Stable symbol ids

Read [the general information for all plans](../00-general-info-plan.md)
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

Steps 1 to 13 are complete (2026-11-17). Step 1: plan 03, step 7. Steps 2 to
5: the findings, the two tables, and the rules below the steps. Steps 6 and 8:
R2 and R3 are in `scip_loader::normalize_moniker`, which takes the project
key; R1 is in `driver::python_project_name`, used by the standalone driver
and the Bazel Python pass. Step 7 has nothing to do (no absolute paths in
scip-go ids). Steps 9 and 10: the only functions that write symbol ids are
the three writers of `scip_loader` (the entity upsert, the placeholder
insert, and the relation insert; the other `INSERT INTO entities` sites are
test fixtures, and `bazel.rs` writes Bazel labels, not SCIP ids), and all
loads go through `load_with_options`, where one `normalize_index` pass
rewrites every id of the parsed index before any row is written. The rust,
typescript, and java snapshots changed; go, python, and the prebuilt SCIP
blobs did not (the fifth test of this plan). Steps 11 to 13:
`index_state.normalization` (schema v18) records
`scip_loader::NORMALIZATION_VERSION` (1) with each run;
`index::stale_normalization` lists the projects with rows under another
version (NULL, from before the rules, counts as 0); `engraph index` prints a
note that names them and recommends a full `engraph index <repo>` of each.

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

### Step 3: the id fields of each indexer (2026-11-17)

A SCIP id is `<scheme> <package manager> <package name> <version>
<descriptors>`. The sources of the four fields:

| Indexer | Scheme | Package manager | Package name | Version |
|---|---|---|---|---|
| rust-analyzer | `rust-analyzer` | `cargo` | `Cargo.toml` `name` (crates); `core`, `alloc`, `std` (standard library) | `Cargo.toml` `version`; a GitHub URL for the standard library |
| scip-python | `scip-python` | `python` | the basename of the indexed directory (driver `--project-name`); `python-stdlib` (standard library); the pip name (installed packages) | the pin `SCIP_PYTHON_VERSION` = `0.0.0` (driver `--project-version`); the typeshed version in scip-python (standard library); the pip version (installed packages) |
| scip-go | `scip-go` | `gomod` | the `module` path of `go.mod`; `github.com/golang/go/src` (standard library) | the pin `SCIP_GO_VERSION` = `0.0.0` (driver and loader, part 2 step 1); the `go` directive of `go.mod` (standard library); the `go.sum` version (modules) |
| scip-typescript | `scip-typescript` | `npm` | `package.json` `name`; `typescript` (compiler library) | `package.json` `version`; the TypeScript version in scip-typescript |
| scip-java (Maven) | `semanticdb` | `maven` | `maven/<groupId>/<artifactId>`; `jdk` (JDK); empty (package symbols) | the `pom.xml` version; the JDK major of the computer; empty (package symbols) |

### Step 4: the problems for each scheme (2026-11-17)

The categories of the step: empty versions, absolute paths, and versions that
change. The version that the scip-python and scip-go drivers give stays as it
is (the step's constraint, and part 2 step 1). Step 5 writes one rule for each
problem.

| # | Scheme | Problem | Effect |
|---|---|---|---|
| P1 | `scip-python` | The package name is the basename of the indexed directory, not a value from the repo. | One repo gives different ids in two checkouts with different directory names. The worktrees of this repo do this today. Also, two repos with the same directory name and the same module paths have the same ids. |
| P2 | `semanticdb` | Package symbols have an empty package name and an empty version (`semanticdb maven . . fixture/models/`). | The package rows of all repos, and of the JDK, with the same package name are one row in the database. Two repos with the same package names share these rows (the third test of this plan). Only package symbols: classes, methods, and fields carry the Maven coordinates. |
| P3 | `semanticdb` | JDK ids carry the JDK major of the computer (`jdk 21`). | Two computers with different JDK majors give different ids to the same JDK references. Relations to JDK symbols do not connect between the two. |
| P4 | `rust-analyzer`, `scip-typescript`, `semanticdb` | The first-party version follows the manifest (`Cargo.toml`, `package.json`, `pom.xml`). A release bump changes every id of the package. | Not "at each commit", but each release churns the repo's graph and breaks relations from other repos. The pins for scip-python and scip-go avoid this for those two schemes. |
| — | `scip-go` | None. No absolute paths in 0.2.7 (the installed and pinned version), the version is pinned, and the module path is unique. | Step 7 (absolute paths in scip-go ids) has nothing to do. |

Absolute paths occur in no scheme. The standard-library versions of
scip-python (typeshed) and scip-typescript (bundled TypeScript) follow the
indexer version, which the test suite of part 1 tracks; the Go standard
library follows the `go` directive of `go.mod`. These change ids only together
with a deliberate tool or repo change, so they are not problems in the table.

### Step 5: the rules (2026-11-17)

One rule for each problem. The pin value is `0.0.0`, the value the
scip-python and scip-go drivers already give (`SCIP_PYTHON_VERSION`,
`SCIP_GO_VERSION`). Each rule changes the ids of its scheme one time; step 11
to 13 record the rule version and recommend a full index run.

| Rule | For | Statement | Where |
|---|---|---|---|
| R1 | P1 | The scip-python driver gives `--project-name` the `[project].name` of `pyproject.toml`. When the repo has no `pyproject.toml` or the table has no `name`, the driver gives the basename of the indexed directory, as before. | `ScipPython::command` (`driver.rs`). The package name comes from the repo, not from the directory, so two checkouts give the same ids. scip-python 0.6.8 writes the name as it is in `pyproject.toml` (`engraph-fixture` keeps its `-`), found out in step 8 and recorded in the Python snapshot. |
| R2 | P2 | A `semanticdb` id with the empty package name `.` gets the project tag (the first 16 hex digits of the SHA-256 of the project key, `project_tag`, the tag of local symbols) as its version: `semanticdb maven . . fixture/models/` → `semanticdb maven . <tag> fixture/models/`. | `normalize_moniker`, which gets the project key as a parameter for this rule. Package rows of two repos with the same package names are then two rows (the third test). JDK package rows (`. . java/util/`) become per-project placeholders, which is harmless. R2 applies before R3. |
| R3 | P3, P4 | The version of each `rust-analyzer`, `scip-typescript`, and `semanticdb` id becomes the pin `0.0.0`: `rust-analyzer cargo engraph-core 0.3.0 …` → `rust-analyzer cargo engraph-core 0.0.0 …`, `semanticdb maven jdk 21 …` → `semanticdb maven jdk 0.0.0 …`. The rule applies to all ids of the scheme, first-party and dependency, so a reference from one repo and the definition in another repo stay one id (the cross-repo test). | `normalize_moniker`, one `match` arm for each scheme. The rust-analyzer standard library URL and the JDK major are also replaced; they said which tool version produced the id, which the `index_state` row of the run records instead. Two versions of one dependency used at the same time become one placeholder row, which has no definition and no location; this is the accepted cost. |
| — | scip-go | No rule. The driver pins the module version (step 1) and the loader pins the `.` version of sibling modules (`pin_go_module_versions`). | — |

The scip-python and scip-go drivers keep their pinned versions (the
constraint of step 4). Dependency ids of scip-python (pip version) and scip-go
(`go.sum` version) keep their versions: those two schemes have the same
one-sided pin they had before this plan, and a cross-repo reference between
two Python or two Go repos does not connect; that is a separate problem, not
in the table.

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
