# Plan 10: Separate code graphs for worktrees

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- An agent in a worktree sees the code graph of that worktree: the base graph
  together with the changes of that worktree. It does not see the changes of
  other worktrees.
- When the base branch changes (for example, after a pull request merges),
  engraph updates the base graph and the overlay of each worktree.
- A worktree that has a feature branch, also the main worktree, does not change
  the base graph.

## Condition at this time (2026-10-30)

This condition includes the `add-auto-indexing` branch.

- The code graph is one graph for each repo. The project key is the path of the
  main worktree (`divergence::index_scope`).
- Each symbol has one row. `entities.id` is the SCIP symbol id, and it is the
  primary key (`id TEXT PRIMARY KEY`). Thus, the graph can keep only one
  version of a symbol.
- `index_state` has one row for each `(project, subtree)`, with the commit and
  the index mode (`source`, `lang`, `bazel_symbols`, schema v11). It has no
  worktree or branch.
- The automatic refresh (`engraph index --if-changed`) operates only from the
  main worktree (`divergence::in_linked_worktree`). In a linked worktree, the
  per-file divergence check operates: a grep for a symbol in a changed file
  continues, and `engraph subgraph` shows a "Changed since the index" note.
  The result is correct, but engraph saves no tokens for the changed files.
- If the main worktree has a feature branch, the automatic refresh indexes
  that branch into the shared graph.
- In a bare-repo layout (`git clone --bare` and `git worktree add`), each
  worktree is a linked worktree. Thus, no automatic refresh occurs.
- The standard Go driver gives no `--module-version` to scip-go. Thus, each Go
  symbol id contains the commit hash, and no Go file has the same ids in two
  commits ([plan 05](05-stable-symbol-ids-plan.md), part 2, step 1).

## Design

### Base graph

The base graph is a full index of the base branch at a specified commit. All
worktrees of the repo use it.

### Overlay

Each checkout has an overlay. The main worktree also has one. The overlay
contains only the files whose content is different from the base commit:

- the files from `git diff --name-only <base-commit> -- .` against the working
  tree,
- the untracked files that are not ignored
  (`git ls-files --others --exclude-standard`),
- the deleted files. These have no rows, but they hide the rows of the base
  graph.

The difference goes in the two directions. A file that the worktree changed is
in the overlay. A file that the base branch changed after the worktree started
is also in the overlay, with the old content of the worktree. Thus, each
worktree sees its own code.

### Lookups

A lookup from worktree W uses these rows:

- the overlay rows of W for the files in the overlay of W,
- the base rows for all other files,
- the base relations whose source file is not in the overlay of W,
- no relation to a symbol that the view of W does not have (for example, a
  symbol that W removed or renamed).

This rule is correct because an unchanged file gives the same symbols and the
same relations in the base and in the worktree. It is correct only if symbol
ids are stable between commits (refer to "Dependencies").

### Storage

Add new tables: `overlay_entities`, `overlay_relations`, and `overlay_files`,
each with a `worktree` column. Do not change the base tables or the base
loader. One view function combines the base and the overlay for
`subgraph_for`, the grep redirect, and the PostToolUse(Read) hook.

Do not change `entities` to a composite key. That change must rebuild the
tables that `relations` refers to, and it gives the same result with more risk.

### Overlay index runs

SCIP indexers analyze a project, not a file. An index run for an overlay must
analyze the changed code and its dependencies. It does not have to write rows
for unchanged files.

- Index only the packages, crates, or modules that contain changed files. For
  Go, give package patterns to scip-go (`PackagePatterns`, scip-go v0.2.7). For
  Rust, index the crates that contain changed files.
- Load only the documents of the changed files into the overlay.
- Start the run in the background. The `--if-changed` refresh and its lock
  already do this.

A fast syntax pass (for example, tree-sitter) for changed files is not part of
this plan. Add it only if telemetry shows that overlay index runs are too slow
(the `auto` index events record the time of each run).

