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
# For pnpm: when the pnpm on PATH cannot read the lockfile version, it stops and
# names the two fixes (upgrade the lockfile, or put the pnpm that writes that
# version on PATH); when the lockfile is out of date with a package.json it
# installs with --no-frozen-lockfile and restores the lockfile after. yarn and
# npm stay strict.
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

list_tsconfigs() {
  find . \( -name node_modules -o -name 'bazel-*' -o -name '.*' ! -name . \) -prune \
    -o -name tsconfig.json -print | sort
}

INSTALL_LOG=""
LOCK_BACKUP=""
WORK=""
TSCONFIGS_BEFORE=""
cleanup() {
  # Put back the lockfile that a --no-frozen-lockfile install rewrote.
  if [ -n "$LOCK_BACKUP" ]; then cp "$LOCK_BACKUP" pnpm-lock.yaml && rm -f "$LOCK_BACKUP"; fi
  # Remove the tsconfig.json files --infer-tsconfig wrote into the repo: new
  # since the snapshot, with exactly one of the two contents it writes.
  if [ -n "$TSCONFIGS_BEFORE" ] && [ -f "$TSCONFIGS_BEFORE" ]; then
    list_tsconfigs | comm -13 "$TSCONFIGS_BEFORE" - | while IFS= read -r f; do
      c="$(cat "$f" 2>/dev/null || true)"
      if [ "$c" = '{}' ] || [ "$c" = '{"compilerOptions":{"allowJs":true}}' ]; then rm -f "$f"; fi
    done
  fi
  rm -rf ${INSTALL_LOG:+"$INSTALL_LOG"} ${WORK:+"$WORK"} ${TSCONFIGS_BEFORE:+"$TSCONFIGS_BEFORE"}
}
trap cleanup EXIT

# A pnpm that cannot read the lockfile's version (pnpm 12 refuses a v6 lockfile;
# a rules_js repo often pins no pnpm, since rules_js reads the lockfile itself).
# The frozen install fails at once with ERR_PNPM_BROKEN_LOCKFILE, which is the
# check: no table of which pnpm reads which version. The version only names the
# pnpm to suggest — the major that writes it.
lockfile_version_error() {
  local version want
  version="$(sed -n "s/^lockfileVersion: *['\"]\{0,1\}\([0-9.]*\).*/\1/p" pnpm-lock.yaml | head -n 1)"
  case "${version%%.*}" in
    5) want=7 ;;
    6) want=8 ;;
    *) return 1 ;; # a broken lockfile of a current version: pnpm's own error says why
  esac
  cat >&2 <<EOF
