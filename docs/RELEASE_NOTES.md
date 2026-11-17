# Release notes

Changes since v0.3.0.

Last update: 2026-11-16, main at `267b7f5`

## Features

- **Go build tags.** `engraph index --go-tags <a,b>` gives scip-go build tags, so the files behind `//go:build` lines that need them are in the code graph. It works for the Go driver and the Bazel Go pass, keeps the other flags in your `GOFLAGS`, and `--if-changed` repeats the tags of the recorded run. Schema v15 records them.
- **Go workspaces.** A directory with a `go.work` file is a Go project, with or without its own `go.mod`. One scip-go run indexes every module that the `go.work` lists, so `engraph index` and `--recursive` do not index them one at a time. A Go module that the `go.work` does not list is still its own project.
- **Discovery obeys ignore files.** `engraph index --workspace` and `--recursive` skip the directories that `.gitignore` (inside a git repo), `.ignore`, `.git/info/exclude`, or the global git excludes name — the same rules as `git ls-files --exclude-standard`. An ignored directory that is its own git repo is still found, and the parent's rules do not apply inside it. Both modes also skip hidden and build/dependency directories by name.
- **Discovery flags.** `--max-depth <n>` (default 16, with `--recursive`) limits how deep discovery goes. `--include <glob>` and `--exclude <glob>` (repeatable, with either mode) filter the discovered projects by their path below the workspace root: `*` stays within one level, `**` crosses levels, `.` is the root, and a glob matches only the path it names. When both match a project, `--exclude` wins with a warning.
- **The dry run explains discovery.** `engraph index --dry-run` with `--workspace` or `--recursive` lists each directory that explains the result, with its cause: projects with their languages, workspace members with the project that covers them, Bazel roots, ignored directories, directories beyond `--max-depth`, and projects the filter globs dropped.
- **Per-language workspace member rules.** A nested project is skipped only when the project above it really indexes it: a Cargo crate under a `[workspace]`, a TypeScript package that `pnpm-workspace.yaml` or yarn `workspaces` (with a `yarn.lock`) lists, and nested Python and Java projects. A Go module below another Go module is now always its own project, and so are a package of an npm-only workspace, a crate that declares its own `[workspace]`, and a nested git repo.
- **scip-typescript indexes workspaces.** In a pnpm or yarn workspace, the driver passes `--pnpm-workspaces` / `--yarn-workspaces` plus the root, so the workspace packages and the root are indexed in one run. If pnpm or yarn is not installed, the run falls back to the root alone, with a warning, and each package is indexed as its own project.
- **Background index refresh.** For a repo that you indexed before, the SessionStart hook starts `engraph index --if-changed` in the background. This run indexes again only the subtrees that changed since their last index, and it repeats how you indexed each subtree. Set `ENGRAPH_AUTO_INDEX=0` to stop it.
- **Index lock.** One OS file lock per project prevents two index runs at the same time. An explicit run waits for the lock, and a background refresh does not run.
- **Branch-aware code graph.** Each index records the commit that it indexed. When your checkout changed a file after that commit, the grep redirect lets the grep run, and `engraph subgraph` shows the names of the changed files.
- **One index for a monorepo.** `engraph index <subdir>` and `--workspace` put each sub-project into the index of its repo. A sub-project run replaces only the symbols that it can make, and it keeps the symbols of other sub-projects and languages.
- **Removal of stale symbols.** An index run removes the symbols that are no longer in the source. Use `--no-gc` to keep them.
- **Qualified lookups.** `engraph subgraph` and the grep redirect accept `Config::new`, `Config#new`, and `Config.new`. A module path such as `config::Config::new` also finds its symbol.
- **Call lines in "Called by".** `engraph subgraph` shows each caller one time, with the line numbers of its calls.
- **Session digests.** Each ingest makes a digest of each session: the user prompts and the last reply of each turn, word for word. The SessionStart brief shows the last session, and `engraph resume` shows its full digest.
- **Messages stored as written.** Ingest does not compress messages. Compression saved no tokens in recall, and it damaged the compaction summaries.
- **Recall previews at the match.** A recall result shows the text around the match, not the start of the message.
- **Codex support.** engraph ingests Codex rollout transcripts. `engraph hook session-start --client codex` gives the brief to Codex, and the install scripts set up the Codex `SessionStart` and `Stop` hooks.
- **Savings by source.** `engraph gain` shows the saved tokens for each source: commands, code graph, and memory. The itemized table includes all savings, so its total agrees with the summary.
- **engraph-refresh skill.** This Claude Code skill updates the embeddings after a session, and it indexes the code again when you ask. The install scripts can install it.
- **Repeated log lines collapse.** For tests, builds, lints, installs, and logs, `engraph run` keeps the first of the lines that differ only in their timestamp and adds a count. It also collapses 3 or more identical lines in a sequence. File reads, diffs, searches, and listings do not change.
- **Launcher commands get their filter.** Commands that start with `uv run`, `npx`, `python -m`, or `pnpm exec` get the filter of the command that they start. `pnpm test` and `yarn test` use the `npm test` filter.

