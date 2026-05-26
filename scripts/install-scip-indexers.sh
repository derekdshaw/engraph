#!/usr/bin/env bash
set -euo pipefail

# Install the external SCIP indexers that engraph's codegraph drivers shell
# out to. Each language driver in `engraph-codegraph` (RustAnalyzer,
# ScipPython, ScipGo, ScipTypescript, ScipJava) needs the matching upstream
# binary on $PATH. Without it, `engraph index <repo>` fails for that language.
#
# This script is idempotent: it checks for each binary first and skips if
# already present. It validates each indexer's prerequisite (rustup, npm, go,
# coursier) and skips with a warning if missing — rather than half-installing
# the rest of the toolchain.
#
# Pass --force to reinstall even when the binary is already on PATH.
#
# Not installed by this script:
#   - bazel + scip-bazel:  only needed for F2 Phase 2.3 (polyglot Bazel
#     monorepos), which the current engraph-codegraph crate does not drive.
#     Skip until Phase 2.3 ships.
#
# WSL note: if your `npm` is the Windows install (e.g. NVM-for-Windows
# accessed at /mnt/c/...), `npm install -g` lands the bins under a Windows
# AppData path that Linux $PATH does not see — engraph can't spawn them.
# Install Node.js inside WSL (e.g. `nvm install node` after Linux nvm setup,
# or `sudo apt install nodejs npm`) so the bins land somewhere a Linux
# process can see.

