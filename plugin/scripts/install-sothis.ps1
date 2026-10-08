<#
.SYNOPSIS
    One-shot installer for the Sothis suite CLIs. Native PowerShell port of
    install-sothis.sh.

.DESCRIPTION
    Sothis is the local-first suite led by Sirius Forester: the foreman
    (`sirius`) claims work from an Ametrite board (`amt`), locks code through a
    Hayvenhurst code graph (`hayven`), pairs with Catryna Wikinelli (`catryna`)
    for the "why" docs, and rings PingMyBell (the desktop notch/voice app) when
    the fleet needs you. Each tool stands alone; full fleet control comes from
    all five.

    WHY this exists: the tools install five different ways. This script is the
    workhorse of "let's Sothis this up" - it installs the binaries the plugins
    can't ship, then finishes the Claude Code plugin half itself via the
    NON-interactive `claude plugin` CLI (v2.1.195+), and verifies the result:

      sirius   - signed prebuilt binary  -> this repo's install-sirius.ps1 (delegated)
      hayven   - prebuilt binary         -> Hayvenhurst's own installer (delegated)
      amt      - Rust binary (cargo)     -> detected; guided if missing (never auto-built)
      catryna  - bun-based MCP plugin    -> bun checked; the plugin comes from the
                                           marketplace bundle (auto-installed below)

    On Windows the hayven step is best-effort: Hayvenhurst has no native
    Windows installer yet (no install-hayven.ps1, and its install-hayven.sh
    refuses Git Bash / MSYS / Cygwin), so a failure there prints a WARNING
    with the manual route and the run carries on. Elsewhere it stays fatal.

    WHY a .ps1 next to the .sh: install-sothis.sh is POSIX sh and needs a POSIX
    shell. A stock Windows box - and a Claude Code agent running in a
    PowerShell-only session - may have no Git Bash at all. This file is a
    faithful port: same flags, same ordering, same delegation, same final
    plugin-half verification. Keep the two in sync.

    DELEGATION, not duplication: each binary is fetched and verified by that
    tool's OWN authoritative installer (sirius verifies a Sigstore signature;
    hayven verifies a sha256). This script never re-implements a download or a
    signature check - it orchestrates. The one supply-chain note: when the
    Hayvenhurst plugin isn't already on disk, this fetches its installer over
    HTTPS from $HAYVEN_REPO at $HAYVEN_INSTALLER_REF (default: a release TAG,
    so the fetched script is an immutable, reviewed revision rather than
    whatever `main` holds today) and runs it. That fetched script then verifies
    the hayven binary's sha256 itself. Prefer the local copy (found
    automatically) or pass -SkipHayven and run /hayvenhurst:install-binary
    yourself if you'd rather not run a fetched script at all.

    Idempotent + safe to re-run: anything already on PATH is left alone.

.PARAMETER Prefix
    Install binaries into <Prefix>\bin (forwarded to the delegated installers).
    Also SOTHIS_INSTALL_PREFIX. When neither is given, each tool's own default
    chain (TOOL_INSTALL_PREFIX > CLAUDE_PLUGIN_DATA > ~\.local) applies.

.PARAMETER Check
    Report presence of all five; never installs. Exits 0 when the foreman and
    the graph are both present, 3 otherwise.

.PARAMETER RequireSignature
    Forwarded to install-sirius.ps1; abort if the release cannot be verified.

.PARAMETER SkipHayven
    Don't touch hayven (e.g. install it via its own plugin).

.PARAMETER SkipAmt
    Don't check/guide amt.

.PARAMETER SkipPlugins
    Never run `claude plugin ...` (report only). Also
    SOTHIS_SKIP_PLUGIN_INSTALL=1.

.PARAMETER AddToPath
    Forwarded to install-sirius.ps1: add the install dir to the user PATH
    instead of only printing the command.

.EXAMPLE
    .\install-sothis.ps1
    Install every missing suite CLI, then finish and verify the plugin half.

.EXAMPLE
    .\install-sothis.ps1 -Check
    Report the whole suite's status; change nothing.

.EXAMPLE
    .\install-sothis.ps1 -RequireSignature -AddToPath
    Refuse an unverifiable sirius release, and fix PATH while we're here.

.NOTES
    Environment:
      SOTHIS_INSTALL_PREFIX        override the install prefix (same as -Prefix)
      SOTHIS_SKIP_PLUGIN_INSTALL=1 same as -SkipPlugins
      HAYVEN_REPO                  override hayven's owner/repo (default Davidb3l/Hayvenhurst-dev)
      HAYVEN_INSTALLER_REF         ref for the fetched hayven installer (default: a release tag)
      AMETRITE_REPO                shown in the amt hint (default Davidb3l/Ametrite)
      CLAUDE_PLUGINS_DIR           override ~\.claude\plugins
      Plus every variable the delegated installers honor (SIRIUS_REPO,
      SIRIUS_RELEASE_TAG, SIRIUS_INSTALL_PREFIX, HAYVEN_INSTALL_PREFIX, ...).

    Windows PowerShell 5.1 compatible. Exit codes mirror install-sothis.sh:
      0 ok, 1 error, 2 bad usage, 3 (-Check) foreman or graph missing.
#>