## Bugs

- A Go module below a `go.work` that does not list it got no symbols: Go builds nothing in such a module, and scip-go exited 0 with an empty index. Its scip-go run now sets `GOWORK=off`, in the Go driver and in the Bazel Go pass.
- A call from one Go module into another module of the same repo pointed at a placeholder, not at the definition, when scip-go could not tie the two modules together: a `go.work` member indexed alone, and each module of the Bazel Go pass. scip-go gave the reference the version `.`, and the definition had a different version. The loader now pins `.` to `0.0.0`, and the Go driver passes `--module-version 0.0.0` as the Bazel Go pass does. This changes every Go symbol id one time; the next index run of a Go repo replaces them.
- A git submodule got its gitdir (`<parent>/.git/modules/<name>`) as its project key, so engraph treated its checkout as a linked worktree: `engraph index --if-changed` and the SessionStart refresh skipped every submodule. A submodule now scopes to its own checkout.
- `engraph run` showed no output when the shell had to expand a word of the command (`~`, `$`, a glob, or braces). The hook does not wrap these commands now.
- When a command failed, 16 filters did not show its error text. They now add stderr when the exit code is not zero.
- Extractive ranking cut file names and versions such as `CLAUDE.md` and `v1.2.3` at the dot.
- The brevity pass changed the spaces around punctuation and removed identifiers with the name `a`.
- The `fd` filter showed each directory as an empty file name in a separate group.
- The embedding commands wrote a copy of the model (approximately 130 MB) into the current directory. All projects now use one cache: `ENGRAPH_EMBED_CACHE`, or the cache directory of the platform.
- On Windows, Bazel targets got absolute file paths, not paths relative to the repo.
- The `engraph gain --by-project` and `--by-session` tables lost their alignment when a path had more than 40 characters.
- The `rg` filter did not shorten a very long match line in a search of one file, for example a large compressed JSON file.
- A regex metacharacter did not stop the grep redirect for a pattern with a keyword, such as `fn foo\b`.
- `engraph index <subdir>` with a relative path made an empty index and then deleted all the symbols of the project.
- `engraph subgraph` could show siblings from a file with the same path in a different repo.
- The post-read hook did not add symbols for files that Claude Code read, because Read sends an absolute path.
- An index of a Gradle sub-project wrote incorrect paths for the sibling modules that scip-java compiled.
- Symbol names could keep SCIP descriptor characters (`Config#`), so `engraph subgraph Config` found nothing.
- A symbol that only a reference created kept an old, incorrect name after later index runs.
- Python graphs had no CALLS or REFERENCES relations, because scip-python sets no symbol kind.
- SCIP local symbols of different files used the same row, so each file wrote over the others.
- A relation could start at the wrong function: decorators and module-level Python code went to the function above them. Module-level TypeScript code now goes to its file.
- The Bazel symbol pass said "no targets" for languages that had targets, and it told you to set a command for languages that had none. Each language now shows its target count and the environment variable that indexes it.
- When an indexer wrote its error to stdout and not to stderr, the failure message was empty. This applies to driver runs, `bazel query`, and each scip-go module.
- A failure of the Bazel symbol pass showed only its first line, without the cause.
- The hook did not wrap `./gradlew`, and for other path commands it removed the path. `./gradlew` and `/usr/bin/git` now stay as you typed them.
- Color codes in command output stopped the filters that look for line prefixes, for example in `vitest` output.
- Progress bars and other text that the terminal wrote over stayed in the output as joined fragments. Terminal hyperlinks also stayed in the output.
- File reads replaced identical adjacent lines with a marker, so data rows were lost.
- The last-resort name of a SCIP symbol could split inside a generic type.
