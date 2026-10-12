# SPDX-License-Identifier: AGPL-3.0-or-later
# Builds the Windows packages from a release binary: a portable .zip and an
# installer.
#   packaging/windows/package.ps1 -Binary <supas3.exe> -Licenses <file> -Arch x64|arm64 -Out <dir>
# The installer needs Inno Setup 6 (ISCC.exe on PATH or in its default folder).
param(
    [Parameter(Mandatory)] [string] $Binary,
    [Parameter(Mandatory)] [string] $Licenses,
    [Parameter(Mandatory)] [ValidateSet('x64', 'arm64')] [string] $Arch,
    [Parameter(Mandatory)] [string] $Out
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$Binary = (Resolve-Path $Binary).Path
$Licenses = (Resolve-Path $Licenses).Path
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Out = (Resolve-Path $Out).Path

$manifest = Get-Content (Join-Path $repo 'crates\supas3-app\Cargo.toml')
$version = ($manifest | Select-String -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches.Groups[1].Value
if (-not $version) { throw 'could not read the version from Cargo.toml' }

$name = "supas3-$version-windows-$Arch"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("supas3-package-" + [System.Guid]::NewGuid())
$tree = Join-Path $work $name
New-Item -ItemType Directory -Force -Path (Join-Path $tree 'LICENSES') | Out-Null
try {
    Copy-Item $Binary (Join-Path $tree 'supas3.exe')
    Copy-Item (Join-Path $repo 'LICENSE') (Join-Path $tree 'LICENSE.txt')
    Copy-Item (Join-Path $repo 'THIRD_PARTY_NOTICES.md') $tree
    Copy-Item (Join-Path $repo 'LICENSES\*') (Join-Path $tree 'LICENSES')
    Copy-Item $Licenses (Join-Path $tree 'THIRD_PARTY_LICENSES.txt')
    Copy-Item (Join-Path $repo 'packaging\icons\supas3.ico') $tree

    # --- portable archive: unpack anywhere and run supas3.exe
    $zip = Join-Path $Out "$name.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    Compress-Archive -Path $tree -DestinationPath $zip

    # --- installer
    $iscc = $null
    $onPath = Get-Command ISCC.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($onPath) { $iscc = $onPath.Source }
    if (-not $iscc) {
        $candidates = @(
            (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
            (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
        )
        $iscc = $candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
    }
    if (-not $iscc) { throw 'Inno Setup 6 (ISCC.exe) was not found' }
    & $iscc "/DAppVersion=$version" "/DArch=$Arch" "/DSourceDir=$tree" "/DOutputDir=$Out" /Qp (Join-Path $PSScriptRoot 'supas3.iss')
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE" }
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
Get-ChildItem $Out | Format-Table Name, Length
