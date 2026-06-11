# Engraph memory
Persist durable signals so the next session's SessionStart brief surfaces them.
These are plain Bash calls; the engraph pre-bash hook passes `engraph …` through
untouched.

- **User corrects you / states a hard rule** → `engraph remember "<imperative, specific rule>"`
- **After you fix a bug** → `engraph bug "<one-line summary incl. the error>" --content "<root cause + fix>"`; when a tracked bug is fixed, close it with `engraph bug --resolve <id>`
- **Architecture / library / design decision** → `engraph save "<decision + one-line rationale>" --kind architecture` (use `--kind convention` for style/naming/workflow rules, `--kind performance` for optimization choices, default `decision` otherwise)

Run these from the project root so the stored project key matches the session
cwd. From a subdirectory, pass `--project <repo-root-abs-path>`.

## Retrieval — use engraph before a broad grep/read
Before sweeping the codebase with grep/read for orientation, reach for engraph's
own retrieval first; fall back to grep/read when those come up empty or you need
exact text:

- **Prior context / decisions / conversation history** → `engraph recall "<topic>" --project "$(pwd)"`
- **A code symbol with its callers/callees/siblings** → `engraph subgraph <symbol>` (needs a codegraph; build it once with `engraph index .`)
