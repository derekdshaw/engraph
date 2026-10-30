# Plan 01: Automatic index runs

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

When a session starts, engraph updates the code graph of the project if the
code graph is old.

## Condition at this time (2026-10-30)

The `engraph index` command is manual. The SessionStart hook does not start an
index run.

The grep redirect, the PostToolUse(Read) symbol list, and `engraph subgraph` use
the code graph. If the code graph does not exist or is old, these features save
fewer tokens or no tokens. For a file that changed after the index commit, the
grep redirect lets the grep continue, and `engraph subgraph` shows a "Changed
since the index" note. No hook tells Claude or the user to do an index run.

The `index_state` table (schema v10) records the commit of each indexed subtree
of a project.

## Requirements

- The hook must find out if the code graph is old. The commit in `index_state`
  is a possible source for this check.
- A slow index run (for example, a cold scip-java build) must not start in each
  session.

## Dependencies

None.