FORCE=0
for arg in "$@"; do
    case "$arg" in
        -f|--force) FORCE=1 ;;
        -h|--help)
            sed -n '3,18p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RESET='\033[0m'

info()  { printf "${CYAN}${BOLD}==> ${RESET}${BOLD}%s${RESET}\n" "$*"; }
ok()    { printf "${GREEN}${BOLD} OK ${RESET}%s\n" "$*"; }
skip()  { printf "${YELLOW}${BOLD}SKIP${RESET} %s\n" "$*"; }
warn()  { printf "${YELLOW}${BOLD}WARN${RESET} %s\n" "$*"; }
err()   { printf "${RED}${BOLD}ERR ${RESET}%s\n" "$*"; }

INSTALLED=()
SKIPPED=()
FAILED=()

have() { command -v "$1" >/dev/null 2>&1; }

# Check whether the indexer is already on PATH AND actually runnable. A
# rustup shim for an uninstalled component prints to PATH but exits non-zero
# on --version; treat that as not-installed so we proceed to install.
binary_works() {
    local bin="$1"
    have "$bin" && "$bin" --version >/dev/null 2>&1
}

run_install() {
    local name="$1"
    shift
    info "installing $name"
    if "$@"; then
        if binary_works "$name"; then
            ok "$name installed"
            INSTALLED+=("$name")
        else
            err "$name install command completed but binary still not runnable"
            FAILED+=("$name")
        fi
    else
        err "$name install failed"
        FAILED+=("$name")
    fi
}

# When `npm install -g` succeeds but the binary isn't on the calling shell's
# PATH, the most common cause on this machine is a Windows npm (NVM4W under
# /mnt/c/...) writing to a Windows AppData path. Surface a pointed hint
# instead of just "not runnable".
npm_post_install_hint() {
    local name="$1"
    local prefix
    prefix="$(npm prefix -g 2>/dev/null || true)"
    if [[ -z "$prefix" ]]; then
        return
    fi
    if [[ "$prefix" == /mnt/c/* || "$prefix" == [A-Z]:\\* ]]; then
        warn "Your npm is the Windows install (prefix: $prefix)."
        warn "Bins land at Windows paths invisible to Linux PATH; engraph can't spawn $name."
        warn "Install Node.js inside WSL (e.g. apt install nodejs npm, or nvm under Linux), then rerun."
    else
        warn "Check that $prefix/bin is on your PATH."
    fi
}

# --- rust-analyzer (Rust) ---------------------------------------------------
install_rust_analyzer() {
    if [[ $FORCE -eq 0 ]] && binary_works rust-analyzer; then
        skip "rust-analyzer already installed ($(rust-analyzer --version 2>&1))"
        SKIPPED+=("rust-analyzer")
        return
    fi
    if ! have rustup; then
        warn "rust-analyzer needs rustup; install rustup first (https://rustup.rs)"
        FAILED+=("rust-analyzer")
        return
    fi
    run_install rust-analyzer rustup component add rust-analyzer
}

# --- scip-python ------------------------------------------------------------
# Sourcegraph publishes @sourcegraph/scip-python on npm. The package is a
# Node CLI that drives Pyright internally, so npm is the supported install
# path (the binary is not on Homebrew or GitHub releases).
install_scip_python() {
    if [[ $FORCE -eq 0 ]] && binary_works scip-python; then
        skip "scip-python already installed"
        SKIPPED+=("scip-python")
        return
    fi
    if ! have npm; then
        warn "scip-python needs Node.js + npm; install Node.js first"
        FAILED+=("scip-python")
        return
    fi
    run_install scip-python npm install -g @sourcegraph/scip-python
    if ! binary_works scip-python; then
        npm_post_install_hint scip-python
    fi
}

# --- scip-go ----------------------------------------------------------------
# Module path is github.com/scip-code/scip-go since Sourcegraph transferred
# maintenance. The old github.com/sourcegraph/scip-go path errors with a
# go.mod path mismatch.
install_scip_go() {
    if [[ $FORCE -eq 0 ]] && binary_works scip-go; then
        skip "scip-go already installed"
        SKIPPED+=("scip-go")
        return
    fi
    if ! have go; then
        warn "scip-go needs the Go toolchain; install Go first (https://go.dev/dl)"
        FAILED+=("scip-go")
        return
    fi
    # If GOPATH is unset OR is the same dir as GOROOT (a misconfiguration
    # that makes `go install` try to write to system-owned paths), override
    # to $HOME/go for this install. Don't mutate the user's shell config.
    local goroot gopath user_gopath="$HOME/go"
    goroot="$(go env GOROOT 2>/dev/null || true)"
    gopath="$(go env GOPATH 2>/dev/null || true)"
    if [[ -z "$gopath" || "$gopath" == "$goroot" ]]; then
        warn "GOPATH unset or equal to GOROOT ($goroot); overriding GOPATH=$user_gopath for this install"
        mkdir -p "$user_gopath/bin"
        run_install scip-go env GOPATH="$user_gopath" \
            go install github.com/scip-code/scip-go/cmd/scip-go@latest
    else
        run_install scip-go go install github.com/scip-code/scip-go/cmd/scip-go@latest
    fi
    # If go install succeeded but `command -v scip-go` still fails, GOBIN
    # isn't on PATH; surface a clear pointer to the install dir.
    if ! binary_works scip-go; then
        local gobin
        gobin="$(GOPATH="$user_gopath" go env GOBIN 2>/dev/null || true)"
        [[ -z "$gobin" ]] && gobin="$user_gopath/bin"
        warn "scip-go landed in $gobin but isn't on PATH — add it to your shell and re-run"
    fi
}

# --- scip-typescript --------------------------------------------------------
install_scip_typescript() {
    if [[ $FORCE -eq 0 ]] && binary_works scip-typescript; then
        skip "scip-typescript already installed"
        SKIPPED+=("scip-typescript")
        return
    fi
    if ! have npm; then
        warn "scip-typescript needs Node.js + npm; install Node.js first"
        FAILED+=("scip-typescript")
        return
    fi
    run_install scip-typescript npm install -g @sourcegraph/scip-typescript
    if ! binary_works scip-typescript; then
        npm_post_install_hint scip-typescript
    fi
}

# --- scip-java --------------------------------------------------------------
# scip-java is a JVM app, distributed primarily through Coursier (`cs`).
# Coursier itself is one curl + chmod away on Linux/macOS, so install it
# first if absent. Requires a JDK to run.
install_scip_java() {
    if [[ $FORCE -eq 0 ]] && binary_works scip-java; then
        skip "scip-java already installed"
        SKIPPED+=("scip-java")
        return
    fi
    if ! have java; then
        warn "scip-java needs a JDK on PATH (java -version); install a JDK first"
        FAILED+=("scip-java")
        return
    fi
    if ! have mvn && ! have gradle; then
        warn "scip-java also needs \`mvn\` or \`gradle\` on PATH (or sbt/mill) to drive the build it indexes; install one (e.g. apt install maven) before running \`engraph index\` against a Java project. Note: scip-java does NOT auto-drive Bazel — Java-on-Bazel needs the separate scip-bazel tool (Phase 2.3)."
    fi
    if ! have cs; then
        info "installing Coursier (cs) — prerequisite for scip-java"
        local cs_dir="$HOME/.local/bin"
        mkdir -p "$cs_dir"
        local arch os cs_url
        arch="$(uname -m)"
        os="$(uname -s | tr '[:upper:]' '[:lower:]')"
        # Coursier release assets are gzipped binaries named
        # cs-<arch>-pc-linux.gz on Linux and cs-<arch>-apple-darwin.gz on macOS.
        case "$os-$arch" in
            linux-x86_64)   cs_url="https://github.com/coursier/coursier/releases/latest/download/cs-x86_64-pc-linux.gz" ;;
            linux-aarch64|linux-arm64)
                            cs_url="https://github.com/coursier/coursier/releases/latest/download/cs-aarch64-pc-linux.gz" ;;
            darwin-x86_64)  cs_url="https://github.com/coursier/coursier/releases/latest/download/cs-x86_64-apple-darwin.gz" ;;
            darwin-arm64|darwin-aarch64)
                            cs_url="https://github.com/coursier/coursier/releases/latest/download/cs-aarch64-apple-darwin.gz" ;;
            *)
                warn "no prebuilt Coursier for $os-$arch; install manually (https://get-coursier.io)"
                FAILED+=("scip-java")
                return
                ;;
        esac
        if ! have gunzip; then
            warn "gunzip not available; cannot decompress Coursier archive"
            FAILED+=("scip-java")
            return
        fi
        if ! curl -fL "$cs_url" | gunzip > "$cs_dir/cs"; then
            err "failed to download or decompress Coursier from $cs_url"
            rm -f "$cs_dir/cs"
            FAILED+=("scip-java")
            return
        fi
        chmod +x "$cs_dir/cs"
        export PATH="$cs_dir:$PATH"
        if ! have cs; then
            warn "Coursier installed to $cs_dir but not on PATH; add it to your shell profile"
            FAILED+=("scip-java")
            return
        fi
    fi
    # scip-java lives in Coursier's "contrib" apps channel, not the default.
    # Plain `cs install scip-java` fails with "Cannot find app scip-java in
    # channels io.get-coursier:apps".
    run_install scip-java cs install --contrib scip-java
    # cs writes app launchers to ~/.local/share/coursier/bin by default,
    # which isn't on PATH on a fresh setup. Surface the hint.
    if ! binary_works scip-java; then
        local cs_bin="$HOME/.local/share/coursier/bin"
        if [[ -x "$cs_bin/scip-java" ]]; then
            warn "scip-java installed to $cs_bin but not on PATH — add it to your shell profile"
        fi
    fi
}

main() {
    info "Engraph SCIP indexer installer"
    echo ""
    install_rust_analyzer
    install_scip_python
    install_scip_go
    install_scip_typescript
    install_scip_java
    echo ""
    info "Summary"
    [[ ${#INSTALLED[@]} -gt 0 ]] && ok "installed: ${INSTALLED[*]}"
    [[ ${#SKIPPED[@]}   -gt 0 ]] && skip "already present: ${SKIPPED[*]}"
    [[ ${#FAILED[@]}    -gt 0 ]] && err "needs attention: ${FAILED[*]}"
    if [[ ${#FAILED[@]} -gt 0 ]]; then
        exit 1
    fi
}

main "$@"
