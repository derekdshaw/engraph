#Requires -Version 5.1
# Engraph installer for Windows.
# Resolves the engraph.exe binary relative to this script's directory
# (matches the release-archive layout), installs it under a per-user prefix,
# and wires Claude Code hooks into settings.json plus Codex SessionStart/Stop
# hooks into hooks.json.

$ErrorActionPreference = "Stop"

$Binary       = "engraph.exe"
$ClaudeDir    = "$env:USERPROFILE\.claude"
$SettingsFile = "$ClaudeDir\settings.json"
$CodexDir     = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { "$env:USERPROFILE\.codex" }
$CodexHooksFile = "$CodexDir\hooks.json"

function Write-Info($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host " OK $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "WARN $msg" -ForegroundColor Yellow }
function Write-Err($msg)  { Write-Host "ERR $msg" -ForegroundColor Red; exit 1 }

# Resolve the script's own directory.
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path

Write-Info "Engraph installer"
Write-Host ""

# --- Locate binary ---
# Primary: same directory as this script (release archive layout).
# Fallback: ..\target\release relative to the script (running from a source
# checkout without first unzipping a release).
$BinPath = $null
$primary  = Join-Path $ScriptDir $Binary
$fallback = Join-Path $ScriptDir "..\target\release\$Binary"

if (Test-Path $primary) {
    $BinPath = (Resolve-Path $primary).Path
}
elseif (Test-Path $fallback) {
    $BinPath = (Resolve-Path $fallback).Path
}

if (-not $BinPath) {
    Write-Host "Could not find $Binary next to this script ($ScriptDir)."
    Write-Host "Either run this script from an extracted release archive, or build"
    Write-Host "first with: cargo build --release"
    Write-Host ""
    $userPath = Read-Host "Enter the path to $Binary"
    if (Test-Path $userPath) {
        $BinPath = (Resolve-Path $userPath).Path
    }
    else {
        Write-Err "Binary not found at '$userPath'."
    }
}

Write-Ok "Found binary at $BinPath"

# --- Determine install location ---

$DefaultDir = "$env:LOCALAPPDATA\Programs\engraph"
Write-Host ""
$customDir = Read-Host "Install binary to [$DefaultDir]"
if ($customDir) {
    $InstallDir = $customDir
}
else {
    $InstallDir = $DefaultDir
}

if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# --- Copy binary ---

Write-Info "Installing $Binary to $InstallDir"

# Delete-then-copy. If the existing binary is mmap'd by a running Claude
# session, Remove-Item will hit a file lock and surface a clear error so the
# user can quit those sessions before retrying — better than installing a
# half-replaced binary.
$Dest = Join-Path $InstallDir $Binary
if (Test-Path $Dest) {
    Remove-Item $Dest -Force
}
Copy-Item $BinPath $Dest
Write-Ok $Binary

# Check if install dir is on user PATH; offer to add it.
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$InstallDir*") {
    Write-Warn "$InstallDir is not on your PATH"
    $addToPath = Read-Host "Add it to your user PATH? [Y/n]"
    if ($addToPath -ne "n" -and $addToPath -ne "N") {
        [Environment]::SetEnvironmentVariable(
            "Path",
            "$userPath;$InstallDir",
            "User"
        )
        $env:Path = "$env:Path;$InstallDir"
        Write-Ok "Added to user PATH (restart your terminal for it to take effect)"
    }
}

# Build the path the way Claude Code likes — forward slashes work portably.
$EngraphPath = (Join-Path $InstallDir $Binary) -replace '\\', '/'

# --- Configure Claude Code settings.json ---

Write-Info "Configuring Claude Code hooks"

if (-not (Test-Path $ClaudeDir)) {
    New-Item -ItemType Directory -Path $ClaudeDir -Force | Out-Null
}

$settings = @{}
if (Test-Path $SettingsFile) {
    try {
        $settings = Get-Content $SettingsFile -Raw | ConvertFrom-Json -AsHashtable
    }
    catch {
        $settings = @{}
    }
}

# The hooks engraph implements today.
$hooksConfig = @{
    "SessionStart" = @(
        @{
            matcher = ""
            hooks   = @(@{ type = "command"; command = "$EngraphPath hook session-start" })
        }
    )
    "PreToolUse" = @(
        @{
            matcher = "Bash"
            hooks   = @(@{ type = "command"; command = "$EngraphPath hook pre-bash" })
        },
        @{
            matcher = "Grep"
            hooks   = @(@{ type = "command"; command = "$EngraphPath hook pre-grep" })
        }
    )
    "PostToolUse" = @(
        @{
            matcher = "Read"
            hooks   = @(@{ type = "command"; command = "$EngraphPath hook post-read" })
        }
    )
    "SessionEnd" = @(
        @{
            matcher = ""
            hooks   = @(@{ type = "command"; command = "$EngraphPath hook session-end" })
        }
    )
}

