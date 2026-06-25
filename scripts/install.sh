#!/usr/bin/env bash
set -euo pipefail

# Engraph installer for macOS and Linux.
# Resolves the `engraph` binary relative to this script's directory (matches
# the layout of the release archive), installs it under a per-user prefix,
# and wires SessionStart + PreToolUse(Bash,Grep) + PostToolUse(Read) +
# SessionEnd hooks into Claude Code's settings.json.

BINARY="engraph"
CLAUDE_DIR="$HOME/.claude"
SETTINGS_FILE="$CLAUDE_DIR/settings.json"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RESET='\033[0m'

info()  { printf "${CYAN}${BOLD}==> ${RESET}${BOLD}%s${RESET}\n" "$*"; }
ok()    { printf "${GREEN}${BOLD} OK ${RESET}%s\n" "$*"; }
warn()  { printf "${YELLOW}${BOLD}WARN${RESET} %s\n" "$*"; }
err()   { printf "${RED}${BOLD}ERR ${RESET}%s\n" "$*"; exit 1; }

# Resolve the directory containing this script (handles symlinks).
script_dir() {
    local src="${BASH_SOURCE[0]}"
    while [ -h "$src" ]; do
        local dir
        dir="$(cd -P "$(dirname "$src")" && pwd)"
        src="$(readlink "$src")"
        [[ "$src" != /* ]] && src="$dir/$src"
    done
    cd -P "$(dirname "$src")" && pwd
}

SCRIPT_DIR="$(script_dir)"

info "Engraph installer"
echo ""

# --- Locate binary ---
# Primary: same directory as this script (release archive layout).
# Fallback: ./target/release relative to the repo root, for running from a
# source checkout without first untarring.
BIN_PATH=""
if [ -x "$SCRIPT_DIR/$BINARY" ]; then
    BIN_PATH="$SCRIPT_DIR/$BINARY"
elif [ -x "$SCRIPT_DIR/../target/release/$BINARY" ]; then
    BIN_PATH="$SCRIPT_DIR/../target/release/$BINARY"
fi

if [ -z "$BIN_PATH" ]; then
    echo "Could not find $BINARY next to this script ($SCRIPT_DIR)."
    echo "Either run this script from inside an extracted release archive,"
    echo "or build first with: cargo build --release"
    echo ""
    read -rp "Enter the path to the engraph binary: " user_path
    user_path="${user_path/#\~/$HOME}"
    if [ -x "$user_path" ]; then
        BIN_PATH="$user_path"
    else
        err "Binary not executable at '$user_path'."
    fi
fi

ok "Found binary at $BIN_PATH"

# --- Determine install location ---

if [ "$(uname)" = "Darwin" ]; then
    INSTALL_DIR="/usr/local/bin"
    if [ ! -w "$INSTALL_DIR" ]; then
        INSTALL_DIR="$HOME/.local/bin"
    fi
else
    INSTALL_DIR="$HOME/.local/bin"
fi

echo ""
read -rp "Install $BINARY to [$INSTALL_DIR]: " custom_dir
if [ -n "$custom_dir" ]; then
    INSTALL_DIR="${custom_dir/#\~/$HOME}"
fi

mkdir -p "$INSTALL_DIR"

# --- Copy binary ---

info "Installing $BINARY to $INSTALL_DIR"

# Delete-then-copy, NOT cp-overwrite. Overwriting a running binary in place
# mutates the same inode that long-lived Claude sessions may have mmap'd;
# macOS's AMFI later revalidates code pages against the on-disk hash, fails,
# and SIGKILLs those processes. Unlinking first creates a fresh inode for
# the new binary while the old inode (held by existing mmaps) lives on.
DEST="$INSTALL_DIR/$BINARY"
rm -f "$DEST"
cp "$BIN_PATH" "$DEST"
chmod +x "$DEST"
ok "$BINARY"

# Check if install dir is on PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
    warn "$INSTALL_DIR is not on your PATH"
    echo "  Add to your shell profile:"
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
    echo ""
fi

ENGRAPH="$INSTALL_DIR/$BINARY"

# --- Configure Claude Code settings.json ---

info "Configuring Claude Code hooks"

mkdir -p "$CLAUDE_DIR"

if ! command -v python3 &>/dev/null && ! command -v python &>/dev/null; then
    err "python3 or python is required for JSON merging. Install Python and retry."
fi

PYTHON=$(command -v python3 2>/dev/null || command -v python 2>/dev/null)

"$PYTHON" - "$SETTINGS_FILE" "$ENGRAPH" <<'PYEOF'
import json, os, sys

settings_path, engraph = sys.argv[1], sys.argv[2]

settings = {}
if os.path.exists(settings_path):
    try:
        with open(settings_path) as f:
            settings = json.load(f)
    except json.JSONDecodeError:
        settings = {}

# The hooks engraph implements today.
hooks_config = {
    "SessionStart": [{
        "matcher": "",
        "hooks": [{"type": "command", "command": f"{engraph} hook session-start"}],
    }],
    "PreToolUse": [
        {
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": f"{engraph} hook pre-bash"}],
        },
        {
            "matcher": "Grep",
            "hooks": [{"type": "command", "command": f"{engraph} hook pre-grep"}],
        },
    ],
    "PostToolUse": [
        {
            "matcher": "Read",
            "hooks": [{"type": "command", "command": f"{engraph} hook post-read"}],
        },
    ],
    "SessionEnd": [{
        "matcher": "",
        "hooks": [{"type": "command", "command": f"{engraph} hook session-end"}],
    }],
}

existing_hooks = settings.get("hooks", {})
for event, entries in hooks_config.items():
    if event in existing_hooks:
        # Filter out any existing engraph entry so a re-install replaces
        # rather than duplicates.
        filtered = [
            e for e in existing_hooks[event]
            if not any("engraph" in h.get("command", "") for h in e.get("hooks", []))
        ]
        existing_hooks[event] = filtered + entries
    else:
        existing_hooks[event] = entries

settings["hooks"] = existing_hooks

with open(settings_path, "w") as f:
    json.dump(settings, f, indent=2)
    f.write("\n")
PYEOF

ok "Updated $SETTINGS_FILE"

# --- Install memory-capture guidance ---
# The writer commands (remember/bug/save) only get called if Claude is told
# when to call them. Ship a memory file and import it from CLAUDE.md so the
# guidance loads in every session (same pattern as the SessionStart hook's
# consumers). engraph becomes the single capture path.

info "Installing memory-capture guidance"

ENGRAPH_MD="$CLAUDE_DIR/engraph.md"

# Copy the shipped guidance file rather than emitting it inline, so
# docs/engraph.md stays the single source of truth. Resolve it like the binary:
# next to this script (release archive layout), else ../docs (source checkout).
ENGRAPH_MD_SRC=""
if [ -f "$SCRIPT_DIR/engraph.md" ]; then
    ENGRAPH_MD_SRC="$SCRIPT_DIR/engraph.md"
elif [ -f "$SCRIPT_DIR/../docs/engraph.md" ]; then
    ENGRAPH_MD_SRC="$SCRIPT_DIR/../docs/engraph.md"
fi

if [ -n "$ENGRAPH_MD_SRC" ]; then
    cp "$ENGRAPH_MD_SRC" "$ENGRAPH_MD"
    ok "Wrote $ENGRAPH_MD (from $ENGRAPH_MD_SRC)"

    CLAUDE_MD="$CLAUDE_DIR/CLAUDE.md"
    if [ ! -f "$CLAUDE_MD" ] || ! grep -q '@engraph.md' "$CLAUDE_MD"; then
        printf '\n@engraph.md\n' >> "$CLAUDE_MD"
        ok "Imported @engraph.md in $CLAUDE_MD"
    else
        info "@engraph.md already imported in $CLAUDE_MD"
    fi
else
    warn "Could not find docs/engraph.md next to the installer; skipping memory guidance"
fi

# --- Install optional Claude Code skill(s) ---
# Ship the `engraph-refresh` skill (reindex embeddings + opt-in code-graph
# rebuild) into the user's skill directory, opt-in. Resolve the source like the
# binary/md: next to this script (release archive layout), else ../skills
# (source checkout).
info "Optional Claude Code skill"

SKILL_SRC=""
if [ -f "$SCRIPT_DIR/skills/engraph-refresh/SKILL.md" ]; then
    SKILL_SRC="$SCRIPT_DIR/skills/engraph-refresh"
elif [ -f "$SCRIPT_DIR/../skills/engraph-refresh/SKILL.md" ]; then
    SKILL_SRC="$SCRIPT_DIR/../skills/engraph-refresh"
fi

if [ -n "$SKILL_SRC" ]; then
    read -rp "Install the 'engraph-refresh' skill (reindex embeddings + optional code-graph rebuild)? [y/N]: " run_skill
    case "$run_skill" in
        [yY] | [yY][eE][sS])
            SKILL_DEST="$CLAUDE_DIR/skills/engraph-refresh"
            mkdir -p "$SKILL_DEST"
            cp "$SKILL_SRC/SKILL.md" "$SKILL_DEST/SKILL.md"
            ok "Installed skill to $SKILL_DEST"
            ;;
        *)
            echo "Skipped. Install later by copying $SKILL_SRC to $CLAUDE_DIR/skills/"
            ;;
    esac
else
    warn "Could not find skills/engraph-refresh next to the installer; skipping skill"
fi

echo ""
printf "${GREEN}${BOLD}Engraph installed successfully!${RESET}\n"
echo ""
echo "Binary:    $ENGRAPH"
echo "Settings:  $SETTINGS_FILE"
echo "Memory:    $ENGRAPH_MD (imported via @engraph.md)"
echo "Database:  \$ENGRAPH_DB_PATH (default: ~/.local/share/engraph/engraph.db)"
echo ""
echo "Sanity check:"
echo "  $ENGRAPH --version"
echo "  $ENGRAPH gain"
echo ""
echo "Next: open Claude Code in any project. SessionStart will auto-inject"
echo "a brief if there's prior context for that cwd; Bash commands matching"
echo "a wrapper (git log, cargo test, etc.) will be silently rewritten to"
echo "route through 'engraph run'. After 'engraph index .', Grep on a"
echo "bareword symbol indexed in the codegraph is redirected to"
echo "'engraph subgraph <symbol>'."
echo ""
echo "Codegraph features (engraph index / subgraph) need external SCIP"
echo "indexers — one per language (rust-analyzer, scip-python, scip-go,"
echo "scip-typescript, scip-java)."

SCIP_INSTALLER="$SCRIPT_DIR/install-scip-indexers.sh"
if [ -x "$SCIP_INSTALLER" ]; then
    echo ""
    read -rp "Install the SCIP indexers now? [y/N]: " run_scip
    case "$run_scip" in
        [yY] | [yY][eE][sS])
            echo ""
            "$SCIP_INSTALLER"
            ;;
        *)
            echo "Skipped. Run later with: $SCIP_INSTALLER"
            ;;
    esac
else
    echo "Run the companion installer when ready: $SCIP_INSTALLER"
fi
