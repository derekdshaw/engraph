#!/usr/bin/env bash
# scip-java-bazel-index.sh — produce a SCIP index for a Bazel/Java repo.
#
# This is an EXAMPLE driver, not part of engraph. engraph's Java symbol pass is
# build-system-agnostic: it runs whatever `ENGRAPH_BAZEL_SCIP_JAVA_CMD` names as
# `<cmd> <repo> <out.scip>` and merges the SCIP that command writes. Point that
# env var at a copy of this script (or your own Maven/Gradle equivalent):
#
#   export ENGRAPH_BAZEL_SCIP_JAVA_CMD=/path/to/scip-java-bazel-index.sh
#   export ENGRAPH_BAZEL_SCIP_JAVA_TARGETS='//src/java/...'   # optional; scope to your first-party Java roots
#   engraph index --bazel-symbols /path/to/monorepo
#
# It can also be run standalone:  scip-java-bazel-index.sh <repo> <out.scip>
#
# What it does: drives scip-java's SemanticDB aspect, patched for Bazel 8 + custom
# annotation-processor toolchains (stock scip-java 0.12.x ships a Bazel-7 aspect
# that fails on Bazel 8), then merges the per-target *.scip into one index.
# Requirements: bazel/bazelisk + scip-java on PATH, a JDK, and the repo's deps
# fetchable (VPN/artifactory for private monorepos).
set -euo pipefail

REPO="${1:?usage: $0 <repo-root> <output-scip>}"
OUT="${2:?usage: $0 <repo-root> <output-scip>}"
# Default scope when ENGRAPH_BAZEL_SCIP_JAVA_TARGETS is unset. Scope this to your
# repo's first-party Java roots so a bare run never analyzes the whole repo (a
# bare //... can pull in a huge number of non-Java targets, and any one broken
# non-Java target can abort analysis). ADJUST these for your repo, or set the env
# var. Multiple space-separated patterns are allowed; the aspect skips non-Java
# targets within them.
DEFAULT_TARGETS='//src/java/...'
TARGETS="${ENGRAPH_BAZEL_SCIP_JAVA_TARGETS:-$DEFAULT_TARGETS}"
cd "$REPO"

BAZEL="$(command -v bazel || command -v bazelisk || true)"
[ -n "$BAZEL" ] || { echo "scip-java-index: neither bazel nor bazelisk on PATH" >&2; exit 2; }
SCIP_JAVA="$(command -v scip-java || true)"
[ -n "$SCIP_JAVA" ] || { echo "scip-java-index: scip-java not on PATH" >&2; exit 2; }
JH="${JAVA_HOME:-$(/usr/libexec/java_home 2>/dev/null || true)}"
[ -n "$JH" ] || { echo "scip-java-index: JAVA_HOME unset and no default JDK" >&2; exit 2; }

# scip-java's launcher path may contain spaces (Coursier's "Application Support"),
# which breaks `bazel --define`; route through a space-free wrapper.
WRAP="$(mktemp -t scip-java-bin.XXXXXX)"
printf '#!/bin/sh\nexec "%s" "$@"\n' "$SCIP_JAVA" > "$WRAP"
chmod +x "$WRAP"

# The aspect must live in the workspace so `--aspects //:aspects/scip_java.bzl`
# resolves as a label. Back up any existing file and restore on exit so the repo
# is left exactly as we found it (no permanent edits).
ASPECT_DIR="$REPO/aspects"
ASPECT="$ASPECT_DIR/scip_java.bzl"
BACKUP=""
mkdir -p "$ASPECT_DIR"
[ -f "$ASPECT" ] && { BACKUP="$(mktemp -t scip_java.bzl.XXXXXX)"; cp "$ASPECT" "$BACKUP"; }

cleanup() {
  rm -f "$WRAP"
  if [ -n "$BACKUP" ]; then mv -f "$BACKUP" "$ASPECT"
  else rm -f "$ASPECT"; rmdir "$ASPECT_DIR" 2>/dev/null || true; fi
}
trap cleanup EXIT

cat > "$ASPECT" <<'SCIP_JAVA_ASPECT_BZL'
"""
Bazel aspect to run scip-java against a Java Bazel codebase.

Patched (vs stock scip-java 0.12.x) for Bazel 8 + custom annotation-processor
toolchains:
  - javac_options is a depset on Bazel 8 -> .to_list() before iterating
  - struct.to_json() removed -> json.encode()
  - strip '-Xep' javacopts (Error Prone flags the plain scip-java javac rejects)
"""

