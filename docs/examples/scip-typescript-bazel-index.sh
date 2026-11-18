#!/usr/bin/env bash
# scip-typescript-bazel-index.sh — example driver: emit a SCIP index for a Bazel
# TypeScript / JavaScript repo (rules_ts, aspect_rules_js).
#
# Not part of engraph. When ENGRAPH_BAZEL_SCIP_TS_CMD is set, engraph's ts symbol
# pass runs whatever it names as `<cmd> <repo> <out.scip>` and merges the SCIP that
# command writes. Point the env var at a copy of this script (or your own
# equivalent):
#
#   export ENGRAPH_BAZEL_SCIP_TS_CMD="$PWD/docs/examples/scip-typescript-bazel-index.sh"
#   engraph index --bazel-symbols /path/to/repo
#
# Standalone:  scip-typescript-bazel-index.sh <repo> <out.scip>
#
# scip-typescript needs the packages installed at the root, but rules_js makes
# node_modules under bazel-bin/<pkg>/ during a build, not at the root. This script
# installs them at the root with the package manager of the lock file, then runs
# the command of engraph's native pass:
#
#   scip-typescript index --infer-tsconfig [--pnpm-workspaces | --yarn-workspaces <root>]
#
# The install writes node_modules into the repo (gitignored in most repos) and runs
# the packages' install scripts. Once a root node_modules exists, engraph's native
# pass can index the repo without this script.
set -euo pipefail

REPO="${1:?usage: $0 <repo-root> <output-scip>}"
OUT="${2:?usage: $0 <repo-root> <output-scip>}"
# Resolve OUT before the cd: a relative path would land inside the repo.
OUT="$(cd "$(dirname "$OUT")" && pwd -P)/$(basename "$OUT")"
cd "$REPO"

need() {
  command -v "$1" >/dev/null 2>&1 || { echo "scip-typescript-index: $1 not on PATH" >&2; exit 2; }
}
need scip-typescript

if [ -f pnpm-lock.yaml ]; then
  need pnpm
  pnpm install --frozen-lockfile
elif [ -f yarn.lock ]; then
  need yarn
  # yarn 1 has no --immutable; its equivalent is --frozen-lockfile.
  case "$(yarn --version)" in
    1.*) yarn install --frozen-lockfile ;;
    *) yarn install --immutable ;;
  esac
elif [ -f package-lock.json ]; then
  need npm
  npm ci
else
  echo "scip-typescript-index: no pnpm-lock.yaml, yarn.lock, or package-lock.json in $REPO" >&2
  exit 2
fi

# The workspace flag, as engraph's native pass gives it (driver::typescript_command):
# with the flag scip-typescript indexes only the packages the tool lists, and yarn 1
# does not list the root, so name the root too, as its real path.
ARGS=(index --output "$OUT" --infer-tsconfig)
if [ -f pnpm-workspace.yaml ]; then
  ARGS+=(--pnpm-workspaces "$(pwd -P)")
elif [ -f yarn.lock ] && node -e 'process.exit(require("./package.json").workspaces ? 0 : 1)' 2>/dev/null; then
  ARGS+=(--yarn-workspaces "$(pwd -P)")
fi

# scip-typescript writes document paths relative to its cwd, so running it from the
# repo root gives the repo-root-relative paths engraph's delegated path requires (it
# does not rebase them).
scip-typescript "${ARGS[@]}"
echo "scip-typescript-index: wrote $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes)"
