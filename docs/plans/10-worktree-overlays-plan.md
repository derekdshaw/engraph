# Plan 10: Separate code graphs for worktrees

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- An agent in a worktree sees the code graph of that worktree: the base graph
  together with the changes of that worktree. It does not see the changes of
  other worktrees.
- An index run in a worktree indexes only the files that are different from the
  base, not the full repo. engraph makes no second copy of the repo on disk.
- A linked worktree does not change the base graph. It writes only its overlay.
- When the base graph is indexed again (for example, after the user pulls in
  the base checkout), engraph computes the overlay of each worktree again.

## Condition at this time (2026-11-19)

The `add-auto-indexing` branch is in `main`. [Plan 05](completed/05-stable-symbol-ids-plan.md)
is complete, thus the symbol ids are stable (refer to "Findings").

- The code graph is one graph for each repo. The project key is the path of the
  main worktree (`divergence::index_scope`).
- Each symbol has one row. `entities.id` is the SCIP symbol id, and it is the
  primary key (`id TEXT PRIMARY KEY`). Thus, the graph can keep only one
  version of a symbol.
- `index_state` has one row for each `(project, subtree)`, with the commit and
  the index mode (`source`, `lang`, `bazel_symbols`, `go_tags`, `go_tests`,
  `normalization`). It has no worktree or branch.
- The schema is at v19. The overlay tables must have the columns that the base
  tables got after v11: `entities.qualified_name` (v12), `entities.scheme`
  (v13), and `relations.line` (v14).
- The automatic refresh (`engraph index --if-changed`) operates only from the
  main worktree (`divergence::in_linked_worktree`). In a linked worktree, the
  per-file divergence check operates: a grep for a symbol in a changed file
  continues, and `engraph subgraph` shows a "Changed since the index" note.
  The result is correct, but engraph saves no tokens for the changed files.
- If the main worktree has a feature branch, the automatic refresh indexes
  that branch into the shared graph.
- In a bare-repo layout (`git clone --bare` and `git worktree add`), each
  worktree is a linked worktree. Thus, no automatic refresh occurs.

## Design

### Base graph

The base graph is a full index of one checkout of the repo at one commit — the
graph that `engraph index <repo>` writes at this time. All worktrees of the
repo use it. The checkout that has it is the **base checkout** (refer to
decision 1).

engraph makes no checkout of its own. The base is a commit, not a directory:
each worktree of a repo uses one object store, thus a worktree can compare its
files with the base commit (`git diff --name-only <base-commit>`) although that
commit is on another branch and no checkout of it is on disk. `divergence::
diff_names` does this at this time, from a linked worktree, for the "Changed
since the index" note.

The base commit is what `record_index_commit` records at this time: HEAD of
the base checkout, or a `git stash create` commit when its working tree is not
clean (the recorded commit must contain what was indexed, or another worktree
sees the uncommitted edits of the base checkout as its own code). A stash
commit has no ref, thus `git gc` can remove it; engraph accepts this and does
not write to the repo (refer to decision 3). A base commit that git cannot
resolve makes each file count as different (`diff_names`) — one overlay of the
full repo — and the next refresh of the base records a commit that resolves
again.

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
in the overlay. A file that the base checkout changed after the worktree
started is also in the overlay, with the content that the worktree has. Thus,
each worktree sees its own code.

The base checkout also has an overlay, because its working tree moves on after
the commit of its last index run. A refresh of the base makes that overlay
empty again.

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

- The base graph changes only from an index run in the base checkout: the
  automatic refresh (`engraph index --if-changed`) there, or
  `engraph index --full` by hand.
- After a change to the base graph, compute the overlays again, because the set
  of different files changes.
- Remove the overlay of a worktree that `git worktree list` does not show.

## Decisions

1. **The base is a checkout that the user has: the base checkout.** It is the
   main worktree, and for a repo whose main worktree is not a checkout (a bare
   repo with worktrees) it is the checkout of the last full index run, which
   `project_base` records. engraph makes no checkout of its own.

   An earlier version of this plan made the base the default branch ref, from
   a detached checkout in the engraph data directory. That is removed. A second
   working tree of a large repo is too much disk, and it is not necessary: the
   base is a commit, and each worktree can compare its files with a commit of
   another branch without a checkout of it (refer to "Base graph"). The cost of
   this decision is the size of the overlays, not their correctness: if the base
   checkout is on a feature branch, each overlay also contains the files of that
   branch. The result that each worktree sees is still its own code.

