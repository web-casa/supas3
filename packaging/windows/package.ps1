# SPDX-License-Identifier: AGPL-3.0-or-later
# Builds the Windows packages from a release binary: a portable .zip and an
# installer.
#   packaging/windows/package.ps1 -Binary <supas3.exe> -Licenses <file> -Arch x64|arm64 -Out <dir>
# The installer needs Inno Setup 6.5 or later (ISCC.exe on PATH or in its
# default folder), for its Simplified Chinese messages.
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
    # The newest one found: an upgrade may leave an older one on PATH. The
    # installations are listed in the registry; ISCC.exe on PATH may be a
    # Chocolatey shim, which reports no version of its own.
    # ISCC.exe itself carries no version; the registry lists each
    # installation with its version.
    $uninstall = 'Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup *_is1'
    $registered = @{}
    Get-ItemProperty -ErrorAction SilentlyContinue `
            "HKLM:\SOFTWARE\WOW6432Node\$uninstall", "HKLM:\SOFTWARE\$uninstall", "HKCU:\Software\$uninstall" |
        Where-Object { $_.InstallLocation -and $_.DisplayVersion } |
        ForEach-Object {
            $registered[(Join-Path $_.InstallLocation 'ISCC.exe')] = $_.DisplayVersion
        }
    $candidates = @($registered.Keys)
    $candidates += foreach ($major in 7, 6) {
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup $major\ISCC.exe")
        (Join-Path $env:ProgramFiles "Inno Setup $major\ISCC.exe")
    }
    $candidates += @(Get-Command ISCC.exe -All -ErrorAction SilentlyContinue | ForEach-Object Source)
    $versionOf = {
        param($path)
        $text = $registered[$path]
        if (-not $text) { $text = (Get-Item $path).VersionInfo.ProductVersion }
        $text = "$text" -replace '[^0-9.].*$', ''
        if ($text) { [version]$text } else { [version]'0.0' }
    }
    $iscc = $candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -Unique |
        Sort-Object { & $versionOf $_ } -Descending | Select-Object -First 1
    if (-not $iscc) { throw 'Inno Setup (ISCC.exe) was not found' }
    $found = & $versionOf $iscc
    Write-Host "Inno Setup $found at $iscc"
    if ($found.Major -eq 0) {
        # Only a shim was found; the compiler itself refuses messages it
        # does not know.
        Write-Warning "The version of $iscc is unknown; Inno Setup 6.5 or later is needed"
    } elseif ($found -lt [version]'6.5.0') {
        throw "Inno Setup 6.5 or later is needed for the Simplified Chinese installer; found $found"
    }
    & $iscc "/DAppVersion=$version" "/DArch=$Arch" "/DSourceDir=$tree" "/DOutputDir=$Out" /Qp (Join-Path $PSScriptRoot 'supas3.iss')
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE" }
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
Get-ChildItem $Out | Format-Table Name, Length
