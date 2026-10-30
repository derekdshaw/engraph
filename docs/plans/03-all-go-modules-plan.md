# Plan 03: All Go modules in the code graph

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

engraph indexes all Go modules of a repo, with correct relations between the
modules. This is also true in Bazel repos.

## Condition at this time (2026-10-30)

- The Go driver finds a module only when `go.mod` is in the indexed directory.
  For a repo with more than one Go module, a `--recursive` index run is
  necessary. But that run does not include a Go module below a different Go
  module ([plan 02](02-deeper-workspace-discovery-plan.md)).
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

## Steps

1. Write a test with a root Go module and a second Go module in `tools/`.
2. Make sure that `engraph index --recursive` indexes the two modules. [Plan 02](02-deeper-workspace-discovery-plan.md),
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

## Tests

- `engraph index --recursive` indexes the root module and the module in
  `tools/`.
- A relation from module A to module B points to the correct symbol.
- In the Bazel Go pass with `--go-tests`, a function in a `_test.go` file is in
  the code graph. Without `--go-tests`, it is not in the code graph.

## Risks

An index run for each Go module can use much time in a large repo. Use the
`--include` flag of [plan 02](02-deeper-workspace-discovery-plan.md) to select modules.

## Dependencies

[Plan 02](02-deeper-workspace-discovery-plan.md) (the member rule for Go).
