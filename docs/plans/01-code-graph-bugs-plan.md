# Plan 01: Code graph bugs

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- A failed driver of a multi-manifest repo is in the summary, with its
  reason.
- The code graph has no symbols from documents outside the indexed repo,
  for example the Go build cache.

## Condition at this time (2026-11-03)

The test projects in the `engraph-test-repos` directory showed these bugs
(refer to the `README.md` of that directory).

A corrected bug is removed from this plan, and the numbers of the other bugs do
not change. Thus this list does not start at bug 1 (refer to "Plan management"
at the end).

| Bug | Effect |
|---|---|
| 16. A failed driver of a multi-manifest repo is not in the summary | `indexed … driver=scip-typescript`, as if the repo had no Python; the reason only in a `WARN` line |
| 20. Go test runs put symbols of the Go build cache in the code graph | `main`, `init`, `tests`, `benchmarks`, … with a `file_path` of `../../../../../../home/<user>/.cache/go-build/…` |

### Bug 16: a failed driver of a multi-manifest repo is not in the summary

**Found on 2026-11-14**, in the review of part 13.

`index_repo` runs every driver whose manifest the repo root has. When one
driver fails, the run continues with the other drivers, and the reason goes
to a `WARN` line only (`crates/engraph-codegraph/src/index.rs:402-406`):

```rust
tracing::warn!(
    driver = drv.name(),
    error = %e,
    "driver failed; continuing with remaining drivers for this repo",
);
```

`IndexStats` has no field for a failed driver, and `driver_name` has only
the drivers that succeeded: one name, or `multi` (`index.rs:429-433`). Thus,
when scip-python fails and scip-typescript succeeds, the summary is
`indexed … driver=scip-typescript` — the same as for a repo with no Python —
and a detached SessionStart refresh drops a language from the graph with no
sign in the summary. The Bazel symbol pass reports each language on its own
`symbol[…]` line; the driver pass reports only the survivors. When all
drivers fail, the error `all N driver(s) failed` has no reasons
(`index.rs:409-414`).

### Bug 20: Go test runs put symbols of the Go build cache in the code graph

**Found on 2026-11-16**, in the live Bazel validation of plan 03.

When scip-go indexes tests, it also indexes the test main package that
`go test` generates. The source of that package is a file in the Go build
cache, and scip-go writes its path relative to the module root. The path goes
out of the repo. The loader stores `Document.relative_path` as `file_path`
with no check (`crates/engraph-codegraph/src/scip_loader.rs:395-400`):

```rust
let (file_path, line_range) = match def_loc.get(&s.symbol) {
    Some((start, end)) => (
        Some(doc.relative_path.clone()),
        Some(format!("{}:{}", start + 1, end + 1)),
    ),
    None => (Some(doc.relative_path.clone()), None),
};
```

The Go driver always indexes tests, thus each Go repo with a `_test.go` file
has these symbols. The Bazel Go pass has them with `--go-tests`. On a copy of
`go-tutorial-go-bazel` with a `go.mod`, `engraph index --lang scip-go` gave
these entities:

```
main         ../../../../../../home/derekdshaw/.cache/go-build/16/16cea69f…-d
init         ../../../../../../home/derekdshaw/.cache/go-build/16/16cea69f…-d
tests        ../../../../../../home/derekdshaw/.cache/go-build/16/16cea69f…-d
benchmarks   ../../../../../../home/derekdshaw/.cache/go-build/16/16cea69f…-d
examples     ../../../../../../home/derekdshaw/.cache/go-build/16/16cea69f…-d
fuzzTargets  ../../../../../../home/derekdshaw/.cache/go-build/16/16cea69f…-d
m            ../../../../../../home/derekdshaw/.cache/go-build/16/16cea69f…-d
```

The repo has two `main` symbols in `print_fortune.go` and two more at the
cache path. The cache path is not in the repo, and it contains the home
directory of the user and a hash of the build.

## Steps

Do each part on its own branch. The parts do not depend on each other. Each
part has correction steps and validation steps. Do the validation steps after
the correction steps, before the pull request.

### Preparation for a validation

The validation steps use the test projects in the `engraph-test-repos`
directory. Do this procedure for each project that a validation step names.
In these steps:

- `<project>` is the directory name of the project, for example `fd-rust`.
- `$TEST_REPOS` is the path of the `engraph-test-repos` directory.
- `$WORK` is a work directory for the copies and the databases.

1. In the engraph repo, on the branch of the part, build the binary:
   `cargo build -p engraph-cli`
