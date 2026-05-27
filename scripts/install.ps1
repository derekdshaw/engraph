#Requires -Version 5.1
# Engraph installer for Windows.
# Resolves the engraph.exe binary relative to this script's directory
# (matches the release-archive layout), installs it under a per-user prefix,
# and wires SessionStart + PreToolUse(Bash,Grep) + PostToolUse(Read) +
# SessionEnd hooks into Claude Code's settings.json.

$ErrorActionPreference = "Stop"

$Binary       = "engraph.exe"
$ClaudeDir    = "$env:USERPROFILE\.claude"
$SettingsFile = "$ClaudeDir\settings.json"

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

Write-Host ""
Write-Host "Engraph installed successfully!" -ForegroundColor Green
Write-Host ""
Write-Host "Binary:    $Dest"
Write-Host "Settings:  $SettingsFile"
Write-Host "Database:  `$env:ENGRAPH_DB_PATH (default: %LOCALAPPDATA%\engraph\engraph.db)"
Write-Host ""
Write-Host "Sanity check:"
Write-Host "  $Dest --version"
Write-Host "  $Dest gain"
Write-Host ""
Write-Host "Next: open Claude Code in any project. SessionStart will auto-inject"
Write-Host "a brief if there's prior context for that cwd; Bash commands matching"
Write-Host "a wrapper (git log, cargo test, etc.) will be silently rewritten to"
Write-Host "route through 'engraph run'. After 'engraph index .', Grep on a"
Write-Host "bareword symbol indexed in the codegraph is redirected to"
Write-Host "'engraph subgraph <symbol>'."
Write-Host ""
Write-Host "Codegraph features (engraph index / subgraph) need external SCIP"
Write-Host "indexers — one per language. If you want them, the SCIP indexer"
Write-Host "installer is the companion script in this directory:"
Write-Host "  $ScriptDir\install-scip-indexers.sh   (run under WSL or Git Bash)"