### Refresh

- The base graph changes when the base branch ref moves.
- After a change to the base graph, compute the overlays again, because the set
  of different files changes.
- Remove the overlay of a worktree that `git worktree list` does not show.

## Decisions to make first

1. **The base.** Two possible bases:
   - The default branch ref. engraph indexes it from a detached checkout that
     it manages in its data directory. The base then follows merges when the
     local ref moves, also when the main worktree has a feature branch. This
     needs a second checkout of the repo on disk, and the build artifacts of
     some indexers (for example, rust-analyzer).
   - The checkout of the main worktree. This needs no second checkout, but a
     feature branch in the main worktree becomes the base.

   The recommendation is the default branch ref.
2. **Fetch.** engraph does not do `git fetch`. The base changes when the local
   ref moves. The user or other tools fetch.

## Steps

### Part 1: prerequisites

1. Do step 1 of part 2 of [plan 05](05-stable-symbol-ids-plan.md) (a fixed
   `--module-version` for the standard Go driver).
2. Find out if other indexers give ids that change between commits. Do this for
   rust-analyzer, scip-python, scip-typescript, and scip-java. For Rust, find
   out what occurs when a branch changes the version of a crate.
3. Record the decision about the base in the pull request.

### Part 2: base graph from the base branch

4. Record the base of each project: the branch ref and the commit of the last
   base index run.
5. If the base is the default branch ref, make a detached checkout of the ref
   in the engraph data directory. Index the base from this checkout.
6. In the SessionStart hook, find out if the base ref moved after the last base
   index run. If it moved, start a base index run in the background.
7. Replace the main-worktree gate of the automatic refresh with steps 5 and 6.

### Part 3: overlays

8. Add the tables `overlay_entities`, `overlay_relations`, and
   `overlay_files`.
9. Add a function that finds the files of a checkout that are different from
   the base commit: changed, untracked, and deleted files.
10. Add an overlay index run: index the packages that contain these files, and
    load only the documents of these files into the overlay of the checkout.
11. Record the base commit and the checkout state (`git stash create`, or
    `HEAD`) of each overlay.
12. In the SessionStart hook, start an overlay index run in the background if
    the checkout changed after its overlay, or if the base commit changed.
13. Add the view function. Use it in `subgraph_for`, in the grep redirect, and
    in the PostToolUse(Read) hook.
14. When the view function operates, remove the per-file divergence check from
    the grep redirect and from `engraph subgraph`. The view of the worktree
    replaces it.
15. Remove the overlays of worktrees that `git worktree list` does not show.

## Tests

- A symbol that worktree A adds is in the view of A, and it is not in the view
  of worktree B or of the base.
- A symbol that worktree A removes is not in the view of A, and the callers of
  that symbol in the base show no relation to it in the view of A.
- A function that the base branch changes after worktree A started shows the
  old version in the view of A.
- After a commit on the base branch, the base index run starts, and the overlay
  of each worktree is computed again.
- A worktree with no changes has an empty overlay, and its view is the base.
- The overlay of a removed worktree is deleted.
- With the base as the default branch ref, a feature branch in the main
  worktree does not change the base graph.

## Risks

- An overlay index run analyzes the dependencies of the changed packages. For
  Rust, this is a full analysis of each changed crate. For Java and Bazel
  repos, it can take minutes. During that time, the overlay is old or missing,
  and the agent uses grep for the changed files.
- A detached checkout of the base doubles the disk use of the repo, and some
  indexers add build artifacts.
- If symbol ids change between commits, the overlay must contain all files of
  the affected language. Then it gives no saving.

## Dependencies

- The `add-auto-indexing` branch (the background refresh and the index lock).
- [Plan 05](05-stable-symbol-ids-plan.md), part 2, step 1 (stable Go symbol
  ids).

## Effort

Not estimated. Part 3 is the largest part.
