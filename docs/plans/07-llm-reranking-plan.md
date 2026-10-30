# Plan 07: LLM reranking

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

A small model puts the top results of hybrid retrieval (RRF) in a better
sequence. This function is off by default, because it uses tokens.

## Reason to wait

Hybrid retrieval operates at this time. No data shows that its results are not
sufficiently good. Before this work, a benchmark must show that the precision
of the first 5 results (p@5) does not increase more.

## Dependencies

A benchmark data set of real queries. No such data set exists at this time.

## Effort

2 to 3 days. Most of the work is the prompt design and the measurement of the
token cost.