2. **A worktree writes an overlay; only the base checkout writes the base.** An
   index run in a linked worktree writes the overlay of that worktree.
   `engraph index --full <repo>` writes the base graph from the checkout it runs
   in, and that checkout becomes the base checkout. Thus a user with a bare-repo
   layout, or a user who wants another checkout to be the base, has one command
   for it, and no index run in a worktree can replace the graph that the other
   worktrees read by accident.

3. **engraph does not write to the repo.** No refs, no checkouts, no commits:
   engraph only reads the repo and records commit hashes in its own database.
   The cost is that `git gc` can remove a `git stash create` commit that the
   overlays compare with. That loss is accepted: it makes one overlay of the
   full repo, the next refresh of the base heals it, and nothing of engraph is
   left in the repo of the user.

## Findings

### Part 1, step 2: the symbol ids do not change between commits (2026-11-19)

The test indexed each fixture in `testdata/fixtures` with the installed
indexer, then indexed it again after a change, and compared the symbol ids.
The result for all five indexers (rust-analyzer, scip-python, scip-go,
scip-typescript, scip-java):

- **A change to one file does not change the ids in the other files.** A new
  function in `ops` and a changed function body gave 0 different ids in the
  files that did not change. The local ids (`local <n>@<tag>:<path>`) of those
  files also did not change.
- **A change to the version of the package does not change the ids.** A bump
  of the version in `Cargo.toml`, `pyproject.toml`, `package.json`, and
  `pom.xml` gave 0 different ids. This includes the Rust question of step 2:
  rule R3 of `scip_loader::normalize_moniker` gives every `rust-analyzer`,
  `scip-typescript`, and `semanticdb` id the version `0.0.0`, the scip-python
  and scip-go drivers pin their versions, thus no manifest version is in an
  id.
- **The same code at another path gives the same ids,** if the index runs use
  one project key (`engraph index --project`).

Thus, the overlay design is correct: the base rows and the overlay rows join,
and no language needs an overlay of all its files.

**One bug that this plan makes operative.** `driver::python_project_name` uses
the name in `pyproject.toml`, but if there is no name, it uses the basename of
the directory of the project. That basename is part of every first-party
scip-python id. If the Python project is at the root of the checkout, the
basename is the name of the checkout directory, which is different in each
worktree. The test made one repo with an unnamed `pyproject.toml` at the root
and added a worktree. The main worktree `engraph/` gave
`scip-python python engraph 0.0.0 …`, the linked worktree `elm/` gave
`scip-python python elm 0.0.0 …`, and all 117 ids were different. The same
repo with the project at `services/api` gave the same ids in both worktrees,
because that path is part of the repo.

This is not a rule for the name of the base checkout. The user gives the name
of each worktree (`git worktree add ../elm`), thus engraph cannot make the
names the same. It is a bug, and it is not operative at this time only because
the index runs from the main worktree alone (`divergence::in_linked_worktree`).
This plan makes more than one checkout of a project, thus the overlay rows of
a worktree would not join the base rows. Step 4 corrects it.

## Steps

### Part 1: prerequisites

1. ~~Do step 1 of part 2 of [plan 05](completed/05-stable-symbol-ids-plan.md) (a
   fixed `--module-version` for the standard Go driver).~~ Complete: plan 05 is
   in `docs/plans/completed/`.
2. ~~Find out if other indexers give ids that change between commits. Do this
   for rust-analyzer, scip-python, scip-typescript, and scip-java. For Rust,
   find out what occurs when a branch changes the version of a crate.~~
   Complete: refer to "Findings".
3. ~~Record the decision about the base in the pull request.~~ Complete: refer
   to "Decisions".
4. ~~Make the scip-python project name the same in each checkout of a repo.~~
   Complete. The fallback applies only when the project is at the root of its
   checkout (`driver::main_worktree_name`): a project below the root keeps its
   own basename, which does not change between checkouts.

   In
   `driver::python_project_name`, if `pyproject.toml` gives no name, use the
   basename of the main worktree of the repo, not the basename of the
   directory. `cli::git_main_worktree` finds the main worktree from any
   worktree, and it keeps the two special conditions (a bare repo, a
   submodule); move it to `engraph_core::utils`, which both crates use, and
   call it from `canonical_repo_key` and from `python_project_name`. If the
   directory is not in a repo, the basename of the directory, as before.

   Two repos with the same main-worktree basename (two checkouts named `api`)
   then have the same package name in their ids. `scip_loader::shared_packages`
   gives a warning for this condition, which other package names also have.

   This is an id rule: bump `scip_loader::NORMALIZATION_VERSION` to 2 and give
   it the rule R1 note. `engraph index` then recommends a new index run of the
   projects whose rows have version 1 (`index::stale_normalization`).

### Part 2: the base checkout