def _scip_java(target, ctx):
    if JavaInfo not in target or not hasattr(ctx.rule.attr, "srcs"):
        return None

    javac_action = None
    for a in target.actions:
        if a.mnemonic == "Javac":
            javac_action = a
            break

    if not javac_action:
        return None

    info = target[JavaInfo]
    compilation = info.compilation_info
    annotations = info.annotation_processing

    source_files = []
    source_jars = []
    for src in ctx.rule.files.srcs:
        if src.path.endswith(".java"):
            source_files.append(src.path)
        elif src.path.endswith(".srcjar"):
            source_jars.append(src)

    if len(source_files) == 0:
        return None

    output_dir = []

    for source_jar in source_jars:
        dir = ctx.actions.declare_directory(ctx.label.name + ".extracted_srcjar/" + source_jar.short_path)
        output_dir.append(dir)

        ctx.actions.run_shell(
            inputs = javac_action.inputs,
            outputs = [dir],
            mnemonic = "ExtractSourceJars",
            command = """
                [ "$(unzip -q -l {input_file} | wc -l)" -eq 0 ] || unzip {input_file} -d {output_dir}
            """.format(
                output_dir = dir.path,
                input_file = source_jar.path,
            ),
            progress_message = "Extracting source jar {jar}".format(jar = source_jar.path),
        )

        source_files.append(dir.path)

    classpath = [j.path for j in compilation.compilation_classpath.to_list()]
    bootclasspath = [j.path for j in compilation.boot_classpath]

    processorpath = []
    processors = []
    if annotations and annotations.enabled:
        processorpath += [j.path for j in annotations.processor_classpath.to_list()]
        processors = annotations.processor_classnames

    launcher_javac_flags = []
    compiler_javac_flags = []

    # In different versions of bazel javac options are either a nested set or a depset or a list...
    javac_options = []
    if hasattr(compilation, "javac_options_list"):
        javac_options = compilation.javac_options_list
    else:
        javac_options = compilation.javac_options

    # Bazel 8: javac_options may be a depset, which isn't directly iterable.
    if hasattr(javac_options, "to_list"):
        javac_options = javac_options.to_list()

    for value in javac_options:
        # Drop Error Prone javacopts injected by custom java_library toolchains;
        # the plain javac scip-java runs rejects '-Xep' flags.
        if value != "" and "-Xep" not in value:
            if value.startswith("-J"):
                launcher_javac_flags.append(value)
            else:
                compiler_javac_flags.append(value)

    build_config = struct(**{
        "javaHome": ctx.var["java_home"],
        "classpath": classpath,
        "sourceFiles": source_files,
        "javacOptions": compiler_javac_flags,
        "jvmOptions": launcher_javac_flags,
        "processors": processors,
        "processorpath": processorpath,
        "bootclasspath": bootclasspath,
        "reportWarningOnEmptyIndex": False,
    })
    build_config_path = ctx.actions.declare_file(ctx.label.name + ".scip.json")

    scip_output = ctx.actions.declare_file(ctx.label.name + ".scip")
    targetroot = ctx.actions.declare_directory(ctx.label.name + ".semanticdb")
    ctx.actions.write(
        output = build_config_path,
        content = json.encode(build_config),
    )

    deps = [javac_action.inputs, annotations.processor_classpath]

    ctx.actions.run_shell(
        command = "\"{}\" index --no-cleanup --index-semanticdb.allow-empty-index --cwd \"{}\" --targetroot {} --scip-config \"{}\" --output \"{}\"".format(
            ctx.var["scip_java_binary"],
            ctx.var["sourceroot"],
            targetroot.path,
            build_config_path.path,
            scip_output.path,
        ),
        env = {
            "JAVA_HOME": ctx.var["java_home"],
            "NO_PROGRESS_BAR": "true",
        },
        mnemonic = "ScipJavaIndex",
        inputs = depset([build_config_path] + output_dir, transitive = deps),
        outputs = [scip_output, targetroot],
    )

    return scip_output

def _scip_java_aspect(target, ctx):
    scip = _scip_java(target, ctx)
    if not scip:
        return struct()
    return [OutputGroupInfo(scip = [scip])]

scip_java_aspect = aspect(
    _scip_java_aspect,
)
SCIP_JAVA_ASPECT_BZL

# The aspect ADDS SemanticDB/SCIP actions; it does not change the base javac
# actions, so it reuses the incremental + remote cache and doesn't thrash it.
# --spawn_strategy=local is required (the action writes to --targetroot).
# --keep_going indexes what compiles instead of aborting on one bad target.
"$BAZEL" build $TARGETS \
  --spawn_strategy=local \
  --aspects //:aspects/scip_java.bzl%scip_java_aspect \
  --output_groups=scip \
  --define=sourceroot="$REPO" \
  --define=java_home="$JH" \
  --define=scip_java_binary="$WRAP" \
  --keep_going || true

# Merge per-target .scip into OUT. Concatenated SCIP protobufs parse as one merged
# Index (repeated fields concatenate), which is how scip-java itself merges. Collect
# .scip under each pattern's subtree and dedupe by path, so multiple space-separated
# patterns (and any overlap) merge correctly without double-counting.
BB="$("$BAZEL" info bazel-bin 2>/dev/null || true)"
[ -n "$BB" ] || { echo "scip-java-index: could not resolve bazel-bin" >&2; exit 3; }

LIST="$(mktemp -t scip-list.XXXXXX)"
for pat in $TARGETS; do
  sub="${pat#//}"; sub="${sub%/...}"; sub="${sub%%:*}"
  root="$BB"; [ -n "$sub" ] && [ -d "$BB/$sub" ] && root="$BB/$sub"
  find "$root" -name '*.scip' 2>/dev/null || true
done | sort -u > "$LIST"

: > "$OUT"
count="$(wc -l < "$LIST" | tr -d ' ')"
[ "$count" -gt 0 ] && tr '\n' '\0' < "$LIST" | xargs -0 cat > "$OUT" 2>/dev/null || true
rm -f "$LIST"
echo "scip-java-index: wrote $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes from $count targets)"