scip-typescript-index: pnpm $(pnpm --version) cannot read pnpm-lock.yaml (lockfileVersion $version). Either:
  - upgrade the lockfile: run \`pnpm install --lockfile-only\` in the repo with this pnpm (it rewrites pnpm-lock.yaml), or
  - put pnpm $want on PATH (it writes lockfileVersion $version).
EOF
}

# --frozen-lockfile first. A lockfile that is out of date with a package.json
# (a dependency bump that did not update it) can never pass it; only then,
# install with --no-frozen-lockfile and put the original lockfile back after
# (cleanup). The drifted packages then resolve from package.json, not from the
# lockfile that Bazel builds with.
pnpm_install() {
  INSTALL_LOG="$(mktemp -t scip-ts-install.XXXXXX)"
  pnpm install --frozen-lockfile 2>&1 | tee "$INSTALL_LOG" && return
  if grep -q ERR_PNPM_BROKEN_LOCKFILE "$INSTALL_LOG" && lockfile_version_error; then exit 1; fi
  grep -q ERR_PNPM_OUTDATED_LOCKFILE "$INSTALL_LOG" || exit 1
  echo "scip-typescript-index: warning: pnpm-lock.yaml is out of date with a package.json;" \
    "installing with --no-frozen-lockfile, then restoring the lockfile" >&2
  LOCK_BACKUP="$(mktemp -t scip-ts-lock.XXXXXX)"
  cp pnpm-lock.yaml "$LOCK_BACKUP"
  pnpm install --no-frozen-lockfile
}

if [ -f pnpm-lock.yaml ]; then
  need pnpm
  pnpm_install
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

WORK="$(mktemp -d -t scip-ts-work.XXXXXX)"
TSCONFIGS_BEFORE="$(mktemp -t scip-ts-cfgs.XXXXXX)"
list_tsconfigs > "$TSCONFIGS_BEFORE"

# The workspace flag, as engraph's native pass gives it (driver::typescript_command):
# with the flag scip-typescript indexes only the packages the tool lists, and yarn 1
# does not list the root, so name the root too, as its real path.
ARGS=(index --output "$WORK/ts.scip" --infer-tsconfig)
if [ -f pnpm-workspace.yaml ]; then
  ARGS+=(--pnpm-workspaces "$(pwd -P)")
elif [ -f yarn.lock ] && node -e 'process.exit(require("./package.json").workspaces ? 0 : 1)' 2>/dev/null; then
  ARGS+=(--yarn-workspaces "$(pwd -P)")
fi

# scip-typescript writes document paths relative to its cwd, so running it from the
# repo root gives the repo-root-relative paths engraph's delegated path requires (it
# does not rebase them).
scip-typescript "${ARGS[@]}"

# Second pass for the JavaScript files no project covers. The workspace pass
# reads each project through its tsconfig, and a tsconfig without allowJs (or
# the {} that --infer-tsconfig writes once node_modules holds any .d.ts) skips
# the .js/.jsx files. The helper resolves every tsconfig with the TypeScript
# that ships inside scip-typescript — an extends chain into node_modules
# counts, e.g. astro's allowJs — and writes a config whose `files` are exactly
# the JS files no project covers: a file indexed twice gets its relations
# twice in engraph.
cat > "$WORK/uncovered-js.cjs" <<'NODE'
const fs = require("fs");
const path = require("path");
const repo = path.resolve(process.argv[2]);
const outConfig = process.argv[3];
const bin = fs.realpathSync(process.env.SCIP_TS_BIN);
const ts = require(require.resolve("typescript", { paths: [path.dirname(bin)] }));

const tsconfigs = [];
const jsFiles = [];
const skip = (name) => name === "node_modules" || name.startsWith(".") || name.startsWith("bazel-");
(function walk(dir) {
  let entries;
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const entry of entries) {
    if (skip(entry.name) || entry.isSymbolicLink()) continue;
    const p = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(p);
    else if (entry.name === "tsconfig.json") tsconfigs.push(p);
    else if (/\.(js|jsx|mjs|cjs)$/.test(entry.name)) jsFiles.push(p);
  }
})(repo);

const covered = new Set();
const host = { ...ts.sys, onUnRecoverableConfigFileDiagnostic() {} };
for (const cfg of tsconfigs) {
  const parsed = ts.getParsedCommandLineOfConfigFile(cfg, {}, host);
  for (const f of parsed?.fileNames ?? []) covered.add(path.resolve(f));
}
const files = jsFiles.filter((f) => !covered.has(f)).sort();
fs.writeFileSync(
  outConfig,
  JSON.stringify({ compilerOptions: { allowJs: true, checkJs: false, noEmit: true }, files }, null, 2),
);
process.stdout.write(String(files.length));
NODE
mkdir -p "$WORK/js"
if JS_COUNT="$(SCIP_TS_BIN="$(command -v scip-typescript)" node "$WORK/uncovered-js.cjs" "$(pwd -P)" "$WORK/js/tsconfig.json")"; then :; else
  echo "scip-typescript-index: warning: the JavaScript pass failed; the index is TypeScript only" >&2
  JS_COUNT=0
fi
if [ "${JS_COUNT:-0}" -gt 0 ]; then
  scip-typescript index --cwd "$(pwd -P)" --output "$WORK/js.scip" "$WORK/js"
  # One SCIP index: the repeated fields of two indexes concatenate.
  cat "$WORK/ts.scip" "$WORK/js.scip" > "$OUT"
else
  cp "$WORK/ts.scip" "$OUT"
fi
echo "scip-typescript-index: wrote $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes; $JS_COUNT JavaScript files in the second pass)"
