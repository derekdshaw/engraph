# Plan 11: Subgraph results in the grep redirect

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

You can do this plan at any time. It does not need other plans, and no other
plan must wait for it.

## Result

- When the grep redirect finds the symbol of a search, the deny reason
  contains the `engraph subgraph` result for that symbol. The agent does not
  do a second tool call to get it.
- The grep redirect and `engraph subgraph` use one rule to find a symbol. If
  `engraph subgraph` finds no symbol, the search continues as a text search.
- A `.`-only path that has no qualified match (`Cargo.lock`, `self.config`)
  continues as a text search.
- Telemetry records the tokens of the result that the hook gives.

## Condition at this time (2026-11-04)

- The PreToolUse(Grep) hook (`run_pre_grep_hook`) and the PreToolUse(Bash)
  hook (`run_pre_bash_hook`) call `try_subgraph_redirect`
  (`crates/engraph-cli/src/redirect.rs`). If the pattern names an indexed
  symbol, `emit_subgraph_deny` stops the search with a `deny` decision.
- The deny reason only tells the agent to do `engraph subgraph <symbol>`:

  ```
  `{symbol}` is indexed in the engraph code graph ({count} match{plural}).
  Run `engraph subgraph {symbol}` for a 2-hop neighborhood ...
  ```

  Thus, each redirect costs two tool calls: the denied search, and the
  `engraph subgraph` command that follows.
- The redirect has its own lookup (`entities_matching`, `LIMIT 9`). It is
  different from the lookup of `engraph subgraph` (`resolve_matches`,
  `crates/engraph-codegraph/src/subgraph.rs`):
  - For a path, the redirect tries the qualified-name suffixes of two or more
    segments (`qualified_candidates`). Then, for a `::` or `#` path, it tries
    the last segment as a plain name.
  - `engraph subgraph` tries the same suffixes. Then it tries the last segment
    as a qualified name. This finds a top-level item from its module path
    (`divergence::in_linked_worktree`). It does not find a member from a
    wrong path.
  - Example: the redirect stops `rg "other::full_path_base"` and sends the
    agent to the `Config.full_path_base` field. But
    `engraph subgraph other::full_path_base` finds no symbol.
- The redirect continues the search if the symbol has more than 8 matches.
  It also continues the search if the checkout changed the file of a matched
  symbol after the index (`divergence::any_changed_since_index`).
- `engraph subgraph` makes its markdown in `main.rs` (`Cmd::Subgraph`): a
  "Changed since the index" note, then `format_markdown` with
  `DEFAULT_BYTE_CAP` (8192 bytes) and `--max-nodes` (default 30). The symbols
  of the current project are first in a list of matches.
- `emit_subgraph_deny` records the event with `output_tokens: 0`. Thus,
  `engraph gain` does not show the cost of a redirect.
- Codex has no tool-use hooks. This plan does not change Codex.

## Design

### One lookup

The hook uses the lookup of `engraph subgraph`, with two changes for the
redirect:

1. For a `.`-only path, the hook does not try the last segment alone. Thus,
   `self.config` does not find a top-level `config`, and the search continues.
2. The hook gets up to 9 matches. If it gets 9, the search continues, as at
   this time.

All other rules are the same as in `engraph subgraph`. The plain-name fallback
of the redirect for `::` and `#` paths is removed.

### Result in the deny reason

The deny reason contains:

1. The `engraph subgraph` markdown for the symbol, with a smaller byte cap and
   node cap than the command.
2. One line that gives the full command (`engraph subgraph <symbol>`) and the
   bypass (`<symbol>\b`).

The hook does not add the "Changed since the index" note. If a matched file
changed, the search continues before the hook makes the result.

### Telemetry

`emit_subgraph_deny` records the tokens of the deny reason as
`output_tokens`. `input_tokens` stays 0, because the search did not occur.

## Steps

1. Find out if Claude Code gives the full `permissionDecisionReason` of a
   `deny` to the agent, for the Grep tool and for the Bash tool. Find out if
   Claude Code cuts a long reason, and at which length. Record the results in
   the pull request.
2. Find out the size of the `engraph subgraph` result for 20 symbols in the
   repos of the `engraph-test-repos` directory. Use each language. Select a
   byte cap and a node cap for the hook. The cap must be smaller than the
   limit from step 1. Start with 4096 bytes and 15 nodes. Record the sizes and
   the caps in the pull request.
3. Move the markdown part of `Cmd::Subgraph` in `main.rs` into a function in
   `output.rs`. Make `engraph subgraph` use this function.
4. Add a redirect mode to the lookup of `engraph subgraph`. In this mode, the
   lookup does not try the last segment alone for a `.`-only path, and it gets
   up to 9 matches.
5. In `try_subgraph_redirect`, use the lookup from step 4 in place of
   `entities_matching`. Remove `entities_matching` and the plain-name
   fallback. Keep the gate for 9 matches and the divergence check.
6. Put the result from step 3 in the deny reason, with the caps from step 2.
   Add the line with the full command and the bypass.
7. In `emit_subgraph_deny`, record the tokens of the deny reason as
   `output_tokens`.
8. Update the text about the redirect in `DETAILS.md`, `README.md`,
   `AGENTS.md`, `docs/engraph.md`, and `docs/engraph-embeddings.md`. These
   files tell the agent that the hook denies the search and that it must do
   `engraph subgraph`. After this plan, the deny reason contains the result.

## Tests

- A search for an indexed name gets a deny reason that contains the
  `## Symbol` heading and the callers of the symbol.
- A search for a module path to a top-level item
  (`divergence::in_linked_worktree`) gets a deny reason with the result.
- A search for `other::full_path_base` continues, because
  `engraph subgraph other::full_path_base` finds no symbol.
- A search for `self.config` or `Cargo.lock` with no qualified match
  continues.
- A search for a name with 9 or more matches continues.
- A search for a symbol in a file that the checkout changed continues.
- The deny reason is not larger than the byte cap from step 2, plus the last
  line.
- The redirect event has `output_tokens` larger than 0.
- `engraph subgraph <symbol>` gives the same markdown as before step 3.

## Risks

1. **More tokens when the agent wanted text.** At this time, a deny reason is
   approximately 60 tokens. After this plan, it can be approximately 1000
   tokens. If the agent wanted text matches (strings, comments, or
   configuration files), these tokens give no help. The graph does not have
   these matches. Mitigation: the cap from step 2, and the bypass in the last
   line. Use the `output_tokens` of step 7 to monitor this cost.
2. **A slower hook.** The lookup and the neighborhood need more queries than
   the count of matches at this time. The hook operates before each search.
   Mitigation: measure the time of the hook in step 2. If a hook takes more
   than 100 ms, make the node cap smaller.
3. **Fewer redirects for `::` paths.** A `::` path that only the plain-name
   fallback finds at this time continues as a text search. The result is
   correct, but engraph saves no tokens on that search.

## Dependencies

None. If the fix for module paths to top-level items (the last-segment
candidate in `resolve_matches`) is not merged, merge it first. The tests for
`divergence::in_linked_worktree` need it.

## Effort

Approximately one day. Steps 1 and 2 need a Claude Code session and the test
repos.
