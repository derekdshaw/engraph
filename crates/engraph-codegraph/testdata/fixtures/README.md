# Stable-symbol-id fixtures (plan 05)

One small repo per language — `rust`, `python`, `go`, `typescript`, `java` —
with the same shape: a `models` module (types with methods), an `ops` module
that uses `models` across the module boundary, and a `text` module of string
helpers both use. Each defines 50+ symbols (functions, types, methods).

The symbol ids these repos produce are recorded in snapshot files; the snapshot
tests compare a fresh index run against them. Any rename or move here changes
the recorded ids on purpose, so edit these fixtures only together with their
snapshots (`ENGRAPH_UPDATE_SNAPSHOTS=1`).

None of the fixtures has an external dependency, so there is nothing to
install before indexing and no network access in CI.

This directory lives under `testdata/`, a name workspace discovery prunes
(`index.rs`, `is_pruned_dir`), so a dogfooded `engraph index .` of this repo
never descends into the fixture manifests.

scip-python puts the *directory basename* of the indexed repo into every
first-party symbol id (the driver's `--project-name`). Index a copy of the
`python` fixture under a directory named exactly `python`, or the ids will not
match the snapshot.
