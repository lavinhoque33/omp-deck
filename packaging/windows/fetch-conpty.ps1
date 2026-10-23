<#
.SYNOPSIS
  Fetches the pinned Microsoft.Windows.Console.ConPTY NuGet package (MIT) and extracts the sideloadable
  conpty.dll + OpenConsole.exe for OmpDeck's session host.

.PARAMETER OutDir
  Destination directory (created if missing). With -Arch x64 or arm64 the two files are placed directly in it
  (next to omp-deck-host.exe). With -Arch all they go to <OutDir>\x64 and <OutDir>\arm64.

.PARAMETER Arch
  x64 (default), arm64 or all.

.PARAMETER NoticeFile
  Where to write the ConPTY third-party notice (package id, version, source, MIT license text).
  Default: <OutDir>\THIRD-PARTY-ConPTY.txt. Pass an empty string to skip.

.EXAMPLE
  pwsh packaging/windows/fetch-conpty.ps1 -OutDir target/debug
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [ValidateSet('x64', 'arm64', 'all')][string]$Arch = 'x64',
    [string]$NoticeFile
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Pinned: latest stable on nuget.org (2026-10). The sha256 is of the .nupkg exactly as served by
# https://api.nuget.org/v3-flatcontainer/ . To bump: pick a new non-preview version, download it, run sha256sum.
$PackageId = 'Microsoft.Windows.Console.ConPTY'
$Version = '1.25.260930003'
$Sha256 = '02b07b349af66d801159bdf9e440d4a1ce78bb951f37fc8609731665afdae7ee'
$Url = "https://api.nuget.org/v3-flatcontainer/$($PackageId.ToLowerInvariant())/$Version/$($PackageId.ToLowerInvariant()).$Version.nupkg"

# The package ships no license file; its nuspec declares <license type="expression">MIT</license> and
# <copyright>(c) Microsoft Corporation</copyright>. Text below is the MIT license of github.com/microsoft/terminal.
$LicenseText = @'
Copyright (c) Microsoft Corporation. All rights reserved.

MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED *AS IS*, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
'@

if (-not $PSBoundParameters.ContainsKey('NoticeFile')) {
    $NoticeFile = Join-Path $OutDir 'THIRD-PARTY-ConPTY.txt'
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("conpty-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $work | Out-Null
try {
    $nupkg = Join-Path $work 'conpty.nupkg'
    Write-Host "Downloading $PackageId $Version"
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $nupkg

    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $nupkg).Hash.ToLowerInvariant()
    if ($actual -ne $Sha256) {
        throw "sha256 mismatch for $PackageId $Version`: expected $Sha256, got $actual"
    }
    Write-Host "sha256 verified: $actual"

    $extract = Join-Path $work 'pkg'
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory($nupkg, $extract)

    $arches = if ($Arch -eq 'all') { @('x64', 'arm64') } else { @($Arch) }
    foreach ($a in $arches) {
        $dll = Join-Path $extract "runtimes\win-$a\native\conpty.dll"
        $exe = Join-Path $extract "build\native\runtimes\$a\OpenConsole.exe"
        foreach ($f in @($dll, $exe)) {
            if (-not (Test-Path -LiteralPath $f)) { throw "package layout changed: missing $f" }
        }
        $dest = if ($Arch -eq 'all') { Join-Path $OutDir $a } else { $OutDir }
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        Copy-Item -LiteralPath $dll -Destination (Join-Path $dest 'conpty.dll') -Force
        Copy-Item -LiteralPath $exe -Destination (Join-Path $dest 'OpenConsole.exe') -Force
        Write-Host "Installed $a conpty.dll + OpenConsole.exe into $dest"
    }

    if ($NoticeFile) {
        $noticeDir = Split-Path -Parent $NoticeFile
        if ($noticeDir) { New-Item -ItemType Directory -Force -Path $noticeDir | Out-Null }
        $notice = @(
            "$PackageId $Version (conpty.dll, OpenConsole.exe)"
            "Source:  https://www.nuget.org/packages/$PackageId/$Version"
            "Project: https://github.com/microsoft/terminal"
            "License: MIT"
            ""
            $LicenseText
        ) -join "`r`n"
        [System.IO.File]::WriteAllText($NoticeFile, $notice + "`r`n", (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "Wrote $NoticeFile"
    }
}
finally {
    Remove-Item -Recurse -Force -LiteralPath $work -ErrorAction SilentlyContinue
}
