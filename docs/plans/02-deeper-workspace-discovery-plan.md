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
   mode obeys the same rules when it scans the direct children. Complete:
   - Discovery keeps its own traversal. For each child directory, it asks an
     `ignore::IncrementalIgnore` matcher (from `WalkBuilder::build_matchers`)
     if the directory is ignored. The matcher uses the default rules of the
     crate: `.gitignore` inside a git repo, `.ignore`, `.git/info/exclude`,
     and the global git excludes.
   - An ignored directory that contains `.git` is in the result, because it
     is a separate repo. Discovery examines it with a new matcher, so the
     rules of the parent do not apply in it. An ignored directory below an
     ignored directory is not found.
   - The `ignore` walker (`WalkBuilder::build`) is not used. Its filter does
     not see ignored directories, so it cannot find a separate repo in them.
     It was also slower (2026-11-15, release build, warm runs): discovery
     with the walker took 33 to 40 percent more time than discovery without
     ignore rules. Discovery with the matcher takes 8 to 15 percent more time.
     When ignore rules remove subtrees, it takes less time.
   - The plain `--workspace` mode does not skip hidden children, as before.
     See step 12.
2. Keep the list in `is_pruned_dir` together with the `.gitignore` rules.
   Complete with step 1: `walk_recursive` applies `is_pruned_dir` before it
   asks the matcher.
3. Add the `--max-depth` flag, with a default value of 16. The flag requires
   `--recursive`. Complete: the root is depth 0, and discovery examines
   the directories to `--max-depth`. `plan_workspace` and `index_workspace`
   get a `Discovery` value (`Children`, or `Recursive { max_depth }`) in
   place of the `recursive` flag.
4. Add the `--include <glob>` flag. Each glob is relative to the workspace
   root. The flag operates with `--recursive` and with plain `--workspace`.
   Complete:
   - The flag filters the result of discovery. It does not change the walk,
     so it does not include an ignored directory or a project that
     same-language suppression hides.
   - A glob matches the path of a project, `/`-separated, with `.` for the
     root. `*` stays within one level and `**` crosses levels. A glob matches
     only the path that it names: `services/a/**` does not include
     `services/a`. To include a directory and all directories below it, give
     two globs: `services/a` and `services/a/**`.
   - The flag can be repeated. A project that one glob matches is kept. A
     leading `./` is removed from a glob.
   - A run with `--include` and no matching project fails. A dry run shows
     no project.
5. Add the `--exclude <glob>` flag. Each glob is relative to the workspace
   root. The flag operates with `--recursive` and with plain `--workspace`.
   The glob syntax is the same as for `--include`. If `--include` and
   `--exclude` match the same project, `--exclude` wins, and engraph shows a
   warning that names the project. Complete: the flag filters the result
   of discovery, as `--include` does. Excluding a project that no
   `--include` glob matched gives no warning. A run that the two flags
   leave with no project fails.
6. In the `--dry-run` output, show each directory that the discovery found.
   `walk_recursive` must return a record for each directory that it examined,
   not only the recorded directories.
7. For each directory, show the cause for which the discovery included it or
   did not include it.

   Steps 6 and 7 are complete, in one commit:
   - The report lists each directory that explains the result, not each
     directory that discovery examined: a full list has tens of thousands of
     lines on a large tree. It lists each directory with a manifest, and each
     place where discovery stopped (ignored, beyond `--max-depth`, a Bazel
     root). It does not list a plain directory below the root or a directory
     pruned by name. Plain `--workspace` lists every direct child.
   - A member names the project that covers each of its languages. The walk
     keeps a map from each language to the nearest ancestor project that
     covers it.
   - The dry run prints the report as a table of paths relative to the
     workspace root and causes, before the list of discovered projects. The
     report does not say which ignore file names an ignored directory,
     because the `ignore` matcher does not give this.
   - The report is the result of discovery for the real run too, so a dry
     run and a real run cannot differ. The speed of discovery did not change
     (2026-11-15, the benchmark of step 1).
   - Plain `--workspace` lists hidden children, for example `.git`. See
     step 12.
8. Find out if a git repo in a subdirectory of a different git repo has a
   different project key. Complete: see
   [Project keys of nested git repos](#project-keys-of-nested-git-repos).
9. Write a test for this condition. The test must examine a nested git repo
   and a git submodule. Complete: `a_nested_git_repo_has_its_own_project_key`
   (`engraph-cli/src/divergence.rs`) examines the nested repo. The submodule
   test is part of step 10, because it shows the correct key, which the code
   gives only after the correction of step 10.
10. Correct the project key of a git submodule. The key is the path of the
    submodule checkout. `in_linked_worktree` is false for a submodule
    checkout. A linked worktree of a normal repo must keep the key of its
    main worktree. Make a separate commit for this step. Complete: when the
    first entry of `git worktree list --porcelain` is not `bare` and has no
    `.git` entry, it is a submodule's gitdir, and `canonical_repo_key` uses
    the checkout's own root. A bare repo's directory stays the shared key of
    its worktrees.
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

    Complete:
    - A member of the parent is not recorded, so a directory below a member
      checks the same parent.
    - Cargo: a line scan finds a `[workspace]` or `[workspace.*]` table
      header. engraph has no TOML parser, and this is sufficient.
    - TypeScript: `pnpm-workspace.yaml` is read with a line scan of the
      block form of `packages` (no YAML parser). `package.json` `workspaces`
      can be an array or an object with `packages`. A glob that starts with
      `!` excludes. The globs use the rules of `--include`, relative to the
      parent.
    - TypeScript: the parent's workspace counts only if scip-typescript can
      index it from the parent: with `pnpm-workspace.yaml`
      (`--pnpm-workspaces`), or with `package.json` `workspaces` and a
      `yarn.lock` (`--yarn-workspaces`). npm has no such flag, so in an npm
      workspace each package is its own project. Step 11b gives the driver
      these flags.
11b. Make the scip-typescript driver index the workspace members of step 11.
    At this time, the driver runs `scip-typescript index --output <file>`,
    which indexes only the projects of the root `tsconfig.json`.
    - If the directory has `pnpm-workspace.yaml`, add `--pnpm-workspaces`.
      scip-typescript then runs `pnpm ls -r`.
    - Else, if `package.json` has `workspaces` and the directory has
      `yarn.lock`, add `--yarn-workspaces`. scip-typescript then runs
      `yarn workspaces info` or `yarn workspaces list`.
    - Use the same conditions as `is_member`, so that a package is a member
      only if the run of its parent indexes it.
    - Make a separate commit for this step.
12. Find out if the plain `--workspace` mode must skip hidden children, as
    `--recursive` does. At this time, `--recursive` skips them
    (`is_pruned_dir`) and `--workspace` does not.

## Tests

- The discovery does not include a directory that `.gitignore` names.
  It includes an ignored directory that contains `.git`, and the rules of
  the parent do not apply in that directory.
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
