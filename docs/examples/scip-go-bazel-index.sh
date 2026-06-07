#!/usr/bin/env bash
# scip-go-bazel-index.sh — produce a SCIP index for a Bazel/Go (rules_go + gazelle) repo.
#
# This is an EXAMPLE driver, not part of engraph. engraph's Go symbol pass is
# build-system-agnostic: when ENGRAPH_BAZEL_SCIP_GO_CMD is set it runs whatever that
# env var names as `<cmd> <repo> <out.scip>` and merges the SCIP that command writes.
# Point the env var at a copy of this script (or your own equivalent):
#
#   export ENGRAPH_BAZEL_SCIP_GO_CMD="$PWD/docs/examples/scip-go-bazel-index.sh"
#   export ENGRAPH_BAZEL_SCIP_GO_TARGETS='//...'        # optional; scope of Go targets
#   engraph index --bazel-symbols /path/to/monorepo
#
# It can also be run standalone:  scip-go-bazel-index.sh <repo> <out.scip>
#
# WHY a driver is needed: scip-go needs a Go module root AND a resolvable dependency
# graph. On a gazelle-managed rules_go monorepo most code lives in `go_library`
# targets with no `go.mod`, and deps come from Bazel (go_repository / bzlmod), not
# GOPATH — so there is no single universal recipe. This script shows ONE plausible
# approach; treat it as a starting point and ADJUST for your repo.
#
# BEST-EFFORT / UNVERIFIED: unlike a single `go.mod` repo, driving scip-go over a
# hermetic Bazel module graph is repo-specific. You will likely need to materialize
# deps first (e.g. `bazel run @rules_go//go -- mod download`, a vendor step, or a
# generated go.mod/go.sum) so scip-go's type-checker can resolve imports.
set -euo pipefail

REPO="${1:?usage: $0 <repo-root> <output-scip>}"
OUT="${2:?usage: $0 <repo-root> <output-scip>}"
TARGETS="${ENGRAPH_BAZEL_SCIP_GO_TARGETS:-//...}"   # currently informational; see notes
cd "$REPO"

command -v scip-go >/dev/null 2>&1 || { echo "scip-go-index: scip-go not on PATH" >&2; exit 2; }

# Pinned module version keeps Go entity IDs stable across commits. scip-go otherwise
# defaults --module-version to the cwd's git short hash, which embeds into every
# moniker and churns them each commit. Match engraph's native pass (SCIP_GO_VERSION).
MODVER="0.0.0"

# OPTIONAL: materialize deps so scip-go can resolve imports. Uncomment / adapt:
#   bazel run @rules_go//go -- mod download 2>/dev/null || true

# Discover Go module roots to index: every dir with a go.mod (skipping vendor),
# falling back to the repo root for a pure-gazelle repo with no go.mod. ADJUST this
# to match your layout (e.g. derive roots from `# gazelle:prefix` or from
# `bazel query "kind(go_library, $TARGETS)"`).
ROOTS="$(find . -name go.mod -not -path '*/vendor/*' -exec dirname {} \; | sort -u)"
[ -n "$ROOTS" ] || ROOTS="."

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
# external_symbols) concatenate into one valid merged Index — the same trick the Java
# driver uses. NOTE: scip-go emits document paths relative to --module-root, so for a
# MULTI-root repo you must rebase each part's paths to repo-root before concatenating
# (engraph's delegated path does NOT rebase). For a single repo-root run this is a
# no-op. ADJUST if you index more than one root.
: > "$OUT"
count="$(wc -l < "$LIST" | tr -d ' ')"
[ "$count" -gt 0 ] && tr '\n' '\0' < "$LIST" | xargs -0 cat > "$OUT" 2>/dev/null || true
echo "scip-go-index: wrote $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes from $count module root(s))"