[CmdletBinding()]
param(
    [string]$Prefix,
    [switch]$Check,
    [switch]$RequireSignature,
    [switch]$SkipHayven,
    [switch]$SkipAmt,
    [switch]$SkipPlugins,
    [switch]$AddToPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# ---- configuration ----------------------------------------------------------

# Whether the CALLER chose a prefix (flag or SOTHIS_INSTALL_PREFIX). Only then
# do we force -Prefix onto the delegated installers; otherwise each tool's own
# default chain (TOOL_INSTALL_PREFIX > CLAUDE_PLUGIN_DATA > ~\.local) wins.
$PrefixExplicit = $false
if (-not [string]::IsNullOrEmpty($Prefix)) { $PrefixExplicit = $true }
if ([string]::IsNullOrEmpty($Prefix)) { $Prefix = $env:SOTHIS_INSTALL_PREFIX }
if (-not [string]::IsNullOrEmpty($env:SOTHIS_INSTALL_PREFIX)) { $PrefixExplicit = $true }
if ([string]::IsNullOrEmpty($Prefix)) { $Prefix = $env:CLAUDE_PLUGIN_DATA }

$UserHome = $env:USERPROFILE
if ([string]::IsNullOrEmpty($UserHome)) { $UserHome = $HOME }
if ([string]::IsNullOrEmpty($UserHome)) { $UserHome = '.' }

if ([string]::IsNullOrEmpty($Prefix)) { $Prefix = Join-Path $UserHome '.local' }
$BinDir = Join-Path $Prefix 'bin'

$HayvenRepo = $env:HAYVEN_REPO
if ([string]::IsNullOrEmpty($HayvenRepo)) { $HayvenRepo = 'Davidb3l/Hayvenhurst-dev' }
# A TAG, not `main`: the fetched-over-HTTPS installer should be an immutable,
# reviewed revision. Bump deliberately when hayven ships installer changes.
$HayvenInstallerRef = $env:HAYVEN_INSTALLER_REF
if ([string]::IsNullOrEmpty($HayvenInstallerRef)) { $HayvenInstallerRef = 'v0.0.6' }

$AmetriteRepo = $env:AMETRITE_REPO
if ([string]::IsNullOrEmpty($AmetriteRepo)) { $AmetriteRepo = 'Davidb3l/Ametrite' }

$SkipPluginInstall = [bool]$SkipPlugins
if ($env:SOTHIS_SKIP_PLUGIN_INSTALL -eq '1') { $SkipPluginInstall = $true }

$ClaudePluginsDir = $env:CLAUDE_PLUGINS_DIR
if ([string]::IsNullOrEmpty($ClaudePluginsDir)) {
    $ClaudePluginsDir = Join-Path (Join-Path $UserHome '.claude') 'plugins'
}
$ClaudeHomeDir = Join-Path $UserHome '.claude'

# Set to $true the moment a `claude plugin` command fails with the SSH-clone
# signature; the final handoff block repeats the workaround when it does.
$script:SshFailureSeen = $false

# Resolve this script's own directory so we can call its sibling
# install-sirius.ps1 regardless of the caller's cwd.
$ScriptDir = $PSScriptRoot
if ([string]::IsNullOrEmpty($ScriptDir)) {
    $ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
}

# Windows PowerShell 5.1 has no $IsWindows and only runs on Windows; 6+ defines
# it. Decides whether a hayven install failure is fatal (see Install-Hayven).
$OnWindows = $true
$isWinVar = Get-Variable -Name 'IsWindows' -ErrorAction SilentlyContinue
if ($null -ne $isWinVar) { $OnWindows = [bool]$isWinVar.Value }

# ---- tiny helpers -----------------------------------------------------------

# install-sothis.sh logs to stderr so stdout stays clean for callers; mirror it.
function Write-Log {
    param([string]$Message = '')
    [Console]::Error.WriteLine($Message)
}

function Stop-WithError {
    param([string]$Message)
    Write-Log ('install-sothis: error: ' + $Message)
    exit 1
}

function Test-HaveCommand {
    param([string]$Name)
    return ($null -ne (Get-Command $Name -ErrorAction SilentlyContinue))
}

function Get-CommandPath {
    param([string]$Name)
    $c = Get-Command $Name -ErrorAction SilentlyContinue
    if ($null -eq $c) { return $null }
    if ($c.PSObject.Properties.Name -contains 'Source' -and -not [string]::IsNullOrEmpty($c.Source)) { return $c.Source }
    return $c.Name
}

# The path of something Start-Process can actually launch. A plain Get-Command
# prefers PowerShell's own command types, so for `claude` it returns npm's
# `claude.ps1` shim (npm installs .ps1, .cmd AND an extensionless sh shim side
# by side). A .ps1 is not a Win32 executable: Start-Process throws on it, the
# caller turns that into exit -1, and every `claude plugin ...` call would be
# reported as failed. -CommandType Application restricts the lookup to real
# programs; the extension filter then keeps exactly the shapes
# Get-NativeInvocation knows how to launch (.exe/.com directly, .cmd/.bat via
# cmd.exe) and drops the extensionless sh shim. $null when nothing matches.
function Get-NativeExePath {
    param([string]$Name)
    $hits = @(Get-Command $Name -CommandType Application -All -ErrorAction SilentlyContinue)
    foreach ($h in $hits) {
        if ($null -ne $h.Path -and $h.Path -match '\.(exe|com|cmd|bat)$') { return $h.Path }
    }
    return $null
}

# Windows command-line quoting; Start-Process -ArgumentList joins an array with
# plain spaces in 5.1, which silently breaks any path containing a space.
function Format-NativeArg {
    param([string]$Value)
    if ($null -eq $Value -or $Value -eq '') { return '""' }
    if ($Value -notmatch '[\s"]') { return $Value }
    $escaped = $Value -replace '(\\*)"', '$1$1\"'
    $escaped = $escaped -replace '(\\*)$', '$1$1'
    return '"' + $escaped + '"'
}

function Get-NativeInvocation {
    param([string]$FilePath, [string[]]$ArgumentList = @())

    $exe = Get-NativeExePath $FilePath
    if ($null -eq $exe) { $exe = $FilePath }

    $quoted = @()
    foreach ($a in $ArgumentList) { $quoted += (Format-NativeArg $a) }

    # A .cmd/.bat shim (how npm installs `claude`) cannot be launched with
    # UseShellExecute=false, which is what -NoNewWindow implies. Go via cmd.exe.
    # The extra outer quotes are cmd's documented /c rule: when the string after
    # /c starts with a quote and holds more than one, cmd strips the outermost
    # pair - which is what keeps a spaced program path intact.
    if ($exe -match '\.(cmd|bat)$') {
        $inner = @((Format-NativeArg $exe)) + $quoted
        return [pscustomobject]@{ Exe = 'cmd.exe'; ArgString = ('/c "' + ($inner -join ' ') + '"') }
    }
    return [pscustomobject]@{ Exe = $exe; ArgString = ($quoted -join ' ') }
}

# Run a native program and CAPTURE its output, without `2>&1`. In Windows
# PowerShell 5.1 redirecting a native command's stderr inside the pipeline wraps
# each line in a NativeCommandError and poisons $? / -ErrorAction Stop.
# Start-Process with real redirect files sidesteps all of it, and it also lets
# us hand the child an empty stdin (the `</dev/null` the shell script uses so an
# older, prompting CLI fails fast instead of hanging).
function Invoke-NativeCapture {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [switch]$NullStdin
    )

    $inv = Get-NativeInvocation -FilePath $FilePath -ArgumentList $ArgumentList
    $outFile = [System.IO.Path]::GetTempFileName()
    $errFile = [System.IO.Path]::GetTempFileName()
    $inFile  = $null

    try {
        $sp = @{
            FilePath               = $inv.Exe
            NoNewWindow            = $true
            Wait                   = $true
            PassThru               = $true
            RedirectStandardOutput = $outFile
            RedirectStandardError  = $errFile
        }
        if (-not [string]::IsNullOrEmpty($inv.ArgString)) { $sp['ArgumentList'] = $inv.ArgString }
        if ($NullStdin) {
            $inFile = [System.IO.Path]::GetTempFileName()
            $sp['RedirectStandardInput'] = $inFile
        }

        $proc = Start-Process @sp
        $stdout = ''
        $stderr = ''
        if (Test-Path -LiteralPath $outFile) { $stdout = (Get-Content -LiteralPath $outFile -Raw -ErrorAction SilentlyContinue) }
        if (Test-Path -LiteralPath $errFile) { $stderr = (Get-Content -LiteralPath $errFile -Raw -ErrorAction SilentlyContinue) }
        if ($null -eq $stdout) { $stdout = '' }
        if ($null -eq $stderr) { $stderr = '' }

        return [pscustomobject]@{ ExitCode = $proc.ExitCode; Output = ($stdout + $stderr).Trim() }
    } catch {
        return [pscustomobject]@{ ExitCode = -1; Output = $_.Exception.Message }
    } finally {
        foreach ($f in @($outFile, $errFile, $inFile)) {
            if ($null -ne $f) { Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue }
        }
    }
}

