<#
.SYNOPSIS
The Windows half of the one-liner of dx/12 section 6.

.DESCRIPTION
    irm https://raw.githubusercontent.com/tamnd/zu/main/install.ps1 | iex

Does what install.sh does and in the same order: fetch the release's
digest list, fetch the archive for this machine, refuse to unpack it if
the two disagree, and install the prefix whole. The two scripts are kept
deliberately parallel, down to the names of the environment variables,
because an install story that behaves differently on one platform is an
install story with two sets of bug reports.

Windows 10 1803 and newer, which is where `tar.exe` arrives in the
system image, so an install needs nothing that is not already there.
PowerShell 5.1 is the floor, which is what ships in the box: nothing
here uses a 7.x-only operator, since a user who has to install
PowerShell first has not been given a one-liner.

.PARAMETER Version
The release to install, or `latest`.

.PARAMETER Prefix
Where to install it. The default is under LOCALAPPDATA, because that is
the per-user location no elevation prompt is needed to write to.

.PARAMETER Target
The platform to install for, when this machine is not it.

.PARAMETER NoModifyPath
Leave the user PATH alone.
#>

[CmdletBinding()]
param(
    [string]$Version = $(if ($env:ZU_VERSION) { $env:ZU_VERSION } else { 'latest' }),
    [string]$Prefix = $(if ($env:ZU_PREFIX) { $env:ZU_PREFIX } else { "$env:LOCALAPPDATA\zu" }),
    [string]$Target = $env:ZU_TARGET,
    [string]$Base = $env:ZU_BASE,
    [switch]$NoModifyPath,
    [switch]$Quiet
)

# Stop on the first thing that goes wrong. Without this a failed download
# is a warning followed by an unpack of the file that is not there, and
# the error a user reports is the second one.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

$repo = 'tamnd/zu'

function Say($message) {
    if (-not $Quiet) { Write-Host $message }
}

function Die($message) {
    Write-Error "install.ps1: $message"
    exit 1
}

# One download. The file scheme is here because curl reads it and
# Invoke-WebRequest does not on every shell this runs in: Windows
# PowerShell hands it to a FileWebRequest and PowerShell 7 is built on
# HttpClient, which has no such scheme. ZU_BASE pointing at a directory
# is how the install check drives this script and how anybody mirroring
# a release onto a share would, and an installer that works on one of the
# two shells is the bug this is here to not have.
function Fetch($uri, $path) {
    if ($uri.StartsWith('file:')) {
        $from = ([Uri]$uri).LocalPath
        if (-not (Test-Path -LiteralPath $from)) { throw "no such file: $from" }
        Copy-Item -LiteralPath $from -Destination $path -Force
    } else {
        Invoke-WebRequest -Uri $uri -OutFile $path -UseBasicParsing
    }
}

# The target triple. Windows on Arm reports itself in PROCESSOR_ARCHITECTURE
# as ARM64 and, when a 32-bit PowerShell is emulating, in the WOW6432
# variable beside it, so both are read: the machine is what it is even
# when the shell asking is not.
function Get-Target {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
    switch ($arch) {
        'AMD64' { return 'x86_64-pc-windows-msvc' }
        'ARM64' { return 'aarch64-pc-windows-msvc' }
        default { Die "no release for $arch, build from source: cargo install zu-cli" }
    }
}

if (-not $Target) { $Target = Get-Target }
if (-not $Base) {
    if ($Version -eq 'latest') {
        $Base = "https://github.com/$repo/releases/latest/download"
    } else {
        $Base = "https://github.com/$repo/releases/download/v$Version"
    }
}

$archive = "libzu-$Target.tar.zst"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("zu-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    Say "installing zu for $Target"

    # The progress bar is off because it costs more than the download on
    # a fast link: Invoke-WebRequest repaints it per chunk, and a 20 MiB
    # archive spends most of its time drawing.
    $progress = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'
    try {
        Fetch "$Base/SHA256SUMS" "$tmp\SHA256SUMS"
    } catch {
        Die "no release at $Base, check the version"
    }
    try {
        Fetch "$Base/$archive" "$tmp\$archive"
    } catch {
        Die "$Target is not a platform this release publishes"
    }
    $ProgressPreference = $progress

    # The digest list is the release's own and is read the way the shell
    # script reads it, two spaces between the digest and the name, so one
    # file serves both installers rather than each getting a format.
    $line = Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match "\s$([regex]::Escape($archive))$" }
    if (-not $line) { Die "$archive is not in the release's SHA256SUMS" }
    $want = ($line -split '\s+')[0]
    $got = (Get-FileHash -Path "$tmp\$archive" -Algorithm SHA256).Hash.ToLower()
    if ($want -ne $got) {
        Die "$archive is not what the release says it is: $got, and SHA256SUMS says $want"
    }

    # tar.exe is libarchive and reads zstd directly. Unpacked beside the
    # download and moved afterwards, so a failure half way through leaves
    # the previous install alone.
    $unpacked = "$tmp\unpacked"
    New-Item -ItemType Directory -Path $unpacked -Force | Out-Null
    & tar.exe -xf "$tmp\$archive" -C $unpacked
    if ($LASTEXITCODE -ne 0) { Die "tar could not unpack $archive" }
    $staged = "$unpacked\libzu-$Target"
    if (-not (Test-Path $staged)) { Die "$archive does not unpack as libzu-$Target" }

    New-Item -ItemType Directory -Path $Prefix -Force | Out-Null
    foreach ($entry in Get-ChildItem -Path $staged) {
        $into = Join-Path $Prefix $entry.Name
        if (Test-Path $into) { Remove-Item -Recurse -Force $into }
        Copy-Item -Recurse -Force -Path $entry.FullName -Destination $into
    }

    $installed = & "$Prefix\bin\zu.exe" version
    if ($LASTEXITCODE -ne 0) { Die "$Prefix\bin\zu.exe does not run on this machine" }
    Say "installed $($installed | Select-Object -First 1) in $Prefix"

    # The user PATH rather than the machine one, which is the same
    # decision as installing under LOCALAPPDATA: no elevation, and
    # uninstalling is one user's problem. Read from the registry through
    # the user scope rather than from $env:PATH, because that one is the
    # two lists already joined and writing it back would copy the machine
    # entries into the user's.
    $bin = Join-Path $Prefix 'bin'
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $already = $user -and (($user -split ';') -contains $bin)
    if (-not $already) {
        if ($NoModifyPath) {
            Say ''
            Say "add it to your PATH with:"
            Say "    `$env:Path = `"$bin;`$env:Path`""
        } else {
            $updated = if ($user) { "$user;$bin" } else { $bin }
            [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
            $env:Path = "$bin;$env:Path"
            Say "put $bin on your PATH, which new shells will pick up"
        }
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
