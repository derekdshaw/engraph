# Plan 03: All Go modules in the code graph

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

engraph indexes all Go modules of a repo, with correct relations between the
modules. This is also true in Bazel repos.

## Condition at this time (2026-11-16)

- The Go driver finds a module when `go.mod` or `go.work` is in the indexed
  directory. For a repo with more than one Go module and no `go.work`, a
  `--recursive` index run is necessary. That run includes a Go module below a
  different Go module ([plan 02](completed/02-deeper-workspace-discovery-plan.md), step 10).
- With a `go.work`, the Go driver gives scip-go one package pattern for each
  `use` directory. One scip-go run indexes all the modules of the `go.work`.
  A module that the `go.work` lists is a member of the `go.work` project in
  `--recursive` discovery. A module below a `go.work` that does not list it
  gets `GOWORK=off`, because Go builds nothing in it.
- A relation from a function in one Go module to a function in a different Go
  module points to the definition. This is true for the native Go pass, with
  or without a `go.work`, and for the Bazel Go pass. The Bazel Go pass indexes
  the modules of a root `go.work` in one scip-go run.
- A vendored copy of a module does not cause a second symbol. Discovery skips
  `vendor` directories, and scip-go does not write documents for vendored
  packages.
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

## Steps

Steps 1 to 8 are complete. Step 3 found that scip-go 0.2.7 indexes all
modules of a `go.work` file in one operation when it gets one `./<dir>/...`
package pattern for each `use` directory. Its default pattern `./...`
indexes only the module of the current directory. Thus step 4 applies, and
step 5 does not.

Step 7 found two bugs:

- In the Bazel Go pass, each module of a `go.work` had its own scip-go run.
  scip-go then gave a reference to a different listed module the version
  `.`, but the definition had the version `0.0.0`. Thus the relation pointed
  to a placeholder. The modules of a root `go.work` now have one run.
- A module below a `go.work` that does not list it got no symbols, in the Go
  driver and in the Bazel Go pass. Its run now sets `GOWORK=off`.

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

## Tests

- `engraph index --recursive` indexes the root module and the module in
  `tools/`.
- `engraph index` of a `go.work` directory without a `go.mod` indexes each
  module that the `go.work` lists.
- A relation from module A to module B points to the correct symbol: with
  a `go.work`, with a `replace` and `--recursive`, and in the Bazel Go pass
  with and without a `go.work`.
- A module below a `go.work` that does not list it gets its symbols.
- A vendored copy of a module does not add a second symbol.
- In the Bazel Go pass with `--go-tests`, a function in a `_test.go` file is in
  the code graph. Without `--go-tests`, it is not in the code graph.

## Risks

An index run for each Go module can use much time in a large repo. Use the
`--include` flag of [plan 02](completed/02-deeper-workspace-discovery-plan.md) to select modules.

In the native Go pass, the version in a Go symbol id is the git commit. When
`--if-changed` indexes module A again after a commit, but not module B, the
relation from A to B points to a placeholder. Step 1 of part 2 of
[plan 05](05-stable-symbol-ids-plan.md) (stable Go symbol ids) removes this
problem.

## Dependencies

[Plan 02](completed/02-deeper-workspace-discovery-plan.md) (the member rule for Go).
