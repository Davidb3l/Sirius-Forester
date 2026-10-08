<#
.SYNOPSIS
    Download + install the platform-correct `sirius` binary from a Sirius
    Forester GitHub Release. Native PowerShell port of install-sirius.sh.

.DESCRIPTION
    WHY this exists: the Claude Code plugin is git-based, so installing the
    plugin only clones the repo's text files (the Agent Skill). It does NOT
    deliver the compiled `sirius` CLI - that is platform-specific and large,
    and is deliberately NOT committed to git. This script is the bridge:
    detect the platform, map to the matching release tarball asset (mirroring
    .github/workflows/release.yml's platform matrix), download it, verify its
    sha256 and its Sigstore signature, and install the binary into a known
    location.

    WHY a .ps1 next to the .sh: install-sirius.sh is POSIX sh and needs a
    POSIX shell. A stock Windows box - and a Claude Code agent running in a
    PowerShell-only session - may have no Git Bash at all. The Windows tarball
    (sirius-forester-<VER>-windows-x64.tar.gz) has always shipped; only the
    installer could not run. This file is a faithful port of install-sirius.sh:
    same flags, same ordering, same security posture. Keep the two in sync.

    SECURITY: the `<tarball>.sha256` is served from the same origin as the
    tarball, so on its own it only catches a corrupted download, not a tampered
    release: anyone who can replace the tarball can replace its checksum too.
    Authenticity comes from the Sigstore bundle (`<tarball>.sigstore.json`),
    whose Fulcio certificate binds the artifact to THIS repo's release
    workflow. We pin both the signer identity and the OIDC issuer; otherwise an
    attacker could sign a malicious tarball with their own identity and it
    would still "verify".

    A bad signature ALWAYS aborts. A MISSING bundle ALWAYS aborts: the tarball
    came from the same origin, every release publishes a bundle, so "tarball
    but no bundle" is a signature-stripping downgrade, not a benign 404.

    The one soft case is a box with no verifier installed (`cosign` or
    `sigstore`): we cannot check, so we warn loudly and continue on TLS plus
    the checksum. An attacker cannot induce that state remotely (it depends on
    what is installed locally). Pass -RequireSignature (or set
    SIRIUS_REQUIRE_SIGNATURE=1) to make it fatal too.

    NOTE: the trust anchor follows SIRIUS_REPO. Overriding it points both the
    download AND the expected signer at that repo, so verification then only
    proves "that repo signed its own artifact". Do not set it to a repo you do
    not trust.

    Idempotent + safe to re-run.

.PARAMETER Version
    Install a specific release tag (e.g. v0.1.1). Also SIRIUS_RELEASE_TAG.
    A bare "0.1.1" is accepted and normalised to "v0.1.1".

.PARAMETER Prefix
    Install into <Prefix>\bin. Also SIRIUS_INSTALL_PREFIX. Default chain:
    SIRIUS_INSTALL_PREFIX > CLAUDE_PLUGIN_DATA > $env:USERPROFILE\.local.

.PARAMETER Check
    Print status only; never downloads. Exits 0 if `sirius` is on PATH or
    already installed in the prefix, 3 if missing.

.PARAMETER RequireSignature
    Abort unless the Sigstore signature actually verifies - i.e. make a
    MISSING verifier fatal too. Also SIRIUS_REQUIRE_SIGNATURE=1.
    (A BAD signature and a MISSING bundle are fatal regardless.)

.PARAMETER AddToPath
    When the install dir is not on the user PATH, add it via
    [Environment]::SetEnvironmentVariable('Path', ..., 'User'). Without this
    switch the exact command is printed instead and nothing is changed.

.PARAMETER Force
    Reinstall even when the requested version is already installed.

.PARAMETER DryRun
    Print what would be downloaded/installed and exit, without network I/O.
    Same as SIRIUS_INSTALL_DRY_RUN=1, and the same effect as -WhatIf.

.EXAMPLE
    .\install-sirius.ps1
    Download + install the latest release.

.EXAMPLE
    .\install-sirius.ps1 -Check
    Report what is installed; change nothing.

.EXAMPLE
    .\install-sirius.ps1 -Version v0.1.1 -RequireSignature -AddToPath
    Install a pinned tag, refuse to proceed unverified, and fix PATH.

.NOTES
    Environment:
      SIRIUS_INSTALL_PREFIX      override the install prefix (same as -Prefix)
      SIRIUS_RELEASE_TAG         pin a release tag (same as -Version)
      SIRIUS_REPO                override owner/repo (default Davidb3l/Sirius-Forester)
      SIRIUS_REQUIRE_SIGNATURE=1 same as -RequireSignature
      SIRIUS_INSTALL_DRY_RUN=1   same as -DryRun
      CLAUDE_PLUGIN_DATA         plugin-managed data dir (default prefix when set)

    Windows PowerShell 5.1 compatible. Exit codes mirror install-sirius.sh:
      0 ok / already installed, 1 error, 2 bad usage, 3 (-Check) not installed.
#>

[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$Version,
    [string]$Prefix,
    [switch]$Check,
    [switch]$RequireSignature,
    [switch]$AddToPath,
    [switch]$Force,
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar makes downloads an order of magnitude slower
# in Windows PowerShell 5.1.
$ProgressPreference = 'SilentlyContinue'

# ---- configuration ----------------------------------------------------------

$Repo = $env:SIRIUS_REPO
if ([string]::IsNullOrEmpty($Repo)) { $Repo = 'Davidb3l/Sirius-Forester' }

$Tag = $Version
if ([string]::IsNullOrEmpty($Tag)) { $Tag = $env:SIRIUS_RELEASE_TAG }

# Default install prefix: ${CLAUDE_PLUGIN_DATA} when invoked by the plugin
# (persists across plugin updates), else ~\.local. We install binaries into
# <prefix>\bin.
if ([string]::IsNullOrEmpty($Prefix)) { $Prefix = $env:SIRIUS_INSTALL_PREFIX }
if ([string]::IsNullOrEmpty($Prefix)) { $Prefix = $env:CLAUDE_PLUGIN_DATA }
if ([string]::IsNullOrEmpty($Prefix)) {
    $homeDir = $env:USERPROFILE
    if ([string]::IsNullOrEmpty($homeDir)) { $homeDir = $HOME }
    if ([string]::IsNullOrEmpty($homeDir)) {
        # Mirrors install-hayven.sh's "no defensible install location" stance:
        # say so rather than silently installing into \.local\bin.
        [Console]::Error.WriteLine('install-sirius: error: cannot determine a home directory. Pass -Prefix <dir> or set SIRIUS_INSTALL_PREFIX.')
        exit 1
    }
    $Prefix = Join-Path $homeDir '.local'
}

$BinDir  = Join-Path $Prefix 'bin'
$BinName = 'sirius.exe'
$BinPath = Join-Path $BinDir $BinName

# Make a missing verifier fatal. A BAD signature is fatal regardless.
$RequireSig = [bool]$RequireSignature
if ($env:SIRIUS_REQUIRE_SIGNATURE -eq '1') { $RequireSig = $true }

# -WhatIf is treated as the dry run: cheap, and it keeps `-WhatIf` from being a
# lie (nothing this script does is undoable-by-preview otherwise).
$IsDryRun = [bool]$DryRun
if ($env:SIRIUS_INSTALL_DRY_RUN -eq '1') { $IsDryRun = $true }
if ($WhatIfPreference) { $IsDryRun = $true }

$CertIssuer = 'https://token.actions.githubusercontent.com'

# ---- tiny helpers -----------------------------------------------------------

# install-sirius.sh logs to stderr so stdout stays clean for callers; mirror it.
function Write-Log {
    param([string]$Message = '')
    [Console]::Error.WriteLine($Message)
}

function Stop-WithError {
    param([string]$Message)
    Write-Log ('install-sirius: error: ' + $Message)
    exit 1
}

# `command -v <x> >/dev/null 2>&1`
function Test-HaveCommand {
    param([string]$Name)
    $c = Get-Command $Name -ErrorAction SilentlyContinue
    return ($null -ne $c)
}

function Get-CommandPath {
    param([string]$Name)
    $c = Get-Command $Name -ErrorAction SilentlyContinue
    if ($null -eq $c) { return $null }
    if ($c.PSObject.Properties.Name -contains 'Source' -and -not [string]::IsNullOrEmpty($c.Source)) { return $c.Source }
    return $c.Name
}

# False when the resolved command is a Windows "App Execution Alias" stub: a
# 0-byte reparse point that launches the Microsoft Store instead of a program.
function Test-SafeToProbe {
    param([string]$Name)
    $p = Get-CommandPath $Name
    if ($null -eq $p) { return $false }
    try {
        $item = Get-Item -LiteralPath $p -Force -ErrorAction Stop
    } catch {
        return $true   # can't tell; let the probe decide
    }
    if ($item.PSObject.Properties.Name -contains 'Length' -and $item.Length -eq 0 -and
        ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        return $false
    }
    return $true
}

# Windows command-line quoting. Start-Process -ArgumentList joins with plain
# spaces in 5.1, which silently breaks any path containing a space (and this
# repo lives under "0 - Code"). Quote every argument ourselves.
function Format-NativeArg {
    param([string]$Value)
    if ($null -eq $Value) { return '""' }
    if ($Value -eq '') { return '""' }
    if ($Value -notmatch '[\s"]') { return $Value }
    # Double any backslash run that immediately precedes a quote, then escape
    # the quote; finally double a trailing backslash run (it would otherwise
    # escape our closing quote).
    $escaped = $Value -replace '(\\*)"', '$1$1\"'
    $escaped = $escaped -replace '(\\*)$', '$1$1'
    return '"' + $escaped + '"'
}

# Run a native program and capture its output WITHOUT `2>&1`. In Windows
# PowerShell 5.1 redirecting a native command's stderr inside the pipeline
# wraps each line in a NativeCommandError and poisons $? / $ErrorActionPreference
# = 'Stop'. Start-Process with real redirect files sidesteps all of it, and it
# also lets us hand the child an empty stdin (the `</dev/null` the shell script
# uses so an older, prompting CLI fails fast instead of hanging).
function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [switch]$NullStdin
    )

    $exe = Get-CommandPath $FilePath
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
        $argString = '/c "' + ($inner -join ' ') + '"'
        $exe = 'cmd.exe'
    } else {
        $argString = ($quoted -join ' ')
    }

    $outFile = [System.IO.Path]::GetTempFileName()
    $errFile = [System.IO.Path]::GetTempFileName()
    $inFile  = $null

    try {
        $sp = @{
            FilePath               = $exe
            NoNewWindow            = $true
            Wait                   = $true
            PassThru               = $true
            RedirectStandardOutput = $outFile
            RedirectStandardError  = $errFile
        }
        if (-not [string]::IsNullOrEmpty($argString)) { $sp['ArgumentList'] = $argString }
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

        return [pscustomobject]@{
            ExitCode = $proc.ExitCode
            Output   = ($stdout + $stderr).Trim()
        }
    } catch {
        return [pscustomobject]@{
            ExitCode = -1
            Output   = $_.Exception.Message
        }
    } finally {
        foreach ($f in @($outFile, $errFile, $inFile)) {
            if ($null -ne $f) { Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue }
        }
    }
}

