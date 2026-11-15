# Plan 02: Deeper workspace discovery

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

You can select the projects that `engraph index --recursive` finds. The
discovery obeys `.gitignore` files. Go modules below a different Go module
are in the result. Each language has its own rule for the projects below a
project of the same language. A git submodule has its own project key, and
the background index refresh does not skip it.

## Condition at this time (2026-11-15)

`walk_recursive` (`engraph-codegraph/src/index.rs`) examines the directory
tree to a maximum depth of 16 (`RECURSIVE_MAX_DEPTH`). It does not examine the
directories in `is_pruned_dir`: hidden directories (this includes `.venv`),
`node_modules`, `target`, `vendor`, `__pycache__`, `venv`, and `testdata`. It
does not read `.gitignore` files. You cannot include or exclude specified
directories. The `ignore` crate is not a dependency.

`walk_recursive` returns only the directories that it records.
`plan_workspace` and `index_workspace` both call it. The `--dry-run` output
(`print_workspace_plan` in `engraph-cli/src/output.rs`) shows only the
recorded directories, without a cause.

If a directory above a project has the same language, the discovery does not
record the project. This rule applies to all languages. It is correct for
Cargo workspaces. It is not correct for Go: a Go module below a different Go
module is not in the result.

The unit test `recursive_suppresses_same_language_members` puts member crates
below a root `Cargo.toml` that is empty. The test has no `[workspace]` section.

### Project keys of nested git repos

The work found these results for step 8 (git 2.53.0):

- A git repo in a subdirectory of a different git repo (not a submodule) has
  its own project key: the path of its checkout. Its subtree is empty. This
  is correct.
- A git submodule has an incorrect project key. `canonical_repo_key`
  (`engraph-cli/src/main.rs`) uses the first line of `git worktree list
  --porcelain`. In a submodule, this line gives the git directory
  (`<parent>/.git/modules/<name>`), not the checkout (`<parent>/<name>`).
  Thus `in_linked_worktree` (`engraph-cli/src/divergence.rs`) is true for a
  submodule checkout, because the key is not equal to
  `git rev-parse --show-toplevel`. `engraph index --if-changed` and the
  SessionStart refresh skip each submodule. Lookups operate, because each
  function uses the same incorrect key.

### Same-language projects that overlap

scip-python indexes all `.py` files below its working directory, and it uses
the directory name as the package name. If the discovery records a Python
project below a different Python project, the index run of the parent loads
the files of the child with the package name of the parent. The index run of
the child loads the same files with a different package name. Thus the same
code gets two sets of symbol ids. The load of the parent also deletes the
relations of all files below its subtree (`IN_SCOPE` in `scip_loader.rs`),
which includes the files of the child. The result changes with the sequence
of the runs.

Go does not have this problem: Go tools do not include a nested module in the
module above it.

## Steps

1. In `walk_recursive` and in `discover_workspace_repos`, use the `ignore`
   crate to obey `.gitignore` and `.ignore` files. The plain `--workspace`
   mode obeys the same rules when it scans the direct children.
2. Keep the list in `is_pruned_dir` together with the `.gitignore` rules.
3. Add the `--max-depth` flag, with a default value of 16. The flag requires
   `--recursive`.
4. Add the `--include <glob>` flag. Each glob is relative to the workspace
   root. The flag operates with `--recursive` and with plain `--workspace`.
5. Add the `--exclude <glob>` flag. Each glob is relative to the workspace
   root. The flag operates with `--recursive` and with plain `--workspace`.
6. In the `--dry-run` output, show each directory that the discovery found.
   `walk_recursive` must return a record for each directory that it examined,
   not only the recorded directories.
7. For each directory, show the cause for which the discovery included it or
   did not include it.
8. Find out if a git repo in a subdirectory of a different git repo has a
   different project key. Complete: see
   [Project keys of nested git repos](#project-keys-of-nested-git-repos).
9. Write a test for this condition. The test must examine a nested git repo
   and a git submodule.
10. Correct the project key of a git submodule. The key is the path of the
    submodule checkout. `in_linked_worktree` is false for a submodule
    checkout. A linked worktree of a normal repo must keep the key of its
    main worktree. Make a separate commit for this step.
11. Replace the rule that makes a directory with the same language a member.
    Use a rule for each language. "The parent" is the nearest directory above
    that the discovery recorded for the same language, not the workspace root.
    - Cargo: a directory is a member only if the `Cargo.toml` of the parent
      has a `[workspace]` section. The rule does not read the `members` and
      `exclude` lists.
    - TypeScript: a directory is a member only if the parent lists it in the
      `workspaces` of its `package.json` or in the `packages` of its
      `pnpm-workspace.yaml`.
    - Go: a directory with a `go.mod` in it is not a member.
    - Python: keep the current rule. A Python project below a Python project
      is a member, because scip-python indexes the files of the child in the
      run of the parent (see
      [Same-language projects that overlap](#same-language-projects-that-overlap)).
    - Java: keep the current rule. scip-java indexes Maven and Gradle
      multi-module builds from the root.

## Tests

- The discovery does not include a directory that `.gitignore` names.
- `--exclude` removes a directory from the result. `--include` limits the
  result to the directories that it names.
- `--max-depth 2` does not include a project at depth 3. `--max-depth`
  without `--recursive` is an error.
- With plain `--workspace`, the discovery does not include a child that
  `.gitignore` names, and `--include` and `--exclude` filter the children.
- The `--dry-run` output shows a cause for an included directory and for an
  excluded directory.
- A nested git repo has its own project key. A git submodule has the path of
  its checkout as its project key, and `in_linked_worktree` is false for it.
- A Cargo crate below a parent with `[workspace]` is a member. A Cargo crate
  below a parent crate without `[workspace]` is not a member. Change the setup
  of `recursive_suppresses_same_language_members` to add `[workspace]`.
- A TypeScript package that the `workspaces` of the parent `package.json`
  lists is a member. A package that `pnpm-workspace.yaml` lists is a member. A
  package that neither file lists is not a member.
- A Go module below a parent Go module is not a member.
- A Python project below a parent Python project is a member.

## Risks

The new member rules can increase the number of index runs in a workspace. Use
`--dry-run` to see the result before you do a full index run.

A TypeScript package that is not a workspace member can be in the `include`
of the parent `tsconfig.json`. Then scip-typescript indexes its files in the
runs of the parent and of the child, as for Python. Examine the result on a
real repo before you complete step 11.

## Dependencies

None. [Plan 03](03-all-go-modules-plan.md) uses the member rules of this plan.