if (-not $settings.ContainsKey("hooks")) {
    $settings["hooks"] = @{}
}

foreach ($event in $hooksConfig.Keys) {
    $newEntries = $hooksConfig[$event]
    if ($settings["hooks"].ContainsKey($event)) {
        # Filter out any pre-existing engraph entry so a re-install replaces
        # rather than duplicates.
        $existing = $settings["hooks"][$event] | Where-Object {
            $hasEngraph = $false
            foreach ($h in $_.hooks) {
                if ($h.command -match "engraph") { $hasEngraph = $true }
            }
            -not $hasEngraph
        }
        if ($null -eq $existing) { $existing = @() }
        if ($existing -isnot [array]) { $existing = @($existing) }
        $settings["hooks"][$event] = @($existing) + $newEntries
    }
    else {
        $settings["hooks"][$event] = $newEntries
    }
}

$settings | ConvertTo-Json -Depth 10 | Set-Content $SettingsFile -Encoding UTF8
Write-Ok "Updated $SettingsFile"

# --- Configure Codex hooks ---

Write-Info "Configuring Codex hooks"

if (-not (Test-Path $CodexDir)) {
    New-Item -ItemType Directory -Path $CodexDir -Force | Out-Null
}

$codexSettings = @{}
if (Test-Path $CodexHooksFile) {
    try {
        $codexSettings = Get-Content $CodexHooksFile -Raw | ConvertFrom-Json -AsHashtable
    }
    catch {
        $codexSettings = @{}
    }
}

$codexHooksConfig = @{
    "SessionStart" = @(
        @{
            matcher = ""
            hooks   = @(@{ type = "command"; command = "$EngraphPath hook session-start --client codex" })
        }
    )
    "Stop" = @(
        @{
            hooks = @(@{ type = "command"; command = "$EngraphPath hook session-end" })
        }
    )
}

if (-not $codexSettings.ContainsKey("hooks")) {
    $codexSettings["hooks"] = @{}
}

foreach ($event in $codexHooksConfig.Keys) {
    $newEntries = $codexHooksConfig[$event]
    if ($codexSettings["hooks"].ContainsKey($event)) {
        $existing = $codexSettings["hooks"][$event] | Where-Object {
            $hasEngraph = $false
            foreach ($h in $_.hooks) {
                if ($h.command -match "engraph") { $hasEngraph = $true }
            }
            -not $hasEngraph
        }
        if ($null -eq $existing) { $existing = @() }
        if ($existing -isnot [array]) { $existing = @($existing) }
        $codexSettings["hooks"][$event] = @($existing) + $newEntries
    }
    else {
        $codexSettings["hooks"][$event] = $newEntries
    }
}

$codexSettings | ConvertTo-Json -Depth 10 | Set-Content $CodexHooksFile -Encoding UTF8
Write-Ok "Updated $CodexHooksFile"

# --- Install memory-capture guidance ---
# The writer commands (remember/bug/save) only get called if Claude is told
# when to call them. Ship a guidance file and import it from CLAUDE.md so the
# guidance loads in every session. engraph becomes the single capture path.

Write-Info "Installing memory-capture guidance"

$EngraphMd = Join-Path $ClaudeDir "engraph.md"

# Copy the shipped guidance file rather than emitting it inline, so
# docs/engraph.md stays the single source of truth. Resolve it like the binary:
# next to this script (release archive layout), else ..\docs (source checkout).
$EngraphMdSrc = $null
$mdPrimary  = Join-Path $ScriptDir "engraph.md"
$mdFallback = Join-Path $ScriptDir "..\docs\engraph.md"
if (Test-Path $mdPrimary) {
    $EngraphMdSrc = (Resolve-Path $mdPrimary).Path
}
elseif (Test-Path $mdFallback) {
    $EngraphMdSrc = (Resolve-Path $mdFallback).Path
}

