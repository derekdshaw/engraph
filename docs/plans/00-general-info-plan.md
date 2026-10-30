# Plan 00: General information for all plans

This file gives the rules and the sequence for the plans in `docs/plans/`. Each
numbered plan is for one item in [`docs/ROADMAP.md`](../ROADMAP.md).
[`general-perf-improvements.md`](general-perf-improvements.md) is a list of
small performance improvements that are not roadmap items. Read this file
before you use a plan.

## Language

The plans and the roadmap use ASD-STE100 Simplified Technical English (issue 9).
Code names, file names, and commands are technical nouns. They show in `code`
font.

## Format of a plan

Each plan has these sections. Some plans do not have all of them.

- **Result**: what is true when the work is complete.
- **Condition at this time**: what the code does at the specified date.
- **Requirements**: conditions that the work must satisfy, when the plan does
  not give steps.
- **Steps**: the work, in sequence.
- **Tests**: the tests that the work must add.
- **Risks**: problems that the work can cause.
- **Reason to wait**: why the work is not started at this time.
- **Dependencies**: the plans that must be complete before this plan.
- **Effort**: the expected time.

Each plan gives its condition at a specified date. Before you start a plan, make
sure that the condition is still correct.

## Rules for the work

1. Do the work for each plan on a new branch.
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
      `README.md`, `docs/ROADMAP.md`, and the plan.
4. Some steps start with "Find out". For these steps, record the result in the
   pull request. Then do the steps that follow.
5. When the work of a plan is complete, remove the item from
   `docs/ROADMAP.md`, and describe the new function in `DETAILS.md`.

## Sequence of work

The roadmap gives the items in the sequence of their expected effect. This is
not the sequence of the work. Some plans must wait for other plans:

1. Start steps 1 to 4 of [plan 04](04-file-read-measurement-plan.md) (measurement of file reads) at the start of the
   work. These steps must collect data for two weeks. You can do other plans
   at the same time.
2. Do [plan 02](02-deeper-workspace-discovery-plan.md) (deeper workspace discovery) before [plan 03](03-all-go-modules-plan.md) (all Go modules).
   [Plan 03](03-all-go-modules-plan.md) uses the member rules of [plan 02](02-deeper-workspace-discovery-plan.md).
3. In [plan 05](05-stable-symbol-ids-plan.md) (stable symbol ids), do part 1 (test suite) before part 2
   (normalization). Part 2 changes symbol ids. The test suite shows if a change
   to the ids causes problems. Step 1 of part 2 is a bug fix. You can do it
   before part 1.
4. You can do the other plans in any sequence.
