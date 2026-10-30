# Plan 04: Measurement of file reads

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

Telemetry shows the tokens that file reads use. It also shows the tokens of
reads of a file that the session read before. With these data, you can decide
if a change to file reads is worth the work.

## Condition at this time (2026-10-30)

The PostToolUse(Read) hook adds a list of symbols after Claude reads a file. Its
telemetry event (`read_augment`) records only the tokens of this list. It does
not record the tokens of the read.

An earlier plan added a PreToolUse(Read) hook that gives a file summary before
the read. The purpose of the summary was that Claude reads only the necessary
part of the file. This plan does not operate. The Claude Code hooks documentation
(`code.claude.com/docs/en/hooks`) gives this description of `additionalContext`
for PreToolUse: "String added to Claude's context alongside the tool result."
Thus, Claude receives the summary at the same time as the full file. The summary
cannot make the read smaller. It gives the same data as the PostToolUse(Read)
symbol list.

A hook that stops the read and gives only a summary can make Claude use
incorrect data.

## Steps

1. Find out if the input of the PostToolUse(Read) hook includes the content of
   the read in `tool_response`. If it does not, count the tokens of the lines
   that the read selected. Use the file path, `offset`, and `limit` in
   `tool_input`.
2. In the PostToolUse(Read) hook, record a telemetry event for each read. In
   this event, record the token count of the read and a hash of the file path.
3. Add the `--reads` flag to the `engraph gain` command. With `--reads`, show
   the tokens of all reads. Also show the tokens of reads of a file that the
   session read before.
4. Collect this data for two weeks. In the same two weeks, measure the total
   savings with `engraph gain`. The initial success criterion of engraph is
   that it saves 20% or more of all tokens.
5. If the reads of the same file again are not one of the three largest token
   sources, stop the work on this plan.
6. Find out if a different method can make reads of the same file smaller. A
   possible method: for a read of an unchanged file that the session read
   before, the PreToolUse(Read) hook stops the read and refers Claude to the
   earlier result.
7. Before you use the method of step 6, find out what occurs after Claude Code
   compacts the conversation. After compaction, the earlier result is possibly
   not in the context.

## Tests

- The PostToolUse(Read) hook records one telemetry event for each read, with
  the token count of the read.
- `engraph gain --reads` shows the tokens of reads of the same file again
  separately from the tokens of all reads.

## Risks

The hook operates for each read. The telemetry must be small and fast.

## Dependencies

None.
