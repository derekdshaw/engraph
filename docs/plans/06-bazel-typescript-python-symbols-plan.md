# Plan 06: Bazel symbols for TypeScript and Python

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

The symbol-level Bazel pass (`--bazel-symbols`) indexes TypeScript and Python
code.

## Condition at this time (2026-10-30)

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

## Reason to wait

Do this work when a real repo shows the problem.

## Dependencies

None.
