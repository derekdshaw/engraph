# Plan 02: Deeper workspace discovery

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

You can select the projects that `engraph index --recursive` finds. The
discovery obeys `.gitignore` files. Go modules and Python projects below a
project of the same language are in the result.

## Condition at this time (2026-10-30)

`walk_recursive` examines the directory tree to a maximum depth of 16
(`RECURSIVE_MAX_DEPTH`). It does not examine the directories in
`is_pruned_dir`: hidden directories, `node_modules`, `target`, `vendor`,
`__pycache__`, `venv`, and `testdata`. It does not read `.gitignore` files. You
cannot include or exclude specified directories.

If a directory above a project has the same language, the discovery does not
record the project. This rule is correct for Cargo workspaces. It is not correct
for Go: a Go module below a different Go module is not in the result. The rule
is possibly not correct for TypeScript packages and Python projects.

## Steps

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

## Tests

- The discovery does not include a directory that `.gitignore` names.
- `--exclude` removes a directory from the result. `--include` limits the
  result to the directories that it names.
- `--max-depth 2` does not include a project at depth 3.
- A Cargo crate below a root `[workspace]` is a member. A Cargo crate below a
  root crate without `[workspace]` is not a member.
- A Go module below a root Go module is not a member.

## Risks

The new member rules can increase the number of index runs in a workspace. Use
`--dry-run` to see the result before you do a full index run.

## Dependencies

None. [Plan 03](03-all-go-modules-plan.md) uses the member rules of this plan.