# Windows PowerShell 5.1 negotiates SSL3/TLS1.0 by default on some boxes;
# github.com and the API both require TLS 1.2+.
function Enable-Tls12 {
    try {
        [Net.ServicePointManager]::SecurityProtocol = `
            [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch {
        # Nothing sane to do; the request below will report the real failure.
    }
}

$UserAgent = 'install-sirius.ps1 (+https://github.com/Davidb3l/Sirius-Forester)'

function Get-RemoteFile {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$OutFile
    )
    Enable-Tls12
    Invoke-WebRequest -Uri $Uri -OutFile $OutFile -UseBasicParsing -UserAgent $UserAgent -ErrorAction Stop | Out-Null
}

function Get-RemoteString {
    param([Parameter(Mandatory = $true)][string]$Uri)
    Enable-Tls12
    $resp = Invoke-WebRequest -Uri $Uri -UseBasicParsing -UserAgent $UserAgent -ErrorAction Stop
    return [string]$resp.Content
}

# ---- suite awareness --------------------------------------------------------
# Sirius is the foreman: it dispatches work on an Ametrite board and locks code
# through a Hayvenhurst graph, with Catryna holding the "why" docs. Nudge (one
# short block, only when something is missing) toward the full suite - full
# fleet control needs all four.
#
# Test-SuiteRepo: true when the cwd already uses any suite tool. The
# SessionStart -Check runs in EVERY repo; the nudge stays quiet outside suite
# repos so it never nags unrelated projects.

$ClaudePluginsDir = $env:CLAUDE_PLUGINS_DIR
if ([string]::IsNullOrEmpty($ClaudePluginsDir)) {
    $userHome = $env:USERPROFILE
    if ([string]::IsNullOrEmpty($userHome)) { $userHome = $HOME }
    if ([string]::IsNullOrEmpty($userHome)) { $userHome = '.' }
    $ClaudePluginsDir = Join-Path (Join-Path $userHome '.claude') 'plugins'
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

function Test-SuiteRepo {
    # .docs\ alone is too generic a name; require Catryna's index file.
    if (Test-Path -LiteralPath '.sirius')   { return $true }
    if (Test-Path -LiteralPath '.ametrite') { return $true }
    if (Test-Path -LiteralPath '.hayven')   { return $true }
    if (Test-Path -LiteralPath (Join-Path '.docs' '_index.json')) { return $true }
    return $false
}

function Write-SuiteHint {
    $missing = @()
    if (-not (Test-HaveCommand 'amt'))    { $missing += 'Ametrite' }
    if (-not (Test-HaveCommand 'hayven')) { $missing += 'Hayvenhurst' }
    # Prefix match: catryna installs as `catryna@<marketplace>` and there are
    # two legitimate marketplaces (the Sothis bundle `sirius-forester`, and its
    # own `catryna-wikinelli`). Pinning one key nags the other's users forever.
    if (-not (Test-FileContains (Join-Path $ClaudePluginsDir 'installed_plugins.json') '"catryna@')) {
        $missing += 'Catryna'
    }
    if ($missing.Count -eq 0) { return }

    Write-Log ''
    Write-Log ('fleet suite: missing: ' + ($missing -join ' ') + '. Sirius is the foreman; for full fleet control install the whole suite (one-shot: /sirius:install-suite):')
    # Anyone running this script has the sirius plugin, hence the
    # sirius-forester marketplace: the Sothis bundle entries install with no
    # extra marketplace add.
    if ($missing -contains 'Hayvenhurst') { Write-Log '  Hayvenhurst (code graph): claude plugin install hayvenhurst@sirius-forester, then /hayvenhurst:install-binary' }
    if ($missing -contains 'Catryna')     { Write-Log '  Catryna Wikinelli (code wiki): claude plugin install catryna@sirius-forester' }
    if ($missing -contains 'Ametrite')    { Write-Log '  Ametrite (task board): ask Claude to "ametrite this repo" (the skill bootstraps the amt CLI)' }
}

# ---- platform detection -> release asset name -------------------------------
# Mirrors the matrix in .github/workflows/release.yml:
#   linux-x64-glibc  linux-arm64  macos-x64  macos-arm64  windows-x64
# Tarball asset name: sirius-forester-<version>-<platform>.tar.gz
#   (version = tag with the leading "v" stripped)
#
# This file covers the windows-x64 slot. PowerShell 7 also runs on Linux and
# macOS, but those platforms already have install-sirius.sh (which handles
# their shells, their PATH files and their arch split), so point there rather
# than maintaining a second, weaker copy of that logic.
function Get-Platform {
    $onWindows = $true
    $v = Get-Variable -Name 'IsWindows' -ErrorAction SilentlyContinue
    if ($null -ne $v) { $onWindows = [bool]$v.Value }  # PowerShell 6+ only
    if (-not $onWindows) {
        Stop-WithError 'this PowerShell is not running on Windows. Use install-sirius.sh (the POSIX installer) on macOS and Linux.'
    }

    $arch = $env:PROCESSOR_ARCHITECTURE
    if ([string]::IsNullOrEmpty($arch)) { $arch = 'AMD64' }
    switch ($arch.ToUpperInvariant()) {
        'AMD64' { return 'windows-x64' }
        'X86'   {
            # A 32-bit PowerShell on 64-bit Windows: PROCESSOR_ARCHITEW6432
            # still reports the real machine.
            $native = $env:PROCESSOR_ARCHITEW6432
            if (-not [string]::IsNullOrEmpty($native)) { return 'windows-x64' }
            Stop-WithError 'unsupported CPU arch ''x86'' (the release matrix publishes windows-x64 only)'
        }
        'ARM64' {
            # There is no windows-arm64 release target; Windows on ARM runs the
            # x64 build under emulation. Say so rather than failing outright.
            Write-Log 'install-sirius: note: no windows-arm64 release exists; using the windows-x64 build (runs under Windows'' x64 emulation).'
            return 'windows-x64'
        }
    }
    Stop-WithError ('unsupported CPU arch ''' + $arch + '''')
}

# Resolve "latest" to a concrete tag. Primary: the public GitHub API. Fallback:
# the /releases/latest redirect, which needs no token and survives a rate-limited
# API.
function Resolve-LatestTag {
    if (-not [string]::IsNullOrEmpty($Tag)) { return $Tag }

    $resolved = $null
    Enable-Tls12
    try {
        $rel = Invoke-RestMethod -Uri ('https://api.github.com/repos/' + $Repo + '/releases/latest') `
            -UseBasicParsing -UserAgent $UserAgent -ErrorAction Stop
        if ($null -ne $rel -and $rel.PSObject.Properties.Name -contains 'tag_name') {
            $resolved = [string]$rel.tag_name
        }
    } catch {
        $resolved = $null
    }

    if ([string]::IsNullOrEmpty($resolved)) {
        try {
            $resp = Invoke-WebRequest -Uri ('https://github.com/' + $Repo + '/releases/latest') `
                -UseBasicParsing -UserAgent $UserAgent -ErrorAction Stop
            $final = ''
            $base = $resp.BaseResponse
            if ($null -ne $base) {
                if ($base.PSObject.Properties.Name -contains 'ResponseUri' -and $null -ne $base.ResponseUri) {
                    $final = [string]$base.ResponseUri.AbsoluteUri          # 5.1
                } elseif ($base.PSObject.Properties.Name -contains 'RequestMessage' -and $null -ne $base.RequestMessage) {
                    $final = [string]$base.RequestMessage.RequestUri.AbsoluteUri  # 7+
                }
            }
            if ($final -match '/releases/tag/(.+)$') { $resolved = $Matches[1] }
        } catch {
            $resolved = $null
        }
    }

    if ([string]::IsNullOrEmpty($resolved)) {
        Stop-WithError ('could not resolve the latest release tag for ' + $Repo + ' (pass -Version vX.Y.Z)')
    }
    return $resolved
}

# ---- PATH -------------------------------------------------------------------

# Two PATHs matter and they disagree constantly on Windows: the CURRENT
# process's copy (inherited at launch) and the persisted USER value (what new
# shells will get). "Already added, just stale here" is a different message
# from "never added", so answer them separately.
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

function Test-DirOnProcessPath {
    param([string]$Dir)
    return (Test-DirInPathString -PathValue $env:PATH -Dir $Dir)
}

function Test-DirOnUserPath {
    param([string]$Dir)
    return (Test-DirInPathString -PathValue ([Environment]::GetEnvironmentVariable('Path', 'User')) -Dir $Dir)
}

$PathAddCommand = '[Environment]::SetEnvironmentVariable(''Path'', [Environment]::GetEnvironmentVariable(''Path'',''User'') + '';' + $BinDir + ''', ''User'')'

function Add-BinDirToUserPath {
    # Writes the USER (not machine) Path only - no elevation, no other user
    # affected, and reversible from the same API.
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $current) { $current = '' }
    if (Test-DirInPathString -PathValue $current -Dir $BinDir) {
        Write-Log ('install-sirius: ' + $BinDir + ' is already on your user PATH.')
        return
    }
    if ($current -eq '') {
        $updated = $BinDir
    } else {
        $updated = $current.TrimEnd(';') + ';' + $BinDir
    }
    try {
        [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
    } catch {
        Write-Log ('install-sirius: WARNING: could not update the user PATH: ' + $_.Exception.Message)
        Write-Log ('install-sirius: WARNING: add it yourself with:')
        Write-Log ('      ' + $PathAddCommand)
        return
    }
    # Make it usable in THIS process too; the persisted value only reaches new
    # processes.
    if ([string]::IsNullOrEmpty($env:PATH)) {
        $env:PATH = $BinDir
    } else {
        $env:PATH = $env:PATH.TrimEnd(';') + ';' + $BinDir
    }
    Write-Log ('install-sirius: added ' + $BinDir + ' to your user PATH.')
}

function Write-PathHint {
    if (Test-DirOnProcessPath $BinDir) { return }
    if (Test-DirOnUserPath $BinDir) {
        Write-Log ''
        Write-Log ('note: ' + $BinDir + ' is already on your user PATH, but this shell was')
        Write-Log '      started before that took effect. Restart your shell (and Claude Code /'
        Write-Log '      the Claude desktop app) to pick it up.'
        return
    }
    Write-Log ''
    if ($AddToPath) {
        if ($IsDryRun) {
            Write-Log ('DRY RUN: would add ' + $BinDir + ' to your user PATH')
        } else {
            Add-BinDirToUserPath
        }
    } else {
        Write-Log ('note: ' + $BinDir + ' is not on your PATH. Add it with:')
        Write-Log ('      ' + $PathAddCommand)
        Write-Log '      (or re-run this installer with -AddToPath)'
    }
    Write-Log 'note: PATH changes only reach NEW processes - restart your shell'
    Write-Log '      (and Claude Code / the Claude desktop app) to pick it up.'
}

# ---- installed-version probe ------------------------------------------------

function Get-InstalledVersion {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    try {
        $r = Invoke-Native -FilePath $Path -ArgumentList @('--version')
    } catch {
        return $null
    }
    if ($r.ExitCode -ne 0) { return $null }
    if ($r.Output -match '(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.\-]+)?)') { return $Matches[1] }
    return $null
}

# ---- -Check: status only, never downloads -----------------------------------

if ($Check) {
    $onPath = Get-CommandPath 'sirius'
    if ($null -ne $onPath) {
        $v = Get-InstalledVersion $onPath
        if ($null -ne $v) {
            Write-Log ('sirius: already on PATH (' + $onPath + ', version ' + $v + ')')
        } else {
            Write-Log ('sirius: already on PATH (' + $onPath + ')')
        }
        if (Test-SuiteRepo) { Write-SuiteHint }
        exit 0
    }
    if (Test-Path -LiteralPath $BinPath) {
        $v = Get-InstalledVersion $BinPath
        if ($null -ne $v) {
            Write-Log ('sirius: installed at ' + $BinPath + ' (version ' + $v + ', not on PATH)')
        } else {
            Write-Log ('sirius: installed at ' + $BinPath + ' (not on PATH)')
        }
        Write-PathHint
        if (Test-SuiteRepo) { Write-SuiteHint }
        exit 0
    }
    Write-Log 'sirius: not installed. Run /sirius:install-binary (or plugin\scripts\install-sirius.ps1) to install it.'
    if (Test-SuiteRepo) { Write-SuiteHint }
    exit 3
}

# ---- signature verification --------------------------------------------------
# Verify with whichever Sigstore verifier is on the box. Pin BOTH the signer
# identity (this repo's release.yml, at this tag) and the OIDC issuer: an
# unpinned verify only proves "somebody signed this", not "the release workflow
# signed this".
function Invoke-SignatureVerification {
    param(
        [Parameter(Mandatory = $true)][string]$Bundle,
        [Parameter(Mandatory = $true)][string]$Artifact,
        [Parameter(Mandatory = $true)][string]$AssetName,
        [Parameter(Mandatory = $true)][string]$ReleaseTag
    )

    $identity = 'https://github.com/' + $Repo + '/.github/workflows/release.yml@refs/tags/' + $ReleaseTag
    $sigFail = @"
SIGNATURE VERIFICATION FAILED for $AssetName
        expected signer: $identity
        expected issuer: $CertIssuer
        Refusing to install: this artifact was not produced by $Repo's release workflow.
"@

    if (Test-HaveCommand 'cosign') {
        Write-Log 'install-sirius: verifying signature (cosign)'
        # Keep the verifier's own diagnostics: on a real identity mismatch
        # cosign prints "expected X, got Y", and an OLD cosign (< 3.x) instead
        # fails to parse sigstore-python v3's `.sigstore.json` bundle at all.
        # Swallowing both makes a stale toolchain look identical to a tampered
        # artifact.
        $r = Invoke-Native -FilePath 'cosign' -ArgumentList @(
            'verify-blob',
            '--bundle', $Bundle,
            '--certificate-identity', $identity,
            '--certificate-oidc-issuer', $CertIssuer,
            $Artifact
        )
        if ($r.ExitCode -ne 0) {
            Stop-WithError ($sigFail + "
        verifier output:
" + $r.Output + "

        If your cosign predates v3.0, it cannot read this bundle format -
        upgrade cosign (or install the ``sigstore`` python tool) and retry.")
        }
        Write-Log 'install-sirius: signature OK (cosign)'
        return
    }

    # `sigstore` as its own entry point, else any python that can import it.
    $sigExe = $null
    $sigPrefixArgs = @()
    if (Test-HaveCommand 'sigstore') {
        $sigExe = 'sigstore'
    } else {
        foreach ($py in @('python3', 'python', 'py')) {
            if (-not (Test-HaveCommand $py)) { continue }
            # Stock Windows puts 0-byte "App Execution Alias" reparse points for
            # python/python3 in %LOCALAPPDATA%\Microsoft\WindowsApps. Running one
            # OPENS THE MICROSOFT STORE - a GUI popping up mid-install. Skip
            # them; the worst case is falling through to the documented
            # no-verifier warning, which is safe (a bad signature still aborts).
            if (-not (Test-SafeToProbe $py)) { continue }
            $probe = Invoke-Native -FilePath $py -ArgumentList @('-c', 'import sigstore')
            if ($probe.ExitCode -eq 0) {
                $sigExe = $py
                $sigPrefixArgs = @('-m', 'sigstore')
                break
            }
        }
    }

    if ($null -ne $sigExe) {
        Write-Log 'install-sirius: verifying signature (sigstore)'
        $sigArgs = $sigPrefixArgs + @(
            'verify', 'identity',
            '--bundle', $Bundle,
            '--cert-identity', $identity,
            '--cert-oidc-issuer', $CertIssuer,
            $Artifact
        )
        $r = Invoke-Native -FilePath $sigExe -ArgumentList $sigArgs
        if ($r.ExitCode -ne 0) {
            Stop-WithError ($sigFail + "
        verifier output:
" + $r.Output)
        }
        Write-Log 'install-sirius: signature OK (sigstore)'
        return
    }

    if ($RequireSig) {
        Stop-WithError 'no signature verifier found, and -RequireSignature was set.
        Install one:  winget install Sigstore.Cosign   (or)   pip install sigstore'
    }
    Write-Log 'install-sirius: WARNING: no signature verifier (cosign / sigstore) found.'
    Write-Log 'install-sirius: WARNING: proceeding on TLS + checksum alone, which cannot'
    Write-Log 'install-sirius: WARNING: detect a tampered release. To verify provenance:'
    Write-Log 'install-sirius: WARNING:   winget install Sigstore.Cosign  (or)  pip install sigstore'
    Write-Log 'install-sirius: WARNING: then re-run with -RequireSignature.'
}

# ---- install ----------------------------------------------------------------

$Platform = Get-Platform
$Tag = Resolve-LatestTag
if ($Tag -notmatch '^[vV]') { $Tag = 'v' + $Tag }   # accept "0.1.1" as "v0.1.1"
$VersionNumber = $Tag -replace '^[vV]', ''         # version = tag minus the leading "v"

$Tarball     = 'sirius-forester-' + $VersionNumber + '-' + $Platform + '.tar.gz'
$BaseUrl     = 'https://github.com/' + $Repo + '/releases/download/' + $Tag
$TarballUrl  = $BaseUrl + '/' + $Tarball
$ChecksumUrl = $TarballUrl + '.sha256'
$BundleUrl   = $TarballUrl + '.sigstore.json'

Write-Log ('install-sirius: repo=' + $Repo + ' tag=' + $Tag + ' platform=' + $Platform)
Write-Log ('install-sirius: asset=' + $Tarball)

# Idempotence: re-running with the same version is a no-op, not a re-download.
if (-not $Force) {
    $installed = Get-InstalledVersion $BinPath
    if ($null -eq $installed) {
        $onPath = Get-CommandPath 'sirius'
        if ($null -ne $onPath) { $installed = Get-InstalledVersion $onPath }
    }
    if ($null -ne $installed -and $installed -eq $VersionNumber) {
        Write-Log ('install-sirius: sirius ' + $installed + ' is already installed; nothing to do (pass -Force to reinstall).')
        Write-PathHint
        exit 0
    }
}

# Allow a dry run of just the detection/mapping logic without network I/O.
if ($IsDryRun) {
    Write-Log ('DRY RUN: would download: ' + $TarballUrl)
    Write-Log ('DRY RUN: would verify:   ' + $ChecksumUrl)
    Write-Log ('DRY RUN: would verify:   ' + $BundleUrl)
    Write-Log ('DRY RUN: would install into: ' + $BinDir)
    Write-PathHint
    exit 0
}

# tar.exe (bsdtar) ships in Windows 10 1803+ / Windows 11. Prefer the System32
# copy: a Git-for-Windows MSYS tar earlier on PATH can mangle drive-letter paths.
$TarExe = Join-Path $env:SystemRoot 'System32\tar.exe'
if (-not (Test-Path -LiteralPath $TarExe)) {
    $TarExe = Get-CommandPath 'tar.exe'
    if ($null -eq $TarExe) { $TarExe = Get-CommandPath 'tar' }
}
if ($null -eq $TarExe) {
    Stop-WithError 'no tar.exe found. It ships with Windows 10 1803+ and Windows 11; on an older box, extract the release tarball manually.'
}

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('sirius-install.' + [System.Guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $Tmp -Force | Out-Null

try {
    $tarballPath = Join-Path $Tmp $Tarball
    $bundlePath  = $tarballPath + '.sigstore.json'

    Write-Log ('install-sirius: downloading ' + $TarballUrl)
    try {
        Get-RemoteFile -Uri $TarballUrl -OutFile $tarballPath
    } catch {
        Stop-WithError ('download failed: ' + $TarballUrl + ' (does a release exist for ' + $Tag + ' / ' + $Platform + '?)
        ' + $_.Exception.Message)
    }

    # Verify sha256 against the published per-asset checksum file. The release
    # publishes `<tarball>.sha256` in the `shasum -a 256` format:
    # "<hex>  <name>".
    Write-Log 'install-sirius: verifying sha256'
    $checksumLine = ''
    try {
        $checksumLine = Get-RemoteString -Uri $ChecksumUrl
    } catch {
        $checksumLine = ''
    }
    if ([string]::IsNullOrEmpty($checksumLine)) {
        Stop-WithError ('could not fetch checksum: ' + $ChecksumUrl)
    }
    $expected = ($checksumLine.Trim() -split '\s+')[0]
    if ([string]::IsNullOrEmpty($expected)) {
        Stop-WithError 'published checksum was empty'
    }
    if ($expected -notmatch '^[0-9A-Fa-f]{64}$') {
        Stop-WithError ('published checksum is not a sha256 hex digest: ' + $expected)
    }
    $actual = (Get-FileHash -LiteralPath $tarballPath -Algorithm SHA256).Hash
    if ($expected.ToLowerInvariant() -ne $actual.ToLowerInvariant()) {
        Stop-WithError ('checksum mismatch for ' + $Tarball + '
        expected: ' + $expected.ToLowerInvariant() + '
        actual:   ' + $actual.ToLowerInvariant())
    }
    Write-Log ('install-sirius: checksum OK (' + $actual.ToLowerInvariant() + ')')

    # Authenticity. The checksum above came from the same origin as the tarball,
    # so it proves nothing about provenance on its own.
    #
    # A missing bundle is ALWAYS fatal, never a skip. The tarball just
    # downloaded from this same origin, and every release publishes
    # <tarball>.sigstore.json (release.yml uploads with if-no-files-found:
    # error). So "tarball present, bundle absent" is not a benign 404 - it is
    # exactly what an attacker who can serve a tampered tarball would return in
    # order to strip the signature and downgrade us to the checksum, which they
    # also control.
    Write-Log 'install-sirius: fetching signature bundle'
    try {
        Get-RemoteFile -Uri $BundleUrl -OutFile $bundlePath
    } catch {
        Stop-WithError ('no Sigstore bundle at ' + $BundleUrl + '
        The tarball downloaded but its signature did not. Refusing to install.
        Every Sirius Forester release publishes <tarball>.sigstore.json, so a
        missing bundle means the release is malformed or the download was
        tampered with.')
    }
    Invoke-SignatureVerification -Bundle $bundlePath -Artifact $tarballPath -AssetName $Tarball -ReleaseTag $Tag

    Write-Log 'install-sirius: extracting'
    $untar = Invoke-Native -FilePath $TarExe -ArgumentList @('-xzf', $tarballPath, '-C', $Tmp)
    if ($untar.ExitCode -ne 0) {
        Stop-WithError ('tar failed to extract ' + $Tarball + '
        ' + $untar.Output)
    }

    # The tarball expands to a top-level dir:
    # sirius-forester-<version>-<platform>\ holding the binary plus LICENSE and
    # README.md. install-sirius.sh installs ONLY the binary; mirror that exactly
    # (the docs stay in the tarball).
    $stage = Join-Path $Tmp ('sirius-forester-' + $VersionNumber + '-' + $Platform)
    if (-not (Test-Path -LiteralPath $stage)) {
        Stop-WithError ('unexpected tarball layout (no ' + $stage + ')')
    }
    $staged = Join-Path $stage $BinName
    if (-not (Test-Path -LiteralPath $staged)) {
        Stop-WithError ('tarball is missing the ' + $BinName + ' binary')
    }

    if (-not (Test-Path -LiteralPath $BinDir)) {
        New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    }
    # Atomic-ish: write then move into place. A running sirius.exe holds a lock
    # on the destination, so say which process to stop rather than leaving a
    # half-written binary.
    $tmpDst = Join-Path $BinDir ('.sirius.tmp.' + $PID + '.exe')
    Copy-Item -LiteralPath $staged -Destination $tmpDst -Force
    try {
        Move-Item -LiteralPath $tmpDst -Destination $BinPath -Force -ErrorAction Stop
    } catch {
        Remove-Item -LiteralPath $tmpDst -Force -ErrorAction SilentlyContinue
        Stop-WithError ('could not replace ' + $BinPath + ' - is sirius running? Close it and re-run.
        ' + $_.Exception.Message)
    }
    Write-Log ('install-sirius: installed ' + $BinPath)
} finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Log ''
Write-Log ('install-sirius: done. sirius ' + $VersionNumber + ' installed for ' + $Platform + '.')
Write-PathHint
Write-Log ''
Write-Log 'Next steps:'
Write-Log '  sirius init      # set up the .sirius\ ledger in your repo'
Write-Log '  sirius doctor    # verify the workspace contracts (amt + hayven + config)'
Write-SuiteHint
exit 0
