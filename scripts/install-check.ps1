<#
.SYNOPSIS
The Windows half of scripts/install-check.sh.

.DESCRIPTION
Installs zu the way a reader of the README installs it on Windows, and
then does the first thing they came to do with it: convert an edge list
to Parquet, load it and print the statistics.

The release it installs from is one platform's, assembled from the
package the job staged a step earlier by the same table and the same
packer a real release uses, so what lands here is what a user would have
downloaded. `install.ps1` has no unit tests of its own, since the shell
half's live in a Rust test that is `#![cfg(unix)]` and a PowerShell
installer driven from anything but PowerShell tests neither, so this is
the whole of what proves that script works.

.PARAMETER Target
The platform this job built, which is the archive the release carries.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Target
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

$root = Split-Path -Parent $PSScriptRoot
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("zu-install-check-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work -Force | Out-Null

try {
    $release = Join-Path $work 'release'
    # `--fast` because this release is installed once and deleted, and
    # the level a real one is packed at costs two minutes of a core to
    # shorten a copy from one directory to another.
    & cargo run -q -p xtask -- artifacts --assemble $release --built dist --target $Target --fast
    if ($LASTEXITCODE -ne 0) { throw "assembling a release of $Target failed" }

    # `irm | iex`, actually piped, because that is the invocation the
    # README gives and the one with a failure mode of its own: a script
    # run this way has no file on disk, so anything reading $PSScriptRoot
    # or its own source works when run as a file and not here.
    #
    # file:// rather than a server, since the release is on this disk and
    # what is under test is the installer. The target is not passed,
    # which makes the fetch a check on Get-Target: name the platform
    # wrongly and the release has no such archive.
    $prefix = Join-Path $work 'prefix'
    $env:ZU_BASE = 'file:///' + ($release -replace '\\', '/')
    $env:ZU_PREFIX = $prefix
    Get-Content (Join-Path $root 'install.ps1') -Raw | Invoke-Expression

    # The whole prefix and not just the CLI, because dx/12 section 6
    # installs a package: a user who takes the one-liner today compiles
    # against the header next week.
    foreach ($file in @('bin\zu.exe', 'include\zu.h', 'lib\pkgconfig\libzu.pc')) {
        if (-not (Test-Path -LiteralPath (Join-Path $prefix $file))) {
            throw "$file is not in $prefix, so what landed is not a package"
        }
    }

    # What the promise says, in the order a user meets it. The conversion
    # is done by the installed binary rather than by the build, so a CLI
    # shipped without the arrow feature fails here rather than in a bug
    # report.
    $zu = Join-Path $prefix 'bin\zu.exe'
    $edges = Join-Path $work 'edges.txt'
    Set-Content -LiteralPath $edges -Value "1 2`n1 3`n2 3`n3 1`n" -NoNewline
    & $zu convert $edges (Join-Path $work 'edges.parquet')
    if ($LASTEXITCODE -ne 0) { throw 'the installed zu cannot write Parquet' }
    & $zu copy (Join-Path $work 'edges.parquet') (Join-Path $work 'graph.zu1')
    if ($LASTEXITCODE -ne 0) { throw 'the installed zu cannot read Parquet' }
    $stat = & $zu stat (Join-Path $work 'graph.zu1')
    if ($LASTEXITCODE -ne 0) { throw 'the installed zu cannot read what it wrote' }
    $stat | Write-Host

    # That the statistics are of the graph that went in, since `zu stat`
    # on a database it failed to load would print a shape rather than
    # fail, and four edges over four nodes is the answer the four lines
    # above have.
    $said = $stat -join "`n"
    foreach ($want in @('node (4 rows)', 'edge (4 edges, node to node)')) {
        if (-not $said.Contains($want)) {
            throw "zu stat does not say `"$want`""
        }
    }

    Write-Host "install: $Target installs from a release, reads Parquet and answers"
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
