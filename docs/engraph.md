# Engraph memory
Persist durable signals so the next session's SessionStart brief surfaces them.
These are plain Bash calls; the engraph pre-bash hook passes `engraph …` through
untouched.

- **User corrects you / states a hard rule** → `engraph remember "<imperative, specific rule>"`
- **After you fix a bug** → `engraph bug "<one-line summary incl. the error>" --content "<root cause + fix>"`; when a tracked bug is fixed, close it with `engraph bug --resolve <id>`
- **Architecture / library / design decision** → `engraph save "<decision + one-line rationale>" --kind architecture` (use `--kind convention` for style/naming/workflow rules, `--kind performance` for optimization choices, default `decision` otherwise)

Run these from the project root so the stored project key matches the session
cwd. From a subdirectory, pass `--project <repo-root-abs-path>`.

## Retrieval — `engraph subgraph` is the DEFAULT for code context, before Read or grep
When you want context on a code symbol — what it is, what calls it, what it
calls, what lives beside it — run `engraph subgraph <symbol>` FIRST. It returns a
2-hop neighborhood (callers, callees, siblings) in one call. Do **not** open the
file with Read, or grep the symbol, just to see what surrounds a definition —
that is the slow path. The pre-grep hook already denies grep/rg on indexed
symbols; **Read has no such guard, so reaching for subgraph before Read is on
you.** Fall back to Read/grep only when subgraph comes up empty, the target isn't
a symbol, or you need exact text / line content.

- **Code symbol + its callers/callees/siblings** → `engraph subgraph <symbol>` (the first move for code context; build the graph once with `engraph index .`)
- **Prior context / decisions / conversation history** → `engraph recall "<topic>" --project "$(pwd)"`