if ($EngraphMdSrc) {
    Copy-Item $EngraphMdSrc $EngraphMd -Force
    Write-Ok "Wrote $EngraphMd (from $EngraphMdSrc)"

    $ClaudeMd = Join-Path $ClaudeDir "CLAUDE.md"
    if (-not (Test-Path $ClaudeMd) -or
        -not (Select-String -Path $ClaudeMd -Pattern '@engraph.md' -SimpleMatch -Quiet)) {
        Add-Content -Path $ClaudeMd -Value "`n@engraph.md"
        Write-Ok "Imported @engraph.md in $ClaudeMd"
    }
    else {
        Write-Info "@engraph.md already imported in $ClaudeMd"
    }
}
else {
    Write-Warn "Could not find docs/engraph.md next to the installer; skipping memory guidance"
}

# --- Install optional Claude Code skill(s) ---
# Ship the `engraph-refresh` skill (reindex embeddings + opt-in code-graph
# rebuild) into the user's skill directory, opt-in. Resolve the source like the
# binary/md: next to this script (release archive layout), else ..\skills.
Write-Info "Optional Claude Code skill"

$SkillSrc = $null
$skillPrimary  = Join-Path $ScriptDir "skills\engraph-refresh\SKILL.md"
$skillFallback = Join-Path $ScriptDir "..\skills\engraph-refresh\SKILL.md"
if (Test-Path $skillPrimary) {
    $SkillSrc = (Resolve-Path $skillPrimary).Path
}
elseif (Test-Path $skillFallback) {
    $SkillSrc = (Resolve-Path $skillFallback).Path
}

if ($SkillSrc) {
    $runSkill = Read-Host "Install the 'engraph-refresh' skill (reindex embeddings + optional code-graph rebuild)? [y/N]"
    if ($runSkill -eq "y" -or $runSkill -eq "Y" -or $runSkill -eq "yes") {
        $SkillDest = Join-Path $ClaudeDir "skills\engraph-refresh"
        if (-not (Test-Path $SkillDest)) {
            New-Item -ItemType Directory -Path $SkillDest -Force | Out-Null
        }
        Copy-Item $SkillSrc (Join-Path $SkillDest "SKILL.md") -Force
        Write-Ok "Installed skill to $SkillDest"
    }
    else {
        Write-Host "Skipped. Install later by copying $SkillSrc to $ClaudeDir\skills\"
    }
}
else {
    Write-Warn "Could not find skills\engraph-refresh next to the installer; skipping skill"
}

Write-Host ""
Write-Host "Engraph installed successfully!" -ForegroundColor Green
Write-Host ""
Write-Host "Binary:    $Dest"
Write-Host "Claude:    $SettingsFile"
Write-Host "Codex:     $CodexHooksFile"
Write-Host "Memory:    $EngraphMd (imported via @engraph.md)"
Write-Host "Database:  `$env:ENGRAPH_DB_PATH (default: %LOCALAPPDATA%\engraph\engraph.db)"
Write-Host ""
Write-Host "Sanity check:"
Write-Host "  $Dest --version"
Write-Host "  $Dest gain"
Write-Host ""
Write-Host "Next: open Claude Code or Codex in any project. SessionStart will"
Write-Host "auto-inject a brief if there's prior context for that cwd. In Claude,"
Write-Host "Bash commands matching a wrapper (git log, cargo test, etc.) will be"
Write-Host "silently rewritten to route through 'engraph run'. After"
Write-Host "'engraph index .', Grep on a bareword symbol indexed in the codegraph"
Write-Host "is redirected to 'engraph subgraph <symbol>'."
Write-Host ""
Write-Host "Codegraph features (engraph index / subgraph) need external SCIP"
Write-Host "indexers — one per language (rust-analyzer, scip-python, scip-go,"
Write-Host "scip-typescript, scip-java). The installer is a bash script, so it"
Write-Host "needs WSL or Git Bash."

$scipInstaller = Join-Path $ScriptDir "install-scip-indexers.sh"
if (Test-Path $scipInstaller) {
    Write-Host ""
    $runScip = Read-Host "Install the SCIP indexers now? (needs WSL or Git Bash) [y/N]"
    if ($runScip -eq "y" -or $runScip -eq "Y" -or $runScip -eq "yes") {
        $bash = Get-Command bash -ErrorAction SilentlyContinue
        if ($bash) {
            & $bash.Source $scipInstaller
        }
        else {
            Write-Warn "No 'bash' found on PATH. Install WSL or Git Bash, then run:"
            Write-Host "  bash `"$scipInstaller`""
        }
    }
    else {
        Write-Host "Skipped. Run later (under WSL or Git Bash) with:"
        Write-Host "  bash `"$scipInstaller`""
    }
}