2. Put the binary first in `PATH`: `export PATH="$PWD/target/debug:$PATH"`
3. Use a new database for the project:
   `export ENGRAPH_DB_PATH="$WORK/<project>.db" && rm -f "$ENGRAPH_DB_PATH"`
4. Make a new copy of the project:
   `mkdir -p "$WORK" && rm -rf "$WORK/<project>" && cp -r "$TEST_REPOS/<project>" "$WORK/"`
5. Go to the copy: `cd "$WORK/<project>"`
6. Make a git baseline:
   `git init -q && git add -A && git -c user.name=t -c user.email=t@t commit -qm baseline`
7. Do the setup that the section of the project in `$TEST_REPOS/README.md`
   gives. Some projects have no setup.
8. Define the query function `q`. It shows the result of an SQL query on the
   database:

   ```sh
   q() { python3 -c 'import sqlite3,os,sys; [print(*r) for r in sqlite3.connect(os.environ["ENGRAPH_DB_PATH"]).execute(sys.argv[1])]' "$1"; }
   ```

Some validation steps record a value with the `main` binary before the change.
For these steps, do steps 1 and 2 on the `main` branch, and then steps 3 to 8.

### Part 16: the failed drivers of a multi-manifest repo (bug 16)

1. Add the failed drivers to `IndexStats`: the driver name and the failure
   message. The message already carries the `failure_tail` of the indexer.
2. Print one `driver[<name>]: failed: …` line per failed driver under the
   repo summary, the tail indented — the shape of `format_symbol_langs`
   (part 11). `--workspace` and the single-repo path use the same printer.
3. Put the per-driver reasons in the `all N driver(s) failed` error.
4. Write an integration test: a repo with `pyproject.toml` and
   `package.json` + `tsconfig.json`, and fake `scip-python` and
   `scip-typescript` binaries first in the `PATH` of the spawned `engraph`
   process. One writes a small SCIP index; the other exits 2 with text on
   stderr. The summary must name the failed driver and show the text.

Validation of part 16:

1. The integration test covers the behavior. No test project has two
   manifests with one broken toolchain; a manual check can copy
   `hono-typescript`, add a `pyproject.toml`, and put a failing
   `scip-python` shim first in `PATH`.

### Part 20: documents outside the repo (bug 20)

1. Find out if scip-go writes other documents with a path that goes out of
   the module root, for example cgo output or `//go:embed` files.
2. In the loader, do not load a document whose `relative_path` goes out of
   the repo: a path that starts with `../`, or an absolute path. Do this
   before pass 1, so that its symbols are not current symbols and the prune
   removes the rows of an earlier run. Record the number of documents that
   the loader did not load in a `debug` line.
3. Write a unit test: a SCIP index with a document `src/a.go` and a document
   `../../cache/go-build/x-d`. Only the symbols of `src/a.go` are loaded.
4. Write a test that loads the index twice: first with the outside document,
   then without it. After the second load, no row has the outside path.

Validation of part 20:

1. Make a copy of `go-tutorial-go-bazel` (see "Preparation for a
   validation"), and add the `go.mod` file
   `module github.com/bazelbuild/examples/go-tutorial/stage3` with `go 1.21`.
2. Do `engraph index --lang scip-go .` with the `main` binary. Record the
   number of rows: `q "SELECT COUNT(*) FROM entities WHERE file_path LIKE '../%'"`.
   The number is more than 0.
3. Do the same index with the binary of the part. The number is 0, and
   `TestGet` is in `fortune/fortune_test.go`.
4. Do `engraph index --bazel-symbols --go-tests .` with the binary of the
   part. The number is 0.

## Tests

- A failed driver of a multi-manifest repo is named in the summary with its
  reason (part 16).
- The loader does not load a document whose path goes out of the repo, and a
  load removes the rows of such a document from an earlier load (part 20).

## Risks

None. The parts that had a risk are complete, and their risks are recorded
in their pull requests.

## Dependencies

None.

## Effort

Part 16 adds a field to
`IndexStats`, one printer, and an integration test.

Part 20 adds one filter in the loader and two tests.

## Plan management

This plan has independent parts, and each part is removed when it is complete.
These tasks replace rule 5 of the general information:

1. When the validation of a part passes, remove its bug and its part from this
   plan: the bug section, the part section, the table row, and the bullets in
   "Result", "Tests", and "Risks". Record the validation data in the pull
   request. The numbers of the other bugs do not change.
2. When all parts that are not optional are complete, remove item 1 from
   `docs/ROADMAP.md`, describe the new functions in `DETAILS.md`, and move this
   plan to `docs/plans/completed/`. An optional part that is not done moves
   with the plan.
