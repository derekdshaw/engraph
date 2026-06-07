#!/usr/bin/env bash
# scip-go-bazel-index.sh — example driver: emit a merged SCIP index for a Go repo.
#
# Not part of engraph. engraph's Go symbol pass is build-system-agnostic: when
# ENGRAPH_BAZEL_SCIP_GO_CMD is set, engraph runs whatever it names as
# `<cmd> <repo> <out.scip>` and merges the SCIP that command writes. Point the env
# var at a copy of this script (or your own equivalent):
#
#   export ENGRAPH_BAZEL_SCIP_GO_CMD="$PWD/docs/examples/scip-go-bazel-index.sh"
#   export ENGRAPH_BAZEL_SCIP_GO_ROOTS="dir1 dir2"   # optional; see below
#   engraph index --bazel-symbols /path/to/repo
#
# Standalone:  scip-go-bazel-index.sh <repo> <out.scip>
#
# It runs scip-go over one or more Go MODULE ROOTS and concatenates the SCIP. scip-go
# needs a module root (a dir with go.mod) AND a resolvable dependency graph; making
# deps resolvable (module cache / GOPATH / vendor) is the caller's job and is
# repo-specific — a Bazel repo may need a deps/vendor step first. Keep any such glue
# in how you INVOKE this script, not in the script.
set -euo pipefail

REPO="${1:?usage: $0 <repo-root> <output-scip>}"
OUT="${2:?usage: $0 <repo-root> <output-scip>}"
cd "$REPO"
command -v scip-go >/dev/null 2>&1 || { echo "scip-go-index: scip-go not on PATH" >&2; exit 2; }

# Which module roots to index, in order of preference:
#   1. ENGRAPH_BAZEL_SCIP_GO_ROOTS — space-separated dirs (relative to the repo),
#      for layouts where discovery isn't right (e.g. one module rooted elsewhere).
#   2. every dir containing a go.mod (excluding vendor).
#   3. the repo root.
if [ -n "${ENGRAPH_BAZEL_SCIP_GO_ROOTS:-}" ]; then
  ROOTS="$ENGRAPH_BAZEL_SCIP_GO_ROOTS"
else
  ROOTS="$(find . -name go.mod -not -path '*/vendor/*' -exec dirname {} \; | sort -u)"
  [ -n "$ROOTS" ] || ROOTS="."
fi

# Pin --module-version so Go entity IDs stay stable across commits (scip-go else
# defaults it to the cwd's git short hash, churning every moniker). Matches engraph's
# native pass (driver::SCIP_GO_VERSION).
MODVER="0.0.0"

WORK="$(mktemp -d -t scip-go.XXXXXX)"
LIST="$(mktemp -t scip-go-list.XXXXXX)"
trap 'rm -rf "$WORK" "$LIST"' EXIT

i=0
for root in $ROOTS; do
  part="$WORK/part-$i.scip"; i=$((i + 1))
  if scip-go index --module-root "$root" --module-version "$MODVER" \
       --skip-tests --output "$part" >/dev/null 2>&1 && [ -s "$part" ]; then
    echo "$part" >> "$LIST"
  else
    echo "scip-go-index: scip-go failed for module root '$root' (skipped)" >&2
  fi
done

# Merge per-root .scip by concatenation: a SCIP Index's repeated fields (documents,
# external_symbols) concatenate into one valid Index. NOTE: scip-go emits paths
# relative to --module-root, so indexing MULTIPLE roots can collide paths — engraph's
# delegated path does NOT rebase. Prefer a single root, or rebase per root before
# concatenating.
: > "$OUT"
count="$(wc -l < "$LIST" | tr -d ' ')"
[ "$count" -gt 0 ] && tr '\n' '\0' < "$LIST" | xargs -0 cat > "$OUT" 2>/dev/null || true
echo "scip-go-index: wrote $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes from $count module root(s))"
