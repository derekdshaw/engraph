# Engraph roadmap

This file gives the features and changes that engraph does not have at this
time. `DETAILS.md` describes the features that engraph has.

The items are in the sequence of their expected effect, from the largest to the
smallest. First, the effect on the tokens that engraph saves. Second, the effect
on the part of a codebase that engraph shows correctly. This sequence is not the
sequence of the work.

Each item has a plan in `docs/plans/`. Read
[the general information for all plans](plans/00-general-info-plan.md) first.
It gives the language, the rules for the work, and the sequence of the work.

The number of an item is the number of its plan. When the work of an item is
complete, the item and its plan are removed. The numbers of the other items do
not change.

Small performance improvements are not items in this file. They are in
[`docs/plans/general-perf-improvements.md`](plans/general-perf-improvements.md).

---

## 2. Deeper workspace discovery

You can select the projects that `engraph index --recursive` finds. The discovery obeys `.gitignore` files. Go modules and Python projects below a project of the same language are in the result.

Plan: [`docs/plans/02-deeper-workspace-discovery-plan.md`](plans/02-deeper-workspace-discovery-plan.md)

## 3. All Go modules in the code graph

engraph indexes all Go modules of a repo, with correct relations between the modules. This is also true in Bazel repos.

Plan: [`docs/plans/03-all-go-modules-plan.md`](plans/03-all-go-modules-plan.md)

## 10. Separate code graphs for worktrees

An agent in a worktree sees the base graph together with the changes of that worktree, and not the changes of other worktrees. A merge to the base branch updates the base graph and each overlay.

Plan: [`docs/plans/10-worktree-overlays-plan.md`](plans/10-worktree-overlays-plan.md)

## 4. Measurement of file reads

Telemetry shows the tokens that file reads use. It also shows the tokens of reads of a file that the session read before. With these data, you can decide if a change to file reads is worth the work.

Plan: [`docs/plans/04-file-read-measurement-plan.md`](plans/04-file-read-measurement-plan.md)

## 5. Stable symbol ids

A symbol id is the same on each computer and after each commit. Two different symbols do not have the same id. A test shows when a change to engraph or to an indexer changes the ids or the relations of known symbols.

Plan: [`docs/plans/05-stable-symbol-ids-plan.md`](plans/05-stable-symbol-ids-plan.md)

## 6. Bazel symbols for TypeScript and Python

The symbol-level Bazel pass (`--bazel-symbols`) indexes TypeScript and Python code.

Plan: [`docs/plans/06-bazel-typescript-python-symbols-plan.md`](plans/06-bazel-typescript-python-symbols-plan.md)

## 7. LLM reranking

A small model puts the top results of hybrid retrieval (RRF) in a better sequence. This function is off by default, because it uses tokens.

Plan: [`docs/plans/07-llm-reranking-plan.md`](plans/07-llm-reranking-plan.md)

## 8. `/resume` and `/save` skills

A `/save` skill calls `engraph save`. A `/resume` skill calls `engraph resume`.

Plan: [`docs/plans/08-resume-save-skills-plan.md`](plans/08-resume-save-skills-plan.md)

## 9. Obsidian export

engraph writes `context_items`, `bugs`, and decisions to an Obsidian vault, with wikilinks between projects.

Plan: [`docs/plans/09-obsidian-export-plan.md`](plans/09-obsidian-export-plan.md)

---

## Possible future work (not on the roadmap)

- **MCP server.** An MCP server can give `engraph recall` and
  `engraph subgraph` as MCP tools with typed arguments. For Claude Code and
  Codex, this adds almost nothing. These clients can do the `engraph` commands
  with their shell tool. The hooks can also change and stop tool calls, and an
  MCP server cannot do this. Each MCP tool also adds tokens to each session. Do
  this work only if a client that has no shell access must use engraph.
- **Compression that changes with the budget level.** When
  `engraph budget status` shows level 2 or 3, the filters can use smaller
  limits. For example, the `tree` depth can change from 3 to 2, and the
  `docker logs` tail can change from 200 lines to 100 lines. At this time, only
  the SessionStart brief and `engraph budget status` show the level. No filter
  uses it. A design for this
  escalation does not exist at this time.
- **Removal of old memory.** A memory store that only increases becomes a
  problem. A possible design: a score that decreases with time and has a
  minimum, a lower rank for an item that retrieval does not find after N
  queries, and a size limit for each project. The schema has the necessary
  column (`scopes.archived`). The algorithm does not exist.
- **Cloud sync.** engraph keeps all data on the local computer. To share memory
  between computers, these parts can be the base: the `EmbeddingProvider`
  trait, the UUIDv7 ids, and the append-only `ingestion_log` table. The work
  must add `MemoryStore` and `GraphStore` traits, and then an implementation
  for Turso or LiteFS. The architecture review recommended that engraph does
  not add these traits before a real need for cloud sync occurs.
- **Weekly summary.** `engraph gain --weekly` shows token totals for each week.
  It does not show the data that follows. An `engraph weekly` command reads
  `events` and `messages`.
  It shows the most compressed commands, the most recalled phrases, the longest
  sessions, and the largest savings. These data help the user and help to
  adjust the filters.