The base graph is the graph that `engraph index` writes at this time, thus this
part adds no index run. It records which checkout the base belongs to, and it
keeps the base commit safe.

4. ~~Record the base of each project in `project_base`: the base checkout and
   the commit of the last base index run.~~ Complete: the table `project_base`
   (schema v19) with the columns `base_worktree` and `base_commit`, and the
   functions `index::record_base` and `index::base_state`.
   `utils::default_branch_ref`, which the earlier decision 1 needed, is
   removed.
5. ~~Give the base checkout a fallback: a project with no row has the main
   worktree as its base checkout, thus a repo that engraph indexed before this
   plan needs no new index run to get a base. A bare repo has no main worktree;
   such a project has no base until a full index run records one.~~ Complete:
   `index::base_checkout` gives the recorded checkout, else the project key
   when that key is the root of a working tree, else nothing. The fallback is
   a question of ownership only, thus it is a function beside `base_state` and
   not a part of it: a project with no row has no base commit to give, but its
   `index_state` rows have the commit of each subtree, which is what the
   overlays of part 3 compare with.
6. Removed. An earlier version of this step wrote the base commit to a ref
   (`refs/engraph/base/…`), so `git gc` keeps it. Decision 3 removes it:
   engraph does not write to the repo. The recorded commit in `index_state` is
   the base, and a commit that git cannot resolve heals at the next refresh of
   the base.
7. Add `engraph index --full`: a full index run that writes the base graph from
   the checkout it runs in and makes that checkout the base checkout. Without
   it, an index run in a linked worktree writes an overlay (part 3), and the
   automatic refresh in the base checkout refreshes the base as before. This
   step replaces the main-worktree gate of the automatic refresh
   (`divergence::in_linked_worktree` in `main` and in `hooks`).

### Part 3: overlays

8. ~~Add the tables `overlay_entities`, `overlay_relations`, and
   `overlay_files`.~~ Complete (schema v20). `worktree` is the path of the
   checkout, and the row tables key on `(worktree, id)`. `overlay_files` lists
   the files of the overlay with a `deleted` flag: a deleted file has a row
   there and none in `overlay_entities`, thus it hides the base rows.
   `overlay_relations` has no foreign key to `entities`, because an overlay
   edge can end at a symbol that only that worktree has.
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

- A Python project with no name in `pyproject.toml`, at the root of a repo,
  gives the same symbol ids in the main worktree and in a linked worktree with
  another name (step 4).
- A symbol that worktree A adds is in the view of A, and it is not in the view
  of worktree B or of the base.
- A symbol that worktree A removes is not in the view of A, and the callers of
  that symbol in the base show no relation to it in the view of A.
- A function that the base checkout changes after worktree A started shows the
  version of A in the view of A.
- After a refresh of the base graph, the overlay of each worktree is computed
  again.
- A worktree with no changes has an empty overlay, and its view is the base.
- The overlay of a removed worktree is deleted.
- An index run in a linked worktree writes an overlay. It does not change the
  base rows that the other worktrees read.
- `engraph index --full` in a linked worktree writes the base graph and makes
  that checkout the base checkout.
- An overlay of a worktree is computed against a base commit that no branch
  contains (a `git stash create` commit).
- When the base commit does not resolve (after `git gc --prune=now`), the view
  of a worktree is still correct, and the next refresh of the base makes the
  overlays small again.

## Risks

- An overlay index run analyzes the dependencies of the changed packages. For
  Rust, this is a full analysis of each changed crate. For Java and Bazel
  repos, it can take minutes. During that time, the overlay is old or missing,
  and the agent uses grep for the changed files.
- If the base checkout is on a branch that is far from the branches of the
  worktrees, each overlay contains the files of both branches, and the saving
  is small. The user controls this: keep the base checkout on the branch that
  the work starts from, or make another checkout the base with
  `engraph index --full`.
- A base commit that git cannot resolve (`git gc` of a `git stash create`
  commit, or a re-made repo) makes each file count as different, thus one
  overlay of the full repo until the next refresh of the base. Accepted, per
  decision 3. If telemetry shows this occurs often, index the base only when
  its working tree is clean, so the recorded commit is always a reachable
  HEAD.
- If symbol ids change between commits, the overlay must contain all files of
  the affected language. Then it gives no saving. The test of part 1, step 2
  shows that no installed indexer does this (refer to "Findings").

## Dependencies

- The `add-auto-indexing` branch (the background refresh and the index lock).
- [Plan 05](05-stable-symbol-ids-plan.md), part 2, step 1 (stable Go symbol
  ids).

## Effort

Not estimated. Part 3 is the largest part.