# Run a native program and let its output STREAM to the console. Delegated
# installers print progress; swallowing that until they finish would make a
# 30-second download look like a hang.
function Invoke-NativePassthru {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$ArgumentList = @()
    )
    $inv = Get-NativeInvocation -FilePath $FilePath -ArgumentList $ArgumentList
    try {
        $sp = @{ FilePath = $inv.Exe; NoNewWindow = $true; Wait = $true; PassThru = $true }
        if (-not [string]::IsNullOrEmpty($inv.ArgString)) { $sp['ArgumentList'] = $inv.ArgString }
        $proc = Start-Process @sp
        return $proc.ExitCode
    } catch {
        Write-Log ('install-sothis: could not run ' + $FilePath + ': ' + $_.Exception.Message)
        return -1
    }
}

function Enable-Tls12 {
    try {
        [Net.ServicePointManager]::SecurityProtocol = `
            [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch {
        # Nothing sane to do; the request itself will report the real failure.
    }
}

$UserAgent = 'install-sothis.ps1 (+https://github.com/Davidb3l/Sirius-Forester)'

function Get-RemoteFile {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$OutFile
    )
    Enable-Tls12
    Invoke-WebRequest -Uri $Uri -OutFile $OutFile -UseBasicParsing -UserAgent $UserAgent -ErrorAction Stop | Out-Null
}

function Test-FileContains {
    param([string]$Path, [string]$Needle)
    if ([string]::IsNullOrEmpty($Path)) { return $false }
    if (-not (Test-Path -LiteralPath $Path)) { return $false }
    try {
        $text = Get-Content -LiteralPath $Path -Raw -ErrorAction Stop
    } catch {
        return $false
    }
    if ($null -eq $text) { return $false }
    return $text.Contains($Needle)
}

# A CLI counts as installed if it's on PATH or sitting in our install dir.
function Get-BinDirPath {
    param([string]$Name)
    return (Join-Path $BinDir ($Name + '.exe'))
}

function Test-ToolPresent {
    param([string]$Name)
    if (Test-HaveCommand $Name) { return $true }
    return (Test-Path -LiteralPath (Get-BinDirPath $Name))
}

function Get-ToolWhere {
    param([string]$Name)
    $p = Get-CommandPath $Name
    if ($null -ne $p) { return $p }
    $local = Get-BinDirPath $Name
    if (Test-Path -LiteralPath $local) { return ($local + ' (not on PATH)') }
    return 'not installed'
}

# ---- the plugin half (the audited drop-off) ---------------------------------
# The CLIs and the Claude Code plugins install SEPARATELY, and a real machine
# audit (2026-08-05) found every binary present with the sirius plugin never
# installed - the printed /plugin handoff had silently dropped. So we CHECK the
# plugin half instead of only printing instructions.
#
# Detection: ~\.claude\plugins\known_marketplaces.json is keyed by marketplace
# name; installed_plugins.json v2 keys plugins as "<name>@<marketplace>". The
# same plugin can come from DIFFERENT marketplaces (hayvenhurst@hayvenhurst vs
# hayvenhurst@sirius-forester), so match the "<name>@ prefix, never a full key.
# Only the bundle marketplace is matched by its exact name.
function Get-PluginHandoffMissing {
    $missing = @()
    if (-not (Test-FileContains (Join-Path $ClaudePluginsDir 'known_marketplaces.json') '"sirius-forester"')) {
        $missing += 'bundle-marketplace'
    }
    foreach ($p in @('sirius', 'hayvenhurst', 'catryna')) {
        if (-not (Test-FileContains (Join-Path $ClaudePluginsDir 'installed_plugins.json') ('"' + $p + '@'))) {
            $missing += ($p + '-plugin')
        }
    }
    return , $missing
}

# ---- the SSH-clone trap -----------------------------------------------------
# Observed on a real Windows box (2026-08-24): `claude plugin install` clones a
# git-subdir plugin source over SSH. On an HTTPS-only machine that fails first
# with "No ED25519 host key is known" (strict host checking) and then, once host
# keys exist, with "Permission denied (publickey)". Note the asymmetry:
# `claude plugin marketplace add` DOES fall back to HTTPS, `plugin install` does
# NOT - and this bundle's .claude-plugin\marketplace.json declares hayvenhurst
# with "source": "git-subdir", exactly the plugin type that fails.
#
# This used to be discarded, leaving the user with a bare YOU ARE NOT DONE
# block, no cause and no fix. So: surface what the CLI said, and when it smells
# like SSH, print the verified workaround as a ready-to-paste command. Mirrors
# install-sothis.sh - keep the two in sync.
function Test-PluginOutputIsSshFailure {
    param([string]$Output)
    if ([string]::IsNullOrEmpty($Output)) { return $false }
    return ($Output -match 'Permission denied \(publickey\)|host key is known|Host key verification failed|ssh: connect|git@github\.com')
}

function Write-Indented {
    param([string]$Text)
    if ([string]::IsNullOrEmpty($Text)) { return }
    foreach ($line in ($Text -split "`r?`n")) { Write-Log ('      ' + $line) }
}

# The verified workaround: an env-scoped URL rewrite. In PowerShell the env vars
# are set on the session rather than on one command line, so we also show how to
# clear them again - the user's git config is never touched either way.
function Write-SshWorkaround {
    param([string]$Command)
    $script:SshFailureSeen = $true
    Write-Log ''
    Write-Log '  ^ that is the SSH-clone failure: "claude plugin install" clones'
    Write-Log '    git-subdir plugin sources over SSH, and this machine has no usable'
    Write-Log '    GitHub SSH key. ("claude plugin marketplace add" falls back to HTTPS;'
    Write-Log '    "plugin install" does not.) Re-run it with an env-scoped rewrite that'
    Write-Log '    sends git over HTTPS instead - copy-paste this whole thing:'
    Write-Log ''
    Write-Log ('      $env:GIT_CONFIG_COUNT=1; $env:GIT_CONFIG_KEY_0=''url.https://github.com/.insteadOf''; $env:GIT_CONFIG_VALUE_0=''git@github.com:''; ' + $Command)
    Write-Log ''
    Write-Log '    Afterwards, drop them again with:'
    Write-Log '      Remove-Item Env:GIT_CONFIG_COUNT, Env:GIT_CONFIG_KEY_0, Env:GIT_CONFIG_VALUE_0'
    Write-Log '    Nothing in your git config changes.'
}

# Auto-install the plugin half when possible. Claude Code v2.1.195+ ships a
# NON-interactive `claude plugin` CLI, so this script can finish the handoff
# itself instead of printing homework. Guarded three ways: the kill-switch
# (-SkipPlugins / SOTHIS_SKIP_PLUGIN_INSTALL=1 - tests use the env so fixture
# runs never mutate real plugin state), a missing `claude` CLI (fall through to
# the printed instructions), and per-command fallbacks (an older claude that
# prompts interactively just fails fast against the empty stdin we hand it, and
# the block below picks up whatever is still missing).
function Invoke-PluginAutoInstall {
    if ($SkipPluginInstall) { return }
    if (-not (Test-HaveCommand 'claude')) { return }
    $missing = Get-PluginHandoffMissing
    if ($missing.Count -eq 0) { return }

    Write-Log ''
    Write-Log 'install-sothis: finishing the plugin half via the claude CLI (non-interactive)'

    if ($missing -contains 'bundle-marketplace') {
        $r = Invoke-NativeCapture -FilePath 'claude' -ArgumentList @('plugin', 'marketplace', 'add', 'Davidb3l/Sirius-Forester') -NullStdin
        if ($r.ExitCode -eq 0) {
            Write-Log '  added marketplace: sirius-forester'
        } else {
            Write-Log '  marketplace add failed (claude CLI too old? needs v2.1.195+) - it said:'
            Write-Indented $r.Output
            if (Test-PluginOutputIsSshFailure $r.Output) {
                Write-SshWorkaround 'claude plugin marketplace add Davidb3l/Sirius-Forester'
            }
            Write-Log '  (routes to finish it by hand are in the block below)'
        }
    }
    foreach ($p in @('sirius', 'hayvenhurst', 'catryna')) {
        if (-not ($missing -contains ($p + '-plugin'))) { continue }
        $r = Invoke-NativeCapture -FilePath 'claude' -ArgumentList @('plugin', 'install', ($p + '@sirius-forester')) -NullStdin
        if ($r.ExitCode -eq 0) {
            Write-Log ('  installed plugin: ' + $p + '@sirius-forester')
        } else {
            Write-Log ('  ' + $p + ' plugin install failed - it said:')
            Write-Indented $r.Output
            if (Test-PluginOutputIsSshFailure $r.Output) {
                Write-SshWorkaround ('claude plugin install ' + $p + '@sirius-forester')
            } else {
                Write-Log '  (routes to finish it by hand are in the block below)'
            }
        }
    }
}

# The loud finish. A printed list buried in install output is a drop-off point;
# this block is unmissable and names ONLY what is actually missing.
#
# Deliberate asymmetry rule (mirrors doctor's plugin_handoff intent): a box with
# NO ~\.claude at all is not a Claude Code machine - yelling YOU ARE NOT DONE
# about /plugin commands there is a false alarm, so we print one quiet note
# instead. But ~\.claude PRESENT with no plugins state is exactly the cold-start
# Claude Code user this block exists for - full volume.
function Write-PluginHandoffBlock {
    if (-not (Test-Path -LiteralPath $ClaudeHomeDir) -and -not (Test-Path -LiteralPath $ClaudePluginsDir)) {
        Write-Log ''
        Write-Log 'install-sothis: no Claude Code detected on this machine - skipping the'
        Write-Log '  plugin-half check. If you use Claude Code elsewhere, finish there with:'
        Write-Log '  claude plugin marketplace add Davidb3l/Sirius-Forester'
        return
    }
    $missing = Get-PluginHandoffMissing
    if ($missing.Count -eq 0) {
        Write-Log ''
        Write-Log 'install-sothis: plugin half complete - bundle marketplace + plugins present.'
        return
    }
    Write-Log ''
    Write-Log '============================================================================'
    Write-Log '  YOU ARE NOT DONE.'
    Write-Log ('  The CLIs are installed, but the Claude Code PLUGIN half is missing: ' + ($missing -join ', '))
    Write-Log ''
    Write-Log '  Any ONE of these routes finishes it:'
    Write-Log ''
    Write-Log '  a) From any shell (claude CLI v2.1.195+):'
    if ($missing -contains 'bundle-marketplace') { Write-Log '       claude plugin marketplace add Davidb3l/Sirius-Forester' }
    if ($missing -contains 'sirius-plugin')      { Write-Log '       claude plugin install sirius@sirius-forester' }
    if ($missing -contains 'hayvenhurst-plugin') { Write-Log '       claude plugin install hayvenhurst@sirius-forester' }
    if ($missing -contains 'catryna-plugin')     { Write-Log '       claude plugin install catryna@sirius-forester' }
    if ($script:SshFailureSeen) {
        Write-Log ''
        Write-Log '     This machine hit the SSH-clone failure above, so set the HTTPS'
        Write-Log '     rewrite in this session before running those commands:'
        Write-Log '       $env:GIT_CONFIG_COUNT=1; $env:GIT_CONFIG_KEY_0=''url.https://github.com/.insteadOf''; $env:GIT_CONFIG_VALUE_0=''git@github.com:'''
    }
    Write-Log ''
    Write-Log '  b) Claude DESKTOP APP (no terminal): click + next to the prompt box'
    Write-Log '     -> Plugins -> Add plugin -> add the Davidb3l/Sirius-Forester'
    Write-Log '     marketplace and install what''s listed above.'
    Write-Log ''
    Write-Log '  c) Terminal claude session: the interactive /plugin dialog.'
    Write-Log ''
    Write-Log '  Plugins are per-machine - one route, any surface, done everywhere.'
    Write-Log '  Then confirm with:  sirius doctor'
    Write-Log '============================================================================'
}

# ---- -Check: report all five, install nothing -------------------------------

# PingMyBell is a desktop app, not a PATH CLI: count it installed if the app
# bundle exists (macOS) or a `pingmybell` binary is reachable. On Windows the
# usual landing spot is %LOCALAPPDATA%\Programs.
function Get-PingMyBellWhere {
    $p = Get-CommandPath 'pingmybell'
    if ($null -ne $p) { return $p }
    if (-not [string]::IsNullOrEmpty($env:LOCALAPPDATA)) {
        $winApp = Join-Path $env:LOCALAPPDATA 'Programs\PingMyBell\PingMyBell.exe'
        if (Test-Path -LiteralPath $winApp) { return $winApp }
    }
    if (Test-Path -LiteralPath '/Applications/PingMyBell.app') { return '/Applications/PingMyBell.app' }
    return 'not installed'
}

if ($Check) {
    Write-Log 'Sothis suite status:'
    foreach ($t in @('sirius', 'hayven', 'amt', 'catryna')) {
        Write-Log ('  ' + $t + ': ' + (Get-ToolWhere $t))
    }
    # catryna is a plugin, not a PATH binary; report its runtime instead.
    $bun = Get-CommandPath 'bun'
    if ($null -ne $bun) {
        Write-Log ('  bun (catryna runtime): ' + $bun)
    } else {
        Write-Log '  bun (catryna runtime): not installed'
    }
    # pingmybell is a desktop app; report the bundle.
    Write-Log ('  pingmybell (the bell): ' + (Get-PingMyBellWhere))
    # The plugin half - the audited drop-off. Report it here too so a -Check is
    # a complete picture, not just the CLI half.
    $missing = Get-PluginHandoffMissing
    if ($missing.Count -gt 0) {
        Write-Log ('  claude code plugin half: MISSING - ' + ($missing -join ', '))
    } else {
        Write-Log '  claude code plugin half: complete'
    }
    # Exit 0 if the foreman + graph are present, 3 otherwise (mirrors the
    # per-tool -Check contract so a SessionStart hook can branch on it).
    if ((Test-ToolPresent 'sirius') -and (Test-ToolPresent 'hayven')) { exit 0 }
    exit 3
}

# ---- sirius (delegated to the bundled installer) ----------------------------
function Install-Sirius {
    if (Test-ToolPresent 'sirius') {
        Write-Log ('sirius: already installed (' + (Get-ToolWhere 'sirius') + '); skipping.')
        return
    }
    $installer = Join-Path $ScriptDir 'install-sirius.ps1'
    if (-not (Test-Path -LiteralPath $installer)) {
        Stop-WithError ('cannot find install-sirius.ps1 next to this script (' + $installer + ')')
    }
    Write-Log 'sirius: installing via install-sirius.ps1'

    # Force our prefix only when the caller chose one; otherwise let the tool's
    # own default chain (SIRIUS_INSTALL_PREFIX > CLAUDE_PLUGIN_DATA > ~\.local)
    # decide, as the header promises.
    $sirArgs = @{}
    if ($PrefixExplicit)   { $sirArgs['Prefix'] = $Prefix }
    if ($RequireSignature) { $sirArgs['RequireSignature'] = $true }
    if ($AddToPath)        { $sirArgs['AddToPath'] = $true }

    # Call the sibling script directly: `exit` inside it returns control here
    # (and sets $LASTEXITCODE) rather than killing this run, and its output
    # streams as it goes. No second powershell.exe, no execution-policy
    # surprises, no re-quoting of a path that may contain spaces.
    $global:LASTEXITCODE = 0
    try {
        & $installer @sirArgs
    } catch {
        Stop-WithError ('install-sirius.ps1 failed: ' + $_.Exception.Message)
    }
    if ($LASTEXITCODE -ne 0) { Stop-WithError 'install-sirius.ps1 failed' }
}

# ---- hayven (delegated to Hayvenhurst's own installer) ----------------------
# Prefer a copy already on disk (installed Hayvenhurst plugin); fall back to
# fetching it over HTTPS from the pinned repo. Either way, hayven's script does
# its own download + checksum verification.
#
# Layouts differ by install path:
#   marketplaces\hayvenhurst\            = a clone of Hayvenhurst-dev, so the
#                                          script sits under plugin\scripts\.
#   cache\<marketplace>\hayvenhurst\<v>\ = the INSTALLED PLUGIN root (no
#                                          plugin\ segment), so scripts\ is
#                                          top-level. <marketplace> is
#                                          `hayvenhurst` for a standalone
#                                          install and `sirius-forester` for
#                                          the Sothis bundle; accept any.
#
# A native install-hayven.ps1 wins over the .sh: on a PowerShell-only box the
# POSIX script needs an interpreter we may not have. (Hayvenhurst ships no .ps1
# today, and its .sh refuses Windows shells - so on Windows this step is
# expected to end in Stop-HayvenInstall's warning, not an abort.)
function Find-LocalHayvenInstaller {
    $roots = @(
        (Join-Path $ClaudePluginsDir 'marketplaces\hayvenhurst\plugin\scripts'),
        (Join-Path $ClaudePluginsDir 'cache\*\hayvenhurst\*\scripts')
    )
    foreach ($ext in @('ps1', 'sh')) {
        foreach ($root in $roots) {
            $hits = @(Get-ChildItem -Path (Join-Path $root ('install-hayven.' + $ext)) -File -ErrorAction SilentlyContinue)
            if ($hits.Count -gt 0) { return $hits[0].FullName }
        }
    }
    return $null
}

# The WSL launcher (%SystemRoot%\System32\bash.exe) is a `bash` on PATH on any
# box with WSL enabled - and often the FIRST one, since System32 leads PATH.
# It is not a POSIX shell for Windows paths: it starts a Linux distro, where
# C:\...\install-hayven.sh does not exist (and, if it ran, it would install a
# LINUX hayven inside WSL). Never accept anything under %SystemRoot% (that also
# covers SysWOW64 / Sysnative), nor the WindowsApps App Execution Alias stubs.
function Test-UsablePosixShell {
    param([string]$Path)
    if ([string]::IsNullOrEmpty($Path)) { return $false }
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    $full = [System.IO.Path]::GetFullPath($Path)
    if (-not [string]::IsNullOrEmpty($env:SystemRoot)) {
        $sysRoot = $env:SystemRoot.TrimEnd('\') + '\'
        if ($full.StartsWith($sysRoot, [System.StringComparison]::OrdinalIgnoreCase)) { return $false }
    }
    if ($full -match '\\Microsoft\\WindowsApps\\') { return $false }
    return $true
}

# Prefer Git for Windows' own bash (an MSYS2 bash that maps C:\ paths). Find it
# from wherever git.exe lives - the installer puts git.exe in <Git>\cmd\, and
# some setups expose <Git>\bin\ or <Git>\mingw64\bin\ instead, so walk up a few
# levels looking for <dir>\bin\bash.exe - then the standard install locations,
# then any other bash/sh on PATH that survives Test-UsablePosixShell.
function Get-PosixShell {
    $candidates = @()

    $git = Get-NativeExePath 'git'
    if ($null -ne $git) {
        $dir = Split-Path -Parent $git
        for ($i = 0; $i -lt 3 -and -not [string]::IsNullOrEmpty($dir); $i++) {
            $candidates += (Join-Path $dir 'bin\bash.exe')
            $dir = Split-Path -Parent $dir
        }
    }
    foreach ($root in @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:ProgramW6432)) {
        if (-not [string]::IsNullOrEmpty($root)) { $candidates += (Join-Path $root 'Git\bin\bash.exe') }
    }
    if (-not [string]::IsNullOrEmpty($env:LOCALAPPDATA)) {
        # Git for Windows' per-user ("only for me") install location.
        $candidates += (Join-Path $env:LOCALAPPDATA 'Programs\Git\bin\bash.exe')
    }
    foreach ($sh in @('bash', 'sh')) {
        foreach ($hit in @(Get-Command $sh -CommandType Application -All -ErrorAction SilentlyContinue)) {
            if ($null -ne $hit.Path) { $candidates += $hit.Path }
        }
    }

    foreach ($c in $candidates) {
        if (Test-UsablePosixShell $c) { return $c }
    }
    return $null
}

# On Windows a hayven failure is a WARNING, not an abort. Hayvenhurst ships no
# native install-hayven.ps1 yet, and its install-hayven.sh refuses
# MINGW/MSYS/CYGWIN ("unsupported OS"), so on this platform the delegated
# install is expected to fail - and aborting would take amt / catryna /
# sirius doctor / the plugin half down with it. Say what happened, how to get
# hayven by hand, and carry on. Anywhere else (pwsh on macOS/Linux) a failure
# is a real failure, so it stays fatal, exactly like install-sothis.sh.
function Stop-HayvenInstall {
    param([string]$Reason)
    if (-not $OnWindows) { Stop-WithError $Reason }
    Write-Log ''
    Write-Log ('hayven: WARNING: ' + $Reason)
    Write-Log '        Skipping hayven and carrying on with the rest of the suite.'
    Write-Log '        Hayvenhurst has no native Windows installer yet (its install-hayven.sh'
    Write-Log '        only covers macOS/Linux), so install hayven by hand: from'
    Write-Log ('          https://github.com/' + $HayvenRepo + '/releases/latest')
    Write-Log '        take hayvenhurst-<version>-windows-x64.tar.gz, check it against the'
    Write-Log '        .sha256 published beside it, extract it (tar -xzf), and put hayven.exe'
    Write-Log ('        (plus hayven-native.exe if present) in ' + $BinDir)
    Write-Log '        Until then sirius still runs, with reduced function: no Hayvenhurst'
    Write-Log '        code graph (locks, maps, impact), and sirius doctor will flag hayven.'
}

function Invoke-HayvenInstaller {
    param([string]$Path)

    $fwd = @()
    if ($PrefixExplicit) { $fwd = @('--prefix', $Prefix) }

    if ($Path -match '\.ps1$') {
        $hayArgs = @{}
        if ($PrefixExplicit) { $hayArgs['Prefix'] = $Prefix }
        $global:LASTEXITCODE = 0
        try {
            & $Path @hayArgs
        } catch {
            Stop-HayvenInstall ('install-hayven.ps1 failed: ' + $_.Exception.Message)
            return $false
        }
        if ($LASTEXITCODE -ne 0) {
            Stop-HayvenInstall 'install-hayven.ps1 failed'
            return $false
        }
        return $true
    }

    # A POSIX installer needs a POSIX shell. On a Git-Bash-less Windows box
    # there is nothing to run it with. That case cannot arise for
    # install-sothis.sh (it IS a POSIX shell), so it has no mirror image in the
    # shell script: rather than aborting the whole one-shot over one tool, warn
    # and carry on so amt / catryna / the plugin half still get handled.
    #
    # No "install Git for Windows and re-run" advice here any more: Git Bash's
    # bash IS what Get-PosixShell picks when present, but install-hayven.sh
    # refuses MINGW/MSYS/CYGWIN anyway, so sending the user off to install Git
    # would not get them hayven. Stop-HayvenInstall prints the route that does.
    $shell = Get-PosixShell
    if ($null -eq $shell) {
        Stop-HayvenInstall ('found only the POSIX installer (install-hayven.sh) and no usable sh/bash
        to run it with (Git for Windows'' <Git>\bin\bash.exe is used when present;
        the WSL launcher in System32 never is - it would run a LINUX install inside WSL)')
        return $false
    }
    $code = Invoke-NativePassthru -FilePath $shell -ArgumentList (@($Path) + $fwd)
    if ($code -ne 0) {
        Stop-HayvenInstall ('install-hayven.sh failed (exit ' + $code + ')')
        return $false
    }
    return $true
}

function Install-Hayven {
    if ($SkipHayven) {
        Write-Log 'hayven: -SkipHayven set; skipping.'
        return
    }
    if (Test-ToolPresent 'hayven') {
        Write-Log ('hayven: already installed (' + (Get-ToolWhere 'hayven') + '); skipping.')
        return
    }

    $local = Find-LocalHayvenInstaller
    if ($null -ne $local) {
        Write-Log ('hayven: installing via local installer (' + $local + ')')
        [void](Invoke-HayvenInstaller -Path $local)
        return
    }

    # No local copy. Fetch the installer from the pinned ref - preferring a
    # native .ps1 if Hayvenhurst ships one (it does not yet; the 404 is
    # expected), else its .sh.
    $base = 'https://raw.githubusercontent.com/' + $HayvenRepo + '/' + $HayvenInstallerRef + '/plugin/scripts/install-hayven.'
    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ('hayven-install.' + [System.Guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
    try {
        $fetched = $null
        foreach ($ext in @('ps1', 'sh')) {
            $url = $base + $ext
            $dest = Join-Path $tmpDir ('install-hayven.' + $ext)
            Write-Log ('hayven: no local installer found; trying ' + $url)
            try {
                Get-RemoteFile -Uri $url -OutFile $dest
            } catch {
                continue
            }
            if ((Test-Path -LiteralPath $dest) -and ((Get-Item -LiteralPath $dest).Length -gt 0)) {
                $fetched = $dest
                break
            }
        }
        if ($null -eq $fetched) {
            Stop-HayvenInstall ('could not download install-hayven from ' + $HayvenRepo + '@' + $HayvenInstallerRef + '.
        Install hayven yourself with /hayvenhurst:install-binary, or re-run with -SkipHayven.')
            return
        }
        [void](Invoke-HayvenInstaller -Path $fetched)
    } finally {
        Remove-Item -LiteralPath $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# ---- amt (detect only; never auto-build) ------------------------------------
function Test-Amt {
    if ($SkipAmt) {
        Write-Log 'amt: -SkipAmt set; skipping.'
        return
    }
    if (Test-ToolPresent 'amt') {
        Write-Log ('amt: already installed (' + (Get-ToolWhere 'amt') + ').')
        return
    }
    Write-Log ''
    Write-Log 'amt (Ametrite, the board): not installed. It''s a Rust binary, and this'
    Write-Log 'one-shot deliberately does NOT clone or build it for you. Get it by'
    Write-Log 'asking Claude Code to "ametrite this repo" (the ametrite skill bootstraps'
    Write-Log 'the amt CLI), or take the prebuilt Windows asset from the release:'
    Write-Log ('  https://github.com/' + $AmetriteRepo + '/releases/latest')
    Write-Log '  asset: amt-x86_64-pc-windows-msvc.zip  (verify amt-x86_64-pc-windows-msvc.zip.sha256,'
    Write-Log ('         then unzip and put amt.exe in ' + $BinDir + ')')
    Write-Log '  the release also ships cargo-dist''s amt-installer.ps1 if you prefer it'
    Write-Log 'or build it yourself:'
    Write-Log ('  git clone https://github.com/' + $AmetriteRepo + '.git')
    Write-Log ('  cd ' + (Split-Path -Leaf $AmetriteRepo) + '; cargo build --release')
    Write-Log ('  Copy-Item .\target\release\amt.exe ' + (Join-Path $BinDir 'amt.exe'))
}

# ---- catryna (a plugin; verify its bun runtime) -----------------------------
function Test-Catryna {
    # Catryna may be installed from the Sothis bundle (catryna@sirius-forester)
    # or its standalone marketplace (catryna@catryna-wikinelli) - accept either.
    $installedPlugins = Join-Path $ClaudePluginsDir 'installed_plugins.json'
    $present = (Test-FileContains $installedPlugins '"catryna@sirius-forester"') -or `
               (Test-FileContains $installedPlugins '"catryna@catryna-wikinelli"')
    if ($present) {
        Write-Log 'catryna: plugin installed.'
    } else {
        Write-Log ''
        Write-Log 'catryna (Catryna Wikinelli, the docs): plugin not installed - the'
        Write-Log '  auto-install step at the end of this run will try to fix that.'
    }
    if (-not (Test-HaveCommand 'bun')) {
        Write-Log 'catryna: WARNING: bun not found. The Catryna MCP server runs on bun;'
        Write-Log '         install it:  winget install Oven-sh.Bun   (see https://bun.sh)'
    }
}

# ---- pingmybell (desktop app; detect only, never auto-install) --------------
function Test-PingMyBell {
    $where = Get-PingMyBellWhere
    if ($where -eq 'not installed') {
        Write-Log ''
        Write-Log 'pingmybell (the bell, optional): not installed. Voice callouts + a notch'
        Write-Log 'command center for the fleet - a desktop app, built from source (early'
        Write-Log 'alpha, macOS; no prebuilt releases yet):'
        Write-Log '  https://github.com/Davidb3l/pingmybell'
    } else {
        Write-Log ('pingmybell: installed (' + $where + ').')
    }
}

# ---- PATH -------------------------------------------------------------------

function Test-DirInPathString {
    param([string]$PathValue, [string]$Dir)
    if ([string]::IsNullOrEmpty($PathValue)) { return $false }
    $needle = $Dir.TrimEnd('\', '/')
    foreach ($entry in $PathValue.Split(';')) {
        if ([string]::IsNullOrEmpty($entry)) { continue }
        if ($entry.Trim().Trim('"').TrimEnd('\', '/') -eq $needle) { return $true }
    }
    return $false
}

function Write-PathHint {
    if (Test-DirInPathString -PathValue $env:PATH -Dir $BinDir) { return }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (Test-DirInPathString -PathValue $userPath -Dir $BinDir) {
        Write-Log ''
        Write-Log ('note: ' + $BinDir + ' is already on your user PATH, but this shell was')
        Write-Log '      started before that took effect. Restart your shell (and Claude Code /'
        Write-Log '      the Claude desktop app) to pick it up.'
        return
    }
    Write-Log ''
    Write-Log ('note: ' + $BinDir + ' is not on your PATH. Add it with:')
    Write-Log ('      [Environment]::SetEnvironmentVariable(''Path'', [Environment]::GetEnvironmentVariable(''Path'',''User'') + '';' + $BinDir + ''', ''User'')')
    Write-Log '      (or re-run this installer with -AddToPath)'
    Write-Log 'note: PATH changes only reach NEW processes - restart your shell'
    Write-Log '      (and Claude Code / the Claude desktop app) to pick it up.'
}

# ---- run --------------------------------------------------------------------

Write-Log ('install-sothis: installing the Sothis suite CLIs (prefix: ' + $Prefix + ')')
Write-Log ''
Install-Sirius
Install-Hayven
Test-Amt
Test-Catryna
Test-PingMyBell

# PATH hint if our install dir isn't on PATH.
Write-PathHint

Write-Log ''
Write-Log 'install-sothis: CLI half done.'

# The foreman's health check - the suite's ground truth. It needs a .sirius\
# workspace; if there isn't one yet, point at `sirius init` instead of letting
# doctor error out.
if (Test-ToolPresent 'sirius') {
    $siriusBin = Get-CommandPath 'sirius'
    if ($null -eq $siriusBin) { $siriusBin = Get-BinDirPath 'sirius' }
    if (Test-Path -LiteralPath '.sirius') {
        Write-Log 'install-sothis: running sirius doctor'
        # `|| true` in the shell: doctor reporting problems is information, not
        # an installer failure.
        [void](Invoke-NativePassthru -FilePath $siriusBin -ArgumentList @('doctor'))
    } else {
        Write-Log 'Next: in your repo, run  sirius init  then  sirius doctor'
    }
}

# Finish the plugin half ourselves when the claude CLI allows it, then verify.
# LAST output on purpose: if anything is STILL missing after the attempt, this
# is an unmissable YOU ARE NOT DONE block.
Invoke-PluginAutoInstall
Write-PluginHandoffBlock
exit 0
